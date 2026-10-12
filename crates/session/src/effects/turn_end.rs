use super::{KindSpec, Outcome};
use crate::cx::Cx;
use crate::decision::Trigger;
use crate::events::*;
use crate::protocol::{EffectStatus, ErrorInfo};
use crate::schedule::Dep;
use crate::state::{EffectKind, QueueEntry, SessionState, TurnPhase};

pub struct TurnEndSpec;

impl KindSpec for TurnEndSpec {
    fn kind(&self) -> EffectKind {
        EffectKind::TurnEnd
    }

    fn settle(&self, _state: &Cx<'_>, _id: &str, _outcome: Outcome) -> Vec<EventPayload> {
        Vec::new()
    }

    fn voids_when_missing(&self) -> bool {
        false
    }

    fn deps(&self, state: &SessionState, entry: &QueueEntry) -> Vec<Dep> {
        state
            .effects_of(EffectKind::Decision)
            .find(|e| {
                matches!(&e.decision().map(|d| &d.trigger),
                    Some(Trigger::TurnFinished { turn_id, .. }) if *turn_id == entry.id)
            })
            .map(|e| {
                vec![Dep::DecisionSettled {
                    decision_id: e.id.clone(),
                }]
            })
            .unwrap_or_default()
    }

    fn dispatch(&self, state: &SessionState, _id: &str) -> Vec<EventPayload> {
        state.finalize_run(None)
    }
}

impl SessionState {
    pub fn finalize_run(&self, error: Option<&ErrorInfo>) -> Vec<EventPayload> {
        match self.phase.finalizing().cloned() {
            Some(f) => vec![
                EventPayload::TurnCompleted(TurnCompleted {
                    turn_id: f.turn_id,
                    data: f.data,
                    turn_cost: f.cost,
                    turn_token_usage: f.usage,
                    error: error.cloned(),
                }),
                EventPayload::SessionDone(SessionDone {}),
            ],
            None => vec![EventPayload::SessionDone(SessionDone {})],
        }
    }

    pub fn fail_run(&self, error: &ErrorInfo) -> Vec<EventPayload> {
        if self.phase.finalizing().is_some() {
            return self.finalize_run(Some(error));
        }
        let mut events = self.void_effects(|_, tracking, _| {
            matches!(
                tracking.status(),
                EffectStatus::Queued | EffectStatus::Pending | EffectStatus::RetryScheduled
            )
        });
        events.extend(self.drop_queued_decisions());
        let TurnPhase::Active { turn_id } = &self.phase else {
            return events;
        };
        events.push(EventPayload::TurnCompleted(TurnCompleted {
            turn_id: turn_id.clone(),
            data: serde_json::Value::Null,
            turn_cost: self.turn_cost,
            turn_token_usage: self.turn_token_usage.clone(),
            error: Some(error.clone()),
        }));
        events.push(EventPayload::SessionDone(SessionDone {}));
        events
    }
}
