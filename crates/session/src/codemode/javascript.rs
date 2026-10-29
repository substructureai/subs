//! `run_code` in JavaScript: tools as async functions, declared in TypeScript.

use super::typescript::{is_identifier, render_json_schema_to_typescript};
use super::Dialect;
use crate::protocol::{LlmTool, TOOL_SEARCH};

pub const DIALECT: Dialect = Dialect {
    code: "JavaScript: the body of an async function.",
    describe,
};

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

const SEARCH: &str = "Tools not declared here can be found with `tools.tool_search({ query })` \
and called with `tools.call_tool({ name, arguments })`.";

fn describe(tools: &[&LlmTool]) -> String {
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
