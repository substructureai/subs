use serde::{Deserialize, Serialize};

/// The contract's version.
pub const VERSION: u32 = 1;
/// The oldest version a runner still takes, so engines and runners can be
/// deployed one before the other.
pub const OLDEST: u32 = 1;

pub fn supports(version: u32) -> bool {
    (OLDEST..=VERSION).contains(&version)
}

/// The result bytes one script may read, over all its calls.
pub const RESULTS: usize = 8 << 20;

pub fn spent() -> String {
    format!(
        "The script's results passed {} MiB. Make fewer calls, or ask for less in each.",
        RESULTS >> 20
    )
}

fn version() -> u32 {
    VERSION
}

/// A script to run past the calls it already made.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Script {
    #[serde(default = "version")]
    pub version: u32,
    pub code: String,
    pub calls: Vec<ScriptRecord>,
    pub seed: u64,
    pub now_ms: i64,
    /// The names `tools` lists. A script can still call any name.
    #[serde(default)]
    pub tools: Vec<String>,
    /// The script's results reached their budget: it may make no new call.
    #[serde(default)]
    pub spent: bool,
}

/// A call a script made, as the script made it, and how it ended.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ScriptRecord {
    pub name: String,
    pub arguments: String,
    pub ok: bool,
    pub output: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScriptCall {
    pub name: String,
    pub arguments: String,
}

/// Where a run ended. The same script always ends in the same step.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Step {
    /// The script needs these calls before it can go on.
    Calls(Vec<ScriptCall>),
    /// The script returned. The text goes back to the model.
    Done(String),
    Failed(String),
}
