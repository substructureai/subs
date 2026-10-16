use std::sync::Arc;

use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::protocol::DraftMessage;
use crate::runtime::executor::{settled, spawn_bounded_executors, ExecutorPool};
use crate::runtime::inbox::{execute, ExecuteError, ExecuteInput, Inbox};
use crate::runtime::queue::{Queue, SUBAGENT};
use crate::runtime::session::command::{CommandPayload, Outcome, SessionError, SettleError};
use crate::runtime::session::state::EffectKind;
use crate::runtime::span::SpanContext;
use crate::runtime::worker::{directory::create_session_command, AgentDirectory, SessionSeed};
use crate::runtime::Caller;
use substructure_session::WorkId;

use super::SubagentTask;
use crate::protocol::ErrorInfo;

pub fn spawn_subagent_task_executor(
    store: Arc<dyn Inbox>,
    agents: Arc<dyn AgentDirectory>,
    queue: Arc<dyn Queue<SubagentTask>>,
    pool: ExecutorPool,
    cancel: CancellationToken,
) -> Vec<JoinHandle<()>> {
    let inner = store.clone();
    spawn_bounded_executors(store, queue, SUBAGENT, pool, cancel, move |task, span| {
        let store = inner.clone();
        let agents = agents.clone();
        async move { handle_task(store.as_ref(), agents.as_ref(), task, span).await }
    })
}

async fn open(
    store: &dyn Inbox,
    tenant_id: &str,
    child_session_id: &str,
    tool_call_id: &str,
    message: Option<DraftMessage>,
    span: &SpanContext,
) -> Outcome {
    let Some(message) = message else {
        return Outcome::SubagentStarted;
    };
    let id = format!("open:{child_session_id}:{tool_call_id}");
    match send_message(store, tenant_id, child_session_id, &id, message, span).await {
        Ok(_) => Outcome::SubagentStarted,
        Err(err) => SettleError::new(
            ErrorInfo::internal(format!("failed to send the child's opening message: {err}")),
            false,
        )
        .into(),
    }
}

async fn send_message(
    store: &dyn Inbox,
    tenant_id: &str,
    session_id: &str,
    id: &str,
    message: DraftMessage,
    span: &SpanContext,
) -> Result<(), ExecuteError> {
    execute(
        store,
        ExecuteInput {
            id: Some(id.into()),
            session_id: session_id.to_string(),
            caller: Caller::System {
                tenant_id: tenant_id.to_string(),
            },
            command: CommandPayload::SendMessage {
                message,
                stream: false,
                turn_id: Some(id.to_string()),
                parent_id: None,
            },
            span: span.child("send_session_message"),
        },
    )
    .await
    .map(|_| ())
}

async fn cancel_child(
    store: &dyn Inbox,
    tenant_id: &str,
    child_session_id: &str,
    tool_call_id: &str,
    span: &SpanContext,
) {
    let cancelled = execute(
        store,
        ExecuteInput {
            id: Some(format!("cancel:{child_session_id}:{tool_call_id}").into()),
            session_id: child_session_id.to_string(),
            caller: Caller::System {
                tenant_id: tenant_id.to_string(),
            },
            command: CommandPayload::CancelSession,
            span: span.child("cancel_subagent"),
        },
    )
    .await;
    if let Err(error) = cancelled {
        tracing::warn!(%child_session_id, %error, "cancelling a child whose call ended failed");
    }
}

async fn handle_task(
    store: &dyn Inbox,
    agents: &dyn AgentDirectory,
    task: SubagentTask,
    span: SpanContext,
) -> Result<(), String> {
    let SubagentTask {
        parent_session_id,
        tenant_id,
        tool_call_id,
        attempt,
        child_session_id,
        agent_id,
        owner,
        ancestry,
        message,
        retry,
        events,
    } = task;
    let created = execute(
        store,
        ExecuteInput {
            id: Some(format!("create:{child_session_id}").into()),
            session_id: child_session_id.clone(),
            caller: Caller::System {
                tenant_id: tenant_id.clone(),
            },
            command: create_session_command(
                agents,
                agent_id,
                owner,
                ancestry,
                retry,
                SessionSeed {
                    events,
                    ..SessionSeed::default()
                },
            )
            .await,
            span: span.child("create_subagent"),
        },
    )
    .await
    .map(|_| ())
    .or_else(|created| match created {
        ExecuteError::Command(SessionError::SessionAlreadyCreated) => Ok(()),
        err => Err(err),
    });
    let outcome = match created {
        Ok(()) => {
            open(
                store,
                &tenant_id,
                &child_session_id,
                &tool_call_id,
                message,
                &span,
            )
            .await
        }
        Err(err) => SettleError::new(
            ErrorInfo::internal(format!("failed to create child session: {err}")),
            false,
        )
        .into(),
    };
    let call = WorkId::new(EffectKind::Subagent, tool_call_id.clone(), Some(attempt));
    if matches!(outcome, Outcome::SubagentStarted)
        && !store.is_open(&tenant_id, &parent_session_id, &call).await
    {
        cancel_child(store, &tenant_id, &child_session_id, &tool_call_id, &span).await;
    }
    let parent_command = CommandPayload::settle(
        EffectKind::Subagent,
        tool_call_id.clone(),
        Some(attempt),
        outcome,
    );

    settled(
        store,
        &tenant_id,
        &parent_session_id,
        parent_command,
        span.child("subagent_parent_update"),
    )
    .await
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::Mutex;

    use super::*;
    use crate::protocol::RetryPolicy;
    use crate::protocol::{Issuer, Requester, SessionOwner, Subject};
    use crate::runtime::worker::directory::EmptyAgentDirectory;

    struct Recorder {
        open: bool,
        cancelled: Mutex<Vec<String>>,
    }

    #[async_trait::async_trait]
    impl Inbox for Recorder {
        async fn submit(
            &self,
            _tenant_id: &str,
            session_id: &str,
            command: substructure_session::Command,
        ) -> Result<substructure_session::CommandResult, substructure_session::Error> {
            if matches!(command.payload, CommandPayload::CancelSession) {
                self.cancelled.lock().unwrap().push(session_id.to_string());
            }
            Ok(Ok(substructure_session::Accepted {
                seq: 1,
                turn_id: None,
                queued: false,
            }))
        }

        async fn is_open(&self, _: &str, _: &str, _: &WorkId) -> bool {
            self.open
        }
    }

    fn task() -> SubagentTask {
        SubagentTask {
            parent_session_id: "parent".into(),
            tenant_id: "t".into(),
            tool_call_id: "call-sa".into(),
            attempt: 0,
            child_session_id: "child".into(),
            agent_id: "helper".into(),
            owner: SessionOwner {
                tenant_id: "t".into(),
                requester: Requester::new(Subject::new(Issuer::app(), "u"), Default::default()),
                metadata: HashMap::new(),
            },
            ancestry: vec!["parent".into()],
            message: None,
            retry: RetryPolicy::no_retry(),
            events: None,
        }
    }

    async fn cancelled_when(open: bool) -> Vec<String> {
        let inbox = Recorder {
            open,
            cancelled: Mutex::new(Vec::new()),
        };
        handle_task(&inbox, &EmptyAgentDirectory, task(), SpanContext::root())
            .await
            .expect("the task finishes");
        inbox.cancelled.into_inner().unwrap()
    }

    #[tokio::test]
    async fn a_child_whose_call_ended_while_it_started_is_cancelled() {
        assert_eq!(cancelled_when(false).await, ["child"]);
    }

    #[tokio::test]
    async fn a_child_whose_call_is_open_runs() {
        assert!(cancelled_when(true).await.is_empty());
    }
}
