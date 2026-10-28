use std::fmt;

use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

use crate::command::{CommandPayload, Outcome};
use crate::connectors::{ConnectionPath, Requester};
use crate::decision::{LlmHandler, ToolHandler};
use crate::entry::{Command, CommandId};
use crate::events::{ConnectorTarget, EventPayload};
use crate::protocol::{
    DeferToolsStrategy, DraftMessage, EffectKind, EffectStatus, LlmRequest, RetryPolicy, Role,
    SessionOwner, Usage,
};
use crate::request::{PendingPrompt, WorkerDecisionRequest};
use crate::session::{Session, SessionEvent};
use crate::state::{title, EffectState, SessionStatus};
use crate::system1::{transcript_state, System1Request};

#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct WorkId {
    pub kind: EffectKind,
    pub id: String,
    pub attempt: Option<u32>,
}

impl WorkId {
    pub fn new(kind: EffectKind, id: impl Into<String>, attempt: Option<u32>) -> Self {
        Self {
            kind,
            id: id.into(),
            attempt,
        }
    }

    pub fn settle_id(&self) -> CommandId {
        CommandId(format!("settle:{self}"))
    }
}

impl Work {
    pub fn id(&self) -> &WorkId {
        match self {
            Work::CallLlm { work, .. }
            | Work::CallSystem1 { work, .. }
            | Work::CallTool { work, .. }
            | Work::ListTools { work, .. }
            | Work::AskWorker { work, .. }
            | Work::StartSubagent { work, .. } => work,
        }
    }
}

impl fmt::Display for WorkId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.attempt {
            Some(attempt) => write!(f, "{}:{}:{attempt}", self.kind.label(), self.id),
            None => write!(f, "{}:{}", self.kind.label(), self.id),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LlmCall {
    pub call_id: String,
    pub attempt: u32,
    pub llm: String,
    pub request: LlmRequest,
    pub defer_tools_strategy: DeferToolsStrategy,
    pub stream: bool,
    pub owner: SessionOwner,
    pub agent_id: String,
    pub ancestry: Vec<String>,
    pub turn_id: Option<String>,
    pub retry: RetryPolicy,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct System1Call {
    pub call_id: String,
    pub attempt: u32,
    pub system1: String,
    pub request: System1Request,
    pub owner: SessionOwner,
    pub agent_id: String,
    pub retry: RetryPolicy,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "target", rename_all = "snake_case")]
pub enum ToolTarget {
    Remote {
        path: ConnectionPath,
        name: String,
        arguments: serde_json::Value,
        requester: Requester,
    },
    Local,
    /// Run a `run_code` script past the calls it already made. The runner
    /// reads the script from the session with [`Session::script`].
    Script,
}

pub use crate::codemode::{Script, ScriptRecord};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCall {
    pub tool_call_id: String,
    pub attempt: u32,
    pub target: ToolTarget,
    pub retry: RetryPolicy,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Subagent {
    pub child: String,
    pub agent_id: String,
    pub owner: SessionOwner,
    pub ancestry: Vec<String>,
    pub message: Option<DraftMessage>,
    pub retry: RetryPolicy,
    pub events: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Summary {
    pub session_id: String,
    pub tenant_id: String,
    pub seq: u64,
    pub status: SessionStatus,
    pub agent_id: Option<String>,
    pub top_level: bool,
    pub turn_id: Option<String>,
    pub cost: Decimal,
    pub subagent_cost: Decimal,
    pub first_event_at: Option<DateTime<Utc>>,
    pub last_event_at: Option<DateTime<Utc>>,
    pub wake_at: Option<DateTime<Utc>>,
    /// The first thing the user said, as a one-line title.
    pub title: Option<String>,
    /// The owner's subject id: who the session is with.
    pub user_id: Option<String>,
    pub turns: u64,
    pub running: bool,
    pub last_turn_failed: bool,
    /// The session's own usage and its subagents' together.
    pub usage: Usage,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Work {
    CallLlm {
        work: WorkId,
        call: Box<LlmCall>,
    },
    CallSystem1 {
        work: WorkId,
        call: Box<System1Call>,
    },
    CallTool {
        work: WorkId,
        call: ToolCall,
    },
    ListTools {
        work: WorkId,
        path: ConnectionPath,
        requester: Requester,
        retry: RetryPolicy,
    },
    AskWorker {
        work: WorkId,
        request: Box<WorkerDecisionRequest>,
        prompt: Option<Box<PendingPrompt>>,
        seq: u64,
    },
    StartSubagent {
        work: WorkId,
        subagent: Box<Subagent>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Mail {
    pub to: String,
    pub command: Command,
}

impl Session {
    pub fn summary(&self) -> Summary {
        let state = &self.state;
        let meta = state.event_meta(self.last_event_at.unwrap_or_default());
        let running = state.phase.turn_id().is_some();
        let title = state.title.clone().or_else(|| {
            state
                .nodes
                .iter()
                .find(|n| n.message.role == Role::User)
                .and_then(|n| n.message.content.as_ref())
                .and_then(|c| title(&c.text_owned()))
        });
        Summary {
            session_id: self.id.clone(),
            tenant_id: self.tenant_id.clone(),
            seq: self.seq,
            status: state.status.clone(),
            agent_id: state.agent_id.clone(),
            top_level: state.ancestry.is_empty(),
            turn_id: state.turn_id().map(str::to_string),
            cost: meta.cost,
            subagent_cost: meta.subagent_cost,
            first_event_at: self.first_event_at,
            last_event_at: self.last_event_at,
            wake_at: self.wake_at,
            title,
            user_id: state
                .owner
                .as_ref()
                .and_then(|o| o.requester.subject.as_ref())
                .map(|s| s.id.clone()),
            turns: state.completed_turn_ids.len() as u64 + u64::from(running),
            running,
            last_turn_failed: state.last_turn_failed,
            usage: {
                let mut usage = state.token_usage.clone();
                usage.add(&state.subagent_token_usage);
                usage
            },
        }
    }

    pub fn open_work(&self) -> Vec<Work> {
        let state = &self.state;
        state
            .effects
            .iter()
            .filter_map(|((kind, id), effect)| {
                let attempt = effect.tracking.retry.attempts;
                match kind {
                    EffectKind::LlmCall => self.llm(id, attempt),
                    EffectKind::System1Call => self.system1(id, attempt),
                    EffectKind::ToolCall => self.tool(id, attempt),
                    EffectKind::Subagent if effect.tracking.status() == EffectStatus::Pending => {
                        self.subagent(id)
                    }
                    EffectKind::Decision => {
                        let seq = self
                            .open(EffectKind::Decision, id, None)?
                            .decision()?
                            .built_at();
                        let (request, prompt) = self.resumed_request(id, seq)?;
                        Some(Work::AskWorker {
                            work: WorkId::new(EffectKind::Decision, id.clone(), Some(attempt)),
                            request: Box::new(request),
                            prompt: prompt.map(Box::new),
                            seq,
                        })
                    }
                    EffectKind::ConnectorSync => {
                        self.open(EffectKind::ConnectorSync, id, Some(attempt))?;
                        Some(Work::ListTools {
                            work: WorkId::new(EffectKind::ConnectorSync, id.clone(), Some(attempt)),
                            path: ConnectionPath::parse(id)?,
                            requester: Requester::of_owner(state.owner.as_ref()),
                            retry: effect.tracking.retry_policy.clone(),
                        })
                    }
                    _ => None,
                }
            })
            .collect()
    }

    pub fn work(&self, events: &[SessionEvent]) -> Vec<Work> {
        let mut seen = std::collections::HashSet::new();
        events
            .iter()
            .flat_map(|event| self.work_for(event))
            .filter(|work| seen.insert(work.id().clone()))
            .collect()
    }

    fn work_for(&self, event: &SessionEvent) -> Vec<Work> {
        let state = &self.state;
        match &event.payload {
            EventPayload::LlmCallDispatched(d) => self.llm(&d.id, d.attempt).into_iter().collect(),
            EventPayload::System1CallDispatched(d) => {
                self.system1(&d.id, d.attempt).into_iter().collect()
            }
            EventPayload::ToolCallDispatched(d) => {
                self.tool(&d.id, d.attempt).into_iter().collect()
            }
            EventPayload::ToolCallCompleted(c) => self.script_resumed(&c.id).into_iter().collect(),
            EventPayload::ToolCallErrored(e) => self.script_resumed(&e.id).into_iter().collect(),
            EventPayload::ConnectorSyncRequested(req) => self
                .open(
                    EffectKind::ConnectorSync,
                    &req.path.to_string(),
                    Some(req.attempt),
                )
                .map(|_| Work::ListTools {
                    work: WorkId::new(
                        EffectKind::ConnectorSync,
                        req.path.to_string(),
                        Some(req.attempt),
                    ),
                    path: req.path.clone(),
                    requester: Requester::of_owner(state.owner.as_ref()),
                    retry: req.retry.clone(),
                })
                .into_iter()
                .collect(),
            EventPayload::DecisionDispatched(d) => self
                .open(EffectKind::Decision, &d.id, None)
                .and_then(|effect| {
                    let attempt = effect.tracking.retry.attempts;
                    let request =
                        self.decision_request(&d.id, event.seq, &event.meta, &event.span)?;
                    Some((attempt, request))
                })
                .map(|(attempt, (request, prompt))| Work::AskWorker {
                    work: WorkId::new(EffectKind::Decision, d.id.clone(), Some(attempt)),
                    request: Box::new(request),
                    prompt: prompt.map(Box::new),
                    seq: event.seq,
                })
                .into_iter()
                .collect(),
            EventPayload::SubagentDispatched(d) => self.subagent(&d.id).into_iter().collect(),
            _ => Vec::new(),
        }
    }

    pub fn mail(&self, events: &[SessionEvent]) -> Vec<Mail> {
        events
            .iter()
            .flat_map(|event| self.mail_for(event))
            .collect()
    }

    fn mail_for(&self, event: &SessionEvent) -> Vec<Mail> {
        let state = &self.state;
        match &event.payload {
            EventPayload::SessionMessageRequested(req) => vec![Mail {
                to: req.target_session_id.clone(),
                command: self.caused_by(
                    event,
                    CommandPayload::SendMessage {
                        message: req.message.clone(),
                        stream: false,
                        turn_id: Some(event.id.to_string()),
                        parent_id: None,
                    },
                ),
            }],
            EventPayload::CallVoided(v) if v.kind == EffectKind::Subagent => state
                .subagent(&v.id)
                .map(|sa| Mail {
                    to: sa.session_id.clone(),
                    command: self.caused_by(event, CommandPayload::CancelSession),
                })
                .into_iter()
                .collect(),
            EventPayload::TurnCompleted(tc) => event
                .meta
                .ancestry
                .last()
                .map(|parent| Mail {
                    to: parent.clone(),
                    command: self.caused_by(
                        event,
                        CommandPayload::CompleteSubagentTurn {
                            session_id: self.id().to_string(),
                            agent_id: event.meta.agent_id.clone().unwrap_or_default(),
                            turn_id: tc.turn_id.clone(),
                            data: tc.data.clone(),
                            cost: tc.turn_cost,
                            token_usage: tc.turn_token_usage.clone(),
                            error: tc.error.clone(),
                        },
                    ),
                })
                .into_iter()
                .collect(),
            _ => Vec::new(),
        }
    }

    pub fn is_open(&self, work: &WorkId) -> bool {
        self.open(work.kind, &work.id, work.attempt).is_some()
    }

    fn caused_by(&self, event: &SessionEvent, payload: CommandPayload) -> Command {
        Command::system(event.id.to_string(), self.tenant_id(), payload)
    }

    fn open(&self, kind: EffectKind, id: &str, attempt: Option<u32>) -> Option<&EffectState> {
        let effect = self.state.effect(kind, id)?;
        let running = matches!(
            effect.tracking.status(),
            EffectStatus::Pending | EffectStatus::Running
        );
        let current = attempt.is_none_or(|a| a == effect.tracking.retry.attempts);
        (running && current).then_some(effect)
    }

    fn llm(&self, id: &str, attempt: u32) -> Option<Work> {
        let state = &self.state;
        let effect = self.open(EffectKind::LlmCall, id, Some(attempt))?;
        let call = effect.llm()?;
        if call.handler == LlmHandler::Worker {
            return None;
        }
        Some(Work::CallLlm {
            work: WorkId::new(EffectKind::LlmCall, id, Some(attempt)),
            call: Box::new(LlmCall {
                call_id: id.to_string(),
                attempt,
                llm: call.llm.clone(),
                request: call.spec.to_request(call.prompt.clone()),
                defer_tools_strategy: call.defer_tools_strategy,
                stream: call.stream,
                owner: state.owner.clone()?,
                agent_id: state.agent_id.clone()?,
                ancestry: state.ancestry.clone(),
                turn_id: state.turn_id().map(str::to_string),
                retry: effect.tracking.retry_policy.clone(),
            }),
        })
    }

    fn system1(&self, id: &str, attempt: u32) -> Option<Work> {
        let state = &self.state;
        let effect = self.open(EffectKind::System1Call, id, Some(attempt))?;
        let call = effect.system1()?;
        let mut request = call.request.clone();
        if request.state.is_none() {
            let transcript = state.at(effect.anchor.as_deref()).transcript();
            request.state = Some(transcript_state(&transcript));
        }
        Some(Work::CallSystem1 {
            work: WorkId::new(EffectKind::System1Call, id, Some(attempt)),
            call: Box::new(System1Call {
                call_id: id.to_string(),
                attempt,
                system1: call.system1.clone(),
                request,
                owner: state.owner.clone()?,
                agent_id: state.agent_id.clone()?,
                retry: effect.tracking.retry_policy.clone(),
            }),
        })
    }

    fn tool(&self, id: &str, attempt: u32) -> Option<Work> {
        let state = &self.state;
        let effect = self.open(EffectKind::ToolCall, id, Some(attempt))?;
        let tc = effect.tool()?;
        if tc.handler != ToolHandler::Server {
            return None;
        }
        let target = match tc.target.as_ref()? {
            ConnectorTarget::Remote { path, remote_name } => ToolTarget::Remote {
                path: path.clone(),
                name: remote_name.clone(),
                arguments: serde_json::from_str(&tc.arguments)
                    .unwrap_or_else(|_| serde_json::json!({})),
                requester: Requester::of_owner(state.owner.as_ref()),
            },
            ConnectorTarget::Code if state.script_waits(id) => return None,
            ConnectorTarget::Code => ToolTarget::Script,
            _ => ToolTarget::Local,
        };
        Some(Work::CallTool {
            work: WorkId::new(EffectKind::ToolCall, id, Some(attempt)),
            call: ToolCall {
                tool_call_id: id.to_string(),
                attempt,
                target,
                retry: effect.tracking.retry_policy.clone(),
            },
        })
    }

    /// A script runs again once every call it made has settled.
    fn script_resumed(&self, id: &str) -> Option<Work> {
        let state = &self.state;
        let effect = state.effect(EffectKind::ToolCall, id)?;
        let parent = &effect.tool()?.script.as_ref()?.parent;
        if effect.tracking.is_unsettled() {
            return None;
        }
        let attempt = state
            .effect(EffectKind::ToolCall, parent)?
            .tracking
            .retry
            .attempts;
        self.tool(parent, attempt)
    }

    /// A `run_code` call's script and the calls it made, when it is ready to
    /// run again.
    pub fn script(&self, id: &str, limits: &crate::codemode::Limits) -> Option<Script> {
        let state = &self.state;
        let effect = self.open(EffectKind::ToolCall, id, None)?;
        if effect.tool()?.target != Some(ConnectorTarget::Code) || state.script_waits(id) {
            return None;
        }
        let arguments: serde_json::Value =
            serde_json::from_str(&effect.tool()?.arguments).unwrap_or_default();
        let mut read = 0;
        let mut spent = false;
        let calls = state
            .script_calls(id)
            .into_iter()
            .filter_map(|e| {
                let tc = e.tool()?;
                let (name, arguments) = match tc.script.as_ref()?.called.as_ref() {
                    Some(called) => (called.name.clone(), called.arguments.clone()),
                    None => (tc.name.clone(), tc.arguments.clone()),
                };
                let output = tc.result.clone().unwrap_or_default();
                read += output.len();
                spent |= read > limits.results;
                Some(match spent {
                    true => ScriptRecord {
                        name,
                        arguments,
                        ok: false,
                        output: crate::codemode::spent(limits.results),
                    },
                    false => ScriptRecord {
                        name,
                        arguments,
                        ok: !tc.is_error,
                        output,
                    },
                })
            })
            .collect();
        let tools = effect.tool()?.script_tools.clone();
        Some(Script {
            version: crate::codemode::VERSION,
            tools,
            code: arguments
                .get("code")
                .and_then(|c| c.as_str())
                .unwrap_or_default()
                .to_string(),
            calls,
            spent,
            limits: limits.clone(),
            seed: xxhash_rust::xxh3::xxh3_64(id.as_bytes()),
            now_ms: effect
                .tracking
                .started_at
                .map(|t| t.timestamp_millis())
                .unwrap_or_default(),
        })
    }

    fn subagent(&self, id: &str) -> Option<Work> {
        let state = &self.state;
        let effect = self.open(EffectKind::Subagent, id, None)?;
        let sa = effect.subagent()?;
        let mut ancestry = state.ancestry.clone();
        ancestry.push(self.id.clone());
        Some(Work::StartSubagent {
            work: WorkId::new(
                EffectKind::Subagent,
                id,
                Some(effect.tracking.retry.attempts),
            ),
            subagent: Box::new(Subagent {
                child: sa.session_id.clone(),
                agent_id: sa.agent_id.clone(),
                owner: state.owner.clone()?,
                ancestry,
                message: sa.message.clone(),
                retry: effect.tracking.retry_policy.clone(),
                events: state.events.clone(),
            }),
        })
    }
}

pub fn settle(work: &WorkId, tenant_id: &str, outcome: Outcome) -> Command {
    Command::system(
        work.settle_id(),
        tenant_id,
        CommandPayload::settle(work.kind, work.id.clone(), work.attempt, outcome),
    )
}
