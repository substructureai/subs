//! `run_code` in JavaScript: tools as async functions, declared in TypeScript.

use std::collections::BTreeSet;

use super::typescript::{is_identifier, render_json_schema_to_typescript};
use super::Dialect;
use crate::prompts::codemode::javascript as prompts;
use crate::protocol::LlmTool;

pub const DIALECT: Dialect = Dialect {
    code: prompts::CODE,
    describe,
    declare: declaration,
    fence: "ts",
};

fn describe(tools: &[&LlmTool]) -> String {
    let budget = Budget::split(tools);
    let mut sections = vec![prompts::intro(super::OUTPUT >> 10), prompts::find()];
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

/// The tools `run_code`'s description declares, from the first, with the
/// types they name.
struct Budget {
    types: Vec<String>,
    declared: Vec<String>,
}

impl Budget {
    fn split(tools: &[&LlmTool]) -> Self {
        let mut taken = BTreeSet::new();
        let declarations: Vec<Declared> = tools.iter().map(|t| declared(t, &mut taken)).collect();
        let sizes: Vec<usize> = declarations
            .iter()
            .zip(tools)
            .map(|(d, t)| {
                let summary = super::summary(&t.description).len() + 6;
                summary + d.method.len() + 2 + d.types.iter().map(|t| t.len() + 1).sum::<usize>()
            })
            .collect();
        let count = fit(&sizes, super::DECLARED);
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

/// `tool`'s method and the types it names, with names not in `taken`.
fn declared(tool: &LlmTool, taken: &mut BTreeSet<String>) -> Declared {
    let name = match is_identifier(&tool.name) {
        true => tool.name.clone(),
        false => serde_json::to_string(&tool.name).unwrap_or_default(),
    };
    let mut types = Vec::new();
    let mut render = |schema: Option<&serde_json::Value>, empty: &str| match schema {
        Some(schema) => {
            let rendered = render_json_schema_to_typescript(schema, &tool.name, taken);
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
fn declaration(tool: &LlmTool, taken: &mut BTreeSet<String>) -> String {
    let declared = declared(tool, taken);
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
        let count = Budget::split(&refs).declared.len();
        assert!(count > 0 && count < 400);
        assert!(
            budget
                .declared
                .iter()
                .chain(&budget.types)
                .map(String::len)
                .sum::<usize>()
                <= super::super::DECLARED
        );
        let text = describe(&refs);
        assert!(text.contains(&format!("tool_{}(args:", count - 1)));
        assert!(!text.contains(&format!("tool_{count}(args:")));
        assert!(!text.contains("More detail."));
    }
}
