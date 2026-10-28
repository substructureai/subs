use std::sync::Arc;
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::codemode::{settle, Admission, Limit, Limits, Ran, ScriptRunner, Stats, Step};
use crate::protocol::RetryPolicy;
use crate::runtime::executor::{
    settled, spawn_bounded_executors, Bound, BoundedTask, ExecutorPool,
};
use crate::runtime::inbox::Inbox;
use crate::runtime::queue::{Queue, SCRIPT};
use crate::runtime::sessions::Sessions;
use crate::runtime::span::SpanContext;

/// Run a `run_code` call's script again. The script and its calls are read
/// from the session when the step runs.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScriptTask {
    pub session_id: String,
    pub tenant_id: String,
    pub tool_call_id: String,
    pub attempt: u32,
    pub retry: RetryPolicy,
    #[serde(default = "Utc::now")]
    pub queued_at: DateTime<Utc>,
}

/// A step checks its own wait, after it has a slot, and reads the session
/// once, so the executor holds it to nothing.
impl BoundedTask for ScriptTask {
    fn bound(&self) -> Option<Bound> {
        None
    }
}

/// Steps waiting for a slot, per engine.
const WAITING: usize = 64;

pub struct Scripts {
    pub runner: Arc<dyn ScriptRunner>,
    pub admission: Arc<Admission>,
    pub limits: Limits,
    /// How long a step may wait for a slot, or for a runner to take it.
    pub wait: Duration,
}

pub fn spawn_script_executor(
    sessions: Arc<Sessions>,
    scripts: Arc<Scripts>,
    queue: Arc<dyn Queue<ScriptTask>>,
    cancel: CancellationToken,
) -> Vec<JoinHandle<()>> {
    let pool = ExecutorPool {
        workers: 1,
        concurrency: WAITING,
    };
    let inbox: Arc<dyn Inbox> = sessions.clone();
    spawn_bounded_executors(inbox, queue, SCRIPT, pool, cancel, move |task, span| {
        let (sessions, scripts) = (sessions.clone(), scripts.clone());
        async move { step(&sessions, &scripts, task, span).await }
    })
}

async fn step(
    sessions: &Sessions,
    scripts: &Scripts,
    task: ScriptTask,
    span: SpanContext,
) -> Result<(), String> {
    let _ticket = scripts.admission.admit(&task.tenant_id).await;
    let waited = (Utc::now() - task.queued_at).to_std().unwrap_or_default();
    let session = sessions
        .load(&task.tenant_id, &task.session_id)
        .await
        .map_err(|e| e.to_string())?;
    let Some(script) = session.and_then(|s| s.script(&task.tool_call_id, &scripts.limits)) else {
        return Ok(());
    };
    let known = script.calls.len() as u32;
    let records: usize = script.calls.iter().map(|c| c.output.len()).sum();
    let started = Instant::now();
    let ran = match waited > scripts.wait {
        true => late(waited, Limit::Wait),
        false => match scripts.runner.run(script).await {
            Ok(ran) => ran,
            Err(e) if waited + RETRY > scripts.wait => {
                tracing::warn!(tenant_id = %task.tenant_id, error = %e, "no runner took a script step");
                late(waited, Limit::Start)
            }
            Err(e) => return Err(e.to_string()),
        },
    };
    report(
        &task,
        known,
        records,
        waited,
        started.elapsed(),
        &ran,
        &scripts.limits,
    );
    let command = settle(task.tool_call_id, task.attempt, known, ran.step);
    settled(
        sessions,
        &task.tenant_id,
        &task.session_id,
        command,
        span.child("script"),
    )
    .await
}

/// How soon the executor tries a step again after a runner could not take it.
const RETRY: Duration = Duration::from_secs(5);

fn late(waited: Duration, limit: Limit) -> Ran {
    let message = match limit {
        Limit::Wait => format!(
            "The script waited {}s for a free sandbox and did not run. Try again in a moment.",
            waited.as_secs()
        ),
        _ => format!(
            "No sandbox could run the script for {}s. Try again in a moment.",
            waited.as_secs()
        ),
    };
    Ran {
        step: Step::Failed(message),
        stats: Stats {
            limit: Some(limit),
            ..Stats::default()
        },
    }
}

fn report(
    task: &ScriptTask,
    known: u32,
    records: usize,
    waited: Duration,
    ran_for: Duration,
    ran: &Ran,
    limits: &Limits,
) {
    let outcome = match &ran.step {
        Step::Calls(calls) => format!("calls:{}", calls.len()),
        Step::Done(_) => "done".to_string(),
        Step::Failed(_) => "failed".to_string(),
    };
    let limit = ran
        .stats
        .limit
        .map(|l| format!("{l:?}"))
        .unwrap_or_default();
    tracing::info!(
        target: "codemode",
        tenant_id = %task.tenant_id,
        session_id = %task.session_id,
        tool_call_id = %task.tool_call_id,
        step = known,
        records_bytes = records,
        wait_ms = waited.as_millis() as u64,
        run_ms = ran_for.as_millis() as u64,
        warm = ran.stats.warm,
        heap = ran.stats.heap,
        cpu_ms = ran.stats.cpu_ms,
        %outcome,
        %limit,
        "script step"
    );
    let heap = ran.stats.heap > limits.memory / 2;
    let cpu = ran.stats.cpu_ms > limits.compute_ms / 2;
    if heap || cpu {
        tracing::warn!(
            target: "codemode",
            tenant_id = %task.tenant_id,
            tool_call_id = %task.tool_call_id,
            heap = ran.stats.heap,
            cpu_ms = ran.stats.cpu_ms,
            "a script step used over half its memory or compute"
        );
    }
}
