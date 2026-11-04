//! Every piece of text the model reads that the engine writes: tool
//! descriptions, what it adds to the prompt, refusals and errors, and how
//! answers are laid out. The code only refers to these.
//!
//! - Each item says when the model reads it.
//! - Text with parts that vary is a function; the rest are constants.
//! - Tool names come from their constants, so a renamed tool renames its text.
//! - Structure stays in the code: rendered TypeScript, and JSON key names.
//! - Text only people read lives with the code that shows it.

pub mod answers;
pub mod codemode;
pub mod context;
pub mod errors;
pub mod tools;

/// `ids` joined with commas, or "none". Lists what an agent or a config
/// declares, in errors for the model and for people alike.
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
