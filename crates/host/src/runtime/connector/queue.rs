use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::connectors::registry::ConnectionPath;
use crate::connectors::Requester;
use crate::protocol::RetryPolicy;
use crate::runtime::executor::{Bound, BoundedTask};
use crate::runtime::session::state::EffectKind;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "task", rename_all = "snake_case")]
pub enum ConnectorTask {
    Sync {
        session_id: String,
        tenant_id: String,
        connection_id: ConnectionPath,
        requester: Requester,
        attempt: u32,
        retry: RetryPolicy,
    },
    CallTool {
        session_id: String,
        tenant_id: String,
        tool_call_id: String,
        attempt: u32,
        connection_id: ConnectionPath,
        requester: Requester,
        remote_name: String,
        arguments: Value,
        retry: RetryPolicy,
    },
    RunScript {
        session_id: String,
        tenant_id: String,
        tool_call_id: String,
        attempt: u32,
        script: Box<crate::codemode::Script>,
        retry: RetryPolicy,
    },
}

impl BoundedTask for ConnectorTask {
    fn bound(&self) -> Option<Bound> {
        let (tenant_id, session_id, kind, id, attempt, retry) = match self {
            ConnectorTask::Sync {
                tenant_id,
                session_id,
                connection_id,
                attempt,
                retry,
                ..
            } => (
                tenant_id,
                session_id,
                EffectKind::ConnectorSync,
                connection_id.to_string(),
                attempt,
                retry,
            ),
            ConnectorTask::CallTool {
                tenant_id,
                session_id,
                tool_call_id,
                attempt,
                retry,
                ..
            }
            | ConnectorTask::RunScript {
                tenant_id,
                session_id,
                tool_call_id,
                attempt,
                retry,
                ..
            } => (
                tenant_id,
                session_id,
                EffectKind::ToolCall,
                tool_call_id.clone(),
                attempt,
                retry,
            ),
        };
        Some(Bound {
            tenant_id: tenant_id.clone(),
            session_id: session_id.clone(),
            kind,
            id,
            attempt: Some(*attempt),
            queue_timeout: retry.queue_timeout(),
            run_timeout: retry.run_timeout(),
        })
    }
}
