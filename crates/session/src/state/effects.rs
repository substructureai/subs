use super::*;

impl SessionState {
    pub(super) fn release(&mut self) {
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
        let ended: Vec<BranchId> = self
            .branches
            .iter()
            .filter(|(id, open)| !id.is_main() && open.run.is_none() && !self.names(id))
            .map(|(id, _)| id.clone())
            .collect();
        for id in ended {
            self.branches.remove(&id);
        }
    }

    pub(super) fn answered(&mut self, message: &Message, branch: &BranchId) {
        let Some(answer) = message.tool_call_id.as_deref() else {
            return;
        };
        for e in self.effects.values_mut().filter(|e| e.branch == *branch) {
            match &mut e.payload {
                EffectPayload::LlmCall(call) => call.unanswered.retain(|id| id != answer),
                EffectPayload::ToolCall(tool) if e.id == answer => tool.recorded = true,
                _ => {}
            }
        }
        self.release();
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
            .and_then(|e| e.asked_at.as_deref());
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

    pub fn worker_decision(&self, id: &str) -> Option<&WorkerDecisionState> {
        self.effect(EffectKind::Decision, id)
            .and_then(|e| e.decision())
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
                    asked_at: e.asked_at.clone(),
                    name: None,
                    arguments: None,
                    handler: None,
                    stream: None,
                    agent_id: None,
                    session_id: None,
                    branch: e.branch.clone().into(),
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
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EffectTracking {
    pub(super) status: EffectStatus,
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
    pub asked_at: Option<String>,
    #[serde(default, skip_serializing_if = "BranchId::is_main")]
    pub branch: BranchId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn: Option<String>,
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

payload_views! {
    llm, llm_mut => LlmCall(LlmCallState),
    tool, tool_mut => ToolCall(ToolCallState),
    subagent, subagent_mut => Subagent(SubagentCallState),
    connector, connector_mut => ConnectorSync(ConnectorSyncState),
    decision, decision_mut => Decision(WorkerDecisionState),
    system1, system1_mut => System1Call(System1CallState),
}

impl EffectState {
    pub fn new(id: impl Into<String>, tracking: EffectTracking, payload: EffectPayload) -> Self {
        Self {
            id: id.into(),
            tracking,
            asked_at: None,
            branch: BranchId::MAIN,
            turn: None,
            payload,
        }
    }

    pub fn at(mut self, anchor: Option<&str>) -> Self {
        self.asked_at = anchor.map(str::to_string);
        self
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LlmCallState {
    pub prompt: Vec<Message>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dispatched_seq: Option<u64>,
    pub llm: String,
    pub spec: LlmCallSpec,
    pub stream: bool,
    pub handler: LlmHandler,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub format: Option<LlmFormat>,
    pub defer_tools_strategy: DeferToolsStrategy,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reads_from: Option<String>,
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
    pub handler: ToolHandler,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<ConnectorTarget>,
    pub arguments: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<String>,
    pub is_error: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub script: Option<ScriptCaller>,
    /// For `run_code`: the language its script is in.
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
    pub session_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<DraftMessage>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<String>,
    pub is_error: bool,
    pub mode: SpawnMode,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct System1CallState {
    pub system1: String,
    pub request: System1Request,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConnectorSyncState {
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

pub(super) fn trigger_effect(trigger: &Trigger) -> Option<(EffectKind, &str)> {
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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkerDecisionState {
    pub trigger: Trigger,
    #[serde(default, skip_serializing_if = "BranchId::is_main")]
    pub branch: BranchId,
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
            asked_at: Some("n1".to_string()),
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
