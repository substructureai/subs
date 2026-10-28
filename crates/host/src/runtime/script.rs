use std::sync::Arc;

use serde::{Deserialize, Serialize};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::codemode::{settle, Admission, ScriptRunner};
use crate::protocol::RetryPolicy;
use crate::runtime::executor::{
    settled, spawn_bounded_executors, Bound, BoundedTask, ExecutorPool,
};
use crate::runtime::inbox::Inbox;
use crate::runtime::queue::{Queue, SCRIPT};
use crate::runtime::session::state::EffectKind;
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
}

impl BoundedTask for ScriptTask {
    fn bound(&self) -> Option<Bound> {
        Some(Bound {
            tenant_id: self.tenant_id.clone(),
            session_id: self.session_id.clone(),
            kind: EffectKind::ToolCall,
            id: self.tool_call_id.clone(),
            attempt: Some(self.attempt),
            queue_timeout: self.retry.queue_timeout(),
            run_timeout: self.retry.run_timeout(),
        })
    }
}

/// Steps waiting for a slot, per engine.
const WAITING: usize = 64;

pub fn spawn_script_executor(
    sessions: Arc<Sessions>,
    runner: Arc<dyn ScriptRunner>,
    admission: Arc<Admission>,
    queue: Arc<dyn Queue<ScriptTask>>,
    cancel: CancellationToken,
) -> Vec<JoinHandle<()>> {
    let pool = ExecutorPool {
        workers: 1,
        concurrency: WAITING,
    };
    let inbox: Arc<dyn Inbox> = sessions.clone();
    spawn_bounded_executors(inbox, queue, SCRIPT, pool, cancel, move |task, span| {
        let (sessions, runner, admission) = (sessions.clone(), runner.clone(), admission.clone());
        async move { step(&sessions, runner.as_ref(), &admission, task, span).await }
    })
}

async fn step(
    sessions: &Sessions,
    runner: &dyn ScriptRunner,
    admission: &Arc<Admission>,
    task: ScriptTask,
    span: SpanContext,
) -> Result<(), String> {
    let _ticket = admission.admit(&task.tenant_id).await;
    let session = sessions
        .load(&task.tenant_id, &task.session_id)
        .await
        .map_err(|e| e.to_string())?;
    let Some(script) = session.and_then(|s| s.script(&task.tool_call_id)) else {
        return Ok(());
    };
    let known = script.calls.len() as u32;
    let step = runner.run(script).await.map_err(|e| e.to_string())?;
    let command = settle(task.tool_call_id, task.attempt, known, step);
    settled(
        sessions,
        &task.tenant_id,
        &task.session_id,
        command,
        span.child("script"),
    )
    .await
}
