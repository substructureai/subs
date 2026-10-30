mod contract;
mod javascript;
mod typescript;

pub use contract::*;
pub use typescript::{is_identifier, render_json_schema_to_typescript};

use std::num::NonZeroUsize;

use crate::agent_config::NamedSkill;
use crate::connectors::filter;
use crate::protocol::{
    AgentConfig, ConnectorTool, ConnectorToolKind, LlmTool, RetryPolicy, RUN_CODE, TOOL_SEARCH,
};

/// `run_code` as the engine routes it. [`offered`] is the tool as the model
/// gets it.
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

/// `run_code` as the model gets it: its description declares `tools`, from
/// the first, as many as fit, and holds nothing that changes while a session
/// runs.
pub fn offered(language: Language, tools: &[&LlmTool]) -> LlmTool {
    LlmTool {
        description: (dialect(language).describe)(tools),
        ..tool(language).to_llm_tool()
    }
}

/// Characters of a listed description.
const SUMMARY: usize = 100;
/// Bytes of one block of declarations: in `run_code`'s description, a search
/// answer, or a skill's answer. Past it, tools are listed by name. The first
/// tool is always declared, so a search for its name always finds it.
pub const DECLARED: usize = 12 << 10;

/// The first line of `description`, cut at [`SUMMARY`] characters.
pub fn summary(description: &str) -> String {
    let line = description.lines().next().unwrap_or_default().trim();
    match line.char_indices().nth(SUMMARY) {
        Some((at, _)) => format!("{}…", &line[..at]),
        None => line.to_string(),
    }
}

/// How `run_code` is put to the model in one language.
pub struct Dialect {
    /// What `code` holds.
    pub code: &'static str,
    pub describe: fn(&[&LlmTool]) -> String,
    /// One tool's declaration, with its description, as a search answers it.
    /// The types it names avoid the names in the set, and join it.
    pub declare: fn(&LlmTool, &mut std::collections::BTreeSet<String>) -> String,
    /// The fence of a block of declarations.
    pub fence: &'static str,
}

pub fn dialect(language: Language) -> &'static Dialect {
    match language {
        Language::Javascript => &javascript::DIALECT,
    }
}

/// How a script reaches a tool. In code mode the model gets only
/// `run_code`, and the agent's tool set holds only what scripts reach.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reach {
    /// A tool a script calls.
    Code,
    /// `tool_search`, answered over the tools scripts call.
    Search,
    /// `run_code` itself.
    Itself,
    /// There is no such tool.
    Unknown,
}

impl Reach {
    /// What a script that calls `name` is told, when it may not. `near` are
    /// the tools named most like it.
    pub fn refusal(self, name: &str, near: &[&str]) -> Option<String> {
        Some(match self {
            Reach::Code | Reach::Search => return None,
            Reach::Itself => format!("`{name}` cannot call itself."),
            Reach::Unknown if near.is_empty() => format!(
                "There is no tool named `{name}`. Find tools with \
                 `await tools.{TOOL_SEARCH}({{ query }})`."
            ),
            Reach::Unknown => format!(
                "There is no tool named `{name}`. The closest are: {}. Find tools with \
                 `await tools.{TOOL_SEARCH}({{ query }})`.",
                near.iter()
                    .map(|n| format!("`{n}`"))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        })
    }
}

pub fn reach(name: &str, config: Option<&AgentConfig>, connector_tools: &[ConnectorTool]) -> Reach {
    if name == TOOL_SEARCH {
        return Reach::Search;
    }
    if let Some(tool) = connector_tools.iter().find(|t| t.name == name) {
        return match tool.kind {
            ConnectorToolKind::Remote | ConnectorToolKind::Subagent | ConnectorToolKind::Skill => {
                Reach::Code
            }
            ConnectorToolKind::Code => Reach::Itself,
            _ => Reach::Unknown,
        };
    }
    match config.and_then(|c| c.tool(name)) {
        Some(_) => Reach::Code,
        None => Reach::Unknown,
    }
}

/// A script's `tool_search`: [`filter::search`]'s matches as text for the
/// model, the tools with their declarations.
pub fn search(
    language: Language,
    tools: &[LlmTool],
    skills: &[NamedSkill],
    query: &str,
    max: NonZeroUsize,
) -> String {
    let found = filter::search(tools, skills, query, max);
    let loaded: Vec<String> = found
        .skills
        .iter()
        .map(|s| {
            format!(
                "- Skill `{}`: {} Load it with `await tools.skill({{ name: {:?} }})`.",
                s.name,
                sentence(s.description),
                s.name
            )
        })
        .collect();
    let shown = found.tools.len() + found.skills.len();
    let mut sections = vec![match found.matched {
        0 => crate::copy::NOTHING_MATCHED.to_string(),
        n if n > shown => format!("{shown} of {n} matches. Narrow the query to see the rest."),
        1 => "1 match.".to_string(),
        n => format!("{n} matches."),
    }];
    if let Some(block) = declarations(language, &found.tools) {
        sections.push(block);
    }
    if !loaded.is_empty() {
        sections.push(loaded.join("\n"));
    }
    sections.join("\n\n")
}

/// `tools` declared in one block, as many as fit in [`DECLARED`], the first
/// always, then the rest by name, to search for by name.
pub fn declarations(language: Language, tools: &[&LlmTool]) -> Option<String> {
    let dialect = dialect(language);
    let mut taken = std::collections::BTreeSet::new();
    let mut used = 0;
    let mut declared = Vec::new();
    let mut rest = Vec::new();
    for tool in tools {
        let declaration = (dialect.declare)(tool, &mut taken);
        match declared.is_empty() || (rest.is_empty() && used + declaration.len() <= DECLARED) {
            true => {
                used += declaration.len();
                declared.push(declaration);
            }
            false => rest.push(*tool),
        }
    }
    let mut sections = Vec::new();
    if !declared.is_empty() {
        sections.push(format!(
            "```{}\n{}\n```",
            dialect.fence,
            declared.join("\n\n")
        ));
    }
    if !rest.is_empty() {
        let names = rest
            .iter()
            .map(|t| t.name.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        let listed = rest
            .iter()
            .map(|t| format!("- {}: {}", t.name, summary(&t.description)))
            .collect::<Vec<_>>()
            .join("\n");
        sections.push(format!(
            "Get the declarations of these with \
             `await tools.{TOOL_SEARCH}({{ query: \"{names}\" }})`:\n{listed}"
        ));
    }
    (!sections.is_empty()).then(|| sections.join("\n\n"))
}

/// `text` trimmed, ending in a full stop.
fn sentence(text: &str) -> String {
    let text = text.trim();
    match text.ends_with(['.', '!', '?']) || text.is_empty() {
        true => text.to_string(),
        false => format!("{text}."),
    }
}

/// A script waits on its calls, which keep their own clocks, and its budgets
/// bound what it runs. So `run_code` has no deadline of its own. Each step's
/// wait for a sandbox is the runtime's `script_wait`.
pub fn retry() -> RetryPolicy {
    RetryPolicy {
        queue_timeout_secs: None,
        run_timeout_secs: None,
        total_timeout_secs: None,
        max_attempts: 1,
        backoff_base_secs: 0,
        backoff_max_secs: 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_search_that_names_tools_answers_only_them() {
        let tool = |name: &str| LlmTool {
            name: name.into(),
            description: format!("{name} audiences."),
            input: None,
            output: None,
            defer: true,
        };
        let tools = [
            tool("find_audiences"),
            tool("get_audience"),
            tool("create_audience"),
        ];
        let answer = |query| {
            search(
                Language::Javascript,
                &tools,
                &[],
                query,
                NonZeroUsize::new(5).unwrap(),
            )
        };
        assert!(answer("get_audience").starts_with("1 match."));
        let two = answer("create_audience, find_audiences");
        assert!(
            two.starts_with("2 matches.") && !two.contains("get_audience"),
            "{two}"
        );
        assert!(answer("audiences").starts_with("3 matches."));
    }

    #[test]
    fn a_broad_search_lists_what_does_not_fit_and_a_named_one_declares_it() {
        let tool = |name: &str| LlmTool {
            name: name.into(),
            description: format!("{name} audiences.\n{}", "x".repeat(7 << 10)),
            input: None,
            output: None,
            defer: true,
        };
        let tools = [tool("find_audiences"), tool("get_audience")];
        let answer = |query| {
            search(
                Language::Javascript,
                &tools,
                &[],
                query,
                NonZeroUsize::new(5).unwrap(),
            )
        };
        let broad = answer("audiences");
        assert!(broad.contains("find_audiences(args: "), "{broad}");
        assert!(
            broad.contains("query: \"get_audience\" })`:\n- get_audience: get_audience audiences."),
            "{broad}"
        );
        let named = answer("get_audience");
        assert!(named.contains("\nget_audience(args: "), "{named}");
    }

    #[test]
    fn declarations_past_the_budget_are_named_but_the_first_is_always_declared() {
        let tool = |name: &str, size: usize| LlmTool {
            name: name.into(),
            description: "x".repeat(size),
            input: None,
            output: None,
            defer: true,
        };
        let (a, b) = (tool("a", 0), tool("b", 0));
        let both = declarations(Language::Javascript, &[&a, &b]).unwrap();
        assert_eq!(
            both,
            "```ts\na(args: {}): Promise<unknown>;\n\nb(args: {}): Promise<unknown>;\n```"
        );
        let (big, small) = (tool("big", DECLARED * 2), tool("small", 0));
        let one = declarations(Language::Javascript, &[&big, &small]).unwrap();
        assert!(
            one.contains("\nbig(args: "),
            "the first is declared, however big"
        );
        assert!(
            one.ends_with(
                "Get the declarations of these with \
                 `await tools.tool_search({ query: \"small\" })`:\n- small: "
            ),
            "{one}"
        );
        assert_eq!(declarations(Language::Javascript, &[]), None);
    }

    #[test]
    fn a_search_answers_declarations_and_skills_as_text() {
        let tools = [LlmTool {
            name: "find_audiences".into(),
            description: "Find audiences by name.".into(),
            input: Some(serde_json::json!({
                "type": "object",
                "properties": { "query": { "type": "string" } }
            })),
            output: None,
            defer: true,
        }];
        let skills = [NamedSkill {
            name: "audiences".into(),
            description: "Build an audience.",
            tools: &[],
            source: Default::default(),
            plugin: None,
            bundle: "b",
            skill: "audiences",
        }];
        let answer = search(
            Language::Javascript,
            &tools,
            &skills,
            "audiences",
            NonZeroUsize::new(5).unwrap(),
        );
        assert_eq!(
            answer,
            "2 matches.\n\n```ts\n// Find audiences by name.\n\
             find_audiences(args: { query?: string; }): Promise<unknown>;\n```\n\n\
             - Skill `audiences`: Build an audience. Load it with \
             `await tools.skill({ name: \"audiences\" })`."
        );
    }
}
