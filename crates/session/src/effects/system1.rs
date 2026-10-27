use super::{decision_queued, fail, mismatched, void_events, KindSpec, Outcome, SettleError};
use crate::command::SessionError;
use crate::cx::Cx;
use crate::decision::Trigger;
use crate::events::*;
use crate::protocol::{EffectStatus, RetryPolicy};
use crate::state::{EffectKind, SessionState};
use crate::system1::{System1Request, System1Response};
use crate::Caller;

pub struct System1Spec;

impl KindSpec for System1Spec {
    fn kind(&self) -> EffectKind {
        EffectKind::System1Call
    }

    fn settle(&self, state: &Cx<'_>, id: &str, outcome: Outcome) -> Vec<EventPayload> {
        match outcome {
            Outcome::System1(response) => complete(state, id, *response),
            Outcome::Error(e) => fail(self, state, id, &e),
            other => mismatched(self.kind(), &other),
        }
    }

    fn errored(&self, state: &SessionState, id: &str, e: &SettleError) -> Option<EventPayload> {
        Some(EventPayload::System1CallErrored(System1CallErrored {
            id: id.to_string(),
            attempt: attempt(state, id),
            error: e.error.clone(),
            retryable: e.retryable,
        }))
    }

    fn terminal(&self, state: &Cx<'_>, id: &str, e: &SettleError) -> Vec<EventPayload> {
        vec![decision_queued(
            state.ids(),
            Trigger::system1_err(id.to_string(), e.error.clone()),
        )]
    }

    fn dispatch(&self, state: &SessionState, id: &str) -> Vec<EventPayload> {
        let Some(effect) = state.effect(EffectKind::System1Call, id) else {
            return void_events(EffectKind::System1Call, id.to_string());
        };
        vec![EventPayload::System1CallDispatched(System1CallDispatched {
            id: id.to_string(),
            attempt: effect.tracking.retry.attempts,
        })]
    }

    fn retry(&self, state: &SessionState, id: &str) -> Vec<EventPayload> {
        let Some(effect) = state.effect(EffectKind::System1Call, id) else {
            return Vec::new();
        };
        let Some(call) = effect.system1() else {
            return Vec::new();
        };
        vec![EventPayload::System1CallRequested(Box::new(
            System1CallRequested {
                id: id.to_string(),
                attempt: effect.tracking.retry.attempts,
                system1: call.system1.clone(),
                request: call.request.clone(),
                retry: effect.tracking.retry_policy.clone(),
            },
        ))]
    }
}

fn attempt(state: &SessionState, id: &str) -> u32 {
    state
        .tracking(EffectKind::System1Call, id)
        .map(|t| t.retry.attempts)
        .unwrap_or_default()
}

fn complete(state: &Cx<'_>, id: &str, response: System1Response) -> Vec<EventPayload> {
    vec![
        EventPayload::System1CallCompleted(Box::new(System1CallCompleted {
            id: id.to_string(),
            attempt: attempt(state, id),
            response: response.clone(),
        })),
        decision_queued(state.ids(), Trigger::system1_ok(id.to_string(), response)),
    ]
}

pub fn request(
    state: &Cx<'_>,
    call_id: String,
    system1: String,
    request: System1Request,
    retry: RetryPolicy,
    caller: &Caller,
) -> Result<Vec<EventPayload>, SessionError> {
    SessionState::ensure_internal(caller)?;
    let issue = matches!(
        state
            .tracking(EffectKind::System1Call, &call_id)
            .map(|t| t.status()),
        None | Some(EffectStatus::Failed) | Some(EffectStatus::RetryScheduled)
    );
    if !issue {
        tracing::debug!(%call_id, "system1 call id already issued; request no-ops");
        return Ok(Vec::new());
    }
    Ok(vec![EventPayload::System1CallRequested(Box::new(
        System1CallRequested {
            id: call_id,
            attempt: 0,
            system1,
            request,
            retry,
        },
    ))])
}
