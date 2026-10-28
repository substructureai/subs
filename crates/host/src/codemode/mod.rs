mod admission;
mod child;
mod confine;
mod process;
mod run;

pub use admission::{Admission, Ticket};
pub use child::{dispatch, ARG, PROBE, PROBE_LOCK};
pub use process::{ProcessRunner, RunError, ScriptRunner};
pub use substructure_session::codemode::{
    spent, supports, Script, ScriptCall, ScriptRecord, Step, OLDEST, RESULTS, VERSION,
};

use substructure_session::command::{CommandPayload, Outcome, SettleError};
use substructure_session::protocol::{ErrorCode, ErrorInfo, StoredResult};
use substructure_session::state::EffectKind;

pub fn settle(tool_call_id: String, attempt: u32, known: u32, step: Step) -> CommandPayload {
    let outcome = match step {
        Step::Calls(calls) => Outcome::Script { known, calls },
        Step::Done(text) => Outcome::Tool {
            result: StoredResult::text(text),
        },
        Step::Failed(message) => {
            SettleError::new(ErrorInfo::new(ErrorCode::HandlerError, message), false).into()
        }
    };
    CommandPayload::settle(EffectKind::ToolCall, tool_call_id, Some(attempt), outcome)
}
