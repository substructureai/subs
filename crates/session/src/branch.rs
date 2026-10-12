use std::collections::{HashMap, HashSet};

use crate::command::CommandPayload;
use crate::entry::Entry;
use crate::events::{AgentConfigUpdated, EventPayload, WorkerStateUpdated};
use crate::protocol::{ClientPayload, DraftMessage, NewMessage};
use crate::session::{Error, Session};
use crate::state::{Logged, SessionState};
use crate::store::{Store, StoreError};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tie {
    pub node: String,
    pub seq: u64,
    pub message: bool,
    pub parent: Option<String>,
}

pub fn tie(payload: &EventPayload, seq: u64) -> Option<Tie> {
    let (node, message, parent) = match payload {
        EventPayload::NewMessage(m) => (m.message.id.clone(), true, m.parent_id.clone()),
        EventPayload::WorkerStateUpdated(v) => (v.anchor.clone()?, false, None),
        EventPayload::AgentConfigUpdated(v) => (v.anchor.clone()?, false, None),
        _ => return None,
    };
    Some(Tie {
        node,
        seq,
        message,
        parent,
    })
}

impl Entry {
    pub fn ties(&self) -> Vec<Tie> {
        self.events
            .iter()
            .filter_map(|e| tie(&e.payload, e.seq))
            .collect()
    }
}

#[derive(Debug, Clone, Default)]
pub struct Loaded {
    pub nodes: Vec<Logged<NewMessage>>,
    pub states: Vec<Logged<WorkerStateUpdated>>,
    pub agents: Vec<Logged<AgentConfigUpdated>>,
}

impl Loaded {
    pub fn seqs(&self) -> Vec<u64> {
        let mut seqs: Vec<u64> = self
            .nodes
            .iter()
            .map(|n| n.seq)
            .chain(self.states.iter().map(|v| v.seq))
            .chain(self.agents.iter().map(|v| v.seq))
            .collect();
        seqs.sort_unstable();
        seqs
    }

    fn take(&mut self, event: crate::SessionEvent) {
        let seq = event.seq;
        match event.payload {
            EventPayload::NewMessage(node) => self.nodes.push(Logged { seq, entry: node }),
            EventPayload::WorkerStateUpdated(v) => self.states.push(Logged { seq, entry: v }),
            EventPayload::AgentConfigUpdated(v) => self.agents.push(Logged { seq, entry: v }),
            _ => {}
        }
    }
}

pub fn resolve(store: &dyn Store, seqs: &[u64]) -> Result<Loaded, StoreError> {
    let mut loaded = Loaded::default();
    for event in store.events_at(seqs)? {
        loaded.take(event);
    }
    Ok(loaded)
}

#[derive(Debug, Clone, Default)]
pub struct Outside {
    known: HashSet<String>,
    nodes: HashMap<String, Logged<NewMessage>>,
    states: Vec<Logged<WorkerStateUpdated>>,
    agents: Vec<Logged<AgentConfigUpdated>>,
}

impl Outside {
    pub fn knows(&self, id: &str) -> bool {
        self.known.contains(id)
    }

    pub fn is_empty(&self) -> bool {
        self.known.is_empty()
    }

    pub fn branch_to(&self, ids: &[&str], state: &SessionState) -> Option<Loaded> {
        let mut wanted = HashSet::new();
        for id in ids {
            let mut cursor = Some(*id);
            while let Some(at) = cursor {
                if state.has_node(at) || !wanted.insert(at.to_string()) {
                    break;
                }
                cursor = self.nodes.get(at).and_then(|n| n.parent_id.as_deref());
            }
        }
        let mut nodes: Vec<Logged<NewMessage>> = wanted
            .iter()
            .filter_map(|id| self.nodes.get(id).cloned())
            .collect();
        if nodes.is_empty() {
            return None;
        }
        nodes.sort_by_key(|n| n.seq);
        let on = |anchor: &Option<String>| anchor.as_ref().is_some_and(|a| wanted.contains(a));
        Some(Loaded {
            nodes,
            states: self
                .states
                .iter()
                .filter(|v| on(&v.anchor))
                .cloned()
                .collect(),
            agents: self
                .agents
                .iter()
                .filter(|v| on(&v.anchor))
                .cloned()
                .collect(),
        })
    }

    pub fn drop_known(&self, payload: ClientPayload) -> Option<ClientPayload> {
        let fresh = |m: &DraftMessage| m.id.as_deref().is_none_or(|id| !self.knows(id));
        match payload {
            ClientPayload::Message(message) => {
                fresh(&message.message).then_some(ClientPayload::Message(message))
            }
            ClientPayload::Append(mut append) => {
                append.messages.retain(fresh);
                (!append.messages.is_empty()).then_some(ClientPayload::Append(append))
            }
            other => Some(other),
        }
    }

    fn load(&mut self, store: &dyn Store, state: &SessionState, id: &str) -> Result<(), Error> {
        let mut seqs = Vec::new();
        let mut cursor = Some(id.to_string());
        while let Some(at) = cursor.take() {
            if state.has_node(&at) || self.known.contains(&at) {
                break;
            }
            let ties = store.tied(&at)?;
            if ties.is_empty() {
                break;
            }
            self.known.insert(at);
            for tie in ties {
                seqs.push(tie.seq);
                if tie.message {
                    cursor = tie.parent;
                }
            }
        }
        if seqs.is_empty() {
            return Ok(());
        }
        for event in store.events_at(&seqs)? {
            let seq = event.seq;
            match event.payload {
                EventPayload::NewMessage(node) => {
                    self.nodes
                        .insert(node.message.id.clone(), Logged { seq, entry: node });
                }
                EventPayload::WorkerStateUpdated(v) => self.states.push(Logged { seq, entry: v }),
                EventPayload::AgentConfigUpdated(v) => self.agents.push(Logged { seq, entry: v }),
                _ => {}
            }
        }
        Ok(())
    }

    fn mark(&mut self, store: &dyn Store, state: &SessionState, id: &str) -> Result<(), Error> {
        if state.has_node(id) || self.known.contains(id) {
            return Ok(());
        }
        if !store.tied(id)?.is_empty() {
            self.known.insert(id.to_string());
        }
        Ok(())
    }
}

impl Session {
    pub fn outside(&self, store: &dyn Store, command: &CommandPayload) -> Result<Outside, Error> {
        let mut outside = Outside::default();
        let state = self.state();
        match command {
            CommandPayload::SubmitWorkerDecision { transcript, .. } => {
                for id in transcript.iter().filter_map(|m| m.id.as_deref()) {
                    outside.load(store, state, id)?;
                }
            }
            CommandPayload::SubmitClientPayload { payload, .. } => {
                let ids: Vec<&str> = match payload {
                    ClientPayload::Message(m) => m.message.id.as_deref().into_iter().collect(),
                    ClientPayload::Append(a) => {
                        a.messages.iter().filter_map(|m| m.id.as_deref()).collect()
                    }
                    ClientPayload::Messages(v) => {
                        v.messages.iter().filter_map(|m| m.id.as_deref()).collect()
                    }
                    ClientPayload::Action(_) => Vec::new(),
                };
                for id in ids {
                    outside.mark(store, state, id)?;
                }
            }
            _ => {}
        }
        Ok(outside)
    }
}

pub fn scan_ties<S: Store + ?Sized>(store: &S, node: &str) -> Result<Vec<Tie>, StoreError> {
    Ok(store
        .read(0)?
        .iter()
        .flat_map(Entry::ties)
        .filter(|tie| tie.node == node)
        .collect())
}
