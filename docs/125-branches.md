---
title: Branches
group: Building agents
---

A branch is a path in the conversation's tree that is not the conversation. Use
one for work beside the conversation: a title, a memory to save, a judge, a
router, a draft, research.

A model call reads one path in the tree, and its reply lands at the end of that
path. The conversation is the path the head points to. A call on a branch reads
the branch, and its reply never reaches the conversation, a client, or Slack.

## Start a branch

Add `branch` to an `llm.call`. Its `messages` go after `from`, and the call
reads that path.

```json
{
  "type": "llm.call",
  "branch": { "id": "title", "from": "head", "mode": "detached" },
  "messages": [{ "role": "user", "content": "Write a five-word title for this chat." }]
}
```

| `from` | The branch reads |
| --- | --- |
| `"head"` | The conversation, after this response's messages, then its own. It shares the conversation's cached prompt. |
| a node id | The path to that node, then its own. |
| absent or `null` | Only its own messages. |

A fork runs as the config of the path it forks. A root runs with no config: the
model reads its messages as written, with no system prompt, tools, or context.
`agent` runs a branch as another config: a registered agent's id, or a config.

What a branch inherits, it reads as it was when the branch started. A later
change to the conversation's config or state does not reach it.

`id` is optional. A branch already started is not started again. `name` is a
label for your worker and the dashboard. The engine never reads it.

A branch cannot fork at a message whose tool calls are unanswered.

## Decide on a branch

When a branch's call ends, the engine asks your worker as it does for the
conversation. The request carries `branch`, and every field is read on it:
`messages` is the branch's path, and `agent`, `state` and `calls` are the
branch's.

```json
{
  "trigger": { "type": "llm.finished", "id": "...", "ok": true, "message": { "...": "..." } },
  "branch": { "id": "title", "mode": "detached", "from": "m4", "leaf": "m9" },
  "messages": [ "... the conversation up to m4, then the branch ..." ],
  "turn": null
}
```

The response applies to the branch:

- `messages` extend the branch. A view that leaves the branch's path moves
  the branch's leaf where it leaves. The head does not move.
- An `llm.call` without `branch` goes on on the branch.
- Tool calls run on the branch, and their results land there.
- `done` ends the branch with its `data`.
- `fail` ends the branch as failed. The run goes on.

`proposed` works the same way: return it, and a branch runs to its answer and
ends with the reply as its `data`.

A decision that leaves a branch nothing to wait for ends it, as a quiet turn
ends.

## Hear back

When a branch ends, whoever started it gets `branch.finished`.

```json
{
  "type": "branch.finished",
  "id": "title",
  "mode": "detached",
  "ok": true,
  "data": "Debugging the failed deploy",
  "from": "m4",
  "leaf": "m9",
  "stale": false
}
```

`data` is what `done` carried. A failed branch has `ok: false` and `error`.
`stale` is `true` when the node it forked at is no longer on the conversation,
after an edit or a reset.

## Blocking and detached

| `mode` | The branch |
| --- | --- |
| `blocking` (the default) | Belongs to the turn that started it. The turn waits for it, and its cost is the turn's. It ends with the turn: on `done`, a cancel, a failure, or an edit. |
| `detached` | Outlives the turn. Its cost is the session's only. A cancel does not stop it. `branch.finished` arrives with `turn: null`, and proposes nothing. |

After a blocking branch, `proposed` goes on from the conversation's last message:
a model call for a user message or a tool result, `done` for a reply.

A tool that needs approval fails on a detached branch: nobody is there to give
it.

## Stop or keep a branch

```json
{ "type": "branch.close", "id": "draft-b" }
{ "type": "branch.promote", "id": "draft-a" }
```

`branch.close` stops a branch, and cancels its calls. `branch.promote` makes a
branch the conversation: the head moves to its newest node. A branch that
finished, or one with no call out, can be promoted. A conversation response
never lands on a branch: a branch message in its view is copied into the
conversation.

Best of two:

```json
{ "actions": [
  { "type": "llm.call", "branch": { "id": "draft-a", "from": "head" } },
  { "type": "llm.call", "branch": { "id": "draft-b", "from": "head" }, "model": "other-model" }
] }
```

Each `branch.finished` arrives in turn. Judge the two, then promote the winner.

## Calls with your own prompt

An `llm.call` with `messages` and no `branch` runs on a branch of its own, with
no config. The model reads the messages as written. The branch forks where its
leading messages already are in the tree, so only the rest are stored.

Its reply stays on the branch, and reaches the conversation as
`branch.finished`. To put it in the conversation, write it there.

## Parallel branches

The engine asks about one decision at a time. Set `parallel_branches = true` on
the agent and each branch waits in its own line: the worker is asked about one
branch while another, or the conversation, is still out.

```toml title="subs.toml"
[agent.bot]
llm = "claude"
model = "claude-sonnet-4-5"
parallel_branches = true
```

Your worker then gets more than one request for a session at a time. An
interrupt holds only the branch it was raised on.

## Events

| Event | Meaning |
| --- | --- |
| `branch.started` | `{ id, from?, parent?, agent, name?, mode, turn_id?, subagent?, leaf? }` |
| `branch.closed` | `{ id, reason, data?, error? }`. `reason` is `done`, `failed`, `voided`, `closed`, or `promoted`. |
| `message.new` | With `branch`, a branch message. It does not move the head. |

## Next steps

- [Subagents](./80-subagents.md) on a branch.
- [Make room](./120-conversations.md#make-room): summaries run on a branch.
- [Protocol](./230-protocol.md)
