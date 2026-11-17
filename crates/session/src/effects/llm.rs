use super::{decision_queued, fail, mismatched, void_events, KindSpec, Outcome, SettleError};
use crate::command::SessionError;
use crate::cx::Cx;
use crate::decision::{LlmHandler, Trigger};
use crate::events::*;
use crate::protocol::{
    Content, DraftMessage, EffectStatus, ErrorCode, ErrorInfo, LlmFormat, LlmRequest, LlmResponse,
    RetryPolicy, Role, StoredContent,
};
use crate::schedule::Dep;
use crate::state::{EffectKind, QueueEntry, SessionState};
use crate::Caller;

pub struct LlmSpec;

impl KindSpec for LlmSpec {
    fn kind(&self) -> EffectKind {
        EffectKind::LlmCall
    }

    fn authorize(
        &self,
        state: &SessionState,
        id: &str,
        caller: &Caller,
    ) -> Result<(), SessionError> {
        state.check_llm_call_caller(state.llm_call(id), caller)
    }

    fn settle(&self, state: &Cx<'_>, id: &str, outcome: Outcome) -> Vec<EventPayload> {
        match outcome {
            Outcome::Llm(response) => complete(state, id, *response),
            Outcome::Error(e) => fail(self, state, id, &e),
            other => mismatched(self.kind(), &other),
        }
    }

    fn errored(&self, state: &SessionState, id: &str, e: &SettleError) -> Option<EventPayload> {
        Some(EventPayload::LlmCallErrored(LlmCallErrored {
            id: id.to_string(),
            attempt: attempt(state, id),
            error: e.error.clone(),
            retryable: e.retryable,
        }))
    }

    fn terminal(&self, state: &Cx<'_>, id: &str, e: &SettleError) -> Vec<EventPayload> {
        let overflow = (e.error.code == ErrorCode::ContextOverflow)
            .then(|| crate::context::overflow(state, id))
            .flatten();
        if let Some(limited) = overflow {
            let trigger = Trigger::ContextLimit {
                reason: limited.reason,
                strategy: limited.strategy,
                tokens: limited.tokens,
                window: limited.window,
                call: limited.call.clone(),
            };
            return vec![
                EventPayload::ContextLimited(Box::new(limited)),
                decision_queued(state.ids(), trigger),
            ];
        }
        vec![decision_queued(
            state.ids(),
            Trigger::llm_err(id.to_string(), e.error.clone()),
        )]
    }

    fn timeout_error(&self, total: bool) -> SettleError {
        SettleError::new(
            ErrorInfo::new(ErrorCode::DeadlineExceeded, super::DEADLINE),
            !total,
        )
    }

    fn dispatch(&self, state: &Cx<'_>, id: &str) -> Vec<EventPayload> {
        let Some(effect) = state.effect(EffectKind::LlmCall, id) else {
            return void_events(EffectKind::LlmCall, id.to_string());
        };
        let out = state.outgoing(id);
        let placed = effect.llm().is_some_and(|c| c.reads_from.is_some());
        let leaf = state.scope_node(effect.branch.as_deref());
        let mut events = Vec::new();
        if let Some(out) = out
            .as_ref()
            .filter(|out| out.places && !placed && out.node == leaf)
        {
            events.push(EventPayload::ContextPlaced(ContextPlaced {
                node: crate::tree::Place {
                    id: state.ids().next(),
                    parent_id: out.node.clone(),
                    branch: effect.branch.clone(),
                },
                call: id.to_string(),
                placed: out.placed.clone(),
            }));
        }
        events.push(EventPayload::LlmCallDispatched(LlmCallDispatched {
            id: id.to_string(),
            attempt: effect.tracking.retry.attempts,
            fingerprint: out.map(|out| crate::prompt_context::fingerprint(&out.prompt)),
        }));
        events
    }

    fn execute_trigger(&self, state: &SessionState, id: &str) -> Option<Trigger> {
        let effect = state.effect(EffectKind::LlmCall, id)?;
        let call = effect.llm().filter(|c| c.handler == LlmHandler::Worker)?;
        Some(Trigger::LlmExecute {
            id: id.to_string(),
            request: call.sent_request(),
            format: call.format,
            defer_tools_strategy: call.defer_tools_strategy,
            stream: call.stream,
            attempt: effect.tracking.retry.attempts,
            deadline: effect.tracking.deadline,
        })
    }

    fn deps(&self, state: &SessionState, entry: &QueueEntry) -> Vec<Dep> {
        let effect = state.effect(EffectKind::LlmCall, &entry.id);
        if let Some(branch) = effect.and_then(|e| e.branch.as_deref()) {
            return super::connector::owed(state.at_scope(Some(branch)));
        }
        let node = effect
            .and_then(|e| e.asked_at.clone())
            .or_else(|| state.head_id.clone());
        super::connector::owed(state.at(node.as_deref()))
    }

    fn retry(&self, state: &SessionState, id: &str) -> Vec<EventPayload> {
        let Some(effect) = state.effect(EffectKind::LlmCall, id) else {
            return Vec::new();
        };
        let (t, Some(call)) = (&effect.tracking, effect.llm()) else {
            return Vec::new();
        };
        vec![EventPayload::LlmCallRequested(LlmCallRequested {
            id: id.to_string(),
            attempt: t.retry.attempts,
            llm: call.llm.clone(),
            spec: call.spec.clone(),
            at: effect.asked_at.clone(),
            stream: call.stream,
            retry: t.retry_policy.clone(),
            handler: call.handler,
            format: call.format,
            defer_tools_strategy: call.defer_tools_strategy,
            branch: effect.branch.clone(),
        })]
    }
}

fn attempt(state: &SessionState, id: &str) -> u32 {
    state
        .tracking(EffectKind::LlmCall, id)
        .map(|t| t.retry.attempts)
        .unwrap_or_default()
}

fn complete(state: &Cx<'_>, id: &str, response: LlmResponse) -> Vec<EventPayload> {
    let message = assistant_message(id, &response);
    let truncated = response.finish_reason.as_deref() == Some("length");
    let refused = refused(response.finish_reason.as_deref());
    let usage = response.usage.clone();
    let cost = response.cost;
    vec![
        EventPayload::LlmCallCompleted(LlmCallCompleted {
            id: id.to_string(),
            attempt: attempt(state, id),
            response,
        }),
        decision_queued(
            state.ids(),
            Trigger::llm_ok(id.to_string(), message, truncated, refused, usage, cost),
        ),
    ]
}

fn refused(finish_reason: Option<&str>) -> bool {
    matches!(finish_reason, Some("refusal") | Some("content_filter"))
}

#[allow(clippy::too_many_arguments)]
pub fn request(
    state: &Cx<'_>,
    call_id: String,
    llm: String,
    request: LlmRequest,
    stream: bool,
    retry: RetryPolicy,
    handler: LlmHandler,
    format: Option<LlmFormat>,
    caller: &Caller,
) -> Result<Vec<EventPayload>, SessionError> {
    SessionState::ensure_internal(caller)?;
    let issue = matches!(
        state
            .tracking(EffectKind::LlmCall, &call_id)
            .map(|t| t.status()),
        None | Some(EffectStatus::Failed) | Some(EffectStatus::RetryScheduled)
    );
    if !issue {
        tracing::debug!(
            %call_id,
            "llm call id already issued; request no-ops (idempotent)"
        );
        return Ok(Vec::new());
    }
    let defer_tools_strategy = state
        .at_scope(state.scope())
        .config()
        .map(|c| c.defer_strategy())
        .unwrap_or_default();
    Ok(vec![EventPayload::LlmCallRequested(LlmCallRequested {
        id: call_id,
        attempt: 0,
        llm,
        spec: crate::state::LlmCallSpec::from(&request),
        at: state.scope_node(state.scope()),
        stream,
        retry,
        handler,
        format,
        defer_tools_strategy,
        branch: state.scope_branch(),
    })])
}

fn assistant_message(call_id: &str, response: &LlmResponse) -> DraftMessage {
    let tool_calls = (!response.tool_calls.is_empty()).then(|| response.tool_calls.clone());
    let content = if response.images.is_empty() {
        response.content.clone().map(Content::Text)
    } else {
        let mut parts: Vec<StoredContent> = Vec::new();
        if let Some(text) = &response.content {
            parts.push(StoredContent::Text { text: text.clone() });
        }
        for img in &response.images {
            parts.push(match img.url.starts_with(crate::blob::BLOB_SCHEME) {
                true => StoredContent::Blob {
                    uri: img.url.clone(),
                },
                false => StoredContent::Link {
                    uri: img.url.clone(),
                    name: None,
                    mime_type: Some("image/png".to_string()),
                },
            });
        }
        Some(Content::Parts(parts))
    };
    DraftMessage {
        id: Some(call_id.to_string()),
        role: Role::Assistant,
        content,
        tool_calls,
        reasoning: response.reasoning.clone(),
        tool_call_id: None,
        name: None,
    }
}
