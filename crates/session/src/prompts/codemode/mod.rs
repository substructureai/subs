//! Code mode's text in any language: limits, how a run ends, and file errors.

use crate::protocol::RUN_CODE;

pub mod files;
pub mod javascript;

/// A script called `run_code`.
pub fn calls_itself(name: &str) -> String {
    format!("`{name}` cannot call itself.")
}

/// The model called `run_code` without `code`.
pub fn code_missing(name: &str) -> String {
    format!("`{name}` takes `code`, a string.")
}

/// A script's call that no decision answered.
pub fn not_run(name: &str) -> String {
    format!("The worker did not run `{name}`.")
}

/// A script's call to a subagent that the worker did not start.
pub fn subagent_not_started(name: &str) -> String {
    format!("The worker did not start the subagent of `{name}`.")
}

/// How many matches a script's search found, of which it shows `shown`.
pub fn matches(shown: usize, matched: usize) -> String {
    match matched {
        n if n > shown => format!("{shown} of {n} matches. Narrow the query to see the rest."),
        1 => "1 match.".to_string(),
        n => format!("{n} matches."),
    }
}

/// A skill a script loaded: where its files are, then its instructions.
pub fn skill_loaded(named: &str, body: &str) -> String {
    format!("Base directory for this skill: {named}/\n\n{body}")
}

/// After a skill a script loaded: the tools it names, declared in `block`.
pub fn skill_tools(block: &str) -> String {
    format!("\nTools this skill uses. You can call them in your next script:\n{block}")
}

/// The script's results passed their budget, `results` bytes.
pub fn spent(results: usize) -> String {
    format!(
        "The script's results passed {} MiB. Ask each call for less, or split the work over \
         several {RUN_CODE} calls.",
        results >> 20
    )
}

/// A script made more calls than its limit, `calls`.
pub fn too_many_calls(calls: u32) -> String {
    format!(
        "A script can make at most {calls} calls. Make fewer, larger calls, or split the work \
         over several {RUN_CODE} calls."
    )
}

/// A script passed `name` arguments of `bytes`, more than the `cap` one call
/// takes.
pub fn call_arguments(name: &str, bytes: usize, cap: usize) -> String {
    format!(
        "Arguments to `{name}` are {}, over the {} one call takes. Pass less in one call.",
        size(bytes),
        size(cap)
    )
}

/// A script's arguments came to `bytes` over all its calls, more than the
/// `cap` a script takes.
pub fn arguments_spent(bytes: usize, cap: usize) -> String {
    format!(
        "The script's arguments came to {} in all, over the {} a script takes. Pass less in \
         each call.",
        size(bytes),
        size(cap)
    )
}

/// The calls one step asked for came to `bytes` of arguments, more than the
/// `cap` one step takes. The step fails.
pub fn step_arguments(bytes: usize, cap: usize) -> String {
    format!(
        "The calls this step asked for came to {} together, over the {} one step takes. The \
         calls before them ran; these did not. Send fewer at once.",
        size(bytes),
        size(cap)
    )
}

/// Bytes in the largest unit they fill: "7.3 MiB", "4 MiB", "512 KiB".
fn size(bytes: usize) -> String {
    let (value, unit) = match bytes {
        b if b >= 1 << 20 => (b as f64 / (1 << 20) as f64, "MiB"),
        b if b >= 1 << 10 => (b as f64 / (1 << 10) as f64, "KiB"),
        b => return format!("{b} bytes"),
    };
    let text = format!("{value:.1}");
    format!("{} {unit}", text.strip_suffix(".0").unwrap_or(&text))
}

/// A script made call `seq` differently from its earlier runs.
pub fn diverged(seq: usize, was: &str, now: &str) -> String {
    format!(
        "The script changed between runs. Call {seq} was `{was}` and is now `{now}`. A script \
         must make the same calls each time it runs: do not branch on timing or on anything \
         outside its results."
    )
}

/// A run used its compute, `ms`.
pub fn run_compute(ms: u64) -> String {
    format!(
        "The script used its {ms} ms of compute for one run. Do less before each call: the \
         script resumes from the start after its calls finish, so that work is done again."
    )
}

/// A script used its compute over all its runs, `secs`.
pub fn total_compute(secs: u64) -> String {
    format!(
        "The script used its {secs}s of compute over all its runs. Make fewer calls one after \
         another, or do less before each: the script resumes from the start after its calls \
         finish."
    )
}

/// A run ran out of memory, `mib`.
pub fn memory(mib: usize) -> String {
    format!(
        "The script ran out of its {mib} MiB of memory. Keep less: take only the fields you \
         need from each result."
    )
}

/// A script waits, with no call to wait on.
pub const NEVER_SETTLES: &str =
    "The script waits on something that never finishes. Wait only on tool calls.";

/// A runner got a script of a version it does not take.
pub fn version(oldest: u32, newest: u32, got: u32) -> String {
    format!("The runner takes versions {oldest} to {newest}, not {got}.")
}

/// `run_code`'s result for a script that finished: what it printed, then
/// the value it returned.
pub fn finished(printed: &str, returned: Option<&str>) -> String {
    let mut text = "Script finished.".to_string();
    if !printed.is_empty() {
        text.push_str(&format!("\nPrinted:\n{printed}"));
    }
    match returned {
        Some(value) => text.push_str(&format!("\nReturned:\n{value}")),
        None if printed.is_empty() => text.push_str(" It printed and returned nothing."),
        None => {}
    }
    text
}

/// `run_code`'s error for a script that failed with `error`, then what it
/// printed.
pub fn failed(error: &str, printed: &str) -> String {
    match printed.is_empty() {
        true => format!("Script failed.\n{}", error.trim_end()),
        false => format!("Script failed.\n{}\nPrinted:\n{printed}", error.trim_end()),
    }
}

/// In place of the middle of a run's output past its limit, `kib` long.
pub fn cut(kib: usize) -> String {
    format!(
        "\n[… {kib} KiB cut from the middle. Print and return less: filter or count in the \
         script. …]\n"
    )
}

/// Before the text of a file a script showed.
pub fn shown_file(id: &str) -> String {
    format!("[file {id}]")
}

/// A step waited past its wait for a free sandbox, `secs`.
pub fn no_free_sandbox(secs: u64) -> String {
    format!("The script waited {secs}s for a free sandbox and did not run. Try again in a moment.")
}

/// No runner took a step within its wait, `secs`.
pub fn no_runner(secs: u64) -> String {
    format!("No sandbox could run the script for {secs}s. Try again in a moment.")
}

/// The sandbox process could not start, for `reason`.
pub fn sandbox_failed(reason: &str) -> String {
    format!("The sandbox could not start: {reason}")
}

/// Why a sandbox could not start: the program did not answer as a runner.
pub fn not_a_runner(program: &str) -> String {
    format!(
        "`{program}` did not answer as a runner. Its `main` must call `codemode::dispatch()` \
         first"
    )
}

/// Why a sandbox could not start: it could not confine itself.
pub fn unconfined(error: &str) -> String {
    format!("it could not confine itself: {error}")
}

/// The sandbox stopped a run, for `reason`.
pub fn stopped(reason: &str) -> String {
    format!(
        "The script stopped: {reason}. It may have run long inside one built-in operation, or \
         run out of memory."
    )
}

/// Why a sandbox stopped a run: it passed its deadline.
pub const RAN_TOO_LONG: &str = "the script ran too long";

/// Why a sandbox stopped a run: its answer passed the size the runner reads.
pub const ANSWER_TOO_LARGE: &str = "its answer was too large";

/// Why a sandbox stopped a run: its process ended without an answer.
pub const EXITED: &str = "it exited";

/// A runner process stopped answering a run's reads.
pub const RUNNER_GONE: &str = "The runner did not answer.";

/// A runner process could not parse the script it was sent.
pub fn unreadable_script(error: &str) -> String {
    format!("The runner could not read the script: {error}")
}

/// A runner has no engine for the script's language.
pub fn no_engine(language: &str) -> String {
    format!("This runner has no engine for {language}.")
}

/// The engine could not set the script up to run.
pub fn could_not_run(error: &str) -> String {
    format!("The script could not run: {error}")
}

/// A script threw a value that is not an error, and not JSON.
pub const THREW: &str = "The script threw.";
