use crate::protocol::{AgentConfig, DraftMessage, ErrorInfo, WorkerState};
use crate::runtime::session::decision::Action;
use crate::runtime::span::SpanContext;
use crate::runtime::Caller;
pub use substructure_session::request::WorkerDecisionRequest;

#[derive(Debug)]
pub struct SubmitDecision {
    pub session_id: String,
    pub caller: Caller,
    pub decision_id: String,
    pub transcript: Vec<DraftMessage>,
    pub actions: Vec<Action>,

    pub state: Option<WorkerState>,

    pub agent: Option<AgentConfig>,
    pub span: SpanContext,
}

#[derive(Debug)]
pub struct FailDecision {
    pub session_id: String,
    pub caller: Caller,
    pub decision_id: String,
    pub attempt: Option<u32>,
    pub error: ErrorInfo,
    pub retryable: bool,
    pub span: SpanContext,
}
