---
title: System One models
group: Building agents
---

A System One model answers typed questions about some content. It returns
typed answers fast. It does not write text. Each answer comes with a
confidence.

Your worker asks these questions in the middle of a turn. The engine runs the
call, saves it, and sends the answers back as a decision. The engine never asks
on its own.

## Declare a block

Declare a `[system1.<id>]` block, the same way you declare an LLM.

```toml title="subs.toml"
[system1.typesafe]
type = "typesafe"
model = "jev-latest"

[system1.cf]
type = "workers-ai"
model = "clef-flash"
account_id = "0123abcd…"
```

| `type` | Calls | Key |
| --- | --- | --- |
| `typesafe` | TypeSafe's `POST /v1/systemone`. | `$TYPESAFE_API_KEY` |
| `workers-ai` | Cloudflare Workers AI, for example Clef. | `$CLOUDFLARE_API_TOKEN` |
| `http` | Any endpoint that takes and returns the same shapes as TypeSafe. | none, or `api_key_env` |

The key stays in the engine. Your worker never holds it.

Use `http` for a model you host yourself, or for fixed answers in tests.

```toml title="subs.toml"
[system1.local]
type = "http"
url = "http://localhost:8080/v1/systemone"
model = "my-model"
```

On a deployment, upload the key with `subs auth system1.<id>`. A `workers-ai`
block there names its `account_id`, and an `http` block's `url` must be a
public https address.

## Ask a question

Return a `system1.call` action. Each question has a name and a type.

```json
{
    "type": "system1.call",
    "id": "spam:turn_1",
    "questions": {
        "spam": { "type": "noul", "instructions": "Is this message spam?" }
    }
}
```

- Without `id`, the engine makes one.
- Without `system1`, the call runs on the only block the file declares.
- Without `model`, the call uses the block's `model`.
- Without `state`, the model reads the conversation as a list of roles and
  text.

Send your own `state` to ask about something else. It is a string, an object,
or an array.

```json
{
    "type": "system1.call",
    "system1": "cf",
    "model": "clef",
    "state": "I was charged twice. Please help.",
    "questions": {
        "team": {
            "type": "choice",
            "instructions": "Which team handles this?",
            "criteria": { "billing": "Money and invoices", "technical": "Bugs and outages" }
        }
    }
}
```

## Get the answers

When the call ends, your worker gets a `system1.finished` decision.

```json
{
    "type": "system1.finished",
    "id": "spam:turn_1",
    "ok": true,
    "model": "jev-1.13",
    "answers": { "spam": { "type": "noul", "noul": 0.03 } },
    "usage": { "input": 120, "output": 12, "uncached_input": 120, "cache_read": 0, "cache_write": 0, "total": 132 }
}
```

A failed call has `ok: false` and an `error`.

The engine proposes nothing for this trigger. Your worker decides what happens
next. To go on with the turn, return an `llm.call`. To end it, return `done`.

## Question types

| `type` | Asks | Answer |
| --- | --- | --- |
| `noul` | Is it true? | `noul`: the probability of yes. |
| `choice` | Which option? `criteria` maps each option to when it applies. | `choice`, `confidence`, `probabilities`. |
| `score` | Where on a scale? `criteria` lists the levels in order. | `score`, `confidence`, `legend`, `probabilities`. |
| `box` | Where in the image? Needs an image in `assets`. | `box` edges and `confidence`. |

An answer keeps every field the provider sends, including fields the engine
does not name.

The engine knows what some models answer. It checks the question types and the
limits before it makes the call.

| Block and model | Types | Limits |
| --- | --- | --- |
| `typesafe`, `jev-*` | all four | none |
| `workers-ai`, `clef` and `clef-flash` | `noul`, `choice`, `score` | Each question needs `instructions`, and 2 or more options or levels. Question names are 1 to 100 letters, digits, `_`, `.` or `-`. At most 4 images. |

A call with more than 64 questions to Clef goes out as several requests. You
get one `system1.finished` with all the answers.

For any other model, the engine sends the call and the provider decides. To
have the engine check a new model, list its types on the block.

```toml
[system1.cf]
type = "workers-ai"
model = "clef-2"
questions = ["noul", "choice", "score"]
```

## Images

An asset is an image from the session, or a `data:` URI. Its key is a
placeholder. Use the key in the state, the instructions, or the criteria.

```json
{
    "type": "system1.call",
    "state": "Receipt: img-1",
    "assets": { "img-1": { "type": "image", "uri": "blob://…" } },
    "questions": { "total": { "type": "box", "instructions": "The total." } }
}
```

The engine stores a `data:` URI as a blob, so the log holds a reference and not
the bytes.

Clef does not read placeholders. It puts the images before the state, in the
order of their keys.

## Guard a tool

Ask before a tool runs. The tool call waits, the same as an
[async tool](./110-async-tools.md).

```javascript title="server.mjs"
if (trigger.type === "tool.execute" && trigger.name === "refund") {
    pending[trigger.id] = trigger;
    return {
        actions: [{
            type: "system1.call",
            id: `guard:${trigger.id}`,
            questions: { asked: { type: "noul", instructions: "The user asked for this refund." } }
        }]
    };
}

if (trigger.type === "system1.finished" && trigger.id.startsWith("guard:")) {
    const call = pending[trigger.id.slice("guard:".length)];
    if (trigger.ok && trigger.answers.asked.noul >= 0.9) {
        return { actions: [{ type: "tool.result", id: call.id, result: runRefund(call) }] };
    }
    return { actions: [{ type: "tool.error", id: call.id, error: "The user did not ask for this." }] };
}
```

## What the engine does for you

- It saves each call and its answers in the session log. After a crash it does
  not run a finished call again.
- It retries rate limits and server errors. Set the policy with
  `[agent.<id>.retry] system1`. See [Retries](./210-retries.md).
- It stops an open call when the turn ends, when the run is cancelled, or when
  the branch changes.
- An open call keeps the turn open.
- It adds the call's cost and tokens to the turn and the session.
  `turn_calls.system1` on `turn.completed` shows them apart from the model's.
  Set the price per million input tokens on the block.

```toml
[system1.cf]
type = "workers-ai"
model = "clef-flash"
price = { input = "0.038" }   # Cloudflare's price for clef-flash. clef is 0.24.
```

## Next steps

- [Workers](./50-workers.md): answer decisions with your own code.
- [Protocol](./230-protocol.md): every field of `system1.call` and
  `system1.finished`.
- [Config](./220-config.md#system1id): every key of a `[system1.<id>]` block.
