//! Descriptions of the engine's own tools and their arguments. The model
//! reads them in every request that offers the tool.

use crate::protocol::{SKILL, TOOL_SEARCH};

/// `tool_search`.
pub const SEARCH: &str = "Search the tools this agent can reach. They are not listed up front. \
     Answers with the name, the description, and the input schema of each match, best match \
     first. Any word can match, so a plain description of the task searches well. A connector's \
     name is a word in every one of its tools, so adding it keeps the search to that connection. \
     An empty query matches every tool: start there when you do not know what is available.";

/// After `tool_search`'s description, when the agent searches its skills.
pub fn search_skills() -> String {
    format!(
        " It finds skills too, which are not listed either. Load one with `{SKILL}` before you \
         start a task it covers."
    )
}

/// `tool_search`'s `query`.
pub const SEARCH_QUERY: &str = "Words to match against tool names and descriptions.";

/// `call_tool`.
pub fn call() -> String {
    format!(
        "Run one tool this agent can reach. Take `name` from `{TOOL_SEARCH}` exactly as it was \
         given, and pass that tool's own arguments in `arguments`."
    )
}

/// `call_tool`'s `name`.
pub const CALL_NAME: &str = "The tool's name, exactly as a search gave it.";

/// `call_tool`'s `arguments`.
pub const CALL_ARGUMENTS: &str =
    "The arguments that tool's own input schema declares; {} for one that takes none.";

/// `skill`.
pub const SKILL_TOOL: &str = "Load a skill's instructions, and follow what comes back. Name the \
     skill as it was listed or found: `<plugin>:<skill>` for a plugin's skill, the bare name for \
     one of the agent's own. Some tools come with a skill and can't be found or called until it \
     is loaded. Pass `file` to read one of the files the instructions reference.";

/// `skill`'s `name`.
pub const SKILL_NAME: &str = "The skill, as it was listed or found.";

/// `skill`'s `file`.
pub const SKILL_FILE: &str = "A file the skill's instructions reference, by its listed path.";

/// `subagent`, which delegates to any of the agent's subagents, `listed` one
/// a line.
pub fn subagent(listed: &str) -> String {
    format!(
        "Delegate a turn to one of this agent's subagents. The call runs in the named agent's \
         own session and answers with its result and that session's id. Pass the id back as \
         `session` to continue where it left off. Agents:\n{listed}"
    )
}

/// A subagent in `subagent`'s list. `mode` is [`ALWAYS_BLOCKING`],
/// [`ALWAYS_DETACHED`], or empty.
pub fn subagent_listed(id: &str, description: &str, mode: &str) -> String {
    match description.is_empty() {
        true => format!("- {id}{mode}"),
        false => format!("- {id} — {description}{mode}"),
    }
}

/// After a listed subagent that only blocks.
pub const ALWAYS_BLOCKING: &str = " [always blocking]";

/// After a listed subagent that only detaches.
pub const ALWAYS_DETACHED: &str = " [always detached]";

/// `subagent`'s `agent`.
pub const SUBAGENT_AGENT: &str = "The agent to delegate to.";

/// A per-agent subagent tool whose agent has no description.
pub fn delegate_to(id: &str) -> String {
    format!("Delegate to {id}")
}

/// After a per-agent subagent tool's description, when it only detaches.
pub const DETACHED_ONLY: &str = " Runs detached: the call answers at once with the session id, \
     and the result arrives as a later message.";

/// A subagent tool's `message`.
pub const SUBAGENT_MESSAGE: &str = "The message to send to the agent";

/// A subagent tool's `session`.
pub const SUBAGENT_SESSION: &str =
    "Session id of an earlier call to this agent, to continue that conversation";

/// A subagent tool's `mode`, when it offers both.
pub const SUBAGENT_MODE: &str = "How the call returns [default: blocking]. `blocking` waits and \
     answers with the result. `detached` answers at once with the session id; the result \
     arrives later as a message.";

/// `subagent_wait`.
pub const WAIT: &str = "Wait for the result of an earlier detached subagent call. Pass the \
     `session` that call answered with. Answers at once when the result is already in, and \
     holds until the child finishes when it is not.";

/// `subagent_wait`'s `session`.
pub const WAIT_SESSION: &str = "The detached call's session id";

/// `attachment_read`.
pub const ATTACHMENT_READ: &str = "Read a text attachment. Answers one chunk, with the total \
     size and line count so you can ask for the next. Name the attachment by the id in its \
     `[attachment …]` line.";

/// `attachment_view`.
pub const ATTACHMENT_VIEW: &str = "Look at an image, audio, or video attachment. Answers with \
     the file itself. Name the attachment by the id in its `[attachment …]` line.";

/// An attachment tool's `attachment`.
pub const ATTACHMENT_ID: &str = "The attachment id, exactly as its line gives it.";

/// `attachment_read`'s `from_line`.
pub const ATTACHMENT_FROM_LINE: &str = "First line to answer with, counting from 1.";

/// `attachment_read`'s `lines`.
pub const ATTACHMENT_LINES: &str = "How many lines to answer with.";
