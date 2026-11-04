use super::*;
use crate::events::TurnJoined;
use crate::protocol::{DecisionAction, DecisionTrigger};

fn join_command(message: DraftMessage, turn_id: &str, metadata: &[(&str, &str)]) -> CommandPayload {
    CommandPayload::SubmitClientPayload {
        payload: ClientPayload::Message(ClientMessage {
            message,
            stream: false,
        }),
        turn: TurnTarget::Open(turn_id.to_string()),
        queue: QueueMode::Join,
        metadata: meta(metadata),
    }
}

fn tool_message(id: &str, text: &str) -> DraftMessage {
    DraftMessage {
        id: None,
        role: Role::Tool,
        content: Some(Content::Text(text.to_string())),
        tool_calls: None,
        tool_call_id: Some(id.to_string()),
        name: Some("render".to_string()),
        reasoning: None,
    }
}

fn result(id: &str) -> Action {
    Action::ToolResult {
        id: id.to_string(),
        attempt: Some(0),
        result: StoredResult::text("rendered".to_string()),
    }
}

impl Trace {
    fn join(&mut self, message: DraftMessage, turn_id: &str, metadata: &[(&str, &str)]) {
        self.run(join_command(message, turn_id, metadata), &frontend());
    }

    fn joins(&self) -> Vec<Vec<String>> {
        self.agg
            .state
            .effects_of(EffectKind::Decision)
            .filter_map(|e| match &e.decision()?.trigger {
                Trigger::ClientMessage {
                    messages,
                    join: true,
                    ..
                } => Some(texts(messages)),
                _ => None,
            })
            .collect()
    }

    fn asked(&self) -> crate::request::WorkerDecisionRequest {
        let id = self.live_decision();
        self.agg
            .open_work()
            .into_iter()
            .find_map(|w| match w {
                crate::Work::AskWorker { work, request, .. } if work.id == id => Some(*request),
                _ => None,
            })
            .expect("the worker is asked")
    }

    fn proposed(&self) -> Vec<DecisionAction> {
        self.asked().proposed.actions
    }

    fn asked_to_join(&self) -> bool {
        matches!(
            self.asked().trigger,
            DecisionTrigger::ClientTranscript { join: true, .. }
        )
    }

    fn calls_model(&self) -> bool {
        self.proposed()
            .iter()
            .any(|a| matches!(a, DecisionAction::CallLlm { .. }))
    }

    fn joined(&mut self, message: DraftMessage, actions: Vec<Action>) -> Vec<TurnJoined> {
        let mut transcript = self.recorded();
        transcript.push(message);
        let decision_id = self.live_decision();
        self.run(
            CommandPayload::SubmitWorkerDecision {
                decision_id,
                transcript,
                actions,
                state: None,
                agent: None,
            },
            &system(),
        )
        .into_iter()
        .filter_map(|e| match e {
            EventPayload::TurnJoined(j) => Some(j),
            _ => None,
        })
        .collect()
    }

    fn answer(&mut self, call: &str, tool_calls: Vec<ToolCall>) {
        self.run(
            CommandPayload::settle(
                EffectKind::LlmCall,
                call.to_string(),
                Some(0),
                Outcome::Llm(Box::new(llm_response(tool_calls))),
            ),
            &system(),
        );
    }
}

fn reply(call: &str, text: &str, tool_calls: Vec<ToolCall>) -> DraftMessage {
    DraftMessage {
        id: Some(call.to_string()),
        role: Role::Assistant,
        content: (!text.is_empty()).then(|| Content::Text(text.to_string())),
        tool_calls: (!tool_calls.is_empty()).then_some(tool_calls),
        tool_call_id: None,
        name: None,
        reasoning: None,
    }
}

fn asking(text: &str) -> Trace {
    let mut t = Trace::create();
    t.decide(vec![], vec![]);
    t.submit(text, Some("turn-1"));
    t.decide_appending(
        user_message(text),
        vec![call_llm("call-1", RetryPolicy::no_retry())],
    );
    t
}

#[test]
fn a_joined_message_waits_for_the_running_work_and_runs_in_the_turn() {
    let mut t = at_tool_execute();
    t.join(with_id("m1", "also make it blue"), "turn-2", &[]);
    assert_eq!(t.joins(), vec![vec!["also make it blue".to_string()]]);
    assert!(
        matches!(t.live_trigger(), Trigger::ToolExecute { .. }),
        "the running tool keeps the decision"
    );

    t.decide(vec![], vec![result("tc-1")]);
    assert!(
        matches!(t.live_trigger(), Trigger::ToolFinished { .. }),
        "the turn's own step goes first"
    );
    assert!(
        t.proposed().is_empty(),
        "the finished tool leaves the next model call to the joined message"
    );
    assert!(t.asked().joining, "the worker is told a message waits");

    let joined = t.joined(tool_message("tc-1", "rendered"), vec![]);
    assert!(matches!(
        t.live_trigger(),
        Trigger::ClientMessage { join: true, .. }
    ));
    assert_eq!(t.live_turn().as_deref(), Some("turn-1"));
    assert!(t.asked_to_join(), "the worker sees the message joins");
    assert!(!t.asked().joining);
    assert!(matches!(
        joined.as_slice(),
        [TurnJoined { turn_id, next_turn_id: Some(next), .. }] if turn_id == "turn-1" && next == "turn-2"
    ));

    t.decide_appending(
        with_id("m1", "also make it blue"),
        vec![call_llm("call-2", RetryPolicy::no_retry())],
    );
    assert_eq!(
        texts(&t.recorded()),
        vec!["render it", "", "rendered", "also make it blue"],
        "the message lands after the tool's result"
    );
    assert_eq!(t.agg.state.phase.active_turn_id(), Some("turn-1"));
    assert_eq!(t.started.len(), 1, "no turn starts for it");
}

#[test]
fn a_finished_tool_calls_the_model_when_nothing_joins() {
    let mut t = at_tool_execute();
    t.decide(vec![], vec![result("tc-1")]);
    assert!(matches!(
        t.proposed().as_slice(),
        [DecisionAction::CallLlm { .. }]
    ));
    assert!(!t.asked().joining);
}

#[test]
fn a_joined_message_on_an_idle_session_starts_a_turn() {
    let mut t = Trace::create();
    t.decide(vec![], vec![]);
    t.join(user_message("hi"), "turn-1", &[("ref", "m1")]);
    assert_eq!(t.agg.state.phase.active_turn_id(), Some("turn-1"));
    assert!(matches!(
        t.live_trigger(),
        Trigger::ClientMessage { join: false, .. }
    ));
    assert_eq!(
        t.started,
        vec![("turn-1".to_string(), meta(&[("ref", "m1")]))]
    );
}

#[test]
fn a_joined_message_keeps_the_turn_past_the_models_final_answer() {
    let mut t = asking("hi");
    t.join(user_message("one more thing"), "turn-2", &[("ref", "m2")]);
    t.answer("call-1", vec![]);
    assert!(matches!(t.live_trigger(), Trigger::LlmFinished { .. }));
    assert!(
        t.proposed().is_empty(),
        "the final answer does not end the turn while a message waits"
    );
    assert!(t.asked().joining);

    let joined = t.joined(reply("call-1", "hello", vec![]), vec![]);
    assert!(t.agg.state.completed_turn_ids.is_empty());
    assert!(matches!(
        t.live_trigger(),
        Trigger::ClientMessage { join: true, .. }
    ));
    assert_eq!(t.live_turn().as_deref(), Some("turn-1"));
    assert_eq!(t.started.len(), 1, "no turn starts for it");
    assert!(matches!(
        joined.as_slice(),
        [TurnJoined { metadata, .. }] if *metadata == meta(&[("ref", "m2")])
    ));

    t.decide_appending(
        user_message("one more thing"),
        vec![call_llm("call-2", RetryPolicy::no_retry())],
    );
    assert_eq!(
        texts(&t.recorded()),
        vec!["hi", "hello", "one more thing"],
        "the message lands after the answer"
    );
}

#[test]
fn a_worker_that_ends_the_turn_sends_a_joined_message_to_the_next_turn() {
    let mut t = asking("hi");
    t.join(user_message("one more thing"), "turn-2", &[("ref", "m2")]);
    t.answer("call-1", vec![]);

    t.decide(vec![], vec![done()]);
    assert!(matches!(t.live_trigger(), Trigger::TurnFinished { .. }));
    assert_eq!(t.joins().len(), 1, "the ending turn keeps the message");

    t.decide(vec![], vec![done()]);
    assert_eq!(t.agg.state.completed_turn_ids, vec!["turn-1".to_string()]);
    assert!(matches!(
        t.live_trigger(),
        Trigger::ClientMessage { join: true, .. }
    ));
    assert_eq!(t.live_turn().as_deref(), Some("turn-2"));
    assert!(
        !t.asked_to_join(),
        "the worker sees a message that opens its own turn"
    );
    assert_eq!(
        t.started.last(),
        Some(&("turn-2".to_string(), meta(&[("ref", "m2")])))
    );
    assert!(!t.names.iter().any(|n| n == "turn.joined"));
}

#[test]
fn a_worker_that_calls_the_model_anyway_keeps_the_joined_message_in_the_turn() {
    let mut t = at_tool_execute();
    t.join(user_message("also blue"), "turn-2", &[]);
    t.decide(vec![], vec![result("tc-1")]);
    t.decide_appending(
        tool_message("tc-1", "rendered"),
        vec![call_llm("call-2", RetryPolicy::no_retry())],
    );
    assert!(
        !t.agg.state.has_pending_worker_decision(),
        "the message waits for the model call"
    );

    t.answer("call-2", vec![]);
    assert!(t.proposed().is_empty());
    t.decide_appending(reply("call-2", "done", vec![]), vec![]);
    assert!(matches!(
        t.live_trigger(),
        Trigger::ClientMessage { join: true, .. }
    ));
    assert_eq!(t.live_turn().as_deref(), Some("turn-1"));
}

#[test]
fn a_joined_message_waits_for_every_parallel_call() {
    let mut t = asking("render both");
    let calls = vec![tool_call("tc-1", "render"), tool_call("tc-2", "render")];
    t.answer("call-1", calls.clone());
    t.decide_appending(
        reply("call-1", "", calls),
        vec![call_tool("tc-1", "render"), call_tool("tc-2", "render")],
    );
    t.join(user_message("and frame them"), "turn-2", &[]);

    let mut finished = Vec::new();
    loop {
        match t.live_trigger() {
            Trigger::ToolExecute { id, .. } => t.decide(vec![], vec![result(&id)]),
            Trigger::ToolFinished { id, .. } => {
                assert!(
                    !t.calls_model(),
                    "{id} leaves the model call to the message"
                );
                t.decide_appending(tool_message(&id, "rendered"), vec![]);
                finished.push(id);
            }
            Trigger::ClientMessage { join: true, .. } => break,
            other => panic!("unexpected {other:?}"),
        }
    }
    finished.sort();
    assert_eq!(finished, vec!["tc-1", "tc-2"]);
    assert_eq!(t.live_turn().as_deref(), Some("turn-1"));

    t.decide_appending(user_message("and frame them"), vec![]);
    assert_eq!(
        texts(&t.recorded()),
        vec!["render both", "", "rendered", "rendered", "and frame them"],
        "the message lands after both results"
    );
}

#[test]
fn a_joined_message_waits_for_a_subagent() {
    let mut t = asking("ask the child");
    let calls = vec![tool_call("tc-1", "child")];
    t.answer("call-1", calls.clone());
    t.decide_appending(
        reply("call-1", "", calls),
        vec![Action::SpawnSubagent {
            session_id: None,
            agent_id: "child".to_string(),
            tool_call_id: "tc-1".to_string(),
            message: None,
            retry: RetryPolicy::no_retry(),
            mode: None,
        }],
    );
    t.join(user_message("ask it briefly"), "turn-2", &[]);
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
    assert!(!t.agg.state.has_pending_worker_decision());
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
    assert!(matches!(t.live_trigger(), Trigger::SubagentFinished { .. }));
    assert!(!t.calls_model());
    assert!(t.asked().joining);

    t.decide_appending(tool_message("tc-1", "answer"), vec![]);
    assert!(matches!(
        t.live_trigger(),
        Trigger::ClientMessage { join: true, .. }
    ));
    assert_eq!(t.live_turn().as_deref(), Some("turn-1"));
}

#[test]
fn a_joined_message_waits_for_the_whole_script() {
    let mut t = Trace::create();
    t.decide_with(
        vec![],
        vec![],
        Some(AgentConfig {
            codemode: Some(crate::codemode::Language::Javascript),
            tools: vec![AgentTool {
                name: "weather".to_string(),
                description: "d".to_string(),
                input: None,
                output: None,
                handler: None,
                defer: None,
            }],
            ..config()
        }),
    );
    t.submit("weather?", Some("turn-1"));
    t.decide_appending(
        user_message("weather?"),
        vec![call_llm("call-1", RetryPolicy::no_retry())],
    );
    let code = serde_json::json!({ "code": "..." }).to_string();
    let run = ToolCall {
        function: ToolCallFunction {
            name: crate::protocol::RUN_CODE.to_string(),
            arguments: code.clone(),
        },
        ..tool_call("tc-1", "")
    };
    t.answer("call-1", vec![run.clone()]);
    t.decide_appending(
        reply("call-1", "", vec![run]),
        vec![Action::CallTool {
            id: "tc-1".to_string(),
            name: crate::protocol::RUN_CODE.to_string(),
            arguments: code,
            retry: None,
        }],
    );
    t.join(user_message("in Celsius"), "turn-2", &[]);

    t.run(
        CommandPayload::settle(
            EffectKind::ToolCall,
            "tc-1".to_string(),
            Some(0),
            Outcome::Script {
                known: 0,
                calls: vec![crate::events::ScriptCall {
                    name: "weather".to_string(),
                    arguments: "{}".to_string(),
                }],
                cpu_ms: 1,
                file_bytes: 0,
                skill_files: None,
            },
        ),
        &system(),
    );
    let Trigger::CodemodeCalls { tool_calls, .. } = t.live_trigger() else {
        panic!("the script asks about its call");
    };
    let weather = tool_calls[0].id.clone();
    t.decide(
        vec![],
        vec![Action::CallTool {
            id: weather.clone(),
            name: "weather".to_string(),
            arguments: "{}".to_string(),
            retry: None,
        }],
    );
    assert!(matches!(t.live_trigger(), Trigger::ToolExecute { .. }));
    t.decide(vec![], vec![result(&weather)]);
    assert!(
        !t.agg.state.has_pending_worker_decision(),
        "the message waits while the script runs again"
    );
    assert_eq!(t.joins().len(), 1);

    t.run(
        CommandPayload::settle(
            EffectKind::ToolCall,
            "tc-1".to_string(),
            Some(0),
            Outcome::Tool {
                result: StoredResult::text("3°C".to_string()),
            },
        ),
        &system(),
    );
    assert!(matches!(t.live_trigger(), Trigger::ToolFinished { .. }));
    assert!(!t.calls_model());

    t.decide_appending(tool_message("tc-1", "3°C"), vec![]);
    assert!(matches!(
        t.live_trigger(),
        Trigger::ClientMessage { join: true, .. }
    ));
    assert_eq!(t.live_turn().as_deref(), Some("turn-1"));
}

#[test]
fn a_joined_message_that_fails_fails_the_turn() {
    let mut t = at_tool_execute();
    t.join(user_message("also blue"), "turn-2", &[]);
    t.decide(vec![], vec![result("tc-1")]);
    t.decide_appending(tool_message("tc-1", "rendered"), vec![]);
    let joined = t.live_decision();
    t.run(
        CommandPayload::settle(
            EffectKind::Decision,
            joined,
            None,
            SettleError::new(ErrorInfo::internal("worker down".to_string()), false),
        ),
        &system(),
    );
    assert_eq!(t.agg.state.completed_turn_ids, vec!["turn-1".to_string()]);
    assert!(t.joins().is_empty());
    assert!(!t.agg.state.has_pending_worker_decision());
}

#[test]
fn a_joined_message_while_a_turn_ends_runs_as_the_next_turn() {
    let mut t = Trace::create();
    t.decide(vec![], vec![]);
    t.submit("hi", Some("turn-1"));
    t.decide_appending(user_message("hi"), vec![done()]);
    assert!(t.agg.state.phase.finalizing().is_some());

    t.join(user_message("later"), "turn-2", &[]);
    assert_eq!(
        t.agg.state.phase.turn_id(),
        Some("turn-1"),
        "turn-1 still ends"
    );

    t.decide(vec![], vec![done()]);
    assert_eq!(t.agg.state.completed_turn_ids, vec!["turn-1".to_string()]);
    assert_eq!(t.live_turn().as_deref(), Some("turn-2"));
}

#[test]
fn a_failed_turn_keeps_a_joined_message_for_the_next_turn() {
    let mut t = at_tool_execute();
    t.join(user_message("still there?"), "turn-2", &[]);
    t.decide(
        vec![],
        vec![Action::Fail {
            error: ErrorInfo::internal("boom".to_string()),
        }],
    );
    assert_eq!(t.agg.state.completed_turn_ids, vec!["turn-1".to_string()]);
    assert_eq!(t.live_turn().as_deref(), Some("turn-2"));
}

#[test]
fn joined_messages_wait_as_one_and_a_resent_one_is_kept_once() {
    let mut t = at_tool_execute();
    t.join(
        with_id("m1", "blue"),
        "turn-2",
        &[("ref", "m1"), ("room", "r1")],
    );
    t.join(with_id("m1", "blue"), "turn-3", &[]);
    t.join(with_id("m2", "and round"), "turn-4", &[("ref", "m2")]);
    assert_eq!(
        t.joins(),
        vec![vec!["blue".to_string(), "and round".to_string()]],
        "the messages wait in one decision"
    );

    t.decide(vec![], vec![result("tc-1")]);
    t.decide_appending(tool_message("tc-1", "rendered"), vec![]);
    let decision = t.live_decision();
    let turn = t
        .agg
        .state
        .worker_decision(&decision)
        .and_then(|d| d.turn.as_deref())
        .cloned()
        .expect("the decision runs in a turn");
    assert_eq!(turn.id, "turn-1");
    assert_eq!(
        turn.metadata,
        meta(&[("ref", "m2"), ("room", "r1")]),
        "a later key wins"
    );

    t.join(with_id("m2", "and round"), "turn-5", &[]);
    assert!(
        t.joins().len() == 1,
        "a message the turn holds is not added"
    );
    t.decide_appending(with_id("m1", "blue"), vec![]);
    t.join(with_id("m1", "blue"), "turn-6", &[]);
    assert!(t.joins().is_empty(), "a recorded message is not added");
}

#[test]
fn a_joined_message_is_refused_while_the_head_is_parked() {
    let mut t = at_tool_execute();
    t.run(
        CommandPayload::Interrupt {
            interrupt_id: "int-1".to_string(),
            reason: "approval".to_string(),
            payload: serde_json::Value::Null,
        },
        &frontend(),
    );
    let err = t.reject(
        join_command(user_message("again"), "turn-2", &[]),
        &frontend(),
    );
    assert!(matches!(err, SessionError::SessionInterrupted), "{err:?}");
}

#[test]
fn a_joined_message_waits_out_an_interrupt_raised_after_it() {
    let mut t = at_tool_execute();
    t.join(user_message("blue"), "turn-2", &[]);
    t.run(
        CommandPayload::Interrupt {
            interrupt_id: "int-1".to_string(),
            reason: "approval".to_string(),
            payload: serde_json::Value::Null,
        },
        &frontend(),
    );
    t.run(
        CommandPayload::settle(
            EffectKind::ToolCall,
            "tc-1".to_string(),
            Some(0),
            Outcome::Tool {
                result: StoredResult::text("rendered".to_string()),
            },
        ),
        &system(),
    );
    assert!(
        !t.agg.state.has_pending_worker_decision(),
        "the parked head dispatches nothing"
    );
    assert_eq!(t.joins().len(), 1);

    t.run(
        CommandPayload::ResumeInterrupt {
            interrupt_id: "int-1".to_string(),
            payload: serde_json::json!({"approved": true}),
        },
        &frontend(),
    );
    assert!(matches!(t.live_trigger(), Trigger::InterruptResumed { .. }));
}

#[test]
fn a_resume_leaves_the_model_call_to_a_joined_message() {
    let mut t = Trace::create();
    t.decide_with(vec![], vec![], Some(config()));
    t.submit("render it", Some("turn-1"));
    t.decide_appending(
        user_message("render it"),
        vec![call_llm("call-1", RetryPolicy::no_retry())],
    );
    t.join(user_message("blue"), "turn-2", &[]);
    t.run(
        CommandPayload::Interrupt {
            interrupt_id: "int-1".to_string(),
            reason: "pause".to_string(),
            payload: serde_json::Value::Null,
        },
        &frontend(),
    );
    t.run(
        CommandPayload::ResumeInterrupt {
            interrupt_id: "int-1".to_string(),
            payload: serde_json::Value::Null,
        },
        &frontend(),
    );
    assert!(matches!(t.live_trigger(), Trigger::InterruptResumed { .. }));
    assert!(
        !t.calls_model(),
        "the resume leaves the model call to the message"
    );
    assert!(t.asked().joining);

    t.decide(t.recorded(), vec![]);
    assert!(matches!(
        t.live_trigger(),
        Trigger::ClientMessage { join: true, .. }
    ));
    assert_eq!(t.live_turn().as_deref(), Some("turn-1"));
}

#[test]
fn a_declined_call_leaves_the_model_call_to_a_joined_message() {
    let mut t = asking("clean up");
    t.join(user_message("keep the logs"), "turn-2", &[]);
    let calls = vec![tool_call("tc-1", "delete")];
    t.answer("call-1", calls.clone());
    t.decide_appending(
        reply("call-1", "", calls),
        vec![Action::Interrupt {
            interrupt_id: format!("{}tc-1", crate::interrupts::approval::PREFIX),
            reason: "approval".to_string(),
            payload: serde_json::Value::Null,
        }],
    );
    assert_eq!(t.joins().len(), 1, "the message waits out the question");

    t.run(
        CommandPayload::ResumeInterrupt {
            interrupt_id: format!("{}tc-1", crate::interrupts::approval::PREFIX),
            payload: serde_json::json!({"approved": false}),
        },
        &frontend(),
    );
    assert!(matches!(t.live_trigger(), Trigger::InterruptResumed { .. }));
    assert!(
        !t.calls_model(),
        "the decline leaves the model call to the message"
    );
    let refusal = t
        .asked()
        .proposed
        .messages
        .last()
        .cloned()
        .expect("the decline is recorded");
    assert_eq!(refusal.tool_call_id.as_deref(), Some("tc-1"));

    t.decide(t.asked().proposed.messages, vec![]);
    assert!(matches!(
        t.live_trigger(),
        Trigger::ClientMessage { join: true, .. }
    ));
    assert_eq!(t.live_turn().as_deref(), Some("turn-1"));
}

fn mail(message: DraftMessage, turn_id: Option<&str>) -> CommandPayload {
    CommandPayload::SendMessage {
        message,
        stream: false,
        turn_id: turn_id.map(str::to_string),
        parent_id: None,
    }
}

#[test]
fn a_session_message_joins_the_running_turn_once() {
    let mut t = at_tool_execute();
    t.run(mail(with_id("m1", "from A"), Some("mail-1")), &system());
    t.run(mail(with_id("m1", "from A"), Some("mail-2")), &system());
    assert_eq!(t.joins(), vec![vec!["from A".to_string()]]);

    t.decide(vec![], vec![result("tc-1")]);
    t.decide_appending(tool_message("tc-1", "rendered"), vec![]);
    assert!(matches!(
        t.live_trigger(),
        Trigger::ClientMessage { join: true, .. }
    ));
    assert_eq!(t.live_turn().as_deref(), Some("turn-1"));
    assert_eq!(t.started.len(), 1, "no turn starts for it");
}

#[test]
fn a_session_message_with_no_turn_waits_for_the_running_call() {
    let mut t = Trace::create();
    t.decide(vec![], vec![]);
    t.submit("hi", Some("turn-1"));
    t.decide_appending(
        user_message("hi"),
        vec![call_llm("call-1", RetryPolicy::no_retry())],
    );
    t.run(mail(user_message("more"), None), &system());
    assert_eq!(t.joins(), vec![vec!["more".to_string()]]);
    assert!(
        !t.agg.state.has_pending_worker_decision(),
        "no second model call starts beside the running one"
    );
}

#[test]
fn a_session_message_on_an_idle_session_opens_its_turn() {
    let mut t = Trace::create();
    t.decide(vec![], vec![]);
    t.run(
        mail(user_message("do it"), Some("open:child:call")),
        &system(),
    );
    assert_eq!(t.agg.state.phase.active_turn_id(), Some("open:child:call"));
    assert!(matches!(
        t.live_trigger(),
        Trigger::ClientMessage {
            join: false,
            turn_id: None,
            ..
        }
    ));
}

#[test]
fn a_session_message_while_a_turn_ends_runs_as_the_next_turn() {
    let mut t = Trace::create();
    t.decide(vec![], vec![]);
    t.submit("hi", Some("turn-1"));
    t.decide_appending(user_message("hi"), vec![done()]);
    t.run(mail(user_message("later"), Some("mail-1")), &system());

    t.decide(vec![], vec![done()]);
    assert_eq!(t.agg.state.completed_turn_ids, vec!["turn-1".to_string()]);
    assert_eq!(t.live_turn().as_deref(), Some("mail-1"));
}

#[test]
fn a_session_message_is_refused_while_the_head_is_parked() {
    let mut t = at_tool_execute();
    t.run(
        CommandPayload::Interrupt {
            interrupt_id: "int-1".to_string(),
            reason: "approval".to_string(),
            payload: serde_json::Value::Null,
        },
        &frontend(),
    );
    let err = t.reject(mail(user_message("again"), Some("mail-1")), &system());
    assert!(matches!(err, SessionError::SessionInterrupted), "{err:?}");
}

#[test]
fn a_cancel_keeps_a_joined_message_for_the_next_turn() {
    let mut t = at_tool_execute();
    t.join(with_id("m1", "also blue"), "turn-2", &[]);
    t.run(
        CommandPayload::CancelSession { drop_queued: false },
        &frontend(),
    );
    assert_eq!(t.agg.state.completed_turn_ids, vec!["turn-1".to_string()]);
    assert!(matches!(
        t.live_trigger(),
        Trigger::ClientMessage { join: true, .. }
    ));
    assert_eq!(t.live_turn().as_deref(), Some("turn-2"));
}

#[test]
fn a_cancel_that_drops_queued_names_the_joined_messages() {
    let mut t = at_tool_execute();
    t.join(with_id("m1", "also blue"), "turn-2", &[]);
    let events = t.run(
        CommandPayload::CancelSession { drop_queued: true },
        &frontend(),
    );
    assert!(t.joins().is_empty());
    assert!(!t.agg.state.has_pending_worker_decision());
    assert!(
        events.iter().any(|e| matches!(e,
            EventPayload::DecisionDropped(d) if d.messages == vec!["m1".to_string()])),
        "{events:?}"
    );
}
