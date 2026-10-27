// Routes each message to a small or a big model by a System One score. No dependencies.
import { createServer } from "node:http";

const SMALL = "claude-haiku-4-5-20251001";
const BIG = "claude-sonnet-5";

function decide({ trigger, proposed, messages }) {
    // Before the model answers, ask how hard the request is.
    if (trigger.type === "client.messages") {
        return {
            messages: proposed.messages,
            actions: [
                {
                    type: "system1.call",
                    questions: {
                        hard: {
                            type: "score",
                            instructions: "How hard is the user's last request to answer well?",
                            criteria: ["A greeting or a simple fact", "Some thought", "Deep reasoning or expertise"]
                        }
                    }
                }
            ]
        };
    }

    // Pick the model from the answer. If the call failed, use the big one.
    if (trigger.type === "system1.finished") {
        const score = trigger.ok ? trigger.answers.hard.score : 2;
        const model = score >= 1.5 ? BIG : SMALL;
        console.log(`score ${score} -> ${model}`);
        return { messages, actions: [{ type: "llm.call", model }] };
    }

    // Accept the engine's proposal for every other decision.
    return proposed;
}

const server = createServer((req, res) => {
    let body = "";
    req.on("data", (chunk) => (body += chunk));
    req.on("end", () => {
        const decision = decide(JSON.parse(body));
        res.writeHead(200, { "content-type": "application/json" });
        res.end(JSON.stringify(decision));
    });
});

server.listen(4444, () => console.log("worker listening on http://localhost:4444"));
