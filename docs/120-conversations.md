---
title: Conversations
group: Building agents
---

A session's history is a tree of messages. The engine saves it. You do not build
a chat backend.

To edit, regenerate, or branch, a client sends its view of the conversation
again. The engine merges that view into the tree.

## The message tree

Each message is a node with a parent. The head is the active leaf. The path from
the head to the root is the conversation that the model sees.

A decision request holds that path as `messages`.

The engine keeps the active branch in memory. Every other branch stays in the
session log. When a view names a message on another branch, the engine loads
that branch again. The [client API](./250-api.md) returns the whole tree.

## Submit input

A client sends input in one of three ways.

| Input | Effect |
| --- | --- |
| `client.message` | Adds one message at the head. |
| `client.append` | Adds several messages at the head. It never branches. Use it to sync an outside conversation. |
| `client.messages` | Sends the full conversation view. The engine merges edits and branches into the tree. |

The engine matches a `client.messages` view against the tree by message `id`.
Known IDs at the start match existing nodes. The first message that is new or
has no ID starts a branch. `new_from` on the trigger gives its index.

## Edit and branch

To edit, send the view again with an earlier message changed. Give that message
a new ID, or no ID.

The engine starts a branch at the last matching node, records the changed
message and everything after it, and moves the head to the end. The original
branch stays in the tree.

To regenerate, send the view without the last assistant reply. The engine
records a new reply beside the old one.

Sending a message again under its existing ID changes nothing. The engine
matches on ID, not on content.

A view that stops at an earlier node writes nothing. It moves the head to that
node and emits `head.moved`. The next reply branches there, and the engine
cancels work in flight on the branch that you left. This is how a client changes
branches.

## Example: edit an earlier message

An active path of `u1 → a1 → u2`. The client edits back to the first question.

```jsonc
{
    "type": "client.messages",
    "agent_id": "bot",
    "messages": [
        { "id": "u1", "role": "user", "content": "weather in NYC?" },
        { "role": "user", "content": "weather in LA instead" }
    ]
}
```

`u1` matches. The message with no ID starts a branch under it. The worker sees
`messages: [u1, e1]` with `new_from: 1`. The `a1 → u2` branch stays.

## Make room

A model reads a limited number of tokens. Before a model call, the engine
checks whether there is room. When there is not, it holds the call and asks the
worker with a `context.limit` decision.

The engine counts what the model would read. It takes the input tokens that the
provider reported for the last call, and adds about one token for every four
characters written since.

### Strategies

The agent lists strategies in `[agent.<id>.context]`. The engine tries them in
order. The first that applies sets the plan in `proposed`.

```toml
[agent.bot.context]
window = 200000

[[agent.bot.context.strategy]]
use = "reset"
idle_secs = 3600

[[agent.bot.context.strategy]]
use = "summarize"
```

Here an hour with no turn starts the model over. Otherwise, when the model would
read more than 184,000 tokens, the engine summarizes.

| Strategy | `proposed` holds |
| --- | --- |
| `reset` | `context.replace` with only the turn's own messages, then the held call on them. |
| `summarize` | An `llm.call` that summarizes the older messages. About `keep_tokens` of recent messages, from a user message, stay as they are. |
| `worker` | The held call, as it was. Change it or replace it. |

`window` is the model's context window. Set it to what the model reads. A
strategy without `idle_secs` applies when the model would read more than the
window less 16,000 tokens, or more than its `over_tokens`. Without a window,
only `idle_secs`, `over_tokens` and a provider's refusal apply.

`idle_secs` applies only at a turn's first model call. See
[`[agent.<id>.context]`](./220-config.md#agentidcontext) for every key.

### Answer `context.limit`

```jsonc
{
    "type": "context.limit",
    "reason": "full",           // "idle", "full", or "overflow"
    "strategy": "summarize",
    "tokens": 186400,
    "window": 200000,
    "call": { "id": "llm-7", "llm": "claude", "request": { ... }, "stream": true }
}
```

`call` is the held call. An `llm.call` with its `id` runs it again. The engine
does not hold it a second time.

| Answer | Then |
| --- | --- |
| `context.replace`, then `call` | The engine sets the history, and the call runs on it. |
| An `llm.call` with another id | That call writes the summary. Its `llm.finished` proposes `context.replace` with the summary and the kept messages, then `call` on them. |
| `call` alone | The history stays. The engine does not ask again in that turn. |

The summary is not added to the conversation. When the summary call fails, the
engine proposes `call` as it was. When the provider finds the summary call too
long, the engine proposes a shorter one.

A `summarize` strategy takes `prompt` for its own instructions, and `llm` and
`model` to write the summary elsewhere. An earlier summary is part of what the
next one reads.

### When the provider refuses the call

A provider can refuse a call as too long. The engine then asks with
`reason: "overflow"` and a summary in `proposed`. The call runs once more. If
the provider refuses it again, the turn fails.

### Replace the history at any time

Any decision can return `context.replace`. Use it to reset on a deploy, or for
your own compaction.

```jsonc
{
    "actions": [
        {
            "type": "context.replace",
            "summary": "Sam logs feeds and naps.",
            "messages": [
                { "id": "u40", "role": "user", "content": "nap at 2" },
                { "id": "a40", "role": "assistant", "content": "Logged." }
            ]
        },
        { "type": "llm.call" }
    ]
}
```

The engine starts a new branch at the root. `summary` comes first, as a user
message that says the earlier conversation was summarized. A message with a
recorded `id` becomes a copy. Its `message.new` event names the original in
`copied_from`. Any other message is new.

The worker's state and the config carry over to the new branch. The old
messages stay in the tree, and the model no longer reads them. An `llm.call`
without `messages` after the replace reads the new history.

### Show it in a UI

| Event | Show |
| --- | --- |
| `context.limited` | The engine is making room. |
| `context.replaced` | Room was made. |
| `context.kept` | The history stays. |

A model call between `context.limited` and the next of the other two writes the
summary. Do not show its text as an answer. The summary that the engine
proposes is not streamed.

## One turn at a time

A session runs one turn at a time. A submit that arrives during a turn is
refused with `409` and names the turn.

Send `queue: true` to wait instead. The engine holds the message and starts it
when the running turn completes. A queued message sees the reply it waited for,
so it reads as a follow-up question. Queued turns run in the order they arrived.
Only `client.message` and `client.append` take the flag.

Send `queue: "merge"` to put messages that arrive during a turn into one next
turn. Use it for a chat where people send several short lines.

```
turn running...      "Sam had 120ml"      "oh and a nap at 2"
turn ends       ->   one turn with both lines
```

Each merged submit gets back the same `turn_id`. If two submits set
`client.state`, the later one wins. Give each message an `id`, so that a resent
message is not added twice. A merge does not join a turn that was queued with
`queue: true`, so turns still run in the order they arrived.

To redirect an agent that is working, [interrupt](./100-interrupts.md) it first,
then submit.

## Next steps

- [Agent state](./90-state.md): the engine reads state along the active path.
- [AG-UI](./140-ag-ui.md): do this from a browser chat UI.
- [Protocol](./230-protocol.md): messages, the tree, and client inputs.
