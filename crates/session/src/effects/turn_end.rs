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
use crate::state::{BranchId, EffectKind, SessionState, Turn};

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

    fn dispatch(&self, state: &Cx<'_>, _id: &str) -> Vec<EventPayload> {
        state.finalize_run(None)
    }
}

pub const CANCELLED_AT_TURN_END: &str = "cancelled: the turn ended";

impl SessionState {
    pub fn close_open_calls(&self, ids: &Ids, answering: Option<&Trigger>) -> Vec<EventPayload> {
        let at = self.on(&BranchId::MAIN);
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
        let mut parent = self.leaf(&BranchId::MAIN).map(str::to_string);
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
                branch: None,
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
            .filter(|e| e.branch.is_main())
            .filter(|e| closed.contains(&e.id) || at.anchor_on_path(e.asked_at.as_deref()))
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
                events.push(self.decision_dropped(&e.id));
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
        self.turn
            .as_ref()
            .is_some_and(|t| t.quiet_end && t.ending.is_none())
            && !self.head_parked()
            && !self.has_open_work()
    }

    pub fn has_open_work(&self) -> bool {
        !self.open_work().is_empty()
    }

    pub fn open_work(&self) -> Vec<(EffectKind, &str)> {
        self.effects
            .values()
            .filter(|e| !self.detached(&e.branch))
            .filter(|e| match e.decision() {
                Some(d) => d.trigger.is_turn_work(),
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
        let mut events = self.void_turn_branches();
        events.extend(self.finalized(error));
        events
    }

    fn finalized(&self, error: Option<&ErrorInfo>) -> Vec<EventPayload> {
        let ending = self.turn.as_ref().filter(|t| t.ending.is_some());
        let mut events: Vec<EventPayload> =
            ending.map(|t| completed(t, error)).into_iter().collect();
        events.push(EventPayload::SessionDone(SessionDone {}));
        events
    }

    pub fn cancel_run(&self, drop_queued: bool) -> Vec<EventPayload> {
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
        events.extend(self.void_turn_branches());
        events.extend(self.void_effects(|_, tracking, _| tracking.is_unsettled()));
        events.extend(
            self.effects_of(EffectKind::Decision)
                .filter(|e| !self.detached(&e.branch))
                .filter(|e| {
                    let Some(d) = e.decision() else { return false };
                    let waits =
                        e.tracking.status() == EffectStatus::Queued && !d.trigger.is_turn_work();
                    e.tracking.is_unsettled()
                        && !matches!(d.trigger, Trigger::SessionStart)
                        && (drop_queued || !waits)
                })
                .map(|e| self.decision_dropped(&e.id)),
        );
        let error = ErrorInfo::new(ErrorCode::Cancelled, "cancelled");
        if let Some(turn) = &self.turn {
            events.push(completed(turn, Some(&error)));
            events.push(EventPayload::SessionDone(SessionDone {}));
        }
        events
    }

    pub fn cancellable(&self) -> bool {
        self.turn.is_some()
            || !self.open_interrupts.is_empty()
            || self.effects.values().any(|e| {
                e.tracking.is_unsettled()
                    && !self.detached(&e.branch)
                    && !matches!(
                        e.decision().map(|d| &d.trigger),
                        Some(Trigger::SessionStart)
                    )
                    && e.kind() != EffectKind::ConnectorSync
            })
    }

    pub fn fail_run(&self, error: &ErrorInfo) -> Vec<EventPayload> {
        let mut events = self.void_turn_branches();
        events.extend(self.void_effects(|kind, tracking, _| stops(kind, tracking)));
        events.extend(self.drop_queued_decisions());
        if let Some(turn) = &self.turn {
            events.push(completed(turn, Some(error)));
            events.push(EventPayload::SessionDone(SessionDone {}));
        }
        events
    }
}

fn completed(turn: &Turn, error: Option<&ErrorInfo>) -> EventPayload {
    let (data, cost, usage) = match &turn.ending {
        Some(end) => (end.data.clone(), end.cost, end.usage.clone()),
        None => (serde_json::Value::Null, turn.cost, turn.usage.clone()),
    };
    EventPayload::TurnCompleted(TurnCompleted {
        turn_id: turn.id.clone(),
        data,
        turn_cost: cost,
        turn_token_usage: usage,
        turn_calls: Box::new(turn.calls.clone()),
        turn_started_at: Some(turn.started_at),
        error: error.cloned(),
    })
}
