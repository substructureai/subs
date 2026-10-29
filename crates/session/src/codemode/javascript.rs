//! `run_code` in JavaScript: tools as async functions, declared in TypeScript.

use super::typescript::{is_identifier, render_json_schema_to_typescript};
use super::Dialect;
use crate::protocol::{LlmTool, TOOL_SEARCH};

pub const DIALECT: Dialect = Dialect {
    code: "JavaScript: the body of an async function.",
    describe,
    declare: declaration,
};

/// Declarations `run_code`'s description holds, in bytes. Past it, tools are
/// listed by name.
const DECLARED: usize = 12 << 10;
/// Names `run_code`'s description lists, in bytes.
const LISTED: usize = 4 << 10;
/// Characters of a listed tool's description.
const SUMMARY: usize = 100;

const INTRO: &str = "Run JavaScript that calls your tools. Use it to chain calls, to run them at \
once, or to filter a large result before you read it.
- `code` is the body of an async function. `return` a value to get it back.
- Call a tool with `await tools.<name>(args)`. Use `tools[\"<name>\"](args)` when the name is not \
an identifier. A call returns the tool's result, parsed when it is JSON.
- A call that fails throws an `Error` with the tool's message. For many calls at once, use \
`Promise.allSettled`, so one failure does not lose the rest.
- Tools that the worker runs answer one at a time. Other tools run at once.
- `console.log` output comes back with the return value. Results you do not return or log stay \
out of the conversation.
- Return a summary, not raw results: what a script returns and logs is capped.
- A script resumes from the start after its calls finish, so work done before a call is done \
again each time. It has limits on its calls, on the results it reads, on memory and on compute; \
an error names the limit and what to do.
- There is no network, file system, timer or module. `Date` and `Math.random` stay the same \
for the whole run.
- Tools that need approval, run on the client, or start a subagent cannot be called from code.";

fn describe(tools: &[&LlmTool]) -> String {
    let budget = Budget::split(tools);
    let mut sections = vec![
        INTRO.to_string(),
        format!(
            "Find tools with `await tools.{TOOL_SEARCH}({{ query }})`. It answers with each \
             match's declaration. Search before you call a tool that is not declared here."
        ),
        format!(
            "```ts\ndeclare const tools: {{\n{}\n}};\n```",
            budget.declared.join("\n")
        ),
    ];
    if !budget.listed.is_empty() {
        let mut listed = budget.listed;
        if budget.more > 0 {
            listed.push(format!("- and {} more.", budget.more));
        }
        sections.push(format!(
            "These tools can be called too. Search for one by its name to get its \
             declaration:\n{}",
            listed.join("\n")
        ));
    }
    sections.join("\n\n")
}

/// The tools `run_code`'s description declares, those it lists by name, and
/// how many more there are.
struct Budget {
    declared: Vec<String>,
    listed: Vec<String>,
    more: usize,
}

impl Budget {
    fn split(tools: &[&LlmTool]) -> Self {
        let lines: Vec<String> = tools.iter().map(|t| indent(&declaration(t))).collect();
        let declared = fit(&lines, DECLARED);
        let entries: Vec<String> = tools[declared..]
            .iter()
            .map(|t| format!("- {}: {}", t.name, summary(&t.description)))
            .collect();
        let listed = fit(&entries, LISTED);
        Budget {
            more: entries.len() - listed,
            declared: lines[..declared].to_vec(),
            listed: entries[..listed].to_vec(),
        }
    }
}

/// How many of `lines`, from the first, fit in `bytes`.
fn fit(lines: &[String], bytes: usize) -> usize {
    let mut used = 0;
    lines
        .iter()
        .take_while(|line| {
            used += line.len();
            used <= bytes
        })
        .count()
}

fn indent(line: &str) -> String {
    format!("  {}", line.replace('\n', "\n  "))
}

fn summary(description: &str) -> String {
    let line = description.lines().next().unwrap_or_default().trim();
    match line.char_indices().nth(SUMMARY) {
        Some((at, _)) => format!("{}…", &line[..at]),
        None => line.to_string(),
    }
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
    format!("{name}(args: {input}): Promise<{output}>;")
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
        assert!(text.contains("tools.tool_search({ query })"));
    }

    #[test]
    fn tools_past_the_budget_are_listed_by_name() {
        let schema = serde_json::json!({
            "type": "object",
            "properties": { "a_long_property_name": { "type": "string" } }
        });
        let tools: Vec<LlmTool> = (0..400)
            .map(|i| LlmTool {
                description: format!("Tool number {i}.\nMore detail."),
                ..tool(&format!("tool_{i}"), Some(schema.clone()), None)
            })
            .collect();
        let refs: Vec<&LlmTool> = tools.iter().collect();
        let budget = Budget::split(&refs);
        assert!(!budget.declared.is_empty() && !budget.listed.is_empty() && budget.more > 0);
        assert_eq!(
            budget.declared.len() + budget.listed.len() + budget.more,
            400
        );
        assert!(budget.declared.iter().map(String::len).sum::<usize>() <= DECLARED);
        assert!(budget.listed.iter().map(String::len).sum::<usize>() <= LISTED);
        let text = describe(&refs);
        assert!(text.contains(&format!("- and {} more.", budget.more)));
        assert!(!text.contains("More detail."));
    }
}
