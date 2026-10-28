use std::sync::Arc;

use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::codemode::Sandbox;
use crate::connectors::registry::Connections;
use crate::connectors::ConnectorError;
use crate::plugins::PluginResolver;
use crate::protocol::StoredResult;
use crate::runtime::blob::BlobStore;
use crate::runtime::executor::{settled, spawn_bounded_executors, ExecutorPool};
use crate::runtime::inbox::Inbox;
use crate::runtime::queue::{Queue, CONNECTOR, SCRIPT};
use crate::runtime::session::command::{CommandPayload, Outcome, SettleError};
use crate::runtime::session::engine_tools;
use crate::runtime::session::state::EffectKind;
use crate::runtime::span::SpanContext;
use substructure_session::Session;

use super::ConnectorTask;
use crate::protocol::{ErrorCode, ErrorInfo};

pub fn spawn_connector_task_executor(
    store: Arc<dyn Inbox>,
    connections: Option<Arc<Connections>>,
    queue: Arc<dyn Queue<ConnectorTask>>,
    pool: ExecutorPool,
    cancel: CancellationToken,
) -> Vec<JoinHandle<()>> {
    let inner = store.clone();
    spawn_bounded_executors(store, queue, CONNECTOR, pool, cancel, move |task, span| {
        let store = inner.clone();
        let connections = connections.clone();
        async move { handle_task(store.as_ref(), connections.as_deref(), None, task, span).await }
    })
}

const SCRIPTS_WAITING: usize = 64;

/// Script steps wait for a sandbox slot here, apart from connector calls.
pub fn spawn_script_executor(
    store: Arc<dyn Inbox>,
    sandbox: Arc<Sandbox>,
    queue: Arc<dyn Queue<ConnectorTask>>,
    cancel: CancellationToken,
) -> Vec<JoinHandle<()>> {
    let pool = ExecutorPool {
        workers: 1,
        concurrency: SCRIPTS_WAITING,
    };
    let inner = store.clone();
    spawn_bounded_executors(store, queue, SCRIPT, pool, cancel, move |task, span| {
        let store = inner.clone();
        let sandbox = sandbox.clone();
        async move { handle_task(store.as_ref(), None, Some(&sandbox), task, span).await }
    })
}

fn unreachable() -> ConnectorError {
    ConnectorError::permanent("no connections are configured on this engine")
}

async fn handle_task(
    store: &dyn Inbox,
    connections: Option<&Connections>,
    sandbox: Option<&Arc<Sandbox>>,
    task: ConnectorTask,
    span: SpanContext,
) -> Result<(), String> {
    match task {
        ConnectorTask::RunScript {
            session_id,
            tenant_id,
            tool_call_id,
            attempt,
            ..
        } => {
            let Some(sandbox) = sandbox else {
                return Err("a script reached the connector executor".to_string());
            };
            let Some(script) = store.script(&tenant_id, &session_id, &tool_call_id).await else {
                return Ok(());
            };
            let known = script.calls.len() as u32;
            let step = sandbox.run(&tenant_id, script).await;
            let command = crate::codemode::settle(tool_call_id, attempt, known, step);
            submit(store, &session_id, &tenant_id, command, span, "script").await
        }
        ConnectorTask::Sync {
            session_id,
            tenant_id,
            connection_id,
            requester,
            attempt,
            ..
        } => {
            let listed = match connections {
                Some(c) => c.list_tools(&tenant_id, &connection_id, &requester).await,
                None => Err(unreachable()),
            };
            let command = match listed {
                Ok(offer) => CommandPayload::settle(
                    EffectKind::ConnectorSync,
                    connection_id.to_string(),
                    Some(attempt),
                    Outcome::Connector {
                        server: offer.server,
                        prefix: offer.prefix,
                        tools: offer.tools,
                        instructions: offer.instructions,
                    },
                ),
                Err(err) => CommandPayload::settle(
                    EffectKind::ConnectorSync,
                    connection_id.to_string(),
                    Some(attempt),
                    SettleError::new(
                        ErrorInfo::new(ErrorCode::HandlerError, err.message.clone()),
                        err.retryable,
                    )
                    .auth(err.auth),
                ),
            };
            submit(
                store,
                &session_id,
                &tenant_id,
                command,
                span,
                "connector_sync",
            )
            .await
        }

        ConnectorTask::CallTool {
            session_id,
            tenant_id,
            tool_call_id,
            attempt,
            connection_id,
            requester,
            remote_name,
            arguments,
            ..
        } => {
            let result = match connections {
                Some(c) => {
                    c.call_tool(
                        &tenant_id,
                        &connection_id,
                        &requester,
                        &remote_name,
                        &arguments,
                    )
                    .await
                }
                None => Err(unreachable()),
            };
            let command = settle_call(tool_call_id, attempt, result);
            submit(
                store,
                &session_id,
                &tenant_id,
                command,
                span,
                "connector_call",
            )
            .await
        }
    }
}

pub async fn answer_locally(
    session: &Session,
    tool_call_id: &str,
    blobs: &dyn BlobStore,
    plugins: &dyn PluginResolver,
) -> Option<StoredResult> {
    let state = session.state();
    if let Some(call) = state.attachment_call(tool_call_id) {
        return Some(crate::attachments::tools::answer(call, blobs).await);
    }
    let call = state.skill_call(tool_call_id)?;
    let bundle = match call.plugin_id.is_empty() {
        true => None,
        false => plugins.resolve(session.tenant_id(), &call.plugin_id).await,
    };
    Some(engine_tools::skill_answer(
        state.at(call.node.as_deref()),
        bundle.as_ref(),
        &call.arguments,
    ))
}

pub fn settle_call(
    tool_call_id: String,
    attempt: u32,
    result: Result<StoredResult, ConnectorError>,
) -> CommandPayload {
    let outcome = match result {
        Ok(result) if result.is_error => SettleError::new(
            ErrorInfo::new(ErrorCode::HandlerError, result.rendered()),
            false,
        )
        .into(),
        Ok(result) => Outcome::Tool { result },
        Err(err) => SettleError::new(
            ErrorInfo::new(ErrorCode::HandlerError, err.message),
            err.retryable,
        )
        .auth(err.auth)
        .into(),
    };
    CommandPayload::settle(EffectKind::ToolCall, tool_call_id, Some(attempt), outcome)
}

async fn submit(
    store: &dyn Inbox,
    session_id: &str,
    tenant_id: &str,
    command: CommandPayload,
    span: SpanContext,
    name: &'static str,
) -> Result<(), String> {
    settled(store, tenant_id, session_id, command, span.child(name)).await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn outcome(
        content: &str,
        structured: Option<serde_json::Value>,
        is_error: bool,
    ) -> StoredResult {
        StoredResult {
            content: vec![crate::protocol::StoredContent::Text {
                text: content.to_string(),
            }],
            structured_content: structured,
            is_error,
        }
    }

    #[test]
    fn a_tool_that_ran_and_failed_is_terminal_not_a_retry() {
        let cmd = settle_call("tc-1".into(), 0, Ok(outcome("no such issue", None, true)));
        match cmd {
            CommandPayload::SettleEffect {
                kind: EffectKind::ToolCall,
                outcome: Outcome::Error(e),
                ..
            } => {
                assert_eq!(e.error.message, "no such issue");
                assert!(!e.retryable, "the tool ran; running it again says the same");
            }
            other => panic!("expected a terminal failure; got {other:?}"),
        }
    }

    #[test]
    fn a_transport_fault_keeps_the_calls_own_retry_policy() {
        let cmd = settle_call(
            "tc-1".into(),
            2,
            Err(ConnectorError::retryable("connection reset")),
        );
        match cmd {
            CommandPayload::SettleEffect {
                kind: EffectKind::ToolCall,
                outcome: Outcome::Error(e),
                attempt,
                ..
            } => {
                assert!(e.retryable, "the engine's problem, not the model's");
                assert_eq!(attempt, Some(2), "fenced against a stale executor");
            }
            other => panic!("expected a retryable failure; got {other:?}"),
        }
    }

    #[test]
    fn structured_output_wins_over_its_rendering() {
        let structured = serde_json::json!({ "id": 7 });
        let cmd = settle_call(
            "tc-1".into(),
            0,
            Ok(outcome("Issue 7", Some(structured.clone()), false)),
        );
        match cmd {
            CommandPayload::SettleEffect {
                kind: EffectKind::ToolCall,
                outcome: Outcome::Tool { result, .. },
                ..
            } => {
                assert_eq!(
                    result.rendered(),
                    structured.to_string(),
                    "only the structured form can satisfy a declared output schema"
                );
            }
            other => panic!("expected a result; got {other:?}"),
        }
    }

    #[test]
    fn rendered_content_settles_a_connection_that_sent_no_structure() {
        let cmd = settle_call("tc-1".into(), 0, Ok(outcome("Issue 7", None, false)));
        match cmd {
            CommandPayload::SettleEffect {
                kind: EffectKind::ToolCall,
                outcome: Outcome::Tool { result, .. },
                ..
            } => assert_eq!(result.rendered(), "Issue 7"),
            other => panic!("expected a result; got {other:?}"),
        }
    }
}
