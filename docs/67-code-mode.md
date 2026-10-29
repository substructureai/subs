---
title: Code mode
group: Building agents
---

Code mode gives the model one more tool, `run_code`. The model writes
JavaScript that calls the agent's tools, and gets back only what the script
returns or logs.

Use it when the model chains many calls, runs calls that do not depend on each
other, or reads large results to pick out a few values. Each call in a script
costs no model turn, and its result stays out of the conversation.

## Turn it on

```toml title="subs.toml"
[agent.assistant]
model = "claude-sonnet-5"
codemode = "javascript"
```

A worker sets `codemode: "javascript"` on the agent config. JavaScript is the
only language so far.

## What the model writes

`run_code` takes `code`, the body of an async function.

```javascript
const prs = await tools.github__list_prs({ repo: "acme/api", state: "open" });
const stale = prs.filter((pr) => Date.parse(pr.updated_at) < Date.now() - 14 * 864e5);
const reviews = await Promise.all(stale.map((pr) => tools.github__get_reviews({ number: pr.number })));
return stale.map((pr, i) => ({ number: pr.number, reviews: reviews[i].length }));
```

Code mode is all or nothing. The model gets one tool, `run_code`, and reaches
every other tool from a script: there is no direct `tool_search`, `call_tool`,
`skill` or attachment tool.

The description of `run_code` declares each tool the script can call, with its
input type and, when the tool declares an output schema, its result type. The
declarations stop at 12 KiB. Past that, tools are listed by name, up to 4 KiB.
A deferred tool is neither declared nor listed.

A script finds tools with `tools.tool_search({ query })`, which answers with
each match's declaration, and then calls them by name. It works whether or not
the agent defers tools.

A call returns the tool's result, parsed when it is JSON. A call that fails
throws an `Error` with the tool's message.

## What a script can call

A script can call the agent's worker tools, connector tools, client tools,
subagents, `skill` and `tool_search`. A call that needs approval waits for a
person, and a client tool waits for the client, as a direct call does. A
subagent call returns the subagent's answer. A script cannot call `call_tool`,
the attachment tools, or `run_code`: it calls tools by name, and reads files
with `files`. A call to one of these throws, and the script can catch it.

`await tools.skill({ name })` loads a skill as a direct call does: its tools
unlock. The model reads its instructions in the `run_code` result, whether or
not the script returns them. When the agent searches its skills,
`tools.tool_search` finds them too.

A skill's other files are files. A script reads them by `<skill>/<path>`, such
as `files.text("runbooks:respond/references/escalation.md")`, and shows them
with `files.show`. `files.list` lists them with the skill they belong to, as
`skill`. A script reads the plugin as it
was at its first step: a plugin that changes while the script runs fails it,
and the model can run it again.

## What reaches the model

The `run_code` result is what the script returns and prints, then each file it
showed, then the instructions of each skill it loaded.

`files.show(id)` puts a file in front of the model: an image, PDF, audio or
video as itself, and any other file as its text. A script shows at most 4
files, 64 KiB of file text and 2 MiB of media, which bounds what a file can
cost in context. Text the script makes is printed with `console.log`.

## How a run works

When a script stops on calls, the engine asks your worker about them with a
`codemode.calls` trigger, as it asks about the model's calls on `llm.finished`.

```jsonc
{ "type": "codemode.calls",
  "id": "<run_code call id>",
  "tool_calls": [
    { "id": "call_s1", "type": "function",
      "function": { "name": "weather", "arguments": "{\"city\":\"Oslo\"}" } }
  ] }
```

`proposed` runs each call with `tool.call`, or starts a subagent with
`subagent.spawn`. When a call needs approval, it asks a person about that call
first, and the rest wait. When a subagent finishes, `subagent.finished`
proposes its answer as the call's `tool.result`. Return `proposed`, or answer
each call by its `id`: `tool.call` runs it, and `tool.error` refuses it with
your message. A call that no answer runs or refuses fails, and the script can
catch the error. A worker that returns `proposed` for triggers it does not know
needs no change.

Each call a script makes is a tool call in the session log, under the
`run_code` call. It runs as a direct call does, with the same retries,
deadlines and skill locks. The model does not see it in the transcript.

The engine does not keep a script running while it waits. When the calls a
script made have settled, the engine runs the script again from the start. A
call with a recorded result gets that result at once. The run stops at the
first call with no result, and the engine makes that call. A restart loses
nothing, because the log holds every result.

So a script must make the same calls each time it runs. `Date` and
`Math.random` give the same values on every run. A run that makes a different
call fails.

`run_code` has no deadline of its own. Its calls keep theirs, and the script's
[limits](#limits) bound what it runs. An [interrupt](./100-interrupts.md) holds
a script: no step runs, and no call it asks for goes out, until the resume.

`run_code` has no deadline of its own. Its calls keep theirs, and the script's
[limits](#limits) bound what it runs. An [interrupt](./100-interrupts.md) holds
a script: no step runs, and no call it asks for goes out, until the resume.

## Files

A script reads the conversation's files by the id in their `[attachment …]`
line. The bytes never enter the model's context: the script reads them, and
only what it returns or prints goes back.

```javascript
const totals = {};
for await (const row of files.csv("sales.csv")) {
    totals[row.region] = (totals[row.region] ?? 0) + Number(row.amount);
}
return totals;
```

| Call | Gives |
|---|---|
| `files.list()`, `files.stat(id)` | `{ id, mime, size }` of each file, or of one, and `skill` for a skill's file |
| `await files.text(id)` | The whole file as text, up to an eighth of `memory` |
| `files.show(id)` | Nothing; the model sees the file when the script ends |
| `files.csv(id, { header, delimiter })` | Rows: objects keyed by the header, or arrays with `header: false` |
| `files.lines(id)` | Lines, without their line ends |
| `files.jsonl(id)` | One JSON value per line |
| `files.chunks(id, { size, offset })` | Text in pieces of about `size` bytes, from byte `offset` |

Each stream takes `{ limit }`, a count of records, and `break` ends it. The
files are the attachments on the branch where `run_code` was called.

The script's process decodes the files, so decoding counts toward its CPU and
memory. Its runner reads the bytes: the process has no file system or network,
and never sees where a file is stored. A file's bytes never change, so the log
records no read: each run reads its files again. Read files before calling
tools, so the reading happens once per run.

A file that is gone throws in the script. A store that fails sends the step
back to its queue, as a runner that is down does.

## Sandbox

Each run of a script is a new process that the engine starts from its own
binary. Before the process reads the script, it closes every file it
inherited, starts with no environment, drops to `nobody` when it runs as root,
and locks itself down: on macOS with a sandbox profile that denies everything,
and on Linux with a seccomp filter that has no call to open a file, open a
socket or start a process. A process that cannot lock itself down does not run
the script. The engine kills a process that runs past its deadline.

`SUBS_SANDBOX_SLOTS` sets how many scripts `subs` runs at once. The default is
the number of CPUs, from 2 to 8. A tenant may take every slot while no other
tenant waits; while another does, it keeps at most all but one.

## Run scripts somewhere else

A program that embeds the engine chooses what runs scripts with
`RuntimeConfig::script_runner`, an implementation of
`substructure_host::codemode::ScriptRunner`. A runner takes one step of a
script, a `Script`, and returns where it ended, a `Step`. It keeps nothing
between steps, so it can be a process here, another machine, or a service. The
engine still decides how many steps run at once, reads each step from the
session, and checks every call a step asks for.

A runner that cannot take a step now, because it is down, busy or of another
version, returns a `RunError`. The engine puts the step back on its queue and
tries it again a few seconds later, until the step's queue timeout. A script
that fails is a `Step::Failed`, not an error.

The default runner, `ProcessRunner`, starts this binary as a confined process
for each step. That binary must call `substructure_host::codemode::dispatch()`
first in `main`, before it starts a thread. Without it, every script fails
with an error that says so.

The process speaks JSON lines on stdin and stdout. It first writes
`{"ready": {"version": N, "oldest": M, "languages": [...]}}`, or
`{"unconfined": {"error": "..."}}` when it could not confine itself. Then it
reads one `Script` and writes one `Ran`. While it runs, it may ask for file
bytes with `{"read": {"file": "sales.csv", "offset": 0, "len": 262144}}`, and
its runner answers `{"bytes": n}` followed by `n` raw bytes, or `{"missing":
"…"}`. A runner reads a script's files from the `ScriptFiles` the engine passes it: each
file's blob and the store to read it from.

A runner on another machine can start the same binary with `__sandbox` the same
way, behind its own endpoint. Scripts that read no files need nothing more. For
files, the process that starts the binary answers its reads, so a runner on
another machine needs the blobs too: it reads them from the store, or the
engine answers the reads for it. The engine does not provide either yet. A
`Script` carries its version, and a runner takes every version from `OLDEST` to
`VERSION`, so an engine and its runners can be deployed one before the other.

Script steps wait on `RuntimeDeps::script_task_queue`.

## Add a language

Nothing outside three places knows which language a script is in:

- `CodeLanguage`, the enum `codemode` takes, in `session::codemode`.
- A dialect in `session::codemode`, which tells the model how to write
  `run_code` in that language and declares each tool in its types.
  `javascript.rs` declares tools in TypeScript.
- An engine in `host::codemode`, an implementation of `Engine`, listed in
  `engine.rs`. It interprets the language and asks `Replay` for each call the
  script makes. `Replay` answers from the script's records, holds it to its
  limits, and turns how it ended into a step, so an engine only binds the
  language: a call to a tool, printed output, the value returned, and an error
  raised. `quickjs.rs` is the JavaScript engine.

A runner process says which languages it runs when it starts, and a step in a
language it does not run is tried again elsewhere.

## Limits

A script has no network, file system, timer or module. The engine sets these
limits with `RuntimeConfig::script_limits`, and a runner holds each script to
them. An error names the limit that was met and what to do.

| Limit | Default | Applies to |
|---|---|---|
| `calls` | 1,000 | Calls over the whole script |
| `results` | 8 MiB | Results the script reads, over all its calls |
| `arguments` | 32 MiB | Arguments over the whole script, and 256 KiB in one call |
| `returned` | 64 KiB | The value it returns; past it, the start is shown with a note |
| `logs` | 16 KiB | `console.log` output |
| `memory` | 96 MiB | JavaScript heap in one run |
| `compute_ms` | 500 | CPU in one run |
| `total_compute_ms` | 10,000 | CPU over all of a script's runs |
| `file_bytes` | 64 MiB | File bytes one run reads |
| `total_file_bytes` | 512 MiB | File bytes over all of a script's runs |
| `streams` | 4 | Files open at once |
| `shown` | 64 KiB | File text the script shows the model |
| `shown_media` | 2 MiB | Media files the script shows the model |

A run reads the script, the results its calls already returned, and the files
it asks for, and stops at the first call with no result. Time spent waiting on
file reads does not count toward its deadline. So a run is quick, and a
run that needs more than half a second of CPU is almost always a runaway. A
script resumes from the start after its calls finish, so work done before a
call counts against each run and against the whole budget. A run's process is
stopped three times its CPU allowance and one second after it starts, and the
operating system stops it a second past its allowance. `RuntimeConfig::script_wait`, 60 seconds by
default, is how long a step may wait for a slot or for a runner to take it.
`ProcessRunner::with_data_limit` caps a process's heap, records included; keep
it above `memory`. Parsed JSON takes about seven times its size in heap, so keep
`results` under an eighth of `memory`.

Each step logs a `script step` line under the `codemode` target, with its
tenant, the bytes of results it was given, its wait, its run time, its heap and
CPU, and the limit it met, if any. A step that uses over half its memory or
compute logs a warning.
