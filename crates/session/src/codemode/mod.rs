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
/// `run_code`'s description, which declares `tools` from the first, as many
/// as fit. It holds nothing that changes while a session runs.
pub fn describe(language: Language, tools: &[&LlmTool]) -> String {
    (dialect(language).describe)(tools)
}

/// How many of `tools`, from the first, `run_code`'s description declares.
pub fn declares(language: Language, tools: &[&LlmTool]) -> usize {
    (dialect(language).declares)(tools)
}

/// Characters of a listed description.
const SUMMARY: usize = 100;
/// Bytes of declarations a search answers with, but for one that names its
/// tools. Past it, matches are listed by name.
const SEARCHED: usize = 8 << 10;
/// Bytes an answer leaves free under the output limit.
pub const MARGIN: usize = 1 << 10;

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
    /// How many tools, from the first, `describe` declares.
    pub declares: fn(&[&LlmTool]) -> usize,
    /// One tool's declaration, with its description, as a search answers it.
    pub declare: fn(&LlmTool) -> String,
    /// The fence of a block of declarations.
    pub fence: &'static str,
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
    Engine,
    Files,
}

impl Reach {
    /// What a script that calls `name` is told, when it may not. `near` are
    /// the tools named most like it.
    pub fn refusal(self, name: &str, near: &[&str]) -> Option<String> {
        Some(match self {
            Reach::Code | Reach::Search => return None,
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
            Reach::Direct(Direct::Itself) => format!("`{name}` cannot call itself."),
            Reach::Direct(Direct::Engine) => format!(
                "`{name}` is not for code. Call tools by name, and find them with \
                 `await tools.{TOOL_SEARCH}({{ query }})`."
            ),
            Reach::Direct(Direct::Files) => format!(
                "`{name}` is not for code. Read files with `files`, and show one with \
                 `files.show`."
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
            ConnectorToolKind::Code => Reach::Direct(Direct::Itself),
            ConnectorToolKind::Attachment => Reach::Direct(Direct::Files),
            _ => Reach::Direct(Direct::Engine),
        };
    }
    match config.and_then(|c| c.tool(name)) {
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

/// A script's `tool_search`: the tools and skills that match `query`, best
/// first, as text for the model. Tools come with their declarations. A tool
/// named exactly comes first.
pub fn search(
    language: Language,
    tools: &[LlmTool],
    skills: &[NamedSkill],
    query: &str,
    max: NonZeroUsize,
) -> String {
    let skill_tools: Vec<LlmTool> = skills
        .iter()
        .map(|s| LlmTool {
            name: s.name.clone(),
            description: format!("{} {}", s.description, s.tools.join(" ")),
            input: None,
            output: None,
            defer: true,
        })
        .collect();
    let corpus: Vec<LlmTool> = tools.iter().chain(&skill_tools).cloned().collect();
    let words: Vec<&str> = query
        .split(|c: char| c.is_whitespace() || c == ',')
        .filter(|w| !w.is_empty())
        .collect();
    let named: Vec<&LlmTool> = words
        .iter()
        .filter_map(|w| tools.iter().find(|t| t.name == *w))
        .collect();
    let exact = !words.is_empty() && named.len() == words.len();
    let mut found = match exact {
        true => named,
        false => filter::find(&corpus, query),
    };
    if let Some(at) = found.iter().position(|t| t.name == query.trim()) {
        let named = found.remove(at);
        found.insert(0, named);
    }
    let shown = &found[..found.len().min(max.get())];
    let declarable: Vec<&LlmTool> = shown
        .iter()
        .filter(|t| !skills.iter().any(|s| s.name == t.name))
        .copied()
        .collect();
    let budget = match exact {
        true => Limits::default().output - MARGIN,
        false => SEARCHED,
    };
    let loaded: Vec<String> = shown
        .iter()
        .filter_map(|t| skills.iter().find(|s| s.name == t.name))
        .map(|s| {
            format!(
                "- Skill `{}`: {} Load it with `await tools.skill({{ name: {:?} }})`.",
                s.name,
                sentence(s.description),
                s.name
            )
        })
        .collect();
    let mut sections = vec![match found.len() {
        0 => crate::copy::NOTHING_MATCHED.to_string(),
        n if n > shown.len() => format!(
            "{} of {n} matches. Narrow the query to see the rest.",
            shown.len()
        ),
        1 => "1 match.".to_string(),
        n => format!("{n} matches."),
    }];
    if let Some(block) = declarations(language, &declarable, budget) {
        sections.push(block);
    }
    if !loaded.is_empty() {
        sections.push(loaded.join("\n"));
    }
    sections.join("\n\n")
}

/// `tools` declared in one block, as many as fit in `budget` bytes, then the
/// rest by name, to search for by name.
pub fn declarations(language: Language, tools: &[&LlmTool], budget: usize) -> Option<String> {
    let dialect = dialect(language);
    let mut used = 0;
    let mut declared = Vec::new();
    let mut rest = Vec::new();
    for tool in tools {
        let declaration = (dialect.declare)(tool);
        match rest.is_empty() && used + declaration.len() <= budget {
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
            description: format!("{name} audiences.\n{}", "x".repeat(5 << 10)),
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
        let named = answer("find_audiences, get_audience");
        assert!(named.contains("\nget_audience(args: "), "{named}");
    }

    #[test]
    fn declarations_past_the_budget_are_named() {
        let tool = |name: &str| LlmTool {
            name: name.into(),
            description: String::new(),
            input: None,
            output: None,
            defer: true,
        };
        let (a, b) = (tool("a"), tool("b"));
        let tools = [&a, &b];
        let both = declarations(Language::Javascript, &tools, 1 << 10).unwrap();
        assert_eq!(
            both,
            "```ts\na(args: {}): Promise<unknown>;\n\nb(args: {}): Promise<unknown>;\n```"
        );
        let one = declarations(Language::Javascript, &tools, 30).unwrap();
        assert!(
            one.ends_with(
                "Get the declarations of these with \
                 `await tools.tool_search({ query: \"b\" })`:\n- b: "
            ),
            "{one}"
        );
        assert_eq!(declarations(Language::Javascript, &[], 30), None);
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
