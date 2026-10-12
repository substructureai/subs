use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use crate::branch::Tie;
use crate::entry::Entry;
use crate::protocol::NewMessage;
use crate::session::{Session, SessionEvent};

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
    fn read(&self, after_version: u64) -> Result<Vec<Entry>, StoreError>;
    fn read_after_seq(&self, after_seq: u64, max: usize) -> Result<Vec<Entry>, StoreError>;
    fn snapshot(&self, session: &Session) -> Result<(), StoreError>;
    fn latest(&self) -> Result<Option<Session>, StoreError>;
    fn cursors(&self) -> Result<BTreeMap<String, u64>, StoreError>;
    fn set_cursor(&self, consumer: &str, seq: u64) -> Result<(), StoreError>;
    fn tied(&self, node: &str) -> Result<Vec<Tie>, StoreError>;
    fn events_at(&self, seqs: &[u64]) -> Result<Vec<SessionEvent>, StoreError>;
    fn messages(&self) -> Result<Vec<NewMessage>, StoreError>;
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
    ties: BTreeMap<String, Vec<Tie>>,
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
        for tie in entry.ties() {
            space.ties.entry(tie.node.clone()).or_default().push(tie);
        }
        space.log.insert(entry.version, entry.clone());
        Ok(())
    }

    fn read(&self, after_version: u64) -> Result<Vec<Entry>, StoreError> {
        Ok(self
            .space()
            .log
            .range(after_version + 1..)
            .map(|(_, entry)| entry.clone())
            .collect())
    }

    fn read_after_seq(&self, after_seq: u64, max: usize) -> Result<Vec<Entry>, StoreError> {
        Ok(self
            .space()
            .log
            .values()
            .filter(|entry| entry.last_seq().is_some_and(|s| s > after_seq))
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

    fn tied(&self, node: &str) -> Result<Vec<Tie>, StoreError> {
        Ok(self.space().ties.get(node).cloned().unwrap_or_default())
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

    fn messages(&self) -> Result<Vec<NewMessage>, StoreError> {
        Ok(self
            .space()
            .log
            .values()
            .flat_map(|entry| entry.events.iter())
            .filter_map(|event| match &event.payload {
                crate::events::EventPayload::NewMessage(node) => Some(node.clone()),
                _ => None,
            })
            .collect())
    }

    fn set_cursor(&self, consumer: &str, seq: u64) -> Result<(), StoreError> {
        let mut space = self.space();
        let cursor = space.cursors.entry(consumer.to_string()).or_default();
        *cursor = (*cursor).max(seq);
        Ok(())
    }
}
