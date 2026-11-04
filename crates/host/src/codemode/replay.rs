//! What every engine shares: a script's calls answered from its records, the
//! limits, and how a run's end becomes a step. An engine only interprets the
//! language, and asks this for each call the script makes.

use std::collections::VecDeque;

use super::{spent, Limit, Limits, Ran, Script, ScriptCall, ScriptRecord, Stats, Step};
use substructure_session::prompts::codemode as prompts;

/// The answer to a call the script made.
#[derive(Debug, Clone, PartialEq)]
pub enum Answer {
    /// The call ran before; hand the script its result, or raise its error.
    Result { ok: bool, output: String },
    /// The call has not run yet. Leave the script waiting on it.
    Pending,
    /// The call may not be made. Raise this in the script.
    Refused(String),
}

/// How the script's run came to an end.
#[derive(Debug, Clone, PartialEq)]
pub enum End {
    /// It returned this value, as JSON, or nothing.
    Returned(Option<String>),
    /// It raised this error. `limit` when the engine knows the error was a
    /// limit, such as memory or stack.
    Raised {
        message: String,
        limit: Option<Limit>,
    },
    /// It stopped with nothing left to run: waiting on calls, or on nothing.
    Waiting,
}

/// One run of a script, answered from its records.
pub struct Replay {
    limits: Limits,
    output: usize,
    records: Vec<ScriptRecord>,
    spent: bool,
    /// The run may use less than a run's limit, because the script's whole
    /// budget is nearly spent.
    whole: bool,
    seq: usize,
    calls: Vec<ScriptCall>,
    argument_bytes: usize,
    diverged: Option<String>,
    printed: Printed,
    end: Option<End>,
    stopped: Option<Limit>,
    refused: Option<Limit>,
}

impl Replay {
    pub fn new(script: &Script) -> Self {
        Replay {
            limits: script.limits.clone(),
            output: script.output,
            records: script.calls.clone(),
            spent: script.spent,
            whole: script.compute_ms() < script.limits.compute_ms,
            seq: 0,
            calls: Vec::new(),
            argument_bytes: 0,
            diverged: None,
            printed: Printed::new(script.output),
            end: None,
            stopped: None,
            refused: None,
        }
    }

    /// The script called `name` with `arguments`, a JSON object.
    /// A refused call takes no place in the sequence, so the calls after it
    /// still find their records.
    pub fn call(&mut self, name: String, arguments: String) -> Answer {
        let total = self.argument_bytes + arguments.len();
        let refused = if self.seq >= self.limits.calls as usize {
            Some((Limit::Calls, prompts::too_many_calls(self.limits.calls)))
        } else if arguments.len() > self.limits.call_arguments {
            Some((
                Limit::Arguments,
                prompts::call_arguments(&name, arguments.len(), self.limits.call_arguments),
            ))
        } else if total > self.limits.arguments {
            Some((
                Limit::Arguments,
                prompts::arguments_spent(total, self.limits.arguments),
            ))
        } else {
            None
        };
        if let Some((limit, message)) = refused {
            self.refused.get_or_insert(limit);
            return Answer::Refused(message);
        }
        self.seq += 1;
        self.argument_bytes = total;
        let Some(record) = self.records.get(self.seq - 1) else {
            if self.spent {
                self.refused.get_or_insert(Limit::Results);
                return Answer::Refused(spent(self.limits.results));
            }
            self.calls.push(ScriptCall { name, arguments });
            return Answer::Pending;
        };
        if record.name != name || record.arguments != arguments {
            self.diverged
                .get_or_insert(prompts::diverged(self.seq, &record.name, &name));
            return Answer::Pending;
        }
        Answer::Result {
            ok: record.ok,
            output: record.output.clone(),
        }
    }

    /// The script printed `line`.
    pub fn log(&mut self, line: String) {
        self.printed.push(line);
    }

    /// The engine stopped the run at `limit`.
    pub fn stop(&mut self, limit: Limit) {
        self.stopped.get_or_insert(limit);
    }

    /// How the script ended. The first end counts.
    pub fn end(&mut self, end: End) {
        self.end.get_or_insert(end);
    }

    pub fn returned(&self) -> bool {
        matches!(self.end, Some(End::Returned(_)))
    }

    pub fn waits_on_calls(&self) -> bool {
        !self.calls.is_empty()
    }

    pub fn limits(&self) -> &Limits {
        &self.limits
    }

    /// Where the run ended, and the limit that ended it, if one did.
    pub fn finish(self) -> (Step, Option<Limit>) {
        if let Some(limit) = self.stopped {
            let message = match limit {
                Limit::Compute if self.whole => total_spent(&self.limits),
                Limit::Compute => prompts::run_compute(self.limits.compute_ms),
                _ => memory(&self.limits),
            };
            return (Step::Failed(message), Some(limit));
        }
        if let Some(diverged) = &self.diverged {
            return (Step::Failed(diverged.clone()), Some(Limit::Diverged));
        }
        let output = self.output;
        let printed = self.printed.text();
        let dropped = self.printed.dropped;
        match self.end.clone().unwrap_or(End::Waiting) {
            End::Returned(value) => {
                let (text, cut) = finished(&printed, value.as_deref(), (output, dropped));
                (
                    Step::Done(text),
                    cut.then_some(Limit::Output).or(self.refused),
                )
            }
            End::Raised { message, limit } => {
                let message = match limit {
                    Some(Limit::Memory) => memory(&self.limits),
                    _ => message,
                };
                (
                    Step::Failed(failed(&message, &printed, (output, dropped))),
                    limit.or(self.refused),
                )
            }
            End::Waiting if !self.calls.is_empty() => {
                let asked: usize = self.calls.iter().map(|c| c.arguments.len()).sum();
                if asked > self.limits.step_arguments {
                    let message = prompts::step_arguments(asked, self.limits.step_arguments);
                    return (
                        Step::Failed(failed(&message, &printed, (output, dropped))),
                        Some(Limit::Arguments),
                    );
                }
                (Step::Calls(self.calls), self.refused)
            }
            End::Waiting => (
                Step::Failed(failed(prompts::NEVER_SETTLES, &printed, (output, dropped))),
                Some(Limit::NeverSettles),
            ),
        }
    }
}

/// What a script prints, kept at its start and its end when it is long.
struct Printed {
    head: Vec<String>,
    tail: VecDeque<String>,
    bytes: (usize, usize),
    keep: usize,
    /// Bytes dropped between the head and the tail.
    dropped: usize,
}

impl Printed {
    fn new(keep: usize) -> Self {
        Printed {
            head: Vec::new(),
            tail: VecDeque::new(),
            bytes: (0, 0),
            keep,
            dropped: 0,
        }
    }

    fn push(&mut self, line: String) {
        if self.bytes.0 < self.keep {
            self.bytes.0 += line.len() + 1;
            self.head.push(line);
            return;
        }
        self.bytes.1 += line.len() + 1;
        self.tail.push_back(line);
        while self.bytes.1 > self.keep && self.tail.len() > 1 {
            let dropped = self.tail.pop_front().unwrap_or_default();
            self.bytes.1 -= dropped.len() + 1;
            self.dropped += dropped.len() + 1;
        }
    }

    fn text(&self) -> String {
        self.head
            .iter()
            .chain(&self.tail)
            .cloned()
            .collect::<Vec<_>>()
            .join("\n")
    }
}

/// Refuses a run before it starts: a script of a version this runner does not
/// take, or one with no compute left.
pub fn precheck(script: &Script) -> Option<Ran> {
    let (step, limit) = match () {
        _ if !super::supports(script.version) => (
            Step::Failed(prompts::version(
                super::OLDEST,
                super::VERSION,
                script.version,
            )),
            None,
        ),
        _ if script.compute_ms() == 0 => (
            Step::Failed(total_spent(&script.limits)),
            Some(Limit::Compute),
        ),
        _ => return None,
    };
    Some(Ran {
        step,
        stats: Stats {
            limit,
            ..Stats::default()
        },
        shown: Vec::new(),
    })
}

fn total_spent(limits: &Limits) -> String {
    prompts::total_compute(limits.total_compute_ms / 1000)
}

fn memory(limits: &Limits) -> String {
    prompts::memory(limits.memory >> 20)
}

/// A finished run's text: what it printed, then the value it returned, a
/// string as itself and any other value as JSON. Says whether it was cut.
/// `budget` is the output limit, and the bytes of printed lines already
/// dropped from the middle.
fn finished(printed: &str, value: Option<&str>, budget: (usize, usize)) -> (String, bool) {
    let value = value.map(|v| match serde_json::from_str::<serde_json::Value>(v) {
        Ok(serde_json::Value::String(s)) => s,
        _ => v.to_string(),
    });
    let text = prompts::finished(printed, value.as_deref());
    cut(text, budget)
}

fn failed(error: &str, printed: &str, budget: (usize, usize)) -> String {
    cut(prompts::failed(error, printed), budget).0
}

/// `text` within `output` bytes, its middle cut out with a note that counts
/// it and the `dropped` bytes already cut.
fn cut(text: String, (output, dropped): (usize, usize)) -> (String, bool) {
    if text.len() <= output && dropped == 0 {
        return (text, false);
    }
    let (head, tail) = match text.len() > output {
        true => (
            floor(&text, output / 2),
            ceil(&text, text.len() - output / 2),
        ),
        false => {
            let middle = floor(&text, text.len() / 2);
            (middle, middle)
        }
    };
    let note = prompts::cut((tail - head + dropped).div_ceil(1024));
    (format!("{}{note}{}", &text[..head], &text[tail..]), true)
}

fn floor(text: &str, at: usize) -> usize {
    (0..=at)
        .rev()
        .find(|&i| text.is_char_boundary(i))
        .unwrap_or(0)
}

fn ceil(text: &str, at: usize) -> usize {
    (at..=text.len())
        .find(|&i| text.is_char_boundary(i))
        .unwrap_or(text.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// These drive a replay the way any engine would, with no interpreter.
    fn script(calls: Vec<ScriptRecord>) -> Script {
        Script {
            version: super::super::VERSION,
            language: Default::default(),
            code: String::new(),
            calls,
            seed: 0,
            now_ms: 0,
            spent: false,
            limits: Limits::default(),
            output: crate::codemode::OUTPUT,
            cpu_used_ms: 0,
            files: vec![],
            file_bytes_used: 0,
        }
    }

    fn record(name: &str, ok: bool, output: &str) -> ScriptRecord {
        ScriptRecord {
            name: name.to_string(),
            arguments: "{}".to_string(),
            ok,
            output: output.to_string(),
        }
    }

    #[test]
    fn recorded_calls_are_answered_and_the_rest_wait() {
        let mut replay = Replay::new(&script(vec![record("a", true, "1")]));
        assert_eq!(
            replay.call("a".into(), "{}".into()),
            Answer::Result {
                ok: true,
                output: "1".into()
            }
        );
        assert_eq!(replay.call("b".into(), "{}".into()), Answer::Pending);
        replay.end(End::Waiting);
        let (step, limit) = replay.finish();
        assert_eq!(
            step,
            Step::Calls(vec![ScriptCall {
                name: "b".into(),
                arguments: "{}".into()
            }])
        );
        assert_eq!(limit, None);
    }

    #[test]
    fn a_different_call_on_replay_fails_the_run() {
        let mut replay = Replay::new(&script(vec![record("a", true, "1")]));
        replay.call("b".into(), "{}".into());
        replay.end(End::Waiting);
        let (step, limit) = replay.finish();
        assert!(matches!(step, Step::Failed(m) if m.contains("changed between runs")));
        assert_eq!(limit, Some(Limit::Diverged));
    }

    #[test]
    fn a_returned_value_comes_back_with_what_was_printed() {
        let mut replay = Replay::new(&script(vec![]));
        replay.log("hi".into());
        replay.end(End::Returned(Some("[1,2]".into())));
        assert_eq!(
            replay.finish().0,
            Step::Done("Script finished.\nPrinted:\nhi\nReturned:\n[1,2]".into())
        );
        let mut replay = Replay::new(&script(vec![]));
        replay.end(End::Returned(Some(r#""a \"b\"""#.into())));
        assert_eq!(
            replay.finish().0,
            Step::Done("Script finished.\nReturned:\na \"b\"".into())
        );
    }

    #[test]
    fn long_output_keeps_its_start_and_its_end() {
        let mut replay = Replay::new(&script(vec![]));
        let output = crate::codemode::OUTPUT;
        for i in 0..10_000 {
            replay.log(format!("line {i}"));
        }
        replay.end(End::Returned(Some("\"the answer\"".into())));
        let (step, limit) = replay.finish();
        let Step::Done(text) = step else {
            panic!("done");
        };
        assert!(text.len() < output + 200);
        assert!(text.starts_with("Script finished.\nPrinted:\nline 0\n"));
        let printed: usize = (0..10_000).map(|i| format!("line {i}").len() + 1).sum();
        let note = text.split("[… ").nth(1).expect("a cut note");
        let cut: usize = note.split(' ').next().unwrap().parse().unwrap();
        assert!(
            cut >= (printed - output) >> 10,
            "the note counts every byte cut: {cut} KiB of {printed}"
        );
        assert!(text.ends_with("line 9999\nReturned:\nthe answer"));
        assert_eq!(limit, Some(Limit::Output));
    }

    #[test]
    fn a_limit_the_engine_met_wins_over_how_the_script_ended() {
        let mut replay = Replay::new(&script(vec![]));
        replay.stop(Limit::Compute);
        replay.end(End::Returned(Some("1".into())));
        let (step, limit) = replay.finish();
        assert!(matches!(step, Step::Failed(m) if m.contains("compute for one run")));
        assert_eq!(limit, Some(Limit::Compute));
    }

    #[test]
    fn a_run_with_nothing_to_wait_on_never_finishes() {
        let mut replay = Replay::new(&script(vec![]));
        replay.end(End::Waiting);
        assert_eq!(replay.finish().1, Some(Limit::NeverSettles));
    }

    #[test]
    fn a_script_of_another_version_or_no_compute_does_not_run() {
        let mut old = script(vec![]);
        old.version = super::super::VERSION + 1;
        assert!(precheck(&old).is_some());
        let mut spent = script(vec![]);
        spent.cpu_used_ms = spent.limits.total_compute_ms;
        assert_eq!(
            precheck(&spent).map(|r| r.stats.limit),
            Some(Some(Limit::Compute))
        );
        assert!(precheck(&script(vec![])).is_none());
    }
}
