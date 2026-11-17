use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};

use crate::decision::BranchConfig;
use crate::events::BranchEnd;
use crate::protocol::{AgentConfig, DraftMessage, Message, NewMessage, WorkerState};
use crate::state::{BranchState, Logged};
use crate::store::StoreError;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Place {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Fact {
    Message {
        message: Message,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        copied_from: Option<String>,
    },
    State {
        state: WorkerState,
    },
    Config {
        #[serde(default)]
        config: Option<AgentConfig>,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        root: bool,
    },
    Context {
        call: String,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        placed: Vec<crate::prompt_context::Placed>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Node {
    #[serde(flatten)]
    pub place: Place,
    #[serde(flatten)]
    pub fact: Fact,
}

impl Node {
    pub fn id(&self) -> &str {
        &self.place.id
    }

    pub fn parent(&self) -> Option<&str> {
        self.place.parent_id.as_deref()
    }

    pub fn branch(&self) -> Option<&str> {
        self.place.branch.as_deref()
    }

    pub fn message(&self) -> Option<&Message> {
        match &self.fact {
            Fact::Message { message, .. } => Some(message),
            _ => None,
        }
    }

    pub fn of_message(m: &NewMessage) -> Self {
        Node {
            place: Place {
                id: m.message.id.clone(),
                parent_id: m.parent_id.clone(),
                branch: m.branch.clone(),
            },
            fact: Fact::Message {
                message: m.message.clone(),
                copied_from: m.copied_from.clone(),
            },
        }
    }
}

pub type Shared = Arc<Logged<Node>>;

#[derive(Debug, Clone)]
pub struct BranchRecord {
    pub state: BranchState,
    pub closed: Option<BranchEnd>,
}

pub trait Nodes: Send + Sync {
    fn node(&self, id: &str) -> Result<Option<Shared>, StoreError>;
    fn ancestors(&self, id: &str, limit: usize) -> Result<Vec<Shared>, StoreError>;
    fn children(&self, parent: &str, branch: Option<&str>) -> Result<Vec<Shared>, StoreError>;
    fn answers(&self, tool_call_id: &str) -> Result<Vec<Shared>, StoreError>;
    fn branch(&self, id: &str) -> Result<Option<BranchRecord>, StoreError>;
    fn versions(&self, prev: Option<&str>, branch: Option<&str>)
        -> Result<Vec<Shared>, StoreError>;
    fn remember(&self, _nodes: Vec<Shared>) {}
}

#[derive(Default)]
pub struct Tree {
    source: Option<Arc<dyn Nodes>>,
    fresh: HashMap<String, Shared>,
    branches: HashMap<String, BranchRecord>,
    failed: Mutex<Option<StoreError>>,
}

impl Clone for Tree {
    fn clone(&self) -> Self {
        Tree {
            source: self.source.clone(),
            fresh: self.fresh.clone(),
            branches: self.branches.clone(),
            failed: Mutex::default(),
        }
    }
}

impl std::fmt::Debug for Tree {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Tree")
            .field("stored", &self.source.is_some())
            .field("fresh", &self.fresh.len())
            .finish()
    }
}

impl Tree {
    pub fn over(source: Arc<dyn Nodes>) -> Self {
        Tree {
            source: Some(source),
            ..Tree::default()
        }
    }

    pub fn failed(&self) -> Option<StoreError> {
        self.failed.lock().ok()?.take()
    }

    pub fn failing(&self) -> bool {
        self.failed.lock().is_ok_and(|failed| failed.is_some())
    }

    fn read<T: Default>(&self, read: impl FnOnce(&dyn Nodes) -> Result<T, StoreError>) -> T {
        let Some(source) = &self.source else {
            return T::default();
        };
        read(source.as_ref()).unwrap_or_else(|error| {
            if let Ok(mut failed) = self.failed.lock() {
                failed.get_or_insert(error);
            }
            T::default()
        })
    }

    pub fn get(&self, id: &str) -> Option<Shared> {
        match self.fresh.get(id) {
            Some(node) => Some(node.clone()),
            None => self.read(|s| s.node(id)),
        }
    }

    pub fn has(&self, id: &str) -> bool {
        self.get(id).is_some()
    }

    pub fn push(&mut self, node: Logged<Node>) {
        self.fresh
            .entry(node.id().to_string())
            .or_insert_with(|| Arc::new(node));
    }

    pub fn record(&mut self, id: &str, state: BranchState, closed: Option<BranchEnd>) {
        self.branches
            .insert(id.to_string(), BranchRecord { state, closed });
    }

    pub fn branch(&self, id: &str) -> Option<BranchRecord> {
        match self.branches.get(id) {
            Some(record) => Some(record.clone()),
            None => self.read(|s| s.branch(id)),
        }
    }

    pub fn flush(&mut self) {
        let Some(source) = &self.source else {
            return;
        };
        self.branches.clear();
        source.remember(self.fresh.drain().map(|(_, node)| node).collect());
    }

    pub fn fresh(&self) -> impl Iterator<Item = &Shared> {
        self.fresh.values()
    }

    pub fn up(&self, from: Option<&str>) -> Up<'_> {
        Up {
            tree: self,
            cursor: from.map(str::to_string),
            stored: Vec::new().into_iter(),
            steps: 0,
        }
    }

    pub fn children(&self, parent: &str, branch: Option<&str>) -> Vec<Shared> {
        let mut found = self.read(|s| s.children(parent, branch));
        found.extend(
            self.fresh
                .values()
                .filter(|n| n.parent() == Some(parent) && n.branch() == branch)
                .cloned(),
        );
        found.sort_by_key(|n| n.seq);
        found
    }

    pub fn answers(&self, tool_call_id: &str) -> Vec<Shared> {
        let mut found = self.read(|s| s.answers(tool_call_id));
        found.extend(
            self.fresh
                .values()
                .filter(|n| {
                    n.message()
                        .is_some_and(|m| m.tool_call_id.as_deref() == Some(tool_call_id))
                })
                .cloned(),
        );
        found.sort_by_key(|n| n.seq);
        found
    }

    pub fn versions(&self, message: &str) -> Vec<String> {
        let Some(node) = self.get(message).filter(|n| n.message().is_some()) else {
            return Vec::new();
        };
        let prev = self.message_at(node.parent());
        let branch = node.branch().map(str::to_string);
        let mut found = self.read(|s| s.versions(prev.as_deref(), branch.as_deref()));
        found.extend(
            self.fresh
                .values()
                .filter(|n| n.message().is_some() && n.branch() == branch.as_deref())
                .filter(|n| self.message_at(n.parent()) == prev)
                .cloned(),
        );
        found.sort_by_key(|n| n.seq);
        found.dedup_by(|a, b| a.id() == b.id());
        found.into_iter().map(|n| n.id().to_string()).collect()
    }

    pub fn newest(&self, from: &str) -> String {
        let mut at = from.to_string();
        let branch = self.get(from).and_then(|n| n.branch().map(str::to_string));
        while let Some(next) = self.children(&at, branch.as_deref()).pop() {
            at = next.id().to_string();
        }
        at
    }

    pub fn path_ids(&self, leaf: &str) -> HashSet<String> {
        self.up(Some(leaf)).map(|n| n.id().to_string()).collect()
    }

    pub fn messages(&self, leaf: Option<&str>) -> Vec<Message> {
        let mut path: Vec<Message> = self.up(leaf).filter_map(|n| n.message().cloned()).collect();
        path.reverse();
        path
    }

    pub fn message_at(&self, node: Option<&str>) -> Option<String> {
        self.up(node)
            .find(|n| n.message().is_some())
            .map(|n| n.id().to_string())
    }

    pub fn shared(
        &self,
        view: &[DraftMessage],
        matches: impl Fn(&DraftMessage, &Node) -> bool,
    ) -> usize {
        let mut last: Option<String> = None;
        let mut count = 0;
        for draft in view {
            let Some(node) = draft.id.as_deref().and_then(|id| self.get(id)) else {
                break;
            };
            if node.message().is_none()
                || self.message_at(node.parent()) != last
                || !matches(draft, &node)
            {
                break;
            }
            last = Some(node.id().to_string());
            count += 1;
        }
        count
    }

    pub fn fork_on(
        &self,
        leaf: Option<&str>,
        message: Option<&str>,
        floor: Option<&str>,
    ) -> Option<Option<String>> {
        let mut fork = leaf.map(str::to_string);
        for node in self.up(leaf) {
            if Some(node.id()) == message || Some(node.id()) == floor {
                return Some(fork);
            }
            if node.message().is_some() {
                fork = node.parent().map(str::to_string);
            }
        }
        message.is_none().then_some(fork)
    }

    pub fn tail(&self, message: &str) -> (String, bool) {
        let mut at = message.to_string();
        let branch = self
            .get(message)
            .and_then(|n| n.branch().map(str::to_string));
        loop {
            let children = self.children(&at, branch.as_deref());
            let Some(next) = children.iter().rev().find(|n| n.message().is_none()) else {
                let continues = children.iter().any(|n| n.message().is_some());
                return (at, continues);
            };
            at = next.id().to_string();
        }
    }
}

const ANCESTORS: usize = 256;

pub struct Up<'a> {
    tree: &'a Tree,
    cursor: Option<String>,
    stored: std::vec::IntoIter<Shared>,
    steps: usize,
}

impl Iterator for Up<'_> {
    type Item = Shared;

    fn next(&mut self) -> Option<Shared> {
        if let Some(node) = self.stored.next() {
            return Some(node);
        }
        let id = self.cursor.take()?;
        match self.tree.fresh.get(&id) {
            Some(node) => {
                self.steps += 1;
                if self.steps <= self.tree.fresh.len() {
                    self.cursor = node.parent().map(str::to_string);
                }
                Some(node.clone())
            }
            None => {
                let mut chunk = self.tree.read(|s| s.ancestors(&id, ANCESTORS));
                if chunk.len() == ANCESTORS {
                    self.cursor = chunk.pop().map(|n| n.id().to_string());
                }
                self.stored = chunk.into_iter();
                self.stored.next()
            }
        }
    }
}

impl Node {
    pub fn of_event(payload: &crate::events::EventPayload) -> Option<Node> {
        use crate::events::EventPayload;
        Some(match payload {
            EventPayload::NewMessage(m) => Node::of_message(m),
            EventPayload::WorkerStateUpdated(v) => Node {
                place: v.node.clone(),
                fact: Fact::State {
                    state: v.state.clone(),
                },
            },
            EventPayload::AgentConfigUpdated(v) => Node {
                place: v.node.clone(),
                fact: Fact::Config {
                    config: Some(v.config.clone()),
                    root: false,
                },
            },
            EventPayload::ContextPlaced(c) => Node {
                place: c.node.clone(),
                fact: Fact::Context {
                    call: c.call.clone(),
                    placed: c.placed.clone(),
                },
            },
            EventPayload::BranchStarted(b) if b.leaf.is_none() => Node {
                place: Place {
                    id: b.node.clone(),
                    parent_id: b.from.clone(),
                    branch: Some(b.id.clone()),
                },
                fact: Fact::Config {
                    config: match &b.agent {
                        BranchConfig::Inherit => return None,
                        BranchConfig::None => None,
                        BranchConfig::Config(config) => Some((**config).clone()),
                    },
                    root: true,
                },
            },
            _ => return None,
        })
    }
}
