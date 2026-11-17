### Rust crates
```
crates/session           → One session: rules, state, log, stores
crates/host              → Runs sessions: server, providers, connectors, transports
crates/subs              → The `subs` CLI
```

### TypeScript packages
```
@substructure.ai/cli     → npm wrapper shipping the Rust CLI binary
```

### Session model
1. The log is the truth.
2. Every answer is appended to the log: from a model, a tool, the worker, or a person.
3. The log forms a tree of immutable nodes.
4. A branch is a leaf of that tree.
5. A prompt is a pure function of a branch.

### Coding guidelines:
Keep comments to an absolute minimum and use as few words as possible.

### CHANGELOG.md:
Keep CHANGELOG.md current. Write one short sentence for each item. Write what changed, not why or how. The reader skims the list; details live in the code and the commits. Compare each item to the last release, not to the last commit. If a change only modifies something added after the last release, do not write an item. If related changes tell one story, write one item. Write items only for changes to the code. Do not write items for changes to the documents or to the examples. Write all items in ASD-STE100. Example items: "Failed decisions now retry or end the run with the error." "Add a Slack adapter."

