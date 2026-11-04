//! Code mode's text that is JavaScript.

use crate::protocol::{RUN_CODE, SKILL, TOOL_SEARCH};

/// `run_code`'s `code` argument.
pub const CODE: &str = "JavaScript: the body of an async function.";

/// The start of `run_code`'s description: how to write a script. `output` is
/// the KiB a run sends back before its middle is cut.
pub fn intro(output: usize) -> String {
    format!(
        "Run JavaScript code to orchestrate tool calls: chain them, run them at once, \
and filter large results before you read them.
- `code` is the body of an async function. All tools are on the global `tools` object. Call one \
by its exact name: `await tools.<name>(args)`, or `tools[\"<name>\"](args)` when the name is not \
an identifier. A call returns the tool's result, parsed when it is JSON.
- One call: `const issue = await tools.get_issue({{ id: 7 }}); return issue.title;`
- Calls at once: `const [a, b] = await Promise.all([tools.get_issue({{ id: 1 }}), \
tools.get_issue({{ id: 2 }})]);`. A call that fails throws an `Error` with the tool's message; use \
`Promise.allSettled` so that one failure does not lose the rest.
- You see what the script logs and returns: strings as they are, objects as JSON. Past {output} \
KiB, the sandbox cuts the middle.
- A script runs again from the start each time its calls finish, and gets their results without \
calling again. Work before a call is done again on each run. A script has limits on calls, \
results, memory and compute; an error names the limit and what to do.
- Files in the conversation show as `[attachment <id> …]` lines. Read one by its id with \
`await files.text(id)`, or stream it with `for await (const row of files.csv(id))`. \
`files.lines`, `files.jsonl` and `files.chunks` stream too. Each stream takes \
`{{ limit, offset }}` in records, so a script can start where the last one stopped. \
`files.csv` also takes `{{ header, delimiter }}`; `files.chunks` also takes `{{ size }}`, and \
counts its offset in bytes. \
`files.xlsx` streams the rows of an xlsx sheet like `files.csv`, with numbers, booleans and \
ISO dates as such, and takes `{{ sheet, header }}`; `files.sheets(id)` lists its sheets. \
`files.list()` lists the files.
- `files.show(id)` puts a file in front of you when the script returns: an image, PDF, audio or \
video as itself, any other file as its text.
- Load a skill with `console.log(await tools.{SKILL}({{ name }}))`, and several with \
`for (const s of await Promise.all([tools.{SKILL}({{ name: \"a\" }}), tools.{SKILL}({{ name: \"b\" }})])) \
console.log(s);`. Log each answer whole: it holds the skill's instructions and the declarations \
of the tools it brings, which you can call in your next script. Read the files it mentions with \
`files.text`, by their path under the skill's base directory.
- There is no network, file system, timer or module. No state persists between scripts. \
`Date` and `Math.random` stay the same for the whole run.
- A call that needs approval waits for a person, and a client tool waits for the client. A \
subagent call returns `{{ session, result }}`: its answer, and the session to pass as `session` \
to continue it."
    )
}

/// After the intro in `run_code`'s description: how to find the tools it
/// does not declare.
pub fn find() -> String {
    format!(
        "Find tools that are not declared here, and skills, with \
         `await tools.{TOOL_SEARCH}({{ query }})`, or name tools exactly to get just their \
         declarations. It answers with text that is already short: log it with \
         `console.log` as it is, then call the tools in your next script."
    )
}

/// The model called `named` directly, though code mode reaches it only from
/// `run_code`.
pub fn call_from_code(named: &str) -> String {
    format!("Call `{named}` from `{RUN_CODE}`: `await tools.{named}(args)`.")
}

/// A script called `name`, which no tool has. `near` are the tools named most
/// like it.
pub fn no_such_tool(name: &str, near: &[&str]) -> String {
    let find = format!("Find tools with `await tools.{TOOL_SEARCH}({{ query }})`.");
    match near.is_empty() {
        true => format!("There is no tool named `{name}`. {find}"),
        false => format!(
            "There is no tool named `{name}`. The closest are: {}. {find}",
            near.iter()
                .map(|n| format!("`{n}`"))
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

/// A skill a script's search found: what it is, and how to load it.
pub fn found_skill(name: &str, description: &str) -> String {
    format!(
        "- Skill `{name}`: {description} Load it with `await tools.{SKILL}({{ name: {name:?} }})`."
    )
}

/// After a block of declarations: the tools past its budget, `listed` one a
/// line, and how to get theirs.
pub fn get_declarations(names: &str, listed: &str) -> String {
    format!(
        "Get the declarations of these with \
         `await tools.{TOOL_SEARCH}({{ query: \"{names}\" }})`:\n{listed}"
    )
}

/// A script asked `skill` for a file; it reads files as files.
pub fn read_skill_file(file: &str) -> String {
    format!(
        "Read a skill's files with `files.text`, by their path under its base directory: \
         `files.text({file:?})`."
    )
}
