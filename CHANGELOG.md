# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).
The `@substructure.ai/cli` package and the `subs`, `substructure-host` and
`substructure-session` crates release together at the same version.

## [Unreleased]

### Added

- A machine submit can change the agent config when it creates the session.
- `turn.completed` has `turn_calls` and `turn_started_at`. `turn_calls` gives the count, the errors, the cost and the tokens of the model, tool, subagent and System One calls of the turn.
- Add `[system1.<id>]` blocks for System One models. The types are `typesafe`, `workers-ai` and `http`.
- A worker can send a `system1.call` action. The answers come back in a `system1.finished` decision.
- `[agent.<id>.retry]` takes a `system1` override.

### Changed

- The `agent` of an admin run changes the config from the file. Fields that it does not set keep their values, and unknown fields are refused.
- `SessionSeed.agent` is now an `AgentConfigPatch`, and `create_session_command` returns a `Result`.
- `RuntimeError` has an `Invalid` variant. The HTTP APIs answer it with `400`.
- `RuntimeDeps` must now have `system1` and `system1_task_queue`, and `resolve_response` takes the `[system1.*]` blocks.

### Fixed

- A worker can now set `temperature` on an `llm.call`. A call with a `temperature` now reads back from the log and the snapshot.

## [0.10.1] - 2026-10-26

### Added

- The session index keeps the title, user, turn count, token usage and activity of each session. The activity is `running`, `waiting`, `failed`, `done` or `idle`. A migration fills them in for the sessions you have.
- The admin session list takes `user_id`, `activity` and `query`. `query` matches the title.
- The session list sorts by start, spend, turns, tokens and cache rate, and `SessionFilter` takes ranges of spend, turns and start time.
- `SessionIndexStore::aggregate_sessions` totals the sessions that match a filter.

### Changed

- `SessionCursor.at` is now optional, and the cursor has a `value` for the sorts that are not by time.
- A `SessionIndexStore` must now implement `aggregate_sessions`.

### Removed

- The release does not publish the CLI to npm. Install it from `https://subs.dev/cli.sh`.

## [0.10.0] - 2026-10-18

### Added

- A machine submit can set the agent state when it creates the session.
- Add the `substructure-session` crate. It runs one session on its own log, in memory or in a store you supply.
- Add `[events.<id>]` blocks. The `events` URL of an agent gets the events and the live text of its sessions.
- A subagent session sends its events to the events URL of its parent when its agent names none.
- Add `subs secret events.<id>`.
- A submit with `queue: "merge"` adds its messages to the turn that waits, so the messages sent during a turn become one turn.
- A message that is copied onto a new branch names its original in `copied_from`.
- Add an admin route that resumes a parked reader of a session.
- A host can now give a `SlackBot` to the runtime as a session reader.
- A host can now give the runtime the HTTP client it uses for events URLs.
- A machine submit can set `metadata` for its turn. `turn.started` and each decision of the turn carry it.
- Add `[agent.<id>.context]` strategies. Before a model call that has no room, the engine sends a `context.limit` decision with a plan to reset or summarize.
- Add the `context.replace` action. It sets the history on a new branch and keeps the state and the config.
- Add `POST /api/machine/sessions/{id}/cancel`, `POST /api/client/sessions/{id}/cancel` and `subs sessions cancel`. They cancel the current run of a session and its subagents.
- When a provider refuses a call as too long, the engine summarizes and tries the call one more time.
- An `llm.call` with no messages after a `context.replace` reads the new history.
- An LLM call with an empty tool list now sends no tools to the provider.

### Changed

- A decision request now has `turn: { id, metadata }` in place of `turn_id`. A decision that runs outside a turn has no turn.
- `done` now ends each tool call that has no result in the history. The call gets the result that arrived, or `cancelled: the turn ended`. A later result gets an error.
- A client action now runs in the running turn, or in its own turn with the id and `metadata` of its submit. The submit returns the id of that turn. When its own turn has no more work, the turn ends.
- The Slack bot posts no reply for a turn that shows nothing and returns no data.
- When `done`, `fail` or a new branch ends a blocking subagent call, the engine now cancels the child's run.
- A cancel now stops the current run and keeps the session usable. The turn ends with the error code `cancelled`, the open interrupts end as cancelled, and the work that waits is dropped.
- The engine now stops a running model call or tool when its call ends.
- `Ctrl-C` during a turn in `subs chat` and `subs run` now cancels the turn. `subs chat` then shows the prompt again.
- `subs chat` no longer shows a tool call of a failed turn as waiting.
- A turn now waits for the work that its `turn.finished` answer starts.
- A failed turn now keeps the client actions that wait.
- The `substructure-core` crate is now two crates: `substructure-host` and `subs`, which holds the CLI.
- Log filters now name `substructure_host`, `substructure_session` and `subs` in place of `substructure_core`.
- Each session now keeps its own log of entries in the database.
- The database migration moves existing sessions into their logs and removes the old event tables.
- Work, the session list, wake times and Slack messages now catch up after a crash.
- The engine skips a task when its work was cancelled or settled.
- After a restart, the engine sends the open work of each session again.
- Each event now names the command that caused it in `command_id`.
- A command now returns before the engine sends its work, updates the session list or posts to Slack.
- The engine now writes a wake time only when it changes.
- A failed delivery to the session list, to Slack or to another session now retries with backoff and is written to the session log.
- Work that the engine cannot put on its queue is sent again later as the same attempt.
- Model calls, tool calls, subagents and decisions now go through one queue interface that a host can replace.
- A queued decision is now leased while a worker takes it, so a crash does not lose it.
- Each failed push of a decision or start of a subagent is now recorded, not only the first.
- A decision push that runs longer than its lease keeps the lease and is not pushed twice.
- The engine no longer pushes a decision that was settled or dropped while it waited.
- A task result that fails to save is saved again before the task runs again.
- Work that the engine can never send now fails at once with the error.
- The engine now answers its own tools, such as tool search, in the same step as the call.
- The engine and the CLI now return or log errors that stopped them before.
- A session now keeps only its active branch in memory. Other branches stay in the log and load again when a view names them.
- A resent message whose ID the session already recorded on another branch now starts no turn.
- Work or a delivery that does not send within `send_timeout_secs` now retries later. The default is 30 seconds.
- The Slack bot now builds its task card from the session events.
- A click on a Slack prompt button now resumes the interrupt directly.
- A failed read of an events block now retries the delivery instead of dropping the events.

### Removed

- Decision requests no longer carry `message_tree`. Read `messages` for the active branch, or the client session for every branch.
- Decision responses no longer carry `channels`, and the engine no longer writes `channels.updated`. To change what a channel shows, use an events URL.

## [0.9.5] - 2026-09-09

### Changed

- A model call that fails, is truncated, or is refused now fails the turn instead of pausing the session.
- Add a `fail` action that ends the turn as a failed run.

### Fixed

- The key and token prompts no longer show a pasted secret and no longer corrupt the display.

## [0.9.4] - 2026-09-02

### Added

- Add better multi-modal support via attachments..
- Make max body size configurable.

## [0.9.3] - 2026-09-02

### Fixed

- Secret command picks correct environment

## [0.9.2] - 2026-09-01

### Added

- Add detached subagent support.
- Add `[worker.*]` blocks. Agents name a worker instead of a URL.
- Project level default worker.
- Add `subs secret` to print or rotate a minted secret.

### Changed

- Consistent references in config and on the wire.
- A plugin reference can narrow its servers: `plugins = [{ id = "x", servers = ["y"] }]`.
- Make workers a separate entity, not an agent property.
- The agent directory gives all of one tenant's declaration in one read.

### Removed

- Remove `signing_secret_env`.
- Remove `subs agents secret` and `subs agents rotate-secret`; use `subs secret worker.<id>`.
- An agent's `mcp` list no longer names a plugin's server; use the plugin's `servers`.

## [0.9.1] - 2026-08-28

### Added

- Better plugin path expansion.

## [0.9.0] - 2026-08-28

### Added

- Add `max_subagent_depth` with a default of 5.
- A subagent call can continue an earlier child session.
- Add a `single` strategy that offers one `subagent` tool for all subagents.

### Changed

- Secret prompts now mask each character, to show that a paste arrived.
- A subagent result now contains the child session id.
- `defer_tools` now covers subagent tools.
- Subagent references in the manifest use the `agent.<id>` form.
- Session cancel now stops running subagents.
- Rename `sub_agents` to `subagents` in the manifest.

## [0.8.3] - 2026-08-26

### Changed

- Move to per agent slack manifest install

## [0.8.2] - 2026-08-25

### Changed

- The license is now MIT.

## [0.8.1] - 2026-08-25

### Added

- Better MCP connection failure handling.
- Per-agent Slack manifest based installs

### Changed

- The default config file is now `subs.toml`, the default database is now `subs.db`, and the default config directory is now `~/.config/subs`.
- `subs run` and `subs chat` take agent id as a positional argument
- Retry policy improvementss
- Better concurrent tool execution within a turn.

### Fixed

- A plugin server now re-fetches its tools when a person authorizes it.

## [0.8.0] - 2026-08-24

### Added

- Add CLI chat.

### Changed

- Manifest format updates

## [0.7.2] - 2026-08-21

### Fixed

- Allow local HTTP MCP servers.
- The resume hint now repeats the flags you gave and ends with the message placeholder.

## [0.7.1] - 2026-08-19

### Fixed

- `subs mcp` commands now find a plugin's servers when the file names a `[remote]`.

## [0.7.0] - 2026-08-19

### Added

- Support approval interrupts on destructive MCP tools.
- Add agent-plugins support https://agent-plugins.org/
- Support per-user MCP credentials
- A client token says who else can read the session.

### Changed

- Auth refactor.
- A Slack turn shows one task card that updates in place and settles to the answer.

### Fixed

- A connection that needs authorizing no longer discards the message that started the turn.
- The Retry button now answers its prompt when a second connection also needs authorizing.

## [0.6.0] - 2026-08-14

### Added

- MCP revision 2026-07-28.
- Richer tool result support.
- Slack messages with audio and video reach the model.

### Fixed

- A model call that gets no answer is tried again.

## [0.5.0] - 2026-08-13

### Added

- Agents can set `effort` in the manifest.
- Slack messages with images, PDFs, and text files reach the model.
- Slack replies carry the images the model makes.
- Add a blob store for message attachments and generated images. The local engine keeps the bytes in its database.
- Add a client endpoint that serves stored blobs.

### Fixed

- `subs serve --slack-agent` stops at startup if the name is not a declared agent.
- A model that refuses a request stops the run, instead of answering it with nothing.
- Anthropic calls send the reasoning fields, the output limit, and the sampling that the model reads.
- OpenAI calls read the whole model name, and keep `temperature` for chat models and at no effort.
- OpenRouter calls keep a deferred tool out of the request, as the other providers do.
- An uauthorized mcp interrupts the session.
- Streamed calls now read `data:` lines that have no space after the colon.
- Responses keep the model's reasoning, and Anthropic and OpenRouter calls send it back.
- Calls to OpenAI-compatible providers no longer send engine-internal message fields.
- Engine context added inline keeps a message's image parts.

## [0.4.1] - 2026-08-11

### Changed

- Admin caller renamed to Operator.

### Fixed

- Session streams release their subscription when the reader goes away

## [0.4.0] - 2026-08-11

### Changed

- Improve MCP auth failure handling.
- Standardize usage reporting.
- Clearer split between remote/local command handling
- Split the machine caller into an API key caller and an admin caller.

### Added

- CLI run works against a remote server
- Slackbot prompts for MCP reauth by default. Overrideable in config.
- Engine driven deferred tool definition support.
- Calls to Anthropic, OpenAI, and OpenRouter now cache the prompt.
- Streamed calls now report their token counts, and the cached part of the prompt.
- Support cache_ttl on llm config blocks in the manifest.

## [0.3.1] - 2026-08-07

### Added

- Add support for MCP token auth.

### Changed

## [0.3.0] - 2026-08-06

### Changed

- The changelog starts again at this release. For the changes before it, refer
  to the git history.

[Unreleased]: https://github.com/substructureai/substructure/compare/v0.10.1...HEAD
[0.10.1]: https://github.com/substructureai/substructure/compare/v0.10.0...v0.10.1
[0.10.0]: https://github.com/substructureai/substructure/compare/v0.9.5...v0.10.0
[0.9.5]: https://github.com/substructureai/substructure/compare/v0.9.4...v0.9.5
[0.9.4]: https://github.com/substructureai/substructure/compare/v0.9.3...v0.9.4
[0.9.3]: https://github.com/substructureai/substructure/compare/v0.9.2...v0.9.3
[0.9.2]: https://github.com/substructureai/substructure/compare/v0.9.1...v0.9.2
[0.9.1]: https://github.com/substructureai/substructure/compare/v0.9.0...v0.9.1
[0.9.0]: https://github.com/substructureai/substructure/compare/v0.8.3...v0.9.0
[0.8.3]: https://github.com/substructureai/substructure/compare/v0.8.2...v0.8.3
[0.8.2]: https://github.com/substructureai/substructure/compare/v0.8.1...v0.8.2
[0.8.1]: https://github.com/substructureai/substructure/compare/v0.8.0...v0.8.1
[0.8.0]: https://github.com/substructureai/substructure/compare/v0.7.2...v0.8.0
[0.7.2]: https://github.com/substructureai/substructure/compare/v0.7.1...v0.7.2
[0.7.1]: https://github.com/substructureai/substructure/compare/v0.7.0...v0.7.1
[0.7.0]: https://github.com/substructureai/substructure/compare/v0.6.0...v0.7.0
[0.6.0]: https://github.com/substructureai/substructure/compare/v0.5.0...v0.6.0
[0.5.0]: https://github.com/substructureai/substructure/compare/v0.4.1...v0.5.0
[0.4.1]: https://github.com/substructureai/substructure/compare/v0.4.0...v0.4.1
[0.4.0]: https://github.com/substructureai/substructure/compare/v0.3.1...v0.4.0
[0.3.1]: https://github.com/substructureai/substructure/compare/v0.3.0...v0.3.1
[0.3.0]: https://github.com/substructureai/substructure/compare/v0.2.3...v0.3.0
