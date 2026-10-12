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

## Reset or compact

A worker can shorten what the model reads. Its decision returns a short
`messages` view whose first message is new. The engine starts a new branch at
the root and moves the head there. The old messages stay in the tree, and the
model no longer reads them.

To reset, send only the new message:

```jsonc
{
    "messages": [{ "role": "user", "content": "Sam: had 120ml" }],
    "actions": [{ "type": "llm.call" }],
    "state": { "last_reset": "2026-10-11T14:02:00Z" }
}
```

To compact, send a summary first, then the messages to keep. The engine records
copies of the kept messages on the new branch. Each copy names its original in
`copied_from` on its `message.new` event.

```jsonc
{
    "messages": [
        { "role": "user", "content": "Summary: Sam logs feeds and naps." },
        { "id": "u40", "role": "user", "content": "nap at 2" },
        { "id": "a40", "role": "assistant", "content": "Logged." }
    ]
}
```

State and config written during the conversation stay on the old branch. Send
`state` again in the same decision, and `agent` if you changed it. The engine
writes them after the messages, so they land on the new branch. The config from
`session.start` holds on every branch.

Reset at the start of a turn, on `client.messages`. A new branch cancels work in
flight on the old one.

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
