use super::Working;
use crate::context;
use crate::decision::{PromptSource, Trigger};
use crate::events::*;
use crate::ids::Ids;
use crate::protocol::{DraftMessage, HeldCall, NewMessage, Role, WorkerState};
use crate::state::SessionState;

impl Working {
    pub(super) fn held_source(&self, id: &str, source: PromptSource) -> PromptSource {
        let legacy = self
            .limited
            .as_ref()
            .is_some_and(|l| l.call.id == id && l.source == PromptSource::Legacy);
        match source {
            PromptSource::Explicit if legacy => PromptSource::Legacy,
            source => source,
        }
    }

    pub(super) fn held_for_room(&mut self, call: &HeldCall, source: &PromptSource) -> bool {
        if self.holds(&call.id) {
            self.emit(EventPayload::ContextKept);
            return false;
        }
        let Some(limited) = context::limit(&self.state, call, source, self.now) else {
            return false;
        };
        let trigger = Trigger::ContextLimit {
            reason: limited.reason,
            strategy: limited.strategy,
            tokens: limited.tokens,
            window: limited.window,
            call: limited.call.clone(),
        };
        self.emit(EventPayload::ContextLimited(Box::new(limited)));
        self.queue_decision(trigger);
        true
    }

    pub(super) fn replace_context(&mut self, summary: Option<String>, messages: Vec<DraftMessage>) {
        if summary.is_none() && messages.is_empty() {
            tracing::warn!("context.replace with no summary and no messages; ignored");
            return;
        }
        let before = self.at_head().transcript();
        let reason = self.limited.as_ref().filter(|l| l.open()).map(|l| l.reason);
        let (written, summary, copies) = self.restart(&self.ids, summary, messages);
        let kept = written
            .iter()
            .filter(|e| matches!(e, EventPayload::NewMessage(_)))
            .count()
            - usize::from(summary.is_some());
        self.emit_all(written);
        self.then(|s| s.void_stranded_work(None));
        let dropped = before
            .iter()
            .filter(|m| m.role != Role::System)
            .count()
            .saturating_sub(copies);
        self.emit(EventPayload::ContextReplaced(ContextReplaced {
            reason,
            summary,
            kept,
            dropped,
        }));
    }
}

impl SessionState {
    fn restart(
        &self,
        ids: &Ids,
        summary: Option<String>,
        messages: Vec<DraftMessage>,
    ) -> (Vec<EventPayload>, Option<ContextSummary>, usize) {
        let here = self.at_head();
        let mut parent: Option<String> = None;
        let mut events = Vec::with_capacity(messages.len() + 3);
        let place = |parent: &mut Option<String>| {
            let id = ids.next();
            crate::tree::Place {
                id: id.clone(),
                parent_id: parent.replace(id),
                branch: None,
            }
        };
        let state = here.resolve_state_for();
        if state != WorkerState::default() {
            events.push(EventPayload::WorkerStateUpdated(WorkerStateUpdated {
                state,
                node: place(&mut parent),
            }));
        }
        if let Some(config) = here.resolve_agent_for() {
            events.push(EventPayload::AgentConfigUpdated(AgentConfigUpdated {
                config,
                node: place(&mut parent),
            }));
        }
        let mut copies = 0;
        let mut write = |draft: DraftMessage| {
            let copied_from = draft.id.clone().filter(|id| self.has_node(id));
            let message = match copied_from {
                Some(_) => draft.rerecord(ids),
                None => draft.record(ids),
            };
            copies += usize::from(copied_from.is_some());
            let id = message.id.clone();
            events.push(EventPayload::NewMessage(NewMessage {
                message,
                parent_id: parent.replace(id.clone()),
                copied_from,
                branch: None,
            }));
            id
        };
        let summary = summary.map(|text| ContextSummary {
            id: write(context::summary_message(&text)),
            text,
        });
        for draft in messages {
            write(draft);
        }
        (events, summary, copies)
    }
}
