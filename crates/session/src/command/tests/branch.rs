use super::*;
use crate::decision::{BranchConfig, BranchFrom, BranchStart};
use crate::protocol::{BranchMode, DecisionAction, ErrorCode, ToolCall, ToolCallFunction, Usage};

fn configured() -> Session {
    create_session_with_config("sess-b", "tenant-a", "user-1", Some(agent_config("m1")))
}

fn user_turn(agg: &mut Session, id: &str, text: &str, turn: &str) -> Vec<EventPayload> {
    dispatch(
        agg,
        CommandPayload::SubmitClientPayload {
            payload: ClientPayload::Message(ClientMessage {
                message: node_msg(id, Role::User, text),
                stream: false,
            }),
            turn: TurnTarget::Open(turn.to_string()),
            queue: QueueMode::Refuse,
            metadata: Default::default(),
        },
        &system(),
    )
}

fn tree_call(id: &str) -> Action {
    Action::CallLlm {
        id: id.to_string(),
        llm: "claude".to_string(),
        request: request_with(vec![]),
        stream: false,
        retry: RetryPolicy::no_retry(),
        handler: LlmHandler::Server,
        format: None,
        branch: None,
    }
}

fn start(id: &str, from: BranchFrom, mode: BranchMode) -> BranchStart {
    BranchStart {
        id: id.to_string(),
        from,
        agent: BranchConfig::Inherit,
        name: Some(id.to_string()),
        mode,
    }
}

fn branch_call(id: &str, start: BranchStart, messages: Vec<DraftMessage>) -> Action {
    Action::CallLlm {
        id: id.to_string(),
        llm: "claude".to_string(),
        request: request_with(messages),
        stream: false,
        retry: RetryPolicy::no_retry(),
        handler: LlmHandler::Server,
        format: None,
        branch: Some(start),
    }
}

fn reply(agg: &mut Session, id: &str, response: LlmResponse) -> Vec<EventPayload> {
    let attempt = agg
        .state
        .tracking(EffectKind::LlmCall, id)
        .map(|t| t.retry.attempts);
    dispatch(
        agg,
        CommandPayload::settle(
            EffectKind::LlmCall,
            id.to_string(),
            attempt,
            Outcome::Llm(Box::new(response)),
        ),
        &system(),
    )
}

fn said(text: &str, input_tokens: u64) -> LlmResponse {
    LlmResponse {
        usage: Some(Usage::new(input_tokens, 0, 0, 5)),
        cost: Some(rust_decimal::Decimal::new(1, 2)),
        ..llm_response(text)
    }
}

fn calling(tool: &str, call_id: &str) -> LlmResponse {
    LlmResponse {
        content: None,
        tool_calls: vec![ToolCall {
            id: call_id.to_string(),
            call_type: "function".to_string(),
            function: ToolCallFunction {
                name: tool.to_string(),
                arguments: "{}".to_string(),
            },
        }],
        finish_reason: Some("tool_calls".to_string()),
        ..llm_response("")
    }
}

fn echo(agg: &mut Session) -> Vec<EventPayload> {
    let (id, _) = live_decision(agg);
    let proposed = proposed_for(agg, &id);
    let actions = proposed.actions.into_iter().filter_map(lower).collect();
    submit_decision(agg, id, proposed.messages, actions)
}

fn lower(action: DecisionAction) -> Option<Action> {
    Some(match action {
        DecisionAction::CallLlm {
            id,
            messages,
            tools,
            model,
            ..
        } => Action::CallLlm {
            id: id.unwrap_or_else(|| uuid::Uuid::now_v7().to_string()),
            llm: "claude".to_string(),
            request: LlmRequest {
                tools,
                model: model.unwrap_or_else(|| "test-model".to_string()),
                ..request_with(messages.clone().unwrap_or_default())
            },
            stream: false,
            retry: RetryPolicy::no_retry(),
            handler: LlmHandler::Server,
            format: None,
            branch: None,
        },
        DecisionAction::CallTool {
            id,
            name,
            arguments,
            ..
        } => Action::CallTool {
            id: id.unwrap_or_default(),
            name,
            arguments: match arguments {
                serde_json::Value::String(s) => s,
                other => other.to_string(),
            },
            retry: None,
        },
        DecisionAction::ToolError { id, error, .. } => Action::ToolError {
            id: id.unwrap_or_default(),
            attempt: None,
            error: ErrorInfo::handler(error),
            retryable: false,
        },
        DecisionAction::Done { data } => Action::Done { data },
        DecisionAction::Fail { error } => Action::Fail { error },
        DecisionAction::SpawnSubagent {
            session_id,
            agent_id,
            tool_call_id,
            message,
            mode,
            ..
        } => Action::SpawnSubagent {
            config: (agent_id == "poet").then(|| {
                Box::new(AgentConfig {
                    system: Some("You write poems.".to_string()),
                    ..agent_config("m-poet")
                })
            }),
            session_id,
            agent_id,
            tool_call_id,
            message,
            retry: RetryPolicy::no_retry(),
            mode,
        },
        other => panic!("not lowered here: {other:?}"),
    })
}

fn text_of(message: &Message) -> String {
    message
        .content
        .as_ref()
        .map(Content::text_owned)
        .unwrap_or_default()
}

fn prompt_texts(agg: &Session, call: &str) -> Vec<String> {
    agg.sent
        .get(call)
        .cloned()
        .or_else(|| agg.state.outgoing(call).map(|out| out.prompt))
        .expect("the call builds a prompt")
        .iter()
        .map(text_of)
        .collect()
}

fn branch_closed(events: &[EventPayload], id: &str) -> Option<RunEnd> {
    events.iter().find_map(|e| match e {
        EventPayload::BranchFinished(c) if c.id == id => Some(c.outcome),
        _ => None,
    })
}

fn branch_finished(events: &[EventPayload]) -> Option<(Trigger, Option<String>)> {
    events.iter().find_map(|e| match e {
        EventPayload::DecisionQueued(q) if matches!(q.trigger, Trigger::BranchFinished { .. }) => {
            Some((q.trigger.clone(), q.branch.clone()))
        }
        _ => None,
    })
}

fn turn_with_side_call(mode: BranchMode) -> Session {
    let mut agg = configured();
    user_turn(&mut agg, "u1", "what is the weather in Paris?", "turn-1");
    let (d, _) = live_decision(&agg);
    submit_decision(
        &mut agg,
        d,
        vec![node_msg("u1", Role::User, "what is the weather in Paris?")],
        vec![
            tree_call("c1"),
            branch_call(
                "t1",
                start("title", BranchFrom::Head, mode),
                vec![node_msg("", Role::User, "Write a short title.")],
            ),
        ],
    );
    agg
}

#[test]
fn a_fork_reads_the_conversation_and_then_its_own_messages() {
    let agg = turn_with_side_call(BranchMode::Detached);
    let branch = agg
        .state
        .open_run(&BranchId::from("title"))
        .expect("the branch started");
    assert_eq!(branch.from.as_deref(), Some("u1"));
    assert_eq!(
        agg.state.message_tree().head_id.as_deref(),
        Some("u1"),
        "the head stays"
    );
    let own = agg
        .state
        .tree
        .message_at(agg.state.leaf(&BranchId::from("title")))
        .expect("its message is its own node");
    assert_eq!(agg.state.node_branch(&own).name(), Some("title"));

    let call = agg.state.effect(EffectKind::LlmCall, "t1").unwrap();
    assert_eq!(call.branch.name(), Some("title"));
    assert_eq!(call.asked_at.as_deref(), Some(own.as_str()));

    let chat = prompt_texts(&agg, "c1");
    let side = prompt_texts(&agg, "t1");
    assert_eq!(
        side[..chat.len()],
        chat[..],
        "the fork keeps the cached prefix"
    );
    assert_eq!(
        side.last().map(String::as_str),
        Some("Write a short title.")
    );
}

#[test]
fn a_detached_branch_reports_back_without_a_turn() {
    let mut agg = turn_with_side_call(BranchMode::Detached);
    reply(&mut agg, "c1", said("Sunny.", 100));
    let events = echo(&mut agg);
    assert!(
        events
            .iter()
            .any(|e| matches!(e, EventPayload::DecisionQueued(q) if matches!(q.trigger, Trigger::TurnFinished { .. }))),
        "the detached branch does not hold the turn: {events:?}"
    );
    echo(&mut agg);
    assert!(agg.state.open_turn().is_none(), "the turn ended");
    assert!(
        agg.state.open_run(&BranchId::from("title")).is_some(),
        "and the branch lives on"
    );

    reply(&mut agg, "t1", said("Paris weather", 120));
    let (d, _) = live_decision(&agg);
    assert_eq!(agg.state.decision_branch(&d).name(), Some("title"));
    let proposed = proposed_for(&agg, &d);
    assert!(matches!(
        proposed.actions.as_slice(),
        [DecisionAction::Done { data }] if data == &serde_json::json!("Paris weather")
    ));
    let events = echo(&mut agg);
    assert_eq!(branch_closed(&events, "title"), Some(RunEnd::Done));
    let (finished, on) = branch_finished(&events).expect("its parent hears");
    assert_eq!(on, None, "the conversation started it");
    assert!(matches!(
        finished,
        Trigger::BranchFinished { ok: true, ref data, .. } if data == &serde_json::json!("Paris weather")
    ));
    assert_eq!(
        agg.state.leaf(&BranchId::MAIN),
        Some("c1"),
        "the reply never reached the chat"
    );

    let (d, _) = live_decision(&agg);
    assert!(agg.state.worker_decision(&d).unwrap().turn.is_none());
    assert!(proposed_for(&agg, &d).actions.is_empty());
}

#[test]
fn a_blocking_branch_holds_the_turn_until_it_reports() {
    let mut agg = turn_with_side_call(BranchMode::Blocking);
    reply(&mut agg, "c1", said("Sunny.", 100));
    let (d, _) = live_decision(&agg);
    assert!(
        proposed_for(&agg, &d).actions.is_empty(),
        "the reply waits for the branch"
    );
    echo(&mut agg);
    assert!(agg.state.active_turn_id().is_some());

    reply(&mut agg, "t1", said("Paris weather", 120));
    echo(&mut agg);
    let (d, trigger) = live_decision(&agg);
    assert!(matches!(trigger, Trigger::BranchFinished { .. }));
    assert_eq!(
        agg.state
            .worker_decision(&d)
            .unwrap()
            .turn
            .as_ref()
            .map(|t| t.id.as_str()),
        Some("turn-1")
    );
    assert!(matches!(
        proposed_for(&agg, &d).actions.as_slice(),
        [DecisionAction::Done { data }] if data == &serde_json::json!("Sunny.")
    ));
    let events = echo(&mut agg);
    assert!(events
        .iter()
        .any(|e| matches!(e, EventPayload::DecisionQueued(q) if matches!(q.trigger, Trigger::TurnFinished { .. }))));
}

#[test]
fn a_turn_that_ends_takes_its_blocking_branches_with_it() {
    let mut agg = turn_with_side_call(BranchMode::Blocking);
    reply(&mut agg, "c1", said("Sunny.", 100));
    let (d, _) = live_decision(&agg);
    let view = delivered_transcript(&agg);
    let events = submit_decision(
        &mut agg,
        d,
        view,
        vec![Action::Done {
            data: serde_json::Value::Null,
        }],
    );
    assert_eq!(branch_closed(&events, "title"), Some(RunEnd::Voided));
    assert!(branch_finished(&events).is_none(), "nobody waits for it");
    assert_eq!(
        agg.state
            .tracking(EffectKind::LlmCall, "t1")
            .unwrap()
            .status(),
        EffectStatus::Failed
    );
}

#[test]
fn a_detached_branch_outlives_a_cancel() {
    let mut agg = turn_with_side_call(BranchMode::Detached);
    dispatch(
        &mut agg,
        CommandPayload::CancelSession { drop_queued: false },
        &frontend(),
    );
    assert!(agg.state.open_turn().is_none());
    assert!(agg.state.open_run(&BranchId::from("title")).is_some());
    assert!(agg
        .state
        .tracking(EffectKind::LlmCall, "t1")
        .unwrap()
        .is_unsettled());
    assert_eq!(
        agg.state
            .tracking(EffectKind::LlmCall, "c1")
            .unwrap()
            .status(),
        EffectStatus::Failed
    );
}

#[test]
fn a_branch_runs_its_tools_on_itself() {
    let mut agg = turn_with_side_call(BranchMode::Detached);
    reply(&mut agg, "c1", said("Sunny.", 100));
    echo(&mut agg);
    echo(&mut agg);

    reply(&mut agg, "t1", calling("lookup", "tc-b"));
    echo(&mut agg);
    let tool = agg.state.effect(EffectKind::ToolCall, "tc-b").unwrap();
    assert_eq!(tool.branch.name(), Some("title"));
    while live_decision_kind(&agg).is_some_and(|t| matches!(t, Trigger::ToolExecute { .. })) {
        let (d, _) = live_decision(&agg);
        submit_decision(&mut agg, d, vec![], vec![tool_result_action("tc-b")]);
    }
    let (d, trigger) = live_decision(&agg);
    assert!(matches!(trigger, Trigger::ToolFinished { .. }));
    assert_eq!(agg.state.decision_branch(&d).name(), Some("title"));
    echo(&mut agg);

    let next = agg
        .state
        .effects_of(EffectKind::LlmCall)
        .find(|e| e.id != "c1" && e.id != "t1")
        .expect("the branch calls the model again");
    assert_eq!(next.branch.name(), Some("title"));
    let leaf = agg
        .state
        .leaf(&BranchId::from("title"))
        .unwrap()
        .to_string();
    assert_eq!(
        next.asked_at,
        agg.state.tree.message_at(Some(&leaf)),
        "it reads the branch, tool result included"
    );
    assert_eq!(agg.state.leaf(&BranchId::MAIN), Some("c1"));
}

fn live_decision_kind(agg: &Session) -> Option<Trigger> {
    agg.state
        .effects_of(EffectKind::Decision)
        .filter(|d| d.tracking.status() == EffectStatus::Pending)
        .find_map(|e| e.decision().map(|d| d.trigger.clone()))
}

#[test]
fn a_side_call_neither_counts_as_the_conversations_read_nor_its_turn() {
    let mut agg = turn_with_side_call(BranchMode::Detached);
    reply(&mut agg, "c1", said("Sunny.", 9_000));
    reply(&mut agg, "t1", said("Paris weather", 50));
    let read = agg
        .state
        .last_read
        .clone()
        .expect("the conversation's read");
    assert_eq!((read.call_id.as_str(), read.tokens), ("c1", 9_000));
    assert_eq!(
        agg.state.turn.as_ref().unwrap().calls.llm.count,
        1,
        "the detached call is not the turn's"
    );
    assert_eq!(
        agg.state.turn.as_ref().unwrap().cost,
        rust_decimal::Decimal::new(1, 2)
    );
    assert_eq!(
        agg.state.cost,
        rust_decimal::Decimal::new(2, 2),
        "the session pays for both"
    );
}

#[test]
fn a_root_branch_with_no_config_sends_its_messages_as_written() {
    let mut agg = configured();
    user_turn(&mut agg, "u1", "hi", "turn-1");
    let (d, _) = live_decision(&agg);
    submit_decision(
        &mut agg,
        d,
        vec![node_msg("u1", Role::User, "hi")],
        vec![branch_call(
            "j1",
            start("judge", BranchFrom::Root, BranchMode::Blocking),
            vec![
                node_msg("", Role::System, "You judge."),
                node_msg("", Role::User, "Is 2+2=4?"),
            ],
        )],
    );
    assert_eq!(prompt_texts(&agg, "j1"), vec!["You judge.", "Is 2+2=4?"]);
    assert_eq!(
        agg.state.on(&BranchId::from(Some("judge"))).config(),
        None,
        "a root runs with no config"
    );
}

#[test]
fn a_fork_reads_the_config_as_it_was_when_it_started() {
    let mut agg = turn_with_side_call(BranchMode::Detached);
    let (d, _) = {
        reply(&mut agg, "c1", said("Sunny.", 100));
        live_decision(&agg)
    };
    let transcript = delivered_transcript(&agg)
        .into_iter()
        .chain([node_msg("c1", Role::Assistant, "Sunny.")])
        .collect();
    dispatch(
        &mut agg,
        CommandPayload::SubmitWorkerDecision {
            decision_id: d,
            transcript,
            actions: vec![],
            state: None,
            agent: Some(agent_config("m2")),
        },
        &machine(),
    );
    assert_eq!(
        agg.state.on(&BranchId::MAIN).config().map(|c| c.model),
        Some("m2".to_string())
    );
    assert_eq!(
        agg.state
            .on(&BranchId::from(Some("title")))
            .config()
            .map(|c| c.model),
        Some("m1".to_string()),
        "a later write on the conversation does not reach the branch"
    );
}

#[test]
fn the_conversation_cannot_land_on_an_open_branch() {
    let mut agg = turn_with_side_call(BranchMode::Detached);
    let own = agg
        .state
        .leaf(&BranchId::from("title"))
        .unwrap()
        .to_string();
    reply(&mut agg, "c1", said("Sunny.", 100));
    let (d, _) = live_decision(&agg);
    let landing: Vec<DraftMessage> = agg
        .state
        .message_tree()
        .path_to(&own)
        .into_iter()
        .map(DraftMessage::from)
        .collect();
    submit_decision(&mut agg, d, landing, vec![]);
    assert_ne!(agg.state.leaf(&BranchId::MAIN), Some(own.as_str()));
}

#[test]
fn a_branch_with_work_out_cannot_be_promoted() {
    let mut agg = turn_with_side_call(BranchMode::Blocking);
    reply(&mut agg, "c1", said("Draft A", 100));
    let (d, _) = live_decision(&agg);
    submit_decision(
        &mut agg,
        d,
        vec![],
        vec![Action::PromoteBranch {
            id: "title".to_string(),
        }],
    );
    assert_eq!(agg.state.message_tree().head_id.as_deref(), Some("u1"));
    assert!(agg.state.open_run(&BranchId::from("title")).is_some());
}

#[test]
fn an_open_branch_stays_loaded_when_the_conversation_moves_on() {
    let mut agg = turn_with_side_call(BranchMode::Detached);
    reply(&mut agg, "c1", said("Sunny.", 100));
    echo(&mut agg);
    echo(&mut agg);
    let own = agg
        .state
        .leaf(&BranchId::from("title"))
        .unwrap()
        .to_string();
    assert!(agg.state.has_node(&own));
}

#[test]
fn a_branch_decision_that_leaves_nothing_open_ends_the_branch() {
    let mut agg = turn_with_side_call(BranchMode::Detached);
    reply(&mut agg, "t1", said("Paris weather", 100));
    let (d, _) = live_decision(&agg);
    let proposed = proposed_for(&agg, &d);
    let events = submit_decision(&mut agg, d, proposed.messages, vec![]);
    assert_eq!(branch_closed(&events, "title"), Some(RunEnd::Done));
}

fn explicit(id: &str, messages: Vec<DraftMessage>) -> Action {
    Action::CallLlm {
        id: id.to_string(),
        llm: "claude".to_string(),
        request: request_with(messages),
        stream: false,
        retry: RetryPolicy::no_retry(),
        handler: LlmHandler::Server,
        format: None,
        branch: None,
    }
}

#[test]
fn an_explicit_call_runs_on_its_own_branch_as_written() {
    let mut agg = configured();
    user_turn(&mut agg, "u1", "what time is it?", "turn-1");
    let (d, _) = live_decision(&agg);
    submit_decision(
        &mut agg,
        d,
        vec![node_msg("u1", Role::User, "what time is it?")],
        vec![explicit(
            "x1",
            vec![
                node_msg("", Role::System, "my rules"),
                node_msg("u1", Role::User, "what time is it?"),
            ],
        )],
    );
    let branch = agg
        .state
        .open_run(&BranchId::from("x1"))
        .expect("its own branch");
    assert_eq!(
        branch.from, None,
        "a system message first: nothing to share"
    );
    assert!(agg.state.on(&BranchId::from("x1")).config().is_none());
    assert_eq!(prompt_texts(&agg, "x1"), ["my rules", "what time is it?"]);
    assert_eq!(agg.state.leaf(&BranchId::MAIN), Some("u1"));

    reply(&mut agg, "x1", calling("clock", "tc-1"));
    echo(&mut agg);
    while live_decision_kind(&agg).is_some_and(|t| matches!(t, Trigger::ToolExecute { .. })) {
        let (d, _) = live_decision(&agg);
        submit_decision(&mut agg, d, vec![], vec![tool_result_action("tc-1")]);
    }
    echo(&mut agg);
    let next = agg
        .state
        .effects_of(EffectKind::LlmCall)
        .find(|e| e.id != "x1")
        .map(|e| e.id.clone())
        .expect("the branch goes on");
    assert_eq!(
        prompt_texts(&agg, &next),
        ["my rules", "what time is it?", "", "result-tc-1"],
        "the worker's prompt and what followed, as written"
    );
    assert_eq!(agg.state.leaf(&BranchId::MAIN), Some("u1"));
    assert!(agg.state.placed().is_empty(), "no config told it anything");
}

#[test]
fn an_explicit_prompt_that_starts_with_the_conversation_forks_from_it() {
    let mut agg = configured();
    user_turn(&mut agg, "u1", "what time is it?", "turn-1");
    let (d, _) = live_decision(&agg);
    submit_decision(
        &mut agg,
        d,
        vec![node_msg("u1", Role::User, "what time is it?")],
        vec![explicit(
            "x1",
            vec![
                node_msg("u1", Role::User, "what time is it?"),
                node_msg("", Role::User, "Write a title."),
            ],
        )],
    );
    let branch = agg.state.open_run(&BranchId::from("x1")).unwrap();
    assert_eq!(
        branch.from.as_deref(),
        Some("u1"),
        "the shared message is not copied"
    );
    assert_eq!(
        prompt_texts(&agg, "x1"),
        ["what time is it?", "Write a title."]
    );
    let copies = agg
        .state
        .tree
        .unwritten()
        .filter_map(|n| n.message())
        .filter(|m| {
            m.content.as_ref().map(Content::text_owned).as_deref() == Some("what time is it?")
        })
        .count();
    assert_eq!(copies, 1);
}

#[test]
fn an_explicit_prompt_edited_on_the_way_is_copied_from_there() {
    let mut agg = configured();
    user_turn(&mut agg, "u1", "what time is it?", "turn-1");
    let (d, _) = live_decision(&agg);
    submit_decision(
        &mut agg,
        d,
        vec![node_msg("u1", Role::User, "what time is it?")],
        vec![explicit(
            "x1",
            vec![node_msg("u1", Role::User, "what time is it in Paris?")],
        )],
    );
    let branch = agg.state.open_run(&BranchId::from("x1")).unwrap();
    assert_eq!(branch.from, None);
    assert_eq!(prompt_texts(&agg, "x1"), ["what time is it in Paris?"]);
}

fn dispatched_fingerprint(events: &[EventPayload], id: &str) -> Option<String> {
    events.iter().find_map(|e| match e {
        EventPayload::LlmCallDispatched(d) if d.id == id => d.fingerprint.clone(),
        _ => None,
    })
}

#[test]
fn a_settled_call_keeps_no_prompt() {
    let mut agg = configured();
    user_turn(&mut agg, "u1", "what is the weather in Paris?", "turn-1");
    let (d, _) = live_decision(&agg);
    let events = submit_decision(
        &mut agg,
        d,
        vec![node_msg("u1", Role::User, "what is the weather in Paris?")],
        vec![
            tree_call("c1"),
            branch_call(
                "t1",
                start("title", BranchFrom::Head, BranchMode::Detached),
                vec![node_msg("", Role::User, "Write a short title.")],
            ),
        ],
    );
    let sent: Vec<(String, String)> = ["c1", "t1"]
        .iter()
        .map(|id| {
            (
                id.to_string(),
                dispatched_fingerprint(&events, id).expect("dispatched"),
            )
        })
        .collect();
    assert!(
        !agg.state.llm_call("c1").unwrap().prompt.is_empty(),
        "open: kept"
    );

    reply(&mut agg, "c1", said("Sunny.", 100));
    echo(&mut agg);
    echo(&mut agg);
    reply(&mut agg, "t1", said("Paris weather", 100));
    for (id, fingerprint) in sent {
        assert!(
            agg.state.llm_call(&id).is_none_or(|c| c.prompt.is_empty()),
            "{id} settled: the state keeps no prompt"
        );
        assert_eq!(
            crate::prompt_context::fingerprint(&agg.sent[&id]),
            fingerprint,
            "{id}"
        );
    }
}

fn with_subagents(branch: crate::protocol::SubagentBranch, id: &str) -> Session {
    let config = AgentConfig {
        system: Some("be brief".to_string()),
        subagents: vec![crate::protocol::Subagent {
            id: id.to_string(),
            description: "Looks into things.".to_string(),
            defer: None,
            prefix: None,
            mode: None,
            branch: Some(branch),
        }],
        ..agent_config("m1")
    };
    create_session_with_config("sess-s", "tenant-a", "user-1", Some(config))
}

fn delegating(agent: &str, call: &str, task: &str) -> LlmResponse {
    LlmResponse {
        tool_calls: vec![ToolCall {
            id: call.to_string(),
            call_type: "function".to_string(),
            function: ToolCallFunction {
                name: agent.to_string(),
                arguments: serde_json::json!({ "message": task }).to_string(),
            },
        }],
        ..calling(agent, call)
    }
}

fn delegated(agent: &str, branch: crate::protocol::SubagentBranch) -> (Session, String) {
    let mut agg = with_subagents(branch, agent);
    user_turn(&mut agg, "u1", "why did the deploy fail?", "turn-1");
    let (d, _) = live_decision(&agg);
    submit_decision(
        &mut agg,
        d,
        vec![node_msg("u1", Role::User, "why did the deploy fail?")],
        vec![tree_call("c1")],
    );
    reply(
        &mut agg,
        "c1",
        delegating(agent, "tc-r", "Find the causes."),
    );
    echo(&mut agg);
    let sa = agg
        .state
        .subagent("tc-r")
        .expect("the call started a subagent");
    let branch = sa.session_id.clone();
    assert!(sa.on_branch.is_some(), "on a branch, not a session");
    let (d, trigger) = live_decision(&agg);
    assert!(matches!(trigger, Trigger::BranchStarted { .. }));
    assert_eq!(agg.state.decision_branch(&d).name(), Some(branch.as_str()));
    echo(&mut agg);
    (agg, branch)
}

fn branch_call_id(agg: &Session, branch: &str) -> String {
    agg.state
        .effects_of(EffectKind::LlmCall)
        .find(|e| e.branch.name() == Some(branch))
        .map(|e| e.id.clone())
        .expect("the branch calls the model")
}

#[test]
fn a_forked_subagent_reads_the_conversation_and_answers_its_call() {
    let (mut agg, branch) = delegated("researcher", crate::protocol::SubagentBranch::Head);
    let b = agg.state.open_run(&BranchId::from(&branch)).unwrap();
    assert_eq!(b.from.as_deref(), Some("c1"), "it forks at the call");
    assert_eq!(
        agg.state
            .on(&BranchId::from(&branch))
            .config()
            .map(|c| c.model),
        agg.state.on(&BranchId::MAIN).config().map(|c| c.model),
        "it inherits the conversation's config"
    );

    let call = branch_call_id(&agg, &branch);
    let side = prompt_texts(&agg, &call);
    let chat = prompt_texts(&agg, "c1");
    assert_eq!(
        side[..chat.len()],
        chat[..],
        "the fork keeps the cached prefix"
    );
    assert_eq!(
        side[chat.len()..],
        ["", crate::prompts::answers::FORKED, "Find the causes.",],
        "the call it answers, then its task"
    );

    reply(&mut agg, &call, said("Three causes.", 100));
    let events = echo(&mut agg);
    assert_eq!(branch_closed(&events, &branch), Some(RunEnd::Done));
    let (_, trigger) = live_decision(&agg);
    assert!(matches!(
        trigger,
        Trigger::SubagentFinished { ok: true, ref result, .. } if result.as_deref() == Some("Three causes.")
    ));
    echo(&mut agg);
    let chat = agg.state.on(&BranchId::MAIN).transcript();
    let answered = chat
        .iter()
        .find(|m| m.tool_call_id.as_deref() == Some("tc-r"))
        .unwrap();
    assert!(text_of(answered).contains("Three causes."));
    assert!(
        text_of(answered).contains(&branch),
        "it names the branch to continue"
    );
    let again = agg
        .state
        .effects_of(EffectKind::LlmCall)
        .any(|e| e.branch.is_main() && e.id != "c1");
    assert!(again, "the conversation goes on with the answer");
}

#[test]
fn a_root_subagent_runs_clean_as_its_own_config() {
    let (agg, branch) = delegated("poet", crate::protocol::SubagentBranch::Root);
    let b = agg.state.open_run(&BranchId::from(&branch)).unwrap();
    assert_eq!(b.from, None);
    let call = branch_call_id(&agg, &branch);
    assert_eq!(
        prompt_texts(&agg, &call),
        ["You write poems.", "Find the causes."]
    );
    assert_eq!(agg.state.llm_call(&call).unwrap().spec.model, "m-poet");
}

#[test]
fn a_subagent_on_a_branch_ends_with_its_turn() {
    let (mut agg, branch) = delegated("researcher", crate::protocol::SubagentBranch::Head);
    dispatch(
        &mut agg,
        CommandPayload::CancelSession { drop_queued: false },
        &frontend(),
    );
    assert!(agg.state.open_run(&BranchId::from(&branch)).is_none());
    assert_eq!(
        agg.state
            .tracking(EffectKind::Subagent, "tc-r")
            .unwrap()
            .status(),
        EffectStatus::Failed
    );
}

#[test]
fn a_detached_subagent_on_a_branch_reports_back_as_a_message() {
    let mut agg = with_subagents(crate::protocol::SubagentBranch::Head, "researcher");
    user_turn(&mut agg, "u1", "look into it", "turn-1");
    let (d, _) = live_decision(&agg);
    submit_decision(
        &mut agg,
        d,
        vec![node_msg("u1", Role::User, "look into it")],
        vec![tree_call("c1")],
    );
    let call = LlmResponse {
        tool_calls: vec![ToolCall {
            id: "tc-r".to_string(),
            call_type: "function".to_string(),
            function: ToolCallFunction {
                name: "researcher".to_string(),
                arguments: serde_json::json!({ "message": "dig", "mode": "detached" }).to_string(),
            },
        }],
        ..calling("researcher", "tc-r")
    };
    reply(&mut agg, "c1", call);
    echo(&mut agg);
    let branch = agg.state.subagent("tc-r").unwrap().session_id.clone();
    assert_eq!(
        agg.state.open_run(&BranchId::from(&branch)).unwrap().mode,
        BranchMode::Detached
    );

    for _ in 0..8 {
        let Some(trigger) = live_decision_kind(&agg) else {
            break;
        };
        let on_branch = {
            let (d, _) = live_decision(&agg);
            !agg.state.decision_branch(&d).is_main()
        };
        if on_branch && matches!(trigger, Trigger::BranchStarted { .. }) {
            echo(&mut agg);
            continue;
        }
        if on_branch {
            break;
        }
        echo(&mut agg);
    }
    let call = branch_call_id(&agg, &branch);
    reply(&mut agg, &call, said("Found it.", 100));
    echo(&mut agg);
    assert!(agg.state.open_run(&BranchId::from(&branch)).is_none());
    let notice = agg
        .state
        .queued_decisions()
        .into_iter()
        .find_map(|e| match &e.decision()?.trigger {
            Trigger::SubagentNotice { messages, .. } => Some((e.branch.clone(), messages.clone())),
            _ => None,
        })
        .expect("the result waits for the conversation's turn");
    assert!(notice.0.is_main());
    assert!(text_of(&notice.1[0].clone().recorded(String::new())).contains("Found it."));
}

fn pending_lanes(agg: &Session) -> Vec<BranchId> {
    let mut lanes: Vec<BranchId> = agg
        .state
        .effects_of(EffectKind::Decision)
        .filter(|e| e.tracking.status() == EffectStatus::Pending)
        .map(|e| e.branch.clone())
        .collect();
    lanes.sort();
    lanes
}

fn two_drafts(parallel: bool) -> Session {
    let config = AgentConfig {
        parallel_branches: parallel.then_some(true),
        ..agent_config("m1")
    };
    let mut agg = create_session_with_config("sess-p", "tenant-a", "user-1", Some(config));
    user_turn(&mut agg, "u1", "write a haiku", "turn-1");
    let (d, _) = live_decision(&agg);
    submit_decision(
        &mut agg,
        d,
        vec![node_msg("u1", Role::User, "write a haiku")],
        vec![
            branch_call(
                "a1",
                start("draft-a", BranchFrom::Head, BranchMode::Blocking),
                vec![],
            ),
            branch_call(
                "b1",
                start("draft-b", BranchFrom::Head, BranchMode::Blocking),
                vec![],
            ),
        ],
    );
    reply(&mut agg, "a1", said("Leaves fall", 10));
    reply(&mut agg, "b1", said("Waves fold", 10));
    agg
}

#[test]
fn branches_in_parallel_are_decided_at_once() {
    let agg = two_drafts(true);
    assert_eq!(
        pending_lanes(&agg),
        vec![BranchId::from("draft-a"), BranchId::from("draft-b")]
    );
}

#[test]
fn branches_are_decided_one_at_a_time_unless_asked() {
    let mut agg = two_drafts(false);
    assert_eq!(pending_lanes(&agg), vec![BranchId::from("draft-a")]);
    echo(&mut agg);
    assert_eq!(pending_lanes(&agg), vec![BranchId::from("draft-b")]);
}

#[test]
fn an_interrupt_on_one_branch_holds_only_that_branch() {
    let mut agg = two_drafts(true);
    let a = agg
        .state
        .effects_of(EffectKind::Decision)
        .find(|e| {
            e.branch.name() == Some("draft-a") && e.tracking.status() == EffectStatus::Pending
        })
        .map(|e| e.id.clone())
        .unwrap();
    let a_view = agg
        .state
        .on(&BranchId::from(Some("draft-a")))
        .transcript()
        .into_iter()
        .map(DraftMessage::from)
        .chain([node_msg("a1", Role::Assistant, "Leaves fall")])
        .collect();
    submit_decision(
        &mut agg,
        a,
        a_view,
        vec![Action::Interrupt {
            interrupt_id: "approve-a".to_string(),
            reason: "check the draft".to_string(),
            payload: serde_json::Value::Null,
        }],
    );
    assert_eq!(pending_lanes(&agg), vec![BranchId::from("draft-b")]);
    assert!(
        agg.state.open_run(&BranchId::from("draft-a")).is_some(),
        "held, not ended"
    );
}

#[test]
fn a_message_a_client_edits_in_place_starts_a_new_path() {
    let mut agg = configured();
    seed_tree(
        &mut agg,
        vec![
            node_msg("u1", Role::User, "hi"),
            node_msg("a1", Role::Assistant, "hello"),
        ],
    );
    let events = submit_messages(&mut agg, vec![node_msg("u1", Role::User, "hi there")]);
    let messages = transcript_messages(&events).expect("the edit reaches the worker");
    assert_eq!(messages[0].id, None, "the edit is a new message");
    let (d, _) = live_decision(&agg);
    submit_decision(&mut agg, d, messages, vec![]);
    let path: Vec<String> = agg
        .state
        .on(&BranchId::MAIN)
        .transcript()
        .iter()
        .map(text_of)
        .collect();
    assert_eq!(path, ["hi there"]);
    assert_ne!(agg.state.leaf(&BranchId::MAIN), Some("u1"));
}

#[test]
fn a_settled_call_stays_until_its_calls_are_answered() {
    let tools = Some(vec![crate::protocol::LlmTool {
        name: "lookup".to_string(),
        description: "Looks it up.".to_string(),
        input: None,
        output: None,
        defer: false,
    }]);
    let mut agg = configured();
    user_turn(&mut agg, "u1", "what time is it?", "turn-1");
    let (d, _) = live_decision(&agg);
    let call = match tree_call("c1") {
        Action::CallLlm {
            id,
            llm,
            request,
            stream,
            retry,
            handler,
            format,
            branch,
        } => Action::CallLlm {
            id,
            llm,
            request: LlmRequest {
                tools: tools.clone(),
                ..request
            },
            stream,
            retry,
            handler,
            format,
            branch,
        },
        other => other,
    };
    submit_decision(
        &mut agg,
        d,
        vec![node_msg("u1", Role::User, "what time is it?")],
        vec![call],
    );
    reply(&mut agg, "c1", calling("lookup", "tc-1"));
    assert!(
        agg.state.llm_call("c1").unwrap().spec.tools.is_some(),
        "its call is not answered"
    );
    echo(&mut agg);
    while live_decision_kind(&agg).is_some_and(|t| matches!(t, Trigger::ToolExecute { .. })) {
        let (d, _) = live_decision(&agg);
        submit_decision(&mut agg, d, vec![], vec![tool_result_action("tc-1")]);
    }
    let (d, _) = live_decision(&agg);
    let proposed = proposed_for(&agg, &d);
    assert!(
        matches!(
            proposed.actions.as_slice(),
            [DecisionAction::CallLlm { tools: Some(t), .. }] if t.len() == 1
        ),
        "the next call asks for the same tools"
    );
    echo(&mut agg);
    assert!(agg.state.llm_call("c1").is_none(), "answered: released");
}

#[test]
fn a_call_with_messages_inside_a_branch_starts_a_child_that_reports_to_it() {
    let mut agg = turn_with_side_call(BranchMode::Detached);
    reply(&mut agg, "t1", said("Paris weather", 100));
    let (d, _) = live_decision(&agg);
    assert_eq!(agg.state.decision_branch(&d).name(), Some("title"));
    submit_decision(
        &mut agg,
        d,
        vec![],
        vec![explicit("x2", vec![node_msg("", Role::User, "shorter")])],
    );
    let child = agg
        .state
        .open_run(&BranchId::from("x2"))
        .expect("a child branch");
    assert_eq!(child.parent.name(), Some("title"));
    assert_eq!(prompt_texts(&agg, "x2"), ["shorter"]);

    reply(&mut agg, "x2", said("Paris", 10));
    let (d, _) = live_decision(&agg);
    assert_eq!(agg.state.decision_branch(&d).name(), Some("x2"));
    let events = submit_decision(&mut agg, d, vec![], vec![]);
    assert_eq!(branch_closed(&events, "x2"), Some(RunEnd::Done));
    let (_, on) = branch_finished(&events).expect("the child reports back");
    assert_eq!(on.as_deref(), Some("title"));
}

#[test]
fn a_call_with_messages_too_deep_for_a_branch_is_answered_with_an_error() {
    let mut agg = turn_with_side_call(BranchMode::Detached);
    reply(&mut agg, "t1", said("Paris weather", 100));
    let mut calling = "title".to_string();
    for level in 2.. {
        let (d, _) = live_decision(&agg);
        let id = format!("x{level}");
        let events = submit_decision(
            &mut agg,
            d,
            vec![],
            vec![explicit(&id, vec![node_msg("", Role::User, "deeper")])],
        );
        if agg.state.open_run(&BranchId::from(&id)).is_none() {
            assert_eq!(
                agg.state.branch_depth(&BranchId::from(&calling)),
                crate::command::branch::MAX_BRANCH_DEPTH
            );
            let refused = events.iter().find_map(|e| match e {
                EventPayload::DecisionQueued(q) => match &q.trigger {
                    Trigger::LlmFinished {
                        id: call,
                        ok: false,
                        error: Some(error),
                        ..
                    } if *call == id => Some((error.code.clone(), q.branch.clone())),
                    _ => None,
                },
                _ => None,
            });
            assert_eq!(refused, Some((ErrorCode::Unroutable, Some(calling))));
            break;
        }
        reply(&mut agg, &id, said("deeper", 10));
        calling = id;
    }
}

#[test]
fn a_call_with_messages_asked_again_continues_its_branch_with_what_is_new() {
    let mut agg = configured();
    let ask = |agg: &mut Session, messages: Vec<DraftMessage>| {
        dispatch(
            agg,
            CommandPayload::RequestLlmCall {
                llm: "claude".to_string(),
                call_id: "x1".to_string(),
                request: request_with(messages),
                stream: false,
                retry: RetryPolicy::no_retry(),
                handler: LlmHandler::Server,
                format: None,
            },
            &system(),
        )
    };
    let written = |events: &[EventPayload]| -> Vec<NewMessage> {
        events
            .iter()
            .filter_map(|e| match e {
                EventPayload::NewMessage(m) => Some(m.clone()),
                _ => None,
            })
            .collect()
    };
    let first = written(&ask(&mut agg, vec![node_msg("", Role::User, "a title")]));
    let asked = first[0].message.id.clone();
    reply(&mut agg, "x1", said("A title", 10));
    let (d, _) = live_decision(&agg);
    let events = submit_decision(&mut agg, d, vec![], vec![]);
    assert_eq!(branch_closed(&events, "x1"), Some(RunEnd::Done));

    let again = ask(
        &mut agg,
        vec![
            node_msg(&asked, Role::User, "a title"),
            node_msg("", Role::User, "shorter"),
        ],
    );
    let leaf = again
        .iter()
        .find_map(|e| match e {
            EventPayload::BranchStarted(b) if b.id == "x1" => b.leaf.clone(),
            _ => None,
        })
        .expect("the branch goes on from where it ended");
    assert_eq!(agg.state.tree.message_at(Some(&leaf)), Some(asked));
    let new = written(&again);
    assert_eq!(new.len(), 1, "only what is new is written");
    assert_eq!(
        new[0].parent_id,
        Some(leaf),
        "after what the last call read"
    );
    assert_eq!(prompt_texts(&agg, "x1"), ["a title", "shorter"]);
}

#[test]
fn a_call_with_messages_neither_counts_as_the_conversations_read_nor_its_turn() {
    let mut agg = configured();
    user_turn(&mut agg, "u1", "hi", "turn-1");
    let (d, _) = live_decision(&agg);
    submit_decision(
        &mut agg,
        d,
        vec![node_msg("u1", Role::User, "hi")],
        vec![explicit(
            "sum-1",
            vec![node_msg("", Role::User, "summarize")],
        )],
    );
    reply(&mut agg, "sum-1", said("a summary", 9_000));
    assert!(agg.state.last_read.is_none());
    assert!(!agg.state.turn.as_ref().unwrap().called);
}

fn nested(levels: usize) -> (Session, String) {
    let mut agg = configured();
    user_turn(&mut agg, "u1", "hi", "turn-1");
    let mut transcript = vec![node_msg("u1", Role::User, "hi")];
    for level in 1..=levels {
        let (d, _) = live_decision(&agg);
        let id = format!("x{level}");
        submit_decision(
            &mut agg,
            d,
            std::mem::take(&mut transcript),
            vec![explicit(&id, vec![node_msg("", Role::User, "deeper")])],
        );
        reply(&mut agg, &id, said("deeper", 10));
    }
    let (d, _) = live_decision(&agg);
    (agg, d)
}

fn spawn(agg: &mut Session, decision: String) -> Vec<EventPayload> {
    submit_decision(
        agg,
        decision,
        vec![],
        vec![Action::SpawnSubagent {
            session_id: None,
            agent_id: "researcher".to_string(),
            tool_call_id: "s1".to_string(),
            message: Some(node_msg("", Role::User, "look it up")),
            retry: RetryPolicy::no_retry(),
            mode: None,
            config: None,
        }],
    )
}

#[test]
fn a_subagent_asked_for_on_a_calls_own_branch_counts_that_branch() {
    let (mut agg, d) = nested(4);
    assert_eq!(agg.state.branch_depth(agg.state.decision_branch(&d)), 4);
    let events = spawn(&mut agg, d);
    assert!(events
        .iter()
        .any(|e| matches!(e, EventPayload::SubagentRequested(_))));

    let (mut agg, d) = nested(5);
    assert_eq!(agg.state.branch_depth(agg.state.decision_branch(&d)), 5);
    let events = spawn(&mut agg, d);
    let refused = events.iter().any(|e| {
        matches!(e, EventPayload::DecisionQueued(q) if matches!(&q.trigger,
            Trigger::SubagentFinished { ok: false, error: Some(error), .. }
                if error.code == ErrorCode::BudgetExceeded))
    });
    assert!(refused, "{events:?}");
}

#[test]
fn a_message_branch_spawns_within_the_sessions_subagent_limit() {
    let mut agg = create_session_with_config(
        "sess-b",
        "tenant-a",
        "user-1",
        Some(AgentConfig {
            max_subagent_depth: Some(1),
            ..agent_config("m1")
        }),
    );
    user_turn(&mut agg, "u1", "hi", "turn-1");
    let (d, _) = live_decision(&agg);
    submit_decision(
        &mut agg,
        d,
        vec![node_msg("u1", Role::User, "hi")],
        vec![explicit("x1", vec![node_msg("", Role::User, "deeper")])],
    );
    reply(&mut agg, "x1", said("deeper", 10));
    let (d, _) = live_decision(&agg);
    let events = spawn(&mut agg, d);
    let refused = events.iter().any(|e| {
        matches!(e, EventPayload::DecisionQueued(q) if matches!(&q.trigger,
            Trigger::SubagentFinished { ok: false, error: Some(error), .. }
                if error.code == ErrorCode::BudgetExceeded))
    });
    assert!(refused, "{events:?}");
}

#[test]
fn every_branch_reads_the_same_way() {
    let mut agg = turn_with_side_call(BranchMode::Blocking);
    let texts = |at: crate::state::At<'_, '_>| -> Vec<String> {
        at.transcript().iter().map(text_of).collect()
    };
    let main = agg.state.on(&BranchId::MAIN);
    let title = agg.state.on(&BranchId::from("title"));
    assert_eq!(texts(main), ["what is the weather in Paris?"]);
    assert_eq!(
        texts(title),
        ["what is the weather in Paris?", "Write a short title."]
    );
    assert_eq!(
        title.config().map(|c| c.model),
        main.config().map(|c| c.model),
        "a branch that inherits reads its parent's config"
    );

    let first = agg.state.leaf(&BranchId::MAIN).unwrap().to_string();
    reply(&mut agg, "c1", said("Sunny.", 10));
    echo(&mut agg);
    assert_ne!(agg.state.leaf(&BranchId::MAIN), Some(first.as_str()));
    assert_eq!(
        texts(agg.state.at(Some(&first))),
        ["what is the weather in Paris?"],
        "an older leaf reads the conversation as it was"
    );
}

#[test]
fn a_branch_whose_run_failed_is_promoted_from_its_stored_leaf() {
    let mut agg = turn_with_side_call(BranchMode::Detached);
    reply(&mut agg, "t1", said("Paris weather", 10));
    let (d, _) = live_decision(&agg);
    let events = submit_decision(
        &mut agg,
        d,
        vec![],
        vec![Action::Fail {
            error: ErrorInfo::new(ErrorCode::HandlerError, "no title"),
        }],
    );
    assert_eq!(branch_closed(&events, "title"), Some(RunEnd::Failed));
    let title = BranchId::from("title");
    assert!(agg.state.open_run(&title).is_none(), "its run ended");
    let leaf = agg
        .state
        .tree
        .stored(&title)
        .and_then(|s| s.leaf)
        .expect("the store keeps its leaf");

    let (d, _) = live_decision(&agg);
    submit_decision(
        &mut agg,
        d,
        vec![],
        vec![Action::PromoteBranch {
            id: "title".to_string(),
        }],
    );
    assert_eq!(agg.state.leaf(&BranchId::MAIN), Some(leaf.as_str()));
}
