use std::collections::{HashMap, HashSet};

use super::{stops, KindSpec, Outcome};
use crate::cx::Cx;
use crate::decision::Trigger;
use crate::events::*;
use crate::ids::Ids;
use crate::propose::{settled_content, subagent_text};
use crate::protocol::{
    Content, DraftMessage, EffectStatus, ErrorCode, ErrorInfo, InterruptResolution, NewMessage,
    ResumeStatus, Role,
};
use crate::schedule::Dep;
use crate::state::{EffectKind, SessionState, TurnPhase};

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

    fn park(&self, state: &SessionState, _id: &str) -> Option<Dep> {
        let (kind, id) = state.open_work().into_iter().next()?;
        Some(match kind {
            EffectKind::Decision => Dep::DecisionSettled {
                decision_id: id.to_string(),
            },
            kind => Dep::WorkSettled {
                kind,
                id: id.to_string(),
            },
        })
    }

    fn dispatch(&self, state: &SessionState, _id: &str) -> Vec<EventPayload> {
        state.finalize_run(None)
    }
}

pub const CANCELLED_AT_TURN_END: &str = "cancelled: the turn ended";

impl SessionState {
    pub fn close_open_calls(&self, ids: &Ids, answering: Option<&Trigger>) -> Vec<EventPayload> {
        let at = self.at_head();
        let transcript = at.transcript();
        let answered: HashSet<&str> = transcript
            .iter()
            .filter_map(|m| m.tool_call_id.as_deref())
            .collect();
        let settled: HashMap<&str, Content> = self
            .effects_of(EffectKind::Decision)
            .filter_map(|e| e.decision().map(|d| &d.trigger))
            .chain(answering)
            .filter_map(|trigger| match trigger {
                Trigger::ToolFinished {
                    id,
                    ok,
                    result,
                    error,
                    ..
                } => Some((id.as_str(), settled_content(*ok, result, error))),
                Trigger::SubagentFinished {
                    id,
                    ok,
                    session_id,
                    result,
                    error,
                    ..
                } => Some((
                    id.as_str(),
                    Content::Text(subagent_text(*ok, session_id, result, error)),
                )),
                _ => None,
            })
            .collect();
        let mut events = Vec::new();
        let mut parent = self.head_id.clone();
        let mut closed = HashSet::new();
        for call in transcript.iter().flat_map(|m| &m.tool_calls) {
            if answered.contains(call.id.as_str()) || !closed.insert(call.id.clone()) {
                continue;
            }
            let content = settled
                .get(call.id.as_str())
                .cloned()
                .unwrap_or_else(|| Content::Text(CANCELLED_AT_TURN_END.to_string()));
            let message = DraftMessage {
                id: None,
                role: Role::Tool,
                content: Some(content),
                tool_calls: None,
                tool_call_id: Some(call.id.clone()),
                name: Some(call.function.name.clone()),
                reasoning: None,
            }
            .record(ids);
            let id = message.id.clone();
            events.push(EventPayload::NewMessage(NewMessage {
                message,
                parent_id: parent.replace(id),
                copied_from: None,
            }));
        }
        let voided: Vec<(EffectKind, String)> = self
            .effects
            .values()
            .filter(|e| {
                matches!(
                    e.kind(),
                    EffectKind::ToolCall
                        | EffectKind::Subagent
                        | EffectKind::LlmCall
                        | EffectKind::System1Call
                )
            })
            .filter(|e| closed.contains(&e.id) || at.anchor_on_path(e.anchor.as_deref()))
            .filter(|e| e.tracking.is_unsettled())
            .map(|e| (e.kind(), e.id.clone()))
            .collect();
        let gone = |kind: EffectKind, id: &str| {
            closed.contains(id) || voided.iter().any(|(k, v)| *k == kind && v == id)
        };
        for e in self.effects_of(EffectKind::Decision) {
            let Some(d) = e.decision() else { continue };
            let about = match &d.trigger {
                Trigger::ToolExecute { id, .. }
                | Trigger::ToolFinished { id, .. }
                | Trigger::CodemodeCalls { id, .. } => gone(EffectKind::ToolCall, id),
                Trigger::SubagentFinished { id, .. } => gone(EffectKind::Subagent, id),
                Trigger::LlmExecute { id, .. } | Trigger::LlmFinished { id, .. } => {
                    gone(EffectKind::LlmCall, id)
                }
                Trigger::System1Finished { id, .. } => gone(EffectKind::System1Call, id),
                _ => false,
            };
            if about
                && matches!(
                    e.tracking.status(),
                    EffectStatus::Queued | EffectStatus::RetryScheduled
                )
            {
                events.push(EventPayload::DecisionDropped(DecisionDropped {
                    id: e.id.clone(),
                }));
            }
        }
        events.extend(
            voided
                .into_iter()
                .map(|(kind, id)| EventPayload::CallVoided(CallVoided { kind, id })),
        );
        events
    }

    pub fn quiet_turn_ended(&self) -> bool {
        self.quiet_end.is_some()
            && self.quiet_end.as_deref() == self.phase.active_turn_id()
            && !self.head_parked()
            && !self.has_open_work()
    }

    pub fn has_open_work(&self) -> bool {
        !self.open_work().is_empty()
    }

    pub fn open_work(&self) -> Vec<(EffectKind, &str)> {
        self.effects
            .values()
            .filter(|e| match e.decision() {
                Some(d) => {
                    d.trigger.deferred_turn_id().is_none()
                        && !matches!(d.trigger, Trigger::ClientAction { .. })
                }
                None => matches!(
                    e.kind(),
                    EffectKind::ToolCall
                        | EffectKind::LlmCall
                        | EffectKind::Subagent
                        | EffectKind::System1Call
                ),
            })
            .filter(|e| e.tracking.is_unsettled())
            .map(|e| (e.kind(), e.id.as_str()))
            .collect()
    }

    pub fn finalize_run(&self, error: Option<&ErrorInfo>) -> Vec<EventPayload> {
        match self.phase.finalizing().cloned() {
            Some(f) => vec![
                EventPayload::TurnCompleted(TurnCompleted {
                    turn_id: f.turn_id,
                    data: f.data,
                    turn_cost: f.cost,
                    turn_token_usage: f.usage,
                    turn_calls: Box::new(self.turn_calls.clone()),
                    turn_started_at: self.turn_started_at,
                    error: error.cloned(),
                }),
                EventPayload::SessionDone(SessionDone {}),
            ],
            None => vec![EventPayload::SessionDone(SessionDone {})],
        }
    }

    pub fn cancel_run(&self) -> Vec<EventPayload> {
        let cancelled = serde_json::to_value(InterruptResolution {
            status: ResumeStatus::Cancelled,
            payload: serde_json::Value::Null,
            responder: None,
        })
        .unwrap_or_default();
        let mut events: Vec<EventPayload> = self
            .open_interrupts
            .iter()
            .map(|i| {
                EventPayload::InterruptResumed(InterruptResumed {
                    interrupt_id: i.interrupt_id.clone(),
                    payload: cancelled.clone(),
                })
            })
            .collect();
        events.extend(self.void_effects(|_, tracking, _| tracking.is_unsettled()));
        events.extend(
            self.effects_of(EffectKind::Decision)
                .filter(|e| {
                    e.tracking.is_unsettled()
                        && !matches!(
                            e.decision().map(|d| &d.trigger),
                            Some(Trigger::SessionStart)
                        )
                })
                .map(|e| EventPayload::DecisionDropped(DecisionDropped { id: e.id.clone() })),
        );
        let error = ErrorInfo::new(ErrorCode::Cancelled, "cancelled");
        match &self.phase {
            TurnPhase::Finalizing(_) => events.extend(self.finalize_run(Some(&error))),
            TurnPhase::Active { turn_id } => events.extend([
                EventPayload::TurnCompleted(TurnCompleted {
                    turn_id: turn_id.clone(),
                    data: serde_json::Value::Null,
                    turn_cost: self.turn_cost,
                    turn_token_usage: self.turn_token_usage.clone(),
                    turn_calls: Box::new(self.turn_calls.clone()),
                    turn_started_at: self.turn_started_at,
                    error: Some(error),
                }),
                EventPayload::SessionDone(SessionDone {}),
            ]),
            TurnPhase::Idle => {}
        }
        events
    }

    pub fn cancellable(&self) -> bool {
        self.phase.turn_id().is_some()
            || !self.open_interrupts.is_empty()
            || self.effects.values().any(|e| {
                e.tracking.is_unsettled()
                    && !matches!(
                        e.decision().map(|d| &d.trigger),
                        Some(Trigger::SessionStart)
                    )
                    && e.kind() != EffectKind::ConnectorSync
            })
    }

    pub fn fail_run(&self, error: &ErrorInfo) -> Vec<EventPayload> {
        let mut events = self.void_effects(|kind, tracking, _| stops(kind, tracking));
        events.extend(self.drop_queued_decisions());
        if self.phase.finalizing().is_some() {
            events.extend(self.finalize_run(Some(error)));
            return events;
        }
        let TurnPhase::Active { turn_id } = &self.phase else {
            return events;
        };
        events.push(EventPayload::TurnCompleted(TurnCompleted {
            turn_id: turn_id.clone(),
            data: serde_json::Value::Null,
            turn_cost: self.turn_cost,
            turn_token_usage: self.turn_token_usage.clone(),
            turn_calls: Box::new(self.turn_calls.clone()),
            turn_started_at: self.turn_started_at,
            error: Some(error.clone()),
        }));
        events.push(EventPayload::SessionDone(SessionDone {}));
        events
    }
}
