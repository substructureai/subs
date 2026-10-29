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

use super::child::{Hello, DATA, DATA_MIB};
use super::{Ran, Script, Stats, Step, ARG};

/// Runs one step of a script. A runner keeps nothing between steps, so any
/// runner can take any step: a process here, a machine elsewhere, a service.
/// The engine checks every call a step asks for, so a runner gains nothing by
/// lying.
#[async_trait]
pub trait ScriptRunner: Send + Sync {
    /// The step the script reached and what it took, or an error that a
    /// later try may not meet. A failed script is a [`Step::Failed`], not an
    /// error. A runner holds the script to its [`super::Limits`].
    async fn run(&self, script: Script) -> Result<Ran, RunError>;
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

    async fn exchange(&self, worker: &mut Worker, script: &Script) -> io::Result<Ran> {
        let mut request = serde_json::to_vec(script)?;
        request.push(b'\n');
        worker.stdin.write_all(&request).await?;
        worker.stdin.flush().await?;
        let deadline = self
            .deadline
            .unwrap_or(Duration::from_millis(3 * script.compute_ms()) + SLACK);
        let line = tokio::time::timeout(deadline, read_line(&mut worker.stdout))
            .await
            .map_err(|_| io::Error::other("the script ran too long"))??;
        Ok(serde_json::from_slice(&line)?)
    }
}

#[async_trait]
impl ScriptRunner for ProcessRunner {
    async fn run(&self, script: Script) -> Result<Ran, RunError> {
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
        let answer = self.exchange(&mut worker, &script).await;
        let _ = worker.process.start_kill();
        Ok(match answer {
            Ok(mut ran) => {
                ran.stats.warm = warm;
                ran
            }
            Err(e) => failed(
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
