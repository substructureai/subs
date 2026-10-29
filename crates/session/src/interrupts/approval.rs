use serde_json::Value;

use super::InterruptKind;
use crate::events::ScriptAsk;
use crate::propose::{
    asked_about, call_tool, recorded, reissue, resumed, route_tool_call, script_calls, settled,
    siblings_answered, Proposing,
};
use crate::protocol::{
    ConnectorTool, ConnectorToolKind, Content, DecisionAction, DecisionResponse, DraftMessage,
    Message, Role, ToolCall,
};
use crate::state::inner_arguments;

pub const PREFIX: &str = "mcp-approve:";

pub struct Approval;

impl InterruptKind for Approval {
    fn prefix(&self) -> &'static str {
        PREFIX
    }

    fn resumed(&self, tail: &str, payload: &Value, p: &Proposing<'_>) -> Option<DecisionResponse> {
        let transcript = p.state.transcript();
        let transcript = transcript.as_slice();
        let llm_calls = &p.state.open_llm_calls();
        let dispatched = p.state.state().dispatched_calls();
        let dispatched = dispatched.as_slice();
        let config = p.state.resolve_agent_for();
        let config = config.as_ref();
        let connector_tools = p.state.connector_tools().tools;
        let connector_tools = connector_tools.as_slice();
        let pending_calls = p.pending_calls;
        if let Some((parent, _)) = p.state.state().script_ask(tail) {
            let asked = p.state.state().script_asked(parent);
            return Some(script_resumed(
                tail,
                approved(payload),
                asked,
                transcript,
                connector_tools,
            ));
        }
        let Some((at, call)) = asked_about(tail, transcript, dispatched) else {
            return Some(match config {
                Some(c) => resumed(transcript, c),
                None => DecisionResponse {
                    messages: recorded(transcript),
                    ..Default::default()
                },
            });
        };

        let approved = approved(payload);
        let refusals = match approved {
            true => Vec::new(),
            false => vec![refusal(call)],
        };
        let mut actions = match approved {
            true => route_tool_call(call, connector_tools),
            false => Vec::new(),
        };

        let waiting: Vec<&ToolCall> = transcript[at]
            .tool_calls
            .iter()
            .filter(|c| c.id != call.id && !settled(&c.id, at, transcript, dispatched))
            .collect();
        let mut gated = waiting.iter().filter(|c| needed(c, connector_tools));
        match gated.next() {
            Some(next) => actions.push(ask(next, connector_tools, gated.count())),
            None => actions.extend(
                waiting
                    .iter()
                    .flat_map(|c| route_tool_call(c, connector_tools)),
            ),
        }

        if actions.is_empty() && pending_calls == 0 && siblings_answered(&call.id, transcript) {
            actions.extend(reissue(&call.id, &refusals, transcript, llm_calls));
        }
        let mut messages = recorded(transcript);
        messages.extend(refusals);
        Some(DecisionResponse {
            messages,
            actions,
            ..Default::default()
        })
    }
}

pub fn needed(call: &ToolCall, connector_tools: &[ConnectorTool]) -> bool {
    target_of(call, connector_tools).is_some_and(|tool| tool.approve)
}

pub fn ask(held: &ToolCall, connector_tools: &[ConnectorTool], remaining: usize) -> DecisionAction {
    let name = ran_name(held, connector_tools);
    let arguments = ran_arguments(held, connector_tools);
    let shown = match &arguments {
        Value::Object(o) if o.is_empty() => String::new(),
        Value::Null => String::new(),
        value => format!(
            "\n\n```\n{}\n```",
            serde_json::to_string_pretty(value).unwrap_or_else(|_| value.to_string())
        ),
    };
    let after = match remaining {
        0 => String::new(),
        1 => "\n\nOne more call waits behind it.".to_string(),
        n => format!("\n\n{n} more calls wait behind it."),
    };
    DecisionAction::Interrupt {
        interrupt_id: Some(format!("{PREFIX}{}", held.id)),
        reason: format!("`{name}` needs approval before it runs"),
        payload: serde_json::json!({
            "message": format!("Run `{name}`?{shown}{after}"),
            "toolCallId": held.id,
            "metadata": {
                "type": "tool.approval",
                "tool": name,
                "arguments": arguments,
                "remaining": remaining,
                "options": [
                    { "label": "Run it", "value": { "approved": true }, "style": "primary" },
                    { "label": "Decline", "value": { "approved": false }, "style": "danger" },
                ],
            },
        }),
    }
}

/// The answer about one of a script's calls: run it or refuse it, then ask
/// about the next that needs approval, or run the rest.
fn script_resumed(
    id: &str,
    approved: bool,
    asked: &[ScriptAsk],
    transcript: &[Message],
    connector_tools: &[ConnectorTool],
) -> DecisionResponse {
    let calls: Vec<ToolCall> = asked.iter().map(ScriptAsk::tool_call).collect();
    let (this, waiting): (Vec<ToolCall>, Vec<ToolCall>) =
        calls.into_iter().partition(|c| c.id == id);
    let mut actions: Vec<DecisionAction> = match approved {
        true => this.iter().map(call_tool).collect(),
        false => this
            .iter()
            .map(|c| DecisionAction::ToolError {
                id: Some(c.id.clone()),
                attempt: None,
                error: format!("A person declined `{}`.", c.function.name),
                retryable: false,
                code: None,
                detail: None,
            })
            .collect(),
    };
    actions.extend(script_calls(&waiting, connector_tools));
    DecisionResponse {
        messages: recorded(transcript),
        actions,
        ..Default::default()
    }
}

fn refusal(call: &ToolCall) -> DraftMessage {
    DraftMessage {
        id: None,
        role: Role::Tool,
        content: Some(Content::Text(
            "A person declined this call. Do not try it again; \
             say what you were going to do and why it stopped."
                .to_string(),
        )),
        tool_calls: None,
        tool_call_id: Some(call.id.clone()),
        name: Some(call.function.name.clone()),
        reasoning: None,
    }
}

fn approved(payload: &Value) -> bool {
    if payload.get("status").and_then(|s| s.as_str()) == Some("cancelled") {
        return false;
    }
    payload
        .get("payload")
        .unwrap_or(payload)
        .get("approved")
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

fn ran_name<'a>(call: &'a ToolCall, connector_tools: &'a [ConnectorTool]) -> &'a str {
    target_of(call, connector_tools)
        .map(|t| t.name.as_str())
        .unwrap_or(&call.function.name)
}

fn ran_arguments(call: &ToolCall, connector_tools: &[ConnectorTool]) -> Value {
    let written = serde_json::from_str::<Value>(&call.function.arguments)
        .unwrap_or_else(|_| Value::String(call.function.arguments.clone()));
    if target_of(call, connector_tools).is_none_or(|t| t.name == call.function.name) {
        return written;
    }
    let inner = inner_arguments(&written);
    serde_json::from_str(&inner).unwrap_or(Value::String(inner))
}

fn target_of<'a>(
    call: &ToolCall,
    connector_tools: &'a [ConnectorTool],
) -> Option<&'a ConnectorTool> {
    let named = connector_tools
        .iter()
        .find(|t| t.name == call.function.name)?;
    if named.kind != ConnectorToolKind::Call {
        return Some(named);
    }
    let inner = serde_json::from_str::<Value>(&call.function.arguments)
        .ok()?
        .get("name")?
        .as_str()?
        .to_string();
    connector_tools
        .iter()
        .find(|t| t.name == inner && t.kind.is_remote())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::connectors::filter;
    use crate::propose::script_calls;

    fn tool(name: &str, approve: bool) -> ConnectorTool {
        ConnectorTool {
            approve,
            ..filter::engine_tool(
                name,
                String::new(),
                serde_json::json!({}),
                ConnectorToolKind::Remote,
            )
        }
    }

    fn ask(id: &str, name: &str, seq: u32) -> ScriptAsk {
        ScriptAsk {
            id: id.to_string(),
            seq,
            name: name.to_string(),
            arguments: "{}".to_string(),
        }
    }

    fn tools() -> Vec<ConnectorTool> {
        vec![tool("sentry__search", false), tool("sentry__delete", true)]
    }

    fn named(actions: &[DecisionAction]) -> Vec<String> {
        actions
            .iter()
            .map(|a| match a {
                DecisionAction::CallTool { id: Some(id), .. } => format!("run {id}"),
                DecisionAction::ToolError { id: Some(id), .. } => format!("refuse {id}"),
                DecisionAction::Interrupt {
                    interrupt_id: Some(id),
                    ..
                } => format!("ask {id}"),
                other => format!("{other:?}"),
            })
            .collect()
    }

    #[test]
    fn a_scripts_calls_run_unless_one_needs_approval() {
        let asked = [
            ask("c1", "sentry__search", 1),
            ask("c2", "sentry__search", 2),
        ];
        let calls: Vec<ToolCall> = asked.iter().map(ScriptAsk::tool_call).collect();
        assert_eq!(named(&script_calls(&calls, &tools())), ["run c1", "run c2"]);

        let asked = [
            ask("c1", "sentry__search", 1),
            ask("c2", "sentry__delete", 2),
        ];
        let calls: Vec<ToolCall> = asked.iter().map(ScriptAsk::tool_call).collect();
        assert_eq!(
            named(&script_calls(&calls, &tools())),
            ["ask mcp-approve:c2"],
            "a person is asked first, and the rest wait"
        );
    }

    #[test]
    fn an_answer_about_a_scripts_call_runs_or_refuses_it_then_the_rest() {
        let asked = [
            ask("c1", "sentry__search", 1),
            ask("c2", "sentry__delete", 2),
            ask("c3", "sentry__delete", 3),
        ];
        let approved = script_resumed("c2", true, &asked, &[], &tools());
        assert_eq!(named(&approved.actions), ["run c2", "ask mcp-approve:c3"]);

        let declined = script_resumed("c3", false, &asked[2..], &[], &tools());
        assert_eq!(named(&declined.actions), ["refuse c3"]);

        let last = script_resumed("c2", true, &asked[..2], &[], &tools());
        assert_eq!(named(&last.actions), ["run c2", "run c1"]);
    }
}
