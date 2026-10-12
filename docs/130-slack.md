---
title: Slack
group: Frontends
---

Mention the bot in a channel and it answers in the thread. DM it and it answers
every message.

The thread is the session. Later mentions continue the conversation.

## Set up the bot

Each agent gets its own Slack app. Declare it, create it in Slack, then give
the deployment the two values Slack hands back.

```toml title="subs.toml"
[agent.support.slack]
name = "Support"
```

```sh
subs apply
subs auth agent.support.slack
```

`subs auth` prints an app manifest. Go to
[api.slack.com/apps](https://api.slack.com/apps), choose Create New App, then
From a manifest, and paste it in. Create the app and install it to your
workspace.

Slack then shows you two values. Paste them back when the command asks for
them.

| Value | Where Slack shows it |
| --- | --- |
| Bot User OAuth Token, starting `xoxb-` | OAuth & Permissions |
| Signing Secret | Basic Information |

The bot answers as soon as both are in. Run `subs list` to see which agents
still need theirs.

To replace the two values later, run `subs auth` again. To remove them, run
`subs revoke agent.support.slack`. The app itself stays in Slack, and the same
manifest still works, so you can put new values in without creating it again.

An engine you run yourself uses the same block and a different pair of values.
See [Self-hosting](./180-self-hosting.md#slack).

## Where the bot answers

Invite the app to a channel and it answers mentions there. Remove it and it
stops. There is no channel list in `subs.toml`.

DMs work as soon as the app is installed.

To keep an agent to one or the other, set `answers`.

```toml title="subs.toml"
[agent.support.slack]
name = "Support"
answers = "dm"
```

| Value | The bot answers |
| --- | --- |
| `both` | DMs, and mentions in any channel it is in. This is the default. |
| `dm` | DMs only. |
| `channels` | Mentions only. |

`answers` decides which permissions the app asks Slack for. An app set to `dm`
is never given permission to read channels, so a mention does not reach it even
if someone invites it to one.

Changing `answers` changes the manifest. Apply the file, then update the app in
Slack from the new manifest that `subs auth` prints.

## Naming the app

```toml title="subs.toml"
[agent.support.slack]
name = "Support"
description = "Answers customer questions"
```

`name` is what the bot is called in Slack, up to 35 characters. Without it the
agent ID is used. `description` is the app's About text, up to 140 characters.

Both come from the manifest, so changing them means updating the app in Slack.

## Several bots in one workspace

Every agent with an `[agent.<id>.slack]` block is a separate Slack app, with
its own name, icon, and DMs. Run `subs auth` once for each.

```toml title="subs.toml"
[agent.support.slack]
name = "Support"

[agent.oncall.slack]
name = "On-call"
```

Two of your bots can sit in the same channel. Each answers only when it is
mentioned by name, and each keeps its own conversation in a thread.

## How Slack maps to the engine

| Slack | Engine |
| --- | --- |
| A thread, in a channel or a DM | Session `slack:{agent}:{channel}:{thread_ts}` |
| A mention or a DM message | One turn |
| A task card | A tool call or a subagent run |
| A reply | The turn's result |

A turn is one Slack message. The message opens with the first task card. More
cards stream in as the turn makes tool calls. The result completes the message.

A thread shows one open message at a time. A queued turn waits for the turn
ahead of it.

Each tool call and subagent run shows as a
[task card](https://docs.slack.dev/reference/block-kit/blocks/task-card-block/).
Cards are collapsed by default. After the turn finishes, each card shows its
arguments and its result.

## Interrupt prompts

An interrupt with a `message` in its payload posts that message with buttons.
This is how a worker asks a person something from Slack.

The payload is an [AG-UI Interrupt](https://docs.ag-ui.com/concepts/interrupts).
Put the spec's fields at the top level. Put everything else under `metadata`.

```json
{
  "message": "Run `send_email`?",
  "toolCallId": "tc_1",
  "expiresAt": "2026-07-24T18:00:00Z",
  "metadata": {
    "options": [
      { "label": "Approve", "value": { "decision": "approve" }, "style": "primary" },
      { "label": "Deny", "value": { "decision": "deny" }, "style": "danger" }
    ]
  }
}
```

`message` is mrkdwn. Each entry in `metadata.options` becomes a button.
`expiresAt` shows as a footnote. An interrupt with no `message` posts
`Paused: {reason}`.

The engine delivers `metadata` to AG-UI clients unchanged. Every client can read
it, so keep anything private in worker state.

Put the routing hint in the interrupt's `reason`: `tool_call`,
`input_required`, `confirmation`, or your own namespaced value.

### What a click does

A click answers the question. The prompt then loses its buttons and says who
answered and which option they picked. A `danger` button records a decline. Any
other button records an approval. Work after that goes into a new message.

A click cannot send anything that the prompt did not offer, and a second click
changes nothing.

For the worker side, what a click resumes with and how to answer one yourself,
see [Interrupts](./100-interrupts.md#answer-a-prompt) and the
[`node-hono-tool-approval`](../examples/node-hono-tool-approval) example.

### Send a message while a prompt is open

The engine refuses a message while the session is paused, because the prompt
needs an answer first. The bot posts the open question again instead of staying
silent, so any link in the question comes back with it.

## Authorize a connection

The bot raises a prompt of its own when a connection has no usable credential.
It names the connection, says what happened to its credential, and links to the
page where a person authorizes it. See
[Connectors](./40-connectors.md#when-a-credential-stops-working).

The link appears when the file has a `[remote]` pinned to a project on the
hosted cloud, which is the only deployment whose dashboard address follows from
its API address. Anywhere else the prompt names `subs auth <path>`, which is
what an operator runs on the machine where the engine runs.

`Retry` fetches the connection's tools again, and the turn continues with them.
If the credential is still not good, the same question comes back. The engine
raises one prompt per connection however many times a decision runs, so nobody
is asked twice.

## Add your own buttons

The bot draws its messages from the session's events. To show more, give the
agent an [events URL](./55-events-url.md) and post to the thread from your app
with the bot token. The events carry the thread in `metadata.slack_channel` and
`metadata.slack_thread_ts`.

A button you post comes back to the worker as a `client.action` decision, named
after its `action_id`. Its `args` carry the `value`, the user, and the message
it came from. Nothing about a click expires: answer it with an `llm.call` days
later, and the work opens a new turn.

See [node-hono-slack-skills](../examples/node-hono-slack-skills) for a worker and
events URL in one program.

## Thread context

The bot reads what it missed. Messages sent between mentions reach the agent at
the next mention. Users appear as `<@U…>: text`.

The bot needs `channels:history` and `im:history` to do this. Without them it
appends the new message alone, with a note that context might be missing.

## Attachments

A file uploaded with a message reaches the agent. The bot downloads it with the
bot token and stores it in the database. The prompt carries a reference, and the
bytes go to the model at the call. This needs `files:read`.

What the model receives depends on the type.

| Type | Size limit | The model receives |
| --- | --- | --- |
| PNG, JPEG, GIF, WebP | 5 MB | An image |
| PDF | 10 MB | A document |
| Text: CSV, JSON, Markdown, code, logs | 1 MB | A file |
| Audio | 10 MB | Audio |
| Video | 20 MB | Video |
| Anything else | none | A note naming the file |

An image the agent produces goes back the other way. The bot uploads it to Slack
once per workspace and the reply embeds it. This needs `files:write`. An image
that Slack refuses becomes a note in the reply. The text always arrives.

## Point a session at a thread

A session belongs to Slack because of the `slack_channel` and `slack_thread_ts`
owner metadata. The shape of the session ID does not matter.

An API client can set the same metadata on its first submit. That session's
turns then go into the thread like any other.

A click on a reply goes back to the session that posted it. Each reply carries
its session ID.

## Next steps

- [Interrupts](./100-interrupts.md): the pause behind a prompt.
- [Conversations](./120-conversations.md): the session behind a thread.
- [Self-hosting](./180-self-hosting.md#slack): run the bot on your own engine.
