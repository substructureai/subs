use super::*;
use crate::propose::{propose, Proposing};
use crate::protocol::{
    ContextConfig, ContextReason, ContextStrategy, ContextStrategyKind, DecisionAction,
    DecisionResponse, DecisionTrigger, Usage,
};

fn strategy(kind: ContextStrategyKind) -> ContextStrategy {
    ContextStrategy {
        kind,
        idle_secs: None,
        over_tokens: None,
        keep_tokens: None,
        llm: None,
        model: None,
        prompt: None,
    }
}

fn configured(strategies: Vec<ContextStrategy>) -> AgentConfig {
    AgentConfig {
        system: Some("be brief".to_string()),
        context: Some(ContextConfig {
            window: None,
            strategy: strategies,
        }),
        ..config()
    }
}

fn started(strategies: Vec<ContextStrategy>) -> Trace {
    let mut t = Trace::create();
    t.decide_with(vec![], vec![], Some(configured(strategies)));
    t
}

fn system_message() -> DraftMessage {
    DraftMessage {
        role: Role::System,
        ..user_message("be brief")
    }
}

thread_local! {
    static SOURCE: std::cell::RefCell<PromptSource> = const { std::cell::RefCell::new(PromptSource::Legacy) };
}

fn skilled(source: PromptSource, strategies: Vec<ContextStrategy>) -> Trace {
    SOURCE.set(source);
    let mut t = Trace::create();
    let agent = AgentConfig {
        skills: vec![crate::protocol::AgentSkill {
            name: "visits".to_string(),
            description: "Who visited, and when".to_string(),
            source: crate::protocol::SkillSource::Bundle,
            tools: Vec::new(),
        }],
        ..configured(strategies)
    };
    t.decide_with(vec![], vec![], Some(agent));
    t
}

fn listings(t: &Trace, id: &str) -> usize {
    requested(t, id)
        .iter()
        .map(|m| m.matches("\"skill\":\"visits\"").count())
        .sum()
}

fn reading(t: &Trace, id: &str, new: Option<DraftMessage>) -> Action {
    let source = SOURCE.with_borrow(Clone::clone);
    let mut messages = Vec::new();
    if !source.is_tree() {
        messages.push(system_message());
        messages.extend(t.recorded());
        messages.extend(new);
    }
    Action::CallLlm {
        source,
        llm: "claude".to_string(),
        format: None,
        id: id.to_string(),
        request: LlmRequest {
            messages,
            ..llm_request()
        },
        stream: false,
        retry: RetryPolicy::no_retry(),
        handler: LlmHandler::Server,
        branch: None,
    }
}

fn answered(t: &mut Trace, id: &str, text: &str, read: u64) {
    t.run(
        CommandPayload::settle(
            EffectKind::LlmCall,
            id.to_string(),
            t.agg
                .state
                .tracking(EffectKind::LlmCall, id)
                .map(|t| t.retry.attempts),
            Outcome::Llm(Box::new(LlmResponse {
                content: Some(text.to_string()),
                usage: Some(Usage::new(read, 0, 0, 10)),
                ..llm_response(vec![])
            })),
        ),
        &system(),
    );
}

fn reply(id: &str, text: &str) -> DraftMessage {
    DraftMessage {
        id: Some(id.to_string()),
        role: Role::Assistant,
        ..user_message(text)
    }
}

fn turn(t: &mut Trace, n: usize, read: u64) {
    turn_saying(t, n, &format!("q{n}"), read);
}

fn turn_saying(t: &mut Trace, n: usize, asked: &str, read: u64) {
    let (asked, call) = (asked.to_string(), format!("call-{n}"));
    t.submit(&asked, Some(&format!("turn-{n}")));
    let action = reading(t, &call, Some(user_message(&asked)));
    t.decide_appending(user_message(&asked), vec![action]);
    answered(t, &call, &format!("a{n}"), read);
    t.decide_appending(reply(&call, &format!("a{n}")), vec![done()]);
    t.decide(vec![], vec![done()]);
}

fn scope(t: &Trace) -> Option<&str> {
    t.agg.state.decision_branch(&t.live_decision())
}

fn proposed(t: &Trace, trigger: &DecisionTrigger) -> DecisionResponse {
    propose(
        trigger,
        &Proposing {
            state: t.agg.state.at_scope(scope(t)),
            pending_calls: 0,
            scope: scope(t),
            detached: false,
        },
    )
    .expect("a plan")
}

fn wire(t: &Trace) -> DecisionTrigger {
    let at = t.agg.state.at_scope(scope(t));
    crate::request::wire_trigger(
        t.live_trigger(),
        &at.transcript(),
        &t.agg.state.message_tree(),
        &at.open_llm_calls(),
    )
    .0
}

fn lowered(action: DecisionAction, id: &str) -> Action {
    match action {
        DecisionAction::ReplaceContext { summary, messages } => {
            Action::ReplaceContext { summary, messages }
        }
        DecisionAction::Done { data } => Action::Done { data },
        DecisionAction::Fail { error } => Action::Fail { error },
        DecisionAction::CallLlm {
            id: given,
            messages,
            model,
            branch,
            ..
        } => Action::CallLlm {
            source: match (&messages, &branch) {
                (Some(_), None) => PromptSource::Explicit,
                _ => PromptSource::tree(),
            },
            llm: "claude".to_string(),
            format: None,
            id: given.unwrap_or_else(|| id.to_string()),
            request: LlmRequest {
                model: model.unwrap_or_default(),
                messages: messages.unwrap_or_default(),
                ..llm_request()
            },
            stream: false,
            retry: RetryPolicy::no_retry(),
            handler: LlmHandler::Server,
            branch: branch.map(|b| crate::decision::BranchStart {
                id: b.id.unwrap_or_default(),
                from: match b.from.as_deref() {
                    None => crate::decision::BranchFrom::Root,
                    Some("head") => crate::decision::BranchFrom::Head,
                    Some(node) => crate::decision::BranchFrom::Node(node.to_string()),
                },
                agent: crate::decision::BranchConfig::Inherit,
                name: b.name,
                mode: b.mode,
            }),
        },
        other => panic!("unexpected plan {other:?}"),
    }
}

fn requested(t: &Trace, id: &str) -> Vec<String> {
    t.agg
        .state
        .sent_prompt(id)
        .expect("requested")
        .iter()
        .map(|m| {
            m.content
                .as_ref()
                .map(Content::text_owned)
                .unwrap_or_default()
        })
        .collect()
}

fn path(t: &Trace) -> Vec<String> {
    texts(&t.recorded())
}

fn plan(t: &mut Trace, summary_id: &str) {
    let plan = proposed(t, &wire(t));
    let actions = plan
        .actions
        .into_iter()
        .map(|a| lowered(a, summary_id))
        .collect();
    t.decide(plan.messages, actions);
}

fn summarized(t: &mut Trace) {
    plan(t, "");
    assert!(matches!(t.live_trigger(), Trigger::BranchFinished { .. }));
    plan(t, "");
}

fn ask(t: &mut Trace, n: usize) {
    let (asked, call) = (format!("q{n}"), format!("call-{n}"));
    t.submit(&asked, Some(&format!("turn-{n}")));
    let action = reading(t, &call, Some(user_message(&asked)));
    t.decide_appending(user_message(&asked), vec![action]);
}

fn emitted(t: &Trace) -> Vec<&str> {
    t.names
        .iter()
        .map(String::as_str)
        .filter(|n| n.starts_with("context."))
        .collect()
}

#[test]
fn a_quiet_session_starts_over_with_the_turns_own_messages() {
    let mut t = started(vec![ContextStrategy {
        idle_secs: Some(3600),
        ..strategy(ContextStrategyKind::Reset)
    }]);
    turn(&mut t, 1, 100);
    t.advance(7200);

    ask(&mut t, 2);
    assert!(t.agg.state.llm_call("call-2").is_none(), "the call waits");
    let DecisionTrigger::ContextLimit {
        reason,
        strategy,
        call,
        ..
    } = wire(&t)
    else {
        panic!("a context.limit decision");
    };
    assert_eq!(reason, ContextReason::Idle);
    assert_eq!(strategy, Some(ContextStrategyKind::Reset));
    assert_eq!(call.id, "call-2");

    plan(&mut t, "");
    assert_eq!(path(&t), ["q2"]);
    assert_eq!(requested(&t, "call-2"), ["be brief", "q2"]);
    let copy = t.agg.state.nodes.last().unwrap();
    assert!(
        copy.entry.copied_from.is_some(),
        "the turn's message is a copy"
    );
    assert!(
        t.agg.state.at_head().resolve_agent_for().is_some(),
        "the config carries over"
    );
    assert_eq!(emitted(&t), ["context.limited", "context.replaced"]);
}

#[test]
fn a_full_session_is_summarized_and_keeps_its_recent_messages() {
    let mut t = started(vec![ContextStrategy {
        over_tokens: Some(1000),
        keep_tokens: Some(1),
        ..strategy(ContextStrategyKind::Summarize)
    }]);
    turn(&mut t, 1, 100);
    turn(&mut t, 2, 2000);

    ask(&mut t, 3);
    let trigger = wire(&t);
    assert!(matches!(
        trigger,
        DecisionTrigger::ContextLimit {
            reason: ContextReason::Full,
            ..
        }
    ));
    let summary = proposed(&t, &trigger).actions.remove(0);
    let DecisionAction::CallLlm {
        messages: Some(asked),
        stream: Some(false),
        ..
    } = &summary
    else {
        panic!("a summary call");
    };
    let asked = texts(asked.as_slice()).join("\n");
    assert!(asked.contains("[User]: q1") && asked.contains("[Assistant]: a2"));
    assert!(!asked.contains("q3"), "the kept message is not summarized");

    plan(&mut t, "sum-1");
    assert!(t.agg.state.is_summary_call("sum-1"));
    let branch = t.agg.state.effect(EffectKind::LlmCall, "sum-1").unwrap();
    assert_eq!(branch.branch.as_deref(), Some("call-3.summary"));
    answered(&mut t, "sum-1", "they asked twice", 50);
    assert_eq!(path(&t).len(), 5, "the summary is not an answer");
    summarized(&mut t);

    assert_eq!(
        path(&t),
        [
            "The earlier conversation was summarized:\n\nthey asked twice",
            "q3"
        ]
    );
    assert_eq!(requested(&t, "call-3")[1..], path(&t));
    assert!(t.agg.state.last_read.is_none());
    assert_eq!(emitted(&t), ["context.limited", "context.replaced"]);

    answered(&mut t, "call-3", "a3", 300);
    t.decide_appending(reply("call-3", "a3"), vec![done()]);
    t.decide(vec![], vec![done()]);
    ask(&mut t, 4);
    assert!(t.agg.state.llm_call("call-4").is_some(), "room enough now");
}

#[test]
fn a_summary_too_long_for_the_model_is_asked_again_shorter() {
    let mut t = started(vec![ContextStrategy {
        over_tokens: Some(1000),
        keep_tokens: Some(1),
        ..strategy(ContextStrategyKind::Summarize)
    }]);
    let long = "a long question ".repeat(500);
    for n in 1..=3 {
        turn_saying(&mut t, n, &long, if n == 3 { 2000 } else { 100 });
    }
    ask(&mut t, 4);
    plan(&mut t, "sum-1");
    let first = crate::context::summary_size(&t.agg.state, "call-4.summary");
    t.run(
        CommandPayload::settle(
            EffectKind::LlmCall,
            "sum-1".to_string(),
            Some(0),
            Outcome::Error(SettleError::new(
                crate::protocol::ErrorInfo::new(ErrorCode::ContextOverflow, "too long"),
                false,
            )),
        ),
        &system(),
    );
    plan(&mut t, "");
    assert!(matches!(
        t.live_trigger(),
        Trigger::BranchFinished { ok: false, .. }
    ));
    plan(&mut t, "sum-2");
    assert!(t.agg.state.is_summary_call("sum-2"));
    let second = t
        .agg
        .state
        .limited
        .as_ref()
        .unwrap()
        .summary
        .clone()
        .unwrap();
    assert!(crate::context::summary_size(&t.agg.state, &second) < first);
}

#[test]
fn a_kept_history_runs_the_call_and_is_not_asked_again_this_turn() {
    let mut t = started(vec![ContextStrategy {
        over_tokens: Some(90),
        ..strategy(ContextStrategyKind::Worker)
    }]);
    turn(&mut t, 1, 100);

    ask(&mut t, 2);
    plan(&mut t, "");
    assert_eq!(emitted(&t), ["context.limited", "context.kept"]);
    assert_eq!(requested(&t, "call-2"), ["be brief", "q1", "a1", "q2"]);
    answered(&mut t, "call-2", "a2", 100);
    let action = reading(&t, "call-3", Some(reply("call-2", "a2")));
    t.decide_appending(reply("call-2", "a2"), vec![action]);
    assert!(t.agg.state.llm_call("call-3").is_some());
}

#[test]
fn a_call_the_provider_finds_too_long_is_summarized_then_tried_once_more() {
    let mut t = started(vec![]);
    turn(&mut t, 1, 100);
    ask(&mut t, 2);

    let too_long = |t: &Trace| {
        let attempt = t
            .agg
            .state
            .tracking(EffectKind::LlmCall, "call-2")
            .map(|t| t.retry.attempts);
        CommandPayload::settle(
            EffectKind::LlmCall,
            "call-2".to_string(),
            attempt,
            Outcome::Error(SettleError::new(
                crate::protocol::ErrorInfo::new(ErrorCode::ContextOverflow, "prompt is too long"),
                false,
            )),
        )
    };
    t.run(too_long(&t), &system());
    assert!(matches!(
        wire(&t),
        DecisionTrigger::ContextLimit {
            reason: ContextReason::Overflow,
            strategy: Some(ContextStrategyKind::Summarize),
            ..
        }
    ));
    plan(&mut t, "sum-1");
    answered(&mut t, "sum-1", "they asked once", 50);
    summarized(&mut t);
    assert_eq!(
        t.agg
            .state
            .tracking(EffectKind::LlmCall, "call-2")
            .map(|e| e.status()),
        Some(EffectStatus::Pending),
        "tried once more"
    );

    t.run(too_long(&t), &system());
    assert!(matches!(
        t.live_trigger(),
        Trigger::LlmFinished { ok: false, .. }
    ));
}

#[test]
fn a_replace_sets_a_new_branch_any_time() {
    let mut t = started(vec![]);
    turn(&mut t, 1, 100);
    t.submit("q2", Some("turn-2"));
    let mut view = t.recorded();
    view.push(user_message("q2"));
    t.decide(
        view,
        vec![Action::ReplaceContext {
            summary: Some("notes".to_string()),
            messages: vec![user_message("q2 with notes")],
        }],
    );
    assert_eq!(
        path(&t),
        [
            "The earlier conversation was summarized:\n\nnotes",
            "q2 with notes"
        ]
    );
    assert_eq!(emitted(&t), ["context.replaced"]);
    assert!(t.agg.state.at_head().resolve_agent_for().is_some());
}

fn summarizing() -> Vec<ContextStrategy> {
    vec![ContextStrategy {
        over_tokens: Some(1000),
        keep_tokens: Some(1),
        ..strategy(ContextStrategyKind::Summarize)
    }]
}

#[test]
fn a_held_tree_call_shows_the_prompt_it_would_send() {
    let mut t = skilled(PromptSource::tree(), summarizing());
    turn(&mut t, 1, 100);
    turn(&mut t, 2, 2000);
    ask(&mut t, 3);
    let DecisionTrigger::ContextLimit { call, .. } = wire(&t) else {
        panic!("a context.limit decision");
    };
    let held = texts(&call.request.messages);
    assert_eq!(held[0], "be brief");
    assert!(held[1].contains("\"skill\":\"visits\""), "{held:?}");
    assert_eq!(held[2..], ["q1", "a1", "q2", "a2", "q3"]);
    assert!(
        t.agg.state.limited.as_ref().unwrap().source.is_tree(),
        "and is held as a tree call"
    );
}

#[test]
fn a_held_tree_call_runs_on_the_new_branch_and_is_told_its_context_once() {
    let mut t = skilled(PromptSource::tree(), summarizing());
    turn(&mut t, 1, 100);
    turn(&mut t, 2, 2000);
    assert_eq!(listings(&t, "call-2"), 1);

    ask(&mut t, 3);
    plan(&mut t, "sum-1");
    let summary = t.agg.state.effect(EffectKind::LlmCall, "sum-1").unwrap();
    assert!(
        summary.branch.is_some(),
        "a summary is written on its own branch"
    );
    assert_eq!(
        t.agg
            .state
            .at_scope(summary.branch.as_deref())
            .resolve_agent_for(),
        None
    );
    assert_eq!(
        listings(&t, "sum-1"),
        0,
        "a summary call is sent as written"
    );
    answered(&mut t, "sum-1", "they asked twice", 50);
    plan(&mut t, "");
    let again = proposed(&t, &wire(&t)).actions.remove(1);
    assert!(
        matches!(&again, DecisionAction::CallLlm { id: Some(id), messages: None, .. } if id == "call-3"),
        "the held call is proposed as a tree call: {again:?}"
    );
    plan(&mut t, "");

    let read = requested(&t, "call-3");
    assert_eq!(read[0], "be brief");
    assert_eq!(listings(&t, "call-3"), 1, "{read:?}");
    assert_eq!(read[2..], path(&t), "then the new branch");
    let head = t.agg.state.head_id.clone();
    assert_eq!(
        t.agg.state.llm_call("call-3").unwrap().source,
        PromptSource::Tree { at: head }
    );

    answered(&mut t, "call-3", "a3", 100);
    t.decide_appending(reply("call-3", "a3"), vec![done()]);
    t.decide(vec![], vec![done()]);
    turn(&mut t, 4, 100);
    assert_eq!(listings(&t, "call-4"), 1, "{:?}", requested(&t, "call-4"));
    assert_eq!(requested(&t, "call-4")[..read.len()], read[..]);
}

fn refused_as_too_long(t: &mut Trace, id: &str) {
    let attempt = t
        .agg
        .state
        .tracking(EffectKind::LlmCall, id)
        .map(|t| t.retry.attempts);
    t.run(
        CommandPayload::settle(
            EffectKind::LlmCall,
            id.to_string(),
            attempt,
            Outcome::Error(SettleError::new(
                crate::protocol::ErrorInfo::new(ErrorCode::ContextOverflow, "prompt is too long"),
                false,
            )),
        ),
        &system(),
    );
}

fn refused_then_summarized(source: PromptSource) -> (usize, usize) {
    let mut t = skilled(source, vec![]);
    turn(&mut t, 1, 100);
    ask(&mut t, 2);
    refused_as_too_long(&mut t, "call-2");
    plan(&mut t, "sum-1");
    answered(&mut t, "sum-1", "they asked once", 50);
    summarized(&mut t);
    let again = listings(&t, "call-2");
    answered(&mut t, "call-2", "a2", 100);
    t.decide_appending(reply("call-2", "a2"), vec![done()]);
    t.decide(vec![], vec![done()]);
    turn(&mut t, 3, 100);
    (again, listings(&t, "call-3"))
}

#[test]
fn a_tree_call_the_provider_refused_moves_to_the_new_branch() {
    assert_eq!(refused_then_summarized(PromptSource::tree()), (1, 1));
}

#[test]
fn a_legacy_call_the_provider_refused_runs_again_as_a_tree_call() {
    let mut t = skilled(PromptSource::Legacy, vec![]);
    turn(&mut t, 1, 100);
    ask(&mut t, 2);
    refused_as_too_long(&mut t, "call-2");
    plan(&mut t, "sum-1");
    answered(&mut t, "sum-1", "they asked once", 50);
    summarized(&mut t);
    assert!(t.agg.state.llm_call("call-2").unwrap().source.is_tree());
    assert_eq!(listings(&t, "call-2"), 1);
}

#[test]
fn a_held_tree_call_kept_as_it_was_stays_a_tree_call() {
    let mut t = skilled(
        PromptSource::tree(),
        vec![ContextStrategy {
            over_tokens: Some(90),
            ..strategy(ContextStrategyKind::Worker)
        }],
    );
    turn(&mut t, 1, 100);
    ask(&mut t, 2);
    plan(&mut t, "");
    assert_eq!(emitted(&t), ["context.limited", "context.kept"]);
    assert!(t.agg.state.llm_call("call-2").unwrap().source.is_tree());
    assert_eq!(listings(&t, "call-2"), 1);
    assert_eq!(requested(&t, "call-2")[2..], ["q1", "a1", "q2"]);
}

#[test]
fn a_held_tree_call_starts_over_on_the_turns_own_messages() {
    let mut t = skilled(
        PromptSource::tree(),
        vec![ContextStrategy {
            idle_secs: Some(3600),
            ..strategy(ContextStrategyKind::Reset)
        }],
    );
    turn(&mut t, 1, 100);
    t.advance(7200);
    ask(&mut t, 2);
    plan(&mut t, "");
    let read = requested(&t, "call-2");
    assert_eq!(read.len(), 3, "{read:?}");
    assert_eq!((read[0].as_str(), read[2].as_str()), ("be brief", "q2"));
    assert_eq!(listings(&t, "call-2"), 1);
}
