---
title: Skills
group: Building agents
---

A skill is a set of instructions that the model loads when a task needs them.
A skill can also hold back tools: the model does not see them, and cannot call
them, until it loads the skill. An agent with many tasks can then keep a short
prompt and a short tool list, and add to each only what the task in hand needs.

A skill comes from one of two places.

| Source | What it holds | Where it is declared |
| --- | --- | --- |
| A bundle | A directory with a `SKILL.md`, and files | `[skill.<id>]` with a `path`, or a plugin's `skills/` |
| The worker | What the worker answers when the model loads it | `[skill.<id>]` with `source = "worker"`, a worker plugin, or the config the worker returns |

A skill of the agent's own has a bare name, such as `automations`. A plugin's
skill has the plugin's ID in front, such as `pdf:form-filling`. See
[Plugins](./45-plugins.md) for skills in a plugin.

## A skill from a directory

Put the skill in a directory whose name is the skill's name. `SKILL.md` starts
with front matter.

```markdown title="skills/automations/SKILL.md"
---
name: automations
description: Make, change and test automations
tools: [create_automation, edit_automation, simulate_automations]
---
Before you change an automation, read it with get_automation...
```

Declare it in the file, and name it on the agent.

```toml title="subs.toml"
[skill.automations]
path = "./skills/automations"

[agent.support]
llm = "claude"
model = "claude-sonnet-5"
worker = "app"
skills = ["automations"]
```

The section's ID must be the skill's name. `description` and `tools` in the
section replace the ones in the front matter. The CLI reads every skill
directory of the project into one bundle, at startup for a local engine and at
`subs apply` for a deployment, as it does for a plugin.

## A skill the worker answers

A skill from the worker can say what is true now, such as an organization's
records. Declare it with `source = "worker"` and a description.

```toml title="subs.toml"
[skill.visits]
source = "worker"
description = "Who visited the clinic, and when"
tools = ["find_bookings"]
```

When the model loads it, the engine sends the worker a `tool.execute` decision
with the name `skill` and the arguments `{"name": "visits"}`. The worker answers
with a `tool.result`, and that is the skill.

## Tools that come with a skill

`tools` names the tools that a skill holds back. Until the model loads the
skill on the branch:

- The request does not carry them.
- `tool_search` does not find them.
- A call to one fails with an error that names the skill to load.

A tool that no skill names is always there. A tool that several skills name
comes with any of them. The engine's own tools, `skill`, `tool_search` and
`call_tool`, are always there.

A skill that the model loads on one branch does not unlock its tools on another.
An edit to an earlier message starts with the tools locked again.

The request changes when a skill unlocks a tool that is not deferred. To keep
the cached prefix, defer the tools that skills hold back. See
[Deferred tools](./65-deferred-tools.md).

In [code mode](./67-code-mode.md) a skill holds back nothing. Loading it answers
with the declarations of its `tools`.

## Find skills by search

The system prompt lists each skill by default: its name and its description.
With many skills, that list is long. Set `skills = "search"` to list none of
them. `tool_search` then finds skills beside tools.

```toml title="subs.toml"
[agent.support.defer_tools]
skills = "search"
```

A search matches a skill on its name, its description, and the tools it holds
back. A match comes back under `skills`, with `load_with: "skill"`.

```json
{
  "tools": [],
  "skills": [
    { "name": "automations", "description": "Make, change and test automations",
      "tools": ["create_automation", "edit_automation", "simulate_automations"] }
  ],
  "load_with": "skill"
}
```

A plugin still says what it is for in the system prompt, without its skills.

## The file and the worker

Everything above can also come from the worker, in the config that it returns.
The file is where the agent starts, and the worker has the last word. A worker
that returns `skills` replaces the list from the file. Nothing has to be in the
file.

```json
{
  "agent": {
    "model": "claude-sonnet-5",
    "skills": [
      { "name": "visits", "description": "Who visited the clinic, and when",
        "source": "worker", "tools": ["find_bookings"] },
      { "name": "automations", "description": "Make, change and test automations",
        "tools": ["create_automation", "edit_automation"] }
    ],
    "defer_tools": { "skills": "search" }
  }
}
```

A skill without `source` comes from the project's bundle. Only the file can
add a skill directory to the bundle. The worker can name any skill that the
bundle holds.

A skill that the worker adds during a session lists itself on the branch where
the worker wrote it, without rewriting the cached prefix.

An agent that has a skill or a tool that the worker answers needs a `worker`.

## Next steps

- [Plugins](./45-plugins.md): skills and servers from a plugin directory.
- [Deferred tools](./65-deferred-tools.md): keep a large tool set out of the request.
- [Config](./220-config.md): every key of a `[skill.<id>]` section.
