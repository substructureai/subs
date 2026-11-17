use std::collections::{HashMap, HashSet};

use crate::command::CommandPayload;
use crate::entry::Entry;
use crate::events::EventPayload;
use crate::protocol::{ClientPayload, DraftMessage};
use crate::session::{Error, Session};
use crate::state::{Logged, SessionState};
use crate::store::{Store, StoreError};
use crate::tree::Node;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tie {
    pub node: String,
    pub seq: u64,
    pub parent: Option<String>,
}

pub fn tie(payload: &EventPayload, seq: u64) -> Option<Tie> {
    let node = Node::of_event(payload)?;
    Some(Tie {
        node: node.id().to_string(),
        seq,
        parent: node.parent().map(str::to_string),
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
    pub nodes: Vec<Logged<Node>>,
}

impl Loaded {
    pub fn seqs(&self) -> Vec<u64> {
        let mut seqs: Vec<u64> = self.nodes.iter().map(|n| n.seq).collect();
        seqs.sort_unstable();
        seqs
    }

    fn take(&mut self, event: crate::SessionEvent) {
        if let Some(node) = Node::of_event(&event.payload) {
            self.nodes.push(Logged {
                seq: event.seq,
                entry: node,
            });
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
    nodes: HashMap<String, Logged<Node>>,
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
                cursor = self.nodes.get(at).and_then(|n| n.parent());
            }
        }
        let mut nodes: Vec<Logged<Node>> = wanted
            .iter()
            .filter_map(|id| self.nodes.get(id).cloned())
            .collect();
        if nodes.is_empty() {
            return None;
        }
        nodes.sort_by_key(|n| n.seq);
        Some(Loaded { nodes })
    }

    pub fn drop_known(
        &self,
        payload: ClientPayload,
        state: &SessionState,
    ) -> Option<ClientPayload> {
        let fresh = |m: &DraftMessage| {
            m.id.as_deref()
                .is_none_or(|id| !self.knows(id) && !state.versions_hold(id))
        };
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
                cursor = tie.parent;
            }
        }
        if seqs.is_empty() {
            return Ok(());
        }
        for event in store.events_at(&seqs)? {
            if let Some(node) = Node::of_event(&event.payload) {
                self.nodes.insert(
                    node.id().to_string(),
                    Logged {
                        seq: event.seq,
                        entry: node,
                    },
                );
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
            CommandPayload::SubmitWorkerDecision {
                transcript,
                actions,
                ..
            } => {
                for id in transcript.iter().filter_map(|m| m.id.as_deref()) {
                    outside.load(store, state, id)?;
                }
                let promoted = actions.iter().filter_map(|a| match a {
                    crate::decision::Action::PromoteBranch { id }
                    | crate::decision::Action::SpawnSubagent {
                        session_id: Some(id),
                        ..
                    } => state.branches.get(id)?.leaf.as_deref(),
                    _ => None,
                });
                for leaf in promoted {
                    outside.load(store, state, leaf)?;
                }
            }
            CommandPayload::SendMessage { message, .. } => {
                if let Some(id) = message.id.as_deref() {
                    outside.mark(store, state, id)?;
                }
            }
            CommandPayload::SubmitClientPayload { payload, .. } => {
                let ids: Vec<&str> = match payload {
                    ClientPayload::Message(m) => m.message.id.as_deref().into_iter().collect(),
                    ClientPayload::Append(a) => {
                        a.messages.iter().filter_map(|m| m.id.as_deref()).collect()
                    }
                    ClientPayload::Messages(v) => {
                        for id in v.messages.iter().filter_map(|m| m.id.as_deref()) {
                            outside.load(store, state, id)?;
                        }
                        Vec::new()
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
