mod confine;
pub mod sandbox;

pub use sandbox::Sandbox;
pub use substructure_session::{Script, ScriptRecord};

use std::cell::RefCell;
use std::rc::Rc;
use std::time::{Duration, Instant};

use rquickjs::{Context, Function, Runtime};
use substructure_session::command::{CommandPayload, Outcome, SettleError};
use substructure_session::events::ScriptCall;
use substructure_session::protocol::{ErrorCode, ErrorInfo, StoredResult};
use substructure_session::state::EffectKind;

const MEMORY: usize = 64 << 20;
const STACK: usize = 512 << 10;
/// QuickJS calls the handler about every 10k instructions.
const TICKS: u64 = 10_000;
const WALL: Duration = Duration::from_secs(10);
const JOBS: usize = 1_000_000;
const CALLS: usize = 1_000;
const ARGUMENTS: usize = 256 << 10;
const ALL_ARGUMENTS: usize = 4 << 20;
const LOGS: usize = 16 << 10;
const RESULT: usize = 32 << 10;

const PRELUDE: &str = r#"
(() => {
  const call = globalThis.__call, log = globalThis.__log, done = globalThis.__done;
  delete globalThis.__call; delete globalThis.__log; delete globalThis.__done;
  const parse = (s) => { try { return JSON.parse(s); } catch { return s; } };
  const invoke = (name, args) => {
    const json = JSON.stringify(args === undefined ? {} : args);
    const r = call(name, json === undefined ? "{}" : json);
    if (r == null) return new Promise(() => {});
    const { ok, output } = JSON.parse(r);
    return ok ? Promise.resolve(parse(output)) : Promise.reject(new Error(output));
  };
  const names = NAMES;
  globalThis.tools = new Proxy(Object.fromEntries(names.map((n) => [n, true])), {
    get: (_, name) => typeof name === "string" && name !== "then" ? (args) => invoke(name, args) : undefined,
  });
  const show = (v) => {
    if (typeof v === "string") return v;
    try { const s = JSON.stringify(v); return s === undefined ? String(v) : s; } catch { return String(v); }
  };
  const write = (...a) => log(a.map(show).join(" "));
  globalThis.console = { log: write, info: write, warn: write, error: write, debug: write };
  let seed = SEED | 0;
  Math.random = () => {
    seed = (seed + 0x6D2B79F5) | 0;
    let t = Math.imul(seed ^ (seed >>> 15), 1 | seed);
    t = (t + Math.imul(t ^ (t >>> 7), 61 | t)) ^ t;
    return ((t ^ (t >>> 14)) >>> 0) / 4294967296;
  };
  const RealDate = Date;
  globalThis.Date = class Date extends RealDate {
    constructor(...a) { a.length ? super(...a) : super(NOW); }
    static now() { return NOW; }
  };
  let settled = false;
  globalThis.__run = (f) => {
    delete globalThis.__run;
    f().then(
      (v) => { if (settled) return; settled = true; let s; try { s = JSON.stringify(v); } catch { s = JSON.stringify(String(v)); } done(true, s === undefined ? null : s); },
      (e) => { if (settled) return; settled = true; done(false, e instanceof Error ? `${e}${e.stack ? "\n" + e.stack : ""}` : show(e)); },
    );
  };
})();
"#;

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum Step {
    /// The script needs these calls before it can go on.
    Calls(Vec<ScriptCall>),
    /// The script returned. The text goes back to the model.
    Done(String),
    Failed(String),
}

#[derive(Default)]
struct Run {
    records: Vec<ScriptRecord>,
    seq: usize,
    calls: Vec<ScriptCall>,
    call_bytes: usize,
    diverged: Option<String>,
    logs: Vec<String>,
    log_bytes: usize,
    logs_cut: bool,
    outcome: Option<Result<Option<String>, String>>,
    stopped: Option<&'static str>,
}

impl Run {
    fn call(&mut self, name: String, arguments: String) -> Option<String> {
        self.seq += 1;
        let refuse =
            |message: &str| Some(serde_json::json!({"ok": false, "output": message}).to_string());
        if self.seq > CALLS {
            return refuse(&format!("A script can make at most {CALLS} calls."));
        }
        if arguments.len() > ARGUMENTS {
            return refuse(&format!(
                "Arguments to `{name}` are over {} KiB.",
                ARGUMENTS >> 10
            ));
        }
        let Some(record) = self.records.get(self.seq - 1) else {
            self.call_bytes += arguments.len();
            if self.call_bytes > ALL_ARGUMENTS {
                return refuse(&format!(
                    "Calls made at once can carry at most {} MiB of arguments.",
                    ALL_ARGUMENTS >> 20
                ));
            }
            self.calls.push(ScriptCall { name, arguments });
            return None;
        };
        if record.name != name || record.arguments != arguments {
            self.diverged.get_or_insert(format!(
                "The script changed between runs. Call {} was `{}` and is now `{name}`. A \
                 script must make the same calls each time it runs.",
                self.seq, record.name
            ));
            return None;
        }
        Some(serde_json::json!({"ok": record.ok, "output": record.output}).to_string())
    }

    fn log(&mut self, line: String) {
        if self.log_bytes + line.len() > LOGS {
            self.logs_cut = true;
            return;
        }
        self.log_bytes += line.len();
        self.logs.push(line);
    }

    fn logs(&self) -> Vec<String> {
        let mut logs = self.logs.clone();
        if self.logs_cut {
            logs.push(format!("[output over {} KiB was cut]", LOGS >> 10));
        }
        logs
    }

    fn step(self) -> Step {
        if let Some(reason) = self.stopped {
            return Step::Failed(reason.to_string());
        }
        if let Some(diverged) = &self.diverged {
            return Step::Failed(diverged.clone());
        }
        let logs = self.logs();
        match &self.outcome {
            Some(Ok(value)) => Step::Done(rendered(value.as_deref(), &logs)),
            Some(Err(error)) => Step::Failed(with_logs(error, &logs)),
            None if !self.calls.is_empty() => Step::Calls(self.calls),
            None => Step::Failed(with_logs(
                "The script waits on a promise that never settles.",
                &logs,
            )),
        }
    }
}

fn rendered(value: Option<&str>, logs: &[String]) -> String {
    let mut out = serde_json::Map::new();
    if let Some(value) = value {
        let value = match value.len() > RESULT {
            true => serde_json::Value::String(format!(
                "[the result is {} KiB, over the {} KiB limit; return less]",
                value.len() >> 10,
                RESULT >> 10
            )),
            false => serde_json::from_str(value).unwrap_or(serde_json::Value::Null),
        };
        out.insert("result".to_string(), value);
    }
    if !logs.is_empty() {
        out.insert("logs".to_string(), serde_json::json!(logs));
    }
    serde_json::Value::Object(out).to_string()
}

fn with_logs(error: &str, logs: &[String]) -> String {
    match logs.is_empty() {
        true => error.to_string(),
        false => format!("{error}\n\nconsole output:\n{}", logs.join("\n")),
    }
}

/// Run `script` past the calls it already made. The same script and records
/// always give the same step.
pub(crate) fn run(script: &Script) -> Step {
    match execute(script) {
        Ok(step) => step,
        Err(e) => Step::Failed(format!("The script could not run: {e}")),
    }
}

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

fn execute(script: &Script) -> rquickjs::Result<Step> {
    let run = Rc::new(RefCell::new(Run {
        records: script.calls.clone(),
        ..Run::default()
    }));
    let rt = Runtime::new()?;
    rt.set_memory_limit(MEMORY);
    rt.set_max_stack_size(STACK);
    let started = Instant::now();
    let mut ticks = 0u64;
    let stop = run.clone();
    rt.set_interrupt_handler(Some(Box::new(move || {
        ticks += 1;
        let reason = match () {
            _ if ticks > TICKS => "The script ran too long.",
            _ if started.elapsed() > WALL => "The script ran too long.",
            _ => return false,
        };
        stop.borrow_mut().stopped.get_or_insert(reason);
        true
    })));
    let ctx = Context::full(&rt)?;
    let compiled = ctx.with(|ctx| -> rquickjs::Result<Option<String>> {
        let globals = ctx.globals();
        let r = run.clone();
        globals.set(
            "__call",
            Function::new(ctx.clone(), move |name: String, arguments: String| {
                r.borrow_mut().call(name, arguments)
            })?,
        )?;
        let r = run.clone();
        globals.set(
            "__log",
            Function::new(ctx.clone(), move |line: String| r.borrow_mut().log(line))?,
        )?;
        let r = run.clone();
        globals.set(
            "__done",
            Function::new(ctx.clone(), move |ok: bool, value: Option<String>| {
                r.borrow_mut().outcome = Some(match ok {
                    true => Ok(value),
                    false => Err(value.unwrap_or_default()),
                });
            })?,
        )?;
        let prelude = PRELUDE
            .replace("SEED", &(script.seed as u32 as i32).to_string())
            .replace("NOW", &script.now_ms.to_string())
            .replace(
                "NAMES",
                &serde_json::to_string(&script.tools).unwrap_or_else(|_| "[]".to_string()),
            );
        ctx.eval::<(), _>(prelude)?;
        let source = format!("__run(async () => {{\n{}\n}});", script.code);
        match ctx.eval::<(), _>(source) {
            Ok(()) => Ok(None),
            Err(rquickjs::Error::Exception) => Ok(Some(exception(&ctx))),
            Err(e) => Err(e),
        }
    })?;
    if let Some(error) = compiled {
        run.borrow_mut().outcome.get_or_insert(Err(error));
    }
    for _ in 0..JOBS {
        match rt.execute_pending_job() {
            Ok(true) => {}
            Ok(false) => break,
            Err(e) => {
                let error = e.0.with(|ctx| exception(&ctx));
                run.borrow_mut().outcome.get_or_insert(Err(error));
                break;
            }
        }
    }
    if rt.is_job_pending() {
        run.borrow_mut()
            .stopped
            .get_or_insert("The script ran too long.");
    }
    if rt.memory_usage().malloc_size as usize > MEMORY / 10 * 9 {
        run.borrow_mut()
            .stopped
            .get_or_insert("The script ran out of memory.");
    }
    drop(ctx);
    rt.set_interrupt_handler(None);
    let run = Rc::try_unwrap(run)
        .map(RefCell::into_inner)
        .unwrap_or_else(|r| std::mem::take(&mut *r.borrow_mut()));
    Ok(run.step())
}

fn exception(ctx: &rquickjs::Ctx<'_>) -> String {
    let caught = ctx.catch();
    if let Some(e) = caught.as_exception() {
        let message = e.message().unwrap_or_default();
        return match e.stack() {
            Some(stack) if !stack.is_empty() => format!("{message}\n{stack}"),
            _ => message,
        };
    }
    ctx.json_stringify(caught)
        .ok()
        .flatten()
        .map(|s| s.to_string().unwrap_or_default())
        .unwrap_or_else(|| "The script threw.".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn script(code: &str, calls: Vec<ScriptRecord>) -> Script {
        Script {
            code: code.to_string(),
            calls,
            seed: 7,
            now_ms: 1_700_000_000_000,
            tools: vec!["weather".to_string(), "list-prs".to_string()],
        }
    }

    fn record(name: &str, arguments: &str, ok: bool, output: &str) -> ScriptRecord {
        ScriptRecord {
            name: name.to_string(),
            arguments: arguments.to_string(),
            ok,
            output: output.to_string(),
        }
    }

    fn call(name: &str, arguments: &str) -> ScriptCall {
        ScriptCall {
            name: name.to_string(),
            arguments: arguments.to_string(),
        }
    }

    #[test]
    fn a_script_with_no_calls_returns_its_value_and_logs() {
        let step = run(&script("console.log('hi', {a: 1}); return [1, 2];", vec![]));
        assert_eq!(
            step,
            Step::Done(r#"{"logs":["hi {\"a\":1}"],"result":[1,2]}"#.into())
        );
    }

    #[test]
    fn a_call_with_no_record_stops_the_run() {
        let step = run(&script(
            "const w = await tools.weather({city: 'Oslo'}); return w;",
            vec![],
        ));
        assert_eq!(
            step,
            Step::Calls(vec![call("weather", r#"{"city":"Oslo"}"#)])
        );
    }

    #[test]
    fn a_recorded_call_returns_its_parsed_result() {
        let code = "const w = await tools.weather({city: 'Oslo'}); return w.temp + 1;";
        let step = run(&script(
            code,
            vec![record(
                "weather",
                r#"{"city":"Oslo"}"#,
                true,
                r#"{"temp":3}"#,
            )],
        ));
        assert_eq!(step, Step::Done(r#"{"result":4}"#.into()));
    }

    #[test]
    fn calls_made_together_stop_the_run_together() {
        let code =
            "const [a, b] = await Promise.all([tools.a(), tools['b-c']({x: 1})]); return a + b;";
        let step = run(&script(code, vec![]));
        assert_eq!(
            step,
            Step::Calls(vec![call("a", "{}"), call("b-c", r#"{"x":1}"#)])
        );
        let step = run(&script(
            code,
            vec![
                record("a", "{}", true, "1"),
                record("b-c", r#"{"x":1}"#, true, "2"),
            ],
        ));
        assert_eq!(step, Step::Done(r#"{"result":3}"#.into()));
    }

    #[test]
    fn a_failed_call_throws_what_the_script_can_catch() {
        let code = "try { await tools.a(); } catch (e) { return e.message; }";
        let step = run(&script(
            code,
            vec![record("a", "{}", false, "no such issue")],
        ));
        assert_eq!(step, Step::Done(r#"{"result":"no such issue"}"#.into()));
    }

    #[test]
    fn a_script_that_calls_differently_on_replay_fails() {
        let step = run(&script(
            "await tools.b();",
            vec![record("a", "{}", true, "1")],
        ));
        assert!(matches!(step, Step::Failed(m) if m.contains("changed between runs")));
    }

    #[test]
    fn a_thrown_error_fails_the_run_with_its_message() {
        let step = run(&script(
            "console.log('before'); throw new Error('boom');",
            vec![],
        ));
        assert!(
            matches!(&step, Step::Failed(m) if m.starts_with("Error: boom") && m.contains("before")),
            "{step:?}"
        );
    }

    #[test]
    fn a_syntax_error_fails_the_run() {
        let step = run(&script("return (;", vec![]));
        assert!(
            matches!(&step, Step::Failed(m) if m.contains("SyntaxError") || m.contains("expect")),
            "{step:?}"
        );
    }

    #[test]
    fn an_endless_loop_is_stopped() {
        let step = run(&script(
            "try { while (true) {} } catch (e) { return 'caught'; }",
            vec![],
        ));
        assert_eq!(step, Step::Failed("The script ran too long.".into()));
    }

    #[test]
    fn an_endless_chain_of_promises_is_stopped() {
        let step = run(&script(
            "const f = () => { Promise.resolve().then(f); }; f(); await new Promise(() => {});",
            vec![],
        ));
        assert!(
            matches!(&step, Step::Failed(m) if m.contains("too long") || m.contains("memory")),
            "{step:?}"
        );
    }

    #[test]
    fn runaway_memory_fails_the_run() {
        let step = run(&script(
            "const a = []; while (true) a.push('x'.repeat(1 << 20));",
            vec![],
        ));
        assert!(matches!(step, Step::Failed(_)), "{step:?}");
    }

    #[test]
    fn deep_recursion_fails_the_run() {
        let step = run(&script(
            "const f = (n) => f(n + 1) + 1; return f(0);",
            vec![],
        ));
        assert!(
            matches!(&step, Step::Failed(m) if m.contains("stack") || m.contains("Range")),
            "{step:?}"
        );
    }

    #[test]
    fn a_promise_that_never_settles_fails_the_run() {
        let step = run(&script("await new Promise(() => {});", vec![]));
        assert!(
            matches!(&step, Step::Failed(m) if m.contains("never settles")),
            "{step:?}"
        );
    }

    #[test]
    fn time_and_random_are_the_same_on_every_run() {
        let code = "return [Date.now(), new Date().getTime(), Math.random()];";
        let first = run(&script(code, vec![]));
        assert_eq!(first, run(&script(code, vec![])));
        assert!(
            matches!(&first, Step::Done(t) if t.starts_with(r#"{"result":[1700000000000,1700000000000,"#)),
            "{first:?}"
        );
    }

    #[test]
    fn the_sandbox_has_no_host_access() {
        let code = "return [typeof fetch, typeof require, typeof setTimeout, typeof __call, typeof os, typeof std];";
        let step = run(&script(code, vec![]));
        assert_eq!(step, Step::Done(r#"{"result":["undefined","undefined","undefined","undefined","undefined","undefined"]}"#.into()));
    }

    #[test]
    fn the_tools_can_be_listed() {
        let code = "return [Object.keys(tools), 'weather' in tools];";
        let step = run(&script(code, vec![]));
        assert_eq!(
            step,
            Step::Done(r#"{"result":[["weather","list-prs"],true]}"#.into())
        );
    }

    #[test]
    fn an_oversized_result_is_replaced_with_a_note() {
        let step = run(&script("return 'x'.repeat(100000);", vec![]));
        assert!(
            matches!(&step, Step::Done(t) if t.contains("over the 32 KiB limit")),
            "{step:?}"
        );
    }
}
