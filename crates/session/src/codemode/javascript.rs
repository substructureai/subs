//! `run_code` in JavaScript: tools as async functions, declared in TypeScript.

use std::collections::BTreeSet;

use super::typescript::{is_identifier, render_json_schema_to_typescript};
use super::Dialect;
use crate::protocol::{LlmTool, TOOL_SEARCH};

pub const DIALECT: Dialect = Dialect {
    code: "JavaScript: the body of an async function.",
    describe,
    declares,
    declare: declaration,
    fence: "ts",
};

/// Declarations `run_code`'s description holds, in bytes. Past it, a script
/// finds tools with `tool_search`.
const DECLARED: usize = 12 << 10;

const INTRO: &str = "Run JavaScript code to orchestrate tool calls: chain them, run them at once, \
and filter large results before you read them.
- `code` is the body of an async function. All tools are on the global `tools` object. Call one \
by its exact name: `await tools.<name>(args)`, or `tools[\"<name>\"](args)` when the name is not \
an identifier. A call returns the tool's result, parsed when it is JSON.
- One call: `const issue = await tools.get_issue({ id: 7 }); return issue.title;`
- Calls at once: `const [a, b] = await Promise.all([tools.get_issue({ id: 1 }), \
tools.get_issue({ id: 2 })]);`. A call that fails throws an `Error` with the tool's message; use \
`Promise.allSettled` so that one failure does not lose the rest.
- You see what the script logs and returns: strings as they are, objects as JSON. Past {output} \
KiB, the sandbox cuts the middle.
- A script runs again from the start each time its calls finish, and gets their results without \
calling again. Work before a call is done again on each run. A script has limits on calls, \
results, memory and compute; an error names the limit and what to do.
- Files in the conversation show as `[attachment <id> …]` lines. Read one by its id with \
`await files.text(id)`, or stream it with `for await (const row of files.csv(id))`. \
`files.lines`, `files.jsonl` and `files.chunks` stream too. Each stream takes `{ limit }`, \
`files.csv` also `{ header, delimiter }`, and `files.chunks` also `{ size, offset }`. \
`files.list()` lists the files.
- `files.show(id)` puts a file in front of you when the script returns: an image, PDF, audio or \
video as itself, any other file as its text.
- Load a skill with `console.log(await tools.skill({ name }))`, and several with \
`for (const s of await Promise.all([tools.skill({ name: \"a\" }), tools.skill({ name: \"b\" })])) \
console.log(s);`. Log each answer whole: it holds the skill's instructions and the declarations \
of the tools it brings, which you can call in your next script. Read the files it mentions with \
`files.text`, by their path under the skill's base directory.
- There is no network, file system, timer or module. No state persists between scripts. \
`Date` and `Math.random` stay the same for the whole run.
- A call that needs approval waits for a person, and a client tool waits for the client. A \
subagent call returns the subagent's answer.";

fn describe(tools: &[&LlmTool]) -> String {
    let budget = Budget::split(tools);
    let mut sections = vec![
        INTRO.replace(
            "{output}",
            &(super::Limits::default().output >> 10).to_string(),
        ),
        format!(
            "Find tools that are not declared here, and skills, with \
             `await tools.{TOOL_SEARCH}({{ query }})`, or name tools exactly to get just their \
             declarations. It answers with text that is already short: log it with \
             `console.log` as it is, then call the tools in your next script."
        ),
    ];
    if !budget.declared.is_empty() {
        sections.push(format!(
            "```ts\n{}declare const tools: {{\n{}\n}};\n```",
            budget
                .types
                .iter()
                .map(|t| format!("{t}\n"))
                .collect::<String>(),
            budget.declared.join("\n")
        ));
    }
    sections.join("\n\n")
}

fn declares(tools: &[&LlmTool]) -> usize {
    Budget::split(tools).declared.len()
}

/// The tools `run_code`'s description declares, from the first, with the
/// types they name.
struct Budget {
    types: Vec<String>,
    declared: Vec<String>,
}

impl Budget {
    fn split(tools: &[&LlmTool]) -> Self {
        let declarations: Vec<Declared> = tools.iter().map(|t| declared(t)).collect();
        let sizes: Vec<usize> = declarations
            .iter()
            .zip(tools)
            .map(|(d, t)| {
                let summary = super::summary(&t.description).len() + 6;
                summary + d.method.len() + 2 + d.types.iter().map(|t| t.len() + 1).sum::<usize>()
            })
            .collect();
        let count = fit(&sizes, DECLARED);
        let declarations = &declarations[..count];
        Budget {
            types: declarations.iter().flat_map(|d| d.types.clone()).collect(),
            declared: declarations
                .iter()
                .zip(tools)
                .map(|(d, t)| indent(&commented(&super::summary(&t.description), &d.method)))
                .collect(),
        }
    }
}

/// How many of `sizes`, from the first, fit in `bytes`.
fn fit(sizes: &[usize], bytes: usize) -> usize {
    let mut used = 0;
    sizes
        .iter()
        .take_while(|size| {
            used += *size;
            used <= bytes
        })
        .count()
}

fn indent(line: &str) -> String {
    format!("  {}", line.replace('\n', "\n  "))
}

/// A tool's method, and the types it names.
struct Declared {
    types: Vec<String>,
    method: String,
}

fn declared(tool: &LlmTool) -> Declared {
    let name = match is_identifier(&tool.name) {
        true => tool.name.clone(),
        false => serde_json::to_string(&tool.name).unwrap_or_default(),
    };
    let mut taken = BTreeSet::new();
    let mut types = Vec::new();
    let mut render = |schema: Option<&serde_json::Value>, empty: &str| match schema {
        Some(schema) => {
            let rendered = render_json_schema_to_typescript(schema, &tool.name, &mut taken);
            types.extend(rendered.types);
            rendered.body
        }
        None => empty.to_string(),
    };
    let input = render(tool.input.as_ref(), "{}");
    let output = render(tool.output.as_ref(), "unknown");
    Declared {
        types,
        method: format!("{name}(args: {input}): Promise<{output}>;"),
    }
}

/// A tool's declaration: the types it names, then its description, then its
/// method.
fn declaration(tool: &LlmTool) -> String {
    let declared = declared(tool);
    let mut lines = declared.types;
    lines.push(commented(&tool.description, &declared.method));
    lines.join("\n")
}

/// `code` after `text` as comment lines.
fn commented(text: &str, code: &str) -> String {
    text.lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(|line| format!("// {line}\n"))
        .chain([code.to_string()])
        .collect()
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
    fn the_description_declares_tools_until_its_budget() {
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
        let count = declares(&refs);
        assert!(count > 0 && count < 400);
        assert!(
            budget
                .declared
                .iter()
                .chain(&budget.types)
                .map(String::len)
                .sum::<usize>()
                <= DECLARED
        );
        let text = describe(&refs);
        assert!(text.contains(&format!("tool_{}(args:", count - 1)));
        assert!(!text.contains(&format!("tool_{count}(args:")));
        assert!(!text.contains("More detail."));
    }
}
