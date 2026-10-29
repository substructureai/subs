use std::collections::HashMap;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::attachments;
use crate::decision::Trigger;
use crate::ids::Ids;
use crate::propose::{propose, Proposing};
use crate::protocol::{
    AgentConfig, ClientContext, DecisionRequest, DecisionResponse, DecisionTrigger, DecisionTurn,
    DeferToolsStrategy, DraftMessage, Effect, InterruptResumption, LlmFormat, LlmRequest, Message,
    MessageTree, SessionOwner, WorkerIdentity, WorkerRef, WorkerState,
};
use crate::reconcile::news_start;
use crate::session::Session;
use crate::span::SpanContext;
use crate::state::{EffectState, EventMeta};
use crate::tool_contract::{classify_arguments, declared_tool, DeclaredTool};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PendingPrompt {
    pub request: LlmRequest,
    pub format: Option<LlmFormat>,
    pub defer_tools_strategy: DeferToolsStrategy,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkerDecisionRequest {
    pub session_id: String,
    pub decision_id: String,
    pub agent_id: String,
    pub identity: SessionOwner,
    pub trigger: DecisionTrigger,

    #[serde(default)]
    pub proposed: DecisionResponse,
    pub state: WorkerState,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<AgentConfig>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worker: Option<WorkerRef>,
    #[serde(default)]
    pub calls: Vec<Effect>,
    #[serde(default)]
    pub pending_calls: usize,

    #[serde(default, rename = "messages")]
    pub transcript: Vec<Message>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ancestry: Vec<String>,

    #[serde(skip_serializing, default = "SpanContext::root")]
    pub span: SpanContext,
    pub attempts: u32,
    pub deadline: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn: Option<DecisionTurn>,
}

impl WorkerDecisionRequest {
    pub fn tenant_id(&self) -> &str {
        &self.identity.tenant_id
    }
}

impl<'a> From<&'a WorkerDecisionRequest> for DecisionRequest<'a> {
    fn from(r: &'a WorkerDecisionRequest) -> Self {
        DecisionRequest {
            session_id: &r.session_id,
            decision_id: &r.decision_id,
            agent_id: &r.agent_id,
            identity: WorkerIdentity {
                requester: r.identity.requester.clone(),
                metadata: r.identity.metadata.clone(),
            },
            trigger: &r.trigger,
            proposed: &r.proposed,
            state: &r.state,
            agent: &r.agent,
            worker: &r.worker,
            calls: &r.calls,
            pending_calls: r.pending_calls,
            messages: &r.transcript,
            ancestry: &r.ancestry,
            attempts: r.attempts,
            deadline: &r.deadline,
            turn: &r.turn,
        }
    }
}

impl Session {
    pub fn decision_request(
        &self,
        decision_id: &str,
        seq: u64,
        meta: &EventMeta,
        span: &SpanContext,
    ) -> Option<(WorkerDecisionRequest, Option<PendingPrompt>)> {
        let agent_id = meta.agent_id.as_ref()?;
        let owner = meta.owner.as_ref()?;
        let decision = meta
            .decisions
            .iter()
            .find(|d| d.decision_id == decision_id)?;
        let worker_decision = self.state.worker_decision(decision_id)?;
        let mut trigger = worker_decision.trigger.clone();
        let worker = self.state.worker.clone();
        let at = self.state.at(meta.head_id.as_deref()).until(seq);
        let message_tree = at.message_tree();
        let transcript = message_tree
            .head_id
            .as_deref()
            .map(|h| message_tree.path_to(h))
            .unwrap_or_default();
        let open_llm_calls = at.open_llm_calls();
        let pending_calls = meta.pending_work(decision_id);
        let worker_state = at.resolve_state_for();
        let agent_config = at.resolve_agent_for();
        if let (
            Some(rules),
            Trigger::ClientMessage { messages, .. } | Trigger::ClientTranscript { messages, .. },
        ) = (
            agent_config.as_ref().and_then(|c| c.attachments.as_ref()),
            &mut trigger,
        ) {
            attachments::mark(messages, rules, &message_tree);
        }
        let (trigger, prompt) = wire_trigger(trigger, &transcript, &message_tree, &open_llm_calls);
        let proposed = propose(
            &trigger,
            &Proposing {
                state: at,
                pending_calls,
            },
        )
        .unwrap_or_default();
        let request = WorkerDecisionRequest {
            session_id: self.id.clone(),
            decision_id: decision_id.to_string(),
            agent_id: agent_id.clone(),
            identity: owner.clone(),
            trigger,
            proposed,
            state: worker_state,
            agent: agent_config,
            worker,
            calls: meta.calls.clone(),
            pending_calls,
            transcript,
            ancestry: meta.ancestry.clone(),
            span: span.clone(),
            attempts: decision.attempts,
            deadline: decision.deadline,
            turn: worker_decision.turn.as_deref().cloned(),
        };
        Some((request, prompt))
    }

    pub(crate) fn resumed_request(
        &self,
        decision_id: &str,
        seq: u64,
    ) -> Option<(WorkerDecisionRequest, Option<PendingPrompt>)> {
        let at = self.last_event_at.unwrap_or_default();
        let meta = self.state.event_meta(at);
        let span = Ids::new(&format!("{}/resume", self.id), seq, at).span("resume");
        self.decision_request(decision_id, seq, &meta, &span)
    }
}

fn appended_transcript(
    messages: Vec<DraftMessage>,
    client: ClientContext,
    active_path: &[Message],
    tree: &MessageTree,
) -> DecisionTrigger {
    let known: std::collections::HashSet<&str> =
        tree.nodes.iter().map(|n| n.message.id.as_str()).collect();
    let mut view: Vec<DraftMessage> = active_path
        .iter()
        .cloned()
        .map(DraftMessage::from)
        .collect();
    let new_from = view.len();
    view.extend(
        messages
            .into_iter()
            .filter(|m| m.id.as_deref().is_none_or(|id| !known.contains(id))),
    );
    DecisionTrigger::ClientTranscript {
        messages: view,
        new_from,
        client,
    }
}

pub fn wire_trigger(
    trigger: Trigger,
    active_path: &[Message],
    tree: &MessageTree,
    open_llm_calls: &HashMap<String, EffectState>,
) -> (DecisionTrigger, Option<PendingPrompt>) {
    let mut pending = None;
    let trigger = match trigger {
        Trigger::SessionStart => DecisionTrigger::SessionStart,
        Trigger::ClientMessage {
            messages, client, ..
        } => appended_transcript(messages, client, active_path, tree),
        Trigger::SubagentNotice { messages, .. } => {
            appended_transcript(messages, ClientContext::default(), active_path, tree)
        }
        Trigger::ClientTranscript {
            messages, client, ..
        } => {
            let known: std::collections::HashSet<&str> =
                tree.nodes.iter().map(|n| n.message.id.as_str()).collect();
            let new_from = news_start(&known, &messages);
            DecisionTrigger::ClientTranscript {
                messages,
                new_from,
                client,
            }
        }
        Trigger::ClientAction { name, args, .. } => DecisionTrigger::ClientAction { name, args },
        Trigger::ContextLimit {
            reason,
            strategy,
            tokens,
            window,
            call,
        } => DecisionTrigger::ContextLimit {
            reason,
            strategy,
            tokens,
            window,
            call,
        },
        Trigger::CodemodeCalls { id, tool_calls } => {
            DecisionTrigger::CodemodeCalls { id, tool_calls }
        }
        Trigger::ToolExecute {
            id,
            name,
            arguments,
            attempt,
            deadline,
        } => {
            let schema = match declared_tool(&id, &name, active_path, |id| {
                open_llm_calls.get(id).and_then(|e| e.llm())
            }) {
                DeclaredTool::Declared(t) => t.input.as_ref(),
                _ => None,
            };
            let input = classify_arguments(&arguments, schema);
            DecisionTrigger::ToolExecute {
                id,
                name,
                arguments,
                input,
                attempt,
                deadline,
            }
        }
        Trigger::LlmExecute {
            id,
            request,
            format,
            defer_tools_strategy,
            stream,
            attempt,
            deadline,
        } => {
            let neutral = serde_json::to_value(&request).unwrap_or_default();
            pending = Some(PendingPrompt {
                request,
                format,
                defer_tools_strategy,
            });
            DecisionTrigger::LlmExecute {
                id,
                request: neutral,
                format,
                stream,
                attempt,
                deadline,
            }
        }
        Trigger::ToolFinished {
            id,
            ok,
            name,
            result,
            error,
        } => DecisionTrigger::ToolFinished {
            id,
            ok,
            name,
            result,
            error,
        },
        Trigger::SubagentFinished {
            id,
            ok,
            session_id,
            agent_id,
            result,
            error,
        } => DecisionTrigger::SubagentFinished {
            id,
            ok,
            session_id,
            agent_id,
            result,
            error,
        },
        Trigger::System1Finished {
            id,
            ok,
            model,
            answers,
            usage,
            cost,
            error,
        } => DecisionTrigger::System1Finished {
            id,
            ok,
            model,
            answers,
            usage,
            cost,
            error,
        },
        Trigger::LlmFinished {
            id,
            ok,
            message,
            truncated,
            refused,
            usage,
            cost,
            error,
        } => DecisionTrigger::LlmFinished {
            id,
            ok,
            message,
            truncated,
            refused,
            usage,
            cost,
            error,
        },
        Trigger::InterruptResumed {
            interrupt_id,
            payload,
        } => DecisionTrigger::InterruptResumed {
            resumption: InterruptResumption {
                interrupt_id,
                payload,
            },
        },
        Trigger::TurnFinished {
            turn_id,
            data,
            cost,
            usage,
        } => DecisionTrigger::TurnFinished {
            turn_id,
            data,
            cost,
            usage,
        },
    };
    (trigger, pending)
}
