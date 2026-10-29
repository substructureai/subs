use super::{decision_queued, fail, mismatched, void_events, KindSpec, Outcome, SettleError};
use crate::codemode;
use crate::command::SessionError;
use crate::connectors::ConnectionPath;
use crate::cx::Cx;
use crate::decision::{ToolHandler, Trigger};
use crate::events::*;
use crate::ids::Ids;
use crate::protocol::{
    ConnectorToolKind, ErrorCode, ErrorInfo, SkillSource, StoredResult, CALL_TOOL, SKILL,
    TOOL_SEARCH,
};
use crate::protocol::{RetryOverride, RetryPolicy};
use crate::state::{inner_arguments, CallTarget, EffectKind, SessionState};
use crate::tool_contract::{declared_tool, output_violation, DeclaredTool};
use crate::Caller;

pub struct ToolSpec;

impl KindSpec for ToolSpec {
    fn kind(&self) -> EffectKind {
        EffectKind::ToolCall
    }

    fn authorize(
        &self,
        state: &SessionState,
        id: &str,
        caller: &Caller,
    ) -> Result<(), SessionError> {
        let tc = state.tool_call(id).ok_or(SessionError::EffectNotFound)?;
        state.check_tool_call_caller(tc, caller)
    }

    fn settle(&self, state: &Cx<'_>, id: &str, outcome: Outcome) -> Vec<EventPayload> {
        match outcome {
            Outcome::Tool { result } => {
                let name = state
                    .tool_call(id)
                    .map(|t| t.name.clone())
                    .unwrap_or_default();
                match state
                    .declared_output_schema(id, &name)
                    .and_then(|schema| output_violation(&schema, &result.rendered()))
                {
                    Some(v) => fail(
                        self,
                        state,
                        id,
                        &SettleError::new(
                            ErrorInfo::new(
                                ErrorCode::InvalidResponse,
                                format!("tool output violated its declared schema: {v}"),
                            )
                            .with_param("output"),
                            false,
                        ),
                    ),
                    None => finished(state, id, name, result),
                }
            }
            Outcome::Script {
                known,
                calls,
                cpu_ms,
            } => script_step(state, id, known, calls, cpu_ms),
            Outcome::Error(e) => fail(self, state, id, &e),
            other => mismatched(self.kind(), &other),
        }
    }

    fn errored(&self, state: &SessionState, id: &str, e: &SettleError) -> Option<EventPayload> {
        Some(EventPayload::ToolCallErrored(ToolCallErrored {
            id: id.to_string(),
            name: name_of(state, id),
            error: e.error.clone(),
            retryable: e.retryable,
        }))
    }

    fn terminal(&self, state: &Cx<'_>, id: &str, e: &SettleError) -> Vec<EventPayload> {
        let mut events = Vec::new();
        if !in_script(state, id) {
            events.extend(void_script(state, id));
            events.push(decision_queued(
                state.ids(),
                Trigger::ToolFinished {
                    id: id.to_string(),
                    ok: false,
                    name: name_of(state, id),
                    result: None,
                    error: Some(e.error.clone()),
                },
            ));
        }
        if let (Some(auth), Some(target)) = (e.auth, connector_target(state, id)) {
            events.push(EventPayload::ConnectorAuthFailed(ConnectorAuthFailed {
                path: target,
                auth,
            }));
        }
        events
    }

    fn dispatch(&self, state: &SessionState, id: &str) -> Vec<EventPayload> {
        let Some(effect) = state.effect(EffectKind::ToolCall, id) else {
            return void_events(EffectKind::ToolCall, id.to_string());
        };
        vec![EventPayload::ToolCallDispatched(ToolCallDispatched {
            id: id.to_string(),
            attempt: effect.tracking.retry.attempts,
        })]
    }

    fn execute_trigger(&self, state: &SessionState, id: &str) -> Option<Trigger> {
        let effect = state.effect(EffectKind::ToolCall, id)?;
        let tc = effect.tool().filter(|t| t.handler == ToolHandler::Worker)?;
        Some(Trigger::ToolExecute {
            id: id.to_string(),
            name: tc.name.clone(),
            arguments: tc.arguments.clone(),
            attempt: effect.tracking.retry.attempts,
            deadline: effect.tracking.deadline,
        })
    }

    fn retry(&self, state: &SessionState, id: &str) -> Vec<EventPayload> {
        let Some(effect) = state.effect(EffectKind::ToolCall, id) else {
            return Vec::new();
        };
        let (t, Some(tc)) = (&effect.tracking, effect.tool()) else {
            return Vec::new();
        };
        vec![EventPayload::ToolCallRequested(ToolCallRequested {
            id: id.to_string(),
            attempt: t.retry.attempts,
            name: tc.name.clone(),
            arguments: tc.arguments.clone(),
            handler: tc.handler,
            target: tc.target.clone(),
            retry: t.retry_policy.clone(),
            script: tc.script.clone(),
        })]
    }
}

fn in_script(state: &SessionState, id: &str) -> bool {
    state.tool_call(id).is_some_and(|t| t.script.is_some())
}

/// A call its script made settles without the model hearing of it.
fn finished(state: &Cx<'_>, id: &str, name: String, result: StoredResult) -> Vec<EventPayload> {
    if in_script(state, id) {
        return vec![EventPayload::ToolCallCompleted(ToolCallCompleted {
            id: id.to_string(),
            name,
            result,
        })];
    }
    let mut events = void_script(state, id);
    events.extend(complete(state.ids(), id.to_string(), name, result));
    events
}

/// A script's calls still open when its `run_code` call ends.
fn void_script(state: &SessionState, parent: &str) -> Vec<EventPayload> {
    state
        .script_calls(parent)
        .into_iter()
        .filter(|e| e.tracking.is_unsettled())
        .flat_map(|e| void_events(EffectKind::ToolCall, e.id.clone()))
        .collect()
}

fn script_step(
    state: &Cx<'_>,
    parent: &str,
    known: u32,
    calls: Vec<ScriptCall>,
    cpu_ms: u64,
) -> Vec<EventPayload> {
    let made = state.script_calls(parent);
    let stale = made.len() != known as usize || made.iter().any(|e| e.tracking.is_unsettled());
    let is_script = state
        .tool_call(parent)
        .is_some_and(|t| t.target == Some(ConnectorTarget::Code));
    if stale || !is_script {
        return Vec::new();
    }
    let stepped = EventPayload::ToolCallStepped(ToolCallStepped {
        id: parent.to_string(),
        cpu_ms,
    });
    std::iter::once(stepped)
        .chain(calls.into_iter().zip(known + 1..).map(|(call, seq)| {
            let script = ScriptCaller {
                parent: parent.to_string(),
                seq,
                called: None,
            };
            scripted(state, state.ids().next(), script, call.name, call.arguments)
        }))
        .collect()
}

fn scripted(
    state: &SessionState,
    id: String,
    script: ScriptCaller,
    name: String,
    arguments: String,
) -> EventPayload {
    let here = state.at_head();
    let config = here.resolve_agent_for();
    let tools = here.connector_tools().tools;
    let refused = codemode::refusal(&name, &arguments, config.as_ref(), &tools);
    let called = ScriptCall {
        name: name.clone(),
        arguments: arguments.clone(),
    };
    let mut call = requested(state, id, name, arguments, None, Some(script));
    if let Some(script) = call.script.as_mut() {
        script.called =
            (call.name != called.name || call.arguments != called.arguments).then_some(called);
    }
    let reason = refused.or_else(|| match call.handler {
        ToolHandler::Client => Some(format!(
            "`{}` runs on the client. Call it directly, not from code.",
            call.name
        )),
        _ => None,
    });
    if let Some(reason) = reason {
        call.handler = ToolHandler::Server;
        call.target = Some(ConnectorTarget::Refused { reason });
        call.retry = RetryPolicy::no_retry();
    }
    EventPayload::ToolCallRequested(call)
}

fn name_of(state: &SessionState, id: &str) -> String {
    state
        .tool_call(id)
        .map(|t| t.name.clone())
        .unwrap_or_default()
}

fn connector_target(state: &SessionState, id: &str) -> Option<ConnectionPath> {
    state
        .tool_call(id)
        .and_then(|t| t.target.as_ref())
        .and_then(|t| match t {
            ConnectorTarget::Remote { path, .. } => Some(path.clone()),
            _ => None,
        })
}

pub fn complete(ids: &Ids, id: String, name: String, result: StoredResult) -> Vec<EventPayload> {
    vec![
        EventPayload::ToolCallCompleted(ToolCallCompleted {
            id: id.clone(),
            name: name.clone(),
            result: result.clone(),
        }),
        decision_queued(
            ids,
            Trigger::ToolFinished {
                id,
                ok: true,
                name,
                result: Some(result),
                error: None,
            },
        ),
    ]
}

pub fn request(
    state: &SessionState,
    tool_call_id: String,
    name: String,
    arguments: String,
    retry: Option<RetryOverride>,
    caller: &Caller,
) -> Result<Vec<EventPayload>, SessionError> {
    SessionState::ensure_internal(caller)?;
    if state.has_effect(EffectKind::ToolCall, &tool_call_id) {
        return Ok(Vec::new());
    }
    let call = requested(state, tool_call_id, name, arguments, retry.as_ref(), None);
    Ok(vec![EventPayload::ToolCallRequested(call)])
}

fn requested(
    state: &SessionState,
    id: String,
    name: String,
    arguments: String,
    retry: Option<&RetryOverride>,
    script: Option<ScriptCaller>,
) -> ToolCallRequested {
    let (name, arguments) = engine_call(state, name, arguments);
    let engine_tool = state
        .connector_tool_for(&name)
        .filter(|t| t.kind != ConnectorToolKind::Subagent);
    let locked = state.at_head().locked_tools().contains_key(&name);
    let (name, arguments, handler, target) = match engine_tool {
        _ if locked => (
            name,
            arguments,
            ToolHandler::Server,
            Some(ConnectorTarget::Locked),
        ),
        Some(tool) if tool.kind == ConnectorToolKind::Call => unwrap_call(state, name, arguments),
        Some(tool) if tool.kind == ConnectorToolKind::Code => (
            name,
            arguments,
            ToolHandler::Server,
            Some(ConnectorTarget::Code),
        ),
        Some(tool) if tool.kind == ConnectorToolKind::Attachment => (
            name,
            arguments,
            ToolHandler::Server,
            Some(ConnectorTarget::Attachment),
        ),
        Some(tool) if tool.kind == ConnectorToolKind::Skill => {
            let named = argument(&arguments, "name").unwrap_or_default();
            let config = state.at_head().resolve_agent_for();
            let found = config.as_ref().and_then(|c| c.named_skill(&named));
            let (plugin, skill) = match &found {
                Some(s) => (s.bundle, s.skill),
                None => crate::engine_tools::split_skill(&named),
            };
            let target = ConnectorTarget::Skill {
                plugin: plugin.to_string(),
                skill: skill.to_string(),
            };
            match found.map(|s| s.source) {
                Some(SkillSource::Worker) => (name, arguments, ToolHandler::Worker, None),
                _ => (name, arguments, ToolHandler::Server, Some(target)),
            }
        }
        Some(tool) => {
            let target = match tool.connector {
                Some(path) => ConnectorTarget::Remote {
                    path,
                    remote_name: tool.remote_name,
                },
                None => ConnectorTarget::Find,
            };
            (name, arguments, ToolHandler::Server, Some(target))
        }
        None => (name.clone(), arguments, state.tool_handler_for(&name), None),
    };
    let retry = match target {
        Some(ConnectorTarget::Code) => codemode::retry(),
        _ => RetryPolicy::resolve(retry, state.retry_config().as_ref(), handler.retry_target()),
    };
    ToolCallRequested {
        id,
        attempt: 0,
        name,
        arguments,
        handler,
        target,
        retry,
        script,
    }
}

/// `call_tool` that names `tool_search` or `skill` is that tool's call, as it
/// is for any other tool.
fn engine_call(state: &SessionState, name: String, arguments: String) -> (String, String) {
    if name != CALL_TOOL {
        return (name, arguments);
    }
    let raw: serde_json::Value = serde_json::from_str(&arguments).unwrap_or_default();
    match raw.get("name").and_then(|v| v.as_str()) {
        Some(named)
            if [TOOL_SEARCH, SKILL].contains(&named)
                && state.connector_tool_for(named).is_some() =>
        {
            (named.to_string(), inner_arguments(&raw))
        }
        _ => (name, arguments),
    }
}

fn unwrap_call(
    state: &SessionState,
    name: String,
    arguments: String,
) -> (String, String, ToolHandler, Option<ConnectorTarget>) {
    let named = argument(&arguments, "name").unwrap_or_default();
    let here = state.at_head();
    let target = here
        .call_tool_fault(&arguments)
        .is_none()
        .then(|| here.call_tool_target(&named))
        .flatten();
    let Some(target) = target else {
        return (
            name,
            arguments,
            ToolHandler::Server,
            Some(ConnectorTarget::Call),
        );
    };
    let inner = inner_arguments(&serde_json::from_str(&arguments).unwrap_or_default());
    match target {
        CallTarget::Connector(tool) => match tool.connector {
            Some(path) => (
                tool.name.clone(),
                inner,
                ToolHandler::Server,
                Some(ConnectorTarget::Remote {
                    path,
                    remote_name: tool.remote_name,
                }),
            ),
            None => (
                name,
                arguments,
                ToolHandler::Server,
                Some(ConnectorTarget::Call),
            ),
        },
        CallTarget::Declared(tool) => (tool.name, inner, ToolHandler::declared(tool.handler), None),
        CallTarget::Subagent(_) => (
            name,
            arguments,
            ToolHandler::Server,
            Some(ConnectorTarget::Call),
        ),
    }
}

fn argument(arguments: &str, key: &str) -> Option<String> {
    serde_json::from_str::<serde_json::Value>(arguments)
        .ok()?
        .get(key)?
        .as_str()
        .map(str::to_string)
}

impl SessionState {
    fn declared_output_schema(&self, tool_call_id: &str, name: &str) -> Option<serde_json::Value> {
        let tree = self.message_tree();
        let path = tree.path_to(tree.head_id.as_deref()?);
        match declared_tool(tool_call_id, name, &path, |id| self.llm_call(id)) {
            DeclaredTool::Declared(tool) => tool.output.clone(),
            _ => None,
        }
    }
}
