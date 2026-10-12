use super::{fail, mismatched, resolve_pending, KindSpec, Outcome, Settle, SettleError};
use crate::command::SessionError;
use crate::cx::Cx;
use crate::decision::Trigger;
use crate::events::*;
use crate::protocol::EffectStatus;
use crate::schedule::Dep;
use crate::state::{DecisionPark, EffectKind, QueueEntry, SessionState};
use crate::Caller;

pub struct DecisionSpec;

impl KindSpec for DecisionSpec {
    fn kind(&self) -> EffectKind {
        EffectKind::Decision
    }

    fn authorize(
        &self,
        _state: &SessionState,
        _id: &str,
        caller: &Caller,
    ) -> Result<(), SessionError> {
        SessionState::ensure_worker_or_system(caller)
    }

    fn resolve(
        &self,
        state: &SessionState,
        id: &str,
        _attempt: Option<u32>,
        _caller: &Caller,
    ) -> Result<Settle, SessionError> {
        Ok(resolve_pending(state.tracking(EffectKind::Decision, id)))
    }

    fn settle(&self, state: &Cx<'_>, id: &str, outcome: Outcome) -> Vec<EventPayload> {
        match outcome {
            Outcome::Error(e) => fail(self, state, id, &e),
            other => mismatched(self.kind(), &other),
        }
    }

    fn errored(&self, _state: &SessionState, id: &str, e: &SettleError) -> Option<EventPayload> {
        Some(EventPayload::DecisionErrored(DecisionErrored {
            id: id.to_string(),
            error: e.error.clone(),
            retryable: e.retryable,
        }))
    }

    fn terminal(&self, state: &Cx<'_>, id: &str, e: &SettleError) -> Vec<EventPayload> {
        let turnless = state
            .worker_decision(id)
            .is_some_and(|d| matches!(d.trigger, Trigger::ClientAction { .. }));
        if turnless {
            return Vec::new();
        }
        state.fail_run(&e.error)
    }

    fn requeues_on_retry(&self) -> bool {
        false
    }

    fn resend(&self, _state: &SessionState, id: &str) -> Vec<EventPayload> {
        vec![EventPayload::DecisionDispatched(DecisionDispatched {
            id: id.to_string(),
        })]
    }

    fn voids_when_missing(&self) -> bool {
        false
    }

    fn park(&self, state: &SessionState, id: &str) -> Option<Dep> {
        let park = state
            .worker_decision(id)
            .and_then(|d| state.decision_park(&d.trigger))?;
        Some(match park {
            DecisionPark::Interrupt(i) => Dep::InterruptResumed {
                interrupt_id: i.interrupt_id.clone(),
            },
            DecisionPark::Turn(turn_id) => Dep::TurnSettled {
                turn_id: turn_id.to_string(),
            },
        })
    }

    fn deps(&self, state: &SessionState, entry: &QueueEntry) -> Vec<Dep> {
        let mut deps: Vec<Dep> = state
            .effects_of(EffectKind::Decision)
            .filter(|e| {
                let Some(wd) = e.decision() else { return false };
                e.tracking.status() == EffectStatus::Pending
                    || (matches!(wd.trigger, Trigger::SessionStart)
                        && e.tracking.status() == EffectStatus::RetryScheduled
                        && e.id != entry.id)
            })
            .map(|e| Dep::DecisionSettled {
                decision_id: e.id.clone(),
            })
            .collect();
        deps.extend(super::connector::owed(state.at_head()));
        deps.sort_by_key(Dep::label);
        deps.dedup();
        deps
    }

    fn dispatch(&self, state: &SessionState, id: &str) -> Vec<EventPayload> {
        let Some(decision) = state.worker_decision(id) else {
            return vec![EventPayload::DecisionDropped(DecisionDropped {
                id: id.to_string(),
            })];
        };
        let mut events = Vec::new();
        if let Some(turn_id) = decision.trigger.deferred_turn_id() {
            if state.completed_turn_ids.iter().any(|t| t == turn_id) {
                return vec![EventPayload::DecisionDropped(DecisionDropped {
                    id: id.to_string(),
                })];
            }
            events.push(EventPayload::TurnStarted(TurnStarted {
                turn_id: turn_id.to_string(),
            }));
        }
        events.push(EventPayload::DecisionDispatched(DecisionDispatched {
            id: id.to_string(),
        }));
        events
    }
}

impl SessionState {
    pub fn drop_queued_decisions(&self) -> Vec<EventPayload> {
        self.queued_decisions()
            .into_iter()
            .filter(|e| {
                e.decision()
                    .is_none_or(|d| d.trigger.deferred_turn_id().is_none())
            })
            .map(|e| EventPayload::DecisionDropped(DecisionDropped { id: e.id.clone() }))
            .collect()
    }
}
