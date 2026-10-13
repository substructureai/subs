---
title: Events URL
group: Building agents
---

An events URL gets everything that happens in an agent's sessions: each logged
event, in order, and the model's text as it streams. Use it to show a session
in your own app, to store it, or to act on it.

A worker decides what happens next. An events URL tells you what happened. One
program can serve both, at two paths.

```toml title="subs.toml"
[worker.app]
url = "https://app.example.com/agent"

[events.app]
url = "https://app.example.com/events"

[agent.bot]
worker = "app"
events = "app"
```

| Direction | What | Your app answers |
| --- | --- | --- |
| Engine → worker URL | "What next?" | Actions |
| Engine → events URL | "This happened", and live text | `2xx` |
| Your app → [machine API](./250-api.md) | Messages, approvals, reads | – |

## Which sessions send to it

Every session of an agent that names the block. A subagent's session sends to
its own agent's events URL. If its agent names none, it sends to its parent's.

A session keeps the events URL it started with. Changing `events` on an agent
applies to new sessions.

## Logged events

The engine POSTs batches of events. Each session's events arrive in order, one
batch at a time.

```json
{
  "type": "events",
  "session_id": "chat:room42",
  "agent_id": "bodi",
  "metadata": { "room": "room42" },
  "events": [
    { "seq": 41, "occurred_at": "…", "turn_id": "t9", "payload": { "type": "turn.started", "turn_id": "t9" } },
    { "seq": 42, "occurred_at": "…", "turn_id": "t9", "payload": { "type": "message.new", "message": { … } } }
  ]
}
```

The top of the POST holds what never changes in a session: `session_id`,
`agent_id`, the `metadata` that the submit set, and `ancestry` for a subagent's
session. Use `metadata` to know where the session lives. Each event holds its
`seq`, its time, its turn, and its [payload](./240-events.md). Skip the events
you do not need.

| Answer | What the engine does |
| --- | --- |
| `2xx` | The batch is delivered. The next one follows. |
| Anything else, or no answer within `timeout_secs` (10 by default, 15 at most) | It sends the same batch again later, with backoff. It writes `delivery.failed` to the session log. |

A `Retry-After` header in seconds sets the wait. The engine tries for up to
three days. Then it skips the event and writes `delivery.skipped`. While the
URL stays down, each later event gets one try and is skipped if it fails. The
first `2xx` ends this and writes `delivery.recovered`.

The same batch can arrive twice, for example after a crash before the engine
saved your answer. Use `session_id` and `seq` to drop a repeat.

A slow or failing events URL does not hold up model calls, workers, or other
sessions.

## Live text

While the model writes, the engine also POSTs its text, about four times a
second.

```json
{
  "type": "live",
  "session_id": "chat:room42",
  "agent_id": "bodi",
  "metadata": { "room": "room42" },
  "turn_id": "t9",
  "call_id": "llm-3",
  "attempt": 0,
  "chunks": [
    { "seq": 7, "text": "Logged 12" },
    { "seq": 8, "text": "0ml." }
  ],
  "done": false
}
```

- Apply a chunk only if its `seq` is higher than the last one you applied. A
  chunk that you did not confirm with a `2xx` comes again in the next POST.
- `done: true` comes with the last chunks of the call.
- A new `attempt` starts over at the first chunk. Replace the old text.
- `reasoning` carries the model's thinking the same way as `text`.
- A gap in `seq` means some text was lost. `turn.completed` holds the final
  reply.

Live text is best effort. It is not written to the log and is not sent again
after a restart. It can arrive before the logged events around it.

## Verify the signature

Each POST carries `X-Substructure-Signature: sha256=<hex>`, an HMAC-SHA256 of
the body, the same as a worker's request. Print the secret:

```sh
subs secret events.app
```

## Map places to sessions

Your app picks the session. Send your own `session_id` on the submit. The same
id reaches the same session.

| You want | `session_id` |
| --- | --- |
| One session per thread | `slack:T1:C9:1700.1` |
| One session per room | `chat:room42` |
| One session per person | `wa:+15551234` |

Do not use `/` in the id. It appears in URL paths.

Put where the session lives in `identity.metadata`. The engine keeps it for the
life of the session, sends it with every event, and copies it to subagent
sessions.

Two more inputs help a chat app:

- [`queue: "merge"`](./120-conversations.md#one-turn-at-a-time) puts the
  messages that arrive during a turn into one next turn.
- `client.state` on `client.append` carries data for the turn, such as the
  message it answers. It arrives on the first decision of the turn and on its
  `decision.queued` event.

## Chat apps

Each one follows the same shape: the platform's webhook reaches your app, your
app submits, and your events URL posts back to the platform.

| | Session id | Live text | Approvals |
| --- | --- | --- | --- |
| Slack | the thread, or the DM | edit or stream one message | block buttons |
| WhatsApp | the person | none: send at `turn.completed` | reply buttons or a list |
| Telegram | the chat or topic | edit one message | inline keyboard |
| Discord | the thread or channel | edit one message | buttons |
| SMS | the person | none: send at `turn.completed` | "reply YES or NO" |

For an approval, `session.interrupted` arrives with its `interrupt_id` and
options. Show them, then send `interrupt.resume` when a person picks one.

On a platform without threads, keep one session per person and reset it from
the worker after a quiet hour. See
[Reset or compact](./120-conversations.md#reset-or-compact).

## Next steps

- [Workers](./50-workers.md): the other half of an app.
- [Events](./240-events.md): every event an events URL can get.
- [Conversations](./120-conversations.md): merge messages into one turn, reset,
  and compact.
