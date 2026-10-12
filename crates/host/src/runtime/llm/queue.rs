use serde::{Deserialize, Serialize};

use crate::protocol::DeferToolsStrategy;
use crate::protocol::{LlmRequest, RetryPolicy, SessionOwner};
use crate::runtime::executor::{Bound, BoundedTask};
use crate::runtime::session::state::EffectKind;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LlmTask {
    pub session_id: String,
    pub tenant_id: String,
    pub agent_id: String,
    pub call_id: String,
    pub attempt: u32,
    pub llm: String,
    pub request: LlmRequest,
    pub defer_tools_strategy: DeferToolsStrategy,
    pub stream: bool,
    pub owner: SessionOwner,
    pub ancestry: Vec<String>,
    pub turn_id: Option<String>,
    pub retry: RetryPolicy,
}

impl BoundedTask for LlmTask {
    fn bound(&self) -> Option<Bound> {
        Some(Bound {
            tenant_id: self.tenant_id.clone(),
            session_id: self.session_id.clone(),
            kind: EffectKind::LlmCall,
            id: self.call_id.clone(),
            attempt: Some(self.attempt),
            queue_timeout: self.retry.queue_timeout(),
            run_timeout: self.retry.run_timeout(),
        })
    }
}
