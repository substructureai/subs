mod contract;
mod javascript;
mod typescript;

pub use contract::*;
pub use typescript::{is_identifier, render_json_schema_to_typescript};

use crate::connectors::filter;
use crate::protocol::{
    AgentConfig, ConnectorTool, ConnectorToolKind, Handler, LlmTool, RetryPolicy, ToolCall,
    ToolCallFunction, RUN_CODE,
};

pub fn tool(language: Language) -> ConnectorTool {
    filter::engine_tool(
        RUN_CODE,
        String::new(),
        serde_json::json!({
            "type": "object",
            "properties": {
                "code": {
                    "type": "string",
                    "description": dialect(language).code
                }
            },
            "required": ["code"]
        }),
        ConnectorToolKind::Code,
    )
}

/// `run_code`'s description, with a declaration for each tool a script can call.
pub fn describe(language: Language, tools: &[&LlmTool]) -> String {
    (dialect(language).describe)(tools)
}

/// How `run_code` is put to the model in one language.
pub struct Dialect {
    /// What `code` holds.
    pub code: &'static str,
    pub describe: fn(&[&LlmTool]) -> String,
}

pub fn dialect(language: Language) -> &'static Dialect {
    match language {
        Language::Javascript => &javascript::DIALECT,
    }
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
/// Each step's wait for a sandbox is the runtime's `script_wait`.
pub fn retry() -> RetryPolicy {
    RetryPolicy {
        queue_timeout_secs: None,
        run_timeout_secs: None,
        total_timeout_secs: Some(3600),
        max_attempts: 1,
        backoff_base_secs: 0,
        backoff_max_secs: 0,
    }
}
