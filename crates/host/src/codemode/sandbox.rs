use std::collections::HashMap;
use std::ffi::OsString;
use std::io::{self, BufRead, Write};
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::sync::{Mutex, Notify, Semaphore};

use super::confine::{self, Denial};
use super::{run, Step};
use substructure_session::Script;

/// The argument that makes a binary a sandbox process.
pub const ARG: &str = "__sandbox";
pub const PROBE: &str = "probe";
pub const PROBE_LOCK: &str = "probe-lock";
/// Overrides how many scripts run at once.
pub const SLOTS_VAR: &str = "SUBS_SANDBOX_SLOTS";
const READY: Duration = Duration::from_secs(10);
const DEADLINE: Duration = Duration::from_secs(15);
const RESPONSE: u64 = 64 << 20;

/// Runs each script step in a fresh confined process started from `program`.
/// The binary must call [`dispatch`] first in `main`.
pub struct Sandbox {
    program: PathBuf,
    args: Vec<OsString>,
    slots: usize,
    pub deadline: Duration,
    permits: Semaphore,
    tenants: std::sync::Mutex<HashMap<String, usize>>,
    turn: Notify,
    spare: Mutex<Option<Worker>>,
    refilling: AtomicBool,
}

struct Worker {
    process: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
}

impl Default for Sandbox {
    fn default() -> Self {
        Sandbox::current()
    }
}

impl Sandbox {
    /// This binary as the sandbox.
    pub fn current() -> Self {
        let program = std::env::current_exe().unwrap_or_default();
        Sandbox::new(program, vec![ARG.into()])
    }

    pub fn new(program: PathBuf, args: Vec<OsString>) -> Self {
        let slots = std::env::var(SLOTS_VAR)
            .ok()
            .and_then(|s| s.parse().ok())
            .filter(|&n| n > 0)
            .unwrap_or_else(|| {
                std::thread::available_parallelism().map_or(2, |n| n.get().clamp(2, 8))
            });
        Sandbox {
            program,
            args,
            slots,
            deadline: DEADLINE,
            permits: Semaphore::new(slots),
            tenants: Default::default(),
            turn: Notify::new(),
            spare: Mutex::new(None),
            refilling: AtomicBool::new(false),
        }
    }

    pub fn with_slots(self, slots: usize) -> Self {
        let slots = slots.max(1);
        Sandbox {
            slots,
            permits: Semaphore::new(slots),
            ..self
        }
    }

    /// A tenant takes all slots but one, so another tenant always gets a turn.
    fn per_tenant(&self) -> usize {
        self.slots.saturating_sub(1).max(1)
    }

    /// The command that starts a sandbox process: no environment, `/` as its
    /// directory, and only the three pipes.
    pub fn command(&self) -> Command {
        let mut command = Command::new(&self.program);
        command
            .args(&self.args)
            .env_clear()
            .current_dir("/")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        command
    }

    pub async fn run(self: &Arc<Self>, tenant: &str, script: Script) -> Step {
        let _turn = self.admit(tenant).await;
        let Ok(_permit) = self.permits.acquire().await else {
            return Step::Failed("The sandbox is shut down.".to_string());
        };
        let worker = match self.take_spare().await {
            Some(worker) => Ok(worker),
            None => self.spawn().await,
        };
        self.refill();
        let mut worker = match worker {
            Ok(worker) => worker,
            Err(e) => return Step::Failed(format!("The sandbox could not start: {e}")),
        };
        let answer = self.exchange(&mut worker, &script).await;
        let _ = worker.process.start_kill();
        answer.unwrap_or_else(|e| Step::Failed(format!("The sandbox stopped: {e}")))
    }

    async fn admit(self: &Arc<Self>, tenant: &str) -> Turn {
        loop {
            let notified = self.turn.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            {
                let mut tenants = self.tenants.lock().expect("tenants");
                let running = tenants.entry(tenant.to_string()).or_default();
                if *running < self.per_tenant() {
                    *running += 1;
                    return Turn {
                        sandbox: self.clone(),
                        tenant: tenant.to_string(),
                    };
                }
            }
            notified.await;
        }
    }

    async fn take_spare(&self) -> Option<Worker> {
        let mut spare = self.spare.lock().await.take()?;
        match spare.process.try_wait() {
            Ok(None) => Some(spare),
            _ => None,
        }
    }

    /// Starts and confines the next process before it is needed.
    fn refill(self: &Arc<Self>) {
        if self.refilling.swap(true, Ordering::AcqRel) {
            return;
        }
        let sandbox = self.clone();
        tokio::spawn(async move {
            if sandbox.spare.lock().await.is_none() {
                if let Ok(worker) = sandbox.spawn().await {
                    sandbox.spare.lock().await.get_or_insert(worker);
                }
            }
            sandbox.refilling.store(false, Ordering::Release);
        });
    }

    async fn spawn(&self) -> io::Result<Worker> {
        let mut process = self.command().spawn()?;
        let stdin = process
            .stdin
            .take()
            .ok_or_else(|| io::Error::other("no stdin"))?;
        let stdout = process
            .stdout
            .take()
            .ok_or_else(|| io::Error::other("no stdout"))?;
        let mut worker = Worker {
            process,
            stdin,
            stdout: BufReader::new(stdout),
        };
        let line = tokio::time::timeout(READY, read_line(&mut worker.stdout))
            .await
            .map_err(|_| io::Error::other("it did not start in time"))??;
        let hello: serde_json::Value = serde_json::from_slice(&line).map_err(|_| {
            io::Error::other(format!(
                "`{}` did not answer as a sandbox. Its `main` must call \
                 `codemode::sandbox::dispatch()` first",
                self.program.display()
            ))
        })?;
        match hello.get("ready").and_then(|r| r.as_bool()) {
            Some(true) => Ok(worker),
            _ => Err(io::Error::other(format!(
                "it could not confine itself: {}",
                hello
                    .get("error")
                    .and_then(|e| e.as_str())
                    .unwrap_or("no reason")
            ))),
        }
    }

    async fn exchange(&self, worker: &mut Worker, script: &Script) -> io::Result<Step> {
        let mut request = serde_json::to_vec(script)?;
        request.push(b'\n');
        worker.stdin.write_all(&request).await?;
        worker.stdin.flush().await?;
        let line = tokio::time::timeout(self.deadline, read_line(&mut worker.stdout))
            .await
            .map_err(|_| io::Error::other("the script ran too long"))??;
        Ok(serde_json::from_slice(&line)?)
    }
}

struct Turn {
    sandbox: Arc<Sandbox>,
    tenant: String,
}

impl Drop for Turn {
    fn drop(&mut self) {
        let mut tenants = self.sandbox.tenants.lock().expect("tenants");
        if let Some(running) = tenants.get_mut(&self.tenant) {
            *running -= 1;
            if *running == 0 {
                tenants.remove(&self.tenant);
            }
        }
        drop(tenants);
        self.sandbox.turn.notify_waiters();
    }
}

async fn read_line(stdout: &mut BufReader<ChildStdout>) -> io::Result<Vec<u8>> {
    let mut line = Vec::new();
    stdout.take(RESPONSE).read_until(b'\n', &mut line).await?;
    match line.pop() {
        Some(b'\n') => Ok(line),
        Some(_) => Err(io::Error::other("its answer was too large")),
        None => Err(io::Error::other("it exited")),
    }
}

/// Call first in `main`, before any thread starts. Becomes the sandbox
/// process and never returns when the binary was started as one.
pub fn dispatch() {
    let mut args = std::env::args_os().skip(1);
    if args.next().as_deref() == Some(ARG.as_ref()) {
        main(args.next());
    }
}

fn main(mode: Option<OsString>) -> ! {
    let mode = mode.as_deref().and_then(|m| m.to_str());
    let confined = match mode {
        Some(PROBE_LOCK) => confine::lock_only(),
        Some(PROBE) => confine::confine(Denial::Refuse),
        _ => confine::confine(Denial::Kill),
    };
    let mut out = io::stdout().lock();
    if matches!(mode, Some(PROBE | PROBE_LOCK)) {
        let report = match &confined {
            Ok(()) => confine::probe(),
            Err(e) => serde_json::json!({ "error": e.to_string() }),
        };
        let _ = writeln!(out, "{report}");
        std::process::exit(0);
    }
    let hello = match &confined {
        Ok(()) => serde_json::json!({ "ready": true }),
        Err(e) => serde_json::json!({ "ready": false, "error": e.to_string() }),
    };
    let _ = writeln!(out, "{hello}");
    let _ = out.flush();
    if confined.is_err() {
        std::process::exit(70);
    }
    let mut line = String::new();
    if io::stdin().lock().read_line(&mut line).unwrap_or(0) == 0 {
        std::process::exit(0);
    }
    let step = match serde_json::from_str::<Script>(&line) {
        Ok(script) => run(&script),
        Err(e) => Step::Failed(format!("The sandbox could not read the script: {e}")),
    };
    let _ = writeln!(out, "{}", serde_json::to_string(&step).unwrap_or_default());
    let _ = out.flush();
    std::process::exit(0)
}
