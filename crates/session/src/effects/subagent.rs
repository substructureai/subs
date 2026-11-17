use crate::cx::Cx;
use crate::ids::Ids;
use crate::prompts::errors;
use rust_decimal::Decimal;
use uuid::Uuid;

use super::{decision_queued, fail, mismatched, void_events, KindSpec, Outcome, SettleError};
use crate::command::SessionError;
use crate::decision::{BranchConfig, Trigger};
use crate::events::*;
use crate::protocol::{
    Content, DraftMessage, ErrorCode, ErrorInfo, RetryPolicy, Role, SpawnMode, SubagentBranch,
    SubagentMode, Usage,
};
use crate::state::{json_to_string, EffectKind, SessionState};
use crate::Caller;

pub use crate::prompts::answers::DETACHED_STARTED;

pub struct SubagentSpec;

impl KindSpec for SubagentSpec {
    fn kind(&self) -> EffectKind {
        EffectKind::Subagent
    }

    fn voids_when_running(&self) -> bool {
        true
    }

    fn settle(&self, state: &Cx<'_>, id: &str, outcome: Outcome) -> Vec<EventPayload> {
        match outcome {
            Outcome::SubagentStarted => {
                let mut events = vec![EventPayload::SubagentStarted(SubagentStarted {
                    id: id.to_string(),
                })];
                if let Some(sa) = state
                    .subagent(id)
                    .filter(|sa| sa.mode == SpawnMode::Detached)
                {
                    events.push(decision_queued(
                        state.ids(),
                        Trigger::SubagentFinished {
                            id: id.to_string(),
                            ok: true,
                            session_id: sa.session_id.clone(),
                            agent_id: sa.agent_id.clone(),
                            result: Some(DETACHED_STARTED.to_string()),
                            error: None,
                        },
                    ));
                }
                events
            }
            Outcome::Error(e) => fail(self, state, id, &e),
            other => mismatched(self.kind(), &other),
        }
    }

    fn errored(&self, _state: &SessionState, id: &str, e: &SettleError) -> Option<EventPayload> {
        Some(EventPayload::SubagentErrored(SubagentErrored {
            id: id.to_string(),
            error: e.error.clone(),
            retryable: e.retryable,
        }))
    }

    fn terminal(&self, state: &Cx<'_>, id: &str, e: &SettleError) -> Vec<EventPayload> {
        let Some(sa) = state.subagent(id) else {
            return Vec::new();
        };
        vec![decision_queued(
            state.ids(),
            Trigger::SubagentFinished {
                id: id.to_string(),
                ok: false,
                session_id: sa.session_id.clone(),
                agent_id: sa.agent_id.clone(),
                result: None,
                error: Some(e.error.clone()),
            },
        )]
    }

    fn dispatch(&self, state: &Cx<'_>, id: &str) -> Vec<EventPayload> {
        if state.has_effect(EffectKind::Subagent, id) {
            vec![EventPayload::SubagentDispatched(SubagentDispatched {
                id: id.to_string(),
            })]
        } else {
            void_events(EffectKind::Subagent, id.to_string())
        }
    }

    fn retry(&self, state: &SessionState, id: &str) -> Vec<EventPayload> {
        let Some(effect) = state.effect(EffectKind::Subagent, id) else {
            return Vec::new();
        };
        let (t, Some(sa)) = (&effect.tracking, effect.subagent()) else {
            return Vec::new();
        };
        vec![EventPayload::SubagentRequested(SubagentRequested {
            id: id.to_string(),
            agent_id: sa.agent_id.clone(),
            session_id: sa.session_id.clone(),
            message: sa.message.clone(),
            retry: t.retry_policy.clone(),
            mode: sa.mode,
            branch: effect.branch.clone(),
            on_branch: sa.on_branch.clone(),
        })]
    }
}

fn withdraw_notice(state: &Cx<'_>, session_id: &str) -> Vec<EventPayload> {
    let Some(prior) = state.queued_subagent_notice() else {
        return Vec::new();
    };
    if !prior.sessions.iter().any(|s| s == session_id) {
        return Vec::new();
    }
    let mut events = vec![state.decision_dropped(prior.decision_id)];
    let (messages, sessions): (Vec<DraftMessage>, Vec<String>) = prior
        .messages
        .iter()
        .zip(prior.sessions)
        .filter(|(_, s)| *s != session_id)
        .map(|(m, s)| (m.clone(), s.clone()))
        .unzip();
    if !messages.is_empty() {
        events.push(decision_queued(
            state.ids(),
            Trigger::SubagentNotice {
                messages,
                sessions,
                turn_id: prior.turn_id.to_string(),
            },
        ));
    }
    events
}

const SPAWN_NAMESPACE: Uuid = Uuid::from_u128(0x7b5e_1f2a_3c4d_5e6f_8091_a2b3_c4d5_e6f7);

fn child_session_id(decision_id: &str, tool_call_id: &str) -> String {
    Uuid::new_v5(
        &SPAWN_NAMESPACE,
        format!("{decision_id}:{tool_call_id}").as_bytes(),
    )
    .to_string()
}

pub struct Spawn {
    pub tool_call_id: String,
    pub agent_id: String,
    pub session_id: Option<String>,
    pub message: Option<DraftMessage>,
    pub retry: RetryPolicy,
    pub decision_id: String,
    pub mode: Option<SpawnMode>,
    pub config: Option<Box<crate::protocol::AgentConfig>>,
}

pub fn request(
    state: &Cx<'_>,
    spawn: Spawn,
    caller: &Caller,
) -> Result<Vec<EventPayload>, SessionError> {
    SessionState::ensure_internal(caller)?;
    if state.has_effect(EffectKind::Subagent, &spawn.tool_call_id) {
        return Ok(Vec::new());
    }
    let session_id = spawn
        .session_id
        .clone()
        .unwrap_or_else(|| child_session_id(&spawn.decision_id, &spawn.tool_call_id));
    let answer = |error: ErrorInfo| {
        Ok(vec![decision_queued(
            state.ids(),
            Trigger::SubagentFinished {
                id: spawn.tool_call_id.clone(),
                ok: false,
                session_id: session_id.clone(),
                agent_id: spawn.agent_id.clone(),
                result: None,
                error: Some(error),
            },
        )])
    };
    let config = state
        .at_scope(state.scope())
        .resolve_agent_for()
        .unwrap_or_default();
    if !config.may_spawn_subagent(state.depth() + state.branch_depth(state.scope()) as u32) {
        let limit = config.depth_limit();
        return answer(ErrorInfo::new(
            ErrorCode::BudgetExceeded,
            errors::subagent_depth(limit),
        ));
    }
    if let Some(named) = spawn.session_id.as_deref() {
        match state.subagent_session(named) {
            None => {
                return answer(ErrorInfo::new(
                    ErrorCode::Unroutable,
                    errors::no_such_subagent_session(named),
                ))
            }
            Some(sa) if !spawn.agent_id.is_empty() && sa.agent_id != spawn.agent_id => {
                return answer(ErrorInfo::new(
                    ErrorCode::Unroutable,
                    errors::subagent_session_of(named, &sa.agent_id, &spawn.agent_id),
                ))
            }
            Some(_) => {}
        }
    }
    if state.subagent_awaiting(&session_id).is_some() {
        return answer(ErrorInfo::new(
            ErrorCode::Unroutable,
            errors::subagent_busy(&spawn.agent_id),
        ));
    }
    let requested = spawn.mode.unwrap_or_default();
    let mode = match config.subagent_mode(&spawn.agent_id) {
        None | Some(SubagentMode::Any) => requested,
        Some(_) if requested == SpawnMode::Wait => SpawnMode::Wait,
        Some(SubagentMode::Blocking) => SpawnMode::Blocking,
        Some(SubagentMode::Detached) => SpawnMode::Detached,
    };
    if mode == SpawnMode::Detached && state.scope().is_some() {
        return answer(ErrorInfo::new(
            ErrorCode::Unroutable,
            errors::DETACHED_ON_BRANCH.to_string(),
        ));
    }
    if mode == SpawnMode::Wait {
        let Some((_, sa)) = spawn
            .session_id
            .as_deref()
            .and_then(|named| state.subagent_detached(named))
        else {
            return answer(ErrorInfo::new(
                ErrorCode::Unroutable,
                errors::WAIT_NEEDS_SESSION.to_string(),
            ));
        };
        let agent_id = sa.agent_id.clone();
        let Some(text) = sa.result.clone() else {
            return Ok(vec![EventPayload::SubagentRequested(SubagentRequested {
                id: spawn.tool_call_id,
                agent_id,
                session_id,
                message: None,
                retry: spawn.retry,
                mode: SpawnMode::Wait,
                branch: state.scope_branch(),
                on_branch: None,
            })]);
        };
        let mut events = withdraw_notice(state, &session_id);
        events.push(decision_queued(
            state.ids(),
            match sa.is_error {
                false => Trigger::SubagentFinished {
                    id: spawn.tool_call_id,
                    ok: true,
                    session_id,
                    agent_id,
                    result: Some(text),
                    error: None,
                },
                true => Trigger::SubagentFinished {
                    id: spawn.tool_call_id,
                    ok: false,
                    session_id,
                    agent_id,
                    result: None,
                    error: Some(ErrorInfo::internal(text)),
                },
            },
        ));
        return Ok(events);
    }
    if state
        .subagent_detached(&session_id)
        .is_some_and(|(_, sa)| sa.result.is_none())
    {
        return answer(ErrorInfo::new(
            ErrorCode::Unroutable,
            errors::subagent_detached(&spawn.agent_id),
        ));
    }
    let on_branch = match spawn
        .session_id
        .as_deref()
        .and_then(|s| state.subagent_session(s))
    {
        Some(sa) => sa.on_branch.clone(),
        None => match config.subagent_branch(&spawn.agent_id) {
            None => None,
            Some(SubagentBranch::Head) => Some(SubagentOnBranch {
                fork: true,
                agent: BranchConfig::Inherit,
            }),
            Some(SubagentBranch::Root) => match spawn.config {
                Some(own) => Some(SubagentOnBranch {
                    fork: false,
                    agent: BranchConfig::Config(own),
                }),
                None => {
                    return answer(ErrorInfo::new(
                        ErrorCode::Unroutable,
                        errors::no_subagent_config(&spawn.agent_id),
                    ))
                }
            },
        },
    };
    Ok(vec![EventPayload::SubagentRequested(SubagentRequested {
        id: spawn.tool_call_id,
        agent_id: spawn.agent_id,
        session_id,
        message: spawn.message,
        retry: spawn.retry,
        mode,
        branch: state.scope_branch(),
        on_branch,
    })])
}

pub fn complete_turn(
    ids: &Ids,
    tool_call_id: String,
    session_id: String,
    agent_id: String,
    data: serde_json::Value,
    cost: Decimal,
    token_usage: Usage,
) -> Vec<EventPayload> {
    let result = json_to_string(&data);
    vec![
        EventPayload::SubagentTurnCompleted(SubagentTurnCompleted {
            id: tool_call_id.clone(),
            cost,
            token_usage,
            data,
            turn_id: None,
            error: None,
        }),
        decision_queued(
            ids,
            Trigger::SubagentFinished {
                id: tool_call_id,
                ok: true,
                session_id,
                agent_id,
                result: Some(result),
                error: None,
            },
        ),
    ]
}

impl SessionState {
    pub fn detached_answer(
        &self,
        ids: &Ids,
        session_id: &str,
        agent_id: &str,
        completed: SubagentTurnCompleted,
    ) -> Vec<EventPayload> {
        let notice = notice(session_id, agent_id, &completed);
        let mut events = vec![EventPayload::SubagentTurnCompleted(completed)];
        let (mut messages, mut sessions, notice_turn) = match self.queued_subagent_notice() {
            Some(prior) => {
                let queued = prior.decision_id.to_string();
                let held = (
                    prior.messages.to_vec(),
                    prior.sessions.to_vec(),
                    prior.turn_id.to_string(),
                );
                events.push(self.decision_dropped(&queued));
                held
            }
            None => (Vec::new(), Vec::new(), ids.next()),
        };
        messages.push(notice);
        sessions.push(session_id.to_string());
        events.push(decision_queued(
            ids,
            Trigger::SubagentNotice {
                messages,
                sessions,
                turn_id: notice_turn,
            },
        ));
        events
    }

    pub fn subagent_branch_answer(
        &self,
        ids: &Ids,
        call: &str,
        data: serde_json::Value,
        error: Option<ErrorInfo>,
    ) -> Vec<EventPayload> {
        let Some(sa) = self.subagent(call) else {
            return Vec::new();
        };
        let completed = SubagentTurnCompleted {
            id: call.to_string(),
            cost: Decimal::ZERO,
            token_usage: Usage::default(),
            data,
            turn_id: None,
            error,
        };
        if sa.mode == SpawnMode::Detached {
            return self.detached_answer(ids, &sa.session_id, &sa.agent_id, completed);
        }
        let finished = Trigger::SubagentFinished {
            id: call.to_string(),
            ok: completed.error.is_none(),
            session_id: sa.session_id.clone(),
            agent_id: sa.agent_id.clone(),
            result: completed
                .error
                .is_none()
                .then(|| json_to_string(&completed.data)),
            error: completed.error.clone(),
        };
        vec![
            EventPayload::SubagentTurnCompleted(completed),
            decision_queued(ids, finished),
        ]
    }
}

pub fn notice(session_id: &str, agent_id: &str, completed: &SubagentTurnCompleted) -> DraftMessage {
    let body = match &completed.error {
        Some(e) => e.message.clone(),
        None => json_to_string(&completed.data),
    };
    DraftMessage {
        id: None,
        role: Role::User,
        content: Some(Content::Text(crate::prompts::context::subagent_result(
            agent_id,
            session_id,
            completed.error.is_some(),
            &body,
        ))),
        tool_calls: None,
        tool_call_id: None,
        name: None,
        reasoning: None,
    }
}
