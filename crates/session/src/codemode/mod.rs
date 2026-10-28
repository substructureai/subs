mod typescript;

pub use typescript::{is_identifier, render_json_schema_to_typescript};

use crate::connectors::filter;
use crate::protocol::{
    AgentConfig, ConnectorTool, ConnectorToolKind, Handler, LlmTool, RetryPolicy, ToolCall,
    ToolCallFunction, RUN_CODE, TOOL_SEARCH,
};

const INTRO: &str = "Run JavaScript that calls your tools. Use it to chain calls, to run them at \
once, or to filter a large result before you read it.
- `code` is the body of an async function. `return` a value to get it back.
- Call a tool with `await tools.<name>(args)`. Use `tools[\"<name>\"](args)` when the name is not \
an identifier. A call returns the tool's result, parsed when it is JSON.
- A call that fails throws an `Error` with the tool's message.
- Run calls at once with `Promise.all`.
- `console.log` output comes back with the return value. Results you do not return or log stay \
out of the conversation.
- There is no network, file system, timer or module. `Date` and `Math.random` stay the same \
for the whole run.
- Tools that need approval, run on the client, or start a subagent cannot be called from code.";

const SEARCH: &str = "Tools not declared here can be found with `tools.tool_search({ query })` \
and called with `tools.call_tool({ name, arguments })`.";

pub fn tool() -> ConnectorTool {
    filter::engine_tool(
        RUN_CODE,
        String::new(),
        serde_json::json!({
            "type": "object",
            "properties": {
                "code": {
                    "type": "string",
                    "description": "JavaScript: the body of an async function."
                }
            },
            "required": ["code"]
        }),
        ConnectorToolKind::Code,
    )
}

/// `run_code`'s description, with a declaration for each tool a script can call.
pub fn describe(tools: &[&LlmTool]) -> String {
    let lines: Vec<String> = tools.iter().map(|t| declaration(t)).collect();
    let mut sections = vec![INTRO.to_string()];
    if tools.iter().any(|t| t.name == TOOL_SEARCH) {
        sections.push(SEARCH.to_string());
    }
    sections.push(format!(
        "```ts\ndeclare const tools: {{\n{}\n}};\n```",
        lines.join("\n")
    ));
    sections.join("\n\n")
}

fn declaration(tool: &LlmTool) -> String {
    let name = match is_identifier(&tool.name) {
        true => tool.name.clone(),
        false => serde_json::to_string(&tool.name).unwrap_or_default(),
    };
    let input = tool
        .input
        .as_ref()
        .map(render_json_schema_to_typescript)
        .unwrap_or_else(|| "{}".to_string());
    let output = tool
        .output
        .as_ref()
        .map(render_json_schema_to_typescript)
        .unwrap_or_else(|| "unknown".to_string());
    let input = input.replace('\n', "\n  ");
    let output = output.replace('\n', "\n  ");
    format!("  {name}(args: {input}): Promise<{output}>;")
}

/// Whether a script may call `name`. `None` when it may.
pub fn refusal(
    name: &str,
    arguments: &str,
    config: Option<&AgentConfig>,
    connector_tools: &[ConnectorTool],
) -> Option<String> {
    if name == RUN_CODE {
        return Some(format!("`{RUN_CODE}` cannot call itself."));
    }
    let call = ToolCall {
        id: String::new(),
        call_type: "function".to_string(),
        function: ToolCallFunction {
            name: name.to_string(),
            arguments: arguments.to_string(),
        },
    };
    if crate::interrupts::approval::needed(&call, connector_tools) {
        return Some(format!(
            "`{name}` needs approval. Call it directly, not from code."
        ));
    }
    if let Some(tool) = connector_tools.iter().find(|t| t.name == name) {
        return match tool.kind {
            ConnectorToolKind::Subagent => Some(format!(
                "`{name}` starts a subagent. Call it directly, not from code."
            )),
            _ => None,
        };
    }
    match config.and_then(|c| c.tool(name)) {
        Some(t) if t.handler == Some(Handler::Client) => Some(format!(
            "`{name}` runs on the client. Call it directly, not from code."
        )),
        Some(_) => None,
        None => Some(format!("There is no tool named `{name}`.")),
    }
}

/// Whether `run_code`'s description declares this tool.
pub fn declares(
    tool: &LlmTool,
    config: Option<&AgentConfig>,
    connector_tools: &[ConnectorTool],
) -> bool {
    !tool.defer && refusal(&tool.name, "{}", config, connector_tools).is_none()
}

/// The names a script may call, sorted.
pub fn callable(config: Option<&AgentConfig>, connector_tools: &[ConnectorTool]) -> Vec<String> {
    let declared = config
        .into_iter()
        .flat_map(|c| c.tools.iter().map(|t| &t.name));
    let mut names: Vec<String> = declared
        .chain(connector_tools.iter().map(|t| &t.name))
        .filter(|name| refusal(name, "{}", config, connector_tools).is_none())
        .cloned()
        .collect();
    names.sort();
    names.dedup();
    names
}

/// A script waits on its calls, so its own deadline is the run's whole life.
pub fn retry() -> RetryPolicy {
    RetryPolicy {
        queue_timeout_secs: Some(60),
        run_timeout_secs: None,
        total_timeout_secs: Some(3600),
        max_attempts: 1,
        backoff_base_secs: 0,
        backoff_max_secs: 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tool(
        name: &str,
        input: Option<serde_json::Value>,
        output: Option<serde_json::Value>,
    ) -> LlmTool {
        LlmTool {
            name: name.to_string(),
            description: String::new(),
            input,
            output,
            defer: false,
        }
    }

    #[test]
    fn the_description_declares_each_tool_with_its_types() {
        let weather = tool(
            "weather",
            Some(serde_json::json!({
                "type": "object",
                "properties": { "city": { "type": "string" } },
                "required": ["city"]
            })),
            Some(serde_json::json!({
                "type": "object",
                "properties": { "temp": { "type": "number" } }
            })),
        );
        let dashed = tool("list-prs", None, None);
        let text = describe(&[&weather, &dashed]);
        assert!(text.contains("  weather(args: { city: string; }): Promise<{ temp?: number; }>;"));
        assert!(text.contains(r#"  "list-prs"(args: {}): Promise<unknown>;"#));
        assert!(!text.contains("tool_search"));
    }
}
