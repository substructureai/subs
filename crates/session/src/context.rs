use chrono::{DateTime, Utc};

use crate::events::ContextLimited;
use crate::protocol::{
    AgentConfig, Content, ContextConfig, ContextReason, ContextStrategy, ContextStrategyKind,
    DecisionAction, DraftMessage, HeldCall, LlmRequest, Message, Role,
};
use crate::state::{EffectKind, SessionState};

pub const RESERVE_TOKENS: u64 = 16_000;
pub const KEEP_TOKENS: u64 = 20_000;
const RESULT_CHARS: usize = 2_000;
const LEAST_CHARS: usize = 4_000;

const SUMMARY_SYSTEM: &str = "You write a handoff summary of a conversation for another model that will continue it. Do not continue the conversation. Do not answer what it asks.";

const HANDOFF: &str = "Summarize the conversation above for whoever continues it. Use these sections, and leave out a section that does not apply.

## Goal
- What the user wants, in one or two lines.

## Requirements
- The rules, preferences and limits the user gave.

## Decisions
- What was decided, and why.

## Done
- What is finished.

## Open
- What is in progress, blocked, or not yet answered.

## Next
1. The next steps, in order.

## Facts
- The names, ids, numbers and values that later steps need.

Keep each line short. Keep names, ids and values exact. Do not say that this is a summary.";

const SUMMARIZED: &str = "The earlier conversation was summarized:";

pub fn summary_message(summary: &str) -> DraftMessage {
    text_message(Role::User, format!("{SUMMARIZED}\n\n{summary}"))
}

fn text_message(role: Role, text: String) -> DraftMessage {
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

fn tokens(chars: usize) -> u64 {
    (chars as u64).div_ceil(4)
}

fn size<T: serde::Serialize>(value: &T) -> u64 {
    tokens(
        serde_json::to_vec(value)
            .map(|v| v.len())
            .unwrap_or_default(),
    )
}

fn history(path: &[Message]) -> Vec<&Message> {
    path.iter().filter(|m| m.role != Role::System).collect()
}

fn read_tokens(state: &SessionState, request: &LlmRequest) -> u64 {
    let last = state.last_read.as_ref().and_then(|read| {
        let at = request
            .messages
            .iter()
            .position(|m| m.id.as_deref() == Some(read.call_id.as_str()))?;
        Some(read.tokens + request.messages[at + 1..].iter().map(size).sum::<u64>())
    });
    last.unwrap_or_else(|| size(request))
}

fn applies(
    strategy: &ContextStrategy,
    tokens: u64,
    window: Option<u64>,
    idle_for: Option<u64>,
) -> Option<ContextReason> {
    if let (Some(idle), Some(quiet)) = (strategy.idle_secs, idle_for) {
        if quiet >= idle {
            return Some(ContextReason::Idle);
        }
    }
    let over = strategy.over_tokens.or_else(|| {
        strategy
            .idle_secs
            .is_none()
            .then(|| window.map(|w| w.saturating_sub(RESERVE_TOKENS)))
            .flatten()
    });
    over.is_some_and(|over| tokens > over)
        .then_some(ContextReason::Full)
}

fn makes_room(state: &SessionState, kind: ContextStrategyKind, path: &[Message]) -> bool {
    match kind {
        ContextStrategyKind::Reset => turn_messages(state, path).len() < history(path).len(),
        ContextStrategyKind::Summarize => !history(path).is_empty(),
        ContextStrategyKind::Worker => true,
    }
}

fn context_of(config: Option<&AgentConfig>) -> Option<&ContextConfig> {
    config.and_then(|c| c.context.as_ref())
}

pub fn strategy(config: Option<&AgentConfig>, kind: ContextStrategyKind) -> ContextStrategy {
    context_of(config)
        .and_then(|c| c.strategy.iter().find(|s| s.kind == kind))
        .cloned()
        .unwrap_or(ContextStrategy {
            kind,
            idle_secs: None,
            over_tokens: None,
            keep_tokens: None,
            llm: None,
            model: None,
            prompt: None,
        })
}

pub fn limit(state: &SessionState, call: &HeldCall, now: DateTime<Utc>) -> Option<ContextLimited> {
    let passes = state
        .limited
        .as_ref()
        .is_some_and(|l| l.open() || l.call.id == call.id);
    if passes || state.context_kept || state.tracking(EffectKind::LlmCall, &call.id).is_some() {
        return None;
    }
    let at = state.at_head();
    let config = at.resolve_agent_for();
    let context = context_of(config.as_ref())?;
    let path = at.transcript();
    let window = context.window;
    let tokens = read_tokens(state, &call.request);
    let idle_for = match (state.turn_called, state.last_turn_at) {
        (false, Some(at)) => Some(u64::try_from((now - at).num_seconds()).unwrap_or_default()),
        _ => None,
    };
    context.strategy.iter().find_map(|s| {
        let reason = applies(s, tokens, window, idle_for)?;
        makes_room(state, s.kind, &path).then(|| ContextLimited {
            reason,
            strategy: Some(s.kind),
            tokens,
            window,
            call: call.clone(),
        })
    })
}

pub fn overflow(state: &SessionState, id: &str) -> Option<ContextLimited> {
    if state
        .limited
        .as_ref()
        .is_some_and(|l| l.open() || l.call.id == id)
    {
        return None;
    }
    let effect = state.effect(EffectKind::LlmCall, id)?;
    let call = effect.llm()?;
    let request = call.spec.to_request(call.prompt.clone());
    let config = state.at_head().resolve_agent_for();
    Some(ContextLimited {
        reason: ContextReason::Overflow,
        strategy: Some(ContextStrategyKind::Summarize),
        tokens: read_tokens(state, &request),
        window: context_of(config.as_ref()).and_then(|c| c.window),
        call: HeldCall {
            id: id.to_string(),
            llm: call.llm.clone(),
            request,
            stream: call.stream,
        },
    })
}

pub fn turn_messages(state: &SessionState, path: &[Message]) -> Vec<DraftMessage> {
    let from = state
        .turn_head
        .as_deref()
        .and_then(|head| path.iter().position(|m| m.id == head))
        .map_or(0, |at| at + 1);
    history(&path[from..])
        .into_iter()
        .cloned()
        .map(DraftMessage::from)
        .collect()
}

fn cut(messages: &[&Message], keep: u64) -> usize {
    let mut kept = 0;
    let mut at_cut = None;
    for at in (1..messages.len()).rev() {
        kept += size(messages[at]);
        if messages[at].role != Role::User {
            continue;
        }
        if kept > keep && at_cut.is_some() {
            break;
        }
        at_cut = Some(at);
        if kept > keep {
            break;
        }
    }
    at_cut.unwrap_or(messages.len())
}

fn keep_tokens(strategy: &ContextStrategy) -> u64 {
    strategy.keep_tokens.unwrap_or(KEEP_TOKENS)
}

pub fn kept(path: &[Message], strategy: &ContextStrategy) -> Vec<DraftMessage> {
    let messages = history(path);
    let at = cut(&messages, keep_tokens(strategy));
    messages[at..]
        .iter()
        .map(|m| DraftMessage::from((*m).clone()))
        .collect()
}

fn text(message: &Message) -> String {
    message
        .content
        .as_ref()
        .map(Content::text_owned)
        .unwrap_or_default()
}

fn clipped(text: String, chars: usize) -> String {
    match text.char_indices().nth(chars) {
        Some((at, _)) => format!(
            "{}… [{} more characters]",
            &text[..at],
            text[at..].chars().count()
        ),
        None => text,
    }
}

fn written(messages: &[&Message]) -> String {
    let mut lines = Vec::new();
    for m in messages {
        match m.role {
            Role::User => lines.push(format!("[User]: {}", text(m))),
            Role::Assistant => {
                let said = text(m);
                if !said.is_empty() {
                    lines.push(format!("[Assistant]: {said}"));
                }
                for call in &m.tool_calls {
                    lines.push(format!(
                        "[Assistant called {}]: {}",
                        call.function.name, call.function.arguments
                    ));
                }
            }
            Role::Tool => lines.push(format!("[Tool result]: {}", clipped(text(m), RESULT_CHARS))),
            Role::System => {}
        }
    }
    lines.join("\n\n")
}

fn tail(text: String, chars: Option<usize>) -> String {
    let Some(chars) = chars else { return text };
    let count = text.chars().count();
    if count <= chars {
        return text;
    }
    let skip = text
        .char_indices()
        .nth(count - chars)
        .map_or(0, |(at, _)| at);
    format!("[earlier part left out]\n\n{}", &text[skip..])
}

pub fn summary_size(state: &SessionState, id: &str) -> usize {
    state
        .llm_call(id)
        .map(|call| call.prompt.iter().map(|m| text(m).chars().count()).sum())
        .unwrap_or_default()
}

pub fn summary_call(
    config: Option<&AgentConfig>,
    strategy: &ContextStrategy,
    path: &[Message],
    most_chars: Option<usize>,
) -> Option<DecisionAction> {
    let messages = history(path);
    let at = cut(&messages, keep_tokens(strategy));
    if at == 0 || most_chars.is_some_and(|c| c < LEAST_CHARS) {
        return None;
    }
    let fits = context_of(config)
        .and_then(|c| c.window)
        .and_then(|w| usize::try_from(w.saturating_sub(RESERVE_TOKENS)).ok())
        .map(|w| w * 3);
    let most = match (fits, most_chars) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (a, b) => a.or(b),
    };
    let older = tail(written(&messages[..at]), most);
    let asked = format!(
        "<conversation>\n{older}\n</conversation>\n\n{}",
        strategy.prompt.as_deref().unwrap_or(HANDOFF)
    );
    Some(DecisionAction::CallLlm {
        id: None,
        llm: strategy
            .llm
            .clone()
            .or_else(|| config.and_then(|c| c.llm.clone())),
        model: strategy
            .model
            .clone()
            .or_else(|| config.map(|c| c.model.clone())),
        messages: Some(vec![
            text_message(Role::System, SUMMARY_SYSTEM.to_string()),
            text_message(Role::User, asked),
        ]),
        tools: Some(Vec::new()),
        temperature: None,
        max_completion_tokens: None,
        reasoning: None,
        stream: Some(false),
        retry: None,
    })
}

pub fn reissue(call: &HeldCall, view: Option<Vec<DraftMessage>>) -> DecisionAction {
    let request = &call.request;
    let messages = match view {
        Some(view) => request
            .messages
            .iter()
            .take_while(|m| m.role == Role::System)
            .cloned()
            .chain(view)
            .collect(),
        None => request.messages.clone(),
    };
    DecisionAction::CallLlm {
        id: Some(call.id.clone()),
        llm: Some(call.llm.clone()),
        model: Some(request.model.clone()),
        messages: Some(messages),
        tools: request.tools.clone(),
        temperature: request.temperature,
        max_completion_tokens: request.max_completion_tokens,
        reasoning: request.reasoning.clone(),
        stream: Some(call.stream),
        retry: None,
    }
}
