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
code_mode = true
```

A worker sets `code_mode: true` on the agent config.

## What the model writes

`run_code` takes `code`, the body of an async function.

```javascript
const prs = await tools.github__list_prs({ repo: "acme/api", state: "open" });
const stale = prs.filter((pr) => Date.parse(pr.updated_at) < Date.now() - 14 * 864e5);
const reviews = await Promise.all(stale.map((pr) => tools.github__get_reviews({ number: pr.number })));
return stale.map((pr, i) => ({ number: pr.number, reviews: reviews[i].length }));
```

The description of `run_code` declares each tool the script can call, with its
input type and, when the tool declares an output schema, its result type. A
deferred tool is not declared. The script finds it with `tools.tool_search` and
calls it with `tools.call_tool`.

A call returns the tool's result, parsed when it is JSON. A call that fails
throws an `Error` with the tool's message.

## What a script can call

A script can call the agent's worker tools and connector tools, `tool_search`,
`call_tool` and `skill`. It cannot call:

- a tool that needs approval,
- a client tool,
- a subagent,
- `run_code`.

A call to one of these throws, and the script can catch it.

## How a run works

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

## Sandbox

Each run of a script is a new process that the engine starts from its own
binary. Before the process reads the script, it closes every file it
inherited, starts with no environment, drops to `nobody` when it runs as root,
and locks itself down: on macOS with a sandbox profile that denies everything,
and on Linux with a seccomp filter that has no call to open a file, open a
socket or start a process. A process that cannot lock itself down does not run
the script. The engine kills a process that runs past its deadline.

`SUBS_SANDBOX_SLOTS` sets how many scripts `subs` runs at once. The default is
the number of CPUs, from 2 to 8. One tenant uses at most all slots but one.
A script also reads at most 8 MiB of results in all; past that, its calls
throw.

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
`{"ready": true, "version": N, "oldest": M}`, then reads one `Script` and
writes one `Step`. A runner on another machine can start the same binary with
`__sandbox` the same way, behind its own endpoint, and needs no other code. A
`Script` carries its version, and a runner takes every version from `OLDEST` to
`VERSION`, so an engine and its runners can be deployed one before the other.

Script steps wait on `RuntimeDeps::script_task_queue`.

## Limits

A script has no network, file system, timer or module. Each run has a memory
limit, a stack limit and an instruction budget. A run that goes over a limit
fails, and `run_code` returns the error. A script can make at most 1,000
calls. A result over 32 KiB is replaced with a note, and `console.log` output
over 16 KiB is cut.
