use std::collections::BTreeMap;

use super::{Completion, SessionError};
use crate::decision::{ToolHandler, Trigger};
use crate::effects::{decision_queued, tool};
use crate::events::*;
use crate::ids::Ids;
use crate::protocol::{AgentConfig, StoredResult, WorkerState};
use crate::protocol::{
    ClientAppend, ClientContext, ClientMessage, ClientMessages, ClientPayload, Content,
    DraftMessage, EffectStatus, NewMessage, QueueMode, Role,
};
use crate::reconcile::{landing_leaf, plan_reconcile};
use crate::state::{EffectKind, SessionState};
use crate::Caller;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Writes {
    Setup,
    Turn,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Landing {
    Follows(Option<String>),
    Forks(Option<String>),
    Strays(Option<String>),
}

impl Landing {
    pub fn node(&self) -> Option<&str> {
        match self {
            Landing::Follows(n) | Landing::Forks(n) | Landing::Strays(n) => n.as_deref(),
        }
    }
}

impl SessionState {
    pub(super) fn write_response(
        &self,
        ids: &Ids,
        transcript: Vec<DraftMessage>,
        state: Option<WorkerState>,
        config: Option<AgentConfig>,
        scope: Option<&str>,
        kind: Writes,
    ) -> Vec<EventPayload> {
        let leaf = self.scope_node(scope);
        let transcript = match scope {
            None => self.normalize_client_view(transcript),
            Some(_) => transcript,
        };
        let known = self.known_messages();
        let plan = plan_reconcile(&known, &transcript);
        let landing = self.landing_point(&transcript, scope);
        let parent = landing.node().map(str::to_string);
        let branch = scope.map(str::to_string);
        let mut cursor = parent.clone();
        let mut events = Vec::new();
        let place = |cursor: &mut Option<String>| {
            let id = ids.next();
            crate::tree::Place {
                id: id.clone(),
                parent_id: cursor.replace(id),
                branch: branch.clone(),
            }
        };
        let base = self.at(parent.as_deref());
        let state = state.filter(|s| *s != base.resolve_state_for());
        let config = config.filter(|c| Some(c) != base.resolve_agent_for().as_ref());
        let messages = !matches!(landing, Landing::Strays(_)) && !plan.is_empty();
        let opens = kind == Writes::Turn
            && scope.is_none()
            && self.tree.start_of(parent.as_deref()).is_none()
            && (state.is_some() || config.is_some() || messages);
        if opens {
            events.push(EventPayload::ConversationStarted(ConversationStarted {
                node: place(&mut cursor),
            }));
        }
        if let Some(state) = state {
            events.push(EventPayload::WorkerStateUpdated(WorkerStateUpdated {
                state,
                node: place(&mut cursor),
            }));
        }
        if let Some(config) = config {
            events.extend(crate::effects::connector::sync(self, &config));
            events.push(EventPayload::AgentConfigUpdated(AgentConfigUpdated {
                config,
                node: place(&mut cursor),
            }));
        }
        if messages {
            let mut plan = plan.iter().peekable();
            for (index, msg) in transcript.into_iter().enumerate() {
                if plan.peek().is_none_or(|w| w.index != index) {
                    continue;
                }
                let rerecord = plan.next().is_some_and(|w| w.rerecord);
                let copied_from = rerecord.then(|| msg.id.clone()).flatten();
                let message = match rerecord {
                    true => msg.rerecord(ids),
                    false => msg.record(ids),
                };
                let id = message.id.clone();
                events.push(EventPayload::NewMessage(NewMessage {
                    message,
                    parent_id: cursor.replace(id),
                    copied_from,
                    branch: branch.clone(),
                }));
            }
        }
        if events.is_empty() && scope.is_none() && parent != leaf {
            if let Some(head_id) = parent {
                events.push(EventPayload::HeadMoved(HeadMoved { head_id }));
            }
        }
        events
    }

    pub(crate) fn landing_point(
        &self,
        transcript: &[DraftMessage],
        scope: Option<&str>,
    ) -> Landing {
        let leaf = self.scope_node(scope);
        let known = self.known_messages();
        let plan = plan_reconcile(&known, transcript);
        let landing = landing_leaf(transcript, &plan);
        let at_leaf =
            transcript.is_empty() || self.tree.message_at(leaf.as_deref()) == landing.as_deref();
        let on_open_branch = landing
            .as_deref()
            .and_then(|l| self.node_branch(l))
            .is_some_and(|b| self.open_branch(b).is_some());
        match (scope, at_leaf) {
            (_, true) => Landing::Follows(leaf),
            (None, false) if !on_open_branch => match landing {
                Some(landing) => Landing::Forks(Some(landing)),
                None => match self.tree.start_of(leaf.as_deref()) {
                    Some(start) => Landing::Forks(Some(start.to_string())),
                    None => Landing::Follows(leaf),
                },
            },
            _ => {
                tracing::warn!(
                    ?scope,
                    "a response that does not follow its path; messages not written"
                );
                Landing::Strays(leaf)
            }
        }
    }

    fn pending_client_result(&self, m: &DraftMessage) -> Option<(String, String, String)> {
        if m.role != Role::Tool {
            return None;
        }
        let id = m.tool_call_id.as_deref()?;
        let effect = self.effect(EffectKind::ToolCall, id)?;
        let tc = effect.tool()?;
        if tc.handler != ToolHandler::Client || effect.tracking.status() != EffectStatus::Pending {
            return None;
        }
        let content = m
            .content
            .as_ref()
            .map(Content::text_owned)
            .unwrap_or_default();
        Some((id.to_string(), tc.name.clone(), content))
    }

    fn recorded_result_node(
        &self,
        tool_call_id: &str,
        on_path: &std::collections::HashSet<&str>,
    ) -> Option<String> {
        let mut best: Option<&str> = None;
        let mut best_on_path = false;
        for m in self.tree.iter().filter_map(|n| n.message()) {
            if m.role != Role::Tool || m.tool_call_id.as_deref() != Some(tool_call_id) {
                continue;
            }
            let on = on_path.contains(m.id.as_str());
            if on {
                best = Some(&m.id);
                best_on_path = true;
            } else if !best_on_path {
                best = Some(&m.id);
            }
        }
        best.map(str::to_string)
    }

    pub fn normalize_client_view(&self, messages: Vec<DraftMessage>) -> Vec<DraftMessage> {
        let known = self.known_messages();
        let on_path: std::collections::HashSet<&str> = self
            .head_id
            .as_deref()
            .map(|h| self.path_ids(h))
            .unwrap_or_default();
        messages
            .into_iter()
            .map(|m| {
                let is_known = m.id.as_deref().is_some_and(|id| known.contains(id));
                if m.role == Role::Tool && !is_known {
                    if let Some(node_id) = m
                        .tool_call_id
                        .as_deref()
                        .and_then(|tcid| self.recorded_result_node(tcid, &on_path))
                    {
                        return DraftMessage {
                            id: Some(node_id),
                            ..m
                        };
                    }
                }
                m
            })
            .collect()
    }

    pub(super) fn client_payload_events(
        &self,
        ids: &Ids,
        payload: ClientPayload,
        caller: &Caller,
        turn_id: Option<String>,
        mode: QueueMode,
        metadata: BTreeMap<String, String>,
    ) -> Result<(Vec<EventPayload>, Option<String>), SessionError> {
        self.ensure_owns_session(caller)?;
        match payload {
            ClientPayload::Message(ClientMessage { message, stream: _ }) => {
                let is_user = message.role == Role::User;
                if self.head_parked() && is_user {
                    return Err(SessionError::SessionInterrupted);
                }
                let mut events = Vec::new();
                if self.session_start_failed && is_user {
                    events.push(decision_queued(ids, Trigger::SessionStart));
                }
                let mut waiting = None;
                if is_user {
                    let (queued, turn) = self.client_turn(
                        ids,
                        vec![message],
                        ClientContext::default(),
                        turn_id,
                        mode,
                        metadata,
                    );
                    events.extend(queued);
                    waiting = turn;
                }
                Ok((events, waiting))
            }
            ClientPayload::Append(ClientAppend {
                messages,
                stream: _,
                client,
            }) => {
                let any_user = messages.iter().any(|m| m.role == Role::User);
                if self.head_parked() && any_user {
                    return Err(SessionError::SessionInterrupted);
                }
                let mut events = Vec::new();
                if self.session_start_failed && any_user {
                    events.push(decision_queued(ids, Trigger::SessionStart));
                }
                let (queued, waiting) =
                    self.client_turn(ids, messages, client, turn_id, mode, metadata);
                events.extend(queued);
                Ok((events, waiting))
            }
            ClientPayload::Messages(ClientMessages {
                messages,
                stream: _,
                client,
            }) => self
                .client_view_events(ids, messages, client)
                .map(|events| (events, None)),
            ClientPayload::Action(action) => Ok((
                vec![decision_queued(
                    ids,
                    Trigger::ClientAction {
                        name: action.name,
                        args: action.args,
                        turn_id,
                        metadata,
                    },
                )],
                None,
            )),
        }
    }

    fn client_turn(
        &self,
        ids: &Ids,
        messages: Vec<DraftMessage>,
        client: ClientContext,
        turn_id: Option<String>,
        mode: QueueMode,
        metadata: BTreeMap<String, String>,
    ) -> (Vec<EventPayload>, Option<String>) {
        let join = mode == QueueMode::Join;
        let (waiting, messages) = match mode {
            QueueMode::Merge => (self.waiting_merge(), messages),
            QueueMode::Join => (
                self.waiting_join(),
                messages
                    .into_iter()
                    .filter(|m| {
                        m.id.as_deref()
                            .is_none_or(|id| !self.has_node(id) && !self.holds_message(id))
                    })
                    .collect(),
            ),
            QueueMode::Refuse | QueueMode::Turn => (None, messages),
        };
        let Some(waiting) = waiting else {
            if join && messages.is_empty() {
                return (Vec::new(), None);
            }
            let queued = vec![decision_queued(
                ids,
                Trigger::ClientMessage {
                    messages,
                    client,
                    turn_id: turn_id.clone(),
                    merge: mode == QueueMode::Merge,
                    join,
                    metadata,
                },
            )];
            return (queued, turn_id);
        };
        let held: std::collections::HashSet<&str> = waiting
            .messages
            .iter()
            .filter_map(|m| m.id.as_deref())
            .collect();
        let fresh: Vec<DraftMessage> = messages
            .into_iter()
            .filter(|m| m.id.as_deref().is_none_or(|id| !held.contains(id)))
            .collect();
        let queued = vec![EventPayload::DecisionAmended(DecisionAmended {
            id: waiting.decision_id.to_string(),
            messages: fresh,
            client,
            metadata,
        })];
        (queued, Some(waiting.turn_id.to_string()))
    }

    fn edits_as_new(&self, messages: Vec<DraftMessage>) -> Vec<DraftMessage> {
        let text = |c: Option<&Content>| c.map(Content::text_owned).unwrap_or_default();
        messages
            .into_iter()
            .map(|m| {
                let edited = m.role == Role::User
                    && m.id.as_deref().is_some_and(|id| {
                        self.tree
                            .get(id)
                            .and_then(|n| n.message())
                            .is_some_and(|n| {
                                n.role == Role::User
                                    && text(n.content.as_ref()) != text(m.content.as_ref())
                            })
                    });
                match edited {
                    true => DraftMessage { id: None, ..m },
                    false => m,
                }
            })
            .collect()
    }

    fn client_view_events(
        &self,
        ids: &Ids,
        messages: Vec<DraftMessage>,
        client: ClientContext,
    ) -> Result<Vec<EventPayload>, SessionError> {
        let messages = self.normalize_client_view(self.edits_as_new(messages));

        let mut seen = std::collections::HashSet::new();
        let completions: Vec<Completion> = messages
            .iter()
            .enumerate()
            .filter_map(|(index, m)| {
                self.pending_client_result(m)
                    .map(|(tc, name, result)| Completion {
                        index,
                        tool_call_id: tc,
                        name,
                        result,
                    })
            })
            .filter(|c| seen.insert(c.tool_call_id.clone()))
            .collect();

        let (landing, single_answer) = {
            let known = self.known_messages();
            let plan = plan_reconcile(&known, &messages);
            let landing = self
                .landing_point(&messages, None)
                .node()
                .map(str::to_string);
            let single_answer = plan.len() == 1
                && completions.len() == 1
                && plan.first().map(|w| w.index) == completions.first().map(|c| c.index);
            (landing, single_answer)
        };

        if completions.is_empty() && self.at(landing.as_deref()).active_interrupt_for().is_some() {
            return Err(SessionError::SessionInterrupted);
        }

        if single_answer {
            return Ok(match completions.into_iter().next() {
                Some(c) => {
                    tool::complete(ids, c.tool_call_id, c.name, StoredResult::text(c.result))
                }
                None => Vec::new(),
            });
        }

        let mut events: Vec<EventPayload> = completions
            .into_iter()
            .map(|c| {
                EventPayload::ToolCallCompleted(ToolCallCompleted {
                    id: c.tool_call_id,
                    name: c.name,
                    result: StoredResult::text(c.result),
                })
            })
            .collect();
        events.push(decision_queued(
            ids,
            Trigger::ClientTranscript {
                messages,
                new_from: 0,
                client,
            },
        ));
        Ok(events)
    }
}
