use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use rust_decimal::Decimal;

use super::decision::{Action, LlmHandler, PromptSource, Trigger};
use super::effects::{self, decision_queued, void_events, Settle};
use super::events::*;
use super::schedule::{self, ScheduleStep};
use super::state::{EffectKind, EffectTracking, EventMeta, SessionState, TurnPhase};
use crate::branch::Outside;
use crate::cx::Cx;
use crate::ids::Ids;
use crate::protocol::{
    AgentConfig, ClientMessage, ClientPayload, DraftMessage, EffectStatus, InterruptOrigin,
    LlmFormat, LlmRequest, QueueMode, RetryOverride, RetryPolicy, SessionOwner, SpawnMode, Usage,
    WorkerRef, WorkerState,
};
use crate::protocol::{ErrorInfo, HeldCall};
use crate::retry::RetryTarget;
use crate::system1::System1Request;
use crate::Caller;

pub use super::effects::{Outcome, SettleError};

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, strum::IntoStaticStr)]
#[serde(tag = "command", rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum CommandPayload {
    CreateSession {
        agent_id: String,
        owner: SessionOwner,
        ancestry: Vec<String>,
        worker_retry: RetryPolicy,
        agent: Option<Box<AgentConfig>>,
        state: Option<WorkerState>,
        worker: Option<WorkerRef>,
        #[serde(default)]
        events: Option<String>,
    },
    SubmitClientPayload {
        payload: ClientPayload,
        turn: TurnTarget,
        #[serde(default)]
        queue: QueueMode,
        #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
        metadata: BTreeMap<String, String>,
    },
    SendMessage {
        message: DraftMessage,
        #[allow(dead_code)]
        stream: bool,
        turn_id: Option<String>,
        parent_id: Option<String>,
    },
    RequestLlmCall {
        call_id: String,
        llm: String,
        request: LlmRequest,
        #[serde(default)]
        source: PromptSource,
        stream: bool,
        retry: RetryPolicy,
        handler: LlmHandler,
        format: Option<LlmFormat>,
    },
    SettleEffect {
        kind: EffectKind,
        id: String,
        attempt: Option<u32>,
        outcome: Outcome,
    },
    RequestSystem1Call {
        call_id: String,
        system1: String,
        request: System1Request,
        retry: RetryPolicy,
    },
    RequestToolCall {
        tool_call_id: String,
        name: String,
        arguments: String,
        retry: Option<RetryOverride>,
    },
    RequestSubagent {
        session_id: Option<String>,
        agent_id: String,
        tool_call_id: String,
        message: Option<DraftMessage>,
        retry: RetryPolicy,
        decision_id: String,
        mode: Option<SpawnMode>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        config: Option<Box<AgentConfig>>,
    },
    CompleteSubagentTurn {
        session_id: String,
        agent_id: String,
        turn_id: String,
        data: serde_json::Value,
        cost: Decimal,
        token_usage: Usage,
        error: Option<ErrorInfo>,
    },
    Interrupt {
        interrupt_id: String,
        reason: String,
        payload: serde_json::Value,
    },
    ResumeInterrupt {
        interrupt_id: String,
        payload: serde_json::Value,
    },
    SubmitWorkerDecision {
        decision_id: String,
        transcript: Vec<DraftMessage>,
        actions: Vec<Action>,
        state: Option<WorkerState>,
        agent: Option<AgentConfig>,
    },
    CancelSession {
        /// Drop the messages that wait for the turn, instead of running them
        /// next.
        #[serde(default)]
        drop_queued: bool,
    },
    FinishTurn {
        data: serde_json::Value,
    },
    CompleteTurn,
    Wake {
        now: DateTime<Utc>,
    },
    DeliveryFailed(crate::delivery::Undelivered),
    WorkUnsent {
        unsent: Vec<crate::delivery::Unsent>,
    },
    DeliveryRecovered {
        subscription: String,
        through: u64,
    },
    ResumeDelivery {
        subscription: String,
    },
}

impl CommandPayload {
    pub fn kind(&self) -> &'static str {
        self.into()
    }

    pub fn natural_id(&self) -> Option<crate::entry::CommandId> {
        match self {
            Self::SettleEffect {
                kind,
                id,
                attempt,
                outcome,
            } => match outcome {
                Outcome::Script { .. } => None,
                _ => Some(crate::work::WorkId::new(*kind, id.clone(), *attempt).settle_id()),
            },
            _ => None,
        }
    }

    pub fn settle(
        kind: EffectKind,
        id: impl Into<String>,
        attempt: Option<u32>,
        outcome: impl Into<Outcome>,
    ) -> Self {
        CommandPayload::SettleEffect {
            kind,
            id: id.into(),
            attempt,
            outcome: outcome.into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", content = "turn_id", rename_all = "snake_case")]
pub enum TurnTarget {
    Open(String),
    Continue(String),
    Detached,
}

#[derive(Debug, Clone, PartialEq, thiserror::Error, serde::Serialize, serde::Deserialize)]
#[serde(tag = "error", rename_all = "snake_case")]
pub enum SessionError {
    #[error("session has not been created")]
    SessionNotCreated,
    #[error("session already exists")]
    SessionAlreadyCreated,
    #[error("session is interrupted")]
    SessionInterrupted,
    #[error("session start failed")]
    SessionStartFailed,
    #[error("turn already active: {turn_id}")]
    TurnAlreadyActive { turn_id: String },
    #[error("turn already completed: {turn_id}")]
    TurnAlreadyCompleted { turn_id: String },
    #[error("session has no active turn")]
    NoActiveTurn,
    #[error("client subject is required")]
    MissingSubject,
    #[error("session access denied")]
    SessionAccessDenied,
    #[error("effect not found")]
    EffectNotFound,
    #[error("effect is not pending")]
    EffectNotPending,
    #[error("effect attempt mismatch")]
    EffectAttemptMismatch,
    #[error("caller may not settle this effect")]
    EffectWrongHandler,
}

impl Action {
    fn into_command(self, decision_id: &str) -> Option<CommandPayload> {
        match self {
            Action::CallLlm {
                id,
                llm,
                request,
                source,
                stream,
                retry,
                handler,
                format,
                branch: _,
            } => Some(CommandPayload::RequestLlmCall {
                call_id: id,
                llm,
                request,
                source,
                stream,
                retry,
                handler,
                format,
            }),
            Action::CallSystem1 {
                id,
                system1,
                request,
                retry,
            } => Some(CommandPayload::RequestSystem1Call {
                call_id: id,
                system1,
                request,
                retry,
            }),
            Action::CallTool {
                id,
                name,
                arguments,
                retry,
            } => Some(CommandPayload::RequestToolCall {
                tool_call_id: id,
                name,
                arguments,
                retry,
            }),
            Action::SpawnSubagent {
                session_id,
                agent_id,
                tool_call_id,
                message,
                retry,
                mode,
                config,
            } => Some(CommandPayload::RequestSubagent {
                session_id,
                agent_id,
                tool_call_id,
                message,
                retry,
                decision_id: decision_id.to_string(),
                mode,
                config,
            }),
            Action::ToolResult {
                id,
                attempt,
                result,
            } => Some(CommandPayload::settle(
                EffectKind::ToolCall,
                id,
                attempt,
                Outcome::Tool { result },
            )),
            Action::LlmResult {
                id,
                attempt,
                response,
            } => Some(CommandPayload::settle(
                EffectKind::LlmCall,
                id,
                attempt,
                Outcome::Llm(Box::new(response)),
            )),
            Action::ToolError {
                id,
                attempt,
                error,
                retryable,
                ..
            } => Some(CommandPayload::settle(
                EffectKind::ToolCall,
                id,
                attempt,
                Outcome::from(SettleError::new(error, retryable)),
            )),
            Action::LlmError {
                id,
                attempt,
                error,
                retryable,
            } => Some(CommandPayload::settle(
                EffectKind::LlmCall,
                id,
                attempt,
                Outcome::from(SettleError::new(error, retryable)),
            )),
            Action::SendMessage { .. }
            | Action::CloseBranch { .. }
            | Action::PromoteBranch { .. }
            | Action::Interrupt { .. }
            | Action::ResolveInterrupt { .. }
            | Action::SyncConnector { .. }
            | Action::Done { .. }
            | Action::Fail { .. }
            | Action::ReplaceContext { .. } => None,
        }
    }
}

struct Completion {
    index: usize,
    tool_call_id: String,
    name: String,
    result: String,
}

impl SessionState {
    fn stale_decision(&self, node: Option<&str>, trigger: &Trigger) -> bool {
        !self.at(node).anchor_on_path(self.trigger_anchor(trigger))
    }

    /// Voids the conversation's calls `stranded` picks. A detached branch's
    /// calls outlive the turn, and a blocking branch's end with the branch.
    pub fn void_effects(
        &self,
        stranded: impl Fn(EffectKind, &EffectTracking, Option<&str>) -> bool,
    ) -> Vec<EventPayload> {
        self.void_effects_where(|e| {
            e.branch.is_none() && stranded(e.kind(), &e.tracking, e.anchor.as_deref())
        })
    }

    pub fn void_effects_where(
        &self,
        stranded: impl Fn(&super::state::EffectState) -> bool,
    ) -> Vec<EventPayload> {
        self.effects
            .values()
            .filter(|e| {
                matches!(
                    e.kind(),
                    EffectKind::ToolCall
                        | EffectKind::LlmCall
                        | EffectKind::Subagent
                        | EffectKind::System1Call
                )
            })
            .filter(|e| stranded(e))
            .map(|e| {
                EventPayload::CallVoided(CallVoided {
                    kind: e.kind(),
                    id: e.id.clone(),
                })
            })
            .collect()
    }

    fn void_stranded_work(&self, node: Option<&str>) -> Vec<EventPayload> {
        let at = self.at(node);
        let mut events = self.void_effects(|kind, tracking, anchor| {
            crate::effects::stops(kind, tracking) && !at.anchor_on_path(anchor)
        });
        for e in self.effects_of(EffectKind::Decision) {
            let Some(wd) = e.decision() else { continue };
            if wd.branch.is_none()
                && matches!(
                    e.tracking.status(),
                    EffectStatus::Queued | EffectStatus::RetryScheduled
                )
                && self.stale_decision(node, &wd.trigger)
            {
                events.push(self.decision_dropped(&e.id));
            }
        }
        events.extend(self.void_stranded_branches(node));
        events
    }

    fn finish_turn_events(
        &self,
        ids: &Ids,
        data: serde_json::Value,
        caller: &Caller,
    ) -> Result<Vec<EventPayload>, SessionError> {
        SessionState::ensure_internal(caller)?;
        Ok(self.turn_finished_events(ids, data))
    }

    fn turn_finished_events(&self, ids: &Ids, data: serde_json::Value) -> Vec<EventPayload> {
        match &self.phase {
            TurnPhase::Active { turn_id } => vec![decision_queued(
                ids,
                Trigger::turn_finished(
                    turn_id.clone(),
                    data,
                    self.turn_cost,
                    self.turn_token_usage.clone(),
                ),
            )],
            TurnPhase::Idle | TurnPhase::Finalizing(_) => {
                vec![EventPayload::SessionDone(SessionDone {})]
            }
        }
    }

    /// Where a write on `branch` anchors: its own newest node. A branch with
    /// none of its own yet takes no write, so the path it forks keeps its own.
    fn write_anchor(&self, branch: Option<&str>) -> Result<Option<String>, ()> {
        match branch {
            None => Ok(self.head_id.clone()),
            Some(b) => match self.branches.get(b).and_then(|b| b.leaf.clone()) {
                Some(leaf) => Ok(Some(leaf)),
                None => {
                    tracing::warn!(
                        branch = b,
                        "a branch with no message of its own takes no write"
                    );
                    Err(())
                }
            },
        }
    }

    fn worker_state_events(
        &self,
        state: Option<WorkerState>,
        branch: Option<&str>,
    ) -> Vec<EventPayload> {
        let Some(state) = state else {
            return Vec::new();
        };
        if state == self.at_scope(branch).resolve_state_for() {
            return Vec::new();
        }
        let Ok(anchor) = self.write_anchor(branch) else {
            return Vec::new();
        };
        vec![EventPayload::WorkerStateUpdated(WorkerStateUpdated {
            state,
            anchor,
        })]
    }

    fn agent_config_events(
        &self,
        config: Option<AgentConfig>,
        branch: Option<&str>,
    ) -> Vec<EventPayload> {
        let Some(config) = config else {
            return Vec::new();
        };
        if Some(&config) == self.at_scope(branch).resolve_agent_for().as_ref() {
            return Vec::new();
        }
        let Ok(anchor) = self.write_anchor(branch) else {
            return Vec::new();
        };
        let mut events = effects::connector::sync(self, &config);
        events.push(EventPayload::AgentConfigUpdated(AgentConfigUpdated {
            config,
            anchor,
        }));
        events
    }
}

pub struct Working {
    state: SessionState,
    ids: Ids,
    seq: u64,
    now: DateTime<Utc>,
    plan_now: DateTime<Utc>,
    events: Vec<(EventPayload, EventMeta)>,
    waiting: Option<String>,
    joining: Option<String>,
    outside: Outside,
    /// The branch the decision being applied is about.
    scope: Option<String>,
}

impl std::ops::Deref for Working {
    type Target = SessionState;
    fn deref(&self) -> &SessionState {
        &self.state
    }
}

impl Working {
    pub fn new(state: SessionState, seq: u64, now: DateTime<Utc>) -> Self {
        let ids = Ids::new(&state.session_id, seq, now);
        Self {
            state,
            ids,
            seq,
            now,
            plan_now: now,
            events: Vec::new(),
            waiting: None,
            joining: None,
            outside: Outside::default(),
            scope: None,
        }
    }

    fn emit(&mut self, payload: EventPayload) {
        self.seq += 1;
        self.state.apply(
            &payload,
            &super::state::ApplyContext {
                occurred_at: self.now,
                sequence: self.seq,
            },
        );
        let meta = self.state.event_meta(self.now);
        self.events.push((payload, meta));
        #[cfg(test)]
        self.check_invariants();
    }

    #[cfg(test)]
    fn check_invariants(&self) {
        if let Err(violation) = self.state.check_invariants() {
            panic!("{violation}; events: {:?}", self.events);
        }
    }

    fn emit_all(&mut self, events: Vec<EventPayload>) {
        for event in events {
            self.emit(event);
        }
    }

    fn cx(&self) -> Cx<'_> {
        Cx::new(&self.state, &self.ids).scoped(self.scope.as_deref())
    }

    fn then(&mut self, step: impl FnOnce(&Cx<'_>) -> Vec<EventPayload>) -> &mut Self {
        let events = step(&self.cx());
        self.emit_all(events);
        self
    }

    fn try_then(
        &mut self,
        step: impl FnOnce(&Cx<'_>) -> Result<Vec<EventPayload>, SessionError>,
    ) -> Result<&mut Self, SessionError> {
        let events = step(&self.cx())?;
        self.emit_all(events);
        Ok(self)
    }

    pub fn with_outside(mut self, outside: Outside) -> Self {
        self.outside = outside;
        self
    }

    fn unload_off_path(&mut self) {
        if self.state.off_path() {
            self.emit(EventPayload::BranchUnloaded(BranchUnloaded {
                head_id: self.state.head_id.clone(),
            }));
        }
    }

    pub fn waiting_turn(&self) -> Option<&str> {
        self.waiting.as_deref()
    }

    /// The turn a joining message opens when the running turn ends first.
    pub fn joining_turn(&self) -> Option<&str> {
        let waiting = self.waiting_join()?;
        self.joining
            .as_deref()
            .filter(|t| *t == waiting.turn_id && self.phase.active_turn_id().is_some())
    }

    pub fn finish(self) -> (SessionState, Vec<(EventPayload, EventMeta)>) {
        (self.state, self.events)
    }

    fn schedule(&mut self) {
        let mut swept = false;
        loop {
            let Some(step) = schedule::plan(&self.state, self.plan_now)
                .into_iter()
                .find(|step| !(swept && step.is_sweep()))
            else {
                return;
            };
            swept |= step.is_sweep();
            self.execute(step);
        }
    }

    fn execute(&mut self, step: ScheduleStep) {
        match step {
            ScheduleStep::RequestFetch { connection_id } => {
                let config = self.state.retry_config();
                self.emit(EventPayload::ConnectorSyncRequested(
                    ConnectorSyncRequested {
                        path: connection_id,
                        attempt: 0,
                        retry: RetryPolicy::resolve(
                            None,
                            config.as_ref(),
                            RetryTarget::ConnectorSync,
                        ),
                    },
                ));
            }
            ScheduleStep::Dispatch { kind, id } => self.dispatch(kind, id),
            ScheduleStep::VoidPhantom { kind, id } => {
                self.then(|_| void_events(kind, id));
            }
            ScheduleStep::TimeOut { kind, id } => self.time_out(kind, id),
            ScheduleStep::Retry { kind, id } => {
                self.then(|s| kind.spec().retry(s, &id));
            }
            ScheduleStep::Resend { kind, id } => {
                self.then(|s| kind.spec().resend(s, &id));
            }
            ScheduleStep::EndQuietTurn => {
                self.then(|s| s.turn_finished_events(s.ids(), serde_json::Value::Null));
            }
            ScheduleStep::RedispatchDecision { decision_id } => {
                self.then(|s| EffectKind::Decision.spec().resend(s, &decision_id));
            }
        }
    }

    fn dispatch(&mut self, kind: EffectKind, id: String) {
        let spec = kind.spec();
        let on_branch = self
            .subagent(&id)
            .filter(|_| kind == EffectKind::Subagent)
            .is_some_and(|sa| sa.on_branch.is_some());
        self.then(|s| spec.dispatch(s, &id));
        if on_branch {
            self.start_subagent_branch(&id);
            return;
        }
        if let Some(trigger) = spec.execute_trigger(&self.state, &id) {
            let queued = decision_queued(&self.ids, trigger);
            self.emit(queued);
        }
        if kind == EffectKind::ToolCall {
            self.answer_locally(&id);
        }
    }

    fn answer_locally(&mut self, id: &str) {
        let Some(result) = self.state.local_connector_answer(id) else {
            return;
        };
        let outcome = match result.is_error {
            true => Outcome::Error(SettleError::new(
                ErrorInfo::new(crate::protocol::ErrorCode::HandlerError, result.rendered()),
                false,
            )),
            false => Outcome::Tool { result },
        };
        let spec = EffectKind::ToolCall.spec();
        self.then(|s| spec.settle(s, id, outcome));
    }

    fn time_out(&mut self, kind: EffectKind, id: String) {
        let spec = kind.spec();
        let now = self.plan_now;
        self.then(|s| {
            let total = s.tracking(kind, &id).is_some_and(|t| t.total_expired(now));
            spec.settle(s, &id, Outcome::Error(spec.timeout_error(total)))
        });
    }

    fn settle_effect(
        &mut self,
        kind: EffectKind,
        id: String,
        attempt: Option<u32>,
        outcome: Outcome,
        caller: &Caller,
    ) -> Result<(), SessionError> {
        let spec = kind.spec();
        self.try_then(|s| {
            spec.authorize(s, &id, caller)?;
            Ok(match spec.resolve(s, &id, attempt, caller)? {
                Settle::Live => spec.settle(s, &id, outcome),
                Settle::Drop => Vec::new(),
            })
        })
        .map(|_| ())
    }

    fn queue_decision(&mut self, trigger: Trigger) -> String {
        self.queue_decision_on(trigger, None)
    }

    fn queue_decision_on(&mut self, trigger: Trigger, branch: Option<String>) -> String {
        let decision_id = self.ids.next();
        self.emit(EventPayload::DecisionQueued(DecisionQueued {
            id: decision_id.clone(),
            trigger,
            branch,
        }));
        decision_id
    }

    fn raise_interrupt(
        &mut self,
        origin: InterruptOrigin,
        interrupt_id: String,
        reason: String,
        payload: serde_json::Value,
    ) {
        let scope = self.scope.clone();
        if self
            .at_scope(scope.as_deref())
            .active_interrupt_for()
            .is_some()
        {
            return;
        }
        let anchor = self.scope_node(scope.as_deref());
        self.emit(EventPayload::SessionInterrupted(SessionInterrupted {
            interrupt_id,
            origin,
            reason,
            payload,
            anchor: anchor.clone(),
        }));
        self.void_llm_calls_for_interrupt(anchor);
    }

    fn deferred_turn(
        &self,
        target: &TurnTarget,
        queue: QueueMode,
        payload: &ClientPayload,
    ) -> Result<Option<String>, SessionError> {
        let deferrable = queue.holds()
            && matches!(
                payload,
                ClientPayload::Message(_) | ClientPayload::Append(_)
            );
        let TurnTarget::Open(turn_id) = target else {
            return Ok(None);
        };
        if !deferrable || self.phase.turn_id().is_none() {
            return Ok(None);
        }
        self.turn_taken(turn_id)?;
        Ok(Some(turn_id.clone()))
    }

    fn action_turn(&self, turn: TurnTarget) -> Result<(TurnTarget, String), SessionError> {
        let own = match turn {
            TurnTarget::Open(id) | TurnTarget::Continue(id)
                if self.phase.active_turn_id() != Some(id.as_str()) =>
            {
                id
            }
            _ => self.ids.next(),
        };
        if matches!(self.phase, TurnPhase::Idle) {
            return Ok((TurnTarget::Open(own.clone()), own));
        }
        self.turn_taken(&own)?;
        Ok((TurnTarget::Detached, own))
    }

    fn turn_taken(&self, turn_id: &str) -> Result<(), SessionError> {
        if self.completed_turn_ids.iter().any(|t| t == turn_id) {
            return Err(SessionError::TurnAlreadyCompleted {
                turn_id: turn_id.to_string(),
            });
        }
        if self.phase.turn_id() == Some(turn_id) || self.queued_turn_ids().any(|t| t == turn_id) {
            return Err(SessionError::TurnAlreadyActive {
                turn_id: turn_id.to_string(),
            });
        }
        Ok(())
    }

    fn queued_turn_ids(&self) -> impl Iterator<Item = &str> {
        self.queued_decisions().into_iter().filter_map(|e| {
            let trigger = &e.decision()?.trigger;
            trigger
                .joining_turn_id()
                .unwrap_or(trigger.deferred_turn_id())
        })
    }

    fn begin_turn(
        &mut self,
        target: TurnTarget,
        metadata: BTreeMap<String, String>,
    ) -> Result<(), SessionError> {
        let turn_id = match target {
            TurnTarget::Detached => return Ok(()),
            TurnTarget::Continue(_) if matches!(self.phase, TurnPhase::Active { .. }) => {
                return Ok(())
            }
            TurnTarget::Continue(id) | TurnTarget::Open(id) => id,
        };
        self.turn_taken(&turn_id)?;
        match &self.phase {
            TurnPhase::Active { turn_id: active } => {
                return Err(SessionError::TurnAlreadyActive {
                    turn_id: active.clone(),
                })
            }
            TurnPhase::Finalizing(_) => {
                self.then(|s| s.finalize_run(None));
            }
            TurnPhase::Idle => {}
        }
        self.emit(EventPayload::TurnStarted(TurnStarted { turn_id, metadata }));
        Ok(())
    }

    fn void_llm_calls_for_interrupt(&mut self, node: Option<String>) {
        let at = self.at(node.as_deref());
        let scope = self.scope.clone();
        let ids: Vec<String> = self
            .effects_of(EffectKind::LlmCall)
            .filter(|e| {
                matches!(
                    e.tracking.status(),
                    EffectStatus::Queued | EffectStatus::Pending
                )
            })
            .filter(|e| e.branch == scope)
            .filter(|e| at.anchor_on_path(e.anchor.as_deref()))
            .map(|e| e.id.clone())
            .collect();
        for id in ids {
            self.emit(EventPayload::CallVoided(CallVoided {
                kind: EffectKind::LlmCall,
                id,
            }));
        }
    }

    fn system(&self) -> Caller {
        Caller::System {
            tenant_id: self
                .owner
                .as_ref()
                .map(|o| o.tenant_id.clone())
                .unwrap_or_default(),
        }
    }

    fn apply_actions(
        &mut self,
        actions: Vec<Action>,
        decision_id: &str,
        answered: Option<Trigger>,
    ) {
        let system = self.system();
        for action in actions {
            if let Action::SpawnSubagent { tool_call_id, .. } = &action {
                if self.script_ask(tool_call_id).is_some() {
                    let id = tool_call_id.clone();
                    self.then(|s| {
                        effects::tool::answer_ask(s, &id, effects::tool::AskAnswer::Spawn)
                    });
                }
            }
            let scope = self.scope.clone();
            let _ = match action {
                Action::CallLlm {
                    id,
                    llm,
                    request,
                    source: _,
                    stream,
                    retry,
                    handler,
                    format,
                    branch: Some(start),
                } => {
                    let messages = request.messages.clone();
                    match self.start_branch(start, messages) {
                        Some(branch) => self.run_on(
                            Some(branch),
                            CommandPayload::RequestLlmCall {
                                call_id: id,
                                llm,
                                request: LlmRequest {
                                    messages: Vec::new(),
                                    ..request
                                },
                                source: PromptSource::tree(),
                                stream,
                                retry,
                                handler,
                                format,
                            },
                            &system,
                        ),
                        None => Ok(()),
                    }
                }
                Action::CloseBranch { id } => {
                    self.then(|s| {
                        s.close_branch(&id, BranchEnd::Closed, serde_json::Value::Null, None)
                    });
                    Ok(())
                }
                Action::PromoteBranch { id } => {
                    self.promote_branch(&id);
                    Ok(())
                }
                Action::ReplaceContext { .. } if scope.is_some() => {
                    tracing::warn!("context.replace on a branch; ignored");
                    Ok(())
                }
                Action::Done { data } if scope.is_some() => {
                    let branch = scope.unwrap_or_default();
                    self.then(|s| s.finish_branch(s.ids(), &branch, data, None));
                    Ok(())
                }
                Action::Fail { error } if scope.is_some() => {
                    self.fail_branch(&scope.unwrap_or_default(), error);
                    Ok(())
                }
                Action::ReplaceContext { summary, messages } => {
                    self.replace_context(summary, messages);
                    Ok(())
                }
                Action::SendMessage {
                    session_id,
                    message,
                } => {
                    self.emit(EventPayload::SessionMessageRequested(
                        SessionMessageRequested {
                            target_session_id: session_id,
                            message,
                        },
                    ));
                    Ok(())
                }
                Action::Interrupt {
                    interrupt_id,
                    reason,
                    payload,
                } => {
                    self.raise_interrupt(InterruptOrigin::Frontend, interrupt_id, reason, payload);
                    Ok(())
                }
                Action::ResolveInterrupt {
                    interrupt_id,
                    payload,
                } => {
                    let allowed = self.open_interrupt(&interrupt_id).is_none_or(|open| {
                        InterruptOrigin::Machine.privilege() >= open.origin.privilege()
                    });
                    if !allowed {
                        tracing::warn!(
                            %interrupt_id,
                            "interrupt.resolve refused: the interrupt outranks the worker"
                        );
                        continue;
                    }
                    self.run_active(
                        CommandPayload::ResumeInterrupt {
                            interrupt_id,
                            payload,
                        },
                        &system,
                    )
                }
                Action::SyncConnector { path: id } => {
                    let named = self
                        .at_head()
                        .resolve_agent_for()
                        .is_some_and(|c| self.state.servers_for(&c).iter().any(|m| m.path == id));
                    let settled = self
                        .tracking(EffectKind::ConnectorSync, &id.to_string())
                        .is_some_and(|t| !t.is_in_flight());
                    if !named || !settled {
                        tracing::warn!(
                            connection = %id,
                            named,
                            settled,
                            "connector.sync refused"
                        );
                        continue;
                    }
                    let config = self.state.retry_config();
                    self.emit(EventPayload::ConnectorSyncRequested(
                        ConnectorSyncRequested {
                            path: id,
                            attempt: 0,
                            retry: RetryPolicy::resolve(
                                None,
                                config.as_ref(),
                                RetryTarget::ConnectorSync,
                            ),
                        },
                    ));
                    Ok(())
                }
                Action::Done { data } => {
                    self.then(|s| s.void_turn_branches());
                    self.then(|s| s.close_open_calls(s.ids(), answered.as_ref()));
                    let cmd = if self.phase.finalizing().is_some() {
                        CommandPayload::CompleteTurn
                    } else {
                        CommandPayload::FinishTurn { data }
                    };
                    self.run_active(cmd, &system)
                }
                Action::Fail { error } => {
                    self.then(|s| s.fail_run(&error));
                    Ok(())
                }
                Action::ToolError { id, error, .. } if self.script_ask(&id).is_some() => {
                    self.then(|s| {
                        effects::tool::answer_ask(
                            s,
                            &id,
                            effects::tool::AskAnswer::Refuse(error.message),
                        )
                    });
                    Ok(())
                }
                other => match other.into_command(decision_id) {
                    Some(cmd) => self.run_active(cmd, &system),
                    None => Ok(()),
                },
            };
        }
    }

    pub fn run(&mut self, cmd: CommandPayload, caller: &Caller) -> Result<(), SessionError> {
        match (self.agent_id.is_some(), cmd) {
            (
                false,
                CommandPayload::CreateSession {
                    agent_id,
                    owner,
                    ancestry,
                    worker_retry,
                    agent,
                    state,
                    worker,
                    events,
                },
            ) => {
                SessionState::ensure_tenant_matches(caller, &owner.tenant_id)?;
                if matches!(caller, Caller::Frontend { .. }) && !caller.owns(&owner) {
                    return Err(SessionError::SessionAccessDenied);
                }
                if agent.is_some()
                    || state.is_some()
                    || worker.as_ref().is_some_and(|w| w.url.is_some())
                {
                    SessionState::ensure_operator_or_system(caller)?;
                }
                self.emit(EventPayload::SessionCreated(Box::new(SessionCreated {
                    agent_id,
                    identity: owner,
                    ancestry,
                    worker_retry,
                    worker,
                    events,
                })));
                self.then(|s| s.agent_config_events(agent.map(|a| *a), None));
                self.then(|s| s.worker_state_events(state, None));
                self.queue_decision(Trigger::SessionStart);
            }
            (true, CommandPayload::CreateSession { .. }) => {
                return Err(SessionError::SessionAlreadyCreated)
            }
            (false, _) => return Err(SessionError::SessionNotCreated),
            (true, cmd) => self.run_active(cmd, caller)?,
        }
        self.schedule();
        self.unload_off_path();
        Ok(())
    }

    fn run_active(&mut self, cmd: CommandPayload, caller: &Caller) -> Result<(), SessionError> {
        if let Some(owner) = self.owner.as_ref() {
            SessionState::ensure_tenant_matches(caller, &owner.tenant_id)?;
        }
        match cmd {
            CommandPayload::CreateSession { .. } => Err(SessionError::SessionAlreadyCreated),

            CommandPayload::SubmitClientPayload {
                payload,
                turn,
                queue,
                metadata,
            } => {
                self.ensure_owns_session(caller)?;
                let Some(payload) = self.outside.drop_known(payload) else {
                    return Ok(());
                };
                let joins = queue == QueueMode::Join
                    && self.phase.turn_id().is_some()
                    && matches!(
                        payload,
                        ClientPayload::Message(_) | ClientPayload::Append(_)
                    );
                let (turn, own_turn) = match joins || matches!(payload, ClientPayload::Action(_)) {
                    true => {
                        let (turn, own) = self.action_turn(turn)?;
                        (turn, Some(own))
                    }
                    false => (turn, None),
                };
                let action_waits = self.phase.finalizing().is_some() && !joins;
                let deferred = self.deferred_turn(&turn, queue, &payload)?;
                if deferred.is_none() {
                    self.begin_turn(turn, metadata.clone())?;
                }
                let mode = match (joins, deferred.is_some()) {
                    (true, _) => QueueMode::Join,
                    (false, true) => queue,
                    (false, false) => QueueMode::Refuse,
                };
                let (events, turn) = {
                    let cx = self.cx();
                    cx.client_payload_events(
                        cx.ids(),
                        payload,
                        caller,
                        deferred.or(own_turn.clone()),
                        mode,
                        metadata,
                    )?
                };
                self.emit_all(events);
                match joins && self.phase.finalizing().is_none() {
                    true => self.joining = turn,
                    false => self.waiting = turn.or(own_turn.filter(|_| action_waits)),
                }
                Ok(())
            }

            CommandPayload::SendMessage {
                message,
                stream: _,
                turn_id,
                parent_id: _,
            } => {
                SessionState::ensure_internal(caller)?;
                self.run_active(
                    CommandPayload::SubmitClientPayload {
                        payload: ClientPayload::Message(ClientMessage {
                            message,
                            stream: false,
                        }),
                        turn: turn_id.map_or(TurnTarget::Detached, TurnTarget::Open),
                        queue: QueueMode::Join,
                        metadata: BTreeMap::new(),
                    },
                    caller,
                )
            }

            CommandPayload::RequestLlmCall {
                call_id,
                llm,
                mut request,
                source,
                stream,
                retry,
                handler,
                format,
            } => {
                SessionState::ensure_internal(caller)?;
                if source == PromptSource::Explicit && self.scope.is_none() {
                    let messages = std::mem::take(&mut request.messages);
                    let Some(branch) = self.explicit_branch(&call_id, messages) else {
                        return Ok(());
                    };
                    return self.run_on(
                        Some(branch),
                        CommandPayload::RequestLlmCall {
                            call_id,
                            llm,
                            request,
                            source: PromptSource::tree(),
                            stream,
                            retry,
                            handler,
                            format,
                        },
                        caller,
                    );
                }
                let source = self.held_source(&call_id, source);
                let held = HeldCall {
                    id: call_id,
                    llm,
                    request,
                    stream,
                };
                if self.scope.is_none() && self.held_for_room(&held, &source) {
                    return Ok(());
                }
                self.try_then(|s| {
                    effects::llm::request(
                        s,
                        held.id,
                        held.llm,
                        held.request,
                        source,
                        held.stream,
                        retry,
                        handler,
                        format,
                        caller,
                    )
                })
                .map(|_| ())
            }

            CommandPayload::SettleEffect {
                kind,
                id,
                attempt,
                outcome,
            } => self.settle_effect(kind, id, attempt, outcome, caller),

            CommandPayload::RequestSystem1Call {
                call_id,
                system1,
                request,
                retry,
            } => self
                .try_then(|s| {
                    effects::system1::request(s, call_id, system1, request, retry, caller)
                })
                .map(|_| ()),

            CommandPayload::RequestToolCall {
                tool_call_id,
                name,
                arguments,
                retry,
            } => self
                .try_then(|s| {
                    effects::tool::request(
                        s,
                        tool_call_id,
                        name,
                        arguments,
                        retry,
                        s.scope(),
                        caller,
                    )
                })
                .map(|_| ()),

            CommandPayload::RequestSubagent {
                session_id,
                agent_id,
                tool_call_id,
                message,
                retry,
                decision_id,
                mode,
                config,
            } => self
                .try_then(|s| {
                    effects::subagent::request(
                        s,
                        effects::subagent::Spawn {
                            tool_call_id,
                            agent_id,
                            session_id,
                            message,
                            retry,
                            decision_id,
                            mode,
                            config,
                        },
                        caller,
                    )
                })
                .map(|_| ()),

            CommandPayload::CompleteSubagentTurn {
                session_id,
                turn_id,
                data,
                cost,
                token_usage,
                error,
                ..
            } => {
                SessionState::ensure_internal(caller)?;
                if let Some((id, sa)) = self.state.subagent_awaiting(&session_id) {
                    let (id, agent_id) = (id.to_string(), sa.agent_id.clone());
                    return match error {
                        Some(error) => self.settle_effect(
                            EffectKind::Subagent,
                            id,
                            None,
                            SettleError::new(error, false).into(),
                            caller,
                        ),
                        None => {
                            let events = effects::subagent::complete_turn(
                                &self.ids,
                                id,
                                session_id,
                                agent_id,
                                data,
                                cost,
                                token_usage,
                            );
                            self.then(|_| events);
                            Ok(())
                        }
                    };
                }
                if self.state.detached_turns.contains(&turn_id) {
                    return Ok(());
                }
                let Some((id, sa)) = self.state.subagent_detached(&session_id) else {
                    return Ok(());
                };
                let (id, agent_id) = (id.to_string(), sa.agent_id.clone());
                let completed = SubagentTurnCompleted {
                    id,
                    cost,
                    token_usage,
                    data,
                    turn_id: Some(turn_id),
                    error,
                };
                self.then(|s| s.detached_answer(s.ids(), &session_id, &agent_id, completed));
                Ok(())
            }

            CommandPayload::Interrupt {
                interrupt_id,
                reason,
                payload,
            } => {
                self.ensure_owns_session(caller)?;
                let origin = SessionState::caller_interrupt_origin(caller);
                self.raise_interrupt(origin, interrupt_id, reason, payload);
                Ok(())
            }

            CommandPayload::ResumeInterrupt {
                interrupt_id,
                payload,
            } => {
                self.ensure_owns_session(caller)?;
                let Some(open) = self.open_interrupt(&interrupt_id) else {
                    return Ok(());
                };
                if SessionState::caller_interrupt_origin(caller).privilege()
                    < open.origin.privilege()
                {
                    return Err(SessionError::SessionAccessDenied);
                }
                let parked_head = self.at_head().anchor_on_path(open.anchor.as_deref());
                let branch = open
                    .anchor
                    .as_deref()
                    .and_then(|a| self.node_branch(a))
                    .filter(|b| self.open_branch(b).is_some())
                    .map(str::to_string);
                self.emit(EventPayload::InterruptResumed(InterruptResumed {
                    interrupt_id: interrupt_id.clone(),
                    payload: payload.clone(),
                }));
                let trigger = Trigger::InterruptResumed {
                    interrupt_id,
                    payload,
                };
                match branch {
                    Some(b) => {
                        self.queue_decision_on(trigger, Some(b));
                    }
                    None if parked_head => {
                        self.queue_decision(trigger);
                    }
                    None => {}
                }
                Ok(())
            }

            CommandPayload::SubmitWorkerDecision {
                decision_id,
                transcript,
                actions,
                state,
                agent,
            } => {
                SessionState::ensure_worker_or_system(caller)?;
                match self
                    .tracking(EffectKind::Decision, &decision_id)
                    .map(|t| t.status())
                {
                    Some(EffectStatus::Pending) => {}
                    _ => return Ok(()),
                }
                let answered = self
                    .worker_decision(&decision_id)
                    .map(|d| d.trigger.clone());
                if let Some(branch) = self.decision_branch(&decision_id).map(str::to_string) {
                    let completed = EventPayload::DecisionCompleted(DecisionCompleted {
                        id: decision_id.clone(),
                    });
                    if self.open_branch(&branch).is_none() {
                        self.emit(completed);
                        return Ok(());
                    }
                    let on = Some(branch.as_str());
                    self.then(|_| vec![completed])
                        .then(|s| s.reconcile_branch(s.ids(), transcript, &branch))
                        .then(|s| s.worker_state_events(state, on))
                        .then(|s| s.agent_config_events(agent, on));
                    let outer = self.scope.replace(branch.clone());
                    self.apply_actions(actions, &decision_id, answered);
                    self.scope = outer;
                    self.then(|s| effects::tool::unanswered(s));
                    self.then(|s| s.quiet_branch(s.ids(), &branch));
                    return Ok(());
                }
                let ids: Vec<&str> = transcript.iter().filter_map(|m| m.id.as_deref()).collect();
                if let Some(loaded) = self.outside.branch_to(&ids, &self.state) {
                    self.emit(EventPayload::BranchLoaded(BranchLoaded {
                        seqs: loaded.seqs(),
                    }));
                    self.state.load_branch(&loaded);
                }
                self.then(|_| {
                    vec![EventPayload::DecisionCompleted(DecisionCompleted {
                        id: decision_id.clone(),
                    })]
                })
                .then(|s| s.reconcile_transcript(s.ids(), transcript).0)
                .then(|s| s.void_stranded_work(s.head_id.as_deref()))
                .then(|s| s.worker_state_events(state, None))
                .then(|s| s.agent_config_events(agent, None));

                self.apply_actions(actions, &decision_id, answered);
                self.then(|s| effects::tool::unanswered(s));
                Ok(())
            }

            CommandPayload::CancelSession { drop_queued } => {
                self.ensure_owns_session(caller)?;
                if !self.cancellable() {
                    return Ok(());
                }
                self.emit(EventPayload::SessionCancelled);
                self.then(|s| s.close_open_calls(s.ids(), None))
                    .then(|s| s.cancel_run(drop_queued));
                Ok(())
            }

            CommandPayload::FinishTurn { data } => self
                .try_then(|s| s.finish_turn_events(s.ids(), data, caller))
                .map(|_| ()),

            CommandPayload::CompleteTurn => self
                .try_then(|s| {
                    SessionState::ensure_internal(caller)?;
                    Ok(s.finalize_run(None))
                })
                .map(|_| ()),

            CommandPayload::Wake { now } => {
                SessionState::ensure_internal(caller)?;
                self.run_wake(now);
                Ok(())
            }
            CommandPayload::DeliveryFailed(undelivered) => {
                SessionState::ensure_internal(caller)?;
                self.delivery_failed(undelivered)
            }
            CommandPayload::WorkUnsent { unsent } => {
                SessionState::ensure_internal(caller)?;
                self.work_unsent(unsent);
                Ok(())
            }
            CommandPayload::DeliveryRecovered {
                subscription,
                through,
            } => {
                SessionState::ensure_internal(caller)?;
                self.delivery_recovered(subscription, through);
                Ok(())
            }
            CommandPayload::ResumeDelivery { subscription } => {
                SessionState::ensure_operator_or_system(caller)?;
                self.resume_delivery(subscription);
                Ok(())
            }
        }
    }

    fn run_wake(&mut self, now: DateTime<Utc>) {
        self.plan_now = now;
    }
}

mod authorize;
pub mod branch;
mod context;
mod delivery;
mod transcript;

#[cfg(test)]
mod tests;

#[cfg(test)]
mod props;

#[cfg(test)]
mod traces;
