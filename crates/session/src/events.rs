use std::collections::{BTreeMap, HashMap};

use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

use super::decision::{LlmHandler, ToolHandler, Trigger};
use crate::connectors::ConnectionPath;
use crate::connectors::{AuthNeed, RemoteTool};
pub use crate::protocol::EffectKind;
use crate::protocol::{
    AgentConfig, ClientContext, ConnectorToolKind, ContextReason, ContextStrategyKind,
    DeferToolsStrategy, DraftMessage, ErrorInfo, HeldCall, InterruptOrigin, LlmFormat, LlmRequest,
    LlmResponse, Message, MessageTree, NewMessage, RetryPolicy, SessionOwner, SpawnMode,
    StoredResult, Usage, WorkerRef, WorkerState,
};
use crate::system1::{System1Request, System1Response};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum EventPayload {
    #[serde(rename = "session.created")]
    SessionCreated(Box<SessionCreated>),
    #[serde(rename = "message.new")]
    NewMessage(NewMessage),
    #[serde(rename = "head.moved")]
    HeadMoved(HeadMoved),
    #[serde(rename = "llm.call.requested")]
    LlmCallRequested(LlmCallRequested),
    #[serde(rename = "llm.call.dispatched")]
    LlmCallDispatched(LlmCallDispatched),
    #[serde(rename = "llm.call.completed")]
    LlmCallCompleted(LlmCallCompleted),
    #[serde(rename = "llm.call.errored")]
    LlmCallErrored(LlmCallErrored),
    #[serde(rename = "tool.call.requested")]
    ToolCallRequested(ToolCallRequested),
    #[serde(rename = "tool.call.dispatched")]
    ToolCallDispatched(ToolCallDispatched),
    #[serde(rename = "tool.call.completed")]
    ToolCallCompleted(ToolCallCompleted),
    #[serde(rename = "tool.call.errored")]
    ToolCallErrored(ToolCallErrored),
    #[serde(rename = "tool.call.stepped")]
    ToolCallStepped(ToolCallStepped),
    #[serde(rename = "system1.call.requested")]
    System1CallRequested(Box<System1CallRequested>),
    #[serde(rename = "system1.call.dispatched")]
    System1CallDispatched(System1CallDispatched),
    #[serde(rename = "system1.call.completed")]
    System1CallCompleted(Box<System1CallCompleted>),
    #[serde(rename = "system1.call.errored")]
    System1CallErrored(System1CallErrored),
    #[serde(rename = "connector.sync.requested")]
    ConnectorSyncRequested(ConnectorSyncRequested),
    #[serde(rename = "connector.sync.completed")]
    ConnectorSyncCompleted(Box<ConnectorSyncCompleted>),
    #[serde(rename = "connector.sync.errored")]
    ConnectorSyncErrored(ConnectorSyncErrored),
    #[serde(rename = "connector.auth.failed")]
    ConnectorAuthFailed(ConnectorAuthFailed),
    #[serde(rename = "subagent.requested")]
    SubagentRequested(SubagentRequested),
    #[serde(rename = "subagent.dispatched")]
    SubagentDispatched(SubagentDispatched),
    #[serde(rename = "subagent.started")]
    SubagentStarted(SubagentStarted),
    #[serde(rename = "subagent.errored")]
    SubagentErrored(SubagentErrored),
    #[serde(rename = "session.interrupted")]
    SessionInterrupted(SessionInterrupted),
    #[serde(rename = "session.interrupt_resumed")]
    InterruptResumed(InterruptResumed),
    #[serde(rename = "decision.dispatched")]
    DecisionDispatched(DecisionDispatched),
    #[serde(rename = "decision.completed")]
    DecisionCompleted(DecisionCompleted),
    #[serde(rename = "decision.errored")]
    DecisionErrored(DecisionErrored),
    #[serde(rename = "session.message_requested")]
    SessionMessageRequested(SessionMessageRequested),
    #[serde(rename = "worker.state.updated")]
    WorkerStateUpdated(WorkerStateUpdated),
    #[serde(rename = "agent.updated")]
    AgentConfigUpdated(AgentConfigUpdated),
    #[serde(rename = "subagent.turn_completed")]
    SubagentTurnCompleted(SubagentTurnCompleted),
    #[serde(rename = "decision.queued")]
    DecisionQueued(DecisionQueued),
    #[serde(rename = "decision.dropped")]
    DecisionDropped(DecisionDropped),
    #[serde(rename = "decision.amended")]
    DecisionAmended(DecisionAmended),
    #[serde(rename = "call.voided")]
    CallVoided(CallVoided),
    #[serde(rename = "channels.updated")]
    ChannelsUpdated(serde_json::Value),
    #[serde(rename = "session.cancelled")]
    SessionCancelled,
    #[serde(rename = "session.done")]
    SessionDone(SessionDone),
    #[serde(rename = "turn.started")]
    TurnStarted(TurnStarted),
    #[serde(rename = "turn.completed")]
    TurnCompleted(TurnCompleted),
    #[serde(rename = "work.unsent")]
    WorkUnsent(WorkUnsent),
    #[serde(rename = "delivery.failed")]
    DeliveryFailed(DeliveryFailed),
    #[serde(rename = "delivery.recovered")]
    DeliveryRecovered(DeliveryRecovered),
    #[serde(rename = "delivery.skipped")]
    DeliverySkipped(DeliverySkipped),
    #[serde(rename = "delivery.parked")]
    DeliveryParked(DeliveryParked),
    #[serde(rename = "delivery.resumed")]
    DeliveryResumed(DeliveryResumed),
    #[serde(rename = "branch.loaded")]
    BranchLoaded(BranchLoaded),
    #[serde(rename = "branch.unloaded")]
    BranchUnloaded(BranchUnloaded),
    #[serde(rename = "context.limited")]
    ContextLimited(Box<ContextLimited>),
    #[serde(rename = "context.replaced")]
    ContextReplaced(ContextReplaced),
    #[serde(rename = "context.kept")]
    ContextKept,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContextLimited {
    pub reason: ContextReason,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub strategy: Option<ContextStrategyKind>,
    pub tokens: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub window: Option<u64>,
    pub call: HeldCall,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContextReplaced {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<ContextReason>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<ContextSummary>,
    pub kept: usize,
    pub dropped: usize,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ContextSummary {
    pub id: String,
    pub text: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct BranchLoaded {
    pub seqs: Vec<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BranchUnloaded {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub head_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkUnsent {
    pub kind: EffectKind,
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attempt: Option<u32>,
    pub error: String,
    pub retry_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeliveryFailed {
    pub subscription: String,
    pub seq: u64,
    pub attempt: u32,
    pub error: String,
    pub retry_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeliveryRecovered {
    pub subscription: String,
    pub through: u64,
    pub attempts: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeliverySkipped {
    pub subscription: String,
    pub seq: u64,
    pub attempt: u32,
    pub error: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeliveryParked {
    pub subscription: String,
    pub seq: u64,
    pub attempt: u32,
    pub error: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeliveryResumed {
    pub subscription: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionCreated {
    pub agent_id: String,
    pub identity: SessionOwner,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ancestry: Vec<String>,
    pub worker_retry: RetryPolicy,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worker: Option<WorkerRef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub events: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionDone {}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HeadMoved {
    pub head_id: String,
}

impl MessageTree {
    pub fn path_to(&self, leaf: &str) -> Vec<Message> {
        let mut by_id: HashMap<&str, &NewMessage> = self
            .nodes
            .iter()
            .map(|n| (n.message.id.as_str(), n))
            .collect();
        let mut path = Vec::new();
        let mut cursor = Some(leaf.to_string());
        while let Some(id) = cursor.take() {
            let Some(node) = by_id.remove(id.as_str()) else {
                break;
            };
            path.push(node.message.clone());
            cursor = node.parent_id.clone();
        }
        path.reverse();
        path
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LlmCallRequested {
    pub id: String,
    pub attempt: u32,
    pub llm: String,
    pub request: LlmRequest,
    pub stream: bool,
    pub retry: RetryPolicy,
    #[serde(default)]
    pub handler: LlmHandler,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub format: Option<LlmFormat>,
    #[serde(default)]
    pub defer_tools_strategy: DeferToolsStrategy,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LlmCallDispatched {
    pub id: String,
    pub attempt: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCallDispatched {
    pub id: String,
    pub attempt: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubagentDispatched {
    pub id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LlmCallCompleted {
    pub id: String,
    pub attempt: u32,
    pub response: LlmResponse,
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LlmCallErrored {
    pub id: String,
    pub attempt: u32,
    pub error: ErrorInfo,
    #[serde(default = "default_true")]
    pub retryable: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct System1CallRequested {
    pub id: String,
    pub attempt: u32,
    pub system1: String,
    pub request: System1Request,
    pub retry: RetryPolicy,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct System1CallDispatched {
    pub id: String,
    pub attempt: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct System1CallCompleted {
    pub id: String,
    pub attempt: u32,
    pub response: System1Response,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct System1CallErrored {
    pub id: String,
    pub attempt: u32,
    pub error: ErrorInfo,
    #[serde(default = "default_true")]
    pub retryable: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConnectorSyncRequested {
    pub path: ConnectionPath,
    pub attempt: u32,
    pub retry: RetryPolicy,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConnectorSyncCompleted {
    pub path: ConnectionPath,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prefix: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server: Option<String>,
    pub tools: Vec<RemoteTool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instructions: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConnectorSyncErrored {
    pub path: ConnectionPath,
    pub error: ErrorInfo,
    #[serde(default)]
    pub retryable: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth: Option<AuthNeed>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConnectorAuthFailed {
    pub path: ConnectionPath,
    pub auth: AuthNeed,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubagentRequested {
    pub id: String,
    pub agent_id: String,
    #[serde(default)]
    pub session_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<DraftMessage>,
    pub retry: RetryPolicy,
    #[serde(default, skip_serializing_if = "is_blocking")]
    pub mode: SpawnMode,
}

fn is_blocking(mode: &SpawnMode) -> bool {
    *mode == SpawnMode::Blocking
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubagentStarted {
    pub id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubagentErrored {
    pub id: String,
    pub error: ErrorInfo,
    #[serde(default)]
    pub retryable: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ConnectorTarget {
    Remote {
        path: ConnectionPath,
        remote_name: String,
    },
    Find,
    Call,
    Skill {
        plugin: String,
        skill: String,
    },
    Attachment,
    /// A tool a skill holds back, called before that skill loaded. The engine
    /// answers it with an error and runs nothing.
    Locked,
    /// `run_code`.
    Code,
    /// A call a script may not make. The engine answers it with `reason`.
    Refused {
        reason: String,
    },
    /// A script's `tool_search`, over the tools a script can call.
    ScriptSearch,
}

impl ConnectorTarget {
    pub fn kind(&self) -> ConnectorToolKind {
        match self {
            Self::Remote { .. } => ConnectorToolKind::Remote,
            Self::Find | Self::ScriptSearch => ConnectorToolKind::Find,
            Self::Call => ConnectorToolKind::Call,
            Self::Skill { .. } => ConnectorToolKind::Skill,
            Self::Attachment => ConnectorToolKind::Attachment,
            Self::Locked | Self::Refused { .. } => ConnectorToolKind::Call,
            Self::Code => ConnectorToolKind::Code,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCallRequested {
    pub id: String,
    pub attempt: u32,
    pub name: String,
    pub arguments: String,
    #[serde(default)]
    pub handler: ToolHandler,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<ConnectorTarget>,
    pub retry: RetryPolicy,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub script: Option<ScriptCaller>,
}

/// A `run_code` call's script ran a step, and asked for these calls.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCallStepped {
    pub id: String,
    pub cpu_ms: u64,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub asked: Vec<ScriptAsk>,
}

/// A call a script asked for, before a decision starts it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScriptAsk {
    pub id: String,
    pub seq: u32,
    pub name: String,
    pub arguments: String,
}

impl ScriptAsk {
    pub fn tool_call(&self) -> crate::protocol::ToolCall {
        crate::protocol::ToolCall {
            id: self.id.clone(),
            call_type: "function".to_string(),
            function: crate::protocol::ToolCallFunction {
                name: self.name.clone(),
                arguments: self.arguments.clone(),
            },
        }
    }
}

/// The `run_code` call that made this call, and where in its script.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScriptCaller {
    pub parent: String,
    pub seq: u32,
    /// What the script called, when routing renamed it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub called: Option<ScriptCall>,
}

pub use crate::codemode::ScriptCall;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCallCompleted {
    pub id: String,
    pub name: String,
    pub result: StoredResult,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCallErrored {
    pub id: String,
    pub name: String,
    pub error: ErrorInfo,
    #[serde(default)]
    pub retryable: bool,
}

impl InterruptOrigin {
    pub fn privilege(self) -> u8 {
        match self {
            InterruptOrigin::System => 3,
            InterruptOrigin::Operator => 2,
            InterruptOrigin::Machine => 1,
            InterruptOrigin::Frontend => 0,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionInterrupted {
    pub interrupt_id: String,
    pub origin: InterruptOrigin,
    pub reason: String,
    pub payload: serde_json::Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub anchor: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InterruptResumed {
    pub interrupt_id: String,
    pub payload: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DecisionDispatched {
    pub id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DecisionCompleted {
    pub id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DecisionErrored {
    pub id: String,
    pub error: ErrorInfo,
    #[serde(default = "default_true")]
    pub retryable: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionMessageRequested {
    pub target_session_id: String,
    pub message: DraftMessage,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkerStateUpdated {
    pub state: WorkerState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub anchor: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentConfigUpdated {
    pub config: AgentConfig,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub anchor: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TurnStarted {
    pub turn_id: String,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub metadata: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TurnCompleted {
    pub turn_id: String,
    #[serde(default, skip_serializing_if = "serde_json::Value::is_null")]
    pub data: serde_json::Value,
    #[serde(default)]
    pub turn_cost: Decimal,
    #[serde(default)]
    pub turn_token_usage: Usage,
    #[serde(default)]
    pub turn_calls: Box<TurnCalls>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_started_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<ErrorInfo>,
}

/// The calls a turn made, by kind. A subagent call costs what its child spent
/// answering it.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct TurnCalls {
    pub llm: CallTotals,
    pub tool: CallTotals,
    pub subagent: CallTotals,
    #[serde(default)]
    pub system1: CallTotals,
}

/// A retry is not a new call, but each failed attempt is an error.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct CallTotals {
    pub count: u32,
    pub errors: u32,
    #[serde(default, skip_serializing_if = "Decimal::is_zero")]
    pub cost: Decimal,
    #[serde(default, skip_serializing_if = "Usage::is_empty")]
    pub tokens: Usage,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DecisionQueued {
    pub id: String,
    pub trigger: Trigger,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DecisionDropped {
    pub id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DecisionAmended {
    pub id: String,
    pub messages: Vec<DraftMessage>,
    #[serde(default)]
    pub client: ClientContext,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub metadata: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CallVoided {
    pub kind: EffectKind,
    pub id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubagentTurnCompleted {
    pub id: String,
    #[serde(default)]
    pub cost: Decimal,
    #[serde(default)]
    pub token_usage: Usage,
    #[serde(default, skip_serializing_if = "serde_json::Value::is_null")]
    pub data: serde_json::Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<ErrorInfo>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_old_channels_event_still_reads_and_writes_back_unchanged() {
        let old = serde_json::json!({
            "type": "channels.updated",
            "decision_id": "d1",
            "finishes_turn": true,
            "channels": { "slack": { "status": "is working…" } },
        });
        let payload: EventPayload = serde_json::from_value(old.clone()).unwrap();
        assert!(matches!(payload, EventPayload::ChannelsUpdated(_)));
        assert_eq!(serde_json::to_value(&payload).unwrap(), old);
    }

    #[test]
    fn a_kind_of_call_without_cost_or_tokens_writes_only_its_counts() {
        let calls = TurnCalls {
            llm: CallTotals {
                count: 2,
                cost: Decimal::new(5, 3),
                tokens: Usage::new(10, 0, 0, 5),
                ..Default::default()
            },
            tool: CallTotals {
                count: 3,
                errors: 1,
                ..Default::default()
            },
            ..Default::default()
        };
        let written = serde_json::to_value(&calls).unwrap();
        assert_eq!(
            written["tool"],
            serde_json::json!({ "count": 3, "errors": 1 })
        );
        assert_eq!(
            written["subagent"],
            serde_json::json!({ "count": 0, "errors": 0 })
        );
        assert_eq!(written["llm"]["tokens"]["total"], 15);
        assert_eq!(serde_json::from_value::<TurnCalls>(written).unwrap(), calls);
    }

    #[test]
    fn a_turn_written_before_its_calls_were_counted_still_reads() {
        let old = serde_json::json!({ "type": "turn.completed", "turn_id": "t1" });
        let payload: EventPayload = serde_json::from_value(old).unwrap();
        assert!(
            matches!(payload, EventPayload::TurnCompleted(t) if *t.turn_calls == TurnCalls::default())
        );
    }
}
