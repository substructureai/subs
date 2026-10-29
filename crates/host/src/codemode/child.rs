//! A runner process: it confines itself, says it is ready, runs one script
//! from stdin, and writes its step to stdout, each as one line of JSON.

use std::ffi::OsString;
use std::io::{self, BufRead, Read, Write};

use serde::{Deserialize, Serialize};

use super::confine::{self, Denial};
use super::engine::{languages, run};
use super::files::{FileError, Host};
use super::{Language, Ran, Script, Step, OLDEST, VERSION};

/// The argument that makes a binary a runner process.
pub const ARG: &str = "__sandbox";
pub const PROBE: &str = "probe";
pub const PROBE_LOCK: &str = "probe-lock";

/// Sets the process's heap cap: `data=<MiB>`.
pub const DATA: &str = "data=";
pub const DATA_MIB: u64 = 256;

/// The first line a runner process writes.
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Hello {
    Ready {
        version: u32,
        oldest: u32,
        languages: Vec<Language>,
    },
    Unconfined {
        error: String,
    },
}

/// What a runner process asks its runner during a run. Its last line is the
/// `Ran` instead.
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Request {
    /// Bytes of a file. The answer is a `Reply`, then for `Bytes(n)`, `n` raw
    /// bytes.
    Read { file: String, offset: u64, len: u64 },
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Reply {
    Bytes(u64),
    Missing(String),
    Refused(String),
}

/// Reads a script's files by asking the runner over the pipes.
struct PipeHost;

impl Host for PipeHost {
    fn read(&mut self, file: &str, offset: u64, len: u64) -> Result<Vec<u8>, FileError> {
        let gone = || FileError::Missing("The runner did not answer.".to_string());
        let request = Request::Read {
            file: file.to_string(),
            offset,
            len,
        };
        let mut out = io::stdout().lock();
        let line = serde_json::to_string(&request).map_err(|_| gone())?;
        writeln!(out, "{line}")
            .and_then(|_| out.flush())
            .map_err(|_| gone())?;
        let mut input = io::stdin().lock();
        let mut line = String::new();
        if input.read_line(&mut line).unwrap_or(0) == 0 {
            return Err(gone());
        }
        match serde_json::from_str(&line).map_err(|_| gone())? {
            Reply::Bytes(n) if n > len => Err(gone()),
            Reply::Bytes(n) => {
                let mut bytes = vec![0; n as usize];
                input.read_exact(&mut bytes).map_err(|_| gone())?;
                Ok(bytes)
            }
            Reply::Missing(m) => Err(FileError::Missing(m)),
            Reply::Refused(m) => Err(FileError::Limit(m)),
        }
    }
}

/// Call first in `main`, before any thread starts. Becomes a runner process
/// and never returns when the binary was started as one.
pub fn dispatch() {
    let mut args = std::env::args_os().skip(1);
    if args.next().as_deref() == Some(ARG.as_ref()) {
        main(args.collect());
    }
}

fn main(args: Vec<OsString>) -> ! {
    let args: Vec<&str> = args.iter().filter_map(|a| a.to_str()).collect();
    let data = args
        .iter()
        .find_map(|a| a.strip_prefix(DATA)?.parse::<u64>().ok())
        .unwrap_or(DATA_MIB)
        << 20;
    let mode = args
        .iter()
        .copied()
        .find(|a| [PROBE, PROBE_LOCK].contains(a));
    let confined = match mode {
        Some(PROBE_LOCK) => confine::lock_only(),
        Some(PROBE) => confine::confine(Denial::Refuse, data),
        _ => confine::confine(Denial::Kill, data),
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
        Ok(()) => Hello::Ready {
            version: VERSION,
            oldest: OLDEST,
            languages: languages(),
        },
        Err(e) => Hello::Unconfined {
            error: e.to_string(),
        },
    };
    let _ = writeln!(out, "{}", serde_json::to_string(&hello).unwrap_or_default());
    let _ = out.flush();
    if confined.is_err() {
        std::process::exit(70);
    }
    let mut line = String::new();
    if io::stdin().lock().read_line(&mut line).unwrap_or(0) == 0 {
        std::process::exit(0);
    }
    let ran = match serde_json::from_str::<Script>(&line) {
        Ok(script) => {
            let _ = confine::limit_cpu(script.compute_ms());
            run(&script, Box::new(PipeHost))
        }
        Err(e) => Ran {
            step: Step::Failed(format!("The runner could not read the script: {e}")),
            stats: Default::default(),
            shown: Vec::new(),
        },
    };
    let _ = writeln!(out, "{}", serde_json::to_string(&ran).unwrap_or_default());
    let _ = out.flush();
    std::process::exit(0)
}
