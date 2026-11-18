use std::collections::HashSet;

use super::{CommandPayload, Working};
use crate::decision::{BranchConfig, BranchFrom, BranchStart, Trigger};
use crate::effects::{self, decision_queued_on};
use crate::events::*;
use crate::ids::Ids;
use crate::protocol::{BranchMode, DraftMessage, EffectStatus, ErrorCode, ErrorInfo, NewMessage};
use crate::state::{BranchId, EffectKind, SessionState};
use crate::Caller;

pub const MAX_BRANCH_DEPTH: usize = 5;

fn refused(message: &str) -> ErrorInfo {
    ErrorInfo::new(ErrorCode::Unroutable, message)
}

impl SessionState {
    pub fn branch_and_below(&self, id: &str) -> Vec<String> {
        let mut found = vec![id.to_string()];
        let mut at = 0;
        while at < found.len() {
            let parent = BranchId::from(found[at].as_str());
            found.extend(
                self.side_branches()
                    .filter(|(_, b)| b.parent == parent)
                    .map(|(child, _)| child.to_string()),
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

    pub fn branch_depth(&self, branch: &BranchId) -> usize {
        self.open_run(branch).map_or(0, |b| b.depth)
    }

    pub fn close_branch(
        &self,
        id: &str,
        reason: RunEnd,
        data: serde_json::Value,
        error: Option<ErrorInfo>,
    ) -> Vec<EventPayload> {
        if self.open_run(&BranchId::from(id)).is_none() {
            return Vec::new();
        }
        let closing: HashSet<BranchId> = self
            .branch_and_below(id)
            .into_iter()
            .map(BranchId::from)
            .collect();
        let mut events = self.void_effects_where(|e| {
            closing.contains(&e.branch) && effects::stops(e.kind(), &e.tracking)
        });
        events.extend(
            self.effects_of(EffectKind::Decision)
                .filter(|e| closing.contains(&e.branch))
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
            events.push(EventPayload::BranchFinished(BranchFinished {
                id: b,
                outcome: if own { reason } else { RunEnd::Stopped },
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
        let key = BranchId::from(id);
        let Some(branch) = self.open_run(&key) else {
            return Vec::new();
        };
        let reason = match error {
            Some(_) => RunEnd::Failed,
            None => RunEnd::Done,
        };
        let trigger = Trigger::BranchFinished {
            id: id.to_string(),
            name: branch.name.clone(),
            mode: branch.mode,
            ok: error.is_none(),
            data: data.clone(),
            error: error.clone(),
            from: branch.from.clone(),
            leaf: self.branches.get(&key).and_then(|open| open.leaf.clone()),
        };
        let parent = branch.parent.clone();
        if let Some(call) = branch.subagent.clone() {
            let mut events = self.close_branch(id, reason, data.clone(), error.clone());
            events.extend(self.subagent_branch_answer(ids, &call, data, error));
            return events;
        }
        let mut events = self.close_branch(id, reason, data, error);
        let voided = !parent.is_main() && self.open_run(&parent).is_none();
        if !voided {
            events.push(decision_queued_on(ids, trigger, parent.into()));
        }
        events
    }

    pub fn void_turn_branches(&self) -> Vec<EventPayload> {
        self.side_branches()
            .filter(|(_, b)| b.mode == BranchMode::Blocking)
            .filter(|(_, b)| !self.detached(&b.parent))
            .filter(|(_, b)| b.parent.is_main())
            .flat_map(|(id, _)| {
                self.close_branch(id, RunEnd::Voided, serde_json::Value::Null, None)
            })
            .collect()
    }

    pub fn void_stranded_branches(&self, scope: &BranchId) -> Vec<EventPayload> {
        let at = self.on(scope);
        self.side_branches()
            .filter(|(_, b)| b.parent == *scope)
            .filter(|(_, b)| b.mode == BranchMode::Blocking)
            .filter(|(_, b)| b.from.is_some() && !at.anchor_on_path(b.from.as_deref()))
            .flat_map(|(id, _)| {
                self.close_branch(id, RunEnd::Voided, serde_json::Value::Null, None)
            })
            .collect()
    }

    pub fn quiet_branch(&self, ids: &Ids, id: &str) -> Vec<EventPayload> {
        let key = BranchId::from(id);
        if self.open_run(&key).is_none() {
            return Vec::new();
        }
        let busy = self.effects.values().any(|e| {
            e.branch == key
                && match e.decision() {
                    Some(_) => e.tracking.is_unsettled(),
                    None => e.kind() != EffectKind::ConnectorSync && e.tracking.is_unsettled(),
                }
        });
        let waits = self.held_by_branches(&key) > 0
            || self
                .side_branches()
                .any(|(_, b)| b.parent == key && b.mode == BranchMode::Detached);
        let parked = self.on(&key).active_interrupt_for().is_some();
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
    ) -> Result<String, ErrorInfo> {
        if start.id.is_empty() {
            return Err(refused("A branch needs an id."));
        }
        let key = BranchId::from(start.id.as_str());
        if self.open_run(&key).is_none() {
            match self.stored_leaf(&key) {
                Some(leaf) => self.continue_branch(&start, leaf)?,
                None => self.begin_branch(&start, messages.is_empty())?,
            }
        }
        let mut parent_id = self.leaf(&key).map(str::to_string);
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
        Ok(start.id)
    }

    fn stored_leaf(&self, key: &BranchId) -> Option<String> {
        let stored = self.tree.stored(key)?;
        stored.leaf.or(stored.from)
    }

    fn run_of(&self, mode: BranchMode) -> (BranchId, usize, bool, Option<String>) {
        let parent = self.scope.clone();
        let detached = mode == BranchMode::Detached || self.detached(&parent);
        let turn_id = match mode {
            BranchMode::Blocking if !self.detached(&parent) => {
                self.active_turn_id().map(str::to_string)
            }
            _ => None,
        };
        (
            parent.clone(),
            self.branch_depth(&parent) + 1,
            detached,
            turn_id,
        )
    }

    fn begin_branch(&mut self, start: &BranchStart, bare: bool) -> Result<(), ErrorInfo> {
        let (parent, depth, detached, turn_id) = self.run_of(start.mode);
        if depth > MAX_BRANCH_DEPTH {
            return Err(refused("The branch would be nested too deep."));
        }
        let from = match &start.from {
            BranchFrom::Root => None,
            BranchFrom::Head => self.leaf(&parent).map(str::to_string),
            BranchFrom::Node(node) => Some(node.clone()),
        };
        if let Some(node) = from.as_deref() {
            if !self.has_node(node) {
                return Err(refused(
                    "The branch forks at a node that is not in the log.",
                ));
            }
            let open = self
                .tree
                .message_at(Some(node))
                .and_then(|m| self.tree.get(&m))
                .is_some_and(|n| n.message().is_some_and(|m| !m.tool_calls.is_empty()));
            if open {
                return Err(refused(
                    "A branch cannot fork at a message whose tool calls are unanswered.",
                ));
            }
        }
        if from.is_none() && bare {
            return Err(refused("A branch from the root needs messages."));
        }
        let agent = match (&start.agent, &from) {
            (BranchConfig::Inherit, None) => BranchConfig::None,
            (agent, _) => agent.clone(),
        };
        if let BranchConfig::Config(config) = &agent {
            self.then(|s| effects::connector::sync(s, config));
        }
        let node = self.ids.next();
        self.emit(EventPayload::BranchStarted(Box::new(BranchStarted {
            id: start.id.clone(),
            node,
            from,
            parent: parent.into(),
            agent,
            name: start.name.clone(),
            mode: start.mode,
            depth,
            detached,
            turn_id,
            subagent: None,
            leaf: None,
        })));
        Ok(())
    }

    fn continue_branch(&mut self, start: &BranchStart, leaf: String) -> Result<(), ErrorInfo> {
        let (parent, depth, detached, turn_id) = self.run_of(start.mode);
        if depth > MAX_BRANCH_DEPTH {
            return Err(refused("The branch would be nested too deep."));
        }
        let node = self.ids.next();
        self.emit(EventPayload::BranchStarted(Box::new(BranchStarted {
            id: start.id.clone(),
            node,
            from: None,
            parent: parent.into(),
            agent: BranchConfig::Inherit,
            name: start.name.clone(),
            mode: start.mode,
            depth,
            detached,
            turn_id,
            subagent: None,
            leaf: Some(leaf),
        })));
        Ok(())
    }

    pub(super) fn message_branch(
        &mut self,
        call_id: &str,
        messages: Vec<DraftMessage>,
    ) -> Result<String, ErrorInfo> {
        let key = BranchId::from(call_id);
        let start = |from| BranchStart {
            id: call_id.to_string(),
            from,
            agent: BranchConfig::None,
            name: None,
            mode: BranchMode::Blocking,
        };
        if self.open_run(&key).is_none() {
            match self.stored_leaf(&key) {
                Some(leaf) => self.continue_branch(&start(BranchFrom::Head), leaf)?,
                None => {
                    let (from, shared) = self.recorded_prefix(&messages);
                    let from = match from {
                        Some(node) => BranchFrom::Node(node),
                        None => BranchFrom::Root,
                    };
                    let rest = messages.into_iter().skip(shared).collect();
                    return self.start_branch(start(from), rest);
                }
            }
        }
        self.then(|s| s.write_response(s.ids(), messages, None, None, &key, None));
        Ok(call_id.to_string())
    }

    pub(super) fn refuse_call(&mut self, call: &str, error: ErrorInfo) {
        let scope = self.scope.clone().into();
        self.queue_decision_on(Trigger::llm_err(call.to_string(), error), scope);
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
            BranchMode::Blocking if !self.detached(&parent) => {
                self.active_turn_id().map(str::to_string)
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
        let key = BranchId::from(branch.as_str());
        let mut own = match self.open_run(&key) {
            Some(_) => self.leaf(&key).map(str::to_string),
            None => self.stored_leaf(&key),
        };
        let from = anchor.clone().filter(|_| on.fork && own.is_none());
        let (agent, node) = (on.agent.clone(), self.ids.next());
        let fresh = own.is_none();
        let depth = self.branch_depth(&parent) + 1;
        let detached = mode == BranchMode::Detached || self.detached(&parent);
        self.emit(EventPayload::BranchStarted(Box::new(BranchStarted {
            id: branch.clone(),
            node: node.clone(),
            from: from.clone(),
            parent: parent.into(),
            agent: agent.clone(),
            name: Some(agent_id),
            mode,
            depth,
            detached,
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
            own = self.leaf(&key).map(str::to_string);
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
        let name = self.open_run(&key).and_then(|b| b.name.clone());
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
        branch: BranchId,
        cmd: CommandPayload,
        caller: &Caller,
    ) -> Result<(), super::SessionError> {
        let outer = std::mem::replace(&mut self.scope, branch);
        let result = self.run_active(cmd, caller);
        self.scope = outer;
        result
    }

    pub(super) fn promote_branch(&mut self, id: &str) {
        let key = BranchId::from(id);
        let leaf = match self.branches.get(&key) {
            Some(open) => open.leaf.clone(),
            None => self.tree.stored(&key).and_then(|s| s.leaf),
        };
        let own = leaf
            .as_deref()
            .and_then(|l| self.tree.message_at(Some(l)))
            .and_then(|m| self.tree.get(&m))
            .is_some_and(|n| n.branch() == Some(id));
        let Some(leaf) = leaf.filter(|_| own) else {
            tracing::warn!(
                branch = id,
                "only a branch with a message of its own can be promoted"
            );
            return;
        };
        let below: HashSet<BranchId> = self
            .branch_and_below(id)
            .into_iter()
            .map(BranchId::from)
            .collect();
        let busy = self
            .effects
            .values()
            .any(|e| below.contains(&e.branch) && e.tracking.is_unsettled());
        if busy {
            tracing::warn!(branch = id, "a branch with work open cannot be promoted");
            return;
        }
        if self.open_run(&key).is_some() {
            self.then(|s| s.close_branch(id, RunEnd::Stopped, serde_json::Value::Null, None));
        }
        self.emit(EventPayload::BranchMoved(BranchMoved {
            leaf,
            branch: None,
        }));
        self.then(|s| s.void_stranded_work(&BranchId::MAIN));
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
