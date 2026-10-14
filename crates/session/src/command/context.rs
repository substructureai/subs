use std::collections::HashSet;

use super::Working;
use crate::context;
use crate::decision::Trigger;
use crate::events::*;
use crate::ids::Ids;
use crate::protocol::{DraftMessage, HeldCall, NewMessage, Role};
use crate::state::SessionState;

impl Working {
    pub(super) fn held_for_room(&mut self, call: &HeldCall) -> bool {
        if self.holds(&call.id) {
            self.emit(EventPayload::ContextKept);
            return false;
        }
        let Some(limited) = context::limit(&self.state, call, self.now) else {
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
        let state = self.at_head().resolve_state_for();
        let config = self.at_head().resolve_agent_for();
        let reason = self.limited.as_ref().filter(|l| l.open()).map(|l| l.reason);

        let (written, summary, copies) = self.branch(&self.ids, summary, messages);
        let kept = written.len() - usize::from(summary.is_some());
        self.emit_all(written);

        let head = self.head_id.clone();
        if self.at_head().resolve_state_for() != state {
            self.emit(EventPayload::WorkerStateUpdated(WorkerStateUpdated {
                state,
                anchor: head.clone(),
            }));
        }
        if let Some(config) = config {
            if self.at_head().resolve_agent_for().as_ref() != Some(&config) {
                self.emit(EventPayload::AgentConfigUpdated(AgentConfigUpdated {
                    config,
                    anchor: head,
                }));
            }
        }
        self.then(|s| s.void_stranded_work(s.head_id.as_deref()));
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
    fn branch(
        &self,
        ids: &Ids,
        summary: Option<String>,
        messages: Vec<DraftMessage>,
    ) -> (Vec<EventPayload>, Option<ContextSummary>, usize) {
        let known: HashSet<&str> = self.nodes.iter().map(|n| n.message.id.as_str()).collect();
        let mut events = Vec::with_capacity(messages.len() + 1);
        let mut parent: Option<String> = None;
        let mut copies = 0;
        let mut write = |draft: DraftMessage| {
            let copied_from = draft.id.clone().filter(|id| known.contains(id.as_str()));
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
