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
    Stats, Step, OLDEST, VERSION,
};

use substructure_session::command::{CommandPayload, Outcome, SettleError};
use substructure_session::protocol::{ErrorCode, ErrorInfo, StoredResult};
use substructure_session::state::EffectKind;

pub fn settle(tool_call_id: String, attempt: u32, known: u32, ran: Ran) -> CommandPayload {
    let outcome = match ran.step {
        Step::Calls(calls) => Outcome::Script {
            known,
            calls,
            cpu_ms: ran.stats.cpu_ms,
            file_bytes: ran.stats.file_bytes,
        },
        Step::Done(text) => Outcome::Tool {
            result: StoredResult::text(text),
        },
        Step::Failed(message) => {
            SettleError::new(ErrorInfo::new(ErrorCode::HandlerError, message), false).into()
        }
    };
    CommandPayload::settle(EffectKind::ToolCall, tool_call_id, Some(attempt), outcome)
}
