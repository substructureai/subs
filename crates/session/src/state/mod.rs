mod at;
mod effects;
mod fold;

pub use at::*;
pub use effects::*;
pub use fold::*;

use std::collections::{BTreeMap, HashMap};

use chrono::{DateTime, Utc};

use serde::{Deserialize, Serialize};

use super::decision::{BranchConfig, LlmHandler, ToolHandler, Trigger};

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

use rust_decimal::Decimal;

pub use crate::protocol::EffectKind;

use crate::protocol::{
    AgentConfig, BranchMode, BranchView, ClientContext, ContextReason, DecisionTurn, DraftMessage,
    Effect, EffectStatus, HeldCall, InterruptOrigin, LlmFormat, LlmRequest, LlmTool, Message,
    MessageTree, NewMessage, ReasoningConfig, RetryConfig, RetryPolicy, Role, SessionOwner,
    SpawnMode, SubagentTools, Usage, WorkerRef, WorkerState,
};

use crate::retry::RetryState;

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

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LastRead {
    pub call_id: String,
    pub tokens: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Limited {
    pub reason: ContextReason,
    pub call: HeldCall,
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
pub struct BranchState {
    pub node: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent: Option<String>,
    pub agent: BranchConfig,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    pub mode: BranchMode,
    pub seq: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub leaf: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subagent: Option<String>,
    pub depth: usize,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub detached: bool,
}

impl BranchState {
    pub fn started(p: &BranchStarted, seq: u64, depth: usize, detached: bool) -> Self {
        BranchState {
            node: p.node.clone(),
            from: p.from.clone(),
            parent: p.parent.clone(),
            agent: p.agent.clone(),
            name: p.name.clone(),
            mode: p.mode,
            seq,
            leaf: p.leaf.clone(),
            turn_id: p.turn_id.clone(),
            subagent: p.subagent.clone(),
            depth,
            detached,
        }
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
    pub cost: Decimal,
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
    pub cost: Decimal,
    pub subagent_cost: Decimal,
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
    pub token_usage: Usage,

    pub cost: Decimal,

    pub subagent_cost: Decimal,

    pub turn_cost: Decimal,

    pub turn_token_usage: Usage,

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

    pub phase: TurnPhase,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_started_seq: Option<u64>,

    pub turns: u64,

    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub recent_turns: Vec<String>,

    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub last_turn_failed: bool,

    /// The first thing a client said, kept even when no turn ever answered it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,

    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub detached_turns: Vec<String>,

    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub deliveries: BTreeMap<String, crate::delivery::Delivery>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub head_id: Option<String>,

    #[serde(skip)]
    pub tree: crate::tree::Tree,

    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub open_interrupts: Vec<OpenInterrupt>,

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
            turns: 0,
            recent_turns: Vec::new(),
            last_turn_failed: false,
            title: None,
            detached_turns: Vec::new(),
            head_id: None,
            tree: crate::tree::Tree::default(),
            open_interrupts: Vec::new(),
            session_start_failed: false,
            branches: BTreeMap::new(),
        }
    }

    pub fn open_branch(&self, id: &str) -> Option<&BranchState> {
        self.branches.get(id)
    }

    pub fn parallel_branches(&self) -> bool {
        self.at_head()
            .config()
            .and_then(|c| c.parallel_branches)
            .unwrap_or(false)
    }

    pub fn lane(&self, branch: Option<&str>) -> Option<String> {
        branch
            .filter(|_| self.parallel_branches())
            .map(str::to_string)
    }

    pub fn detached(&self, branch: Option<&str>) -> bool {
        branch
            .and_then(|b| self.branches.get(b))
            .is_some_and(|b| b.detached)
    }

    pub fn held_by_branches(&self, parent: Option<&str>) -> usize {
        self.branches
            .values()
            .filter(|b| b.mode == BranchMode::Blocking)
            .filter(|b| b.parent.as_deref() == parent)
            .count()
    }

    pub fn node_branch(&self, id: &str) -> Option<String> {
        self.tree
            .get(id)
            .and_then(|n| n.branch().map(str::to_string))
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

    pub fn turn_completed(&self, turn_id: &str) -> bool {
        self.recent_turns.iter().any(|t| t == turn_id)
    }

    pub fn turn_id(&self) -> Option<&str> {
        self.phase
            .turn_id()
            .or_else(|| self.recent_turns.last().map(String::as_str))
    }

    pub fn check_invariants(&self) -> Result<(), String> {
        let mut lanes: BTreeMap<Option<String>, usize> = BTreeMap::new();
        for e in self
            .effects_of(EffectKind::Decision)
            .filter(|e| e.tracking.status == EffectStatus::Pending)
        {
            *lanes.entry(self.lane(e.branch.as_deref())).or_default() += 1;
        }
        if lanes.values().any(|n| *n > 1) {
            return Err(format!("decision slot violated: {lanes:?} live decisions"));
        }
        if self
            .effects_of(EffectKind::Decision)
            .filter(|e| e.tracking.status == EffectStatus::Pending)
            .filter_map(|e| e.decision())
            .any(|d| {
                let node = match d.branch.as_deref() {
                    Some(b) if !self.branches.contains_key(b) => self
                        .tree
                        .branch(b)
                        .and_then(|r| r.state.at().map(str::to_string)),
                    b => self.scope_node(b),
                };
                self.at(node.as_deref()).has_pending_connector_sync()
            })
        {
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
        .and_then(|e| e.asked_at.as_deref())
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

    pub fn tool_handler_for(&self, name: &str) -> ToolHandler {
        self.at_head().tool_handler(name)
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
        let node = effect.asked_at.clone();
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
                effect.asked_at.as_deref(),
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
            node: effect.asked_at.clone(),
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

    pub fn waiting_on_client(&self) -> bool {
        waiting_on_client(&self.effects())
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
            calls: self.effects(),
            decisions,
        }
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
