//! What every engine shares: a script's calls answered from its records, the
//! limits, and how a run's end becomes a step. An engine only interprets the
//! language, and asks this for each call the script makes.

use super::{spent, Limit, Limits, Ran, Script, ScriptCall, ScriptRecord, Stats, Step};

const ARGUMENTS: usize = 256 << 10;
/// The start of an oversized result that is still shown.
const PREVIEW: usize = 4 << 10;

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
    records: Vec<ScriptRecord>,
    spent: bool,
    /// The run may use less than a run's limit, because the script's whole
    /// budget is nearly spent.
    whole: bool,
    seq: usize,
    calls: Vec<ScriptCall>,
    argument_bytes: usize,
    diverged: Option<String>,
    logs: Vec<String>,
    log_bytes: usize,
    logs_cut: bool,
    end: Option<End>,
    stopped: Option<Limit>,
    refused: Option<Limit>,
}

impl Replay {
    pub fn new(script: &Script) -> Self {
        Replay {
            limits: script.limits.clone(),
            records: script.calls.clone(),
            spent: script.spent,
            whole: script.compute_ms() < script.limits.compute_ms,
            seq: 0,
            calls: Vec::new(),
            argument_bytes: 0,
            diverged: None,
            logs: Vec::new(),
            log_bytes: 0,
            logs_cut: false,
            end: None,
            stopped: None,
            refused: None,
        }
    }

    /// The script called `name` with `arguments`, a JSON object.
    pub fn call(&mut self, name: String, arguments: String) -> Answer {
        self.seq += 1;
        self.argument_bytes += arguments.len();
        let mut refuse = |limit: Limit, message: String| {
            self.refused.get_or_insert(limit);
            Answer::Refused(message)
        };
        if self.seq > self.limits.calls as usize {
            return refuse(
                Limit::Calls,
                format!(
                    "A script can make at most {} calls. Make fewer, larger calls, or split the \
                     work over several run_code calls.",
                    self.limits.calls
                ),
            );
        }
        if arguments.len() > ARGUMENTS {
            return refuse(
                Limit::Arguments,
                format!(
                    "Arguments to `{name}` are over {} KiB. Pass less in one call.",
                    ARGUMENTS >> 10
                ),
            );
        }
        if self.argument_bytes > self.limits.arguments {
            return refuse(
                Limit::Arguments,
                format!(
                    "The script's arguments passed {} MiB in all. Pass less in each call.",
                    self.limits.arguments >> 20
                ),
            );
        }
        let Some(record) = self.records.get(self.seq - 1) else {
            if self.spent {
                let message = spent(self.limits.results);
                return refuse(Limit::Results, message);
            }
            self.calls.push(ScriptCall { name, arguments });
            return Answer::Pending;
        };
        if record.name != name || record.arguments != arguments {
            self.diverged.get_or_insert(format!(
                "The script changed between runs. Call {} was `{}` and is now `{name}`. A \
                 script must make the same calls each time it runs: do not branch on timing or \
                 on anything outside its results.",
                self.seq, record.name
            ));
            return Answer::Pending;
        }
        Answer::Result {
            ok: record.ok,
            output: record.output.clone(),
        }
    }

    /// The script printed `line`.
    pub fn log(&mut self, line: String) {
        if self.log_bytes + line.len() > self.limits.logs {
            self.logs_cut = true;
            return;
        }
        self.log_bytes += line.len();
        self.logs.push(line);
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

    fn logs(&self) -> Vec<String> {
        let mut logs = self.logs.clone();
        if self.logs_cut {
            logs.push(format!(
                "[output over {} KiB was cut; print less]",
                self.limits.logs >> 10
            ));
        }
        logs
    }

    /// Where the run ended, and the limit that ended it, if one did.
    pub fn finish(self) -> (Step, Option<Limit>) {
        if let Some(limit) = self.stopped {
            let message = match limit {
                Limit::Compute if self.whole => total_spent(&self.limits),
                Limit::Compute => format!(
                    "The script used its {} ms of compute for one run. Do less before each \
                     call: the script resumes from the start after its calls finish, so that \
                     work is done again.",
                    self.limits.compute_ms
                ),
                _ => memory(&self.limits),
            };
            return (Step::Failed(message), Some(limit));
        }
        if let Some(diverged) = &self.diverged {
            return (Step::Failed(diverged.clone()), Some(Limit::Diverged));
        }
        let logs = self.logs();
        let cut = self.logs_cut.then_some(Limit::Logs);
        match self.end.clone().unwrap_or(End::Waiting) {
            End::Returned(value) => {
                let (text, over) = rendered(value.as_deref(), &logs, self.limits.returned);
                (Step::Done(text), over.or(self.refused).or(cut))
            }
            End::Raised { message, limit } => {
                let message = match limit {
                    Some(Limit::Memory) => memory(&self.limits),
                    _ => message,
                };
                (
                    Step::Failed(with_logs(&message, &logs)),
                    limit.or(self.refused),
                )
            }
            End::Waiting if !self.calls.is_empty() => (Step::Calls(self.calls), self.refused),
            End::Waiting => (
                Step::Failed(with_logs(
                    "The script waits on something that never finishes. Wait only on tool \
                     calls.",
                    &logs,
                )),
                Some(Limit::NeverSettles),
            ),
        }
    }
}

/// Refuses a run before it starts: a script of a version this runner does not
/// take, or one with no compute left.
pub fn precheck(script: &Script) -> Option<Ran> {
    let (step, limit) = match () {
        _ if !super::supports(script.version) => (
            Step::Failed(format!(
                "The runner takes versions {} to {}, not {}.",
                super::OLDEST,
                super::VERSION,
                script.version
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
    })
}

fn total_spent(limits: &Limits) -> String {
    format!(
        "The script used its {}s of compute over all its runs. Make fewer calls one after \
         another, or do less before each: the script resumes from the start after its calls \
         finish.",
        limits.total_compute_ms / 1000
    )
}

fn memory(limits: &Limits) -> String {
    format!(
        "The script ran out of its {} MiB of memory. Keep less: take only the fields you need \
         from each result.",
        limits.memory >> 20
    )
}

fn rendered(value: Option<&str>, logs: &[String], returned: usize) -> (String, Option<Limit>) {
    let mut out = serde_json::Map::new();
    let mut over = None;
    if let Some(value) = value {
        let value = match value.len() > returned {
            true => {
                over = Some(Limit::Returned);
                let start: String = value.chars().take(PREVIEW).collect();
                serde_json::Value::String(format!(
                    "[the result is {} KiB, over the {} KiB limit; return a summary. It began: \
                     {start}]",
                    value.len() >> 10,
                    returned >> 10
                ))
            }
            false => serde_json::from_str(value).unwrap_or(serde_json::Value::Null),
        };
        out.insert("result".to_string(), value);
    }
    if !logs.is_empty() {
        out.insert("logs".to_string(), serde_json::json!(logs));
    }
    (serde_json::Value::Object(out).to_string(), over)
}

fn with_logs(error: &str, logs: &[String]) -> String {
    match logs.is_empty() {
        true => error.to_string(),
        false => format!("{error}\n\nprinted output:\n{}", logs.join("\n")),
    }
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
            tools: vec![],
            spent: false,
            limits: Limits::default(),
            cpu_used_ms: 0,
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
            Step::Done(r#"{"logs":["hi"],"result":[1,2]}"#.into())
        );
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
