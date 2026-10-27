---
title: Events
group: Reference
---

A session is an event log. The engine appends an event before it acts, and
streams every event to any client that is watching.

Use events to render a conversation, to follow a turn, and to monitor a run.

## The stream

```
GET /api/client/sessions/{session_id}/events/stream
```

The response is `text/event-stream`. Each frame carries three fields.

| Frame field | Holds |
| --- | --- |
| `id` | The event's `seq`. |
| `event` | The event type. |
| `data` | An `Event` object. |

```
id: 7
event: message.new
data: {"session_id":"0193a1","seq":7,"occurred_at":"2026-07-14T10:00:03Z","payload":{"type":"message.new","message":{"id":"m2","role":"assistant","content":"Hi"}}}
```

To resume after a drop, pass `?after_seq=<n>`. `EventSource` does this for you.
On reconnect the browser sends the last frame ID in a `Last-Event-ID` header,
which wins over the query parameter.

Each stream covers one session. A subagent is its own session with its own
stream. Open a second stream with the child's ID to watch it.

## Event shape

```typescript
type Event = {
    session_id: string
    seq: number
    occurred_at: string         // RFC 3339
    turn_id?: string            // the turn running when this happened
    payload: EventPayload       // tagged by `type`
}
```

`seq` orders the log. It starts at 1 and never repeats.

## Conversation

Render a chat from these.

| Event | Payload |
| --- | --- |
| `session.created` | The session exists. |
| `message.new` | A message joined the tree: `{ message, parent_id?, copied_from? }`. `copied_from` names the original of a copy. |
| `head.moved` | The active leaf changed. A client branched or edited. |
| `turn.started` | A turn began: `{ turn_id, metadata? }`. `metadata` is the `metadata` the turn was submitted with. For a merged turn, a later submit's key wins. |
| `turn.completed` | A turn ended: `{ turn_id, data?, turn_cost, turn_token_usage, turn_calls, turn_started_at?, error? }`. See [Turn calls](#turn-calls). |
| `session.done` | The session finished its work. |
| `session.cancelled` | The current run was cancelled. A `turn.completed` with the error code `cancelled` follows. The session stays usable. |

### Turn calls

`turn_calls` on `turn.completed` totals the calls that the turn made, for each
kind of call:

```json
{
  "llm":      { "count": 6,  "errors": 0, "cost": "0.0101", "tokens": { "input": 41000, "output": 900, "…": "…" } },
  "tool":     { "count": 14, "errors": 2 },
  "subagent": { "count": 1,  "errors": 0, "cost": "0.0022", "tokens": { "…": "…" } },
  "system1":  { "count": 2,  "errors": 0, "cost": "0.00006", "tokens": { "input": 400, "output": 6, "…": "…" } }
}
```

- `count` is the number of calls. A retry is not a new call.
- `errors` is the number of attempts that failed, including the ones that were
  retried.
- `cost` and `tokens` are present only when they are not zero.
- A subagent call costs what its child spent to answer it. The child's own
  calls are in the child's `turn.completed`. `subagent.started` gives the
  child's session.

`turn_cost` and `turn_token_usage` are the totals of all kinds.
`turn_started_at` is when the turn started.

## Model calls

| Event | Meaning |
| --- | --- |
| `llm.call.requested` | A call was recorded. |
| `llm.call.dispatched` | The call went out. |
| `llm.call.completed` | The call returned. |
| `llm.call.errored` | The call failed. |

Partial output arrives between events as `llm.token.delta` frames.

```
event: llm.token.delta
data: {"type":"llm.token.delta","session_id":"0193a1","call_id":"llm-1","seq":3,"text":"Hi"}
```

These carry a [`StreamDelta`](./230-protocol.md#streaming). The engine does not
record them in the log.

## Tool calls

| Event | Meaning |
| --- | --- |
| `tool.call.requested` | The model asked for a tool: `{ id, name }`. |
| `tool.call.dispatched` | The call went to its handler. |
| `tool.call.completed` | The call returned: `{ id, result }`. |
| `tool.call.errored` | The call failed: `{ id, error }`. |

## Subagents

| Event | Meaning |
| --- | --- |
| `subagent.requested` | The model called a child agent. |
| `subagent.dispatched` | The spawn went out. |
| `subagent.started` | The child session exists. |
| `subagent.turn_completed` | The child's turn ended. |
| `subagent.errored` | The child failed. |

The `id` on each of these is the model tool call the child answers.
`subagent.requested` also carries `session_id`, the child session. Open a
stream with that to watch the child.

## Connectors

| Event | Meaning |
| --- | --- |
| `connector.sync.requested` | The engine is fetching a connection's tools. |
| `connector.sync.completed` | The tools arrived. |
| `connector.sync.errored` | The fetch failed. The turn runs without those tools. |

## Interrupts

| Event | Payload |
| --- | --- |
| `session.interrupted` | `{ interrupt_id, origin, reason, payload, anchor? }`. `anchor` is the head it pauses. Without it, every branch pauses. |
| `session.interrupt_resumed` | `{ interrupt_id, payload }`. |

## State and config

| Event | Meaning |
| --- | --- |
| `worker.state.updated` | A decision wrote new state. |
| `agent.updated` | A decision wrote a new config. |

## Context

See [Make room](./120-conversations.md#make-room).

| Event | Payload |
| --- | --- |
| `context.limited` | A model call waits for room: `{ reason, strategy?, tokens, window?, call }`. A `context.limit` decision follows. |
| `context.replaced` | The history was set on a new branch: `{ reason?, summary?, kept, dropped }`. `summary` holds `{ id, text }`. |
| `context.kept` | The held call runs on the history as it is. |

A model call between `context.limited` and the next `context.replaced` or
`context.kept` writes the summary. Its text is not an answer.

## Decisions

Engine bookkeeping. Useful for monitoring.

| Event | Meaning |
| --- | --- |
| `decision.queued` | A decision is waiting for the slot. |
| `decision.dispatched` | The request went to the worker. |
| `decision.completed` | The worker answered. |
| `decision.errored` | The decision failed. |
| `decision.dropped` | The decision was abandoned. |
| `call.voided` | A call was canceled before it settled. |
| `session.message_requested` | A `message.send` targeted another session. |

## Delivery

The engine hands events to its own readers: the session list, messages to
other sessions, Slack, and the agent's [events URL](./55-events-url.md). A reader that fails retries with backoff, and each failure is
logged. These events go to no reader.

Work goes to queues. Work that cannot be queued waits and goes out again as
the same attempt.

| Event | Payload |
| --- | --- |
| `work.unsent` | `{ kind, id, attempt?, error, retry_at }`. The work goes out again at `retry_at`. |
| `delivery.failed` | `{ subscription, seq, attempt, error, retry_at }`. A retry follows at `retry_at`. |
| `delivery.recovered` | `{ subscription, through, attempts }`. Delivery works again. |
| `delivery.skipped` | `{ subscription, seq, attempt, error }`. The reader gave up on that event. |
| `delivery.parked` | `{ subscription, seq, attempt, error }`. The reader stopped until it is resumed. |
| `delivery.resumed` | `{ subscription }`. A parked reader runs again. |

## Branches

Engine bookkeeping. The engine keeps the active branch in memory and loads other
branches from the log when a view needs them. These events go to no reader.

| Event | Meaning |
| --- | --- |
| `branch.loaded` | A branch came back into memory from the log. |
| `branch.unloaded` | Branches off the active path left memory. They stay in the log. |

## AG-UI

The AG-UI endpoints send AG-UI protocol events instead of these. See
[AG-UI](./140-ag-ui.md).

## Next steps

- [REST API](./250-api.md): the endpoints that serve these streams.
- [Durability](./200-durability.md): why the log is the source of truth.
- [Conversations](./120-conversations.md): the tree that `message.new` builds.
