use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use chrono::{DateTime, Utc};

use crate::entry::{CommandId, CommandResult, Entry, Tail};
use crate::session::{Session, SessionEvent};
use crate::tree::{index, BranchRecord, NodeRef, Nodes};

#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum StoreError {
    #[error("version {0} is taken")]
    VersionTaken(u64),
    #[error("store unavailable: {0}")]
    Unavailable(String),
    #[error("corrupt: {0}")]
    Corrupt(String),
    #[error("store: {0}")]
    Other(String),
}

pub trait Store {
    fn append(&self, entry: &Entry) -> Result<(), StoreError>;
    fn read(&self, after_seq: u64) -> Result<Tail, StoreError>;
    fn events(&self, after_seq: u64, max: usize) -> Result<Vec<SessionEvent>, StoreError>;
    fn events_at(&self, seqs: &[u64]) -> Result<Vec<SessionEvent>, StoreError>;
    fn snapshot(&self, session: &Session) -> Result<(), StoreError>;
    fn latest(&self) -> Result<Option<Session>, StoreError>;
    fn cursors(&self) -> Result<BTreeMap<String, u64>, StoreError>;
    fn set_cursor(&self, consumer: &str, seq: u64) -> Result<(), StoreError>;
    fn nodes(&self) -> Arc<dyn Nodes>;
    fn handled(&self, id: &CommandId)
        -> Result<Option<(CommandResult, DateTime<Utc>)>, StoreError>;
}

impl Entry {
    pub fn to_json(&self) -> Result<Vec<u8>, StoreError> {
        serde_json::to_vec(self).map_err(|e| StoreError::Corrupt(e.to_string()))
    }

    pub fn from_json(body: &[u8]) -> Result<Self, StoreError> {
        serde_json::from_slice(body).map_err(|e| StoreError::Corrupt(e.to_string()))
    }
}

impl Session {
    pub fn to_json(&self) -> Result<Vec<u8>, StoreError> {
        serde_json::to_vec(self).map_err(|e| StoreError::Corrupt(e.to_string()))
    }

    pub fn from_json(body: &[u8]) -> Result<Self, StoreError> {
        serde_json::from_slice(body).map_err(|e| StoreError::Corrupt(e.to_string()))
    }
}

#[derive(Default)]
struct Space {
    log: BTreeMap<u64, Entry>,
    snapshot: Option<Session>,
    cursors: BTreeMap<String, u64>,
    nodes: BTreeMap<u64, NodeRef>,
    ids: BTreeMap<String, u64>,
    handled: BTreeMap<CommandId, (CommandResult, DateTime<Utc>)>,
    branches: BTreeMap<String, BranchRecord>,
}

#[derive(Clone, Default)]
pub struct MemoryStore(Arc<Mutex<Space>>);

impl MemoryStore {
    pub fn new() -> Self {
        Self::default()
    }

    fn space(&self) -> std::sync::MutexGuard<'_, Space> {
        self.0.lock().unwrap_or_else(|e| e.into_inner())
    }
}

impl Store for MemoryStore {
    fn append(&self, entry: &Entry) -> Result<(), StoreError> {
        let mut space = self.space();
        let head = space.log.keys().next_back().copied().unwrap_or(0);
        if entry.version != head + 1 {
            return Err(StoreError::VersionTaken(entry.version));
        }
        let indexed: Vec<_> = entry
            .events
            .iter()
            .map(|e| index(e.seq, &e.payload))
            .collect();
        let ids: Vec<&str> = indexed
            .iter()
            .filter_map(|i| i.node.as_ref().map(|n| n.id()))
            .collect();
        let unique: std::collections::HashSet<&str> = ids.iter().copied().collect();
        if unique.len() < ids.len() || ids.iter().any(|id| space.ids.contains_key(*id)) {
            return Err(StoreError::Corrupt("a node id is written twice".into()));
        }
        for indexed in &indexed {
            if let Some(node) = &indexed.node {
                space.ids.insert(node.id().to_string(), indexed.seq);
                space.nodes.insert(indexed.seq, node.clone());
            }
            if let Some(change) = &indexed.change {
                let known = space.branches.remove(change.id());
                if let Some(record) = change.record(indexed.seq, known) {
                    space.branches.insert(change.id().to_string(), record);
                }
            }
        }
        for handled in &entry.commands {
            space
                .handled
                .insert(handled.id.clone(), (handled.result.clone(), entry.at));
        }
        space.log.insert(entry.version, entry.clone());
        Ok(())
    }

    fn read(&self, after_seq: u64) -> Result<Tail, StoreError> {
        let space = self.space();
        Ok(Tail {
            version: space.log.keys().next_back().copied().unwrap_or(0),
            events: space
                .log
                .values()
                .flat_map(|entry| entry.events.iter())
                .filter(|e| e.seq > after_seq)
                .cloned()
                .collect(),
        })
    }

    fn events(&self, after_seq: u64, max: usize) -> Result<Vec<SessionEvent>, StoreError> {
        Ok(self
            .space()
            .log
            .values()
            .flat_map(|entry| entry.events.iter())
            .filter(|event| event.seq > after_seq)
            .take(max)
            .cloned()
            .collect())
    }

    fn snapshot(&self, session: &Session) -> Result<(), StoreError> {
        let mut space = self.space();
        if space
            .snapshot
            .as_ref()
            .is_none_or(|s| s.version() < session.version())
        {
            space.snapshot = Some(session.clone());
        }
        Ok(())
    }

    fn latest(&self) -> Result<Option<Session>, StoreError> {
        Ok(self.space().snapshot.clone())
    }

    fn cursors(&self) -> Result<BTreeMap<String, u64>, StoreError> {
        Ok(self.space().cursors.clone())
    }

    fn nodes(&self) -> Arc<dyn Nodes> {
        Arc::new(self.clone())
    }

    fn handled(
        &self,
        id: &CommandId,
    ) -> Result<Option<(CommandResult, DateTime<Utc>)>, StoreError> {
        Ok(self.space().handled.get(id).cloned())
    }

    fn events_at(&self, seqs: &[u64]) -> Result<Vec<SessionEvent>, StoreError> {
        let wanted: std::collections::BTreeSet<u64> = seqs.iter().copied().collect();
        Ok(self
            .space()
            .log
            .values()
            .flat_map(|entry| entry.events.iter())
            .filter(|event| wanted.contains(&event.seq))
            .cloned()
            .collect())
    }

    fn set_cursor(&self, consumer: &str, seq: u64) -> Result<(), StoreError> {
        let mut space = self.space();
        let cursor = space.cursors.entry(consumer.to_string()).or_default();
        *cursor = (*cursor).max(seq);
        Ok(())
    }
}

impl Nodes for MemoryStore {
    fn node(&self, id: &str) -> Result<Option<NodeRef>, StoreError> {
        let space = self.space();
        Ok(space
            .ids
            .get(id)
            .and_then(|seq| space.nodes.get(seq))
            .cloned())
    }

    fn ancestors(&self, id: &str, limit: usize) -> Result<Vec<NodeRef>, StoreError> {
        let space = self.space();
        let mut found = Vec::new();
        let mut cursor = Some(id.to_string());
        while let Some(node) = cursor
            .take()
            .and_then(|id| space.ids.get(&id))
            .and_then(|seq| space.nodes.get(seq))
            .filter(|_| found.len() < limit.min(space.nodes.len()))
        {
            cursor = node.parent().map(str::to_string);
            found.push(node.clone());
        }
        Ok(found)
    }

    fn children(&self, parent: &str, branch: Option<&str>) -> Result<Vec<NodeRef>, StoreError> {
        Ok(self
            .space()
            .nodes
            .values()
            .filter(|n| n.parent() == Some(parent) && n.branch() == branch)
            .cloned()
            .collect())
    }

    fn branch(&self, id: &str) -> Result<Option<BranchRecord>, StoreError> {
        Ok(self.space().branches.get(id).cloned())
    }

    fn versions(
        &self,
        prev: Option<&str>,
        branch: Option<&str>,
    ) -> Result<Vec<NodeRef>, StoreError> {
        let space = self.space();
        let message_at = |from: Option<&str>| -> Option<String> {
            let mut cursor = from.map(str::to_string);
            while let Some(node) = cursor
                .as_deref()
                .and_then(|id| space.ids.get(id))
                .and_then(|seq| space.nodes.get(seq))
            {
                if node.message().is_some() {
                    return Some(node.id().to_string());
                }
                cursor = node.parent().map(str::to_string);
            }
            None
        };
        Ok(space
            .nodes
            .values()
            .filter(|n| n.message().is_some() && n.branch() == branch)
            .filter(|n| message_at(n.parent()).as_deref() == prev)
            .cloned()
            .collect())
    }

    fn answers(&self, tool_call_id: &str) -> Result<Vec<NodeRef>, StoreError> {
        Ok(self
            .space()
            .nodes
            .values()
            .filter(|n| {
                n.message()
                    .is_some_and(|m| m.tool_call_id.as_deref() == Some(tool_call_id))
            })
            .cloned()
            .collect())
    }
}
