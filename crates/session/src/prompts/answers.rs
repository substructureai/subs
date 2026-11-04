//! How the engine lays out the answers it writes itself.

/// A `tool_search` that matched nothing, direct or from a script.
pub const NOTHING_MATCHED: &str =
    "Nothing matched. Search again with an empty query for every tool.";

/// A direct `tool_search` matched more than it shows.
pub fn matches_truncated(shown: usize, matched: usize) -> String {
    format!(
        "{shown} of {matched} matches shown. Narrow the query to see the rest: a connector's \
         name is a word in every one of its tools, so adding it keeps the search to that \
         connection."
    )
}

/// A skill the model loaded: its name, description and instructions, then
/// its files, `listed` one a line.
pub fn skill(named: &str, description: &str, body: &str, listed: &str) -> String {
    let mut answer = format!("Skill {named} — {description}\n\n{body}");
    if !listed.is_empty() {
        answer.push_str(&format!(
            "\n\nFiles (read one with `{}` and `file`):\n{listed}",
            crate::protocol::SKILL
        ));
    }
    answer
}

/// A skill in a list of skills.
pub fn skill_listed(name: &str, description: &str) -> String {
    format!("{name} — {description}")
}

/// A skill's text file in its list of files.
pub fn skill_file(path: &str) -> String {
    format!("- {path}")
}

/// A skill's binary file in its list of files.
pub fn skill_binary(path: &str, mime: &str, size: u64) -> String {
    format!("- {path} ({mime}, {size} bytes)")
}

/// A chunk of a text attachment: what it is, which lines these are, then
/// the lines.
pub fn attachment_chunk(
    id: &str,
    mime: &str,
    size: &str,
    lines: usize,
    window: &str,
    chunk: &str,
) -> String {
    format!("{id} {mime} {size}, {lines} lines\n{window}\n\n{chunk}")
}

/// Which lines of a text attachment a chunk holds.
pub fn attachment_window(first: usize, last: usize, lines: usize) -> String {
    format!("lines {first}-{last} of {lines}")
}

/// After a line of an attachment cut at its length.
pub const TRUNCATED: &str = "… [truncated]";

/// A detached subagent call, answered at once.
pub const DETACHED_STARTED: &str = "working detached; the result arrives as a later message";

/// In place of content of `kind`, such as "image" or "resource", that the
/// model cannot be given.
pub fn media(kind: &str) -> String {
    format!("[{kind} content]")
}

/// In place of content of `kind` that could not be kept, `why`, such as
/// "unreadable".
pub fn media_not_kept(why: &str, kind: &str) -> String {
    format!("[{why} {kind} content]")
}

/// In place of content of a kind a connection did not say.
pub const CONTENT: &str = "[content]";

/// A link the model reads as text.
pub fn link(name: Option<&str>, uri: &str) -> String {
    match name {
        Some(name) => format!("{name}: {uri}"),
        None => uri.to_string(),
    }
}

/// A tool result with media, for a provider that takes media only from the
/// user: the result says what it holds, and the media follows.
pub fn attachments_follow(text: &str) -> String {
    match text.is_empty() {
        true => "This tool answered with attachments. They follow in the next message.".to_string(),
        false => format!("{text}\n\nAttachments follow in the next message."),
    }
}

/// Before the media a tool answered with, for a provider that takes media
/// only from the user. `from` is [`tool_named`].
pub fn attachment_from(from: &str) -> String {
    format!("Attachment from {from}:")
}

/// The tool a result came from, by what is known of it.
pub fn tool_named(name: Option<&str>, id: Option<&str>) -> String {
    match (name, id) {
        (Some(name), Some(id)) => format!("the `{name}` tool ({id})"),
        (Some(name), None) => format!("the `{name}` tool"),
        (None, Some(id)) => format!("tool call {id}"),
        (None, None) => "the tool result above".to_string(),
    }
}
