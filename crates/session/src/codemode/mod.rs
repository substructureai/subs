mod contract;
mod javascript;
mod typescript;

pub use contract::*;
pub use typescript::{is_identifier, render_json_schema_to_typescript};

use std::num::NonZeroUsize;

use crate::connectors::filter;
use crate::protocol::{
    AgentConfig, ConnectorTool, ConnectorToolKind, Handler, LlmTool, RetryPolicy, RUN_CODE,
    TOOL_SEARCH,
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
    /// One tool's declaration, as a search answers it.
    pub declare: fn(&LlmTool) -> String,
}

pub fn dialect(language: Language) -> &'static Dialect {
    match language {
        Language::Javascript => &javascript::DIALECT,
    }
}

/// How the model reaches a tool when its agent has code mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reach {
    /// Only from code: `run_code` offers it in place of the tool itself.
    Code,
    /// `tool_search`: directly, and from code over the tools code reaches.
    Search,
    /// Only directly, for this reason.
    Direct(Direct),
    /// There is no such tool.
    Unknown,
}

/// Why a tool is called only directly.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direct {
    Itself,
    Approval,
    Subagent,
    Client,
    Engine,
}

impl Reach {
    /// What a script that calls `name` is told, when it may not.
    pub fn refusal(self, name: &str) -> Option<String> {
        let why = match self {
            Reach::Code | Reach::Search => return None,
            Reach::Unknown => {
                return Some(format!(
                    "There is no tool named `{name}`. Find tools with `{TOOL_SEARCH}`."
                ))
            }
            Reach::Direct(Direct::Itself) => return Some(format!("`{name}` cannot call itself.")),
            Reach::Direct(Direct::Approval) => "needs approval",
            Reach::Direct(Direct::Subagent) => "starts a subagent",
            Reach::Direct(Direct::Client) => "runs on the client",
            Reach::Direct(Direct::Engine) => "is the engine's",
        };
        Some(format!("`{name}` {why}. Call it directly, not from code."))
    }
}

pub fn reach(name: &str, config: Option<&AgentConfig>, connector_tools: &[ConnectorTool]) -> Reach {
    if name == TOOL_SEARCH {
        return Reach::Search;
    }
    if let Some(tool) = connector_tools.iter().find(|t| t.name == name) {
        return match tool.kind {
            _ if tool.approve => Reach::Direct(Direct::Approval),
            ConnectorToolKind::Remote => Reach::Code,
            ConnectorToolKind::Subagent => Reach::Direct(Direct::Subagent),
            ConnectorToolKind::Code => Reach::Direct(Direct::Itself),
            _ => Reach::Direct(Direct::Engine),
        };
    }
    match config.and_then(|c| c.tool(name)) {
        Some(t) if t.handler == Some(Handler::Client) => Reach::Direct(Direct::Client),
        Some(_) => Reach::Code,
        None => Reach::Unknown,
    }
}

/// The names a script may call, sorted.
pub fn callable(config: Option<&AgentConfig>, connector_tools: &[ConnectorTool]) -> Vec<String> {
    let declared = config
        .into_iter()
        .flat_map(|c| c.tools.iter().map(|t| &t.name));
    let mut names: Vec<String> = declared
        .chain(connector_tools.iter().map(|t| &t.name))
        .filter(|name| reach(name, config, connector_tools) == Reach::Code)
        .cloned()
        .chain([TOOL_SEARCH.to_string()])
        .collect();
    names.sort();
    names.dedup();
    names
}

/// A script's `tool_search`: the tools that match `query`, best first, each
/// with its declaration. A tool named exactly comes first.
pub fn search(language: Language, tools: &[LlmTool], query: &str, max: NonZeroUsize) -> String {
    let mut found = filter::find(tools, query);
    if let Some(at) = found.iter().position(|t| t.name == query.trim()) {
        let named = found.remove(at);
        found.insert(0, named);
    }
    let shown: Vec<serde_json::Value> = found
        .iter()
        .take(max.get())
        .map(|tool| {
            serde_json::json!({
                "name": tool.name,
                "description": tool.description,
                "declaration": (dialect(language).declare)(tool),
            })
        })
        .collect();
    let mut answer = serde_json::json!({ "tools": shown, "matched": found.len() });
    if found.is_empty() {
        answer["note"] = serde_json::json!(crate::copy::NOTHING_MATCHED);
    } else if found.len() > max.get() {
        answer["note"] = serde_json::json!(crate::copy::matches_truncated(max.get(), found.len()));
    }
    answer.to_string()
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
