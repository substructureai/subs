mod admission;
mod child;
mod confine;
pub mod engine;
pub mod files;
mod process;
mod quickjs;
pub mod replay;

pub use admission::{Admission, Ticket};
pub use child::{dispatch, ARG, PROBE, PROBE_LOCK};
pub use process::{ProcessRunner, ReadError, RunError, ScriptFiles, ScriptRunner};
pub use substructure_session::codemode::{
    spent, supports, Language, Limit, Limits, Ran, Script, ScriptCall, ScriptFile, ScriptRecord,
    Shown, Stats, Step, OLDEST, VERSION,
};

use std::collections::BTreeMap;
use substructure_session::command::{CommandPayload, Outcome, SettleError};

use substructure_session::blob::BlobRef;
use substructure_session::protocol::{ErrorCode, ErrorInfo, StoredContent, StoredResult};
use substructure_session::state::EffectKind;

/// The command that records a step. A script that returned answers with its
/// text, then each file it showed, found in `files` by id.
pub fn settle(
    tool_call_id: String,
    attempt: u32,
    known: u32,
    ran: Ran,
    files: &BTreeMap<String, BlobRef>,
) -> CommandPayload {
    let outcome = match ran.step {
        Step::Calls(calls) => Outcome::Script {
            known,
            calls,
            cpu_ms: ran.stats.cpu_ms,
            file_bytes: ran.stats.file_bytes,
        },
        Step::Done(text) => {
            let mut result = StoredResult::text(text);
            for shown in ran.shown {
                result.content.extend(match shown {
                    Shown::Text { file, text } => vec![StoredContent::Text {
                        text: format!("[file {file}]\n{text}"),
                    }],
                    Shown::File { file } => match files.get(&file) {
                        Some(blob) => vec![
                            StoredContent::Text {
                                text: format!("[file {file}]"),
                            },
                            StoredContent::Blob { uri: blob.uri() },
                        ],
                        None => Vec::new(),
                    },
                });
            }
            Outcome::Tool { result }
        }
        Step::Failed(message) => {
            SettleError::new(ErrorInfo::new(ErrorCode::HandlerError, message), false).into()
        }
    };
    CommandPayload::settle(EffectKind::ToolCall, tool_call_id, Some(attempt), outcome)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_result_holds_its_text_then_what_the_script_showed() {
        let blob = BlobRef {
            tenant_id: "t1".into(),
            id: "01a18700-0000-7000-8000-000000000000".into(),
            mime: "image/png".into(),
            name: Some("shot.png".into()),
            size: 3,
        };
        let ran = Ran {
            step: Step::Done(r#"{"result":1}"#.into()),
            stats: Stats::default(),
            shown: vec![
                Shown::Text {
                    file: "notes.txt".into(),
                    text: "hello".into(),
                },
                Shown::File {
                    file: "shot.png".into(),
                },
                Shown::File {
                    file: "elsewhere.png".into(),
                },
            ],
        };
        let files = BTreeMap::from([("shot.png".to_string(), blob.clone())]);
        let CommandPayload::SettleEffect { outcome, .. } = settle("tc-1".into(), 0, 0, ran, &files)
        else {
            panic!("a settle");
        };
        let Outcome::Tool { result } = outcome else {
            panic!("a result");
        };
        assert_eq!(
            result.content,
            [
                StoredContent::Text {
                    text: r#"{"result":1}"#.into()
                },
                StoredContent::Text {
                    text: "[file notes.txt]\nhello".into()
                },
                StoredContent::Text {
                    text: "[file shot.png]".into()
                },
                StoredContent::Blob { uri: blob.uri() },
            ],
            "a file not on the branch is not shown"
        );
    }
}
