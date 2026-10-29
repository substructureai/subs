use std::ffi::OsString;
use std::fmt;
use std::io;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::sync::Mutex;

use super::child::{Hello, Reply, Request, DATA, DATA_MIB};
use super::{Ran, Script, Stats, Step, ARG};
use std::collections::BTreeMap;
use substructure_session::blob::{BlobError, BlobRef, BlobStore, MemoryBlobStore};

/// Runs one step of a script. A runner keeps nothing between steps, so any
/// runner can take any step: a process here, a machine elsewhere, a service.
/// The engine checks every call a step asks for, so a runner gains nothing by
/// lying.
#[async_trait]
pub trait ScriptRunner: Send + Sync {
    /// The step the script reached and what it took, or an error that a
    /// later try may not meet. A failed script is a [`Step::Failed`], not an
    /// error. A runner holds the script to its [`super::Limits`], and reads
    /// its files from `files`.
    async fn run(&self, script: Script, files: ScriptFiles) -> Result<Ran, RunError>;
}

/// What is behind each of a script's files, for its runner to read. The
/// script sees only their ids.
#[derive(Clone)]
pub struct ScriptFiles {
    pub tenant_id: String,
    pub sources: BTreeMap<String, FileSource>,
    pub store: Arc<dyn BlobStore>,
}

/// Where a file's bytes are. Neither changes, so a file reads the same on
/// every run.
#[derive(Debug, Clone, PartialEq)]
pub enum FileSource {
    /// A stored blob, such as an attachment.
    Blob(BlobRef),
    /// Text held in a plugin bundle, such as a skill's reference file.
    Text(Arc<str>),
}

/// Why a file could not be read.
pub enum ReadError {
    /// The file is not there, or not this tenant's. The script hears of it.
    Missing(String),
    /// The store failed. The step is tried again.
    Store(String),
}

impl ScriptFiles {
    pub fn none() -> Self {
        ScriptFiles {
            tenant_id: String::new(),
            sources: BTreeMap::new(),
            store: Arc::new(MemoryBlobStore::new()),
        }
    }

    /// Bytes `offset..offset + len` of `file`, fewer at its end.
    pub async fn read(&self, file: &str, offset: u64, len: u64) -> Result<Vec<u8>, ReadError> {
        let missing = || ReadError::Missing(format!("There is no file \"{file}\"."));
        let blob = match self.sources.get(file).ok_or_else(missing)? {
            FileSource::Text(text) => {
                return Ok(substructure_session::blob::slice(
                    text.as_bytes(),
                    offset..offset.saturating_add(len),
                ))
            }
            FileSource::Blob(blob) => blob,
        };
        if blob.tenant_id != self.tenant_id {
            return Err(missing());
        }
        let end = offset.saturating_add(len).min(blob.size);
        if offset >= end {
            return Ok(Vec::new());
        }
        self.store
            .read(blob, offset..end)
            .await
            .map_err(|e| match e {
                BlobError::NotFound => {
                    ReadError::Missing(format!("\"{file}\" is no longer stored."))
                }
                other => ReadError::Store(other.to_string()),
            })
    }
}

/// Why a step ended without a `Ran`.
enum Stop {
    /// The script ran too long or its process died. The script fails.
    Failed(String),
    /// Its files could not be read. The step is tried again.
    Store(String),
}

/// The runner could not take the step this time: it was down, busy, or of
/// another version. The engine tries the step again.
#[derive(Debug, Clone, PartialEq)]
pub struct RunError(pub String);

impl fmt::Display for RunError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for RunError {}

const READY: Duration = Duration::from_secs(10);
/// Past three times a run's CPU, for a contended machine, loading the script,
/// and work QuickJS cannot interrupt.
const SLACK: Duration = Duration::from_secs(1);
const ANSWER: u64 = 64 << 20;
/// Time a run may wait on reading its files, apart from its own deadline.
const READ_WAIT: Duration = Duration::from_secs(60);

/// Runs each step in a fresh confined process started from `program`, which
/// must call [`super::dispatch`] first in `main`.
pub struct ProcessRunner {
    program: PathBuf,
    args: Vec<OsString>,
    deadline: Option<Duration>,
    data_mib: u64,
    spare: Arc<Mutex<Option<Worker>>>,
    refilling: Arc<AtomicBool>,
}

struct Worker {
    process: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    versions: (u32, u32),
    languages: Vec<super::Language>,
}

/// Why a process could not take a step.
enum Start {
    /// It may start next time.
    Again(String),
    /// It never will: the program is not a runner, or cannot confine itself.
    Never(String),
}

impl ProcessRunner {
    /// This binary as the runner.
    pub fn current() -> Self {
        let program = std::env::current_exe().unwrap_or_default();
        ProcessRunner::new(program, vec![ARG.into()])
    }

    pub fn new(program: PathBuf, args: Vec<OsString>) -> Self {
        ProcessRunner {
            program,
            args,
            deadline: None,
            data_mib: DATA_MIB,
            spare: Default::default(),
            refilling: Default::default(),
        }
    }

    /// A fixed deadline per step, in place of the script's compute limit
    /// plus slack.
    pub fn with_deadline(self, deadline: Duration) -> Self {
        ProcessRunner {
            deadline: Some(deadline),
            ..self
        }
    }

    /// The most heap a process may take, script and records together. Keep it
    /// above the scripts' memory limit.
    pub fn with_data_limit(self, mib: u64) -> Self {
        ProcessRunner {
            data_mib: mib,
            ..self
        }
    }

    /// The command that starts a runner: no environment, `/` as its
    /// directory, and only the three pipes.
    pub fn command(&self) -> Command {
        let mut command = Command::new(&self.program);
        command
            .args(&self.args)
            .arg(format!("{DATA}{}", self.data_mib))
            .env_clear()
            .current_dir("/")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        command
    }

    async fn take_spare(&self) -> Option<Worker> {
        let mut spare = self.spare.lock().await.take()?;
        match spare.process.try_wait() {
            Ok(None) => Some(spare),
            _ => None,
        }
    }

    /// Starts and confines the next process before it is needed.
    fn refill(&self) {
        if self.refilling.swap(true, Ordering::AcqRel) {
            return;
        }
        let (spare, refilling) = (self.spare.clone(), self.refilling.clone());
        let started = self.start();
        tokio::spawn(async move {
            if spare.lock().await.is_none() {
                if let Ok(worker) = started.await {
                    spare.lock().await.get_or_insert(worker);
                }
            }
            refilling.store(false, Ordering::Release);
        });
    }

    fn start(&self) -> impl std::future::Future<Output = Result<Worker, Start>> + Send + 'static {
        let spawned = self.command().spawn();
        let program = self.program.clone();
        async move {
            let mut process = spawned.map_err(|e| Start::Again(e.to_string()))?;
            let (Some(stdin), Some(stdout)) = (process.stdin.take(), process.stdout.take()) else {
                return Err(Start::Again("its pipes did not open".to_string()));
            };
            let mut stdout = BufReader::new(stdout);
            let line = tokio::time::timeout(READY, read_line(&mut stdout))
                .await
                .map_err(|_| Start::Again("it did not start in time".to_string()))?
                .map_err(|e| Start::Again(e.to_string()))?;
            let hello = serde_json::from_slice(&line).map_err(|_| {
                Start::Never(format!(
                    "`{}` did not answer as a runner. Its `main` must call \
                     `codemode::dispatch()` first",
                    program.display()
                ))
            })?;
            match hello {
                Hello::Ready {
                    version,
                    oldest,
                    languages,
                } => Ok(Worker {
                    process,
                    stdin,
                    stdout,
                    versions: (oldest, version),
                    languages,
                }),
                Hello::Unconfined { error } => Err(Start::Never(format!(
                    "it could not confine itself: {error}"
                ))),
            }
        }
    }

    /// Sends the script, answers its reads, and takes its `Ran`. Time spent
    /// reading files does not count toward the run's deadline.
    async fn exchange(
        &self,
        worker: &mut Worker,
        script: &Script,
        files: &ScriptFiles,
    ) -> Result<Ran, Stop> {
        let failed = |e: io::Error| Stop::Failed(e.to_string());
        let mut request = serde_json::to_vec(script).map_err(|e| Stop::Failed(e.to_string()))?;
        request.push(b'\n');
        worker.stdin.write_all(&request).await.map_err(failed)?;
        worker.stdin.flush().await.map_err(failed)?;
        let mut deadline = tokio::time::Instant::now()
            + self
                .deadline
                .unwrap_or(Duration::from_millis(3 * script.compute_ms()) + SLACK);
        let (budget, mut served, mut waited) = (script.file_bytes(), 0u64, Duration::ZERO);
        loop {
            let line = tokio::time::timeout_at(deadline, read_line(&mut worker.stdout))
                .await
                .map_err(|_| Stop::Failed("the script ran too long".to_string()))?
                .map_err(failed)?;
            let Ok(Request::Read { file, offset, len }) = serde_json::from_slice(&line) else {
                return serde_json::from_slice(&line).map_err(|e| Stop::Failed(e.to_string()));
            };
            let started = tokio::time::Instant::now();
            let (reply, bytes) = match served.saturating_add(len) > budget {
                true => (
                    Reply::Refused("The script read its limit of file bytes.".to_string()),
                    Vec::new(),
                ),
                false => match files.read(&file, offset, len).await {
                    Ok(bytes) => (Reply::Bytes(bytes.len() as u64), bytes),
                    Err(ReadError::Missing(m)) => (Reply::Missing(m), Vec::new()),
                    Err(ReadError::Store(e)) => return Err(Stop::Store(e)),
                },
            };
            served += bytes.len() as u64;
            let mut head = serde_json::to_vec(&reply).map_err(|e| Stop::Failed(e.to_string()))?;
            head.push(b'\n');
            worker.stdin.write_all(&head).await.map_err(failed)?;
            worker.stdin.write_all(&bytes).await.map_err(failed)?;
            worker.stdin.flush().await.map_err(failed)?;
            let spent = started.elapsed();
            deadline += spent;
            waited += spent;
            if waited > READ_WAIT {
                return Err(Stop::Store("reading its files took too long".to_string()));
            }
        }
    }
}

#[async_trait]
impl ScriptRunner for ProcessRunner {
    async fn run(&self, script: Script, files: ScriptFiles) -> Result<Ran, RunError> {
        let spare = self.take_spare().await;
        let warm = spare.is_some();
        let worker = match spare {
            Some(worker) => Ok(worker),
            None => self.start().await,
        };
        self.refill();
        let failed = |message: String, limit| Ran {
            step: Step::Failed(message),
            stats: Stats {
                limit: Some(limit),
                warm,
                ..Stats::default()
            },
            shown: Vec::new(),
        };
        let mut worker = match worker {
            Ok(worker) => worker,
            Err(Start::Again(e)) => {
                return Err(RunError(format!("the sandbox did not start: {e}")))
            }
            Err(Start::Never(e)) => {
                return Ok(failed(
                    format!("The sandbox could not start: {e}"),
                    super::Limit::Start,
                ))
            }
        };
        if !worker.languages.contains(&script.language) {
            let _ = worker.process.start_kill();
            return Err(RunError(format!(
                "the sandbox runs no {:?}",
                script.language
            )));
        }
        let (oldest, newest) = worker.versions;
        if !(oldest..=newest).contains(&script.version) {
            let _ = worker.process.start_kill();
            return Err(RunError(format!(
                "the sandbox takes versions {oldest} to {newest}, not {}",
                script.version
            )));
        }
        let answer = self.exchange(&mut worker, &script, &files).await;
        let _ = worker.process.start_kill();
        Ok(match answer {
            Ok(mut ran) => {
                ran.stats.warm = warm;
                ran
            }
            Err(Stop::Store(e)) => {
                return Err(RunError(format!(
                    "the script's files could not be read: {e}"
                )))
            }
            Err(Stop::Failed(e)) => failed(
                format!(
                    "The script stopped: {e}. It may have run long inside one built-in \
                     operation, or run out of memory."
                ),
                super::Limit::Deadline,
            ),
        })
    }
}

async fn read_line(stdout: &mut BufReader<ChildStdout>) -> io::Result<Vec<u8>> {
    let mut line = Vec::new();
    stdout.take(ANSWER).read_until(b'\n', &mut line).await?;
    match line.pop() {
        Some(b'\n') => Ok(line),
        Some(_) => Err(io::Error::other("its answer was too large")),
        None => Err(io::Error::other("it exited")),
    }
}
