use serde::{Deserialize, Serialize};

use crate::protocol::{DraftMessage, RetryPolicy, SessionOwner};
use crate::runtime::executor::{Bound, BoundedTask};
use crate::runtime::session::state::EffectKind;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubagentTask {
    pub parent_session_id: String,
    pub tenant_id: String,
    pub tool_call_id: String,
    pub attempt: u32,
    pub child_session_id: String,
    pub agent_id: String,
    pub owner: SessionOwner,
    pub ancestry: Vec<String>,
    pub message: Option<DraftMessage>,
    pub retry: RetryPolicy,
    #[serde(default)]
    pub events: Option<String>,
}

impl BoundedTask for SubagentTask {
    fn bound(&self) -> Option<Bound> {
        Some(Bound {
            tenant_id: self.tenant_id.clone(),
            session_id: self.parent_session_id.clone(),
            kind: EffectKind::Subagent,
            id: self.tool_call_id.clone(),
            attempt: Some(self.attempt),
            queue_timeout: self.retry.queue_timeout(),
            run_timeout: self.retry.run_timeout(),
        })
    }
}
