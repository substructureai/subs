use std::collections::HashSet;

use super::{CommandPayload, Working};
use crate::decision::{BranchConfig, BranchFrom, BranchStart, Trigger};
use crate::effects::{self, decision_queued_on};
use crate::events::*;
use crate::ids::Ids;
use crate::protocol::{BranchMode, DraftMessage, EffectStatus, ErrorCode, ErrorInfo, NewMessage};
use crate::reconcile::{landing_leaf, plan_reconcile};
use crate::state::{EffectKind, SessionState};
use crate::Caller;

pub const MAX_BRANCH_DEPTH: usize = 5;

impl SessionState {
    /// `id` and the open branches below it, parents first.
    pub fn branch_and_below(&self, id: &str) -> Vec<String> {
        let mut found = vec![id.to_string()];
        let mut at = 0;
        while at < found.len() {
            let parent = found[at].clone();
            found.extend(
                self.branches
                    .iter()
                    .filter(|(_, b)| b.is_open() && b.parent.as_deref() == Some(parent.as_str()))
                    .map(|(child, _)| child.clone()),
            );
            at += 1;
        }
        found
    }

    fn branch_depth(&self, branch: Option<&str>) -> usize {
        let mut depth = 0;
        let mut cursor = branch;
        while let Some(b) = cursor.and_then(|id| self.branches.get(id)) {
            depth += 1;
            if depth > self.branches.len() {
                break;
            }
            cursor = b.parent.as_deref();
        }
        depth
    }

    /// Ends `id` and every open branch below it: their work is voided and
    /// their waiting decisions dropped.
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
            events.push(EventPayload::BranchClosed(BranchClosed {
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

    /// Ends `id` and tells whoever started it.
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
        let mut events = self.close_branch(id, reason, data, error);
        let voided = parent
            .as_deref()
            .is_some_and(|p| self.open_branch(p).is_none());
        if !voided {
            events.push(decision_queued_on(ids, trigger, parent));
        }
        events
    }

    /// The blocking branches a turn ending takes with it.
    pub fn void_turn_branches(&self) -> Vec<EventPayload> {
        self.branches
            .iter()
            .filter(|(_, b)| b.is_open() && b.mode == BranchMode::Blocking)
            .filter(|(_, b)| !self.detached(b.parent.as_deref()))
            .filter(|(_, b)| b.parent.is_none())
            .flat_map(|(id, _)| {
                self.close_branch(id, BranchEnd::Voided, serde_json::Value::Null, None)
            })
            .collect()
    }

    /// Blocking branches whose fork is no longer on the conversation.
    pub fn void_stranded_branches(&self, node: Option<&str>) -> Vec<EventPayload> {
        let at = self.at(node);
        self.branches
            .iter()
            .filter(|(_, b)| b.is_open() && b.parent.is_none())
            .filter(|(_, b)| b.mode == BranchMode::Blocking)
            .filter(|(_, b)| b.from.is_some() && !at.anchor_on_path(b.from.as_deref()))
            .flat_map(|(id, _)| {
                self.close_branch(id, BranchEnd::Voided, serde_json::Value::Null, None)
            })
            .collect()
    }

    /// A branch whose decision left nothing to wait for ends, as a turn does.
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
            || self.branches.values().any(|b| {
                b.is_open() && b.parent.as_deref() == Some(id) && b.mode == BranchMode::Detached
            });
        let parked = self.at_scope(Some(id)).active_interrupt_for().is_some();
        if busy || waits || parked {
            return Vec::new();
        }
        self.finish_branch(ids, id, serde_json::Value::Null, None)
    }

    /// The branch's messages as a worker returned them: those after its
    /// newest node are written on it. A view that does not run through its
    /// newest node writes nothing.
    pub(super) fn reconcile_branch(
        &self,
        ids: &Ids,
        transcript: Vec<DraftMessage>,
        branch: &str,
    ) -> Vec<EventPayload> {
        let Some(b) = self.open_branch(branch) else {
            return Vec::new();
        };
        let known: HashSet<&str> = self.nodes.iter().map(|n| n.message.id.as_str()).collect();
        let plan = plan_reconcile(&known, &transcript);
        if plan.is_empty() {
            return Vec::new();
        }
        if landing_leaf(&transcript, &plan).as_deref() != b.at() {
            tracing::warn!(
                branch,
                "a branch's messages must follow its newest node; not written"
            );
            return Vec::new();
        }
        let mut parent = b.at().map(str::to_string);
        let mut events = Vec::with_capacity(plan.len());
        for write in plan {
            let draft = transcript[write.index].clone();
            let copied_from = write.rerecord.then(|| draft.id.clone()).flatten();
            let message = match write.rerecord {
                true => draft.rerecord(ids),
                false => draft.record(ids),
            };
            let id = message.id.clone();
            events.push(EventPayload::NewMessage(NewMessage {
                message,
                parent_id: parent.replace(id),
                copied_from,
                branch: Some(branch.to_string()),
            }));
        }
        events
    }
}

impl Working {
    /// Starts the branch an `llm.call` asked for, with its messages written
    /// after `from`. `None` when it cannot start.
    pub(super) fn start_branch(
        &mut self,
        start: BranchStart,
        messages: Vec<DraftMessage>,
    ) -> Option<String> {
        let parent = self.scope.clone();
        if self.branches.contains_key(&start.id) {
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
            let Some(at) = self.nodes.iter().find(|n| n.message.id == node) else {
                tracing::warn!(branch = %start.id, node, "branch forks at an unknown node");
                return None;
            };
            if !at.message.tool_calls.is_empty() {
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
        self.emit(EventPayload::BranchStarted(Box::new(BranchStarted {
            id: start.id.clone(),
            from: from.clone(),
            parent,
            agent,
            name: start.name.clone(),
            mode: start.mode,
            turn_id,
        })));
        let known: HashSet<String> = self.nodes.iter().map(|n| n.message.id.clone()).collect();
        let mut parent_id = from;
        for draft in messages {
            let copied_from = draft.id.clone().filter(|id| known.contains(id));
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

    /// Runs `cmd` as work on `branch`.
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

    /// The head moves to the branch's newest node. An open branch with no
    /// work out, or one that finished, can be promoted, when the
    /// conversation started it.
    pub(super) fn promote_branch(&mut self, id: &str) {
        let Some(branch) = self.branches.get(id) else {
            tracing::warn!(branch = id, "branch.promote names no branch");
            return;
        };
        let promotable = matches!(branch.closed, None | Some(BranchEnd::Done))
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
            let Some(loaded) = self.outside.branch_to(&[leaf.as_str()], &self.state) else {
                tracing::warn!(branch = id, "the branch's messages could not be loaded");
                return;
            };
            self.emit(EventPayload::BranchLoaded(BranchLoaded {
                seqs: loaded.seqs(),
            }));
            self.state.load_branch(&loaded);
        }
        self.emit(EventPayload::BranchClosed(BranchClosed {
            id: id.to_string(),
            reason: BranchEnd::Promoted,
            data: serde_json::Value::Null,
            error: None,
        }));
        self.emit(EventPayload::HeadMoved(HeadMoved { head_id: leaf }));
        self.then(|s| s.void_stranded_work(s.head_id.as_deref()));
    }

    /// A branch decision's work failed for good: the branch fails, not the
    /// run.
    pub(super) fn fail_branch(&mut self, id: &str, error: ErrorInfo) {
        self.then(|s| s.finish_branch(s.ids(), id, serde_json::Value::Null, Some(error)));
    }
}

pub fn overflowed() -> ErrorInfo {
    ErrorInfo::new(
        ErrorCode::ContextOverflow,
        "the branch's prompt is larger than the model's window",
    )
}
