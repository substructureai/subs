# node-no-deps-system1

An agent that asks a System One model how hard each message is, then answers with a small or a big model. Served with Node's built-in `http` server. No dependencies.

## Run

Install the CLI:

```sh
curl -fsSL https://subs.dev/cli.sh | bash
```

Two terminals.

**1. Start the worker**:

```sh
node server.mjs
```

**2. Send a message with the CLI**

```sh
export ANTHROPIC_API_KEY=sk-ant-...
export TYPESAFE_API_KEY=...
subs run -c subs.toml my-agent "prove that there are infinitely many primes"
```

The worker prints the score and the model it picked.
