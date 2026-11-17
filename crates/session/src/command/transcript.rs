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
use crate::state::{EffectKind, SessionState};
use crate::Caller;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Landed {
    pub at: Option<String>,
    pub shared: usize,
    pub forks: bool,
}

impl Landed {
    fn stay(at: Option<String>) -> Self {
        Landed {
            at,
            shared: 0,
            forks: false,
        }
    }
}

fn settled(view: &[DraftMessage]) -> &[DraftMessage] {
    let end = view
        .iter()
        .rposition(|m| m.role != Role::User)
        .map_or(0, |i| i + 1);
    &view[..end]
}

impl SessionState {
    pub(super) fn write_response(
        &self,
        ids: &Ids,
        transcript: Vec<DraftMessage>,
        state: Option<WorkerState>,
        config: Option<AgentConfig>,
        scope: Option<&str>,
        read: Option<String>,
    ) -> Vec<EventPayload> {
        let leaf = self.scope_node(scope);
        let transcript = match scope {
            None => self.normalize_client_view(transcript),
            Some(_) => transcript,
        };
        let landed = match transcript.is_empty() {
            true => Landed::stay(leaf.clone()),
            false => self.land(&transcript, scope),
        };
        let moved = landed.at != leaf;
        let branch = scope.map(str::to_string);
        let mut cursor = landed.at.clone();
        let mut events = Vec::new();
        for draft in transcript.into_iter().skip(landed.shared) {
            let copied_from = draft.id.clone().filter(|id| self.has_node(id));
            let message = match copied_from {
                Some(_) => draft.rerecord(ids),
                None => draft.record(ids),
            };
            let id = message.id.clone();
            events.push(EventPayload::NewMessage(NewMessage {
                message,
                parent_id: cursor.replace(id),
                copied_from,
                branch: branch.clone(),
            }));
        }
        let place = |cursor: &mut Option<String>| {
            let id = ids.next();
            crate::tree::Place {
                id: id.clone(),
                parent_id: cursor.replace(id),
                branch: branch.clone(),
            }
        };
        let base = self.at(landed.at.as_deref());
        let read = self.at(read.as_deref());
        let state = state
            .or_else(|| landed.forks.then(|| read.worker_state()))
            .filter(|s| *s != base.worker_state());
        let config = config
            .or_else(|| landed.forks.then(|| read.config()).flatten())
            .filter(|c| Some(c) != base.config().as_ref());
        if let Some(state) = state {
            events.push(EventPayload::StateUpdated(StateUpdated {
                state,
                node: place(&mut cursor),
            }));
        }
        if let Some(config) = config {
            events.extend(crate::effects::connector::sync(self, &config));
            events.push(EventPayload::ConfigUpdated(ConfigUpdated {
                config,
                node: place(&mut cursor),
            }));
        }
        if events.is_empty() && moved {
            if let Some(head_id) = landed.at {
                events.push(EventPayload::BranchMoved(BranchMoved {
                    leaf: head_id,
                    branch,
                }));
            }
        }
        events
    }

    pub(crate) fn land(&self, view: &[DraftMessage], scope: Option<&str>) -> Landed {
        self.land_view(view, scope, false)
    }

    fn land_view(&self, view: &[DraftMessage], scope: Option<&str>, regenerate: bool) -> Landed {
        let leaf = self.scope_node(scope);
        let line = std::cell::OnceCell::new();
        let on_line = |id: &str| {
            line.get_or_init(|| {
                leaf.as_deref()
                    .map(|h| self.path_ids(h))
                    .unwrap_or_default()
            })
            .contains(id)
        };
        let full = self.tree.shared(view, |_, node| {
            scope.is_some() || node.branch().is_none() || on_line(node.id())
        });
        let shared = match regenerate && full == view.len() {
            true => settled(view).len(),
            false => full,
        };
        let last = |n: usize| n.checked_sub(1).and_then(|i| view[i].id.as_deref());
        let floor = scope
            .and_then(|b| self.branches.get(b))
            .and_then(|b| b.from.as_deref());
        if let Some(at) = self.tree.fork_on(leaf.as_deref(), last(full), floor) {
            let at = match shared == full {
                true => at,
                false => self
                    .tree
                    .fork_on(leaf.as_deref(), last(shared), floor)
                    .flatten(),
            };
            return Landed {
                forks: at != leaf,
                at,
                shared,
            };
        }
        match (scope, last(shared)) {
            (None, Some(message)) => {
                let (at, forks) = self.tree.tail(message);
                Landed {
                    at: Some(at),
                    shared,
                    forks,
                }
            }
            _ => Landed {
                at: self.tree.fork_on(leaf.as_deref(), None, floor).flatten(),
                shared,
                forks: true,
            },
        }
    }

    fn view_point(&self, view: &[DraftMessage]) -> Landed {
        self.land_view(view, None, true)
    }

    pub fn read_point(&self, trigger: &Trigger, branch: Option<&str>) -> Landed {
        match (branch, trigger) {
            (None, Trigger::ClientTranscript { messages, .. }) => self.view_point(messages),
            _ => Landed::stay(self.scope_node(branch)),
        }
    }

    pub(super) fn drop_recorded(&self, payload: ClientPayload) -> Option<ClientPayload> {
        let head = std::cell::OnceCell::new();
        let fresh = |m: &DraftMessage| {
            m.id.as_deref().is_none_or(|id| {
                !self.has_node(id)
                    || head
                        .get_or_init(|| {
                            self.head_id
                                .as_deref()
                                .map(|h| self.path_ids(h))
                                .unwrap_or_default()
                        })
                        .contains(id)
            })
        };
        match payload {
            ClientPayload::Message(message) => {
                fresh(&message.message).then_some(ClientPayload::Message(message))
            }
            ClientPayload::Append(mut append) => {
                append.messages.retain(fresh);
                (!append.messages.is_empty()).then_some(ClientPayload::Append(append))
            }
            other => Some(other),
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
        on_path: &std::collections::HashSet<String>,
    ) -> Option<String> {
        let answers = self.tree.answers(tool_call_id);
        answers
            .iter()
            .find(|n| on_path.contains(n.id()))
            .or(answers.first())
            .map(|n| n.id().to_string())
    }

    pub fn normalize_client_view(&self, messages: Vec<DraftMessage>) -> Vec<DraftMessage> {
        let mut on_path = None;
        messages
            .into_iter()
            .map(|m| {
                let unknown = m.id.as_deref().is_none_or(|id| !self.has_node(id));
                let tool_call = m.tool_call_id.as_deref().filter(|_| m.role == Role::Tool);
                let recorded = tool_call.filter(|_| unknown).and_then(|tcid| {
                    let path = on_path.get_or_insert_with(|| {
                        self.head_id
                            .as_deref()
                            .map(|h| self.path_ids(h))
                            .unwrap_or_default()
                    });
                    self.recorded_result_node(tcid, path)
                });
                match recorded {
                    Some(node_id) => DraftMessage {
                        id: Some(node_id),
                        ..m
                    },
                    None => m,
                }
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
                        self.tree.get(id).is_some_and(|node| {
                            node.message().is_some_and(|n| {
                                n.role == Role::User
                                    && text(n.content.as_ref()) != text(m.content.as_ref())
                            })
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

        let landing = self.view_point(&messages).at;
        let single_answer = completions.len() == 1
            && completions[0].index + 1 == messages.len()
            && self
                .tree
                .shared(&messages, |_, node| node.branch().is_none())
                + 1
                == messages.len();

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
