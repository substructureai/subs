use async_trait::async_trait;
use substructure_session::{Command, CommandId, CommandResult, WorkId};
use uuid::Uuid;

use crate::runtime::caller::Caller;
use crate::runtime::session::command::{CommandPayload, SessionError};
use crate::runtime::span::SpanContext;

#[async_trait]
pub trait Inbox: Send + Sync {
    async fn submit(
        &self,
        tenant_id: &str,
        session_id: &str,
        command: Command,
    ) -> Result<CommandResult, substructure_session::Error>;

    async fn is_open(&self, tenant_id: &str, session_id: &str, work: &WorkId) -> bool;

    /// A `run_code` call's script, when it is ready to run again.
    async fn script(
        &self,
        _tenant_id: &str,
        _session_id: &str,
        _tool_call_id: &str,
    ) -> Option<substructure_session::Script> {
        None
    }

    async fn ended(&self, tenant_id: &str, session_id: &str, work: &WorkId) {
        loop {
            tokio::time::sleep(RECHECK).await;
            if !self.is_open(tenant_id, session_id, work).await {
                return;
            }
        }
    }
}

pub const RECHECK: std::time::Duration = std::time::Duration::from_secs(10);

pub struct ExecuteInput {
    pub id: Option<CommandId>,
    pub session_id: String,
    pub caller: Caller,
    pub command: CommandPayload,
    pub span: SpanContext,
}

#[derive(Debug, Clone, Default)]
pub struct ExecuteOutput {
    pub turn_id: Option<String>,
    pub queued: bool,
}

#[derive(Debug, thiserror::Error)]
pub enum ExecuteError {
    #[error("command error: {0:?}")]
    Command(SessionError),
    #[error(transparent)]
    Session(#[from] substructure_session::Error),
}

pub async fn execute(
    inbox: &dyn Inbox,
    input: ExecuteInput,
) -> Result<ExecuteOutput, ExecuteError> {
    let id = input
        .id
        .or_else(|| input.command.natural_id())
        .unwrap_or_else(|| CommandId(Uuid::now_v7().to_string()));
    let tenant_id = input.caller.tenant_id().to_string();
    let command = Command {
        id,
        caller: input.caller,
        payload: input.command,
        span: input.span,
    };
    match inbox.submit(&tenant_id, &input.session_id, command).await? {
        Ok(accepted) => Ok(ExecuteOutput {
            turn_id: accepted.turn_id,
            queued: accepted.queued,
        }),
        Err(rejected) => Err(ExecuteError::Command(rejected.error)),
    }
}
