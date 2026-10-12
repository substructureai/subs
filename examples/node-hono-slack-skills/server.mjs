// Skills as Slack buttons: after every turn the thread gets a row of skills, and
// a click turns one on for the thread and answers again with it.
//
// One server, two paths. `/` is the worker: it decides what happens next.
// `/events` is the events URL: it hears what happened and draws the row.
import { serve } from "@hono/node-server";
import { Hono } from "hono";
import { createHmac, timingSafeEqual } from "node:crypto";
import { readdirSync, readFileSync } from "node:fs";
import { join, dirname } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";
import matter from "gray-matter";

const dir = join(dirname(fileURLToPath(import.meta.url)), "skills");
const SLACK_BOT_TOKEN = process.env.SLACK_BOT_TOKEN_ONCALL;
const EVENTS_SECRET = process.env.EVENTS_SECRET;

// Each skill is a folder: SKILL.md holds its instructions, tools.mjs its functions.
const SKILLS = {};
for (const name of readdirSync(dir)) {
    const { data, content } = matter(readFileSync(join(dir, name, "SKILL.md"), "utf8"));
    const tools = await import(pathToFileURL(join(dir, name, "tools.mjs"))).then((m) => m.default).catch(() => []);
    SKILLS[data.name] = { description: data.description, body: content.trim(), tools };
}

const toolsOf = (active) => active.flatMap((name) => SKILLS[name].tools);

const agent = (active) => ({
    llm: "openrouter",
    model: "deepseek/deepseek-v4-flash-0731",
    system: "You are an assistant in Slack. Keep answers short.",
    tools: toolsOf(active)
});

// A click comes back as a `client.action` named after `action_id`, so the id
// carries which skill it is.
const button = (name, on) => ({
    type: "button",
    action_id: `skill:${name}`,
    text: { type: "plain_text", text: on ? `✓ ${name}` : name },
    value: name,
    style: on ? "primary" : undefined
});

const row = (active) => [
    { type: "actions", elements: Object.keys(SKILLS).map((name) => button(name, active.includes(name))) }
];

async function slack(method, body) {
    const res = await fetch(`https://slack.com/api/${method}`, {
        method: "POST",
        headers: { authorization: `Bearer ${SLACK_BOT_TOKEN}`, "content-type": "application/json" },
        body: JSON.stringify(body)
    });
    const out = await res.json();
    if (!out.ok) console.warn(`slack ${method}: ${out.error}`);
}

// --- worker ---------------------------------------------------------------

async function decide({ trigger, proposed, state, messages }) {
    const active = state?.active ?? [];

    if (trigger.type === "session.start") {
        return { agent: agent(active), state: { active } };
    }

    if (trigger.type === "client.action" && trigger.name.startsWith("skill:")) {
        const name = trigger.name.slice("skill:".length);
        const next = active.includes(name) ? active : [...active, name];
        // Move the check mark on the row that was clicked.
        const click = trigger.args;
        await slack("chat.update", { channel: click.channel, ts: click.message_ts, text: "Skills", blocks: row(next) });
        // `messages` is the whole branch, so the skill goes on the end of the
        // thread. Sending it alone starts the turn on a branch of its own.
        const note = {
            role: "user",
            content: `Use this skill and answer again.\n\n${SKILLS[name].body}`
        };
        return {
            state: { active: next },
            agent: agent(next),
            messages: [...messages, note],
            actions: [{ type: "llm.call" }]
        };
    }

    if (trigger.type === "tool.execute") {
        const tool = toolsOf(active).find((t) => t.name === trigger.name);
        return { actions: [{ type: "tool.result", result: { content: [{ type: "text", text: tool.exec(trigger.input.value) }] } }] };
    }

    return proposed;
}

// --- events URL -----------------------------------------------------------

// The skills each thread has on, from the `worker.state.updated` events.
const activeBySession = new Map();

function signed(body, header) {
    if (!EVENTS_SECRET) return true;
    const want = `sha256=${createHmac("sha256", EVENTS_SECRET).update(body).digest("hex")}`;
    return header?.length === want.length && timingSafeEqual(Buffer.from(header), Buffer.from(want));
}

async function heard({ session_id, metadata, events = [] }) {
    for (const { payload } of events) {
        if (payload.type === "worker.state.updated") {
            activeBySession.set(session_id, payload.state?.active ?? []);
        }
        // The row goes into the thread once the turn ends.
        if (payload.type === "turn.completed" && !payload.error) {
            await slack("chat.postMessage", {
                channel: metadata.slack_channel,
                thread_ts: metadata.slack_thread_ts,
                text: "Skills",
                blocks: row(activeBySession.get(session_id) ?? [])
            });
        }
    }
}

const app = new Hono();
app.post("/", async (c) => c.json(await decide(await c.req.json())));
app.post("/events", async (c) => {
    const body = await c.req.text();
    if (!signed(body, c.req.header("x-substructure-signature"))) return c.body(null, 401);
    const batch = JSON.parse(body);
    if (batch.type === "events") await heard(batch);
    return c.body(null, 204);
});

serve({ fetch: app.fetch, port: 4444 }, () =>
    console.log("worker and events URL listening on http://localhost:4444"));
