use std::collections::HashMap;
use std::sync::Arc;

use chrono::{DateTime, Duration, Utc};
use proptest::prelude::*;

use crate::command::{CommandPayload, Outcome, SettleError, TurnTarget};
use crate::decision::{Action, LlmHandler};
use crate::events::EventPayload;
use crate::protocol::{
    AgentConfig, ClientMessage, ClientPayload, Content, DraftMessage, EffectStatus, ErrorInfo,
    Issuer, LlmRequest, LlmResponse, McpServer, QueueMode, Requester, RetryPolicy, Role,
    SessionOwner, StoredResult, Subject,
};
use crate::span::SpanContext;
use crate::state::{ApplyContext, EffectKind, SessionState};
use crate::{Caller, CommitContext, Session};

pub const TENANT: &str = "tenant-a";
pub const SESSION: &str = "sess-1";
const USER: &str = "user-1";

fn system() -> Caller {
    Caller::System {
        tenant_id: TENANT.to_string(),
    }
}

fn frontend() -> Caller {
    Caller::Frontend {
        tenant_id: TENANT.to_string(),
        subject: Subject::new(Issuer::app(), USER.to_string()),
        attrs: HashMap::new(),
    }
}

fn policy() -> RetryPolicy {
    RetryPolicy {
        queue_timeout_secs: None,
        run_timeout_secs: Some(5),
        total_timeout_secs: None,
        max_attempts: 2,
        backoff_base_secs: 1,
        backoff_max_secs: 1,
    }
}

#[derive(Debug, Clone)]
pub struct Fork {
    index: usize,
    new: bool,
    detached: bool,
    agent: u8,
}

#[derive(Debug, Clone)]
pub enum ActOp {
    CallLlm { worker: bool, fork: Option<Fork> },
    CallWith { fork: Option<Fork> },
    CallSystem1 { given: bool },
    CloseBranch { index: usize },
    PromoteBranch { index: usize },
    ReplaceContext,
    CallTool,
    SpawnSubagent,
    Interrupt,
    Done,
}

#[derive(Debug, Clone)]
pub enum Op {
    ClientMessage {
        queue: QueueMode,
    },
    Decide {
        acts: Vec<ActOp>,
        append: bool,
        keep: Option<usize>,
        set_config: bool,
    },
    FailDecision {
        retryable: bool,
    },
    SettleLlm {
        index: usize,
        ok: bool,
        retryable: bool,
    },
    SettleTool {
        index: usize,
        ok: bool,
        retryable: bool,
    },
    SettleSubagent {
        index: usize,
        ok: bool,
        retryable: bool,
    },
    SettleConnector {
        index: usize,
        ok: bool,
        retryable: bool,
    },
    Interrupt,
    Resume {
        index: usize,
    },
    View {
        keep: usize,
        add: bool,
        wait: bool,
    },
    Switch {
        pick: usize,
    },
    SettleSystem1 {
        index: usize,
        ok: bool,
    },
    Repeat,
    Wake {
        advance_secs: u32,
    },
    Cancel,
}

fn fork() -> impl Strategy<Value = Option<Fork>> {
    proptest::option::of(
        (any::<usize>(), any::<bool>(), any::<bool>(), 0u8..3).prop_map(
            |(index, new, detached, agent)| Fork {
                index,
                new,
                detached,
                agent,
            },
        ),
    )
}

fn act_op() -> impl Strategy<Value = ActOp> {
    prop_oneof![
        3 => (any::<bool>(), fork()).prop_map(|(worker, fork)| ActOp::CallLlm { worker, fork }),
        1 => fork().prop_map(|fork| ActOp::CallWith { fork }),
        1 => any::<bool>().prop_map(|given| ActOp::CallSystem1 { given }),
        1 => any::<usize>().prop_map(|index| ActOp::CloseBranch { index }),
        2 => any::<usize>().prop_map(|index| ActOp::PromoteBranch { index }),
        1 => Just(ActOp::ReplaceContext),
        1 => Just(ActOp::CallTool),
        1 => Just(ActOp::SpawnSubagent),
        1 => Just(ActOp::Interrupt),
        1 => Just(ActOp::Done),
    ]
}

fn op() -> impl Strategy<Value = Op> {
    prop_oneof![
        4 => (
            proptest::collection::vec(act_op(), 0..3),
            any::<bool>(),
            proptest::option::weighted(0.5, any::<usize>()),
            any::<bool>(),
        )
            .prop_map(|(acts, append, keep, set_config)| Op::Decide {
                acts,
                append,
                keep,
                set_config,
            }),
        3 => prop_oneof![
            Just(QueueMode::Refuse),
            Just(QueueMode::Turn),
            Just(QueueMode::Merge),
            Just(QueueMode::Join),
        ]
        .prop_map(|queue| Op::ClientMessage { queue }),
        2 => (any::<usize>(), any::<bool>(), any::<bool>())
            .prop_map(|(index, ok, retryable)| Op::SettleLlm { index, ok, retryable }),
        3 => (any::<usize>(), any::<bool>(), any::<bool>())
            .prop_map(|(index, ok, retryable)| Op::SettleTool { index, ok, retryable }),
        2 => (any::<usize>(), any::<bool>(), any::<bool>())
            .prop_map(|(index, ok, retryable)| Op::SettleSubagent { index, ok, retryable }),
        2 => (any::<usize>(), any::<bool>(), any::<bool>())
            .prop_map(|(index, ok, retryable)| Op::SettleConnector { index, ok, retryable }),
        1 => any::<bool>().prop_map(|retryable| Op::FailDecision { retryable }),
        1 => Just(Op::Interrupt),
        1 => any::<usize>().prop_map(|index| Op::Resume { index }),
        3 => (any::<usize>(), any::<bool>(), any::<bool>())
            .prop_map(|(keep, add, wait)| Op::View { keep, add, wait }),
        2 => any::<usize>().prop_map(|pick| Op::Switch { pick }),
        3 => (any::<usize>(), any::<bool>()).prop_map(|(index, ok)| Op::SettleSystem1 { index, ok }),
        1 => Just(Op::Repeat),
        2 => (0u32..12).prop_map(|advance_secs| Op::Wake { advance_secs }),
        1 => Just(Op::Cancel),
    ]
}

pub(crate) struct World {
    pub(crate) agg: Session,
    store: Option<Counted>,
    branch_ids: Vec<String>,
    last: Option<(crate::Command, crate::CommandResult)>,
    now: DateTime<Utc>,
    pub(crate) log: Vec<(u64, DateTime<Utc>, EventPayload)>,
    next_id: u32,
    pub(crate) trace: Vec<String>,
    pub(crate) plan_now: DateTime<Utc>,
    pub(crate) cancelled: bool,
}

impl World {
    #[cfg(test)]
    pub(crate) fn new() -> Self {
        World::with(None)
    }

    pub(crate) fn stored(store: Arc<dyn crate::Store + Send + Sync>) -> Self {
        World::with(Some(Counted {
            store,
            reads: Default::default(),
        }))
    }

    fn with(store: Option<Counted>) -> Self {
        let agg = match &store {
            Some(store) => Session::load(store, SESSION, TENANT, settings()).unwrap(),
            None => Session::with_state(
                SESSION.to_string(),
                TENANT.to_string(),
                SessionState::new(SESSION.to_string()),
            ),
        };
        let mut world = World {
            agg,
            store,
            branch_ids: Vec::new(),
            last: None,
            now: DateTime::parse_from_rfc3339("2026-01-01T00:00:00Z")
                .unwrap()
                .with_timezone(&Utc),
            log: Vec::new(),
            next_id: 0,
            trace: Vec::new(),
            plan_now: DateTime::parse_from_rfc3339("2026-01-01T00:00:00Z")
                .unwrap()
                .with_timezone(&Utc),
            cancelled: false,
        };
        world.run(
            CommandPayload::CreateSession {
                agent_id: "agent-1".to_string(),
                owner: SessionOwner {
                    tenant_id: TENANT.to_string(),
                    requester: Requester::new(
                        Subject::new(Issuer::app(), USER.to_string()),
                        Default::default(),
                    ),
                    metadata: HashMap::new(),
                },
                ancestry: vec![],
                worker_retry: policy(),
                agent: None,
                worker: None,
                events: None,
                state: None,
            },
            &system(),
        );
        world
    }

    fn mint(&mut self) -> String {
        self.next_id += 1;
        format!("e-{}", self.next_id)
    }

    fn run(&mut self, cmd: CommandPayload, caller: &Caller) {
        self.now += Duration::milliseconds(1);
        self.plan_now = match &cmd {
            CommandPayload::Wake { now } => *now,
            _ => self.now,
        };
        self.trace.push(format!("{cmd:?}"));
        if let Some(store) = self.store.clone() {
            let id = format!("cmd-{}", self.trace.len());
            let command = crate::Command::new(id, caller.clone(), cmd);
            let ran = self.agg.run(&store, self.now, command.clone()).unwrap();
            self.last = Some((command, ran.result.clone()));
            for event in ran.written.map(|e| e.events).unwrap_or_default() {
                self.log.push((event.seq, event.occurred_at, event.payload));
            }
            return;
        }
        let Ok(events) = self.agg.handle(cmd, caller, self.now) else {
            return;
        };
        let committed = self.agg.commit(
            events,
            &CommitContext {
                span: SpanContext::root(),
                occurred_at: self.now,
            },
        );
        for event in committed {
            self.log.push((event.seq, event.occurred_at, event.payload));
        }
    }

    fn unfingerprinted(&self) -> Option<String> {
        let mut state = SessionState::new(SESSION.to_string());
        for (seq, occurred_at, payload) in &self.log {
            if let EventPayload::LlmCallDispatched(d) = payload {
                let built = state
                    .outgoing(&d.id)
                    .map(|out| crate::prompt_context::fingerprint(&out.prompt));
                if d.fingerprint.is_some() && d.fingerprint != built {
                    return Some(d.id.clone());
                }
            }
            state.apply(
                payload,
                &ApplyContext {
                    occurred_at: *occurred_at,
                    sequence: *seq,
                },
            );
        }
        None
    }

    fn unprefixed(&self) -> Option<(String, String)> {
        let bare = |prompt: &[crate::protocol::Message]| -> Vec<serde_json::Value> {
            prompt
                .iter()
                .map(|m| {
                    let mut m = DraftMessage::from(m.clone());
                    m.id = None;
                    serde_json::to_value(m).unwrap()
                })
                .collect()
        };
        let mut state = SessionState::new(SESSION.to_string());
        let mut sent: HashMap<String, Vec<serde_json::Value>> = HashMap::new();
        for (seq, occurred_at, payload) in &self.log {
            if let EventPayload::LlmCallDispatched(d) = payload {
                if let Some(out) = state.outgoing(&d.id) {
                    let prompt = bare(&out.prompt);
                    for node in state.tree.up(out.node.as_deref()) {
                        match &node.fact {
                            crate::tree::Fact::Config { .. } => break,
                            crate::tree::Fact::Context { call, .. }
                                if sent
                                    .get(call)
                                    .is_some_and(|before| !prompt.starts_with(before)) =>
                            {
                                return Some((call.clone(), d.id.clone()));
                            }
                            _ => {}
                        }
                    }
                    sent.insert(d.id.clone(), prompt);
                }
            }
            state.apply(
                payload,
                &ApplyContext {
                    occurred_at: *occurred_at,
                    sequence: *seq,
                },
            );
        }
        None
    }

    #[cfg(test)]
    pub(crate) fn replayed(&self) -> SessionState {
        let mut state = SessionState::new(SESSION.to_string());
        for (seq, occurred_at, payload) in &self.log {
            state.apply(
                payload,
                &ApplyContext {
                    occurred_at: *occurred_at,
                    sequence: *seq,
                },
            );
        }
        state
    }

    fn pending<'a>(&self, ids: impl Iterator<Item = &'a str>) -> Vec<String> {
        let mut ids: Vec<String> = ids.map(str::to_string).collect();
        ids.sort();
        ids
    }

    fn pending_llm(&self) -> Vec<String> {
        self.pending(
            self.agg
                .state
                .effects_of(EffectKind::LlmCall)
                .filter(|c| c.tracking.status() == EffectStatus::Pending)
                .map(|c| c.id.as_str()),
        )
    }

    fn pending_tool(&self) -> Vec<String> {
        self.pending(
            self.agg
                .state
                .effects_of(EffectKind::ToolCall)
                .filter(|c| c.tracking.status() == EffectStatus::Pending)
                .map(|c| c.id.as_str()),
        )
    }

    fn pending_subagent(&self) -> Vec<String> {
        self.pending(
            self.agg
                .state
                .effects_of(EffectKind::Subagent)
                .filter(|c| c.tracking.status() == EffectStatus::Pending)
                .map(|c| c.id.as_str()),
        )
    }

    fn pending_connector(&self) -> Vec<String> {
        self.pending(
            self.agg
                .state
                .effects_of(EffectKind::ConnectorSync)
                .filter(|c| c.tracking.status() == EffectStatus::Pending)
                .map(|c| c.id.as_str()),
        )
    }

    fn live_decision(&self) -> Option<String> {
        let mut ids = self.pending(
            self.agg
                .state
                .effects_of(EffectKind::Decision)
                .filter(|d| d.tracking.status() == EffectStatus::Pending)
                .map(|d| d.id.as_str()),
        );
        ids.pop()
    }

    fn recorded(&self) -> Vec<DraftMessage> {
        let tree = self.agg.state.message_tree();
        match tree.head_id.as_deref() {
            Some(head) => tree
                .path_to(head)
                .into_iter()
                .map(DraftMessage::from)
                .collect(),
            None => Vec::new(),
        }
    }

    fn message(&mut self, role: Role) -> DraftMessage {
        let text = self.mint();
        DraftMessage {
            id: None,
            role,
            content: Some(Content::Text(text)),
            tool_calls: None,
            tool_call_id: None,
            name: None,
            reasoning: None,
        }
    }

    fn view(&mut self, messages: Vec<DraftMessage>, wait: bool) {
        let turn_id = self.mint();
        self.run(
            CommandPayload::SubmitClientPayload {
                payload: ClientPayload::Messages(crate::protocol::ClientMessages {
                    messages,
                    stream: false,
                    client: Default::default(),
                }),
                turn: TurnTarget::Open(turn_id),
                queue: match wait {
                    true => QueueMode::Turn,
                    false => QueueMode::Refuse,
                },
                metadata: Default::default(),
            },
            &frontend(),
        );
    }

    fn promotable(&self) -> Vec<String> {
        let state = &self.agg.state;
        let mut ids: Vec<String> =
            self.branch_ids
                .iter()
                .filter(|id| {
                    let leaf = match state.open_branch(id) {
                        Some(open) => open.leaf.clone(),
                        None => state.tree.branch(id).and_then(|r| r.state.leaf),
                    };
                    let own = leaf
                        .and_then(|leaf| state.tree.get(&leaf))
                        .is_some_and(|n| n.branch() == Some(id.as_str()));
                    let busy = state.effects.values().any(|e| {
                        e.branch.as_deref() == Some(id.as_str()) && e.tracking.is_unsettled()
                    });
                    own && !busy
                })
                .cloned()
                .collect();
        ids.sort();
        ids
    }

    fn branch_id(&mut self, index: usize, new: bool) -> String {
        let mut known: Vec<String> = self.agg.state.branches.keys().cloned().collect();
        known.extend(self.branch_ids.iter().cloned());
        known.sort();
        known.dedup();
        match pick(&known, index).filter(|_| !new) {
            Some(id) => id,
            None => {
                let id = format!("b-{}", self.mint());
                self.branch_ids.push(id.clone());
                id
            }
        }
    }

    fn llm(
        &mut self,
        worker: bool,
        messages: Vec<DraftMessage>,
        branch: Option<crate::decision::BranchStart>,
    ) -> Action {
        Action::CallLlm {
            llm: "claude".to_string(),
            format: None,
            id: self.mint(),
            request: LlmRequest {
                model: "test-model".to_string(),
                messages,
                tools: None,
                temperature: None,
                max_completion_tokens: None,
                reasoning: None,
            },
            stream: false,
            retry: policy(),
            handler: if worker {
                LlmHandler::Worker
            } else {
                LlmHandler::Server
            },
            branch,
        }
    }

    fn start(&mut self, f: Fork) -> crate::decision::BranchStart {
        crate::decision::BranchStart {
            id: self.branch_id(f.index, f.new),
            from: crate::decision::BranchFrom::Head,
            agent: match f.agent {
                0 => crate::decision::BranchConfig::Inherit,
                1 => crate::decision::BranchConfig::None,
                _ => crate::decision::BranchConfig::Config(Box::new(self.config())),
            },
            name: None,
            mode: match f.detached {
                true => crate::protocol::BranchMode::Detached,
                false => crate::protocol::BranchMode::Blocking,
            },
        }
    }

    fn actions(&mut self, acts: Vec<ActOp>) -> Vec<Action> {
        acts.into_iter()
            .map(|a| match a {
                ActOp::CallLlm { worker, fork } => {
                    let branch = fork.map(|f| self.start(f));
                    self.llm(worker, Vec::new(), branch)
                }
                ActOp::CallWith { fork } => {
                    let mut messages = self.recorded();
                    messages.push(self.message(Role::User));
                    let branch = fork.map(|f| self.start(f));
                    self.llm(false, messages, branch)
                }
                ActOp::CallSystem1 { given } => Action::CallSystem1 {
                    id: self.mint(),
                    system1: "cf".to_string(),
                    request: crate::system1::System1Request {
                        model: "clef".to_string(),
                        state: given.then(|| serde_json::json!({ "given": true })),
                        questions: std::collections::BTreeMap::from([(
                            "spam".to_string(),
                            crate::system1::Question::Noul {
                                instructions: Some(serde_json::json!("Is this spam?")),
                                criteria: None,
                            },
                        )]),
                        assets: Default::default(),
                    },
                    retry: policy(),
                },
                ActOp::CloseBranch { index } => Action::CloseBranch {
                    id: self.branch_id(index, false),
                },
                ActOp::PromoteBranch { index } => Action::PromoteBranch {
                    id: match pick(&self.promotable(), index) {
                        Some(id) => id,
                        None => self.branch_id(index, false),
                    },
                },
                ActOp::ReplaceContext => Action::ReplaceContext {
                    summary: Some(self.mint()),
                    messages: Vec::new(),
                },
                ActOp::CallTool => Action::CallTool {
                    id: self.mint(),
                    name: "a_tool".to_string(),
                    arguments: "{}".to_string(),
                    retry: Some(policy().as_override()),
                },
                ActOp::SpawnSubagent => Action::SpawnSubagent {
                    session_id: None,
                    agent_id: "child".to_string(),
                    tool_call_id: self.mint(),
                    message: None,
                    retry: policy(),
                    mode: None,
                    config: None,
                },
                ActOp::Interrupt => Action::Interrupt {
                    interrupt_id: self.mint(),
                    reason: "pause".to_string(),
                    payload: serde_json::Value::Null,
                },
                ActOp::Done => Action::Done {
                    data: serde_json::Value::Null,
                },
            })
            .collect()
    }

    fn config(&mut self) -> AgentConfig {
        let model = self.mint();
        AgentConfig {
            llm: Some("claude".to_string()),
            system: Some(format!("system {model}")),
            model,
            mcp: vec![McpServer {
                id: "conn-1".into(),
                tools: None,
                auth_failure: Default::default(),
                tool_sync_failure: Default::default(),
                approve: Default::default(),
            }],
            ..Default::default()
        }
    }

    pub(crate) fn step(&mut self, op: Op) {
        match op {
            Op::ClientMessage { queue } => {
                let message = self.message(Role::User);
                let turn_id = self.mint();
                self.run(
                    CommandPayload::SubmitClientPayload {
                        payload: ClientPayload::Message(ClientMessage {
                            message,
                            stream: false,
                        }),
                        turn: TurnTarget::Open(turn_id),
                        queue,
                        metadata: Default::default(),
                    },
                    &frontend(),
                );
            }
            Op::Decide {
                acts,
                append,
                keep,
                set_config,
            } => {
                let Some(decision_id) = self.live_decision() else {
                    return;
                };
                let transcript = if append {
                    let mut t = self.recorded();
                    if let Some(keep) = keep {
                        t.truncate(keep % t.len().max(1));
                    }
                    let msg = self.message(Role::Assistant);
                    t.push(msg);
                    t
                } else {
                    Vec::new()
                };
                let actions = self.actions(acts);
                let agent = set_config.then(|| self.config());
                self.run(
                    CommandPayload::SubmitWorkerDecision {
                        decision_id,
                        transcript,
                        actions,
                        state: None,
                        agent,
                    },
                    &system(),
                );
            }
            Op::FailDecision { retryable } => {
                let Some(decision_id) = self.live_decision() else {
                    return;
                };
                self.run(
                    CommandPayload::settle(
                        EffectKind::Decision,
                        decision_id,
                        None,
                        SettleError::new(ErrorInfo::internal("boom"), retryable),
                    ),
                    &system(),
                );
            }
            Op::SettleLlm {
                index,
                ok,
                retryable,
            } => {
                let ids = self.pending_llm();
                let Some(call_id) = pick(&ids, index) else {
                    return;
                };
                let cmd = if ok {
                    CommandPayload::settle(
                        EffectKind::LlmCall,
                        call_id,
                        None,
                        Outcome::Llm(Box::new(LlmResponse {
                            model: "test-model".to_string(),
                            content: Some("ok".to_string()),
                            tool_calls: vec![],
                            finish_reason: None,
                            usage: None,
                            cost: None,
                            images: vec![],
                            reasoning: None,
                        })),
                    )
                } else {
                    CommandPayload::settle(
                        EffectKind::LlmCall,
                        call_id,
                        None,
                        SettleError::new(ErrorInfo::internal("boom"), retryable),
                    )
                };
                self.run(cmd, &system());
            }
            Op::SettleTool {
                index,
                ok,
                retryable,
            } => {
                let ids = self.pending_tool();
                let Some(tool_call_id) = pick(&ids, index) else {
                    return;
                };
                let cmd = if ok {
                    CommandPayload::settle(
                        EffectKind::ToolCall,
                        tool_call_id,
                        None,
                        Outcome::Tool {
                            result: StoredResult::text("ok".to_string()),
                        },
                    )
                } else {
                    CommandPayload::settle(
                        EffectKind::ToolCall,
                        tool_call_id,
                        None,
                        SettleError::new(ErrorInfo::internal("boom"), retryable),
                    )
                };
                self.run(cmd, &system());
            }
            Op::SettleSubagent {
                index,
                ok,
                retryable,
            } => {
                let ids = self.pending_subagent();
                let Some(session_id) = pick(&ids, index) else {
                    return;
                };
                let cmd = if ok {
                    CommandPayload::CompleteSubagentTurn {
                        session_id,
                        agent_id: "child".to_string(),
                        turn_id: "t".to_string(),
                        data: serde_json::json!("done"),
                        cost: Default::default(),
                        token_usage: Default::default(),
                        error: None,
                    }
                } else {
                    CommandPayload::settle(
                        EffectKind::Subagent,
                        session_id,
                        None,
                        SettleError::new(ErrorInfo::internal("boom"), retryable),
                    )
                };
                self.run(cmd, &system());
            }
            Op::SettleConnector {
                index,
                ok,
                retryable,
            } => {
                let ids = self.pending_connector();
                let Some(connection_id) = pick(&ids, index) else {
                    return;
                };
                let cmd = if ok {
                    CommandPayload::settle(
                        EffectKind::ConnectorSync,
                        connection_id,
                        None,
                        Outcome::Connector {
                            server: None,
                            prefix: None,
                            tools: vec![],
                            instructions: None,
                        },
                    )
                } else {
                    CommandPayload::settle(
                        EffectKind::ConnectorSync,
                        connection_id,
                        None,
                        SettleError::new(ErrorInfo::internal("boom"), retryable),
                    )
                };
                self.run(cmd, &system());
            }
            Op::Interrupt => {
                let interrupt_id = self.mint();
                self.run(
                    CommandPayload::Interrupt {
                        interrupt_id,
                        reason: "pause".to_string(),
                        payload: serde_json::Value::Null,
                    },
                    &frontend(),
                );
            }
            Op::Resume { index } => {
                let ids = self.pending(
                    self.agg
                        .state
                        .open_interrupts
                        .iter()
                        .map(|i| i.interrupt_id.as_str()),
                );
                let Some(interrupt_id) = pick(&ids, index) else {
                    return;
                };
                self.run(
                    CommandPayload::ResumeInterrupt {
                        interrupt_id,
                        payload: serde_json::Value::Null,
                    },
                    &system(),
                );
            }
            Op::Wake { advance_secs } => {
                self.now += Duration::seconds(i64::from(advance_secs));
                let now = self.now;
                match self.store.clone() {
                    Some(store) => {
                        self.plan_now = now;
                        self.trace.push(format!("wake at {now}"));
                        let written = self.agg.wake(&store, now).unwrap();
                        for event in written.map(|e| e.events).unwrap_or_default() {
                            self.log.push((event.seq, event.occurred_at, event.payload));
                        }
                    }
                    None => self.run(CommandPayload::Wake { now }, &system()),
                }
            }
            Op::View { keep, add, wait } => {
                let mut messages = self.recorded();
                messages.truncate(keep % (messages.len() + 1));
                if add || messages.is_empty() {
                    messages.push(self.message(Role::User));
                }
                self.view(messages, wait);
            }
            Op::Switch { pick: index } => {
                let Some(decision_id) = self.live_decision() else {
                    return;
                };
                let tree = &self.agg.state.tree;
                let others: Vec<String> = self
                    .recorded()
                    .iter()
                    .filter_map(|m| m.id.as_deref())
                    .flat_map(|id| tree.versions(id).into_iter().filter(move |v| v != id))
                    .collect();
                let Some(other) = pick(&others, index) else {
                    return;
                };
                let leaf = tree.newest(&other);
                let messages = self
                    .agg
                    .state
                    .at(Some(&leaf))
                    .transcript()
                    .into_iter()
                    .map(DraftMessage::from)
                    .collect();
                self.run(
                    CommandPayload::SubmitWorkerDecision {
                        decision_id,
                        transcript: messages,
                        actions: Vec::new(),
                        state: None,
                        agent: None,
                    },
                    &system(),
                );
            }
            Op::SettleSystem1 { index, ok } => {
                let ids = self.pending(
                    self.agg
                        .state
                        .effects_of(EffectKind::System1Call)
                        .filter(|c| c.tracking.status() == EffectStatus::Pending)
                        .map(|c| c.id.as_str()),
                );
                let Some(call_id) = pick(&ids, index) else {
                    return;
                };
                let cmd = match ok {
                    true => CommandPayload::settle(
                        EffectKind::System1Call,
                        call_id,
                        None,
                        Outcome::System1(Box::new(crate::system1::System1Response {
                            model: "clef".to_string(),
                            answers: Default::default(),
                            usage: Default::default(),
                            cost: None,
                        })),
                    ),
                    false => CommandPayload::settle(
                        EffectKind::System1Call,
                        call_id,
                        None,
                        SettleError::new(ErrorInfo::internal("boom"), false),
                    ),
                };
                self.run(cmd, &system());
            }
            Op::Repeat => {
                let (Some(store), Some((command, result))) =
                    (self.store.clone(), self.last.clone())
                else {
                    return;
                };
                self.trace.push(format!("repeat {}", command.id.0));
                let ran = self.agg.run(&store, self.now, command).unwrap();
                assert_eq!(ran.result, result, "a repeated command answers as it did");
                assert!(ran.written.is_none(), "a repeated command writes nothing");
            }
            Op::Cancel => {
                self.run(
                    CommandPayload::CancelSession { drop_queued: false },
                    &system(),
                );
                self.cancelled = true;
            }
        }
    }
}

fn pick(ids: &[String], index: usize) -> Option<String> {
    if ids.is_empty() {
        return None;
    }
    Some(ids[index % ids.len()].clone())
}

fn settings() -> crate::Settings {
    crate::Settings {
        snapshot_every: 3,
        ..crate::Settings::default()
    }
}

#[derive(Clone)]
struct Counted {
    store: Arc<dyn crate::Store + Send + Sync>,
    reads: std::sync::Arc<std::sync::atomic::AtomicUsize>,
}

struct CountedNodes {
    nodes: std::sync::Arc<dyn crate::tree::Nodes>,
    reads: std::sync::Arc<std::sync::atomic::AtomicUsize>,
}

impl CountedNodes {
    fn read(&self) -> &dyn crate::tree::Nodes {
        self.reads
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        self.nodes.as_ref()
    }
}

impl crate::tree::Nodes for CountedNodes {
    fn node(&self, id: &str) -> Result<Option<crate::tree::NodeRef>, crate::StoreError> {
        self.read().node(id)
    }
    fn ancestors(
        &self,
        id: &str,
        limit: usize,
    ) -> Result<Vec<crate::tree::NodeRef>, crate::StoreError> {
        self.read().ancestors(id, limit)
    }
    fn children(
        &self,
        parent: &str,
        branch: Option<&str>,
    ) -> Result<Vec<crate::tree::NodeRef>, crate::StoreError> {
        self.read().children(parent, branch)
    }
    fn answers(&self, id: &str) -> Result<Vec<crate::tree::NodeRef>, crate::StoreError> {
        self.read().answers(id)
    }
    fn branch(&self, id: &str) -> Result<Option<crate::tree::BranchRecord>, crate::StoreError> {
        self.read().branch(id)
    }
    fn versions(
        &self,
        prev: Option<&str>,
        branch: Option<&str>,
    ) -> Result<Vec<crate::tree::NodeRef>, crate::StoreError> {
        self.read().versions(prev, branch)
    }
}

impl crate::Store for Counted {
    fn append(&self, entry: &crate::Entry) -> Result<(), crate::StoreError> {
        self.store.append(entry)
    }
    fn read(&self, after: u64) -> Result<Vec<crate::Entry>, crate::StoreError> {
        self.store.read(after)
    }
    fn events(
        &self,
        after: u64,
        max: usize,
    ) -> Result<Vec<crate::SessionEvent>, crate::StoreError> {
        self.store.events(after, max)
    }
    fn events_at(&self, seqs: &[u64]) -> Result<Vec<crate::SessionEvent>, crate::StoreError> {
        self.store.events_at(seqs)
    }
    fn snapshot(&self, session: &Session) -> Result<(), crate::StoreError> {
        self.store.snapshot(session)
    }
    fn latest(&self) -> Result<Option<Session>, crate::StoreError> {
        self.store.latest()
    }
    fn cursors(&self) -> Result<std::collections::BTreeMap<String, u64>, crate::StoreError> {
        self.store.cursors()
    }
    fn set_cursor(&self, consumer: &str, seq: u64) -> Result<(), crate::StoreError> {
        self.store.set_cursor(consumer, seq)
    }
    fn nodes(&self) -> std::sync::Arc<dyn crate::tree::Nodes> {
        std::sync::Arc::new(CountedNodes {
            nodes: self.store.nodes(),
            reads: self.reads.clone(),
        })
    }
    fn handled(
        &self,
        id: &crate::CommandId,
    ) -> Result<Option<(crate::CommandResult, DateTime<Utc>)>, crate::StoreError> {
        self.store.handled(id)
    }
}

fn transcripts(session: &Session) -> Vec<(Option<String>, Vec<String>)> {
    let state = session.state();
    std::iter::once(None)
        .chain(state.branches.keys().cloned().map(Some))
        .map(|b| {
            let ids = state
                .at_scope(b.as_deref())
                .transcript()
                .into_iter()
                .map(|m| m.id)
                .collect();
            (b, ids)
        })
        .collect()
}

fn work(session: &Session) -> serde_json::Value {
    serde_json::to_value(session.open_work()).unwrap()
}

#[cfg(test)]
pub(crate) fn round_trip(state: &SessionState) -> SessionState {
    let json = serde_json::to_string(state).expect("a session state serializes");
    let mut back: SessionState = serde_json::from_str(&json).expect("and deserializes");
    back.tree = state.tree.clone();
    back
}

pub fn ops() -> impl Strategy<Value = Vec<Op>> {
    proptest::collection::vec(op(), 1..40)
}

pub fn check(
    ops: Vec<Op>,
    store: Arc<dyn crate::Store + Send + Sync>,
) -> Result<(), TestCaseError> {
    let mut world = World::stored(store);
    let mut sent: HashMap<String, serde_json::Value> = HashMap::new();
    let mut reads = 0;
    for op in ops {
        world.step(op);
        let commands = world.trace.join("\n");
        let counted = world.store.clone().expect("a stored world");
        if let Some((before, after)) = world.unprefixed() {
            prop_assert!(false, "{before}'s prompt is not a prefix of {after}'s, though {after} reads through it\ncommands:\n{commands}");
        }
        if let Some(call) = world.unfingerprinted() {
            prop_assert!(
                false,
                "{call} sent other bytes than it fingerprinted\ncommands:\n{commands}"
            );
        }
        for item in world.agg.open_work() {
            if !matches!(
                item,
                crate::Work::CallLlm { .. } | crate::Work::CallSystem1 { .. }
            ) {
                continue;
            }
            let now = serde_json::to_value(&item).unwrap();
            let first = sent
                .entry(serde_json::to_string(item.id()).unwrap())
                .or_insert_with(|| now.clone());
            prop_assert_eq!(
                &*first,
                &now,
                "an open call's work changed after it was sent\ncommands:\n{}",
                commands
            );
        }
        counted.reads.store(0, std::sync::atomic::Ordering::Relaxed);
        let loaded = Session::load(&counted, SESSION, TENANT, settings()).unwrap();
        reads = reads.max(counted.reads.load(std::sync::atomic::Ordering::Relaxed));
        prop_assert_eq!(
            serde_json::to_value(loaded.state()).unwrap(),
            serde_json::to_value(world.agg.state()).unwrap(),
            "reload diverged\ncommands:\n{}",
            commands
        );
        prop_assert_eq!(
            transcripts(&loaded),
            transcripts(&world.agg),
            "a branch reads differently after a reload\ncommands:\n{}",
            commands
        );
        prop_assert_eq!(
            work(&loaded),
            work(&world.agg),
            "the work built from a reload differs (replay read the tree {} times)\ncommands:\n{}",
            reads,
            commands
        );
    }
    Ok(())
}
