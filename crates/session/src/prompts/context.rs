//! What the engine adds to the prompt: listings of plugins, skills and
//! servers, told once per branch.

use crate::protocol::{AuthNeed, RUN_CODE, SKILL, TOOL_SEARCH};

/// The `usage` of a plugin or skill listing: how the model loads a skill,
/// from `run_code` when `code`.
pub fn plugin_usage(code: bool) -> String {
    match code {
        false => format!("load a skill with the `{SKILL}` tool"),
        true => format!("load a skill in `{RUN_CODE}` with `await tools.{SKILL}({{ name }})`"),
    }
}

/// The `usage` of a plugin listing when the agent searches its skills.
pub fn searched_skills() -> String {
    format!("find its skills with `{TOOL_SEARCH}`, and load one with the `{SKILL}` tool")
}

/// The `reason` of a connection the prompt lists as unavailable.
pub fn unavailable_reason(auth: Option<AuthNeed>) -> &'static str {
    match auth {
        Some(_) => "needs_authorization",
        None => "unreachable",
    }
}

/// A detached subagent's result, arriving as a later user message.
pub fn subagent_result(agent: &str, session: &str, failed: bool, body: &str) -> String {
    let error = match failed {
        true => " error=\"true\"",
        false => "",
    };
    format!(
        "<subagent_result agent=\"{agent}\" session=\"{session}\"{error}>\n{body}\n\
         </subagent_result>"
    )
}

/// The system prompt of the model that summarizes a conversation.
pub const SUMMARY_SYSTEM: &str = "You write a handoff summary of a conversation for another model that will continue it. Do not continue the conversation. Do not answer what it asks.";

/// What the summarizing model is asked, after the conversation, unless the
/// strategy gives its own prompt.
pub const HANDOFF: &str = "Summarize the conversation above for whoever continues it. Use these sections, and leave out a section that does not apply.

## Goal
- What the user wants, in one or two lines.

## Requirements
- The rules, preferences and limits the user gave.

## Decisions
- What was decided, and why.

## Done
- What is finished.

## Open
- What is in progress, blocked, or not yet answered.

## Next
1. The next steps, in order.

## Facts
- The names, ids, numbers and values that later steps need.

Keep each line short. Keep names, ids and values exact. Do not say that this is a summary.";

/// In place of the summarized part of a conversation.
pub fn summarized(summary: &str) -> String {
    format!("The earlier conversation was summarized:\n\n{summary}")
}

/// What the summarizing model reads: the conversation, then what it is
/// `asked`.
pub fn summary_asked(conversation: &str, asked: &str) -> String {
    format!("<conversation>\n{conversation}\n</conversation>\n\n{asked}")
}

/// A user message in the conversation the summarizing model reads.
pub fn user_said(text: &str) -> String {
    format!("[User]: {text}")
}

/// What the assistant said, in the conversation the summarizing model reads.
pub fn assistant_said(text: &str) -> String {
    format!("[Assistant]: {text}")
}

/// A call the assistant made, in the conversation the summarizing model reads.
pub fn assistant_called(name: &str, arguments: &str) -> String {
    format!("[Assistant called {name}]: {arguments}")
}

/// A tool's result, in the conversation the summarizing model reads.
pub fn tool_result(text: &str) -> String {
    format!("[Tool result]: {text}")
}

/// A tool's result cut short, `more` characters left out.
pub fn clipped(kept: &str, more: usize) -> String {
    format!("{kept}… [{more} more characters]")
}

/// A conversation too long to summarize whole, its start left out.
pub fn left_out(kept: &str) -> String {
    format!("[earlier part left out]\n\n{kept}")
}

/// A file in the conversation, where the model can name it.
pub fn attachment(id: &str, mime: &str, size: &str) -> String {
    format!("[attachment {id} {mime} {size}]")
}

/// A Slack message, with the person who wrote it.
pub fn slack_message(author: &str, text: &str) -> String {
    format!("<@{author}>: {text}")
}

/// After a Slack message: its files the engine could not read, as
/// [`slack_file`].
pub fn slack_unreadable(files: &[String]) -> String {
    format!("\n[unreadable attachments: {}]", files.join(", "))
}

/// A Slack file the engine could not read.
pub fn slack_file(name: Option<&str>, mime: &str) -> String {
    format!("{} ({mime})", name.unwrap_or("attachment"))
}
