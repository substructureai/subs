use std::collections::HashMap;

use chrono::Utc;
use substructure_session::command::{CommandPayload, TurnTarget};
use substructure_session::decision::{Action, BranchConfig, BranchFrom, BranchStart, PromptSource};
use substructure_session::decision::{LlmHandler, Trigger};
use substructure_session::effects::Outcome;
use substructure_session::events::EventPayload;
use substructure_session::protocol::{
    AgentConfig, BranchMode, ClientMessage, ClientPayload, Content, DraftMessage, EffectKind,
    EffectStatus, Issuer, LlmRequest, LlmResponse, QueueMode, Requester, RetryPolicy, Role,
    SessionOwner, Subject,
};
use substructure_session::{Command, MemoryStore, Session, Settings};

const TENANT: &str = "tenant-a";

fn config() -> AgentConfig {
    serde_json::from_value(serde_json::json!({ "llm": "claude", "model": "m1" })).unwrap()
}

fn text(id: Option<&str>, role: Role, said: &str) -> DraftMessage {
    DraftMessage {
        id: id.map(str::to_string),
        role,
        content: Some(Content::Text(said.to_string())),
        tool_calls: None,
        tool_call_id: None,
        name: None,
        reasoning: None,
    }
}

fn call(id: &str, messages: Vec<DraftMessage>, branch: Option<BranchStart>) -> Action {
    Action::CallLlm {
        id: id.to_string(),
        llm: "claude".to_string(),
        request: LlmRequest {
            model: "m1".to_string(),
            messages,
            tools: None,
            temperature: None,
            max_completion_tokens: None,
            reasoning: None,
        },
        source: PromptSource::tree(),
        stream: false,
        retry: RetryPolicy::no_retry(),
        handler: LlmHandler::Server,
        format: None,
        branch,
    }
}

fn said(content: &str) -> LlmResponse {
    serde_json::from_value(serde_json::json!({
        "model": "m1",
        "content": content,
        "finish_reason": "stop",
    }))
    .unwrap()
}

struct Run {
    store: MemoryStore,
    session: Session,
    n: usize,
}

impl Run {
    fn new() -> Self {
        Self::with(config())
    }

    fn with(config: AgentConfig) -> Self {
        let store = MemoryStore::new();
        let session = Session::load(&store, "s1", TENANT, Settings::default()).unwrap();
        let mut run = Self {
            store,
            session,
            n: 0,
        };
        run.send(CommandPayload::CreateSession {
            agent_id: "agent-1".into(),
            owner: SessionOwner {
                tenant_id: TENANT.into(),
                requester: Requester::new(
                    Subject::new(Issuer::app(), "user-1".to_string()),
                    Default::default(),
                ),
                metadata: HashMap::new(),
            },
            ancestry: vec![],
            worker_retry: RetryPolicy::no_retry(),
            agent: Some(Box::new(config)),
            worker: None,
            events: None,
            state: None,
        });
        let start = run.live();
        run.decide(start, vec![], vec![]);
        run
    }

    fn send(&mut self, payload: CommandPayload) -> Vec<EventPayload> {
        self.n += 1;
        let ran = self
            .session
            .run(
                &self.store,
                Utc::now(),
                Command::system(format!("c{}", self.n), TENANT, payload),
            )
            .unwrap();
        ran.result.expect("the command is accepted");
        ran.written
            .map(|e| e.events.into_iter().map(|e| e.payload).collect())
            .unwrap_or_default()
    }

    fn live(&self) -> String {
        self.session
            .state()
            .effects_of(EffectKind::Decision)
            .find(|e| e.tracking.status() == EffectStatus::Pending)
            .map(|e| e.id.clone())
            .expect("a live decision")
    }

    fn live_trigger(&self) -> Trigger {
        let id = self.live();
        self.session
            .state()
            .worker_decision(&id)
            .unwrap()
            .trigger
            .clone()
    }

    fn decide(
        &mut self,
        decision_id: String,
        transcript: Vec<DraftMessage>,
        actions: Vec<Action>,
    ) -> Vec<EventPayload> {
        self.send(CommandPayload::SubmitWorkerDecision {
            decision_id,
            transcript,
            actions,
            state: None,
            agent: None,
        })
    }

    fn reply(&mut self, id: &str, content: &str) -> Vec<EventPayload> {
        self.send(CommandPayload::settle(
            EffectKind::LlmCall,
            id,
            Some(0),
            Outcome::Llm(Box::new(said(content))),
        ))
    }

    fn path(&self, branch: Option<&str>) -> Vec<DraftMessage> {
        self.session
            .state()
            .at_scope(branch)
            .transcript()
            .into_iter()
            .map(DraftMessage::from)
            .collect()
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

#[test]
fn a_finished_draft_is_promoted_from_the_log_and_a_reload_agrees() {
    let mut run = Run::new();
    run.send(CommandPayload::SubmitClientPayload {
        payload: ClientPayload::Message(ClientMessage {
            message: text(Some("u1"), Role::User, "Write me a haiku."),
            stream: false,
        }),
        turn: TurnTarget::Open("turn-1".into()),
        queue: QueueMode::Refuse,
        metadata: Default::default(),
    });
    let d = run.live();
    run.decide(
        d,
        vec![text(Some("u1"), Role::User, "Write me a haiku.")],
        vec![
            call("a1", vec![], None),
            call(
                "b1",
                vec![text(None, Role::User, "Make it about the sea.")],
                Some(BranchStart {
                    id: "draft-b".into(),
                    from: BranchFrom::Head,
                    agent: BranchConfig::Inherit,
                    name: None,
                    mode: BranchMode::Blocking,
                }),
            ),
        ],
    );

    run.reply("b1", "Waves fold into foam");
    let d = run.live();
    let mut branch = run.path(Some("draft-b"));
    branch.push(text(Some("b1"), Role::Assistant, "Waves fold into foam"));
    let events = run.decide(
        d,
        branch,
        vec![Action::Done {
            data: serde_json::json!("Waves fold into foam"),
        }],
    );
    assert!(events
        .iter()
        .any(|e| matches!(e, EventPayload::BranchUnloaded(_))));
    assert!(matches!(run.live_trigger(), Trigger::BranchFinished { .. }));
    let d = run.live();
    run.decide(d, run.path(None), vec![]);

    run.reply("a1", "Leaves drift on the pond");
    let d = run.live();
    let events = run.decide(
        d,
        vec![],
        vec![
            Action::PromoteBranch {
                id: "draft-b".into(),
            },
            Action::Done {
                data: serde_json::Value::Null,
            },
        ],
    );
    assert!(events
        .iter()
        .any(|e| matches!(e, EventPayload::BranchLoaded(_))));
    assert_eq!(
        texts(&run.path(None)),
        vec![
            "Write me a haiku.",
            "Make it about the sea.",
            "Waves fold into foam"
        ]
    );

    let reloaded = Session::load(&run.store, "s1", TENANT, Settings::default()).unwrap();
    assert_eq!(reloaded.state().head_id, run.session.state().head_id);
    assert_eq!(
        serde_json::to_value(&reloaded.state().branches).unwrap(),
        serde_json::to_value(&run.session.state().branches).unwrap()
    );
}

#[test]
fn a_session_loaded_from_an_old_snapshot_drops_the_prompts_of_settled_calls() {
    let mut run = Run::new();
    run.send(CommandPayload::SubmitClientPayload {
        payload: ClientPayload::Message(ClientMessage {
            message: text(Some("u1"), Role::User, "hi"),
            stream: false,
        }),
        turn: TurnTarget::Open("turn-1".into()),
        queue: QueueMode::Refuse,
        metadata: Default::default(),
    });
    let d = run.live();
    run.decide(
        d,
        vec![text(Some("u1"), Role::User, "hi")],
        vec![call("a1", vec![], None)],
    );
    run.reply("a1", "hello");

    let mut old = run.session.state().clone();
    let settled = old
        .effects
        .values_mut()
        .find(|e| e.id == "a1")
        .and_then(|e| e.llm_mut())
        .unwrap();
    assert!(settled.prompt.is_empty());
    settled.prompt = vec![serde_json::from_value(
        serde_json::json!({ "id": "a1:0", "role": "user", "content": "hi" }),
    )
    .unwrap()];
    old.settle_prompts();
    assert!(old.llm_call("a1").unwrap().prompt.is_empty());
    assert_eq!(
        serde_json::to_value(&old).unwrap(),
        serde_json::to_value(run.session.state()).unwrap(),
        "an old snapshot settles to what replaying it now gives"
    );
}

fn calling(id: &str, arguments: serde_json::Value) -> LlmResponse {
    serde_json::from_value(serde_json::json!({
        "model": "m1",
        "tool_calls": [{
            "id": id,
            "type": "function",
            "function": { "name": "researcher", "arguments": arguments.to_string() },
        }],
        "finish_reason": "tool_calls",
    }))
    .unwrap()
}

fn spawn(call: &str, task: &str, session: Option<&str>) -> Action {
    Action::SpawnSubagent {
        session_id: session.map(str::to_string),
        agent_id: "researcher".into(),
        tool_call_id: call.into(),
        message: Some(text(None, Role::User, task)),
        retry: RetryPolicy::no_retry(),
        mode: None,
        config: None,
    }
}

fn assistant(id: &str, response: &LlmResponse) -> DraftMessage {
    DraftMessage {
        id: Some(id.to_string()),
        role: Role::Assistant,
        content: response.content.clone().map(Content::Text),
        tool_calls: (!response.tool_calls.is_empty()).then(|| response.tool_calls.clone()),
        tool_call_id: None,
        name: None,
        reasoning: None,
    }
}

#[test]
fn a_subagent_on_a_branch_goes_on_from_the_log_where_it_left_off() {
    let config = serde_json::from_value(serde_json::json!({
        "llm": "claude",
        "model": "m1",
        "subagents": [{ "id": "researcher", "branch": "head" }],
    }))
    .unwrap();
    let mut run = Run::with(config);
    run.send(CommandPayload::SubmitClientPayload {
        payload: ClientPayload::Message(ClientMessage {
            message: text(Some("u1"), Role::User, "why did it fail?"),
            stream: false,
        }),
        turn: TurnTarget::Open("turn-1".into()),
        queue: QueueMode::Refuse,
        metadata: Default::default(),
    });
    let d = run.live();
    run.decide(d, run.path(None), vec![call("c1", vec![], None)]);

    let asked = calling("tc-r", serde_json::json!({ "message": "dig" }));
    run.send(CommandPayload::settle(
        EffectKind::LlmCall,
        "c1",
        Some(0),
        Outcome::Llm(Box::new(asked.clone())),
    ));
    let d = run.live();
    let mut view = run.path(None);
    view.push(assistant("c1", &asked));
    run.decide(d, view, vec![spawn("tc-r", "dig", None)]);
    let branch = run
        .session
        .state()
        .subagent("tc-r")
        .unwrap()
        .session_id
        .clone();
    assert!(matches!(run.live_trigger(), Trigger::BranchStarted { .. }));
    let d = run.live();
    run.decide(d, run.path(Some(&branch)), vec![call("b1", vec![], None)]);

    run.reply("b1", "Three causes.");
    let d = run.live();
    let mut view = run.path(Some(&branch));
    view.push(text(Some("b1"), Role::Assistant, "Three causes."));
    run.decide(
        d,
        view,
        vec![Action::Done {
            data: serde_json::json!("Three causes."),
        }],
    );
    assert!(matches!(
        run.live_trigger(),
        Trigger::SubagentFinished { ok: true, .. }
    ));
    let d = run.live();
    let mut view = run.path(None);
    view.push(DraftMessage {
        tool_call_id: Some("tc-r".into()),
        ..text(None, Role::Tool, "Three causes.")
    });
    run.decide(d, view, vec![call("c2", vec![], None)]);
    assert!(
        !run.session.state().has_node("b1"),
        "the finished branch is unloaded"
    );

    let again = calling(
        "tc-r2",
        serde_json::json!({ "message": "And the fix?", "session": branch }),
    );
    run.send(CommandPayload::settle(
        EffectKind::LlmCall,
        "c2",
        Some(0),
        Outcome::Llm(Box::new(again.clone())),
    ));
    let d = run.live();
    let mut view = run.path(None);
    view.push(assistant("c2", &again));
    let events = run.decide(d, view, vec![spawn("tc-r2", "And the fix?", Some(&branch))]);
    assert!(events
        .iter()
        .any(|e| matches!(e, EventPayload::BranchLoaded(_))));
    let path = texts(&run.path(Some(&branch)));
    assert!(path.contains(&"Three causes.".to_string()), "{path:?}");
    assert_eq!(path.last().map(String::as_str), Some("And the fix?"));
    assert!(matches!(run.live_trigger(), Trigger::BranchStarted { .. }));
}
