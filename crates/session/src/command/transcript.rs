use std::collections::BTreeMap;

use super::{Completion, SessionError};
use crate::decision::{ToolHandler, Trigger};
use crate::effects::{decision_queued, tool};
use crate::events::*;
use crate::ids::Ids;
use crate::protocol::StoredResult;
use crate::protocol::{
    ClientAppend, ClientContext, ClientMessage, ClientMessages, ClientPayload, Content,
    DraftMessage, EffectStatus, NewMessage, Role,
};
use crate::reconcile::{landing_leaf, plan_reconcile};
use crate::state::{EffectKind, SessionState};
use crate::Caller;

impl SessionState {
    pub(super) fn reconcile_transcript(
        &self,
        ids: &Ids,
        transcript: Vec<DraftMessage>,
    ) -> (Vec<EventPayload>, Option<String>) {
        let transcript = self.normalize_client_view(transcript);
        let known: std::collections::HashSet<&str> =
            self.nodes.iter().map(|n| n.message.id.as_str()).collect();
        let plan = plan_reconcile(&known, &transcript);
        let mut events = Vec::with_capacity(plan.len());
        let mut plan_iter = plan.iter().peekable();
        let mut parent: Option<String> = None;
        for (index, msg) in transcript.into_iter().enumerate() {
            match plan_iter.peek() {
                Some(write) if write.index == index => {
                    let rerecord = write.rerecord;
                    let copied_from = rerecord.then(|| msg.id.clone()).flatten();
                    plan_iter.next();
                    let msg = if rerecord {
                        msg.rerecord(ids)
                    } else {
                        msg.record(ids)
                    };
                    let id = msg.id.clone();
                    events.push(EventPayload::NewMessage(NewMessage {
                        message: msg,
                        parent_id: parent.take(),
                        copied_from,
                    }));
                    parent = Some(id);
                }
                _ => parent = msg.id,
            }
        }
        if !events.is_empty() {
            return (events, parent);
        }
        match parent {
            Some(leaf) if self.head_id.as_deref() != Some(leaf.as_str()) => {
                let moved = EventPayload::HeadMoved(HeadMoved {
                    head_id: leaf.clone(),
                });
                (vec![moved], Some(leaf))
            }
            _ => (events, self.head_id.clone()),
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
        for m in self.nodes.iter().map(|n| &n.message) {
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
        let known: std::collections::HashSet<&str> =
            self.nodes.iter().map(|n| n.message.id.as_str()).collect();
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
        merge: bool,
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
                        merge,
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
                    self.client_turn(ids, messages, client, turn_id, merge, metadata);
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
        merge: bool,
        metadata: BTreeMap<String, String>,
    ) -> (Vec<EventPayload>, Option<String>) {
        let waiting = merge.then(|| self.waiting_merge()).flatten();
        let Some(waiting) = waiting else {
            let queued = vec![decision_queued(
                ids,
                Trigger::ClientMessage {
                    messages,
                    client,
                    turn_id: turn_id.clone(),
                    merge,
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

    fn client_view_events(
        &self,
        ids: &Ids,
        messages: Vec<DraftMessage>,
        client: ClientContext,
    ) -> Result<Vec<EventPayload>, SessionError> {
        let messages = self.normalize_client_view(messages);

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
            let known: std::collections::HashSet<&str> =
                self.nodes.iter().map(|n| n.message.id.as_str()).collect();
            let plan = plan_reconcile(&known, &messages);
            if plan.first().map(|w| w.index) == Some(0) && !self.nodes.is_empty() {
                tracing::warn!(
                    "client view shares no prefix with the session; recording will fork at the root"
                );
            }
            let landing = landing_leaf(&messages, &plan);
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
