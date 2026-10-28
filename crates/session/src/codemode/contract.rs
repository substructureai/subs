use serde::{Deserialize, Serialize};

/// The contract's version.
pub const VERSION: u32 = 1;
/// The oldest version a runner still takes, so engines and runners can be
/// deployed one before the other.
pub const OLDEST: u32 = 1;

pub fn supports(version: u32) -> bool {
    (OLDEST..=VERSION).contains(&version)
}

/// What a script may use. The engine sets them, and a runner holds the script
/// to them.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Limits {
    /// Calls over the whole script.
    pub calls: u32,
    /// Bytes of results the script may read, over all its calls.
    pub results: usize,
    /// Bytes of arguments over the whole script.
    pub arguments: usize,
    /// Bytes of the value it returns.
    pub returned: usize,
    /// Bytes of `console.log` output.
    pub logs: usize,
    /// Bytes of JavaScript heap in one run.
    pub memory: usize,
    /// Milliseconds of CPU in one run.
    pub compute_ms: u64,
}

impl Default for Limits {
    fn default() -> Self {
        Limits {
            calls: 1_000,
            results: 16 << 20,
            arguments: 32 << 20,
            returned: 64 << 10,
            logs: 16 << 10,
            memory: 128 << 20,
            compute_ms: 20_000,
        }
    }
}

pub fn spent(results: usize) -> String {
    format!(
        "The script's results passed {} MiB. Ask each call for less, or split the work over \
         several run_code calls.",
        results >> 20
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
    #[serde(default)]
    pub limits: Limits,
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

/// A step, and what the run took to reach it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Ran {
    pub step: Step,
    #[serde(default)]
    pub stats: Stats,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Stats {
    /// JavaScript heap at the end of the run.
    pub heap: usize,
    pub cpu_ms: u64,
    /// The limit the run met, if one stopped it.
    pub limit: Option<Limit>,
    /// The process was started before the step reached it.
    pub warm: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Limit {
    Calls,
    Arguments,
    Results,
    Returned,
    Logs,
    Compute,
    Memory,
    Stack,
    Diverged,
    NeverSettles,
    Deadline,
    Wait,
    Start,
}
