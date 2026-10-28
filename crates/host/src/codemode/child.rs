//! A runner process: it confines itself, says it is ready, runs one script
//! from stdin, and writes its step to stdout, each as one line of JSON.

use std::ffi::OsString;
use std::io::{self, BufRead, Write};

use super::confine::{self, Denial};
use super::run::run;
use super::{Script, Step, OLDEST, VERSION};

/// The argument that makes a binary a runner process.
pub const ARG: &str = "__sandbox";
pub const PROBE: &str = "probe";
pub const PROBE_LOCK: &str = "probe-lock";

/// Call first in `main`, before any thread starts. Becomes a runner process
/// and never returns when the binary was started as one.
pub fn dispatch() {
    let mut args = std::env::args_os().skip(1);
    if args.next().as_deref() == Some(ARG.as_ref()) {
        main(args.next());
    }
}

fn main(mode: Option<OsString>) -> ! {
    let mode = mode.as_deref().and_then(|m| m.to_str());
    let confined = match mode {
        Some(PROBE_LOCK) => confine::lock_only(),
        Some(PROBE) => confine::confine(Denial::Refuse),
        _ => confine::confine(Denial::Kill),
    };
    let mut out = io::stdout().lock();
    if matches!(mode, Some(PROBE | PROBE_LOCK)) {
        let report = match &confined {
            Ok(()) => confine::probe(),
            Err(e) => serde_json::json!({ "error": e.to_string() }),
        };
        let _ = writeln!(out, "{report}");
        std::process::exit(0);
    }
    let hello = match &confined {
        Ok(()) => serde_json::json!({ "ready": true, "version": VERSION, "oldest": OLDEST }),
        Err(e) => serde_json::json!({ "ready": false, "error": e.to_string() }),
    };
    let _ = writeln!(out, "{hello}");
    let _ = out.flush();
    if confined.is_err() {
        std::process::exit(70);
    }
    let mut line = String::new();
    if io::stdin().lock().read_line(&mut line).unwrap_or(0) == 0 {
        std::process::exit(0);
    }
    let step = match serde_json::from_str::<Script>(&line) {
        Ok(script) => run(&script),
        Err(e) => Step::Failed(format!("The runner could not read the script: {e}")),
    };
    let _ = writeln!(out, "{}", serde_json::to_string(&step).unwrap_or_default());
    let _ = out.flush();
    std::process::exit(0)
}
