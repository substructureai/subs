//! Refusals and errors the model reads as a tool's result. A failed call's
//! error message is its result, so any error a call fails with is here.

use crate::protocol::{ConnectionPath, CALL_TOOL, SKILL, TOOL_SEARCH};

/// A call the engine routed to itself, that it could not answer.
pub const CALL_NOT_ROUTED: &str = "the call could not be routed";

/// `call_tool` named itself.
pub fn call_tool_in_call() -> String {
    format!("`{CALL_TOOL}` can't call itself. Name the tool you want in `name`, beside its `arguments`.")
}

/// The model called `named`, which a skill holds back until one of `skills`
/// loads.
pub fn locked(named: &str, skills: &[String]) -> String {
    let named_skills = skills
        .iter()
        .map(|s| format!("`{s}`"))
        .collect::<Vec<_>>()
        .join(" or ");
    format!(
        "`{named}` comes with the skill {named_skills}. Load it with `{SKILL}` first, then call \
         `{named}` again."
    )
}

/// `call_tool` named a tool the agent does not have. `near` are the tools
/// named most like it; `unavailable` the connections whose tools are not
/// listed.
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

/// A call's arguments fail its tool's input schema: a worker's tool the model
/// called, or any tool a script called.
pub fn invalid_arguments(error: &str) -> String {
    format!("invalid tool arguments: {error}")
}

/// `call_tool`'s arguments for `named` fail its input schema.
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

/// A `skill` call on a branch whose agent has no config.
pub const NO_CONFIG: &str = "this agent has no config on this branch";

/// A `skill` call for a skill whose bundle this engine does not hold.
pub fn skill_without_data(named: &str) -> String {
    format!("skill `{named}` has no data on this engine; re-apply the project")
}

/// A `skill` call for a skill its bundle on this engine does not hold.
pub fn skill_not_in_data(named: &str) -> String {
    format!("skill `{named}` is not in the data this engine holds; re-apply the project")
}

/// A `skill` call that names a plugin the agent does not have; `declared`
/// lists the ones it has.
pub fn no_such_plugin(plugin: &str, declared: &str) -> String {
    format!("no plugin `{plugin}` for this agent. Declared: {declared}.")
}

/// A `skill` call that names a skill its plugin does not have; `listed` are
/// the plugin's skills, one a line.
pub fn no_such_plugin_skill(skill: &str, plugin: &str, listed: &str) -> String {
    format!("no skill `{skill}` in plugin `{plugin}`. Its skills:\n{listed}")
}

/// A `skill` call that names a skill the agent does not have, when skills
/// are searched.
pub fn no_such_searched_skill(named: &str) -> String {
    format!("no skill `{named}`. Find skills with `{TOOL_SEARCH}`.")
}

/// A `skill` call that names a skill the agent does not have; `listed` are
/// its skills, one a line.
pub fn no_such_skill(named: &str, listed: &str) -> String {
    format!("no skill `{named}`. This agent's skills:\n{listed}")
}

/// A skill's stored file the host could not read.
pub fn skill_file_unread(path: &str, named: &str) -> String {
    format!("`{path}` in skill `{named}` could not be read.")
}

/// A skill's stored file whose blob is gone.
pub const SKILL_FILE_GONE: &str = "This file is no longer stored.";

/// A `skill` call that names a file the skill does not have; `listed` are its
/// files, one a line.
pub fn no_such_skill_file(path: &str, named: &str, listed: &str) -> String {
    format!("no file `{path}` in skill `{named}`. Its files:\n{listed}")
}

/// An attachment tool named an attachment the conversation does not have;
/// `declared` lists the ones it has.
pub fn no_such_attachment(named: &str, declared: &str) -> String {
    format!("no attachment `{named}` in this conversation. Attachments: {declared}")
}

/// `attachment_view` named an attachment that is not image, audio or video.
pub fn not_viewable(id: &str, mime: &str, tool: &str) -> String {
    format!("`{id}` is {mime}; {tool} takes image, audio, and video attachments")
}

/// `attachment_read` named an attachment that is not text.
pub fn not_readable(id: &str, mime: &str, tool: &str) -> String {
    format!("`{id}` is {mime}; {tool} takes text attachments")
}

/// An attachment bigger than its tool takes, `cap`.
pub fn attachment_too_big(id: &str, size: &str, tool: &str, cap: &str) -> String {
    format!("`{id}` is {size}; {tool} takes up to {cap}")
}

/// An attachment that is not in the blob store.
pub fn attachment_not_stored(id: &str) -> String {
    format!("`{id}` is not stored")
}

/// An attachment the blob store could not read.
pub fn attachment_unreadable(id: &str, error: &str) -> String {
    format!("`{id}` is unreadable: {error}")
}

/// A subagent call past the agent's depth limit.
pub fn subagent_depth(limit: u32) -> String {
    format!("subagent depth limit reached: max_subagent_depth is {limit}")
}

/// A subagent call whose `session` names no session the agent delegated to.
pub fn no_such_subagent_session(named: &str) -> String {
    format!("`{named}` names no session this agent delegated to")
}

/// A subagent call whose `session` belongs to another agent.
pub fn subagent_session_of(named: &str, owner: &str, asked: &str) -> String {
    format!("`{named}` is a session of `{owner}`, not of `{asked}`")
}

/// A subagent call to a session that is answering another call.
pub fn subagent_busy(agent: &str) -> String {
    format!("{agent} is already answering; wait for its result")
}

/// A `wait` without a detached call's `session`.
pub const WAIT_NEEDS_SESSION: &str = "`wait` needs a `session` from an earlier detached call";

/// A subagent call to a session still working detached.
pub fn subagent_detached(agent: &str) -> String {
    format!("{agent} is still working detached; wait for its result, or start a fresh session")
}

/// A person declined the model's call.
pub const DECLINED: &str = "A person declined this call. Do not try it again; say what you were \
     going to do and why it stopped.";

/// A person declined a call a script made.
pub fn script_call_declined(name: &str) -> String {
    format!("A person declined `{name}`.")
}

/// A tool's result fails its output schema.
pub fn output_violation(violation: &str) -> String {
    format!("tool output violated its declared schema: {violation}")
}

/// The model called a worker's tool its request did not offer.
pub fn unknown_tool(name: &str) -> String {
    format!("unknown tool: {name}")
}

/// A call that passed its deadline.
pub const DEADLINE: &str = "deadline exceeded";

/// A call that passed its deadline waiting in its queue.
pub const DEADLINE_QUEUED: &str = "deadline exceeded while queued";

/// A call that passed its deadline while it ran.
pub const DEADLINE_RUN: &str = "deadline exceeded while running";

/// A call whose work could not be sent to whatever runs it.
pub fn not_sent(error: &str) -> String {
    format!("could not send the work: {error}")
}

/// A subagent call whose opening message could not reach the child.
pub fn child_not_messaged(error: &str) -> String {
    format!("failed to send the child's opening message: {error}")
}

/// A subagent call whose child session could not be made.
pub fn child_not_created(error: &str) -> String {
    format!("failed to create child session: {error}")
}
