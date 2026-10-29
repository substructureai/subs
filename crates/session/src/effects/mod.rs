use super::command::SessionError;
use super::decision::Trigger;
use super::events::{CallVoided, DecisionQueued, EventPayload};
use super::schedule::Dep;
use super::state::{EffectKind, EffectPayload, EffectTracking, QueueEntry, SessionState};
use crate::connectors::{AuthNeed, RemoteTool};
use crate::cx::Cx;
use crate::ids::Ids;
use crate::protocol::{EffectStatus, ErrorCode, ErrorInfo, LlmResponse, StoredResult};
use crate::Caller;

pub mod connector;
pub mod decision;
pub mod llm;
pub mod subagent;
pub mod system1;
pub mod tool;
pub mod turn_end;

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum Outcome {
    Llm(Box<LlmResponse>),
    System1(Box<crate::system1::System1Response>),
    Tool {
        result: StoredResult,
    },
    SubagentStarted,
    Connector {
        prefix: Option<String>,
        server: Option<String>,
        tools: Vec<RemoteTool>,
        instructions: Option<String>,
    },
    /// A `run_code` script ran past the `known` calls it was given and made
    /// `calls`.
    Script {
        known: u32,
        calls: Vec<crate::events::ScriptCall>,
        #[serde(default)]
        cpu_ms: u64,
        #[serde(default)]
        file_bytes: u64,
        /// The hash of each plugin bundle whose files the script may read.
        #[serde(default)]
        pins: std::collections::BTreeMap<String, String>,
    },
    Error(SettleError),
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct SettleError {
    pub error: ErrorInfo,
    pub retryable: bool,
    pub auth: Option<AuthNeed>,
}

impl SettleError {
    pub fn new(error: ErrorInfo, retryable: bool) -> Self {
        Self {
            error,
            retryable,
            auth: None,
        }
    }

    pub fn auth(mut self, auth: Option<AuthNeed>) -> Self {
        self.auth = auth;
        self
    }
}

impl From<SettleError> for Outcome {
    fn from(e: SettleError) -> Self {
        Outcome::Error(e)
    }
}

pub const DEADLINE: &str = "deadline exceeded";
pub const QUEUED: &str = "deadline exceeded while queued";
pub const RUN: &str = "deadline exceeded while running";

pub trait KindSpec: Sync {
    fn kind(&self) -> EffectKind;

    fn authorize(
        &self,
        _state: &SessionState,
        _id: &str,
        caller: &Caller,
    ) -> Result<(), SessionError> {
        SessionState::ensure_internal(caller)
    }

    fn resolve(
        &self,
        state: &SessionState,
        id: &str,
        attempt: Option<u32>,
        caller: &Caller,
    ) -> Result<Settle, SessionError> {
        resolve_settle(state.tracking(self.kind(), id), attempt, caller)
    }

    fn settle(&self, state: &Cx<'_>, id: &str, outcome: Outcome) -> Vec<EventPayload>;

    fn errored(&self, _state: &SessionState, _id: &str, _e: &SettleError) -> Option<EventPayload> {
        None
    }

    fn terminal(&self, _state: &Cx<'_>, _id: &str, _e: &SettleError) -> Vec<EventPayload> {
        Vec::new()
    }

    fn timeout_error(&self, total: bool) -> SettleError {
        SettleError::new(
            ErrorInfo::new(ErrorCode::DeadlineExceeded, DEADLINE),
            !total,
        )
    }

    fn dispatch(&self, state: &SessionState, id: &str) -> Vec<EventPayload>;

    fn execute_trigger(&self, _state: &SessionState, _id: &str) -> Option<Trigger> {
        None
    }

    fn retry(&self, _state: &SessionState, _id: &str) -> Vec<EventPayload> {
        Vec::new()
    }

    fn requeues_on_retry(&self) -> bool {
        true
    }

    fn resend(&self, state: &SessionState, id: &str) -> Vec<EventPayload> {
        self.dispatch(state, id)
    }

    fn voids_when_missing(&self) -> bool {
        true
    }

    fn voids_when_running(&self) -> bool {
        false
    }

    fn park(&self, _state: &SessionState, _id: &str) -> Option<Dep> {
        None
    }

    fn deps(&self, _state: &SessionState, _entry: &QueueEntry) -> Vec<Dep> {
        Vec::new()
    }
}

impl EffectKind {
    pub fn spec(self) -> &'static dyn KindSpec {
        match self {
            EffectKind::LlmCall => &llm::LlmSpec,
            EffectKind::ToolCall => &tool::ToolSpec,
            EffectKind::Subagent => &subagent::SubagentSpec,
            EffectKind::ConnectorSync => &connector::ConnectorSpec,
            EffectKind::Decision => &decision::DecisionSpec,
            EffectKind::TurnEnd => &turn_end::TurnEndSpec,
            EffectKind::System1Call => &system1::System1Spec,
        }
    }
}

impl EffectPayload {
    pub fn spec(&self) -> &'static dyn KindSpec {
        self.kind().spec()
    }
}

pub(super) fn fail(
    spec: &dyn KindSpec,
    state: &Cx<'_>,
    id: &str,
    e: &SettleError,
) -> Vec<EventPayload> {
    let Some(tracking) = state.tracking(spec.kind(), id) else {
        return Vec::new();
    };
    let terminal = tracking.is_terminal_failure(e.retryable);
    let Some(errored) = spec.errored(state, id, e) else {
        return Vec::new();
    };
    let mut events = vec![errored];
    if terminal {
        events.extend(spec.terminal(state, id, e));
    }
    events
}

pub(super) fn mismatched(kind: EffectKind, outcome: &Outcome) -> Vec<EventPayload> {
    tracing::error!(?kind, ?outcome, "an effect cannot settle with this outcome");
    #[cfg(test)]
    panic!("{kind:?} cannot settle with {outcome:?}");
    #[cfg(not(test))]
    Vec::new()
}

pub(super) fn decision_queued(ids: &Ids, trigger: Trigger) -> EventPayload {
    EventPayload::DecisionQueued(DecisionQueued {
        id: ids.next(),
        trigger,
    })
}

pub fn stops(kind: EffectKind, tracking: &EffectTracking) -> bool {
    tracking.is_open()
        || (tracking.status() == EffectStatus::Running && kind.spec().voids_when_running())
}

pub(super) fn void_events(kind: EffectKind, id: String) -> Vec<EventPayload> {
    vec![EventPayload::CallVoided(CallVoided { kind, id })]
}

pub enum Settle {
    Live,
    Drop,
}

pub fn resolve_settle(
    tracking: Option<&EffectTracking>,
    attempt: Option<u32>,
    caller: &Caller,
) -> Result<Settle, SessionError> {
    match tracking {
        Some(t)
            if t.status() == EffectStatus::Pending
                && attempt.is_none_or(|a| a == t.retry.attempts) =>
        {
            Ok(Settle::Live)
        }
        _ if matches!(caller, Caller::System { .. }) => Ok(Settle::Drop),
        None => Err(SessionError::EffectNotFound),
        Some(t) if t.status() != EffectStatus::Pending => Err(SessionError::EffectNotPending),
        Some(_) => Err(SessionError::EffectAttemptMismatch),
    }
}

pub(super) fn resolve_pending(tracking: Option<&EffectTracking>) -> Settle {
    match tracking {
        Some(t) if t.status() == EffectStatus::Pending => Settle::Live,
        _ => Settle::Drop,
    }
}
