use std::cell::RefCell;
use std::rc::Rc;

use rquickjs::{Context, Function, Runtime};

use super::{Limit, Limits, Ran, Script, ScriptCall, ScriptRecord, Stats, Step};

const STACK: usize = 512 << 10;
/// QuickJS calls the interrupt handler about every 10k instructions. A
/// backstop to the CPU limit.
const TICKS: u64 = 1_000_000;
const JOBS: usize = 1_000_000;
const ARGUMENTS: usize = 256 << 10;
/// The start of an oversized result that is still shown.
const PREVIEW: usize = 4 << 10;

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

#[derive(Default)]
struct Run {
    limits: Limits,
    records: Vec<ScriptRecord>,
    spent: bool,
    seq: usize,
    calls: Vec<ScriptCall>,
    argument_bytes: usize,
    diverged: Option<String>,
    logs: Vec<String>,
    log_bytes: usize,
    logs_cut: bool,
    outcome: Option<Result<Option<String>, String>>,
    stopped: Option<Limit>,
    refused: Option<Limit>,
}

impl Run {
    fn call(&mut self, name: String, arguments: String) -> Option<String> {
        self.seq += 1;
        self.argument_bytes += arguments.len();
        let mut refuse = |limit: Limit, message: String| {
            self.refused.get_or_insert(limit);
            Some(serde_json::json!({"ok": false, "output": message}).to_string())
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
                let message = super::spent(self.limits.results);
                return refuse(Limit::Results, message);
            }
            self.calls.push(ScriptCall { name, arguments });
            return None;
        };
        if record.name != name || record.arguments != arguments {
            self.diverged.get_or_insert(format!(
                "The script changed between runs. Call {} was `{}` and is now `{name}`. A \
                 script must make the same calls each time it runs: do not branch on timing or \
                 on anything outside its results.",
                self.seq, record.name
            ));
            return None;
        }
        Some(serde_json::json!({"ok": record.ok, "output": record.output}).to_string())
    }

    fn log(&mut self, line: String) {
        if self.log_bytes + line.len() > self.limits.logs {
            self.logs_cut = true;
            return;
        }
        self.log_bytes += line.len();
        self.logs.push(line);
    }

    fn logs(&self) -> Vec<String> {
        let mut logs = self.logs.clone();
        if self.logs_cut {
            logs.push(format!(
                "[output over {} KiB was cut; log less]",
                self.limits.logs >> 10
            ));
        }
        logs
    }

    fn step(self) -> (Step, Option<Limit>) {
        if let Some(limit) = self.stopped {
            let message = match limit {
                Limit::Compute => format!(
                    "The script used its {}s of compute. Do less work in one run: work done \
                     before a call is done again each time the script resumes.",
                    self.limits.compute_ms / 1000
                ),
                _ => format!(
                    "The script ran out of its {} MiB of memory. Keep less: take only the \
                     fields you need from each result.",
                    self.limits.memory >> 20
                ),
            };
            return (Step::Failed(message), Some(limit));
        }
        if let Some(diverged) = &self.diverged {
            return (Step::Failed(diverged.clone()), Some(Limit::Diverged));
        }
        let logs = self.logs();
        let cut = self.logs_cut.then_some(Limit::Logs);
        match &self.outcome {
            Some(Ok(value)) => {
                let (text, over) = rendered(value.as_deref(), &logs, self.limits.returned);
                (Step::Done(text), over.or(self.refused).or(cut))
            }
            Some(Err(error)) => {
                let limit = match error {
                    e if e.contains("out of memory") => Some(Limit::Memory),
                    e if e.contains("stack overflow") || e.contains("call stack") => {
                        Some(Limit::Stack)
                    }
                    _ => self.refused,
                };
                (Step::Failed(with_logs(error, &logs)), limit)
            }
            None if !self.calls.is_empty() => (Step::Calls(self.calls), self.refused),
            None => (
                Step::Failed(with_logs(
                    "The script waits on a promise that never settles. Await only tool calls \
                     and promises built from them.",
                    &logs,
                )),
                Some(Limit::NeverSettles),
            ),
        }
    }
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
        false => format!("{error}\n\nconsole output:\n{}", logs.join("\n")),
    }
}

/// CPU this thread has used.
fn cpu_ms() -> u64 {
    let mut now = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    unsafe { libc::clock_gettime(libc::CLOCK_THREAD_CPUTIME_ID, &mut now) };
    now.tv_sec as u64 * 1000 + now.tv_nsec as u64 / 1_000_000
}

/// Run `script` past the calls it already made. The same script and records
/// always give the same step.
pub fn run(script: &Script) -> Ran {
    if !super::supports(script.version) {
        let step = Step::Failed(format!(
            "The runner takes versions {} to {}, not {}.",
            super::OLDEST,
            super::VERSION,
            script.version
        ));
        return Ran {
            step,
            stats: Stats::default(),
        };
    }
    let started = cpu_ms();
    let (step, heap, limit) = match execute(script, started) {
        Ok(ran) => ran,
        Err(e) => (
            Step::Failed(format!("The script could not run: {e}")),
            0,
            None,
        ),
    };
    Ran {
        step,
        stats: Stats {
            heap,
            cpu_ms: cpu_ms().saturating_sub(started),
            limit,
            warm: false,
        },
    }
}

fn execute(script: &Script, started: u64) -> rquickjs::Result<(Step, usize, Option<Limit>)> {
    let limits = script.limits.clone();
    let run = Rc::new(RefCell::new(Run {
        limits: limits.clone(),
        records: script.calls.clone(),
        spent: script.spent,
        ..Run::default()
    }));
    let rt = Runtime::new()?;
    rt.set_memory_limit(limits.memory);
    rt.set_max_stack_size(STACK);
    let mut ticks = 0u64;
    let stop = run.clone();
    let compute = limits.compute_ms;
    rt.set_interrupt_handler(Some(Box::new(move || {
        ticks += 1;
        if ticks <= TICKS && cpu_ms().saturating_sub(started) <= compute {
            return false;
        }
        stop.borrow_mut().stopped.get_or_insert(Limit::Compute);
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
        run.borrow_mut().stopped.get_or_insert(Limit::Compute);
    }
    let heap = rt.memory_usage().malloc_size.max(0) as usize;
    {
        let mut run = run.borrow_mut();
        let finished = matches!(run.outcome, Some(Ok(_)));
        if !finished && run.calls.is_empty() && heap > limits.memory / 10 * 9 {
            run.stopped.get_or_insert(Limit::Memory);
        }
    }
    drop(ctx);
    rt.set_interrupt_handler(None);
    let run = Rc::try_unwrap(run)
        .map(RefCell::into_inner)
        .unwrap_or_else(|r| std::mem::take(&mut *r.borrow_mut()));
    let (step, limit) = run.step();
    Ok((step, heap, limit))
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

    fn run(script: &Script) -> Step {
        super::run(script).step
    }

    fn script(code: &str, calls: Vec<ScriptRecord>) -> Script {
        Script {
            limits: Limits {
                compute_ms: 1_000,
                memory: 16 << 20,
                ..Limits::default()
            },
            code: code.to_string(),
            calls,
            seed: 7,
            now_ms: 1_700_000_000_000,
            spent: false,
            version: crate::codemode::VERSION,
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
        assert!(
            matches!(&step, Step::Failed(m) if m.contains("1s of compute")),
            "{step:?}"
        );
    }

    #[test]
    fn an_endless_chain_of_promises_is_stopped() {
        let step = run(&script(
            "const f = () => { Promise.resolve().then(f); }; f(); await new Promise(() => {});",
            vec![],
        ));
        assert!(
            matches!(&step, Step::Failed(m) if m.contains("compute") || m.contains("memory")),
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
    fn a_script_whose_results_are_spent_makes_no_new_call() {
        let mut spent = script(
            "await tools.a(); try { await tools.b(); } catch (e) { return e.message; }",
            vec![record("a", "{}", true, "1")],
        );
        spent.spent = true;
        let step = run(&spent);
        assert!(
            matches!(&step, Step::Done(t) if t.contains("MiB")),
            "{step:?}"
        );
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
            matches!(&step, Step::Done(t) if t.contains("over the 64 KiB limit") && t.contains("It began:")),
            "{step:?}"
        );
    }

    #[test]
    fn a_script_that_finishes_is_not_failed_for_the_memory_it_still_holds() {
        let code = "const kept = Array.from({ length: 40000 }, (_, i) => ({ i, s: 'x'.repeat(200) })); return kept.length;";
        let step = run(&script(code, vec![]));
        assert_eq!(step, Step::Done(r#"{"result":40000}"#.into()));
    }

    #[test]
    fn calls_past_the_limit_throw_what_the_script_can_catch() {
        let mut limited = script(
            "await tools.a(); await tools.b(); try { await tools.c(); } catch (e) { return e.message; }",
            vec![record("a", "{}", true, "1"), record("b", "{}", true, "2")],
        );
        limited.limits.calls = 2;
        let ran = super::run(&limited);
        assert!(
            matches!(&ran.step, Step::Done(t) if t.contains("at most 2 calls")),
            "{ran:?}"
        );
        assert_eq!(ran.stats.limit, Some(Limit::Calls));
    }

    #[test]
    fn arguments_past_the_limit_throw() {
        let mut limited = script(
            "try { await tools.a({ s: 'x'.repeat(2000) }); } catch (e) { return e.message; }",
            vec![],
        );
        limited.limits.arguments = 1000;
        let step = run(&limited);
        assert!(
            matches!(&step, Step::Done(t) if t.contains("arguments passed")),
            "{step:?}"
        );
    }

    #[test]
    fn a_run_says_which_limit_stopped_it() {
        let ran = super::run(&script("while (true) {}", vec![]));
        assert_eq!(ran.stats.limit, Some(Limit::Compute));
        assert!(ran.stats.cpu_ms >= 1_000, "{:?}", ran.stats);
        let ran = super::run(&script("return 1;", vec![]));
        assert_eq!(ran.stats.limit, None);
        assert!(ran.stats.heap > 0);
    }
}
