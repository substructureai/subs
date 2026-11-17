use std::collections::{BTreeMap, BTreeSet, HashMap};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use super::decision::{BranchConfig, LlmHandler, PromptSource, ToolHandler, Trigger};
use super::events::*;
use super::prompt_context;
use super::tool_contract::classify_arguments;
use crate::connectors::ConnectionPath;
use crate::connectors::{filter, AuthNeed, RemoteTool};
use crate::protocol::{
    AgentTool, BoundServer, ConnectorTool, ConnectorToolKind, DeferToolsStrategy, Handler,
    McpServer, StoredResult, SubagentToolsStrategy, CALL_TOOL, RUN_CODE, SKILL,
};
use crate::system1::System1Request;

#[derive(Debug, Clone, PartialEq)]
pub struct Source {
    pub server: BoundServer,
    pub offered: Vec<RemoteTool>,
    pub instructions: Option<String>,
}

#[derive(serde::Serialize)]
struct Summary<'a> {
    mcp_server: &'a ConnectionPath,
    tools: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    about: Option<&'a str>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum CallTarget {
    Connector(ConnectorTool),
    Declared(AgentTool),
    Subagent(ConnectorTool),
}

impl CallTarget {
    pub fn input(&self) -> Option<serde_json::Value> {
        match self {
            CallTarget::Connector(t) | CallTarget::Subagent(t) => t.input.clone(),
            CallTarget::Declared(t) => t.input.clone(),
        }
    }
}

pub fn inner_arguments(raw: &serde_json::Value) -> String {
    match raw.get("arguments") {
        Some(serde_json::Value::String(text)) => text.clone(),
        Some(value) => value.to_string(),
        None => "{}".to_string(),
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct SkillCall {
    pub node: Option<String>,
    pub plugin_id: String,
    pub arguments: String,
    /// A script made the call.
    pub scripted: bool,
}
use rust_decimal::Decimal;

pub use crate::protocol::EffectKind;
use crate::protocol::{
    AgentConfig, BranchMode, BranchView, ClientContext, ContextReason, DecisionTurn, DraftMessage,
    Effect, EffectStatus, HeldCall, InterruptOrigin, LlmFormat, LlmRequest, LlmTool, Message,
    MessageTree, NewMessage, ReasoningConfig, RetryConfig, RetryPolicy, Role, SessionOwner,
    SpawnMode, SubagentTools, Usage, WorkerRef, WorkerState,
};
use crate::retry::RetryState;

pub struct ApplyContext {
    pub occurred_at: DateTime<Utc>,
    pub sequence: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionStatus {
    Idle,
    Interrupted {
        interrupt_id: String,
        origin: InterruptOrigin,
        reason: String,
    },
    Done,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EffectTracking {
    status: EffectStatus,
    pub retry: RetryState,
    pub retry_policy: RetryPolicy,
    pub deadline: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub resend: bool,
}

impl EffectTracking {
    pub fn new(retry_policy: RetryPolicy, now: DateTime<Utc>) -> Self {
        let deadline = retry_policy.attempt_deadline(now);
        Self {
            status: EffectStatus::Pending,
            retry: RetryState::default(),
            retry_policy,
            deadline,
            started_at: Some(now),
            resend: false,
        }
    }

    pub fn new_queued(retry_policy: RetryPolicy) -> Self {
        Self {
            status: EffectStatus::Queued,
            retry: RetryState::default(),
            retry_policy,
            deadline: None,
            started_at: None,
            resend: false,
        }
    }

    pub fn status(&self) -> EffectStatus {
        self.status
    }

    fn move_to(&mut self, next: EffectStatus, from: &[EffectStatus]) {
        if !from.contains(&self.status) {
            tracing::error!(from = ?self.status, to = ?next, "illegal effect transition");
            #[cfg(test)]
            panic!("illegal effect transition {:?} → {next:?}", self.status);
        }
        self.status = next;
    }

    pub fn restart(&mut self, now: DateTime<Utc>) {
        self.move_to(
            EffectStatus::Pending,
            &[EffectStatus::Failed, EffectStatus::Completed],
        );
        self.retry = RetryState::default();
        self.deadline = self.retry_policy.attempt_deadline(now);
        self.started_at = Some(now);
    }

    pub fn requeue(&mut self) {
        self.move_to(
            EffectStatus::Queued,
            &[
                EffectStatus::Queued,
                EffectStatus::Pending,
                EffectStatus::Running,
                EffectStatus::RetryScheduled,
                EffectStatus::Failed,
            ],
        );
    }

    pub fn dispatch(&mut self, now: DateTime<Utc>) {
        self.move_to(
            EffectStatus::Pending,
            &[
                EffectStatus::Queued,
                EffectStatus::Pending,
                EffectStatus::RetryScheduled,
            ],
        );
        self.deadline = self.retry_policy.attempt_deadline(now);
        self.started_at.get_or_insert(now);
        self.retry.next_at = None;
        self.resend = false;
    }

    pub fn unsent(&mut self, retry_at: DateTime<Utc>) {
        self.move_to(EffectStatus::RetryScheduled, &[EffectStatus::Pending]);
        self.retry.next_at = Some(retry_at);
        self.retry.unsent = self.retry.unsent.saturating_add(1);
        self.deadline = None;
        self.resend = true;
    }

    pub fn run(&mut self) {
        self.move_to(EffectStatus::Running, &[EffectStatus::Pending]);
        self.deadline = None;
    }

    pub fn total_deadline(&self) -> Option<DateTime<Utc>> {
        self.started_at
            .and_then(|at| self.retry_policy.total_deadline(at))
    }

    pub fn expiry(&self) -> Option<DateTime<Utc>> {
        let total = self.total_deadline();
        match self.status {
            EffectStatus::Pending => match (self.deadline, total) {
                (Some(a), Some(b)) => Some(a.min(b)),
                (a, b) => a.or(b),
            },
            EffectStatus::Running => total,
            _ => None,
        }
    }

    pub fn total_expired(&self, now: DateTime<Utc>) -> bool {
        self.total_deadline().is_some_and(|d| d <= now)
    }

    pub fn complete(&mut self) {
        self.move_to(
            EffectStatus::Completed,
            &[EffectStatus::Pending, EffectStatus::Running],
        );
    }

    pub fn void(&mut self) {
        self.move_to(
            EffectStatus::Failed,
            &[
                EffectStatus::Queued,
                EffectStatus::Pending,
                EffectStatus::Running,
                EffectStatus::RetryScheduled,
                EffectStatus::Failed,
            ],
        );
    }

    pub fn is_terminal_failure(&self, retryable: bool) -> bool {
        self.retry_policy.exhausted(&self.retry, retryable)
    }

    pub fn record_error(&mut self, retryable: bool, now: DateTime<Utc>) {
        let terminal = self.is_terminal_failure(retryable);
        self.retry = self.retry_policy.record_failure(&self.retry, now);
        let next = if terminal {
            self.retry.next_at = None;
            EffectStatus::Failed
        } else {
            EffectStatus::RetryScheduled
        };
        self.move_to(
            next,
            &[
                EffectStatus::Queued,
                EffectStatus::Pending,
                EffectStatus::RetryScheduled,
            ],
        );
    }

    pub fn is_ready(&self) -> bool {
        self.status == EffectStatus::Completed
    }

    pub fn is_in_flight(&self) -> bool {
        matches!(
            self.status,
            EffectStatus::Pending | EffectStatus::RetryScheduled
        )
    }

    pub fn is_queued(&self) -> bool {
        self.status == EffectStatus::Queued
    }

    pub fn is_open(&self) -> bool {
        self.is_queued() || self.is_in_flight()
    }

    pub fn is_unsettled(&self) -> bool {
        self.is_open() || self.status == EffectStatus::Running
    }

    pub fn earliest_wake(&self) -> Option<DateTime<Utc>> {
        match self.status {
            EffectStatus::Pending | EffectStatus::Running => self.expiry(),
            EffectStatus::RetryScheduled => self.retry.next_at,
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EffectState {
    pub id: String,
    pub tracking: EffectTracking,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub anchor: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    pub payload: EffectPayload,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum EffectPayload {
    LlmCall(LlmCallState),
    ToolCall(ToolCallState),
    Subagent(SubagentCallState),
    ConnectorSync(ConnectorSyncState),
    Decision(WorkerDecisionState),
    System1Call(System1CallState),
}

impl EffectPayload {
    pub fn kind(&self) -> EffectKind {
        match self {
            EffectPayload::LlmCall(_) => EffectKind::LlmCall,
            EffectPayload::ToolCall(_) => EffectKind::ToolCall,
            EffectPayload::Subagent(_) => EffectKind::Subagent,
            EffectPayload::ConnectorSync(_) => EffectKind::ConnectorSync,
            EffectPayload::Decision(_) => EffectKind::Decision,
            EffectPayload::System1Call(_) => EffectKind::System1Call,
        }
    }
}

macro_rules! payload_views {
    ($($get:ident, $get_mut:ident => $variant:ident($ty:ty)),* $(,)?) => {
        impl EffectState {
            pub fn kind(&self) -> EffectKind {
                self.payload.kind()
            }

            $(
                pub fn $get(&self) -> Option<&$ty> {
                    match &self.payload {
                        EffectPayload::$variant(v) => Some(v),
                        _ => None,
                    }
                }
                pub fn $get_mut(&mut self) -> Option<&mut $ty> {
                    match &mut self.payload {
                        EffectPayload::$variant(v) => Some(v),
                        _ => None,
                    }
                }
            )*
        }
    };
}

impl EffectState {
    pub fn new(id: impl Into<String>, tracking: EffectTracking, payload: EffectPayload) -> Self {
        Self {
            id: id.into(),
            tracking,
            anchor: None,
            branch: None,
            payload,
        }
    }

    pub fn at(mut self, anchor: Option<&str>) -> Self {
        self.anchor = anchor.map(str::to_string);
        self
    }
}

payload_views! {
    llm, llm_mut => LlmCall(LlmCallState),
    tool, tool_mut => ToolCall(ToolCallState),
    subagent, subagent_mut => Subagent(SubagentCallState),
    connector, connector_mut => ConnectorSync(ConnectorSyncState),
    decision, decision_mut => Decision(WorkerDecisionState),
    system1, system1_mut => System1Call(System1CallState),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LastRead {
    pub call_id: String,
    pub tokens: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Limited {
    pub reason: ContextReason,
    pub call: HeldCall,
    #[serde(default)]
    pub source: PromptSource,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub released: bool,
}

impl Limited {
    pub fn open(&self) -> bool {
        !self.released
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LlmCallState {
    #[serde(default)]
    pub prompt: Vec<Message>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dispatched_seq: Option<u64>,
    #[serde(default)]
    pub source: PromptSource,
    pub llm: String,
    pub spec: LlmCallSpec,
    pub stream: bool,
    #[serde(default)]
    pub handler: LlmHandler,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub format: Option<LlmFormat>,
    #[serde(default)]
    pub defer_tools_strategy: DeferToolsStrategy,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub context_ids: Vec<String>,
    /// The tools the model gets, set at dispatch from `spec.tools`, which
    /// keeps the tools the call asked for.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sent_tools: Option<Vec<LlmTool>>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub unanswered: Vec<String>,
}

impl LlmCallState {
    /// The request the model gets: its prompt, and the tools it was sent.
    pub fn sent_request(&self) -> LlmRequest {
        let mut request = self.spec.to_request(self.prompt.clone());
        if let Some(tools) = &self.sent_tools {
            request.tools = Some(tools.clone());
        }
        request
    }

    /// The tools the model was sent, or asked for before dispatch.
    pub fn sent(&self) -> Option<&[LlmTool]> {
        self.sent_tools.as_deref().or(self.spec.tools.as_deref())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LlmCallSpec {
    pub model: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<LlmTool>>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "crate::json::option_f64"
    )]
    pub temperature: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_completion_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<ReasoningConfig>,
}

impl LlmCallSpec {
    pub fn to_request(&self, messages: Vec<Message>) -> LlmRequest {
        self.to_wire_request(messages.into_iter().map(DraftMessage::from).collect())
    }

    pub fn to_wire_request(&self, messages: Vec<DraftMessage>) -> LlmRequest {
        LlmRequest {
            model: self.model.clone(),
            messages,
            tools: self.tools.clone(),
            temperature: self.temperature,
            max_completion_tokens: self.max_completion_tokens,
            reasoning: self.reasoning.clone(),
        }
    }
}

impl From<&LlmRequest> for LlmCallSpec {
    fn from(r: &LlmRequest) -> Self {
        Self {
            model: r.model.clone(),
            tools: r.tools.clone(),
            temperature: r.temperature,
            max_completion_tokens: r.max_completion_tokens,
            reasoning: r.reasoning.clone(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCallState {
    pub name: String,
    #[serde(default)]
    pub handler: ToolHandler,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<ConnectorTarget>,
    #[serde(default)]
    pub arguments: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<String>,
    #[serde(default)]
    pub is_error: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub script: Option<ScriptCaller>,
    /// For `run_code`: the language its script is in.
    #[serde(default)]
    pub script_language: crate::codemode::Language,
    /// For `run_code`: CPU its script's runs have used.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub script_cpu_ms: u64,
    /// For `run_code`: bytes of files its script's runs have read.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub script_file_bytes: u64,
    /// For `run_code`: the skills' files, as its script's first step stored
    /// them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub script_skill_files: Option<Vec<crate::codemode::StoredFile>>,
    /// For `run_code`: calls its script asked for that no decision has
    /// started or refused yet.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub script_asked: Vec<ScriptAsk>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub recorded: bool,
}

pub(crate) fn is_zero(n: &u64) -> bool {
    *n == 0
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubagentCallState {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub on_branch: Option<SubagentOnBranch>,
    pub agent_id: String,
    #[serde(default)]
    pub session_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<DraftMessage>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<String>,
    #[serde(default)]
    pub is_error: bool,
    #[serde(default)]
    pub mode: SpawnMode,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct System1CallState {
    pub system1: String,
    pub request: System1Request,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConnectorSyncState {
    #[serde(default)]
    pub tools: Vec<RemoteTool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prefix: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instructions: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth: Option<AuthNeed>,
}

#[derive(Debug, Clone)]
pub struct Outgoing {
    pub node: Option<String>,
    pub prompt: Vec<Message>,
    pub placed: Vec<prompt_context::Placed>,
    pub places: bool,
    context_ids: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OpenInterrupt {
    pub interrupt_id: String,
    pub origin: InterruptOrigin,
    pub reason: String,
    pub payload: serde_json::Value,
    pub anchor: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
}

#[derive(Debug, Clone, Copy)]
pub struct QueuedNotice<'a> {
    pub decision_id: &'a str,
    pub messages: &'a [DraftMessage],
    pub sessions: &'a [String],
    pub turn_id: &'a str,
}

#[derive(Debug, Clone, Copy)]
pub struct WaitingMerge<'a> {
    pub decision_id: &'a str,
    pub messages: &'a [DraftMessage],
    pub client: &'a ClientContext,
    pub turn_id: &'a str,
}

#[derive(Debug, Clone, Copy)]
pub enum DecisionPark<'a> {
    Interrupt(&'a OpenInterrupt),
    Turn(&'a str),
    Work(EffectKind, &'a str),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Logged<T> {
    pub seq: u64,
    #[serde(flatten)]
    pub entry: T,
}

impl<T> std::ops::Deref for Logged<T> {
    type Target = T;
    fn deref(&self) -> &T {
        &self.entry
    }
}

pub trait Anchored {
    fn anchor(&self) -> Option<&str>;
}

impl<T: Anchored> Anchored for Logged<T> {
    fn anchor(&self) -> Option<&str> {
        self.entry.anchor()
    }
}

impl Anchored for OpenInterrupt {
    fn anchor(&self) -> Option<&str> {
        self.anchor.as_deref()
    }
}

fn trigger_effect(trigger: &Trigger) -> Option<(EffectKind, &str)> {
    match trigger {
        Trigger::ToolFinished { id, .. }
        | Trigger::ToolExecute { id, .. }
        | Trigger::CodemodeCalls { id, .. } => Some((EffectKind::ToolCall, id)),
        Trigger::SubagentFinished { id, .. } => Some((EffectKind::Subagent, id)),
        Trigger::LlmFinished { id, .. } | Trigger::LlmExecute { id, .. } => {
            Some((EffectKind::LlmCall, id))
        }
        Trigger::System1Finished { id, .. } => Some((EffectKind::System1Call, id)),
        _ => None,
    }
}

pub fn resolve_on_path<'a, V: Anchored>(
    versions: &'a [V],
    on_path: &std::collections::HashSet<String>,
) -> Option<&'a V> {
    versions.iter().rev().find(|v| match v.anchor() {
        None => true,
        Some(a) => on_path.contains(a),
    })
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkerDecisionState {
    pub trigger: Trigger,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    #[serde(default)]
    pub source_event_sequence: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dispatched_seq: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn: Option<Box<DecisionTurn>>,
}

impl WorkerDecisionState {
    pub fn built_at(&self) -> u64 {
        self.dispatched_seq.unwrap_or(self.source_event_sequence)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BranchState {
    pub node: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent: Option<String>,
    #[serde(default)]
    pub agent: BranchConfig,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default)]
    pub mode: BranchMode,
    pub seq: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub leaf: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub closed: Option<BranchEnd>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subagent: Option<String>,
}

impl BranchState {
    pub fn is_open(&self) -> bool {
        self.closed.is_none()
    }

    pub fn at(&self) -> Option<&str> {
        self.leaf.as_deref().or(self.from.as_deref())
    }

    pub fn view(&self, id: &str) -> BranchView {
        BranchView {
            id: id.to_string(),
            name: self.name.clone(),
            mode: self.mode,
            from: self.from.clone(),
            leaf: self.leaf.clone(),
            parent: self.parent.clone(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QueueEntry {
    pub seq: u64,
    pub kind: EffectKind,
    pub id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TurnEnd {
    pub turn_id: String,
    #[serde(default, skip_serializing_if = "serde_json::Value::is_null")]
    pub data: serde_json::Value,
    #[serde(default)]
    pub cost: Decimal,
    #[serde(default)]
    pub usage: Usage,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(tag = "phase", rename_all = "snake_case")]
pub enum TurnPhase {
    #[default]
    Idle,
    Active {
        turn_id: String,
    },
    Finalizing(TurnEnd),
}

impl TurnPhase {
    pub fn turn_id(&self) -> Option<&str> {
        match self {
            TurnPhase::Idle => None,
            TurnPhase::Active { turn_id } => Some(turn_id),
            TurnPhase::Finalizing(end) => Some(&end.turn_id),
        }
    }

    pub fn active_turn_id(&self) -> Option<&str> {
        match self {
            TurnPhase::Active { turn_id } => Some(turn_id),
            _ => None,
        }
    }

    pub fn finalizing(&self) -> Option<&TurnEnd> {
        match self {
            TurnPhase::Finalizing(end) => Some(end),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EventMeta {
    pub status: SessionStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wake_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner: Option<SessionOwner>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_id: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ancestry: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_id: Option<String>,
    #[serde(default)]
    pub cost: Decimal,
    #[serde(default)]
    pub subagent_cost: Decimal,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub head_id: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub calls: Vec<Effect>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub decisions: Vec<MetaDecision>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MetaDecision {
    pub decision_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    pub status: EffectStatus,
    pub finished: bool,
    pub attempts: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deadline: Option<DateTime<Utc>>,
    pub source_event_sequence: u64,
}

impl EventMeta {
    pub fn pending_work(&self, decision_id: &str) -> usize {
        let branch = self
            .decisions
            .iter()
            .find(|d| d.decision_id == decision_id)
            .and_then(|d| d.branch.as_deref());
        let in_flight_calls = self
            .calls
            .iter()
            .filter(|e| e.branch.as_deref() == branch)
            .filter(|e| {
                matches!(e.kind, EffectKind::ToolCall | EffectKind::Subagent)
                    && matches!(
                        e.status,
                        EffectStatus::Queued
                            | EffectStatus::Pending
                            | EffectStatus::Running
                            | EffectStatus::RetryScheduled
                    )
            })
            .count();
        let unrecorded_results = self
            .decisions
            .iter()
            .filter(|d| d.decision_id != decision_id && d.branch.as_deref() == branch)
            .filter(|d| {
                matches!(d.status, EffectStatus::Pending | EffectStatus::Queued) && d.finished
            })
            .count();
        in_flight_calls + unrecorded_results
    }
}

mod effect_table {
    use super::{EffectKind, EffectState};
    use serde::{Deserialize, Deserializer, Serialize, Serializer};
    use std::collections::BTreeMap;

    pub fn serialize<S: Serializer>(
        table: &BTreeMap<(EffectKind, String), EffectState>,
        s: S,
    ) -> Result<S::Ok, S::Error> {
        table.values().collect::<Vec<_>>().serialize(s)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(
        d: D,
    ) -> Result<BTreeMap<(EffectKind, String), EffectState>, D::Error> {
        Ok(Vec::<EffectState>::deserialize(d)?
            .into_iter()
            .map(|e| ((e.kind(), e.id.clone()), e))
            .collect())
    }
}

pub fn json_to_string(v: &serde_json::Value) -> String {
    match v {
        serde_json::Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionState {
    pub session_id: String,
    pub status: SessionStatus,
    pub agent_id: Option<String>,
    pub owner: Option<SessionOwner>,
    #[serde(default)]
    pub token_usage: Usage,

    #[serde(default)]
    pub cost: Decimal,

    #[serde(default)]
    pub subagent_cost: Decimal,

    #[serde(default)]
    pub turn_cost: Decimal,

    #[serde(default)]
    pub turn_token_usage: Usage,

    #[serde(default)]
    pub turn_calls: TurnCalls,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_started_at: Option<DateTime<Utc>>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_turn_at: Option<DateTime<Utc>>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_read: Option<LastRead>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limited: Option<Limited>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_head: Option<String>,

    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub turn_called: bool,

    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub turn_metadata: BTreeMap<String, String>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quiet_end: Option<String>,

    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub context_kept: bool,

    #[serde(default)]
    pub subagent_token_usage: Usage,

    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ancestry: Vec<String>,

    #[serde(default, skip_serializing_if = "serde_json::Value::is_null")]
    pub data: serde_json::Value,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worker_retry: Option<RetryPolicy>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worker: Option<WorkerRef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub events: Option<String>,

    #[serde(with = "effect_table", default = "BTreeMap::new")]
    pub effects: BTreeMap<(EffectKind, String), EffectState>,

    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub schedule_queue: Vec<QueueEntry>,

    #[serde(default)]
    pub phase: TurnPhase,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_started_seq: Option<u64>,

    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub completed_turn_ids: Vec<String>,

    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub last_turn_failed: bool,

    /// The first thing a client said, kept even when no turn ever answered it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,

    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub detached_turns: BTreeSet<String>,

    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub deliveries: BTreeMap<String, crate::delivery::Delivery>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub head_id: Option<String>,

    #[serde(skip)]
    pub tree: crate::tree::Tree,

    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub open_interrupts: Vec<OpenInterrupt>,

    #[serde(default)]
    pub session_start_failed: bool,

    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub branches: BTreeMap<String, BranchState>,
}

impl SessionState {
    pub fn new(session_id: String) -> Self {
        SessionState {
            deliveries: BTreeMap::new(),
            session_id,
            status: SessionStatus::Done,
            agent_id: None,
            owner: None,
            token_usage: Usage::default(),
            cost: Decimal::ZERO,
            subagent_cost: Decimal::ZERO,
            turn_cost: Decimal::ZERO,
            turn_token_usage: Usage::default(),
            turn_calls: TurnCalls::default(),
            turn_started_at: None,
            last_turn_at: None,
            last_read: None,
            limited: None,
            turn_head: None,
            turn_called: false,
            turn_metadata: BTreeMap::new(),
            quiet_end: None,
            context_kept: false,
            subagent_token_usage: Usage::default(),
            ancestry: Vec::new(),
            data: serde_json::Value::Null,
            worker_retry: None,
            worker: None,
            events: None,
            effects: BTreeMap::new(),
            schedule_queue: Vec::new(),
            phase: TurnPhase::default(),
            turn_started_seq: None,
            completed_turn_ids: Vec::new(),
            last_turn_failed: false,
            title: None,
            detached_turns: BTreeSet::new(),
            head_id: None,
            tree: crate::tree::Tree::default(),
            open_interrupts: Vec::new(),
            session_start_failed: false,
            branches: BTreeMap::new(),
        }
    }

    pub fn at<'a, 'n>(&'a self, node: Option<&'n str>) -> SessionStateAtNode<'a, 'n> {
        SessionStateAtNode { state: self, node }
    }

    pub fn at_head(&self) -> SessionStateAtNode<'_, '_> {
        self.at(self.head_id.as_deref())
    }

    pub fn at_scope<'a>(&'a self, branch: Option<&'a str>) -> SessionStateAtNode<'a, 'a> {
        match branch.and_then(|b| self.branches.get(b)) {
            Some(b) => self.at(b.at()),
            None => self.at_head(),
        }
    }

    pub fn scope_node(&self, branch: Option<&str>) -> Option<String> {
        match branch.and_then(|b| self.branches.get(b)) {
            Some(b) => b.at().map(str::to_string),
            None => self.head_id.clone(),
        }
    }

    pub fn open_branch(&self, id: &str) -> Option<&BranchState> {
        self.branches.get(id).filter(|b| b.is_open())
    }

    pub fn parallel_branches(&self) -> bool {
        self.at_head()
            .resolve_agent_for()
            .and_then(|c| c.parallel_branches)
            .unwrap_or(false)
    }

    pub fn lane(&self, branch: Option<&str>) -> Option<String> {
        branch
            .filter(|_| self.parallel_branches())
            .map(str::to_string)
    }

    pub fn detached(&self, branch: Option<&str>) -> bool {
        let mut cursor = branch;
        let mut seen = 0;
        while let Some(b) = cursor.and_then(|id| self.branches.get(id)) {
            if b.mode == BranchMode::Detached {
                return true;
            }
            cursor = b.parent.as_deref();
            seen += 1;
            if seen > self.branches.len() {
                break;
            }
        }
        false
    }

    pub fn held_by_branches(&self, parent: Option<&str>) -> usize {
        self.branches
            .values()
            .filter(|b| b.is_open() && b.mode == BranchMode::Blocking)
            .filter(|b| b.parent.as_deref() == parent)
            .count()
    }

    pub fn node_branch(&self, id: &str) -> Option<String> {
        self.tree
            .get(id)
            .and_then(|n| n.branch().map(str::to_string))
    }

    #[cfg(test)]
    pub fn seed(&mut self, node: crate::tree::Node) {
        let seq = self.tree.fresh().count() as u64;
        self.grow(node, seq);
    }

    #[cfg(test)]
    pub fn seed_config(&mut self, config: AgentConfig) {
        let parent = self.head_id.clone();
        let id = format!("seed-config-{}", self.tree.fresh().count());
        self.seed(crate::tree::Node {
            place: crate::tree::Place {
                id,
                parent_id: parent,
                branch: None,
            },
            fact: crate::tree::Fact::Config {
                config: Some(config),
                root: false,
            },
        });
    }

    #[cfg(test)]
    pub fn seed_message(&mut self, message: crate::protocol::NewMessage) {
        self.seed(crate::tree::Node::of_message(&message));
    }

    fn grow(&mut self, node: crate::tree::Node, seq: u64) {
        let id = node.id().to_string();
        let user = node.message().is_some_and(|m| m.role == Role::User);
        let branch = node.branch().map(str::to_string);
        let forks = !matches!(node.fact, crate::tree::Fact::Context { .. });
        let parent = node.parent().map(str::to_string);
        self.tree.push(Logged { seq, entry: node });
        if !forks && parent != self.scope_node(branch.as_deref()) {
            return;
        }
        match branch.and_then(|b| self.branches.get_mut(&b)) {
            Some(branch) => branch.leaf = Some(id),
            None => {
                if user {
                    self.status = SessionStatus::Idle;
                }
                self.head_id = Some(id);
            }
        }
    }

    pub fn decision_branch(&self, id: &str) -> Option<&str> {
        self.worker_decision(id).and_then(|d| d.branch.as_deref())
    }

    fn effect_branch(&self, kind: EffectKind, id: &str) -> Option<String> {
        self.effect(kind, id).and_then(|e| e.branch.clone())
    }

    pub fn trigger_branch(&self, trigger: &Trigger) -> Option<String> {
        match trigger {
            Trigger::BranchFinished { id, .. } => {
                self.branches.get(id).and_then(|b| b.parent.clone())
            }
            _ => {
                let (kind, id) = trigger_effect(trigger)?;
                self.effect_branch(kind, id)
            }
        }
    }

    fn release(&mut self) {
        let held: std::collections::HashSet<(EffectKind, String)> = self
            .effects_of(EffectKind::Decision)
            .filter_map(|e| trigger_effect(&e.decision()?.trigger))
            .map(|(kind, id)| (kind, id.to_string()))
            .chain(
                self.effects_of(EffectKind::LlmCall)
                    .filter_map(EffectState::llm)
                    .flat_map(|c| c.unanswered.iter())
                    .map(|id| (EffectKind::ToolCall, id.clone())),
            )
            .collect();
        let running: std::collections::HashSet<String> = self
            .effects_of(EffectKind::ToolCall)
            .filter(|e| e.tracking.is_unsettled())
            .map(|e| e.id.clone())
            .collect();
        let released: Vec<(EffectKind, String)> = self
            .effects
            .iter()
            .filter(|(key, e)| {
                !e.tracking.is_unsettled()
                    && !matches!(
                        key.0,
                        EffectKind::Decision | EffectKind::ConnectorSync | EffectKind::Subagent
                    )
                    && !held.contains(*key)
                    && e.llm().is_none_or(|c| c.unanswered.is_empty())
                    && e.tool().is_none_or(|t| match &t.script {
                        Some(script) => !running.contains(&script.parent),
                        None => t.recorded,
                    })
            })
            .map(|(key, _)| key.clone())
            .collect();
        for (kind, id) in released {
            self.remove_effect(kind, &id);
            self.dequeue(kind, &id);
        }
    }

    fn answered(&mut self, message: &Message, branch: Option<&str>) {
        let Some(answer) = message.tool_call_id.as_deref() else {
            return;
        };
        for e in self
            .effects
            .values_mut()
            .filter(|e| e.branch.as_deref() == branch)
        {
            match &mut e.payload {
                EffectPayload::LlmCall(call) => call.unanswered.retain(|id| id != answer),
                EffectPayload::ToolCall(tool) if e.id == answer => tool.recorded = true,
                _ => {}
            }
        }
        self.release();
    }

    pub fn turn_id(&self) -> Option<&str> {
        self.phase
            .turn_id()
            .or_else(|| self.completed_turn_ids.last().map(String::as_str))
    }

    pub fn effect(&self, kind: EffectKind, id: &str) -> Option<&EffectState> {
        self.effects.get(&(kind, id.to_string()))
    }

    pub fn effect_mut(&mut self, kind: EffectKind, id: &str) -> Option<&mut EffectState> {
        self.effects.get_mut(&(kind, id.to_string()))
    }

    pub fn put_effect(&mut self, effect: EffectState) {
        self.effects
            .insert((effect.kind(), effect.id.clone()), effect);
    }

    pub fn has_effect(&self, kind: EffectKind, id: &str) -> bool {
        self.effects.contains_key(&(kind, id.to_string()))
    }

    pub fn tracking(&self, kind: EffectKind, id: &str) -> Option<&EffectTracking> {
        self.effect(kind, id).map(|e| &e.tracking)
    }

    pub fn effects_of(&self, kind: EffectKind) -> impl Iterator<Item = &EffectState> {
        self.effects.values().filter(move |e| e.kind() == kind)
    }

    pub fn llm_call(&self, id: &str) -> Option<&LlmCallState> {
        self.effect(EffectKind::LlmCall, id).and_then(|e| e.llm())
    }

    pub fn tool_call(&self, id: &str) -> Option<&ToolCallState> {
        self.effect(EffectKind::ToolCall, id).and_then(|e| e.tool())
    }

    /// The calls a `run_code` call's script made, in order.
    pub fn script_calls(&self, parent: &str) -> Vec<&EffectState> {
        let mut calls: Vec<(u32, &EffectState)> = self
            .effects_of(EffectKind::ToolCall)
            .filter_map(|e| {
                let script = e.tool()?.script.as_ref()?;
                (script.parent == parent).then_some((script.seq, e))
            })
            .collect();
        calls.sort_by_key(|(seq, _)| *seq);
        calls.into_iter().map(|(_, e)| e).collect()
    }

    /// Whether a `run_code` call's script can take its next step: none of
    /// its calls is open, and no interrupt holds it.
    pub fn script_runs(&self, parent: &str) -> bool {
        let anchor = self
            .effect(EffectKind::ToolCall, parent)
            .and_then(|e| e.anchor.as_deref());
        !self.script_waits(parent) && !self.anchor_parked(anchor)
    }

    fn script_waits(&self, parent: &str) -> bool {
        !self.script_asked(parent).is_empty()
            || self
                .script_calls(parent)
                .iter()
                .any(|e| e.tracking.is_unsettled())
    }

    /// The calls a `run_code` call's script asked for that no decision has
    /// answered yet.
    pub fn script_asked(&self, parent: &str) -> &[ScriptAsk] {
        self.tool_call(parent)
            .map(|t| t.script_asked.as_slice())
            .unwrap_or_default()
    }

    /// The `run_code` call that asked for `id`, and the ask.
    pub fn script_ask(&self, id: &str) -> Option<(&str, &ScriptAsk)> {
        self.effects_of(EffectKind::ToolCall).find_map(|e| {
            let ask = e.tool()?.script_asked.iter().find(|a| a.id == id)?;
            Some((e.id.as_str(), ask))
        })
    }

    pub fn subagent(&self, id: &str) -> Option<&SubagentCallState> {
        self.effect(EffectKind::Subagent, id)
            .and_then(|e| e.subagent())
    }

    pub fn subagent_awaiting(&self, session_id: &str) -> Option<(&str, &SubagentCallState)> {
        self.effects_of(EffectKind::Subagent)
            .filter(|e| e.tracking.is_unsettled())
            .find_map(|e| {
                let sa = e.subagent()?;
                (sa.session_id == session_id).then_some((e.id.as_str(), sa))
            })
    }

    pub fn subagent_session(&self, session_id: &str) -> Option<&SubagentCallState> {
        self.effects_of(EffectKind::Subagent)
            .find_map(|e| e.subagent().filter(|sa| sa.session_id == session_id))
    }

    pub fn subagent_detached(&self, session_id: &str) -> Option<(&str, &SubagentCallState)> {
        let mut answered = None;
        for e in self.effects_of(EffectKind::Subagent) {
            let Some(sa) = e
                .subagent()
                .filter(|sa| sa.mode == SpawnMode::Detached && sa.session_id == session_id)
            else {
                continue;
            };
            match sa.result {
                None => return Some((e.id.as_str(), sa)),
                Some(_) => answered = answered.or(Some((e.id.as_str(), sa))),
            }
        }
        answered
    }

    pub fn connector_sync(&self, path: &ConnectionPath) -> Option<&ConnectorSyncState> {
        self.effect(EffectKind::ConnectorSync, &path.to_string())
            .and_then(|e| e.connector())
    }

    fn insert_effect(
        &mut self,
        id: &str,
        tracking: EffectTracking,
        payload: EffectPayload,
        branch: Option<&str>,
    ) {
        let anchor = match payload {
            EffectPayload::ConnectorSync(_) => None,
            _ => self.scope_node(branch),
        };
        self.effects.insert(
            (payload.kind(), id.to_string()),
            EffectState {
                id: id.to_string(),
                tracking,
                anchor,
                branch: branch.map(str::to_string),
                payload,
            },
        );
    }

    fn remove_effect(&mut self, kind: EffectKind, id: &str) -> Option<EffectState> {
        self.effects.remove(&(kind, id.to_string()))
    }

    fn enqueue(&mut self, seq: u64, kind: EffectKind, id: &str) {
        if !self
            .schedule_queue
            .iter()
            .any(|e| e.kind == kind && e.id == id)
        {
            self.schedule_queue.push(QueueEntry {
                seq,
                kind,
                id: id.to_string(),
            });
        }
    }

    fn dequeue(&mut self, kind: EffectKind, id: &str) {
        self.schedule_queue
            .retain(|e| !(e.kind == kind && e.id == id));
    }

    fn apply_llm_requested(&mut self, payload: &LlmCallRequested, ctx: &ApplyContext) {
        let prompt: Vec<Message> = payload
            .request
            .messages
            .iter()
            .cloned()
            .enumerate()
            .map(|(i, m)| m.recorded(format!("{}:{i}", payload.id)))
            .collect();
        let spec = LlmCallSpec::from(&payload.request);
        if let Some(existing) = self.effect_mut(EffectKind::LlmCall, &payload.id) {
            existing.tracking.requeue();
            if let PromptSource::Tree { at } = &payload.source {
                existing.anchor = at.clone();
            }
            if let Some(call) = existing.llm_mut() {
                call.prompt = prompt;
                call.source = payload.source.clone();
                call.llm = payload.llm.clone();
                call.spec = spec;
                call.stream = payload.stream;
                call.handler = payload.handler;
                call.format = payload.format;
                call.defer_tools_strategy = payload.defer_tools_strategy;
            }
        } else {
            if !self.detached(payload.branch.as_deref()) {
                self.turn_calls.llm.count += 1;
            }
            self.insert_effect(
                &payload.id,
                EffectTracking::new_queued(payload.retry.clone()),
                EffectPayload::LlmCall(LlmCallState {
                    prompt,
                    source: payload.source.clone(),
                    llm: payload.llm.clone(),
                    spec,
                    stream: payload.stream,
                    handler: payload.handler,
                    format: payload.format,
                    context_ids: Vec::new(),
                    dispatched_seq: None,
                    sent_tools: None,
                    unanswered: Vec::new(),
                    defer_tools_strategy: payload.defer_tools_strategy,
                }),
                payload.branch.as_deref(),
            );
            if let (Some(e), PromptSource::Tree { at }) = (
                self.effect_mut(EffectKind::LlmCall, &payload.id),
                &payload.source,
            ) {
                e.anchor = at.clone();
            }
        }
        match &mut self.limited {
            Some(l) if l.open() && l.call.id != payload.id && payload.branch.is_none() => {
                l.summary = Some(payload.id.clone())
            }
            _ if payload.branch.is_none() => self.turn_called = true,
            _ => {}
        }
        self.enqueue(ctx.sequence, EffectKind::LlmCall, &payload.id);
    }

    pub fn outgoing(&self, id: &str) -> Option<Outgoing> {
        let effect = self.effect(EffectKind::LlmCall, id)?;
        let call = effect.llm()?;
        let node = match &call.source {
            PromptSource::Tree { at } => at.clone(),
            _ => effect.anchor.clone().or_else(|| self.head_id.clone()),
        };
        Some(
            self.at(node.as_deref())
                .outgoing(id, &call.source, &call.prompt, &call.context_ids),
        )
    }

    fn settle_prompt(&mut self, id: &str) {
        if let Some(call) = self
            .effect_mut(EffectKind::LlmCall, id)
            .and_then(|e| e.llm_mut())
        {
            call.prompt = Vec::new();
        }
    }

    fn apply_llm_dispatched(&mut self, payload: &LlmCallDispatched, ctx: &ApplyContext) {
        let Some(out) = self.outgoing(&payload.id) else {
            self.dequeue(EffectKind::LlmCall, &payload.id);
            return;
        };
        if let Some(recorded) = &payload.fingerprint {
            let built = prompt_context::fingerprint(&out.prompt);
            if *recorded != built {
                tracing::warn!(
                    call_id = %payload.id,
                    %recorded,
                    %built,
                    "the prompt built for a call is not the prompt it sent"
                );
            }
        }
        let at = self.at(out.node.as_deref());
        let connector_tools = at.connector_tools().tools;
        let locked = at.locked_tools();
        let run_code = at.run_code();
        if let Some(effect) = self.effect_mut(EffectKind::LlmCall, &payload.id) {
            effect.tracking.dispatch(ctx.occurred_at);
            if let Some(call) = effect.llm_mut() {
                call.sent_tools = match run_code {
                    Some(run_code) => Some(vec![run_code]),
                    None => sent_tools(call.spec.tools.clone(), &connector_tools, &locked),
                };
                call.prompt = out.prompt;
                call.context_ids = out.context_ids;
                call.dispatched_seq = Some(ctx.sequence);
            }
        }
        self.dequeue(EffectKind::LlmCall, &payload.id);
    }

    fn apply_tool_requested(&mut self, payload: &ToolCallRequested, ctx: &ApplyContext) {
        if let Some(existing) = self.effect_mut(EffectKind::ToolCall, &payload.id) {
            existing.tracking.requeue();
            if let Some(tc) = existing.tool_mut() {
                tc.arguments = payload.arguments.clone();
            }
        } else {
            if !self.detached(payload.branch.as_deref()) {
                self.turn_calls.tool.count += 1;
            }
            let script_language = match payload.target {
                Some(ConnectorTarget::Code) => self
                    .at_scope(payload.branch.as_deref())
                    .resolve_agent_for()
                    .and_then(|c| c.codemode)
                    .unwrap_or_default(),
                _ => Default::default(),
            };
            self.insert_effect(
                &payload.id,
                EffectTracking::new_queued(payload.retry.clone()),
                EffectPayload::ToolCall(ToolCallState {
                    name: payload.name.clone(),
                    handler: payload.handler,
                    target: payload.target.clone(),
                    arguments: payload.arguments.clone(),
                    result: None,
                    is_error: false,
                    script: payload.script.clone(),
                    script_language,
                    script_cpu_ms: 0,
                    script_file_bytes: 0,
                    script_skill_files: None,
                    script_asked: Vec::new(),
                    recorded: false,
                }),
                payload.branch.as_deref(),
            );
            if let Some(script) = &payload.script {
                if let Some(parent) = self
                    .effect_mut(EffectKind::ToolCall, &script.parent)
                    .and_then(|e| e.tool_mut())
                {
                    parent.script_asked.retain(|a| a.id != payload.id);
                }
                let anchor = self
                    .effect(EffectKind::ToolCall, &script.parent)
                    .and_then(|e| e.anchor.clone());
                if let Some(e) = self.effect_mut(EffectKind::ToolCall, &payload.id) {
                    e.anchor = anchor;
                }
            }
        }
        self.enqueue(ctx.sequence, EffectKind::ToolCall, &payload.id);
    }

    fn apply_system1_requested(&mut self, p: &System1CallRequested, ctx: &ApplyContext) {
        match self.effect_mut(EffectKind::System1Call, &p.id) {
            Some(existing) => {
                existing.tracking.requeue();
                if let Some(call) = existing.system1_mut() {
                    call.request = p.request.clone();
                }
            }
            None => {
                if !self.detached(p.branch.as_deref()) {
                    self.turn_calls.system1.count += 1;
                }
                self.insert_effect(
                    &p.id,
                    EffectTracking::new_queued(p.retry.clone()),
                    EffectPayload::System1Call(System1CallState {
                        system1: p.system1.clone(),
                        request: p.request.clone(),
                    }),
                    p.branch.as_deref(),
                )
            }
        }
        self.enqueue(ctx.sequence, EffectKind::System1Call, &p.id);
    }

    fn apply_subagent_requested(&mut self, payload: &SubagentRequested, ctx: &ApplyContext) {
        if let Some(existing) = self.effect_mut(EffectKind::Subagent, &payload.id) {
            existing.tracking.requeue();
        } else {
            if !self.detached(payload.branch.as_deref()) {
                self.turn_calls.subagent.count += 1;
            }
            self.insert_effect(
                &payload.id,
                EffectTracking::new_queued(payload.retry.clone()),
                EffectPayload::Subagent(SubagentCallState {
                    on_branch: payload.on_branch.clone(),
                    agent_id: payload.agent_id.clone(),
                    session_id: payload.session_id.clone(),
                    message: payload.message.clone(),
                    result: None,
                    is_error: false,
                    mode: payload.mode,
                }),
                payload.branch.as_deref(),
            );
        }
        self.enqueue(ctx.sequence, EffectKind::Subagent, &payload.id);
    }

    fn apply_interrupted(&mut self, payload: &SessionInterrupted) {
        if !self
            .open_interrupts
            .iter()
            .any(|i| i.interrupt_id == payload.interrupt_id)
        {
            self.open_interrupts.push(OpenInterrupt {
                interrupt_id: payload.interrupt_id.clone(),
                origin: payload.origin,
                reason: payload.reason.clone(),
                payload: payload.payload.clone(),
                anchor: payload.anchor.clone(),
                branch: payload.branch.clone(),
            });
        }
        let lane = self.lane(payload.branch.as_deref());
        let held: Vec<(EffectKind, String)> = self
            .effects_of(EffectKind::Decision)
            .filter(|e| e.tracking.status() == EffectStatus::Pending)
            .filter(|e| self.lane(e.branch.as_deref()) == lane)
            .map(|e| (EffectKind::Decision, e.id.clone()))
            .collect();
        for key in held {
            self.effects.remove(&key);
        }
    }

    fn apply_decision_errored(&mut self, p: &DecisionErrored, now: DateTime<Utc>) {
        let failed = self
            .effect_mut(EffectKind::Decision, &p.id)
            .map(|e| {
                e.tracking.record_error(p.retryable, now);
                e.tracking.status == EffectStatus::Failed
            })
            .unwrap_or(false);
        if failed {
            let started = self
                .remove_effect(EffectKind::Decision, &p.id)
                .and_then(|e| e.decision().map(|d| d.trigger.clone()))
                .is_some_and(|t| matches!(t, Trigger::SessionStart));
            self.session_start_failed |= started;
            self.dequeue(EffectKind::Decision, &p.id);
        }
    }

    fn apply_decision_queued(&mut self, p: &DecisionQueued, ctx: &ApplyContext) {
        match &p.trigger {
            Trigger::SessionStart => self.session_start_failed = false,
            Trigger::TurnFinished { .. } => self.quiet_end = None,
            Trigger::ClientMessage { messages, .. } if self.title.is_none() => {
                self.title = messages
                    .iter()
                    .filter(|m| m.role == Role::User)
                    .find_map(|m| m.content.as_ref().and_then(|c| title(&c.text_owned())));
            }
            _ => {}
        }
        let retry_policy = self.worker_retry.clone().unwrap_or(RetryPolicy::no_retry());
        let branch = p.branch.clone().or_else(|| self.trigger_branch(&p.trigger));
        self.insert_effect(
            &p.id,
            EffectTracking::new_queued(retry_policy),
            EffectPayload::Decision(WorkerDecisionState {
                trigger: p.trigger.clone(),
                branch: branch.clone(),
                source_event_sequence: ctx.sequence,
                dispatched_seq: None,
                turn: None,
            }),
            branch.as_deref(),
        );
        self.enqueue(ctx.sequence, EffectKind::Decision, &p.id);
        if let Trigger::TurnFinished {
            turn_id,
            data,
            cost,
            usage,
        } = &p.trigger
        {
            self.last_turn_at = Some(ctx.occurred_at);
            self.phase = TurnPhase::Finalizing(TurnEnd {
                turn_id: turn_id.clone(),
                data: data.clone(),
                cost: *cost,
                usage: usage.clone(),
            });
            let turn_id = turn_id.clone();
            self.enqueue(ctx.sequence, EffectKind::TurnEnd, &turn_id);
        }
    }

    fn apply_call_voided(&mut self, p: &CallVoided) {
        let kind = p.kind;
        if kind != EffectKind::ConnectorSync {
            if let Some(effect) = self.effect_mut(kind, &p.id) {
                effect.tracking.void();
            }
        }
        if kind == EffectKind::LlmCall {
            self.settle_prompt(&p.id);
        }
        self.dequeue(kind, &p.id);
    }

    pub fn apply(&mut self, event: &EventPayload, ctx: &ApplyContext) {
        let now = ctx.occurred_at;
        match event {
            EventPayload::SessionCreated(payload) => {
                self.status = SessionStatus::Idle;
                self.agent_id = Some(payload.agent_id.clone());
                self.owner = Some(payload.identity.clone());
                self.ancestry = payload.ancestry.clone();
                self.worker_retry = Some(payload.worker_retry.clone());
                self.worker = payload.worker.clone();
                self.events = payload.events.clone();
            }
            EventPayload::NewMessage(payload) => {
                self.answered(&payload.message, payload.branch.as_deref());
                self.grow(crate::tree::Node::of_message(payload), ctx.sequence);
            }
            EventPayload::BranchStarted(p) => {
                if let Some(l) = self.limited.as_mut().filter(|l| l.open()) {
                    if p.parent.is_none() && crate::context::is_summary_branch(&l.call.id, &p.id) {
                        l.summary = Some(p.id.clone());
                    }
                }
                match self.branches.get_mut(&p.id) {
                    Some(b) if !b.is_open() => {
                        b.closed = None;
                        b.mode = p.mode;
                        b.turn_id = p.turn_id.clone();
                        b.subagent = p.subagent.clone();
                    }
                    Some(_) => {}
                    None => {
                        self.branches.insert(
                            p.id.clone(),
                            BranchState {
                                node: p.node.clone(),
                                from: p.from.clone(),
                                parent: p.parent.clone(),
                                agent: p.agent.clone(),
                                name: p.name.clone(),
                                mode: p.mode,
                                seq: ctx.sequence,
                                leaf: None,
                                turn_id: p.turn_id.clone(),
                                closed: None,
                                subagent: p.subagent.clone(),
                            },
                        );
                        if let Some(node) = crate::tree::Node::of_event(event) {
                            self.grow(node, ctx.sequence);
                        }
                    }
                }
            }
            EventPayload::BranchClosed(p) => {
                if let Some(b) = self.branches.get_mut(&p.id) {
                    b.closed = match (b.closed, p.reason) {
                        (_, BranchEnd::Promoted) | (None, _) => Some(p.reason),
                        (closed, _) => closed,
                    };
                }
            }
            EventPayload::HeadMoved(payload) => {
                match payload
                    .branch
                    .as_deref()
                    .and_then(|b| self.branches.get_mut(b))
                {
                    Some(branch) => branch.leaf = Some(payload.head_id.clone()),
                    None => self.head_id = Some(payload.head_id.clone()),
                }
            }
            EventPayload::LlmCallRequested(payload) => self.apply_llm_requested(payload, ctx),
            EventPayload::LlmCallDispatched(payload) => self.apply_llm_dispatched(payload, ctx),
            EventPayload::ContextPlaced(payload) => {
                if let Some(node) = crate::tree::Node::of_event(event) {
                    self.grow(node, ctx.sequence);
                }
                let at = Some(payload.node.id.clone());
                if let Some(effect) = self.effect_mut(EffectKind::LlmCall, &payload.call) {
                    effect.anchor = at.clone();
                    if let Some(call) = effect.llm_mut() {
                        if let PromptSource::Tree { at: source } = &mut call.source {
                            *source = at;
                        }
                    }
                }
            }
            EventPayload::LlmCallCompleted(payload) => {
                let branch = self.effect_branch(EffectKind::LlmCall, &payload.id);
                match &payload.response.usage {
                    Some(usage) if branch.is_none() && !self.is_summary_call(&payload.id) => {
                        self.last_read = Some(LastRead {
                            call_id: payload.id.clone(),
                            tokens: usage.input,
                        });
                    }
                    _ => {}
                }
                let in_turn = !self.detached(branch.as_deref());
                self.track_usage(&payload.response.usage);
                if in_turn {
                    self.track_turn_usage(&payload.response.usage);
                }
                if let Some(c) = payload.response.cost {
                    self.cost += c;
                    if in_turn {
                        self.turn_cost += c;
                        self.turn_calls.llm.cost += c;
                    }
                }
                if let Some(e) = self.effect_mut(EffectKind::LlmCall, &payload.id) {
                    e.tracking.complete();
                }
                self.settle_prompt(&payload.id);
                if let Some(call) = self
                    .effect_mut(EffectKind::LlmCall, &payload.id)
                    .and_then(|e| e.llm_mut())
                {
                    call.unanswered = payload
                        .response
                        .tool_calls
                        .iter()
                        .map(|c| c.id.clone())
                        .collect();
                }
            }
            EventPayload::LlmCallErrored(payload) => {
                self.turn_calls.llm.errors += 1;
                if let Some(e) = self.effect_mut(EffectKind::LlmCall, &payload.id) {
                    e.tracking.record_error(payload.retryable, now);
                }
                if self
                    .tracking(EffectKind::LlmCall, &payload.id)
                    .is_some_and(|t| t.status() == EffectStatus::Failed)
                {
                    self.settle_prompt(&payload.id);
                }
            }
            EventPayload::System1CallRequested(p) => self.apply_system1_requested(p, ctx),
            EventPayload::System1CallDispatched(p) => {
                if let Some(e) = self.effect_mut(EffectKind::System1Call, &p.id) {
                    e.tracking.dispatch(now);
                }
                self.dequeue(EffectKind::System1Call, &p.id);
            }
            EventPayload::System1CallCompleted(p) => {
                let usage = &p.response.usage;
                let in_turn = !self.detached(
                    self.effect_branch(EffectKind::System1Call, &p.id)
                        .as_deref(),
                );
                self.token_usage.add(usage);
                if in_turn {
                    self.turn_token_usage.add(usage);
                    self.turn_calls.system1.tokens.add(usage);
                }
                if let Some(c) = p.response.cost {
                    self.cost += c;
                    if in_turn {
                        self.turn_cost += c;
                        self.turn_calls.system1.cost += c;
                    }
                }
                if let Some(e) = self.effect_mut(EffectKind::System1Call, &p.id) {
                    e.tracking.complete();
                }
            }
            EventPayload::System1CallErrored(p) => {
                self.turn_calls.system1.errors += 1;
                if let Some(e) = self.effect_mut(EffectKind::System1Call, &p.id) {
                    e.tracking.record_error(p.retryable, now);
                }
            }
            EventPayload::ToolCallRequested(payload) => self.apply_tool_requested(payload, ctx),
            EventPayload::ToolCallDispatched(payload) => {
                if let Some(e) = self.effect_mut(EffectKind::ToolCall, &payload.id) {
                    e.tracking.dispatch(now);
                }
                self.dequeue(EffectKind::ToolCall, &payload.id);
            }
            EventPayload::ToolCallCompleted(payload) => {
                if let Some(e) = self.effect_mut(EffectKind::ToolCall, &payload.id) {
                    e.tracking.complete();
                    if let Some(tc) = e.tool_mut() {
                        tc.result = Some(payload.result.rendered());
                        tc.is_error = false;
                    }
                }
            }
            EventPayload::ToolCallStepped(payload) => {
                if let Some(tc) = self
                    .effect_mut(EffectKind::ToolCall, &payload.id)
                    .and_then(|e| e.tool_mut())
                {
                    tc.script_cpu_ms += payload.cpu_ms;
                    tc.script_file_bytes += payload.file_bytes;
                    if tc.script_skill_files.is_none() {
                        tc.script_skill_files = payload.skill_files.clone();
                    }
                    tc.script_asked.extend(payload.asked.iter().cloned());
                }
            }
            EventPayload::ToolCallErrored(payload) => {
                self.turn_calls.tool.errors += 1;
                if let Some(e) = self.effect_mut(EffectKind::ToolCall, &payload.id) {
                    e.tracking.record_error(payload.retryable, now);
                    let failed = e.tracking.status == EffectStatus::Failed;
                    if let Some(tc) = e.tool_mut().filter(|_| failed) {
                        tc.result = Some(payload.error.message.clone());
                        tc.is_error = true;
                    }
                }
            }
            EventPayload::SubagentRequested(payload) => self.apply_subagent_requested(payload, ctx),
            EventPayload::SubagentDispatched(payload) => {
                if let Some(e) = self.effect_mut(EffectKind::Subagent, &payload.id) {
                    e.tracking.dispatch(now);
                }
                self.dequeue(EffectKind::Subagent, &payload.id);
            }
            EventPayload::SubagentStarted(payload) => {
                if let Some(e) = self.effect_mut(EffectKind::Subagent, &payload.id) {
                    match e
                        .subagent()
                        .is_some_and(|sa| sa.mode == SpawnMode::Detached)
                    {
                        true => e.tracking.complete(),
                        false => e.tracking.run(),
                    }
                }
            }
            EventPayload::SubagentErrored(payload) => {
                self.turn_calls.subagent.errors += 1;
                if let Some(e) = self.effect_mut(EffectKind::Subagent, &payload.id) {
                    e.tracking.record_error(payload.retryable, now);
                    let failed = e.tracking.status == EffectStatus::Failed;
                    if let Some(sa) = e.subagent_mut().filter(|_| failed) {
                        sa.result = Some(payload.error.message.clone());
                        sa.is_error = true;
                    }
                }
            }
            EventPayload::SessionInterrupted(payload) => self.apply_interrupted(payload),
            EventPayload::InterruptResumed(p) => {
                self.open_interrupts
                    .retain(|i| i.interrupt_id != p.interrupt_id);
            }
            EventPayload::DecisionDispatched(p) => {
                let turnless = self.detached(self.decision_branch(&p.id))
                    || matches!(
                        self.worker_decision(&p.id).map(|d| &d.trigger),
                        Some(Trigger::BranchFinished {
                            mode: BranchMode::Detached,
                            ..
                        })
                    );
                let turn = self.phase.turn_id().filter(|_| !turnless).map(|id| {
                    Box::new(DecisionTurn {
                        id: id.to_string(),
                        metadata: self.turn_metadata.clone(),
                    })
                });
                let stamped = self
                    .worker_decision(&p.id)
                    .and_then(|d| d.turn.as_ref())
                    .is_some_and(|t| !self.completed_turn_ids.contains(&t.id));
                if let Some(e) = self.effect_mut(EffectKind::Decision, &p.id) {
                    e.tracking.dispatch(now);
                    if let Some(d) = e.decision_mut() {
                        d.dispatched_seq = Some(ctx.sequence);
                        if !stamped {
                            d.turn = turn;
                        }
                    }
                }
                self.dequeue(EffectKind::Decision, &p.id);
                self.status = SessionStatus::Idle;
            }
            EventPayload::DecisionCompleted(p) => {
                let owned = self.worker_decision(&p.id).and_then(|d| {
                    let turn = d.turn.as_ref()?;
                    d.trigger
                        .owns_turn(&p.id, &turn.id)
                        .then(|| turn.id.clone())
                });
                if let Some(turn) = owned.filter(|t| self.phase.active_turn_id() == Some(t)) {
                    self.quiet_end = Some(turn);
                }
                self.remove_effect(EffectKind::Decision, &p.id);
                self.dequeue(EffectKind::Decision, &p.id);
                self.release();
            }
            EventPayload::DecisionErrored(p) => self.apply_decision_errored(p, now),
            EventPayload::SessionMessageRequested(_) => {}
            EventPayload::DecisionQueued(p) => self.apply_decision_queued(p, ctx),
            EventPayload::DecisionAmended(p) => {
                if let Some(Trigger::ClientMessage {
                    messages,
                    client,
                    metadata,
                    ..
                }) = self
                    .effect_mut(EffectKind::Decision, &p.id)
                    .and_then(|e| e.decision_mut())
                    .map(|d| &mut d.trigger)
                {
                    messages.extend(p.messages.iter().cloned());
                    *client = std::mem::take(client).followed_by(p.client.clone());
                    metadata.extend(p.metadata.clone());
                }
            }
            EventPayload::DecisionDropped(p) => {
                self.remove_effect(EffectKind::Decision, &p.id);
                self.dequeue(EffectKind::Decision, &p.id);
                self.release();
            }
            EventPayload::CallVoided(p) => self.apply_call_voided(p),
            EventPayload::ConnectorSyncRequested(p) => {
                if !self.has_effect(EffectKind::ConnectorSync, &p.path.to_string()) {
                    self.insert_effect(
                        &p.path.to_string(),
                        EffectTracking::new(p.retry.clone(), now),
                        EffectPayload::ConnectorSync(ConnectorSyncState {
                            tools: Vec::new(),
                            prefix: None,
                            instructions: None,
                            error: None,
                            auth: None,
                        }),
                        None,
                    );
                }
                if let Some(e) = self.effect_mut(EffectKind::ConnectorSync, &p.path.to_string()) {
                    match e.tracking.status() {
                        EffectStatus::Failed | EffectStatus::Completed => e.tracking.restart(now),
                        _ => e.tracking.dispatch(now),
                    }
                }
            }
            EventPayload::ConnectorSyncCompleted(p) => {
                if let Some(e) = self.effect_mut(EffectKind::ConnectorSync, &p.path.to_string()) {
                    e.tracking.complete();
                    if let Some(sync) = e.connector_mut() {
                        sync.tools = p.tools.clone();
                        sync.prefix = p.prefix.clone();
                        sync.instructions = p.instructions.clone();
                        sync.error = None;
                        sync.auth = None;
                    }
                }
            }
            EventPayload::ConnectorSyncErrored(p) => {
                if let Some(e) = self.effect_mut(EffectKind::ConnectorSync, &p.path.to_string()) {
                    e.tracking.record_error(p.retryable, now);
                    if let Some(sync) = e.connector_mut() {
                        sync.error = Some(p.error.message.clone());
                        sync.auth = p.auth;
                    }
                }
            }
            EventPayload::ConnectorAuthFailed(p) => {
                if let Some(sync) = self
                    .effect_mut(EffectKind::ConnectorSync, &p.path.to_string())
                    .and_then(EffectState::connector_mut)
                {
                    sync.auth = Some(p.auth);
                }
            }
            EventPayload::WorkerStateUpdated(_) | EventPayload::AgentConfigUpdated(_) => {
                if let Some(node) = crate::tree::Node::of_event(event) {
                    self.grow(node, ctx.sequence);
                }
            }
            EventPayload::SessionCancelled => {
                self.limited = None;
            }
            EventPayload::SessionDone(_) => {
                if !self.ancestry.is_empty() {
                    self.status = SessionStatus::Done;
                } else {
                    self.status = SessionStatus::Idle;
                }
                self.schedule_queue
                    .retain(|e| e.kind != EffectKind::TurnEnd);
            }
            EventPayload::SubagentTurnCompleted(payload) => {
                self.subagent_cost += payload.cost;
                self.turn_cost += payload.cost;
                self.subagent_token_usage.add(&payload.token_usage);
                self.turn_token_usage.add(&payload.token_usage);
                self.turn_calls.subagent.cost += payload.cost;
                self.turn_calls.subagent.tokens.add(&payload.token_usage);
                self.turn_calls.subagent.errors += u32::from(payload.error.is_some());
                if let Some(turn_id) = &payload.turn_id {
                    self.detached_turns.insert(turn_id.clone());
                }
                if let Some(e) = self.effect_mut(EffectKind::Subagent, &payload.id) {
                    if let Some(sa) = e.subagent_mut() {
                        sa.result = Some(match &payload.error {
                            Some(error) => error.message.clone(),
                            None => json_to_string(&payload.data),
                        });
                        sa.is_error = payload.error.is_some();
                    }
                    match e.tracking.status() {
                        EffectStatus::Queued | EffectStatus::RetryScheduled => {
                            e.tracking.dispatch(now);
                            e.tracking.complete();
                        }
                        EffectStatus::Pending | EffectStatus::Running => e.tracking.complete(),
                        _ => {}
                    }
                }
                self.dequeue(EffectKind::Subagent, &payload.id);
            }
            EventPayload::ChannelsUpdated(_) => {}
            EventPayload::TurnJoined(p) => self.turn_metadata.extend(p.metadata.clone()),
            EventPayload::TurnStarted(p) => {
                self.phase = TurnPhase::Active {
                    turn_id: p.turn_id.clone(),
                };
                self.turn_started_seq = Some(ctx.sequence);
                self.turn_head = self.head_id.clone();
                self.turn_metadata = p.metadata.clone();
                self.quiet_end = None;
                self.turn_called = false;
                self.context_kept = false;
                self.turn_cost = Decimal::ZERO;
                self.turn_token_usage = Usage::default();
                self.turn_calls = TurnCalls::default();
                self.turn_started_at = Some(now);
            }
            EventPayload::TurnCompleted(payload) => {
                self.completed_turn_ids.push(payload.turn_id.clone());
                self.last_turn_failed = payload.error.is_some();
                if !matches!(self.phase, TurnPhase::Finalizing(_)) {
                    self.last_turn_at = Some(now);
                }
                self.data = payload.data.clone();
                self.turn_cost = Decimal::ZERO;
                self.turn_token_usage = Usage::default();
                self.turn_calls = TurnCalls::default();
                self.turn_started_at = None;
                self.phase = TurnPhase::Idle;
                self.turn_started_seq = None;
                self.limited = None;
                self.turn_metadata.clear();
                self.quiet_end = None;
                self.dequeue(EffectKind::TurnEnd, &payload.turn_id);
            }
            EventPayload::WorkUnsent(p) => {
                if let Some(e) = self.effect_mut(p.kind, &p.id) {
                    e.tracking.unsent(p.retry_at);
                }
            }
            EventPayload::DeliveryFailed(p) => {
                let d = self.deliveries.entry(p.subscription.clone()).or_default();
                d.floor = d.floor.max(p.seq.saturating_sub(1));
                d.attempts = p.attempt;
                d.failing_since.get_or_insert(now);
                d.retry_at = Some(p.retry_at);
            }
            EventPayload::DeliverySkipped(p) => {
                let d = self.deliveries.entry(p.subscription.clone()).or_default();
                d.floor = d.floor.max(p.seq);
                d.attempts = 0;
                d.retry_at = None;
                d.skipped += 1;
            }
            EventPayload::DeliveryParked(p) => {
                let d = self.deliveries.entry(p.subscription.clone()).or_default();
                d.floor = d.floor.max(p.seq.saturating_sub(1));
                d.attempts = p.attempt;
                d.failing_since.get_or_insert(now);
                d.retry_at = None;
                d.parked = Some(crate::delivery::Parked {
                    seq: p.seq,
                    error: p.error.clone(),
                });
            }
            EventPayload::DeliveryRecovered(p) => {
                let d = self.deliveries.entry(p.subscription.clone()).or_default();
                d.floor = d.floor.max(p.through);
                d.attempts = 0;
                d.failing_since = None;
                d.retry_at = None;
            }
            EventPayload::ContextLimited(p) => {
                self.limited = Some(Limited {
                    reason: p.reason,
                    call: p.call.clone(),
                    source: p.source.clone(),
                    summary: None,
                    released: false,
                });
            }
            EventPayload::ContextReplaced(_) => {
                self.last_read = None;
                if let Some(l) = &mut self.limited {
                    l.released = true;
                }
            }
            EventPayload::ContextKept => {
                self.context_kept = true;
                if let Some(l) = &mut self.limited {
                    l.released = true;
                }
            }
            EventPayload::DeliveryResumed(p) => {
                let d = self.deliveries.entry(p.subscription.clone()).or_default();
                d.attempts = 0;
                d.failing_since = None;
                d.retry_at = None;
                d.parked = None;
            }
        }
    }

    pub fn check_invariants(&self) -> Result<(), String> {
        let mut lanes: BTreeMap<Option<String>, usize> = BTreeMap::new();
        for e in self
            .effects_of(EffectKind::Decision)
            .filter(|e| e.tracking.status == EffectStatus::Pending)
        {
            *lanes.entry(self.lane(e.branch.as_deref())).or_default() += 1;
        }
        let live = lanes.get(&None).copied().unwrap_or_default();
        if lanes.values().any(|n| *n > 1) {
            return Err(format!("decision slot violated: {lanes:?} live decisions"));
        }
        if live > 0 && self.at_head().has_pending_connector_sync() {
            return Err(
                "a decision is live while a fetch its config owes is in flight".to_string(),
            );
        }
        let queued_statuses = self
            .effects
            .values()
            .filter(|e| e.kind() != EffectKind::ConnectorSync)
            .filter(|e| e.tracking.status == EffectStatus::Queued)
            .count();
        let effect_entries = self
            .schedule_queue
            .iter()
            .filter(|e| e.kind != EffectKind::TurnEnd)
            .count();
        if queued_statuses != effect_entries {
            return Err(format!(
                "queue ({effect_entries}) and Queued statuses ({queued_statuses}) disagree"
            ));
        }
        let turn_ends = self
            .schedule_queue
            .iter()
            .filter(|e| e.kind == EffectKind::TurnEnd)
            .count();
        if turn_ends > usize::from(self.phase.finalizing().is_some()) {
            return Err("a TurnEnd entry exists without a finalizing turn".to_string());
        }
        Ok(())
    }

    pub fn open_interrupt(&self, id: &str) -> Option<&OpenInterrupt> {
        self.open_interrupts.iter().find(|i| i.interrupt_id == id)
    }

    pub fn head_parked(&self) -> bool {
        self.at_head().active_interrupt_for().is_some()
    }

    pub fn branch_interrupt(&self) -> Option<&OpenInterrupt> {
        self.open_interrupts.iter().find(|i| {
            i.anchor
                .as_deref()
                .and(i.branch.as_deref())
                .is_some_and(|b| self.open_branch(b).is_some() && !self.detached(Some(b)))
        })
    }

    pub fn projected_status(&self) -> SessionStatus {
        match self
            .at_head()
            .active_interrupt_for()
            .or_else(|| self.branch_interrupt())
        {
            Some(i) => SessionStatus::Interrupted {
                interrupt_id: i.interrupt_id.clone(),
                origin: i.origin,
                reason: i.reason.clone(),
            },
            None => self.status.clone(),
        }
    }

    pub fn trigger_anchor(&self, trigger: &Trigger) -> Option<&str> {
        match trigger {
            Trigger::ToolFinished { id, .. }
            | Trigger::ToolExecute { id, .. }
            | Trigger::CodemodeCalls { id, .. } => self.effect(EffectKind::ToolCall, id),
            Trigger::SubagentFinished { id, .. } => self.effect(EffectKind::Subagent, id),
            Trigger::LlmFinished { id, .. } | Trigger::LlmExecute { id, .. } => {
                self.effect(EffectKind::LlmCall, id)
            }
            Trigger::System1Finished { id, .. } => self.effect(EffectKind::System1Call, id),
            _ => None,
        }
        .and_then(|e| e.anchor.as_deref())
    }

    pub fn is_summary_call(&self, id: &str) -> bool {
        let summary = self.limited.as_ref().and_then(|l| l.summary.as_deref());
        summary.is_some()
            && (summary == Some(id)
                || self
                    .effect(EffectKind::LlmCall, id)
                    .is_some_and(|e| e.branch.as_deref() == summary))
    }

    pub fn holds(&self, id: &str) -> bool {
        self.limited
            .as_ref()
            .is_some_and(|l| l.open() && l.call.id == id)
    }

    pub fn worker_decision(&self, id: &str) -> Option<&WorkerDecisionState> {
        self.effect(EffectKind::Decision, id)
            .and_then(|e| e.decision())
    }

    pub fn anchor_parked(&self, anchor: Option<&str>) -> bool {
        self.anchor_interrupt(anchor).is_some()
    }

    /// The interrupt that holds work anchored at `anchor`, if one does.
    pub fn anchor_interrupt(&self, anchor: Option<&str>) -> Option<&OpenInterrupt> {
        match anchor {
            Some(a) if !self.at_head().anchor_on_path(Some(a)) => {
                self.at(Some(a)).active_interrupt_for()
            }
            _ => self.at_head().active_interrupt_for(),
        }
    }

    pub fn decision_parked(&self, trigger: &Trigger) -> bool {
        self.decision_park(trigger).is_some()
    }

    pub fn decision_park(&self, trigger: &Trigger) -> Option<DecisionPark<'_>> {
        if let Some(interrupt) = self.decision_park_interrupt(trigger) {
            return Some(DecisionPark::Interrupt(interrupt));
        }
        if let (Some(_), Some(end)) = (trigger.joining_turn_id(), self.phase.finalizing()) {
            if !self.head_parked() {
                return Some(DecisionPark::Turn(&end.turn_id));
            }
        }
        if trigger.is_join() && self.phase.active_turn_id().is_some() {
            if let Some((kind, id)) = self.open_work().into_iter().next() {
                return Some(DecisionPark::Work(kind, id));
            }
        }
        match (trigger.deferred_turn_id(), self.phase.turn_id()) {
            (Some(turn_id), Some(active)) if active != turn_id => Some(DecisionPark::Turn(active)),
            _ => None,
        }
    }

    fn decision_park_interrupt(&self, trigger: &Trigger) -> Option<&OpenInterrupt> {
        if matches!(trigger, Trigger::ClientAction { .. }) {
            return None;
        }
        if let Trigger::ClientTranscript { .. } = trigger {
            return self
                .at(self.read_point(trigger, None).at.as_deref())
                .active_interrupt_for();
        }
        let at = match self.trigger_anchor(trigger) {
            Some(a) if !self.at_head().anchor_on_path(Some(a)) => self.at(Some(a)),
            _ => self.at_head(),
        };
        at.active_interrupt_for()
    }

    pub fn queued_subagent_notice(&self) -> Option<QueuedNotice<'_>> {
        self.decisions_with(EffectStatus::Queued)
            .find_map(|e| match &e.decision()?.trigger {
                Trigger::SubagentNotice {
                    messages,
                    sessions,
                    turn_id,
                } => Some(QueuedNotice {
                    decision_id: &e.id,
                    messages,
                    sessions,
                    turn_id,
                }),
                _ => None,
            })
    }

    pub fn waiting_join(&self) -> Option<WaitingMerge<'_>> {
        let first = self
            .queued_decisions()
            .into_iter()
            .find(|e| e.decision().is_some_and(|d| d.trigger.is_join()))?;
        match &first.decision()?.trigger {
            Trigger::ClientMessage {
                messages,
                client,
                turn_id: Some(turn_id),
                ..
            } => Some(WaitingMerge {
                decision_id: &first.id,
                messages,
                client,
                turn_id,
            }),
            _ => None,
        }
    }

    pub fn holds_message(&self, id: &str) -> bool {
        self.effects_of(EffectKind::Decision)
            .filter(|e| e.tracking.is_unsettled())
            .filter_map(|e| match &e.decision()?.trigger {
                Trigger::ClientMessage { messages, .. } => Some(messages),
                _ => None,
            })
            .flatten()
            .any(|m| m.id.as_deref() == Some(id))
    }

    pub fn waiting_merge(&self) -> Option<WaitingMerge<'_>> {
        let last = self.queued_decisions().into_iter().rev().find(|e| {
            e.decision()
                .is_some_and(|d| d.trigger.deferred_turn_id().is_some())
        })?;
        match &last.decision()?.trigger {
            Trigger::ClientMessage {
                messages,
                client,
                turn_id: Some(turn_id),
                merge: true,
                ..
            } => Some(WaitingMerge {
                decision_id: &last.id,
                messages,
                client,
                turn_id,
            }),
            _ => None,
        }
    }

    pub fn has_pending_worker_decision(&self) -> bool {
        self.decisions_with(EffectStatus::Pending).next().is_some()
    }

    pub fn queued_decisions(&self) -> Vec<&EffectState> {
        let mut queued: Vec<&EffectState> = self.decisions_with(EffectStatus::Queued).collect();
        queued.sort_by_key(|e| e.decision().map(|d| d.source_event_sequence));
        queued
    }

    pub fn has_queued_worker_decision(&self) -> bool {
        self.decisions_with(EffectStatus::Queued).next().is_some()
    }

    fn decisions_with(&self, status: EffectStatus) -> impl Iterator<Item = &EffectState> {
        self.effects_of(EffectKind::Decision)
            .filter(move |e| e.tracking.status == status)
    }

    pub fn path_ids(&self, node: &str) -> std::collections::HashSet<String> {
        self.tree.path_ids(node)
    }

    pub fn tool_handler_for(&self, name: &str) -> ToolHandler {
        self.at_head().tool_handler(name)
    }

    pub fn dispatched_calls(&self) -> Vec<String> {
        self.effects
            .values()
            .filter_map(|e| match e.kind() {
                EffectKind::ToolCall | EffectKind::Subagent => Some(e.id.clone()),
                _ => None,
            })
            .collect()
    }

    pub fn connector_tool_for(&self, name: &str) -> Option<ConnectorTool> {
        self.at_head().connector_tool(name)
    }

    pub fn connector_tools_for_config(&self, config: &AgentConfig) -> filter::Merged {
        let servers = self.servers_for(config);
        let may_spawn = config.may_spawn_subagent(self.depth());
        let mut resolutions: Vec<filter::Resolution> = Vec::new();
        if may_spawn {
            resolutions.push(filter::subagent_tools(
                &config.subagents,
                config.defers_tools(),
                config.subagent_strategy(),
                SubagentTools::wait_of(config.subagent_tools),
            ));
        }
        resolutions.extend(servers.iter().filter_map(|connector| {
            let effect = self.effect(EffectKind::ConnectorSync, &connector.path.to_string())?;
            let sync = effect.connector()?;
            effect.tracking.is_ready().then(|| {
                filter::resolve(
                    connector,
                    &sync.tools,
                    sync.prefix.as_deref(),
                    filter::defers(connector, config.defers_tools()),
                )
            })
        }));
        let defers = config.defers_tools()
            || config.tools.iter().any(|t| t.defer == Some(true))
            || (may_spawn
                && config.subagent_strategy() == SubagentToolsStrategy::PerAgent
                && config.subagents.iter().any(|s| s.defer == Some(true)))
            || servers.iter().any(|c| filter::defers(c, false));
        match config.codemode {
            Some(language) => resolutions.push(filter::Resolution::of(vec![
                crate::codemode::tool(language),
                filter::find_tool(config.searches_skills()),
            ])),
            None if defers => resolutions.push(filter::Resolution::of(filter::search_tools(
                config.defer_strategy(),
                config.searches_skills(),
            ))),
            None => {}
        }
        if config.has_skills() {
            resolutions.push(filter::Resolution::of(vec![filter::skill_tool()]));
        }
        if let Some(attachments) = config
            .attachments
            .as_ref()
            .filter(|_| config.codemode.is_none())
        {
            resolutions.push(filter::Resolution::of(
                crate::attachments::tools::definitions(attachments),
            ));
        }
        let taken: Vec<&str> = config.tools.iter().map(|t| t.name.as_str()).collect();
        filter::merge(resolutions, taken)
    }

    pub fn local_connector_answer(&self, tool_call_id: &str) -> Option<StoredResult> {
        let effect = self.effect(EffectKind::ToolCall, tool_call_id)?;
        let node = effect.anchor.clone();
        let tc = effect.tool()?;
        let target = tc.target.as_ref()?;
        let at = self.at(node.as_deref());
        if *target == ConnectorTarget::Locked {
            return Some(StoredResult::error(
                at.locked_fault(&tc.name)
                    .unwrap_or_else(|| crate::prompts::errors::CALL_NOT_ROUTED.to_string()),
            ));
        }
        if let ConnectorTarget::Refused { reason } = target {
            return Some(StoredResult::error(reason.clone()));
        }
        if *target == ConnectorTarget::ScriptSearch {
            let language = tc
                .script
                .as_ref()
                .and_then(|s| self.tool_call(&s.parent))
                .map(|parent| parent.script_language)
                .unwrap_or_default();
            return Some(super::engine_tools::script_find(
                at,
                language,
                &tc.arguments,
            ));
        }
        super::engine_tools::answer(at, target.kind(), &tc.arguments)
    }

    pub fn attachment_call(&self, tool_call_id: &str) -> Option<crate::attachments::tools::Call> {
        let effect = self.effect(EffectKind::ToolCall, tool_call_id)?;
        let tc = effect.tool()?;
        if tc.target.as_ref()? != &ConnectorTarget::Attachment {
            return None;
        }
        Some(crate::attachments::tools::Call {
            tool: crate::attachments::Tool::of_name(&tc.name)?,
            arguments: tc.arguments.clone(),
            attachments: crate::attachments::on_path(
                &self.message_tree(),
                effect.anchor.as_deref(),
            ),
        })
    }

    pub fn skill_call(&self, tool_call_id: &str) -> Option<SkillCall> {
        let effect = self.effect(EffectKind::ToolCall, tool_call_id)?;
        let tc = effect.tool()?;
        let ConnectorTarget::Skill { plugin, .. } = tc.target.as_ref()? else {
            return None;
        };
        Some(SkillCall {
            node: effect.anchor.clone(),
            plugin_id: plugin.clone(),
            arguments: tc.arguments.clone(),
            scripted: tc.script.is_some(),
        })
    }

    pub(crate) fn anchored_on(
        path: &std::collections::HashSet<String>,
        anchor: Option<&str>,
    ) -> bool {
        anchor.is_none_or(|a| path.contains(a))
    }

    pub fn retry_config(&self) -> Option<RetryConfig> {
        self.at_head().retry_config()
    }

    pub fn depth(&self) -> u32 {
        self.ancestry.len() as u32
    }

    pub fn servers_for(&self, config: &AgentConfig) -> Vec<BoundServer> {
        let plugin_servers = config
            .plugins
            .iter()
            .flat_map(|p| p.servers.iter().map(|name| p.server(name)));
        config
            .mcp
            .iter()
            .map(McpServer::bind)
            .chain(plugin_servers)
            .collect()
    }

    #[cfg(test)]
    pub fn placed(&self) -> Vec<prompt_context::Placed> {
        let mut nodes: Vec<&crate::tree::Shared> = self.tree.fresh().collect();
        nodes.sort_by_key(|n| n.seq);
        nodes
            .into_iter()
            .filter_map(|n| match &n.fact {
                crate::tree::Fact::Context { placed, .. } => Some(placed.clone()),
                _ => None,
            })
            .flatten()
            .collect()
    }

    pub fn has_node(&self, id: &str) -> bool {
        self.tree.has(id)
    }

    pub fn message_tree(&self) -> MessageTree {
        self.at_head().message_tree()
    }

    pub fn waiting_on_client(&self) -> bool {
        waiting_on_client(&self.effects())
    }

    pub fn effects(&self) -> Vec<Effect> {
        let mut effects: Vec<Effect> = self
            .effects
            .values()
            .filter(|e| {
                matches!(
                    e.tracking.status(),
                    EffectStatus::Queued
                        | EffectStatus::Pending
                        | EffectStatus::Running
                        | EffectStatus::RetryScheduled
                )
            })
            .filter_map(|e| {
                let mut wire = Effect {
                    id: e.id.clone(),
                    kind: e.kind(),
                    status: e.tracking.status(),
                    attempt: e.tracking.retry.attempts,
                    deadline: e.tracking.deadline,
                    anchor: e.anchor.clone(),
                    name: None,
                    arguments: None,
                    handler: None,
                    stream: None,
                    agent_id: None,
                    session_id: None,
                    branch: e.branch.clone(),
                };
                match &e.payload {
                    EffectPayload::ToolCall(c) => {
                        wire.name = Some(c.name.clone());
                        wire.arguments = Some(c.arguments.clone());
                        wire.handler = Some(c.handler.into());
                    }
                    EffectPayload::Subagent(c) => {
                        wire.agent_id = Some(c.agent_id.clone());
                        wire.session_id = Some(c.session_id.clone());
                    }
                    EffectPayload::LlmCall(c) => {
                        wire.handler = Some(c.handler.into());
                        wire.stream = Some(c.stream);
                    }
                    EffectPayload::ConnectorSync(_) => wire.name = Some(e.id.clone()),
                    EffectPayload::System1Call(c) => wire.name = Some(c.system1.clone()),
                    EffectPayload::Decision(_) => return None,
                }
                Some(wire)
            })
            .collect();
        effects.sort_by(|a, b| a.id.cmp(&b.id));
        effects
    }

    pub fn event_meta(&self, now: DateTime<Utc>) -> EventMeta {
        let mut decisions: Vec<MetaDecision> = self
            .effects_of(EffectKind::Decision)
            .filter(|e| {
                matches!(
                    e.tracking.status(),
                    EffectStatus::Pending | EffectStatus::Queued
                )
            })
            .filter_map(|e| {
                let d = e.decision()?;
                Some(MetaDecision {
                    decision_id: e.id.clone(),
                    branch: d.branch.clone(),
                    status: e.tracking.status(),
                    finished: matches!(
                        d.trigger,
                        Trigger::ToolFinished { .. }
                            | Trigger::SubagentFinished { .. }
                            | Trigger::BranchFinished {
                                mode: BranchMode::Blocking,
                                ..
                            }
                    ),
                    attempts: e.tracking.retry.attempts,
                    deadline: e.tracking.deadline,
                    source_event_sequence: d.source_event_sequence,
                })
            })
            .collect();

        decisions.sort_by_key(|d| d.source_event_sequence);

        EventMeta {
            status: self.projected_status(),
            wake_at: super::schedule::wake_at(self, now)
                .into_iter()
                .chain(self.deliveries.values().filter_map(|d| d.retry_at))
                .min(),
            owner: self.owner.clone(),
            agent_id: self.agent_id.clone(),
            ancestry: self.ancestry.clone(),
            turn_id: self.turn_id().map(str::to_string),
            cost: self.cost,
            subagent_cost: self.subagent_cost,
            head_id: self.head_id.clone(),
            calls: self.effects(),
            decisions,
        }
    }

    fn track_usage(&mut self, usage: &Option<Usage>) {
        if let Some(u) = usage {
            self.token_usage.add(u);
        }
    }

    fn track_turn_usage(&mut self, usage: &Option<Usage>) {
        if let Some(u) = usage {
            self.turn_token_usage.add(u);
            self.turn_calls.llm.tokens.add(u);
        }
    }
}

#[derive(Clone, Copy)]
pub struct SessionStateAtNode<'a, 'n> {
    state: &'a SessionState,
    node: Option<&'n str>,
}

impl<'a, 'n> SessionStateAtNode<'a, 'n> {
    pub fn state(&self) -> &'a SessionState {
        self.state
    }

    pub fn node(&self) -> Option<&'n str> {
        self.node
    }

    fn up(&self) -> crate::tree::Up<'a> {
        self.state.tree.up(self.node)
    }

    pub fn message_tree(&self) -> MessageTree {
        let mut nodes: Vec<NewMessage> = Vec::new();
        for node in self.up() {
            let crate::tree::Fact::Message {
                message,
                copied_from,
            } = &node.fact
            else {
                continue;
            };
            if let Some(child) = nodes.last_mut() {
                child.parent_id = Some(message.id.clone());
            }
            nodes.push(NewMessage {
                message: message.clone(),
                parent_id: None,
                copied_from: copied_from.clone(),
                branch: node.branch().map(str::to_string),
            });
        }
        nodes.reverse();
        MessageTree {
            head_id: nodes.last().map(|n| n.message.id.clone()),
            nodes,
        }
    }

    fn path_set(&self) -> std::collections::HashSet<String> {
        self.node
            .map(|n| self.state.path_ids(n))
            .unwrap_or_default()
    }

    pub fn active_interrupt_for(&self) -> Option<&'a OpenInterrupt> {
        resolve_on_path(&self.state.open_interrupts, &self.path_set())
    }

    pub fn interrupts_for(&self) -> Vec<&'a OpenInterrupt> {
        let on_path = self.path_set();
        self.state
            .open_interrupts
            .iter()
            .filter(|i| match i.anchor.as_deref() {
                None => true,
                Some(a) => on_path.contains(a),
            })
            .collect()
    }

    pub fn anchor_on_path(&self, anchor: Option<&str>) -> bool {
        SessionState::anchored_on(&self.path_set(), anchor)
    }

    pub fn resolve_state_for(&self) -> WorkerState {
        self.up()
            .find_map(|n| match &n.fact {
                crate::tree::Fact::State { state } => Some(state.clone()),
                _ => None,
            })
            .unwrap_or_default()
    }

    pub fn resolve_agent_for(&self) -> Option<AgentConfig> {
        self.up()
            .find_map(|n| match &n.fact {
                crate::tree::Fact::Config { config, .. } => Some(config.clone()),
                _ => None,
            })
            .flatten()
    }

    pub fn connector_tool(&self, name: &str) -> Option<ConnectorTool> {
        self.connector_tools()
            .tools
            .into_iter()
            .find(|t| t.name == name)
    }

    pub fn tool_handler(&self, name: &str) -> ToolHandler {
        let config = self.resolve_agent_for();
        let connector = || {
            self.connector_tool(name)
                .is_some_and(|t| t.kind != ConnectorToolKind::Subagent)
        };
        match config.as_ref().and_then(|c| c.tool(name)) {
            Some(t) => ToolHandler::declared(t.handler),
            None if connector() => ToolHandler::Server,
            None => ToolHandler::Worker,
        }
    }

    pub fn retry_config(&self) -> Option<RetryConfig> {
        self.resolve_agent_for().and_then(|c| c.retry).map(|b| *b)
    }

    pub fn connector_tools(&self) -> filter::Merged {
        let Some(config) = self.resolve_agent_for() else {
            return filter::Merged {
                tools: Vec::new(),
                collisions: Vec::new(),
            };
        };
        self.state.connector_tools_for_config(&config)
    }

    fn connector_source(&self, connector_id: &ConnectionPath) -> Option<Source> {
        let config = self.resolve_agent_for()?;
        let server = self
            .state
            .servers_for(&config)
            .into_iter()
            .find(|c| c.path == *connector_id)?;
        let sync = self.state.connector_sync(connector_id)?;
        Some(Source {
            server,
            offered: sync.tools.clone(),
            instructions: sync.instructions.clone(),
        })
    }

    fn searchable_connectors(&self) -> Vec<Source> {
        let Some(config) = self.resolve_agent_for() else {
            return Vec::new();
        };
        self.state
            .servers_for(&config)
            .iter()
            .filter(|c| {
                self.state
                    .effect(EffectKind::ConnectorSync, &c.path.to_string())
                    .is_some_and(|e| e.tracking.is_ready())
            })
            .filter_map(|c| self.connector_source(&c.path))
            .collect()
    }

    /// The tools `tool_search` reaches: the agent's own and its connections',
    /// but those a skill holds back. In code mode, these are the tools a
    /// script calls.
    pub fn searchable_tools(&self) -> Vec<LlmTool> {
        let Some(config) = self.resolve_agent_for() else {
            return Vec::new();
        };
        let locked = config.locked_tools(&self.loaded_skills());
        let connector_tools = self.state.connector_tools_for_config(&config).tools;
        let declared = config
            .tools
            .iter()
            .map(|t| t.to_llm_tool(config.defers_tools()));
        let connector = connector_tools
            .iter()
            .filter(|t| {
                matches!(
                    t.kind,
                    ConnectorToolKind::Remote | ConnectorToolKind::Subagent
                )
            })
            .map(|t| t.to_llm_tool());
        declared
            .chain(connector)
            .filter(|t| !locked.contains_key(&t.name))
            .collect()
    }

    /// In code mode, the one tool the model gets: `run_code`, declaring the
    /// tools a script calls that are not deferred. It does not change while
    /// the session runs, but for tools that connect later.
    pub fn run_code(&self) -> Option<LlmTool> {
        let language = self.resolve_agent_for()?.codemode?;
        let tools = self.searchable_tools();
        let direct: Vec<&LlmTool> = tools.iter().filter(|t| !t.defer).collect();
        Some(crate::codemode::offered(language, &direct))
    }

    /// The tools a script can call that the skill `named` names.
    pub fn skill_tools(&self, named: &str) -> Vec<LlmTool> {
        let Some(config) = self.resolve_agent_for() else {
            return Vec::new();
        };
        let named_tools: Vec<String> = config
            .named_skills()
            .iter()
            .filter(|s| s.name == named)
            .flat_map(|s| s.tools.iter().cloned())
            .collect();
        self.searchable_tools()
            .into_iter()
            .filter(|t| named_tools.contains(&t.name))
            .collect()
    }

    /// The skills loaded on this path: each `skill` call that answered.
    pub fn loaded_skills(&self) -> std::collections::HashSet<String> {
        let named = |arguments: &str| -> Option<String> {
            let arguments: serde_json::Value = serde_json::from_str(arguments).ok()?;
            Some(arguments.get("name")?.as_str()?.to_string())
        };
        let mut answered = std::collections::HashSet::new();
        let mut loaded = std::collections::HashSet::new();
        for node in self.up() {
            let Some(m) = node.message() else { continue };
            if let Some(id) = &m.tool_call_id {
                answered.insert(id.clone());
            }
            loaded.extend(
                m.tool_calls
                    .iter()
                    .filter(|c| c.function.name == SKILL && answered.contains(&c.id))
                    .filter_map(|c| named(&c.function.arguments)),
            );
        }
        let path = self.path_set();
        loaded.extend(
            self.state
                .effects
                .values()
                .filter(|e| SessionState::anchored_on(&path, e.anchor.as_deref()))
                .filter(|e| e.tracking.status() == EffectStatus::Completed)
                .filter_map(EffectState::tool)
                .filter(|tc| tc.name == SKILL)
                .filter_map(|tc| named(&tc.arguments)),
        );
        loaded
    }

    pub fn locked_tools(&self) -> BTreeMap<String, Vec<String>> {
        self.resolve_agent_for()
            .map(|config| config.locked_tools(&self.loaded_skills()))
            .unwrap_or_default()
    }

    pub fn locked_fault(&self, named: &str) -> Option<String> {
        let locked = self.locked_tools();
        Some(crate::prompts::errors::locked(named, locked.get(named)?))
    }

    pub fn connection_summary(&self, path: &ConnectionPath) -> Option<String> {
        let source = self
            .searchable_connectors()
            .into_iter()
            .find(|source| source.server.path == *path)?;
        serde_json::to_string(&Summary {
            mcp_server: &source.server.path,
            tools: filter::callable(&source.server, &source.offered).len(),
            about: source.instructions.as_deref(),
        })
        .ok()
    }

    pub fn unavailable_connectors(
        &self,
        config: &AgentConfig,
    ) -> Vec<(ConnectionPath, Option<AuthNeed>)> {
        self.state
            .servers_for(config)
            .into_iter()
            .filter(|c| c.tool_sync_failure.warns())
            .filter_map(|c| {
                let effect = self
                    .state
                    .effect(EffectKind::ConnectorSync, &c.path.to_string())?;
                if effect.tracking.status() != EffectStatus::Failed {
                    return None;
                }
                Some((c.path.clone(), effect.connector().and_then(|s| s.auth)))
            })
            .collect()
    }

    pub fn unavailable_connector_paths(&self, config: &AgentConfig) -> Vec<ConnectionPath> {
        self.unavailable_connectors(config)
            .into_iter()
            .map(|(path, _)| path)
            .collect()
    }

    fn contexts(&self) -> Vec<prompt_context::Placed> {
        let mut placed: Vec<Vec<prompt_context::Placed>> = Vec::new();
        for node in self.up() {
            match &node.fact {
                crate::tree::Fact::Context { placed: here, .. } => placed.push(here.clone()),
                crate::tree::Fact::Config { root: true, .. } => break,
                _ => {}
            }
        }
        placed.into_iter().rev().flatten().collect()
    }

    pub fn said(&self) -> std::collections::HashSet<String> {
        self.contexts().into_iter().flat_map(|p| p.ids).collect()
    }

    pub fn outgoing(
        &self,
        id: &str,
        source: &PromptSource,
        given: &[Message],
        applied: &[String],
    ) -> Outgoing {
        let node = self.node.map(str::to_string);
        let mut out = Outgoing {
            node,
            prompt: given.to_vec(),
            placed: Vec::new(),
            places: false,
            context_ids: applied.to_vec(),
        };
        match source {
            PromptSource::Explicit => return out,
            PromptSource::Tree { .. } => {
                let view: Vec<DraftMessage> = self
                    .transcript()
                    .into_iter()
                    .map(DraftMessage::from)
                    .collect();
                let prompt = match self.resolve_agent_for() {
                    Some(config) => config.prompt_for(&view),
                    None => view,
                };
                out.prompt = prompt
                    .into_iter()
                    .enumerate()
                    .map(|(i, m)| m.recorded(format!("{id}:{i}")))
                    .collect();
                prompt_context::restore(&mut out.prompt, &self.placed_on_path());
                let open = self.system_prefix_open(id);
                prompt_context::merge(
                    &mut out.prompt,
                    &mut Vec::new(),
                    &mut out.placed,
                    id,
                    open,
                    prompt_context::owed(*self),
                );
                out.places = open || !out.placed.is_empty();
            }
            PromptSource::Legacy => {
                if !self.state.is_summary_call(id) {
                    prompt_context::carry(&mut out.prompt, &self.placed_on_path());
                }
                let open = self.system_prefix_open(id);
                prompt_context::merge(
                    &mut out.prompt,
                    &mut out.context_ids,
                    &mut out.placed,
                    id,
                    open,
                    prompt_context::owed(*self),
                );
                out.places = open || !out.placed.is_empty();
            }
        }
        out
    }

    pub fn placed_on_path(&self) -> Vec<prompt_context::Placed> {
        self.contexts()
    }

    pub fn system_prefix_open(&self, exclude: &str) -> bool {
        let placed = self
            .up()
            .any(|n| matches!(&n.fact, crate::tree::Fact::Context { call, .. } if call != exclude));
        let path = self.path_set();
        !placed
            && !self.state.effects.values().any(|e| {
                e.id != exclude
                    && e.llm().is_some_and(|c| c.source != PromptSource::Explicit)
                    && SessionState::anchored_on(&path, e.anchor.as_deref())
                    && e.tracking.status() != EffectStatus::Queued
            })
    }

    pub fn call_tool_target(&self, named: &str) -> Option<CallTarget> {
        let config = self.resolve_agent_for()?;
        if config
            .locked_tools(&self.loaded_skills())
            .contains_key(named)
        {
            return None;
        }
        if let Some(declared) = config.tool(named) {
            return Some(CallTarget::Declared(declared.clone()));
        }
        self.connector_tool(named).and_then(|t| match t.kind {
            ConnectorToolKind::Remote => Some(CallTarget::Connector(t)),
            ConnectorToolKind::Subagent => Some(CallTarget::Subagent(t)),
            _ => None,
        })
    }

    pub fn call_tool_fault(&self, arguments: &str) -> Option<String> {
        let raw: serde_json::Value = serde_json::from_str(arguments).unwrap_or_default();
        let named = raw
            .get("name")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string();

        if named == CALL_TOOL {
            return Some(crate::prompts::errors::call_tool_in_call());
        }
        if let Some(fault) = self.locked_fault(&named) {
            return Some(fault);
        }
        let Some(target) = self.call_tool_target(&named) else {
            let config = self.resolve_agent_for();
            let tools = self.searchable_tools();
            let cap = config
                .as_ref()
                .map(AgentConfig::defer_settings)
                .unwrap_or_default()
                .max_matches;
            let near = filter::closest(&tools, &named, cap);
            let unavailable = config
                .as_ref()
                .map(|c| self.unavailable_connector_paths(c))
                .unwrap_or_default();
            return Some(crate::prompts::errors::no_such_tool(
                &named,
                &near,
                &unavailable,
            ));
        };
        let input = target.input();
        let fault = match raw.get("arguments") {
            Some(_) => crate::prompts::errors::bad_arguments,
            None => crate::prompts::errors::no_arguments,
        };
        classify_arguments(&inner_arguments(&raw), input.as_ref())
            .error()
            .map(|e| fault(&named, e, input.as_ref()))
    }

    pub fn unsynced_connectors(&self) -> Vec<ConnectionPath> {
        let Some(config) = self.resolve_agent_for() else {
            return Vec::new();
        };
        self.state
            .servers_for(&config)
            .iter()
            .filter(|c| {
                !self
                    .state
                    .has_effect(EffectKind::ConnectorSync, &c.path.to_string())
            })
            .map(|c| c.path.clone())
            .collect()
    }

    pub fn has_pending_connector_sync(&self) -> bool {
        let Some(config) = self.resolve_agent_for() else {
            return false;
        };
        self.state.servers_for(&config).iter().any(|c| {
            self.state
                .tracking(EffectKind::ConnectorSync, &c.path.to_string())
                .is_some_and(EffectTracking::is_in_flight)
        })
    }

    pub fn transcript(&self) -> Vec<Message> {
        self.state.tree.messages(self.node)
    }

    pub fn open_llm_calls(&self) -> HashMap<String, EffectState> {
        let path = self.transcript();
        let answered: std::collections::HashSet<&str> = path
            .iter()
            .filter_map(|m| m.tool_call_id.as_deref())
            .collect();
        path.iter()
            .filter(|m| {
                m.tool_calls
                    .iter()
                    .any(|c| !answered.contains(c.id.as_str()))
            })
            .filter_map(|m| {
                self.state
                    .effect(EffectKind::LlmCall, &m.id)
                    .map(|e| (m.id.clone(), e.clone()))
            })
            .collect()
    }
}

#[cfg(test)]
mod open_llm_calls_tests {
    use super::*;
    use crate::protocol::{NewMessage, ToolCall, ToolCallFunction};

    fn node(message: Message, parent_id: Option<&str>) -> Logged<NewMessage> {
        Logged {
            seq: 0,
            entry: NewMessage {
                message,
                parent_id: parent_id.map(str::to_string),
                copied_from: None,
                branch: None,
            },
        }
    }

    fn user(id: &str) -> Message {
        Message {
            id: id.to_string(),
            role: Role::User,
            content: None,
            tool_calls: vec![],
            tool_call_id: None,
            name: None,
            reasoning: None,
        }
    }

    fn assistant_calling(id: &str, tool_call_id: &str) -> Message {
        Message {
            id: id.to_string(),
            role: Role::Assistant,
            content: None,
            tool_calls: vec![ToolCall {
                id: tool_call_id.to_string(),
                call_type: "function".to_string(),
                function: ToolCallFunction {
                    name: "t".to_string(),
                    arguments: "{}".to_string(),
                },
            }],
            tool_call_id: None,
            name: None,
            reasoning: None,
        }
    }

    fn tool_answer(id: &str, tool_call_id: &str) -> Message {
        Message {
            id: id.to_string(),
            role: Role::Tool,
            content: None,
            tool_calls: vec![],
            tool_call_id: Some(tool_call_id.to_string()),
            name: None,
            reasoning: None,
        }
    }

    fn call_state(call_id: &str) -> EffectState {
        EffectState::new(
            call_id,
            EffectTracking::new(RetryPolicy::no_retry(), Utc::now()),
            EffectPayload::LlmCall(LlmCallState {
                source: Default::default(),
                defer_tools_strategy: Default::default(),
                context_ids: Vec::new(),
                sent_tools: None,
                unanswered: Vec::new(),
                format: None,
                llm: "claude".to_string(),
                prompt: vec![],
                spec: LlmCallSpec {
                    model: "m".to_string(),
                    tools: None,
                    temperature: None,
                    max_completion_tokens: None,
                    reasoning: None,
                },
                stream: false,
                handler: LlmHandler::Server,
                dispatched_seq: None,
            }),
        )
    }

    #[test]
    fn a_call_is_open_until_its_tool_answer_is_recorded() {
        let mut s = SessionState::new("sess-1".to_string());
        s.seed_message(node(user("u1"), None).entry);
        s.seed_message(node(assistant_calling("call-1", "tc-1"), Some("u1")).entry);
        s.head_id = Some("call-1".to_string());
        s.put_effect(call_state("call-1"));

        assert!(
            s.at_head().open_llm_calls().contains_key("call-1"),
            "unanswered tool call keeps the parent open"
        );

        s.seed_message(node(tool_answer("t1", "tc-1"), Some("call-1")).entry);
        s.head_id = Some("t1".to_string());
        assert!(
            s.at_head().open_llm_calls().is_empty(),
            "recorded answer closes the call"
        );
    }
}

#[cfg(test)]
mod tree_tests {
    use super::*;
    use crate::tree::{Fact, Node, Place};

    fn config(model: &str) -> AgentConfig {
        serde_json::from_value(serde_json::json!({ "model": model })).unwrap()
    }

    fn user(id: &str) -> Message {
        serde_json::from_value(serde_json::json!({ "id": id, "role": "user", "content": id }))
            .unwrap()
    }

    fn put(s: &mut SessionState, id: &str, parent: Option<&str>, fact: Fact) {
        s.seed(Node {
            place: Place {
                id: id.to_string(),
                parent_id: parent.map(str::to_string),
                branch: None,
            },
            fact,
        });
    }

    fn state(v: i64) -> Fact {
        Fact::State {
            state: WorkerState(serde_json::json!({ "v": v })),
        }
    }

    fn message(id: &str) -> Fact {
        Fact::Message {
            message: user(id),
            copied_from: None,
        }
    }

    fn read(s: &SessionState, at: &str) -> serde_json::Value {
        s.at(Some(at)).resolve_state_for().0
    }

    #[test]
    fn the_nearest_write_above_a_node_is_what_it_reads() {
        let mut s = SessionState::new("s".into());
        put(
            &mut s,
            "c0",
            None,
            Fact::Config {
                config: Some(config("m1")),
                root: false,
            },
        );
        put(&mut s, "s1", Some("c0"), state(1));
        put(&mut s, "u1", Some("s1"), message("u1"));
        put(&mut s, "s2", Some("u1"), state(2));
        put(&mut s, "u2", Some("s2"), message("u2"));
        assert_eq!(read(&s, "u1"), serde_json::json!({ "v": 1 }));
        assert_eq!(read(&s, "u2"), serde_json::json!({ "v": 2 }));
        assert_eq!(
            s.at(Some("u2")).resolve_agent_for().map(|c| c.model),
            Some("m1".into())
        );
        assert_eq!(
            s.at(Some("u2")).transcript().len(),
            2,
            "only messages are the transcript"
        );
    }

    #[test]
    fn a_write_after_a_fork_is_not_on_the_fork() {
        let mut s = SessionState::new("s".into());
        put(&mut s, "u1", None, message("u1"));
        put(&mut s, "s1", Some("u1"), state(1));
        put(&mut s, "x1", Some("u1"), message("x1"));
        assert_eq!(read(&s, "s1"), serde_json::json!({ "v": 1 }));
        assert_eq!(read(&s, "x1"), serde_json::Value::Null);
    }

    #[test]
    fn a_branch_with_its_own_config_reads_it_and_inherits_state() {
        let mut s = SessionState::new("s".into());
        put(
            &mut s,
            "c0",
            None,
            Fact::Config {
                config: Some(config("m1")),
                root: false,
            },
        );
        put(&mut s, "s1", Some("c0"), state(1));
        put(&mut s, "u1", Some("s1"), message("u1"));
        let start = |config| Fact::Config { config, root: true };
        put(&mut s, "b1", Some("u1"), start(Some(config("m2"))));
        put(&mut s, "b2", Some("u1"), start(None));
        let model = |at| s.at(Some(at)).resolve_agent_for().map(|c| c.model);
        assert_eq!(model("b1"), Some("m2".into()));
        assert_eq!(model("b2"), None);
        assert_eq!(model("u1"), Some("m1".into()));
        assert_eq!(read(&s, "b1"), serde_json::json!({ "v": 1 }));
    }

    #[test]
    fn the_message_view_skips_other_nodes() {
        let mut s = SessionState::new("s".into());
        put(&mut s, "u1", None, message("u1"));
        put(&mut s, "s1", Some("u1"), state(1));
        put(&mut s, "u2", Some("s1"), message("u2"));
        let tree = s.message_tree();
        assert_eq!(tree.head_id.as_deref(), Some("u2"));
        let parents: Vec<_> = tree.nodes.iter().map(|n| n.parent_id.clone()).collect();
        assert_eq!(parents, vec![None, Some("u1".to_string())]);
    }
}

#[cfg(test)]
mod effect_tests {
    use super::*;

    #[test]
    fn effect_serializes_flat_tagged_and_round_trips() {
        let e = Effect {
            id: "call_1".to_string(),
            kind: EffectKind::ToolCall,
            status: EffectStatus::Pending,
            attempt: 0,
            deadline: None,
            anchor: Some("n1".to_string()),
            name: Some("get_weather".to_string()),
            arguments: Some("{}".to_string()),
            handler: Some(crate::protocol::Handler::Worker),
            stream: None,
            agent_id: None,
            session_id: None,
            branch: None,
        };
        let json = serde_json::to_value(&e).unwrap();
        assert_eq!(json["id"], "call_1");
        assert_eq!(json["status"], "pending");
        assert_eq!(json["kind"], "tool_call");
        assert_eq!(json["name"], "get_weather");
        assert_eq!(json["handler"], "worker");
        assert!(json.get("stream").is_none());
        assert!(json.get("agent_id").is_none());
        let back: Effect = serde_json::from_value(json).unwrap();
        assert_eq!(back, e);
    }
}

/// Whether the only open calls wait on the client. A `run_code` call only
/// waits on the calls its script made, so it does not count.
pub fn waiting_on_client(calls: &[Effect]) -> bool {
    let mut open = calls
        .iter()
        .filter(|c| c.name.as_deref() != Some(RUN_CODE))
        .peekable();
    open.peek().is_some() && open.all(|c| c.handler == Some(Handler::Client))
}

const TITLE_CHARS: usize = 200;

/// A message as a one-line title: whitespace collapsed, clipped.
pub fn title(text: &str) -> Option<String> {
    let line = text.split_whitespace().collect::<Vec<_>>().join(" ");
    (!line.is_empty()).then(|| line.chars().take(TITLE_CHARS).collect())
}

#[cfg(test)]
mod summary_fold_tests {
    use serde_json::json;

    use super::*;

    fn apply(s: &mut SessionState, payload: serde_json::Value) {
        let event: EventPayload = serde_json::from_value(payload).unwrap();
        s.apply(
            &event,
            &ApplyContext {
                occurred_at: Utc::now(),
                sequence: 1,
            },
        );
    }

    fn client_message(id: &str, text: &str) -> serde_json::Value {
        json!({
            "type": "decision.queued",
            "id": id,
            "trigger": {"type": "client.message", "messages": [{"role": "user", "content": text}]},
        })
    }

    #[test]
    fn the_title_is_the_first_client_message_on_one_line() {
        let mut s = SessionState::new("sess-1".to_string());
        apply(&mut s, client_message("d1", "  Where is\n my   order? "));
        apply(&mut s, client_message("d2", "Any update?"));
        assert_eq!(s.title.as_deref(), Some("Where is my order?"));
    }

    #[test]
    fn a_title_is_clipped() {
        assert_eq!(
            title(&"x".repeat(500)).unwrap().chars().count(),
            TITLE_CHARS
        );
        assert_eq!(title("   "), None);
    }

    #[test]
    fn the_last_turn_decides_whether_the_session_failed() {
        let mut s = SessionState::new("sess-1".to_string());
        apply(
            &mut s,
            json!({"type": "turn.completed", "turn_id": "t1", "error": {"message": "boom", "code": "invalid_response"}}),
        );
        assert!(s.last_turn_failed);
        apply(&mut s, json!({"type": "turn.completed", "turn_id": "t2"}));
        assert!(!s.last_turn_failed);
    }
}

/// The tools a call sends: those it asked for, then its connections', but
/// those a skill holds back.
fn sent_tools(
    asked: Option<Vec<LlmTool>>,
    connector_tools: &[ConnectorTool],
    locked: &BTreeMap<String, Vec<String>>,
) -> Option<Vec<LlmTool>> {
    let mut sent = asked;
    if !connector_tools.is_empty() {
        let tools = sent.get_or_insert_with(Vec::new);
        for tool in connector_tools {
            if !tools.iter().any(|t| t.name == tool.name) {
                tools.push(tool.to_llm_tool());
            }
        }
    }
    if let Some(tools) = sent.as_mut() {
        tools.retain(|t| !locked.contains_key(&t.name));
    }
    sent
}
