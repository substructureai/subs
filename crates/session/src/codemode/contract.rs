use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// The language a script is written in. Each has a dialect that describes
/// `run_code` to the model and an engine that runs it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
#[schemars(rename = "CodeLanguage")]
pub enum Language {
    #[default]
    Javascript,
}

/// The contract's version.
pub const VERSION: u32 = 2;
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
    /// Bytes of results the script may read, over all its calls. Parsed JSON
    /// takes about seven times its size in heap, so keep it under `memory / 8`.
    pub results: usize,
    /// Bytes of arguments over the whole script.
    pub arguments: usize,
    /// Bytes of the value it returns.
    pub returned: usize,
    /// Bytes of output the script prints.
    pub logs: usize,
    /// Bytes of heap in one run.
    pub memory: usize,
    /// Milliseconds of CPU in one run.
    pub compute_ms: u64,
    /// Milliseconds of CPU over all of a script's runs.
    pub total_compute_ms: u64,
    /// Bytes of files one run may read.
    pub file_bytes: u64,
    /// Bytes of files over all of a script's runs. Each run reads its files
    /// again, so this pays for every pass.
    pub total_file_bytes: u64,
    /// Files open at once.
    pub streams: u32,
    /// Bytes of file text a script shows the model.
    pub shown: usize,
    /// Bytes of media files a script shows the model. Bytes stand in for the
    /// tokens they cost: a PDF's pages and audio's length grow with them.
    pub shown_media: u64,
}

impl Default for Limits {
    fn default() -> Self {
        Limits {
            calls: 1_000,
            results: 8 << 20,
            arguments: 32 << 20,
            returned: 64 << 10,
            logs: 16 << 10,
            memory: 96 << 20,
            compute_ms: 500,
            total_compute_ms: 10_000,
            file_bytes: 64 << 20,
            total_file_bytes: 512 << 20,
            streams: 4,
            shown: 64 << 10,
            shown_media: 2 << 20,
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

/// A file a script may read, by the id the model sees in its
/// `[attachment …]` line.
///
/// A script reads files without recording them: a blob never changes, so
/// every run reads the same bytes. Anything that makes a blob is a call.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScriptFile {
    pub id: String,
    pub mime: String,
    pub size: u64,
    /// The skill whose file this is, at `<skill>/<path>`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skill: Option<String>,
}

/// A script to run past the calls it already made.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Script {
    #[serde(default = "version")]
    pub version: u32,
    #[serde(default)]
    pub language: Language,
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
    /// CPU the script's earlier runs used.
    #[serde(default)]
    pub cpu_used_ms: u64,
    /// The files it may read.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub files: Vec<ScriptFile>,
    /// Bytes of files the script's earlier runs read.
    #[serde(default)]
    pub file_bytes_used: u64,
}

impl Script {
    /// The CPU this run may use: the per-run limit, or what is left of the
    /// script's whole budget.
    pub fn compute_ms(&self) -> u64 {
        let left = self
            .limits
            .total_compute_ms
            .saturating_sub(self.cpu_used_ms);
        self.limits.compute_ms.min(left)
    }

    /// The bytes of files this run may read.
    pub fn file_bytes(&self) -> u64 {
        let left = self
            .limits
            .total_file_bytes
            .saturating_sub(self.file_bytes_used);
        self.limits.file_bytes.min(left)
    }
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
    /// What a script that returned put in front of the model, in order.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub shown: Vec<Shown>,
}

/// Something a script shows the model, apart from what it returns.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Shown {
    /// A text file's text.
    Text { file: String, text: String },
    /// A file the model reads as media, by its id.
    File { file: String },
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Stats {
    /// The most heap the run held at once.
    pub heap: usize,
    pub cpu_ms: u64,
    /// Bytes of files the run read.
    pub file_bytes: u64,
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
    Files,
}
