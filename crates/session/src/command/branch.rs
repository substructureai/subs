use std::collections::HashSet;

use super::{CommandPayload, Working};
use crate::decision::{BranchConfig, BranchFrom, BranchStart, Trigger};
use crate::effects::{self, decision_queued_on};
use crate::events::*;
use crate::ids::Ids;
use crate::protocol::{BranchMode, DraftMessage, EffectStatus, ErrorCode, ErrorInfo, NewMessage};
use crate::state::{EffectKind, SessionState};
use crate::Caller;

pub const MAX_BRANCH_DEPTH: usize = 5;

impl SessionState {
    pub fn branch_and_below(&self, id: &str) -> Vec<String> {
        let mut found = vec![id.to_string()];
        let mut at = 0;
        while at < found.len() {
            let parent = found[at].clone();
            found.extend(
                self.branches
                    .iter()
                    .filter(|(_, b)| b.parent.as_deref() == Some(parent.as_str()))
                    .map(|(child, _)| child.clone()),
            );
            at += 1;
        }
        found
    }

    pub fn recorded_prefix(&self, messages: &[DraftMessage]) -> (Option<String>, usize) {
        let shared = self.tree.shared(messages, |draft, node| {
            node.message().is_some_and(|m| same(draft, m))
        });
        let mut path: Vec<&str> = messages[..shared]
            .iter()
            .filter_map(|m| m.id.as_deref())
            .collect();
        while let Some(last) = path.last() {
            let answered = self
                .tree
                .get(last)
                .is_none_or(|n| n.message().is_none_or(|m| m.tool_calls.is_empty()));
            if answered {
                break;
            }
            path.pop();
        }
        (path.last().map(|id| id.to_string()), path.len())
    }

    pub fn branch_depth(&self, branch: Option<&str>) -> usize {
        branch
            .and_then(|b| self.branches.get(b))
            .map_or(0, |b| b.depth)
    }

    pub fn close_branch(
        &self,
        id: &str,
        reason: BranchEnd,
        data: serde_json::Value,
        error: Option<ErrorInfo>,
    ) -> Vec<EventPayload> {
        if self.open_branch(id).is_none() {
            return Vec::new();
        }
        let closing = self.branch_and_below(id);
        let closing: HashSet<&str> = closing.iter().map(String::as_str).collect();
        let mut events = self.void_effects_where(|e| {
            e.branch.as_deref().is_some_and(|b| closing.contains(b))
                && effects::stops(e.kind(), &e.tracking)
        });
        events.extend(
            self.effects_of(EffectKind::Decision)
                .filter(|e| e.branch.as_deref().is_some_and(|b| closing.contains(b)))
                .filter(|e| {
                    matches!(
                        e.tracking.status(),
                        EffectStatus::Queued | EffectStatus::RetryScheduled
                    )
                })
                .map(|e| self.decision_dropped(&e.id)),
        );
        for b in self.branch_and_below(id).into_iter().rev() {
            let own = b == id;
            let leaf = self.branches.get(&b).and_then(|b| b.leaf.clone());
            events.push(EventPayload::BranchClosed(BranchClosed {
                leaf,
                id: b,
                reason: if own { reason } else { BranchEnd::Closed },
                data: if own {
                    data.clone()
                } else {
                    serde_json::Value::Null
                },
                error: if own { error.clone() } else { None },
            }));
        }
        events
    }

    pub fn finish_branch(
        &self,
        ids: &Ids,
        id: &str,
        data: serde_json::Value,
        error: Option<ErrorInfo>,
    ) -> Vec<EventPayload> {
        let Some(branch) = self.open_branch(id) else {
            return Vec::new();
        };
        let reason = match error {
            Some(_) => BranchEnd::Failed,
            None => BranchEnd::Done,
        };
        let trigger = Trigger::BranchFinished {
            id: id.to_string(),
            name: branch.name.clone(),
            mode: branch.mode,
            ok: error.is_none(),
            data: data.clone(),
            error: error.clone(),
            from: branch.from.clone(),
            leaf: branch.leaf.clone(),
        };
        let parent = branch.parent.clone();
        if let Some(call) = branch.subagent.clone() {
            let mut events = self.close_branch(id, reason, data.clone(), error.clone());
            events.extend(self.subagent_branch_answer(ids, &call, data, error));
            return events;
        }
        let mut events = self.close_branch(id, reason, data, error);
        let voided = parent
            .as_deref()
            .is_some_and(|p| self.open_branch(p).is_none());
        if !voided {
            events.push(decision_queued_on(ids, trigger, parent));
        }
        events
    }

    pub fn void_turn_branches(&self) -> Vec<EventPayload> {
        self.branches
            .iter()
            .filter(|(_, b)| b.mode == BranchMode::Blocking)
            .filter(|(_, b)| !self.detached(b.parent.as_deref()))
            .filter(|(_, b)| b.parent.is_none())
            .flat_map(|(id, _)| {
                self.close_branch(id, BranchEnd::Voided, serde_json::Value::Null, None)
            })
            .collect()
    }

    pub fn void_stranded_branches(&self, scope: Option<&str>) -> Vec<EventPayload> {
        let node = self.scope_node(scope);
        let at = self.at(node.as_deref());
        self.branches
            .iter()
            .filter(|(_, b)| b.parent.as_deref() == scope)
            .filter(|(_, b)| b.mode == BranchMode::Blocking)
            .filter(|(_, b)| b.from.is_some() && !at.anchor_on_path(b.from.as_deref()))
            .flat_map(|(id, _)| {
                self.close_branch(id, BranchEnd::Voided, serde_json::Value::Null, None)
            })
            .collect()
    }

    pub fn quiet_branch(&self, ids: &Ids, id: &str) -> Vec<EventPayload> {
        if self.open_branch(id).is_none() {
            return Vec::new();
        }
        let busy = self.effects.values().any(|e| {
            e.branch.as_deref() == Some(id)
                && match e.decision() {
                    Some(_) => e.tracking.is_unsettled(),
                    None => e.kind() != EffectKind::ConnectorSync && e.tracking.is_unsettled(),
                }
        });
        let waits = self.held_by_branches(Some(id)) > 0
            || self
                .branches
                .values()
                .any(|b| b.parent.as_deref() == Some(id) && b.mode == BranchMode::Detached);
        let parked = self.at_scope(Some(id)).active_interrupt_for().is_some();
        if busy || waits || parked {
            return Vec::new();
        }
        self.finish_branch(ids, id, serde_json::Value::Null, None)
    }
}

impl Working {
    pub(super) fn start_branch(
        &mut self,
        start: BranchStart,
        messages: Vec<DraftMessage>,
    ) -> Option<String> {
        let parent = self.scope.clone();
        if self.branches.contains_key(&start.id) || self.tree.branch(&start.id).is_some() {
            tracing::debug!(branch = %start.id, "branch already started; nothing to do");
            return None;
        }
        if self.branch_depth(parent.as_deref()) >= MAX_BRANCH_DEPTH {
            tracing::warn!(branch = %start.id, "branch too deep; not started");
            return None;
        }
        let from = match &start.from {
            BranchFrom::Root => None,
            BranchFrom::Head => self.scope_node(parent.as_deref()),
            BranchFrom::Node(node) => Some(node.clone()),
        };
        if let Some(node) = from.as_deref() {
            if !self.has_node(node) {
                tracing::warn!(branch = %start.id, node, "branch forks at an unknown node");
                return None;
            }
            let open = self
                .tree
                .message_at(Some(node))
                .and_then(|m| self.tree.get(&m))
                .is_some_and(|n| n.message().is_some_and(|m| !m.tool_calls.is_empty()));
            if open {
                tracing::warn!(
                    branch = %start.id,
                    node,
                    "a branch cannot fork at a message whose tool calls are unanswered"
                );
                return None;
            }
        }
        if from.is_none() && messages.is_empty() {
            tracing::warn!(branch = %start.id, "a root branch needs messages");
            return None;
        }
        let agent = match (&start.agent, &from) {
            (BranchConfig::Inherit, None) => BranchConfig::None,
            (agent, _) => agent.clone(),
        };
        let turn_id = match start.mode {
            BranchMode::Blocking if !self.detached(parent.as_deref()) => {
                self.phase.active_turn_id().map(str::to_string)
            }
            _ => None,
        };
        if let BranchConfig::Config(config) = &agent {
            self.then(|s| effects::connector::sync(s, config));
        }
        let node = self.ids.next();
        self.emit(EventPayload::BranchStarted(Box::new(BranchStarted {
            id: start.id.clone(),
            node: node.clone(),
            from: from.clone(),
            parent,
            agent,
            name: start.name.clone(),
            mode: start.mode,
            turn_id,
            subagent: None,
            leaf: None,
        })));
        let mut parent_id = self.scope_node(Some(&start.id));
        for draft in messages {
            let copied_from = draft.id.clone().filter(|id| self.has_node(id));
            let message = match copied_from {
                Some(_) => draft.rerecord(&self.ids),
                None => draft.record(&self.ids),
            };
            let id = message.id.clone();
            self.emit(EventPayload::NewMessage(NewMessage {
                message,
                parent_id: parent_id.replace(id),
                copied_from,
                branch: Some(start.id.clone()),
            }));
        }
        Some(start.id)
    }

    pub(super) fn explicit_branch(
        &mut self,
        call_id: &str,
        messages: Vec<DraftMessage>,
    ) -> Option<String> {
        if self.branches.contains_key(call_id) {
            return Some(call_id.to_string());
        }
        if self.tree.branch(call_id).is_some() {
            return None;
        }
        let (from, shared) = self.recorded_prefix(&messages);
        let start = BranchStart {
            id: call_id.to_string(),
            from: match from {
                Some(node) => BranchFrom::Node(node),
                None => BranchFrom::Root,
            },
            agent: BranchConfig::None,
            name: None,
            mode: BranchMode::Blocking,
        };
        self.start_branch(start, messages.into_iter().skip(shared).collect())
    }

    pub(super) fn start_subagent_branch(&mut self, call: &str) {
        let Some(effect) = self.effect(EffectKind::Subagent, call) else {
            return;
        };
        let Some(sa) = effect.subagent() else {
            return;
        };
        let Some(on) = sa.on_branch.clone() else {
            return;
        };
        let (branch, agent_id, message) = (
            sa.session_id.clone(),
            sa.agent_id.clone(),
            sa.message.clone(),
        );
        let mode = match sa.mode {
            crate::protocol::SpawnMode::Detached => BranchMode::Detached,
            _ => BranchMode::Blocking,
        };
        let (parent, anchor) = (effect.branch.clone(), effect.asked_at.clone());
        let started = EffectKind::Subagent.spec();
        self.then(|s| started.settle(s, call, effects::Outcome::SubagentStarted));
        let turn_id = match mode {
            BranchMode::Blocking if !self.detached(parent.as_deref()) => {
                self.phase.active_turn_id().map(str::to_string)
            }
            _ => None,
        };
        let task = message.unwrap_or(DraftMessage {
            id: None,
            role: crate::protocol::Role::User,
            content: None,
            tool_calls: None,
            tool_call_id: None,
            name: None,
            reasoning: None,
        });
        let known = self
            .branches
            .get(&branch)
            .cloned()
            .or_else(|| self.tree.branch(&branch).map(|r| r.state));
        let (from, agent, mut own, node) = match known {
            Some(b) => {
                let leaf = b.at().map(str::to_string);
                if leaf.as_deref().is_some_and(|l| !self.has_node(l)) {
                    let error = ErrorInfo::new(
                        ErrorCode::Unroutable,
                        crate::prompts::errors::subagent_branch_unloaded(&agent_id),
                    );
                    self.then(|s| {
                        s.subagent_branch_answer(
                            s.ids(),
                            call,
                            serde_json::Value::Null,
                            Some(error),
                        )
                    });
                    return;
                }
                (b.from.clone(), b.agent.clone(), leaf, b.node.clone())
            }
            None => (
                anchor.clone().filter(|_| on.fork),
                on.agent.clone(),
                None,
                self.ids.next(),
            ),
        };
        let fresh = own.is_none();
        self.emit(EventPayload::BranchStarted(Box::new(BranchStarted {
            id: branch.clone(),
            node: node.clone(),
            from: from.clone(),
            parent,
            agent: agent.clone(),
            name: Some(agent_id),
            mode,
            turn_id,
            subagent: Some(call.to_string()),
            leaf: own.clone(),
        })));
        if let BranchConfig::Config(config) = &agent {
            self.then(|s| effects::connector::sync(s, config));
        }
        let mut messages = Vec::new();
        if fresh && on.fork {
            let asked = self
                .tree
                .message_at(from.as_deref())
                .and_then(|m| self.tree.get(&m))
                .and_then(|n| n.message().map(|m| m.tool_calls.clone()))
                .unwrap_or_default();
            messages.extend(asked.into_iter().map(|c| DraftMessage {
                id: None,
                role: crate::protocol::Role::Tool,
                content: Some(crate::protocol::Content::Text(match c.id == call {
                    true => crate::prompts::answers::FORKED.to_string(),
                    false => crate::prompts::answers::FORK_SIBLING.to_string(),
                })),
                tool_calls: None,
                tool_call_id: Some(c.id.clone()),
                name: Some(c.function.name.clone()),
                reasoning: None,
            }));
        }
        messages.push(task);
        if fresh {
            own = self.scope_node(Some(&branch));
        }
        for draft in messages {
            let message = draft.record(&self.ids);
            let id = message.id.clone();
            self.emit(EventPayload::NewMessage(NewMessage {
                message,
                parent_id: own.replace(id),
                copied_from: None,
                branch: Some(branch.clone()),
            }));
        }
        let name = self.branches.get(&branch).and_then(|b| b.name.clone());
        self.queue_decision_on(
            Trigger::BranchStarted {
                id: branch.clone(),
                name,
                mode,
            },
            Some(branch),
        );
    }

    pub(super) fn run_on(
        &mut self,
        branch: Option<String>,
        cmd: CommandPayload,
        caller: &Caller,
    ) -> Result<(), super::SessionError> {
        let outer = std::mem::replace(&mut self.scope, branch);
        let result = self.run_active(cmd, caller);
        self.scope = outer;
        result
    }

    pub(super) fn promote_branch(&mut self, id: &str) {
        let record = match self.branches.get(id) {
            Some(open) => Some((open.clone(), None)),
            None => self.tree.branch(id).map(|r| (r.state, r.closed)),
        };
        let Some((branch, closed)) = record else {
            tracing::warn!(branch = id, "branch.promote names no branch");
            return;
        };
        let promotable = matches!(closed, None | Some(BranchEnd::Done))
            && branch.parent.is_none()
            && self.branch_and_below(id).len() == 1;
        let Some(leaf) = branch.leaf.clone().filter(|_| promotable) else {
            tracing::warn!(
                branch = id,
                "only an open or finished branch the conversation started, with a message of its own and none open below it, can be promoted"
            );
            return;
        };
        let busy = self
            .effects
            .values()
            .any(|e| e.branch.as_deref() == Some(id) && e.tracking.is_unsettled());
        if busy {
            tracing::warn!(branch = id, "a branch with work open cannot be promoted");
            return;
        }
        if !self.has_node(&leaf) {
            tracing::warn!(branch = id, "the branch's messages are not in the log");
            return;
        }
        self.emit(EventPayload::BranchClosed(BranchClosed {
            id: id.to_string(),
            reason: BranchEnd::Promoted,
            data: serde_json::Value::Null,
            error: None,
            leaf: Some(leaf.clone()),
        }));
        self.emit(EventPayload::BranchMoved(BranchMoved {
            leaf,
            branch: None,
        }));
        self.then(|s| s.void_stranded_work(None));
    }

    pub(super) fn fail_branch(&mut self, id: &str, error: ErrorInfo) {
        self.then(|s| s.finish_branch(s.ids(), id, serde_json::Value::Null, Some(error)));
    }
}

fn same(draft: &DraftMessage, node: &crate::protocol::Message) -> bool {
    let value = |v: &DraftMessage| {
        let mut v = serde_json::to_value(v).unwrap_or_default();
        if let Some(o) = v.as_object_mut() {
            o.remove("id");
            if o.get("tool_calls")
                .is_some_and(|c| c.as_array().is_some_and(Vec::is_empty))
            {
                o.remove("tool_calls");
            }
        }
        v
    };
    value(draft) == value(&DraftMessage::from(node.clone()))
}

pub fn overflowed() -> ErrorInfo {
    ErrorInfo::new(
        ErrorCode::ContextOverflow,
        "the branch's prompt is larger than the model's window",
    )
}
