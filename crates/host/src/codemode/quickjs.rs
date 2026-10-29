//! JavaScript, run by QuickJS.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use rquickjs::allocator::{Allocator, RustAllocator};
use rquickjs::convert::List;
use rquickjs::{Context, Function, Runtime};

use super::engine::{cpu_ms, Engine};
use super::files::{Files, Host};
use super::replay::{Answer, End, Replay};
use super::{Language, Limit, Ran, Script, Stats, Step};

const STACK: usize = 512 << 10;
/// QuickJS calls the interrupt handler about every 10k instructions. A
/// backstop to the CPU limit.
const TICKS: u64 = 1_000_000;
const JOBS: usize = 1_000_000;

pub struct QuickJs;

impl Engine for QuickJs {
    fn language(&self) -> Language {
        Language::Javascript
    }

    fn run(&self, script: &Script, host: Box<dyn Host>) -> Ran {
        let started = cpu_ms();
        let replay = Rc::new(RefCell::new(Replay::new(script)));
        let files = Rc::new(RefCell::new(Files::new(script, host)));
        let heap = match execute(script, &replay, &files, started) {
            Ok(heap) => heap,
            Err(e) => {
                replay.borrow_mut().end(End::Raised {
                    message: format!("The script could not run: {e}"),
                    limit: None,
                });
                0
            }
        };
        let replay = Rc::try_unwrap(replay)
            .ok()
            .expect("the runtime is gone")
            .into_inner();
        let (step, limit) = replay.finish();
        let (file_bytes, files_limit) = files.borrow().spent();
        let limit = match step {
            Step::Failed(_) => limit.or(files_limit),
            _ => limit,
        };
        Ran {
            step,
            stats: Stats {
                heap,
                cpu_ms: cpu_ms().saturating_sub(started),
                file_bytes,
                limit,
                warm: false,
            },
        }
    }
}

const PRELUDE: &str = r#"
(() => {
  const { __call: call, __log: log, __done: done, __names: names, __now: NOW } = globalThis;
  const { __files_list: listed, __files_text: ftext, __files_open: fopen, __files_next: fnext, __files_close: fclose } = globalThis;
  let seed = globalThis.__seed;
  for (const k of ["__call", "__log", "__done", "__names", "__now", "__seed", "__files_list", "__files_text", "__files_open", "__files_next", "__files_close"]) delete globalThis[k];
  const answered = (r) => { const a = JSON.parse(r); if ("error" in a) throw new Error(a.error); return a.value; };
  const known = JSON.parse(listed);
  async function* stream(id, format, options) {
    const s = answered(fopen(String(id), JSON.stringify({ ...(options ?? {}), format })));
    try {
      for (;;) {
        const batch = answered(fnext(s));
        if (batch === null) return;
        yield* batch;
      }
    } finally { fclose(s); }
  }
  globalThis.files = Object.freeze({
    list: () => known.map((f) => ({ ...f })),
    stat: (id) => {
      const f = known.find((f) => f.id === id);
      if (!f) throw new Error(`There is no file "${id}".`);
      return { ...f };
    },
    text: async (id) => answered(ftext(String(id))),
    lines: (id, options) => stream(id, "lines", options),
    jsonl: (id, options) => stream(id, "jsonl", options),
    csv: (id, options) => stream(id, "csv", options),
    chunks: (id, options) => stream(id, "chunks", options),
  });
  const parse = (s) => { try { return JSON.parse(s); } catch { return s; } };
  const invoke = (name, args) => {
    const json = JSON.stringify(args === undefined ? {} : args);
    const r = call(name, json === undefined ? "{}" : json);
    if (r == null) return new Promise(() => {});
    const [ok, output] = r;
    return ok ? Promise.resolve(parse(output)) : Promise.reject(new Error(output));
  };
  globalThis.tools = new Proxy(Object.fromEntries(names.map((n) => [n, true])), {
    get: (_, name) => typeof name === "string" && name !== "then" ? (args) => invoke(name, args) : undefined,
  });
  const show = (v) => {
    if (typeof v === "string") return v;
    try { const s = JSON.stringify(v); return s === undefined ? String(v) : s; } catch { return String(v); }
  };
  const write = (...a) => log(a.map(show).join(" "));
  globalThis.console = { log: write, info: write, warn: write, error: write, debug: write };
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
      (v) => { if (settled) return; settled = true; let s; try { s = JSON.stringify(v); } catch { s = JSON.stringify(String(v)); } done(true, s === undefined ? null : s, null); },
      (e) => { if (settled) return; settled = true; done(false, e instanceof Error ? `${e}${e.stack ? "\n" + e.stack : ""}` : show(e), e instanceof RangeError ? e.message : null); },
    );
  };
})();
"#;

/// QuickJS's allocator: it holds the heap to `limit`, notes when it refused,
/// and keeps the most it held at once.
struct Peak {
    inner: RustAllocator,
    now: usize,
    limit: usize,
    peak: Rc<Cell<usize>>,
    refused: Rc<Cell<bool>>,
}

impl Peak {
    fn fits(&self, size: usize) -> bool {
        let fits = self.now.saturating_add(size) <= self.limit;
        if !fits {
            self.refused.set(true);
        }
        fits
    }

    fn grew(&mut self, ptr: *mut u8) -> *mut u8 {
        if !ptr.is_null() {
            self.now += unsafe { RustAllocator::usable_size(ptr) };
            self.peak.set(self.peak.get().max(self.now));
        }
        ptr
    }
}

unsafe impl Allocator for Peak {
    fn alloc(&mut self, size: usize) -> *mut u8 {
        if !self.fits(size) {
            return std::ptr::null_mut();
        }
        let ptr = self.inner.alloc(size);
        self.grew(ptr)
    }

    fn calloc(&mut self, count: usize, size: usize) -> *mut u8 {
        if !self.fits(count.saturating_mul(size)) {
            return std::ptr::null_mut();
        }
        let ptr = self.inner.calloc(count, size);
        self.grew(ptr)
    }

    unsafe fn dealloc(&mut self, ptr: *mut u8) {
        if !ptr.is_null() {
            self.now = self.now.saturating_sub(RustAllocator::usable_size(ptr));
        }
        self.inner.dealloc(ptr)
    }

    unsafe fn realloc(&mut self, ptr: *mut u8, new_size: usize) -> *mut u8 {
        let old = match ptr.is_null() {
            true => 0,
            false => RustAllocator::usable_size(ptr),
        };
        if new_size > old && !self.fits(new_size - old) {
            return std::ptr::null_mut();
        }
        let moved = self.inner.realloc(ptr, new_size);
        if !moved.is_null() || new_size == 0 {
            self.now = self.now.saturating_sub(old);
        }
        self.grew(moved)
    }

    unsafe fn usable_size(ptr: *mut u8) -> usize {
        RustAllocator::usable_size(ptr)
    }
}

/// QuickJS's message for a `RangeError` past the stack limit.
const OVERFLOW: &str = "Maximum call stack size exceeded";

/// An error the script raised. `range` is the message when it is a
/// `RangeError`.
fn raised(message: String, range: Option<&str>) -> End {
    End::Raised {
        message,
        limit: (range == Some(OVERFLOW)).then_some(Limit::Stack),
    }
}

/// Runs the script on QuickJS, and returns the most heap it held.
fn execute(
    script: &Script,
    replay: &Rc<RefCell<Replay>>,
    files: &Rc<RefCell<Files>>,
    started: u64,
) -> rquickjs::Result<usize> {
    let limits = replay.borrow().limits().clone();
    let peak = Rc::new(Cell::new(0));
    let refused = Rc::new(Cell::new(false));
    let rt = Runtime::new_with_alloc(Peak {
        inner: RustAllocator,
        now: 0,
        limit: limits.memory,
        peak: peak.clone(),
        refused: refused.clone(),
    })?;
    rt.set_max_stack_size(STACK);
    let mut ticks = 0u64;
    let stop = replay.clone();
    let compute = script.compute_ms();
    rt.set_interrupt_handler(Some(Box::new(move || {
        ticks += 1;
        if ticks <= TICKS && cpu_ms().saturating_sub(started) <= compute {
            return false;
        }
        stop.borrow_mut().stop(Limit::Compute);
        true
    })));
    let ctx = Context::full(&rt)?;
    let compiled = ctx.with(|ctx| -> rquickjs::Result<Option<End>> {
        let globals = ctx.globals();
        let r = replay.clone();
        globals.set(
            "__call",
            Function::new(ctx.clone(), move |name: String, arguments: String| match r
                .borrow_mut()
                .call(name, arguments)
            {
                Answer::Result { ok, output } => Some(List((ok, output))),
                Answer::Refused(message) => Some(List((false, message))),
                Answer::Pending => None,
            })?,
        )?;
        let r = replay.clone();
        globals.set(
            "__log",
            Function::new(ctx.clone(), move |line: String| r.borrow_mut().log(line))?,
        )?;
        let r = replay.clone();
        globals.set(
            "__done",
            Function::new(
                ctx.clone(),
                move |ok: bool, value: Option<String>, range: Option<String>| {
                    r.borrow_mut().end(match ok {
                        true => End::Returned(value),
                        false => raised(value.unwrap_or_default(), range.as_deref()),
                    });
                },
            )?,
        )?;
        globals.set("__names", script.tools.clone())?;
        globals.set("__now", script.now_ms as f64)?;
        globals.set("__seed", script.seed as u32 as i32)?;
        let listed = serde_json::to_string(files.borrow().list()).unwrap_or_default();
        globals.set("__files_list", listed)?;
        let f = files.clone();
        globals.set(
            "__files_text",
            Function::new(ctx.clone(), move |id: String| {
                answer(
                    f.borrow_mut()
                        .text(&id)
                        .map(|t| serde_json::Value::String(t).to_string()),
                )
            })?,
        )?;
        let f = files.clone();
        globals.set(
            "__files_open",
            Function::new(ctx.clone(), move |id: String, options: String| {
                answer(
                    f.borrow_mut()
                        .open_with(&id, &options)
                        .map(|s| s.to_string()),
                )
            })?,
        )?;
        let f = files.clone();
        globals.set(
            "__files_next",
            Function::new(ctx.clone(), move |stream: u32| {
                answer(
                    f.borrow_mut()
                        .next(stream)
                        .map(|b| b.unwrap_or_else(|| "null".to_string())),
                )
            })?,
        )?;
        let f = files.clone();
        globals.set(
            "__files_close",
            Function::new(ctx.clone(), move |stream: u32| f.borrow_mut().close(stream))?,
        )?;
        ctx.eval::<(), _>(PRELUDE)?;
        let source = format!("__run(async () => {{\n{}\n}});", script.code);
        match ctx.eval::<(), _>(source) {
            Ok(()) => Ok(None),
            Err(rquickjs::Error::Exception) => Ok(Some(exception(&ctx))),
            Err(e) => Err(e),
        }
    })?;
    if let Some(end) = compiled {
        replay.borrow_mut().end(end);
    }
    for _ in 0..JOBS {
        match rt.execute_pending_job() {
            Ok(true) => {}
            Ok(false) => break,
            Err(e) => {
                let end = e.0.with(|ctx| exception(&ctx));
                replay.borrow_mut().end(end);
                break;
            }
        }
    }
    if rt.is_job_pending() {
        replay.borrow_mut().stop(Limit::Compute);
    }
    if refused.get() {
        replay.borrow_mut().stop(Limit::Memory);
    }
    drop(ctx);
    rt.set_interrupt_handler(None);
    Ok(peak.get())
}

/// A file call's answer for the prelude: `{"value": …}` with JSON, or
/// `{"error": …}`.
fn answer(result: Result<String, String>) -> String {
    match result {
        Ok(json) => format!("{{\"value\":{json}}}"),
        Err(e) => serde_json::json!({ "error": e }).to_string(),
    }
}

fn exception(ctx: &rquickjs::Ctx<'_>) -> End {
    let caught = ctx.catch();
    if let Some(e) = caught.as_exception() {
        let message = e.message().unwrap_or_default();
        let name: Option<String> = e.get("name").ok();
        let range = (name.as_deref() == Some("RangeError")).then_some(message.as_str());
        let text = match e.stack() {
            Some(stack) if !stack.is_empty() => format!("{message}\n{stack}"),
            _ => message.clone(),
        };
        return raised(text, range);
    }
    let text = ctx
        .json_stringify(caught)
        .ok()
        .flatten()
        .map(|s| s.to_string().unwrap_or_default())
        .unwrap_or_else(|| "The script threw.".to_string());
    raised(text, None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codemode::engine;
    use crate::codemode::files::NoFiles;
    use crate::codemode::{Limits, ScriptCall, ScriptRecord, Step};

    fn run(script: &Script) -> Step {
        engine::run(script, Box::new(NoFiles)).step
    }

    fn script(code: &str, calls: Vec<ScriptRecord>) -> Script {
        Script {
            cpu_used_ms: 0,
            files: vec![],
            file_bytes_used: 0,
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
            language: Default::default(),
            tools: vec!["weather".to_string(), "list-prs".to_string()],
        }
    }

    /// Runs `code` with `files` to read.
    fn with_files(code: &str, files: &[(&str, &str)]) -> Ran {
        let mut s = script(code, vec![]);
        s.files = files
            .iter()
            .map(|(id, text)| crate::codemode::ScriptFile {
                id: id.to_string(),
                mime: "text/plain".to_string(),
                size: text.len() as u64,
            })
            .collect();
        let host = crate::codemode::files::MemoryFiles(
            files
                .iter()
                .map(|(id, text)| (id.to_string(), text.as_bytes().to_vec()))
                .collect(),
        );
        engine::run(&s, Box::new(host))
    }

    #[test]
    fn a_script_streams_a_csv_and_sums_it() {
        let mut csv = String::from("region,amount\n");
        for i in 0..3_000 {
            csv.push_str(&format!("{},{}\n", ["EU", "US", "APAC"][i % 3], i));
        }
        let code = "const t = {};\n\
            for await (const r of files.csv('sales.csv')) t[r.region] = (t[r.region] ?? 0) + Number(r.amount);\n\
            return [files.list().map(f => f.id), t];";
        let ran = with_files(code, &[("sales.csv", &csv)]);
        let sum = |k: usize| (0..3_000).filter(|i| i % 3 == k).sum::<usize>();
        assert_eq!(
            ran.step,
            Step::Done(format!(
                r#"{{"result":[["sales.csv"],{{"APAC":{},"EU":{},"US":{}}}]}}"#,
                sum(2),
                sum(0),
                sum(1)
            ))
        );
        assert_eq!(ran.stats.file_bytes, csv.len() as u64);
    }

    #[test]
    fn a_script_reads_lines_text_and_stops_early() {
        let log: String = (0..10_000).map(|i| format!("line {i}\n")).collect();
        let code = "const first = [];\n\
            for await (const l of files.lines('app.log')) { first.push(l); if (first.length === 2) break; }\n\
            const few = [];\n\
            for await (const l of files.lines('app.log', { limit: 1 })) few.push(l);\n\
            const text = await files.text('note.txt');\n\
            return [first, few, text, files.stat('note.txt').size];";
        let ran = with_files(code, &[("app.log", &log), ("note.txt", "hi")]);
        assert_eq!(
            ran.step,
            Step::Done(r#"{"result":[["line 0","line 1"],["line 0"],"hi",2]}"#.into())
        );
    }

    #[test]
    fn a_missing_file_throws_what_the_script_can_catch() {
        let code = "try { await files.text('nope.csv'); } catch (e) { return e.message; }";
        let ran = with_files(code, &[("sales.csv", "a")]);
        assert!(
            matches!(&ran.step, Step::Done(t) if t.contains("Files: sales.csv")),
            "{ran:?}"
        );
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
            matches!(&step, Step::Failed(m) if m.contains("1000 ms of compute for one run")),
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
        let ran = engine::run(
            &script(
                "const a = []; while (true) a.push('x'.repeat(1 << 20));",
                vec![],
            ),
            Box::new(NoFiles),
        );
        assert_eq!(ran.stats.limit, Some(Limit::Memory), "{ran:?}");
    }

    #[test]
    fn a_tool_error_that_names_a_limit_is_not_that_limit() {
        for message in ["database out of memory", "Maximum call stack size exceeded"] {
            let ran = engine::run(
                &script("await tools.a();", vec![record("a", "{}", false, message)]),
                Box::new(NoFiles),
            );
            assert!(
                matches!(&ran.step, Step::Failed(m) if m.contains(message)),
                "{ran:?}"
            );
            assert_eq!(ran.stats.limit, None, "{ran:?}");
        }
    }

    #[test]
    fn deep_recursion_fails_the_run() {
        let ran = engine::run(
            &script("const f = (n) => f(n + 1) + 1; return f(0);", vec![]),
            Box::new(NoFiles),
        );
        assert_eq!(ran.stats.limit, Some(Limit::Stack), "{ran:?}");
        assert!(
            matches!(&ran.step, Step::Failed(m) if m.contains("call stack")),
            "{ran:?}"
        );
    }

    #[test]
    fn a_promise_that_never_settles_fails_the_run() {
        let step = run(&script("await new Promise(() => {});", vec![]));
        assert!(
            matches!(&step, Step::Failed(m) if m.contains("never finishes")),
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
        let ran = engine::run(&limited, Box::new(NoFiles));
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
        let ran = engine::run(&script("while (true) {}", vec![]), Box::new(NoFiles));
        assert_eq!(ran.stats.limit, Some(Limit::Compute));
        assert!(ran.stats.cpu_ms >= 1_000, "{:?}", ran.stats);
        let ran = engine::run(&script("return 1;", vec![]), Box::new(NoFiles));
        assert_eq!(ran.stats.limit, None);
        assert!(ran.stats.heap > 0);
    }

    #[test]
    fn a_run_gets_only_what_is_left_of_the_whole_budget() {
        let mut nearly = script("while (true) {}", vec![]);
        nearly.cpu_used_ms = nearly.limits.total_compute_ms - 50;
        let ran = engine::run(&nearly, Box::new(NoFiles));
        assert!(
            matches!(&ran.step, Step::Failed(m) if m.contains("over all its runs")),
            "{ran:?}"
        );
        assert!(
            ran.stats.cpu_ms < 500,
            "stopped near 50 ms: {:?}",
            ran.stats
        );
    }

    #[test]
    fn a_script_with_no_budget_left_does_not_run() {
        let mut spent = script("return 1;", vec![]);
        spent.cpu_used_ms = spent.limits.total_compute_ms;
        let ran = engine::run(&spent, Box::new(NoFiles));
        assert!(
            matches!(&ran.step, Step::Failed(m) if m.contains("over all its runs")),
            "{ran:?}"
        );
        assert_eq!(ran.stats.limit, Some(Limit::Compute));
    }
}
