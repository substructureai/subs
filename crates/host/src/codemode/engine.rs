//! The engines a runner process can run. Each interprets one language and
//! answers its script's calls through [`super::replay::Replay`].

use super::quickjs::QuickJs;
use super::replay::precheck;
use super::{Language, Ran, Script, Step};

pub trait Engine: Sync {
    fn language(&self) -> Language;

    /// One run of `script` from its start: answer each call from its records,
    /// and stop at the first call with none. The same script must always
    /// end in the same step.
    fn run(&self, script: &Script) -> Ran;
}

static ENGINES: &[&dyn Engine] = &[&QuickJs];

/// The languages this binary can run.
pub fn languages() -> Vec<Language> {
    ENGINES.iter().map(|e| e.language()).collect()
}

pub fn run(script: &Script) -> Ran {
    if let Some(ran) = precheck(script) {
        return ran;
    }
    match ENGINES.iter().find(|e| e.language() == script.language) {
        Some(engine) => engine.run(script),
        None => Ran {
            step: Step::Failed(format!(
                "This runner has no engine for {:?}.",
                script.language
            )),
            stats: Default::default(),
        },
    }
}

/// CPU this thread has used.
pub fn cpu_ms() -> u64 {
    let mut now = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    unsafe { libc::clock_gettime(libc::CLOCK_THREAD_CPUTIME_ID, &mut now) };
    now.tv_sec as u64 * 1000 + now.tv_nsec as u64 / 1_000_000
}
