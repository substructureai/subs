use crate::protocol::{AuthNeed, ConnectionPath, TOOL_SEARCH};

pub const NOTHING_MATCHED: &str =
    "Nothing matched. Search again with an empty query for every tool.";

pub const CALL_NOT_ROUTED: &str = "the call could not be routed";

/// How the model loads a skill, in `run_code` when `code`.
pub fn plugin_usage(code: bool) -> &'static str {
    match code {
        false => "load a skill with the `skill` tool",
        true => "load a skill in `run_code` with `await tools.skill({ name })`",
    }
}

pub fn searched_skills(code: bool) -> &'static str {
    match code {
        false => "find its skills with `tool_search`, and load one with the `skill` tool",
        true => {
            "find its skills in `run_code` with `await tools.tool_search({ query })`, and load \
             one with `await tools.skill({ name })`"
        }
    }
}

pub const CALL_TOOL_IN_CALL: &str =
    "`call_tool` can't call itself. Name the tool you want in `name`, beside its `arguments`.";

pub fn locked(named: &str, skills: &[String]) -> String {
    let named_skills = skills
        .iter()
        .map(|s| format!("`{s}`"))
        .collect::<Vec<_>>()
        .join(" or ");
    format!("`{named}` comes with the skill {named_skills}. Load it with `skill` first, then call `{named}` again.")
}

pub fn declared<I>(ids: I) -> String
where
    I: IntoIterator,
    I::Item: AsRef<str>,
{
    let joined = ids
        .into_iter()
        .map(|id| id.as_ref().to_string())
        .collect::<Vec<_>>()
        .join(", ");
    match joined.is_empty() {
        true => "none".to_string(),
        false => joined,
    }
}

pub fn matches_truncated(shown: usize, matched: usize) -> String {
    format!(
        "{shown} of {matched} matches shown. Narrow the query to see the rest: a connector's \
         name is a word in every one of its tools, so adding it keeps the search to that \
         connection."
    )
}

pub fn no_such_tool(named: &str, near: &[&str], unavailable: &[ConnectionPath]) -> String {
    let mut said = if near.is_empty() {
        format!(
            "no tool `{named}` for this agent. Call `{TOOL_SEARCH}` with an empty query for \
             every tool."
        )
    } else {
        format!(
            "no tool `{named}` for this agent. The closest are: {}. Call `{TOOL_SEARCH}` for \
             the schema of one.",
            near.join(", ")
        )
    };
    for connection in unavailable {
        said.push_str(&format!(
            " `{connection}` is unavailable, so its tools are not listed."
        ));
    }
    said
}

pub fn bad_arguments(named: &str, error: &str, schema: Option<&serde_json::Value>) -> String {
    match schema {
        Some(schema) => format!("`{named}`: {error}. Its input schema is: {schema}"),
        None => format!("`{named}`: {error}"),
    }
}

/// A `call_tool` that named a tool but passed none of its arguments.
pub fn no_arguments(named: &str, error: &str, schema: Option<&serde_json::Value>) -> String {
    format!(
        "{} No arguments reached it: pass them in `arguments`, beside `name`.",
        bad_arguments(named, error, schema)
    )
}

pub fn unavailable_reason(auth: Option<AuthNeed>) -> &'static str {
    match auth {
        Some(_) => "needs_authorization",
        None => "unreachable",
    }
}

pub fn needs_authorization(connection: &str, need: AuthNeed) -> String {
    match need {
        AuthNeed::NeverAuthorized => {
            format!("`{connection}` is not authorized yet, so I cannot use it.")
        }
        AuthNeed::Reauthorize => {
            format!("`{connection}` needs to be authorized again. Its access expired.")
        }
        AuthNeed::TokenRejected => format!(
            "`{connection}` rejected its token. An operator must set a new one \
             with `subs auth {connection}`."
        ),
    }
}
