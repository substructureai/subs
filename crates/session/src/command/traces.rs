use crate::protocol::StoredResult;
use std::collections::{BTreeMap, BTreeSet, HashMap};

use chrono::{DateTime, Duration, Utc};

use crate::protocol::{ErrorCode, LlmResponse};

use super::super::decision::Trigger;
use super::super::events::EventPayload;
use super::*;
use crate::connectors::{AuthNeed, RemoteTool};
use crate::protocol::{
    AgentConfig, AgentTool, ClientAppend, ClientMessage, ClientPayload, Content, DraftMessage,
    Handler, McpServer, Role, ToolCall, ToolCallFunction,
};
use crate::protocol::{Issuer, Requester, Subject};
use crate::span::SpanContext;
use crate::Caller;
use crate::{CommitContext, Session};

const TENANT: &str = "tenant-a";
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

fn name(payload: &EventPayload) -> String {
    serde_json::to_value(payload)
        .ok()
        .and_then(|v| v.get("type")?.as_str().map(str::to_string))
        .unwrap_or_else(|| "unknown".to_string())
}

fn arm(cmd: &CommandPayload) -> &'static str {
    match cmd {
        CommandPayload::CreateSession { .. } => "CreateSession",
        CommandPayload::SubmitClientPayload { .. } => "SubmitClientPayload",
        CommandPayload::SendMessage { .. } => "SendMessage",
        CommandPayload::RequestLlmCall { .. } => "RequestLlmCall",
        CommandPayload::SettleEffect { kind, outcome, .. } => match (kind, outcome) {
            (EffectKind::LlmCall, Outcome::Error(_)) => "FailLlmCall",
            (EffectKind::LlmCall, _) => "CompleteLlmCall",
            (EffectKind::ToolCall, Outcome::Error(_)) => "FailToolCall",
            (EffectKind::ToolCall, _) => "CompleteToolCall",
            (EffectKind::ConnectorSync, Outcome::Error(_)) => "FailConnectorSync",
            (EffectKind::ConnectorSync, _) => "CompleteConnectorSync",
            (EffectKind::Subagent, Outcome::Error(_)) => "FailSubagent",
            (EffectKind::Subagent, _) => "StartSubagent",
            (EffectKind::Decision, _) => "FailWorkerDecision",
            (EffectKind::TurnEnd, _) => "SettleTurnEnd",
            (EffectKind::System1Call, Outcome::Error(_)) => "FailSystem1Call",
            (EffectKind::System1Call, _) => "CompleteSystem1Call",
        },
        CommandPayload::RequestSystem1Call { .. } => "RequestSystem1Call",
        CommandPayload::RequestToolCall { .. } => "RequestToolCall",
        CommandPayload::RequestSubagent { .. } => "RequestSubagent",
        CommandPayload::CompleteSubagentTurn { .. } => "CompleteSubagentTurn",
        CommandPayload::Interrupt { .. } => "Interrupt",
        CommandPayload::ResumeInterrupt { .. } => "ResumeInterrupt",
        CommandPayload::SubmitWorkerDecision { .. } => "SubmitWorkerDecision",
        CommandPayload::CancelSession => "CancelSession",
        CommandPayload::FinishTurn { .. } => "FinishTurn",
        CommandPayload::CompleteTurn => "CompleteTurn",
        CommandPayload::Wake { .. } => "Wake",
        CommandPayload::DeliveryFailed(_) => "DeliveryFailed",
        CommandPayload::WorkUnsent { .. } => "WorkUnsent",
        CommandPayload::DeliveryRecovered { .. } => "DeliveryRecovered",
        CommandPayload::ResumeDelivery { .. } => "ResumeDelivery",
    }
}

fn user_message(text: &str) -> DraftMessage {
    DraftMessage {
        id: None,
        role: Role::User,
        content: Some(Content::Text(text.to_string())),
        tool_calls: None,
        tool_call_id: None,
        name: None,
        reasoning: None,
    }
}

fn texts(messages: &[DraftMessage]) -> Vec<String> {
    messages
        .iter()
        .map(|m| {
            m.content
                .as_ref()
                .map(Content::text_owned)
                .unwrap_or_default()
        })
        .collect()
}

fn with_id(id: &str, text: &str) -> DraftMessage {
    DraftMessage {
        id: Some(id.to_string()),
        ..user_message(text)
    }
}

fn llm_request() -> LlmRequest {
    LlmRequest {
        model: "test-model".to_string(),
        messages: vec![],
        tools: None,
        temperature: None,
        max_completion_tokens: None,
        reasoning: None,
    }
}

fn llm_response(tool_calls: Vec<ToolCall>) -> LlmResponse {
    LlmResponse {
        model: "test-model".to_string(),
        content: None,
        tool_calls,
        finish_reason: None,
        usage: None,
        cost: None,
        images: vec![],
        reasoning: None,
    }
}

fn tool_call(id: &str, name: &str) -> ToolCall {
    ToolCall {
        id: id.to_string(),
        call_type: "function".to_string(),
        function: ToolCallFunction {
            name: name.to_string(),
            arguments: "{}".to_string(),
        },
    }
}

fn config() -> AgentConfig {
    AgentConfig {
        llm: Some("claude".to_string()),
        model: "test-model".to_string(),
        ..Default::default()
    }
}

fn config_with_client_tool(tool: &str) -> AgentConfig {
    AgentConfig {
        tools: vec![AgentTool {
            name: tool.to_string(),
            description: "d".to_string(),
            input: None,
            output: None,
            handler: Some(Handler::Client),
            defer: None,
        }],
        ..config()
    }
}

fn config_with_mcp(connection: &str) -> AgentConfig {
    AgentConfig {
        mcp: vec![McpServer {
            id: connection.to_string(),
            tools: None,
            auth_failure: Default::default(),
            tool_sync_failure: Default::default(),
            approve: Default::default(),
        }],
        ..config()
    }
}

fn meta(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
    pairs
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

fn retrying() -> RetryPolicy {
    RetryPolicy {
        queue_timeout_secs: None,
        run_timeout_secs: Some(60),
        total_timeout_secs: None,
        max_attempts: 2,
        backoff_base_secs: 1,
        backoff_max_secs: 1,
    }
}

struct Trace {
    agg: Session,
    names: Vec<String>,
    started: Vec<(String, BTreeMap<String, String>)>,
    arms: BTreeSet<&'static str>,
    now: DateTime<Utc>,
}

impl Trace {
    fn create() -> Self {
        Self::create_with(RetryPolicy::no_retry())
    }

    fn create_with(worker_retry: RetryPolicy) -> Self {
        let mut trace = Trace {
            agg: Session::with_state(
                "sess-1".to_string(),
                TENANT.to_string(),
                SessionState::new("sess-1".to_string()),
            ),
            names: Vec::new(),
            started: Vec::new(),
            arms: BTreeSet::new(),
            now: DateTime::parse_from_rfc3339("2026-01-01T00:00:00Z")
                .unwrap()
                .with_timezone(&Utc),
        };
        trace.run(
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
                worker_retry,
                agent: None,
                worker: None,
                events: None,
                state: None,
            },
            &system(),
        );
        trace
    }

    #[track_caller]
    fn run(&mut self, cmd: CommandPayload, caller: &Caller) -> Vec<EventPayload> {
        self.now += Duration::milliseconds(1);
        self.arms.insert(arm(&cmd));
        let events = self
            .agg
            .handle(cmd, caller, self.now)
            .expect("command rejected");
        self.agg.commit(
            events.clone(),
            &CommitContext {
                span: SpanContext::root(),
                occurred_at: self.now,
            },
        );
        self.names.extend(events.iter().map(name));
        self.started.extend(events.iter().filter_map(|e| match e {
            EventPayload::TurnStarted(t) => Some((t.turn_id.clone(), t.metadata.clone())),
            _ => None,
        }));
        events
    }

    #[track_caller]
    fn reject(&mut self, cmd: CommandPayload, caller: &Caller) -> SessionError {
        self.arms.insert(arm(&cmd));
        self.agg
            .handle(cmd, caller, self.now)
            .expect_err("command should have been rejected")
    }

    fn advance(&mut self, secs: i64) {
        self.now += Duration::seconds(secs);
    }

    fn wake(&mut self) {
        let now = self.now + Duration::milliseconds(1);
        self.run(CommandPayload::Wake { now }, &system());
    }

    fn submit(&mut self, text: &str, turn_id: Option<&str>) {
        self.run(self.submission(text, turn_id, false), &frontend());
    }

    fn submit_queued(&mut self, text: &str, turn_id: &str) {
        self.run(self.submission(text, Some(turn_id), true), &frontend());
    }

    fn submission(&self, text: &str, turn_id: Option<&str>, queue: bool) -> CommandPayload {
        CommandPayload::SubmitClientPayload {
            payload: ClientPayload::Message(ClientMessage {
                message: user_message(text),
                stream: false,
            }),
            turn: turn_id.map_or(TurnTarget::Detached, |t| TurnTarget::Open(t.to_string())),
            queue: queue.into(),
            metadata: Default::default(),
        }
    }

    fn merge(
        &mut self,
        messages: Vec<DraftMessage>,
        turn_id: &str,
        state: serde_json::Value,
        metadata: &[(&str, &str)],
    ) {
        self.run(
            CommandPayload::SubmitClientPayload {
                payload: ClientPayload::Append(ClientAppend {
                    messages,
                    stream: false,
                    client: ClientContext {
                        state: Some(state),
                        ..Default::default()
                    },
                }),
                turn: TurnTarget::Open(turn_id.to_string()),
                queue: QueueMode::Merge,
                metadata: meta(metadata),
            },
            &frontend(),
        );
    }

    fn click(&mut self, turn_id: &str, metadata: &[(&str, &str)]) {
        self.run(click_command(turn_id, metadata), &frontend());
    }

    fn live_turn(&self) -> Option<String> {
        let id = self.live_decision();
        let turn = self.agg.state.worker_decision(&id)?.turn.as_ref()?;
        Some(turn.id.clone())
    }

    fn waiting_turns(&self) -> Vec<(String, Vec<String>)> {
        self.agg
            .state
            .queued_decisions()
            .into_iter()
            .filter_map(|e| match &e.decision()?.trigger {
                Trigger::ClientMessage {
                    messages,
                    turn_id: Some(turn_id),
                    ..
                } => Some((turn_id.clone(), texts(messages))),
                _ => None,
            })
            .collect()
    }

    #[track_caller]
    fn live_decision(&self) -> String {
        let mut live = self
            .agg
            .state
            .effects_of(EffectKind::Decision)
            .filter(|d| d.tracking.status() == EffectStatus::Pending);
        let id = live.next().map(|d| d.id.clone()).expect("a live decision");
        assert!(live.next().is_none(), "more than one live decision");
        id
    }

    #[track_caller]
    fn live_trigger(&self) -> Trigger {
        let id = self.live_decision();
        self.agg.state.worker_decision(&id).unwrap().trigger.clone()
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

    fn decide(&mut self, transcript: Vec<DraftMessage>, actions: Vec<Action>) {
        self.decide_with(transcript, actions, None);
    }

    #[track_caller]
    fn decide_with(
        &mut self,
        transcript: Vec<DraftMessage>,
        actions: Vec<Action>,
        agent: Option<AgentConfig>,
    ) {
        let decision_id = self.live_decision();
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

    fn decide_appending(&mut self, message: DraftMessage, actions: Vec<Action>) {
        let mut transcript = self.recorded();
        transcript.push(message);
        self.decide(transcript, actions);
    }

    #[track_caller]
    fn assert_trace(&self, expected: &[&str]) {
        if self.names != expected {
            panic!(
                "event trace changed.\nactual:\n{}\n",
                self.names
                    .iter()
                    .map(|n| format!("        \"{n}\","))
                    .collect::<Vec<_>>()
                    .join("\n")
            );
        }
    }
}

fn call_llm(id: &str, retry: RetryPolicy) -> Action {
    Action::CallLlm {
        llm: "claude".to_string(),
        format: None,
        id: id.to_string(),
        request: llm_request(),
        stream: false,
        retry,
        handler: LlmHandler::Server,
    }
}

fn call_tool(id: &str, name: &str) -> Action {
    Action::CallTool {
        id: id.to_string(),
        name: name.to_string(),
        arguments: "{}".to_string(),
        retry: None,
    }
}

fn done() -> Action {
    Action::Done {
        data: serde_json::Value::Null,
    }
}

fn flow_full_loop() -> Trace {
    let mut t = Trace::create();
    t.decide(vec![], vec![]);

    t.submit("hi", Some("turn-1"));

    t.decide_appending(
        user_message("hi"),
        vec![call_llm("call-1", RetryPolicy::no_retry())],
    );

    t.run(
        CommandPayload::settle(
            EffectKind::LlmCall,
            "call-1".to_string(),
            Some(0),
            Outcome::Llm(Box::new(llm_response(vec![tool_call(
                "tc-1",
                "get_weather",
            )]))),
        ),
        &system(),
    );

    assert!(matches!(
        t.live_trigger(),
        Trigger::LlmFinished { ok: true, .. }
    ));
    let assistant = DraftMessage {
        id: Some("call-1".to_string()),
        role: Role::Assistant,
        content: None,
        tool_calls: Some(vec![tool_call("tc-1", "get_weather")]),
        tool_call_id: None,
        name: None,
        reasoning: None,
    };
    t.decide_appending(assistant, vec![call_tool("tc-1", "get_weather")]);

    assert!(matches!(t.live_trigger(), Trigger::ToolExecute { .. }));
    t.decide(
        vec![],
        vec![Action::ToolResult {
            id: "tc-1".to_string(),
            attempt: Some(0),
            result: StoredResult::text("sunny".to_string()),
        }],
    );

    assert!(matches!(
        t.live_trigger(),
        Trigger::ToolFinished { ok: true, .. }
    ));
    let tool_answer = DraftMessage {
        id: None,
        role: Role::Tool,
        content: Some(Content::Text("sunny".to_string())),
        tool_calls: None,
        tool_call_id: Some("tc-1".to_string()),
        name: None,
        reasoning: None,
    };
    t.decide_appending(
        tool_answer,
        vec![Action::Done {
            data: serde_json::json!({"ok": true}),
        }],
    );

    assert!(matches!(t.live_trigger(), Trigger::TurnFinished { .. }));
    t.decide(vec![], vec![done()]);
    t
}

#[test]
fn done_closes_the_calls_still_open() {
    let mut t = Trace::create();
    t.decide(vec![], vec![]);
    t.submit("render it", Some("turn-1"));
    t.decide_appending(
        user_message("render it"),
        vec![call_llm("call-1", RetryPolicy::no_retry())],
    );
    t.run(
        CommandPayload::settle(
            EffectKind::LlmCall,
            "call-1".to_string(),
            Some(0),
            Outcome::Llm(Box::new(llm_response(vec![tool_call("tc-1", "render")]))),
        ),
        &system(),
    );
    let assistant = DraftMessage {
        id: Some("call-1".to_string()),
        role: Role::Assistant,
        content: None,
        tool_calls: Some(vec![tool_call("tc-1", "render")]),
        tool_call_id: None,
        name: None,
        reasoning: None,
    };
    t.decide_appending(assistant, vec![call_tool("tc-1", "render")]);
    assert!(matches!(t.live_trigger(), Trigger::ToolExecute { .. }));

    t.decide(vec![], vec![done()]);

    let last = t.recorded().pop().expect("a message closes the call");
    assert_eq!(last.tool_call_id.as_deref(), Some("tc-1"));
    assert_eq!(
        last.content.as_ref().map(Content::text_owned).as_deref(),
        Some(crate::effects::turn_end::CANCELLED_AT_TURN_END)
    );
    assert_eq!(
        t.agg
            .state
            .effect(EffectKind::ToolCall, "tc-1")
            .map(|e| e.tracking.status()),
        Some(EffectStatus::Failed),
        "the call is voided"
    );
    assert!(matches!(t.live_trigger(), Trigger::TurnFinished { .. }));
    let late = t.reject(
        CommandPayload::settle(
            EffectKind::ToolCall,
            "tc-1".to_string(),
            None,
            Outcome::Tool {
                result: StoredResult::text("https://reports/42.pdf".to_string()),
            },
        ),
        &Caller::ApiKey {
            tenant_id: TENANT.to_string(),
            key_id: "key-1".to_string(),
        },
    );
    assert_eq!(
        late,
        SessionError::EffectNotPending,
        "a late result is refused"
    );
}

fn click_command(turn_id: &str, metadata: &[(&str, &str)]) -> CommandPayload {
    CommandPayload::SubmitClientPayload {
        payload: ClientPayload::Action(crate::protocol::ClientAction {
            name: "approve".to_string(),
            args: None,
        }),
        turn: TurnTarget::Open(turn_id.to_string()),
        queue: QueueMode::Refuse,
        metadata: meta(metadata),
    }
}

fn at_tool_execute() -> Trace {
    let mut t = Trace::create();
    t.decide(vec![], vec![]);
    t.submit("render it", Some("turn-1"));
    t.decide_appending(
        user_message("render it"),
        vec![call_llm("call-1", RetryPolicy::no_retry())],
    );
    t.run(
        CommandPayload::settle(
            EffectKind::LlmCall,
            "call-1".to_string(),
            Some(0),
            Outcome::Llm(Box::new(llm_response(vec![tool_call("tc-1", "render")]))),
        ),
        &system(),
    );
    let assistant = DraftMessage {
        id: Some("call-1".to_string()),
        role: Role::Assistant,
        content: None,
        tool_calls: Some(vec![tool_call("tc-1", "render")]),
        tool_call_id: None,
        name: None,
        reasoning: None,
    };
    t.decide_appending(assistant, vec![call_tool("tc-1", "render")]);
    t
}

#[test]
fn cancel_closes_the_open_calls_and_keeps_the_session() {
    let mut t = at_tool_execute();
    t.run(CommandPayload::CancelSession, &frontend());
    let last = t.recorded().pop().expect("the call gets a result");
    assert_eq!(last.tool_call_id.as_deref(), Some("tc-1"));
    assert!(t.agg.state.phase.turn_id().is_none());
    t.submit("again", Some("turn-2"));
    assert_eq!(t.agg.state.phase.turn_id(), Some("turn-2"));
}

#[test]
fn done_keeps_a_result_that_arrived_with_it() {
    let mut t = at_tool_execute();
    t.decide(
        vec![],
        vec![
            Action::ToolResult {
                id: "tc-1".to_string(),
                attempt: Some(0),
                result: StoredResult::text("https://reports/42.pdf".to_string()),
            },
            done(),
        ],
    );
    let last = t.recorded().pop().expect("the result is recorded");
    assert_eq!(last.tool_call_id.as_deref(), Some("tc-1"));
    assert_eq!(
        last.content.as_ref().map(Content::text_owned).as_deref(),
        Some("https://reports/42.pdf")
    );
}

#[test]
fn done_keeps_the_result_of_the_decision_it_answers() {
    let mut t = at_tool_execute();
    t.decide(
        vec![],
        vec![Action::ToolResult {
            id: "tc-1".to_string(),
            attempt: Some(0),
            result: StoredResult::text("https://reports/42.pdf".to_string()),
        }],
    );
    assert!(matches!(t.live_trigger(), Trigger::ToolFinished { .. }));
    t.decide(vec![], vec![done()]);
    let last = t.recorded().pop().expect("the result is recorded");
    assert_eq!(
        last.content.as_ref().map(Content::text_owned).as_deref(),
        Some("https://reports/42.pdf")
    );
}

#[test]
fn a_turn_id_a_click_holds_is_refused_to_a_message() {
    let mut t = Trace::create();
    t.decide(vec![], vec![]);
    t.submit("hi", Some("turn-1"));
    t.click("t9", &[]);
    let err = t.reject(t.submission("later", Some("t9"), true), &frontend());
    assert!(
        matches!(err, SessionError::TurnAlreadyActive { .. }),
        "{err:?}"
    );
}

#[test]
fn a_click_naming_the_ending_turn_is_refused() {
    let mut t = Trace::create();
    t.decide(vec![], vec![]);
    t.submit("hi", Some("turn-1"));
    t.decide_appending(user_message("hi"), vec![done()]);
    let err = t.reject(click_command("turn-1", &[]), &frontend());
    assert!(
        matches!(err, SessionError::TurnAlreadyActive { .. }),
        "{err:?}"
    );
}

#[test]
fn a_retried_click_opens_its_own_turn_after_the_joined_turn_ends() {
    let mut t = Trace::create_with(retrying());
    t.decide(vec![], vec![]);
    t.submit("hi", Some("turn-1"));
    t.decide_appending(
        user_message("hi"),
        vec![call_llm("call-1", RetryPolicy::no_retry())],
    );
    t.click("turn-2", &[("ref", "m3")]);
    let click = t.live_decision();
    assert_eq!(t.live_turn().as_deref(), Some("turn-1"), "the click joins");
    t.run(
        CommandPayload::settle(
            EffectKind::Decision,
            click.clone(),
            None,
            SettleError::new(ErrorInfo::internal("worker down".to_string()), true),
        ),
        &system(),
    );
    t.run(
        CommandPayload::settle(
            EffectKind::LlmCall,
            "call-1".to_string(),
            Some(0),
            Outcome::Llm(Box::new(llm_response(vec![]))),
        ),
        &system(),
    );
    t.decide(vec![], vec![done()]);
    t.decide(vec![], vec![done()]);
    assert_eq!(t.agg.state.completed_turn_ids, vec!["turn-1".to_string()]);

    t.advance(2);
    t.wake();
    assert_eq!(t.live_decision(), click);
    assert_eq!(t.live_turn().as_deref(), Some("turn-2"));
    assert_eq!(
        t.started.last(),
        Some(&("turn-2".to_string(), meta(&[("ref", "m3")])))
    );
}

#[test]
fn a_click_while_a_turn_ends_waits_for_it() {
    let mut t = Trace::create();
    t.decide(vec![], vec![]);
    t.submit("hi", Some("turn-1"));
    t.decide_appending(user_message("hi"), vec![done()]);
    assert!(matches!(t.live_trigger(), Trigger::TurnFinished { .. }));

    t.click("turn-2", &[("ref", "m1")]);
    assert_eq!(
        t.agg.state.phase.turn_id(),
        Some("turn-1"),
        "turn-1 still ends"
    );
    assert!(matches!(t.live_trigger(), Trigger::TurnFinished { .. }));

    t.decide(vec![], vec![done()]);
    assert_eq!(t.agg.state.completed_turn_ids, vec!["turn-1".to_string()]);
    assert!(matches!(t.live_trigger(), Trigger::ClientAction { .. }));
    assert_eq!(t.live_turn().as_deref(), Some("turn-2"));
    assert_eq!(
        t.started.last(),
        Some(&("turn-2".to_string(), meta(&[("ref", "m1")])))
    );
}

#[test]
fn a_joined_click_runs_in_its_own_turn_when_the_turn_ends_first() {
    let mut t = Trace::create();
    t.decide(vec![], vec![]);
    t.submit("hi", Some("turn-1"));
    t.click("turn-2", &[("ref", "m2")]);
    assert_eq!(t.agg.state.phase.turn_id(), Some("turn-1"));

    t.decide_appending(user_message("hi"), vec![done()]);
    t.decide(vec![], vec![done()]);
    assert!(matches!(t.live_trigger(), Trigger::ClientAction { .. }));
    assert_eq!(t.live_turn().as_deref(), Some("turn-2"));
    assert_eq!(
        t.started.last(),
        Some(&("turn-2".to_string(), meta(&[("ref", "m2")])))
    );
}

#[test]
fn a_click_turn_does_not_wait_for_a_connector_sync() {
    let mut t = Trace::create();
    t.decide(vec![], vec![]);
    t.click("turn-a", &[]);
    t.decide_with(vec![], vec![], Some(config_with_mcp("conn-1")));
    assert_eq!(
        t.agg.state.phase.finalizing().map(|f| f.turn_id.as_str()),
        Some("turn-a"),
        "a sync is not the turn's work"
    );

    t.run(
        CommandPayload::settle(
            EffectKind::ConnectorSync,
            "mcp.conn-1".to_string(),
            Some(0),
            Outcome::Connector {
                server: None,
                prefix: None,
                tools: vec![],
                instructions: None,
            },
        ),
        &system(),
    );
    assert!(matches!(
        t.live_trigger(),
        Trigger::TurnFinished { turn_id, .. } if turn_id == "turn-a"
    ));
}

#[test]
fn an_ending_turn_waits_for_the_work_its_last_step_starts() {
    let mut t = Trace::create();
    t.decide(vec![], vec![]);
    t.submit("hi", Some("turn-1"));
    t.decide_appending(user_message("hi"), vec![done()]);
    t.click("turn-2", &[]);

    t.decide(vec![], vec![call_llm("call-9", RetryPolicy::no_retry())]);
    assert!(t.agg.state.completed_turn_ids.is_empty(), "turn-1 waits");
    assert_eq!(t.agg.state.phase.turn_id(), Some("turn-1"));

    t.run(
        CommandPayload::settle(
            EffectKind::LlmCall,
            "call-9".to_string(),
            Some(0),
            Outcome::Llm(Box::new(llm_response(vec![]))),
        ),
        &system(),
    );
    assert!(matches!(t.live_trigger(), Trigger::LlmFinished { .. }));
    assert_eq!(t.live_turn().as_deref(), Some("turn-1"));

    t.decide(vec![], vec![done()]);
    assert_eq!(t.agg.state.completed_turn_ids, vec!["turn-1".to_string()]);
    assert!(matches!(t.live_trigger(), Trigger::ClientAction { .. }));
    assert_eq!(t.live_turn().as_deref(), Some("turn-2"));
}

#[test]
fn a_click_that_resolves_an_interrupt_keeps_its_turn() {
    let mut t = Trace::create();
    t.decide(vec![], vec![]);
    t.run(
        CommandPayload::Interrupt {
            interrupt_id: "int-1".to_string(),
            reason: "approve?".to_string(),
            payload: serde_json::Value::Null,
        },
        &frontend(),
    );
    t.click("turn-a", &[]);
    t.decide(
        vec![],
        vec![Action::ResolveInterrupt {
            interrupt_id: "int-1".to_string(),
            payload: serde_json::Value::Null,
        }],
    );
    assert_eq!(t.agg.state.phase.active_turn_id(), Some("turn-a"));
    assert!(matches!(t.live_trigger(), Trigger::InterruptResumed { .. }));
    assert_eq!(t.live_turn().as_deref(), Some("turn-a"));
}

#[test]
fn trace_full_loop() {
    let t = flow_full_loop();
    assert_eq!(
        t.agg.state.data,
        serde_json::json!({"ok": true}),
        "the turn's output survives finalization"
    );
    t.assert_trace(&[
        "session.created",
        "decision.queued",
        "decision.dispatched",
        "decision.completed",
        "turn.started",
        "decision.queued",
        "decision.dispatched",
        "decision.completed",
        "message.new",
        "llm.call.requested",
        "llm.call.dispatched",
        "llm.call.completed",
        "decision.queued",
        "decision.dispatched",
        "decision.completed",
        "message.new",
        "tool.call.requested",
        "tool.call.dispatched",
        "decision.queued",
        "decision.dispatched",
        "decision.completed",
        "tool.call.completed",
        "decision.queued",
        "decision.dispatched",
        "decision.completed",
        "message.new",
        "decision.queued",
        "decision.dispatched",
        "decision.completed",
        "turn.completed",
        "session.done",
    ]);
}

fn flow_connector_gating() -> Trace {
    let mut t = Trace::create();
    t.decide_with(vec![], vec![], Some(config_with_mcp("conn-1")));
    assert!(
        t.agg
            .state
            .tracking(EffectKind::ConnectorSync, "mcp.conn-1")
            .unwrap()
            .is_in_flight(),
        "the fetch is in flight"
    );

    t.submit("hi", Some("turn-1"));

    assert!(
        !t.agg.state.has_pending_worker_decision(),
        "the decision waits for the fetch"
    );
    assert_eq!(
        super::schedule::waiting_on(&t.agg.state).values().next(),
        Some(&vec!["connector_sync:mcp.conn-1".to_string()]),
        "and says so"
    );

    t.run(
        CommandPayload::settle(
            EffectKind::ConnectorSync,
            "mcp.conn-1".to_string(),
            Some(0),
            Outcome::Connector {
                server: None,
                prefix: None,
                tools: vec![RemoteTool {
                    name: "lookup".to_string(),
                    title: None,
                    description: "d".to_string(),
                    input: None,
                    output: None,
                    annotations: Default::default(),
                }],
                instructions: None,
            },
        ),
        &system(),
    );

    t.decide_appending(
        user_message("hi"),
        vec![call_llm("call-1", RetryPolicy::no_retry())],
    );
    t
}

#[test]
fn trace_connector_fetch_gates_an_llm_call() {
    let t = flow_connector_gating();
    let merged = t
        .agg
        .state
        .llm_call("call-1")
        .unwrap()
        .sent_tools
        .clone()
        .unwrap_or_default();
    assert_eq!(
        merged.iter().map(|t| t.name.as_str()).collect::<Vec<_>>(),
        vec!["lookup"],
        "dispatch merges the connector tools in force"
    );
    t.assert_trace(&[
        "session.created",
        "decision.queued",
        "decision.dispatched",
        "decision.completed",
        "connector.sync.requested",
        "agent.updated",
        "turn.started",
        "decision.queued",
        "connector.sync.completed",
        "decision.dispatched",
        "decision.completed",
        "message.new",
        "llm.call.requested",
        "llm.call.dispatched",
    ]);
}

fn flow_connector_failure_releases_the_gate() -> Trace {
    let mut t = Trace::create_with(retrying());
    t.decide_with(vec![], vec![], Some(config_with_mcp("conn-1")));
    t.submit("hi", Some("turn-1"));
    t.run(
        CommandPayload::settle(
            EffectKind::ConnectorSync,
            "mcp.conn-1".to_string(),
            Some(0),
            SettleError::new(ErrorInfo::internal("unreachable"), false)
                .auth(Some(AuthNeed::Reauthorize)),
        ),
        &system(),
    );
    t
}

#[test]
fn trace_a_failed_fetch_releases_the_gate() {
    let t = flow_connector_failure_releases_the_gate();
    assert!(
        t.agg.state.has_pending_worker_decision(),
        "the decision the fetch was holding goes live"
    );
    t.assert_trace(&[
        "session.created",
        "decision.queued",
        "decision.dispatched",
        "decision.completed",
        "connector.sync.requested",
        "agent.updated",
        "turn.started",
        "decision.queued",
        "connector.sync.errored",
        "decision.dispatched",
    ]);
}

fn flow_interrupt_and_resume() -> Trace {
    let mut t = Trace::create();
    t.decide_with(vec![], vec![], Some(config_with_client_tool("ask_user")));
    t.submit("hi", Some("turn-1"));

    t.decide_appending(
        user_message("hi"),
        vec![
            call_tool("tc-1", "ask_user"),
            call_llm("call-1", RetryPolicy::no_retry()),
        ],
    );

    t.run(
        CommandPayload::Interrupt {
            interrupt_id: "int-1".to_string(),
            reason: "approval".to_string(),
            payload: serde_json::Value::Null,
        },
        &frontend(),
    );

    let err = t.reject(
        CommandPayload::SubmitClientPayload {
            payload: ClientPayload::Message(ClientMessage {
                message: user_message("again"),
                stream: false,
            }),
            turn: TurnTarget::Detached,
            queue: QueueMode::Refuse,
            metadata: Default::default(),
        },
        &frontend(),
    );
    assert!(matches!(err, SessionError::SessionInterrupted));

    t.run(
        CommandPayload::settle(
            EffectKind::ToolCall,
            "tc-1".to_string(),
            Some(0),
            Outcome::Tool {
                result: StoredResult::text("yes".to_string()),
            },
        ),
        &frontend(),
    );
    assert!(
        !t.agg.state.has_pending_worker_decision(),
        "the parked branch dispatches nothing"
    );

    t.run(
        CommandPayload::ResumeInterrupt {
            interrupt_id: "int-1".to_string(),
            payload: serde_json::json!({"approved": true}),
        },
        &frontend(),
    );
    t
}

#[test]
fn trace_interrupt_and_resume() {
    let t = flow_interrupt_and_resume();
    assert!(
        matches!(t.live_trigger(), Trigger::InterruptResumed { .. }),
        "the resume answers first, ahead of the tool.finished queued before it"
    );
    t.assert_trace(&[
        "session.created",
        "decision.queued",
        "decision.dispatched",
        "decision.completed",
        "agent.updated",
        "turn.started",
        "decision.queued",
        "decision.dispatched",
        "decision.completed",
        "message.new",
        "tool.call.requested",
        "llm.call.requested",
        "tool.call.dispatched",
        "llm.call.dispatched",
        "session.interrupted",
        "call.voided",
        "tool.call.completed",
        "decision.queued",
        "session.interrupt_resumed",
        "decision.queued",
        "decision.dispatched",
    ]);
}

fn flow_queued_turn() -> Trace {
    let mut t = Trace::create();
    t.decide(vec![], vec![]);
    t.submit("hi", Some("turn-1"));

    t.submit_queued("and another thing", "turn-2");
    assert_eq!(
        t.agg.state.phase.turn_id(),
        Some("turn-1"),
        "the running turn keeps the phase"
    );

    assert!(matches!(
        t.reject(t.submission("hi", Some("turn-1"), true), &frontend()),
        SessionError::TurnAlreadyActive { .. }
    ));
    assert!(matches!(
        t.reject(
            t.submission("and another thing", Some("turn-2"), true),
            &frontend()
        ),
        SessionError::TurnAlreadyActive { .. }
    ));

    t.decide_appending(user_message("hi"), vec![done()]);
    t.decide(vec![], vec![done()]);
    t
}

#[test]
fn trace_queued_turn() {
    let t = flow_queued_turn();
    assert_eq!(
        t.agg.state.phase.turn_id(),
        Some("turn-2"),
        "the queued turn is the one running now"
    );
    assert!(
        matches!(t.live_trigger(), Trigger::ClientMessage { .. }),
        "delivered as an ordinary client message"
    );
    t.assert_trace(&[
        "session.created",
        "decision.queued",
        "decision.dispatched",
        "decision.completed",
        "turn.started",
        "decision.queued",
        "decision.dispatched",
        "decision.queued",
        "decision.completed",
        "message.new",
        "decision.queued",
        "decision.dispatched",
        "decision.completed",
        "turn.completed",
        "session.done",
        "turn.started",
        "decision.dispatched",
    ]);
}

fn flow_merged_turn() -> Trace {
    let mut t = Trace::create();
    t.decide(vec![], vec![]);
    t.submit("hi", Some("turn-1"));

    t.merge(
        vec![user_message("Sam: had 120ml")],
        "turn-2",
        serde_json::json!({"ref": "m1"}),
        &[("ref", "m1"), ("room", "r1")],
    );
    t.merge(
        vec![user_message("Sam: nap at 2")],
        "turn-3",
        serde_json::json!({"ref": "m2"}),
        &[("ref", "m2")],
    );
    assert_eq!(
        t.waiting_turns(),
        vec![(
            "turn-2".to_string(),
            vec!["Sam: had 120ml".to_string(), "Sam: nap at 2".to_string()]
        )],
        "both messages wait in the first merged turn"
    );

    t.decide_appending(user_message("hi"), vec![done()]);
    t.decide(vec![], vec![done()]);
    t
}

#[test]
fn trace_merged_turn() {
    let t = flow_merged_turn();
    assert_eq!(t.agg.state.phase.turn_id(), Some("turn-2"));
    let Trigger::ClientMessage {
        messages, client, ..
    } = t.live_trigger()
    else {
        panic!("a client message runs the merged turn");
    };
    assert_eq!(texts(&messages), vec!["Sam: had 120ml", "Sam: nap at 2"]);
    assert_eq!(
        client.state,
        Some(serde_json::json!({"ref": "m2"})),
        "the last state wins"
    );
    assert_eq!(
        t.started,
        vec![
            ("turn-1".to_string(), BTreeMap::new()),
            ("turn-2".to_string(), meta(&[("ref", "m2"), ("room", "r1")])),
        ],
        "a merged turn starts with the metadata of each submit, the later key wins"
    );
    t.assert_trace(&[
        "session.created",
        "decision.queued",
        "decision.dispatched",
        "decision.completed",
        "turn.started",
        "decision.queued",
        "decision.dispatched",
        "decision.queued",
        "decision.amended",
        "decision.completed",
        "message.new",
        "decision.queued",
        "decision.dispatched",
        "decision.completed",
        "turn.completed",
        "session.done",
        "turn.started",
        "decision.dispatched",
    ]);
}

#[test]
fn a_merge_with_no_turn_running_starts_at_once() {
    let mut t = Trace::create();
    t.decide(vec![], vec![]);
    t.merge(
        vec![user_message("hi")],
        "turn-1",
        serde_json::json!({"ref": "m0"}),
        &[("ref", "m0")],
    );
    assert_eq!(t.agg.state.phase.turn_id(), Some("turn-1"));
    assert_eq!(
        t.started,
        vec![("turn-1".to_string(), meta(&[("ref", "m0")]))]
    );
    assert_eq!(t.agg.state.turn_metadata, meta(&[("ref", "m0")]));
    assert!(t.waiting_turns().is_empty());
}

#[test]
fn a_merge_keeps_arrival_order_behind_a_held_turn() {
    let mut t = Trace::create();
    t.decide(vec![], vec![]);
    t.submit("hi", Some("turn-1"));
    t.merge(
        vec![user_message("a")],
        "turn-2",
        serde_json::json!({}),
        &[],
    );
    t.submit_queued("b", "turn-3");
    t.merge(
        vec![user_message("c")],
        "turn-4",
        serde_json::json!({}),
        &[],
    );
    t.merge(
        vec![user_message("d")],
        "turn-5",
        serde_json::json!({}),
        &[],
    );
    assert_eq!(
        t.waiting_turns(),
        vec![
            ("turn-2".to_string(), vec!["a".to_string()]),
            ("turn-3".to_string(), vec!["b".to_string()]),
            ("turn-4".to_string(), vec!["c".to_string(), "d".to_string()]),
        ]
    );
}

#[test]
fn a_merge_drops_a_message_already_waiting() {
    let mut t = Trace::create();
    t.decide(vec![], vec![]);
    t.submit("hi", Some("turn-1"));
    t.merge(
        vec![with_id("c1", "a")],
        "turn-2",
        serde_json::json!({}),
        &[],
    );
    t.merge(
        vec![with_id("c1", "a"), with_id("c2", "b")],
        "turn-3",
        serde_json::json!({}),
        &[],
    );
    assert_eq!(
        t.waiting_turns(),
        vec![("turn-2".to_string(), vec!["a".to_string(), "b".to_string()])]
    );
}

fn flow_timeout_to_exhaustion() -> Trace {
    let mut t = Trace::create();
    t.decide(vec![], vec![]);
    t.submit("hi", Some("turn-1"));

    t.decide_appending(
        user_message("hi"),
        vec![call_llm(
            "call-1",
            RetryPolicy {
                queue_timeout_secs: None,
                run_timeout_secs: Some(1),
                total_timeout_secs: None,
                max_attempts: 2,
                backoff_base_secs: 1,
                backoff_max_secs: 1,
            },
        )],
    );

    t.advance(2);
    t.wake();
    t.advance(2);
    t.wake();
    t.advance(2);
    t.wake();

    assert!(matches!(
        t.live_trigger(),
        Trigger::LlmFinished { ok: false, .. }
    ));
    t.decide(vec![], vec![done()]);
    t.decide(vec![], vec![done()]);
    t
}

#[test]
fn trace_timeout_retry_then_exhaustion() {
    let t = flow_timeout_to_exhaustion();
    t.assert_trace(&[
        "session.created",
        "decision.queued",
        "decision.dispatched",
        "decision.completed",
        "turn.started",
        "decision.queued",
        "decision.dispatched",
        "decision.completed",
        "message.new",
        "llm.call.requested",
        "llm.call.dispatched",
        "llm.call.errored",
        "llm.call.requested",
        "llm.call.dispatched",
        "llm.call.errored",
        "decision.queued",
        "decision.dispatched",
        "decision.completed",
        "decision.queued",
        "decision.dispatched",
        "decision.completed",
        "turn.completed",
        "session.done",
    ]);
}

fn flow_decision_failure() -> Trace {
    let mut t = Trace::create_with(retrying());
    t.decide(vec![], vec![]);
    t.submit("hi", Some("turn-1"));

    let decision_id = t.live_decision();
    t.run(
        CommandPayload::settle(
            EffectKind::Decision,
            decision_id.clone(),
            None,
            SettleError::new(ErrorInfo::internal("worker down".to_string()), true),
        ),
        &system(),
    );
    assert!(
        !t.agg.state.has_pending_worker_decision(),
        "a failed decision leaves the slot"
    );

    t.advance(2);
    t.wake();
    assert_eq!(t.live_decision(), decision_id, "the same decision, retried");

    t.run(
        CommandPayload::settle(
            EffectKind::Decision,
            decision_id,
            None,
            SettleError::new(ErrorInfo::internal("worker down".to_string()), true),
        ),
        &system(),
    );
    t
}

#[test]
fn trace_decision_retry_then_terminal_failure() {
    let t = flow_decision_failure();
    assert!(
        t.agg
            .state
            .effects_of(EffectKind::Decision)
            .next()
            .is_none(),
        "a terminally failed decision leaves the table"
    );
    t.assert_trace(&[
        "session.created",
        "decision.queued",
        "decision.dispatched",
        "decision.completed",
        "turn.started",
        "decision.queued",
        "decision.dispatched",
        "decision.errored",
        "decision.dispatched",
        "decision.errored",
        "turn.completed",
        "session.done",
    ]);
}

fn flow_parallel_fan_out() -> Trace {
    let mut t = Trace::create();
    t.decide(vec![], vec![]);
    t.submit("hi", Some("turn-1"));

    t.decide_appending(
        user_message("hi"),
        vec![call_tool("tc-1", "a"), call_tool("tc-2", "b")],
    );

    assert!(matches!(t.live_trigger(), Trigger::ToolExecute { .. }));
    t.decide(
        vec![],
        vec![Action::ToolResult {
            id: "tc-1".to_string(),
            attempt: Some(0),
            result: StoredResult::text("one".to_string()),
        }],
    );
    t.decide(
        vec![],
        vec![Action::ToolResult {
            id: "tc-2".to_string(),
            attempt: Some(0),
            result: StoredResult::text("two".to_string()),
        }],
    );
    t
}

#[test]
fn trace_parallel_fan_out_holds_a_sibling_result() {
    let t = flow_parallel_fan_out();
    let live = t.live_decision();
    assert!(matches!(t.live_trigger(), Trigger::ToolFinished { .. }));
    assert_eq!(
        t.agg.state.event_meta(t.now).pending_work(&live),
        1,
        "the sibling's tool.finished is queued and unrecorded"
    );
    t.assert_trace(&[
        "session.created",
        "decision.queued",
        "decision.dispatched",
        "decision.completed",
        "turn.started",
        "decision.queued",
        "decision.dispatched",
        "decision.completed",
        "message.new",
        "tool.call.requested",
        "tool.call.requested",
        "tool.call.dispatched",
        "decision.queued",
        "tool.call.dispatched",
        "decision.queued",
        "decision.dispatched",
        "decision.completed",
        "tool.call.completed",
        "decision.queued",
        "decision.dispatched",
        "decision.completed",
        "tool.call.completed",
        "decision.queued",
        "decision.dispatched",
    ]);
}

fn flow_fork_voids_stranded_work() -> Trace {
    let mut t = Trace::create();
    t.decide(vec![], vec![]);
    t.submit("hi", Some("turn-1"));

    t.decide_appending(
        user_message("hi"),
        vec![call_llm("call-1", RetryPolicy::no_retry())],
    );
    t.run(
        CommandPayload::settle(
            EffectKind::LlmCall,
            "call-1".to_string(),
            Some(0),
            Outcome::Llm(Box::new(llm_response(vec![tool_call("tc-1", "a")]))),
        ),
        &system(),
    );

    let assistant = DraftMessage {
        id: Some("call-1".to_string()),
        role: Role::Assistant,
        content: None,
        tool_calls: Some(vec![tool_call("tc-1", "a")]),
        tool_call_id: None,
        name: None,
        reasoning: None,
    };
    t.decide_appending(assistant, vec![call_tool("tc-1", "a")]);

    let root: Vec<DraftMessage> = t.recorded().into_iter().take(1).collect();
    t.decide(root, vec![]);
    t
}

#[test]
fn trace_fork_voids_stranded_work() {
    let t = flow_fork_voids_stranded_work();
    assert_eq!(
        t.agg
            .state
            .effect(EffectKind::ToolCall, "tc-1")
            .unwrap()
            .tracking
            .status(),
        EffectStatus::Failed,
        "the stranded call is voided"
    );
    t.assert_trace(&[
        "session.created",
        "decision.queued",
        "decision.dispatched",
        "decision.completed",
        "turn.started",
        "decision.queued",
        "decision.dispatched",
        "decision.completed",
        "message.new",
        "llm.call.requested",
        "llm.call.dispatched",
        "llm.call.completed",
        "decision.queued",
        "decision.dispatched",
        "decision.completed",
        "message.new",
        "tool.call.requested",
        "tool.call.dispatched",
        "decision.queued",
        "decision.dispatched",
        "decision.completed",
        "head.moved",
        "call.voided",
        "branch.unloaded",
    ]);
}

fn flow_subagent_call() -> Trace {
    let mut t = Trace::create();
    t.decide(vec![], vec![]);
    t.submit("hi", Some("turn-1"));

    t.decide_appending(
        user_message("hi"),
        vec![Action::SpawnSubagent {
            session_id: None,
            agent_id: "child".to_string(),
            tool_call_id: "tc-1".to_string(),
            message: None,
            retry: RetryPolicy::no_retry(),
            mode: None,
        }],
    );
    let child = t
        .agg
        .state
        .subagent("tc-1")
        .expect("the spawn recorded a child")
        .session_id
        .clone();
    t.run(
        CommandPayload::settle(
            EffectKind::Subagent,
            "tc-1".to_string(),
            None,
            Outcome::SubagentStarted,
        ),
        &system(),
    );
    t.run(
        CommandPayload::CompleteSubagentTurn {
            session_id: child,
            agent_id: "child".to_string(),
            turn_id: "child-turn".to_string(),
            data: serde_json::json!("answer"),
            cost: Default::default(),
            token_usage: Default::default(),
            error: None,
        },
        &system(),
    );

    assert!(matches!(
        t.live_trigger(),
        Trigger::SubagentFinished { ok: true, .. }
    ));
    t.decide(vec![], vec![]);
    t.run(
        CommandPayload::RequestSubagent {
            session_id: None,
            agent_id: "child".to_string(),
            tool_call_id: "tc-2".to_string(),
            message: None,
            retry: RetryPolicy::no_retry(),
            decision_id: "d-2".to_string(),
            mode: None,
        },
        &system(),
    );
    t.run(
        CommandPayload::settle(
            EffectKind::Subagent,
            "tc-2".to_string(),
            None,
            SettleError::new(ErrorInfo::internal("spawn failed".to_string()), false),
        ),
        &system(),
    );
    t
}

#[test]
fn trace_subagent_call() {
    let t = flow_subagent_call();
    assert!(matches!(
        t.live_trigger(),
        Trigger::SubagentFinished { ok: false, .. }
    ));
    t.assert_trace(&[
        "session.created",
        "decision.queued",
        "decision.dispatched",
        "decision.completed",
        "turn.started",
        "decision.queued",
        "decision.dispatched",
        "decision.completed",
        "message.new",
        "subagent.requested",
        "subagent.dispatched",
        "subagent.started",
        "subagent.turn_completed",
        "decision.queued",
        "decision.dispatched",
        "decision.completed",
        "subagent.requested",
        "subagent.dispatched",
        "subagent.errored",
        "decision.queued",
        "decision.dispatched",
    ]);
}

fn flow_message_mid_turn() -> Trace {
    let mut t = Trace::create_with(retrying());
    t.decide(vec![], vec![]);
    t.submit("hi", Some("turn-1"));

    t.decide_appending(
        user_message("hi"),
        vec![call_llm("call-1", RetryPolicy::no_retry())],
    );
    t.run(
        CommandPayload::SendMessage {
            message: user_message("more"),
            stream: false,
            turn_id: None,
            parent_id: None,
        },
        &system(),
    );
    assert!(t.agg.state.has_pending_worker_decision());
    t
}

fn flow_direct_commands_then_cancel() -> Trace {
    let mut t = Trace::create();
    t.decide(vec![], vec![]);
    t.submit("hi", Some("turn-1"));
    t.decide_appending(user_message("hi"), vec![]);

    t.run(
        CommandPayload::RequestLlmCall {
            llm: "claude".to_string(),
            call_id: "call-1".to_string(),
            request: llm_request(),
            stream: false,
            retry: RetryPolicy::no_retry(),
            handler: LlmHandler::Server,
            format: None,
        },
        &system(),
    );
    t.run(
        CommandPayload::settle(
            EffectKind::LlmCall,
            "call-1".to_string(),
            Some(0),
            SettleError::new(
                ErrorInfo::new(ErrorCode::ProviderError, "provider down"),
                false,
            ),
        ),
        &system(),
    );
    t.decide(vec![], vec![]);

    t.run(
        CommandPayload::RequestToolCall {
            tool_call_id: "tc-1".to_string(),
            name: "a".to_string(),
            arguments: "{}".to_string(),
            retry: None,
        },
        &system(),
    );
    t.run(
        CommandPayload::settle(
            EffectKind::ToolCall,
            "tc-1".to_string(),
            Some(0),
            SettleError::new(ErrorInfo::internal("tool down".to_string()), false),
        ),
        &system(),
    );

    t.run(
        CommandPayload::FinishTurn {
            data: serde_json::json!("out"),
        },
        &system(),
    );
    t.run(CommandPayload::CompleteTurn, &system());

    t.submit("again", Some("turn-2"));
    t.run(CommandPayload::CancelSession, &system());
    t
}

#[test]
fn trace_direct_commands_then_cancel() {
    let mut t = flow_direct_commands_then_cancel();
    assert!(t.agg.state.phase.turn_id().is_none(), "the run ends");
    assert_eq!(
        t.agg.state.completed_turn_ids,
        vec!["turn-1".to_string(), "turn-2".to_string()]
    );
    assert!(
        t.agg
            .state
            .effects_of(EffectKind::Decision)
            .next()
            .is_none(),
        "no waiting work is left"
    );
    t.submit("and again", Some("turn-3"));
    assert_eq!(
        t.agg.state.phase.turn_id(),
        Some("turn-3"),
        "the session is usable"
    );
    t.names.truncate(t.names.len() - 3);
    t.assert_trace(&[
        "session.created",
        "decision.queued",
        "decision.dispatched",
        "decision.completed",
        "turn.started",
        "decision.queued",
        "decision.dispatched",
        "decision.completed",
        "message.new",
        "llm.call.requested",
        "llm.call.dispatched",
        "llm.call.errored",
        "decision.queued",
        "decision.dispatched",
        "decision.completed",
        "tool.call.requested",
        "tool.call.dispatched",
        "decision.queued",
        "decision.dispatched",
        "tool.call.errored",
        "decision.queued",
        "decision.queued",
        "turn.completed",
        "session.done",
        "turn.started",
        "decision.queued",
        "session.cancelled",
        "decision.dropped",
        "decision.dropped",
        "decision.dropped",
        "decision.dropped",
        "turn.completed",
        "session.done",
    ]);
}

#[test]
fn traces_cover_every_command_arm() {
    const ALL: &[&str] = &[
        "CancelSession",
        "CompleteConnectorSync",
        "CompleteLlmCall",
        "CompleteSubagentTurn",
        "CompleteToolCall",
        "CompleteTurn",
        "CreateSession",
        "FailConnectorSync",
        "FailLlmCall",
        "FailSubagent",
        "FailToolCall",
        "FailWorkerDecision",
        "FinishTurn",
        "Interrupt",
        "RequestLlmCall",
        "RequestSubagent",
        "RequestToolCall",
        "ResumeInterrupt",
        "SendMessage",
        "StartSubagent",
        "SubmitClientPayload",
        "SubmitWorkerDecision",
        "Wake",
    ];

    let flows: Vec<Trace> = vec![
        flow_full_loop(),
        flow_connector_gating(),
        flow_connector_failure_releases_the_gate(),
        flow_interrupt_and_resume(),
        flow_queued_turn(),
        flow_timeout_to_exhaustion(),
        flow_decision_failure(),
        flow_parallel_fan_out(),
        flow_fork_voids_stranded_work(),
        flow_subagent_call(),
        flow_message_mid_turn(),
        flow_direct_commands_then_cancel(),
    ];
    let covered: BTreeSet<&'static str> =
        flows.iter().flat_map(|t| t.arms.iter().copied()).collect();

    let missing: Vec<&&str> = ALL.iter().filter(|a| !covered.contains(**a)).collect();
    assert!(missing.is_empty(), "commands with no trace: {missing:?}");
    let unlisted: Vec<&&str> = covered.iter().filter(|a| !ALL.contains(*a)).collect();
    assert!(
        unlisted.is_empty(),
        "commands missing from ALL: {unlisted:?}"
    );
}

mod context;
