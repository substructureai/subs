import { ExportedMessageRepository, type AssistantRuntime, type ThreadMessageLike } from "@assistant-ui/react";
import { fromAgUiMessages } from "@assistant-ui/react-ag-ui";
import { useEffect } from "react";
import type { Message, MessageTree } from "../protocol";

type Interrupt = { interrupt_id: string; reason: string; anchor?: string | null };
type SessionView = {
    message_tree: MessageTree;
    versions?: Record<string, string[]>;
    interrupts?: Interrupt[];
};
type WireMessage = Message & { metadata?: unknown };
type Engine = { url: string; token: string; sessionId: string };

// Loads the session's message tree — branches included — from the engine and
// imports it into the thread. A history adapter can't do this: the AG-UI
// runtime flattens its load() result to a linear list, so branch structure
// only survives through runtime.thread.import. The engine records every
// message during the run, so there is nothing to persist on our side.
export function useSessionHistory(runtime: AssistantRuntime, engine: Engine) {
    useEffect(() => {
        let stale = false;
        load(engine).then((view) => {
            if (!view || stale) return; // not created yet
            runtime.thread.import(toRepository(view));
        });
        return () => {
            stale = true;
        };
    }, [runtime, engine.url, engine.token, engine.sessionId]);
}

// The engine sends one version at a time: the head, and the ids of every other
// version of its messages. Read each of those once and merge them into one tree.
async function load(engine: Engine): Promise<SessionView | undefined> {
    const get = async (leaf?: string): Promise<SessionView | undefined> => {
        const query = leaf ? `?leaf=${encodeURIComponent(leaf)}` : "";
        const res = await fetch(`${engine.url}/api/client/sessions/${engine.sessionId}${query}`, {
            headers: { authorization: `Bearer ${engine.token}` },
        });
        return res.ok ? res.json() : undefined;
    };
    const head = await get();
    if (!head) return undefined;
    const nodes = new Map(head.message_tree.nodes.map((n) => [n.message.id, n]));
    const others = Object.values(head.versions ?? {})
        .flat()
        .filter((id) => !nodes.has(id));
    for (const view of await Promise.all(others.map(get)))
        for (const n of view?.message_tree.nodes ?? []) nodes.set(n.message.id, n);
    return { ...head, message_tree: { ...head.message_tree, nodes: [...nodes.values()] } };
}

// The wire tree has one node per message, but assistant-ui folds tool results
// into the assistant message that called them, so nodes don't map 1:1 onto UI
// messages. Convert one branch at a time — fromAgUiMessages does the folding —
// and merge the branches back together on converted ids.
function toRepository({ message_tree, interrupts = [] }: SessionView): ExportedMessageRepository {
    const nodes = message_tree.nodes;
    const messages = new Map<string, WireMessage>(nodes.map((n) => [n.message.id, n.message]));
    const parent = new Map(nodes.map((n) => [n.message.id, n.parent_id]));
    const branchTo = (leafId?: string | null) => {
        const branch: WireMessage[] = [];
        for (let id = leafId; id; id = parent.get(id)) branch.unshift(messages.get(id)!);
        return branch;
    };

    attachInterrupts(messages, branchTo(message_tree.head_id), interrupts);

    const items = new Map<string, { message: ThreadMessageLike; parentId: string | null }>();
    const convert = (leafId?: string | null) => {
        const branch = fromAgUiMessages(branchTo(leafId));
        branch.forEach((message, i) => items.set(message.id!, { message, parentId: branch[i - 1]?.id ?? null }));
        return branch;
    };

    const headId = convert(message_tree.head_id).at(-1)?.id ?? null;
    const isParent = new Set(nodes.map((n) => n.parent_id));
    for (const n of nodes) if (!isParent.has(n.message.id)) convert(n.message.id);

    return ExportedMessageRepository.fromBranchableArray([...items.values()], { headId });
}

// Pending interrupts ride as metadata on the newest assistant message of the
// head branch, where the AG-UI converter picks them up.
function attachInterrupts(messages: Map<string, WireMessage>, head: WireMessage[], interrupts: Interrupt[]) {
    const onHead = new Set(head.map((m) => m.id));
    const pending = interrupts
        .filter((i) => !i.anchor || onHead.has(i.anchor))
        .map((i) => ({ id: i.interrupt_id, reason: i.reason }));
    const newest = head.filter((m) => m.role === "assistant").at(-1);
    if (pending.length && newest)
        messages.set(newest.id, { ...newest, metadata: { custom: { agui: { interrupts: pending } } } });
}
