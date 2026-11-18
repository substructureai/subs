use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};

use crate::events::{BranchStarted, EventPayload};
use crate::protocol::{AgentConfig, DraftMessage, LlmTool, Message, NewMessage, WorkerState};
use crate::state::{BranchId, Logged};
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
        config: AgentConfig,
    },
    Reset {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        config: Option<AgentConfig>,
    },
    Context {
        call: String,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        placed: Vec<crate::prompt_context::Placed>,
        tools: Tools,
    },
}

/// The tools a call was sent. `As` names the context node that holds the
/// same set.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Tools {
    Set(Option<Vec<LlmTool>>),
    As(String),
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

pub type NodeRef = Arc<Logged<Node>>;

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Stored {
    pub leaf: Option<String>,
    pub from: Option<String>,
}

#[derive(Debug, Clone, Default)]
struct Pending {
    started: Option<Stored>,
    leaf: Option<String>,
}

pub trait Nodes: Send + Sync {
    fn node(&self, id: &str) -> Result<Option<NodeRef>, StoreError>;
    fn ancestors(&self, id: &str, limit: usize) -> Result<Vec<NodeRef>, StoreError>;
    fn children(&self, parent: &str, branch: Option<&str>) -> Result<Vec<NodeRef>, StoreError>;
    fn answers(&self, tool_call_id: &str) -> Result<Vec<NodeRef>, StoreError>;
    fn stored(&self, branch: &BranchId) -> Result<Option<Stored>, StoreError>;
    fn versions(
        &self,
        prev: Option<&str>,
        branch: Option<&str>,
    ) -> Result<Vec<NodeRef>, StoreError>;
    fn cache(&self, _nodes: Vec<NodeRef>) {}
}

#[derive(Default)]
pub struct Tree {
    source: Option<Arc<dyn Nodes>>,
    unwritten: HashMap<String, NodeRef>,
    pending: HashMap<BranchId, Pending>,
    poisoned: Mutex<Option<StoreError>>,
}

impl Clone for Tree {
    fn clone(&self) -> Self {
        Tree {
            source: self.source.clone(),
            unwritten: self.unwritten.clone(),
            pending: self.pending.clone(),
            poisoned: Mutex::default(),
        }
    }
}

impl std::fmt::Debug for Tree {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Tree")
            .field("stored", &self.source.is_some())
            .field("unwritten", &self.unwritten.len())
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

    pub fn poisoned(&self) -> Option<StoreError> {
        self.poisoned.lock().ok()?.take()
    }

    pub fn is_poisoned(&self) -> bool {
        self.poisoned
            .lock()
            .is_ok_and(|poisoned| poisoned.is_some())
    }

    fn read<T: Default>(&self, read: impl FnOnce(&dyn Nodes) -> Result<T, StoreError>) -> T {
        let Some(source) = &self.source else {
            return T::default();
        };
        read(source.as_ref()).unwrap_or_else(|error| {
            if let Ok(mut poisoned) = self.poisoned.lock() {
                poisoned.get_or_insert(error);
            }
            T::default()
        })
    }

    pub fn tools(&self, id: &str) -> Option<Vec<LlmTool>> {
        match &self.get(id)?.fact {
            Fact::Context {
                tools: Tools::As(set),
                ..
            } => self.tools(set),
            Fact::Context {
                tools: Tools::Set(tools),
                ..
            } => tools.clone(),
            _ => None,
        }
    }

    pub fn get(&self, id: &str) -> Option<NodeRef> {
        match self.unwritten.get(id) {
            Some(node) => Some(node.clone()),
            None => self.read(|s| s.node(id)),
        }
    }

    pub fn has(&self, id: &str) -> bool {
        self.get(id).is_some()
    }

    pub fn push(&mut self, indexed: &Indexed) {
        if let Some(node) = &indexed.node {
            self.unwritten
                .entry(node.id().to_string())
                .or_insert_with(|| node.clone());
        }
        let key = BranchId::from(indexed.branch);
        if let Some(p) = indexed.started.filter(|_| !indexed.continues()) {
            self.pending.insert(
                key.clone(),
                Pending {
                    started: Some(Stored {
                        leaf: None,
                        from: p.from.clone(),
                    }),
                    leaf: None,
                },
            );
        }
        if let Some(leaf) = &indexed.leaf {
            self.pending.entry(key).or_default().leaf = Some(leaf.clone());
        }
    }

    pub fn stored(&self, branch: &BranchId) -> Option<Stored> {
        let Some(pending) = self.pending.get(branch) else {
            return self.read(|s| s.stored(branch));
        };
        let mut stored = match &pending.started {
            Some(started) => Some(started.clone()),
            None => self.read(|s| s.stored(branch)),
        };
        if let Some(leaf) = &pending.leaf {
            stored.get_or_insert_with(Stored::default).leaf = Some(leaf.clone());
        }
        stored
    }

    #[cfg(any(test, feature = "testing"))]
    pub fn seed(&mut self, node: Logged<Node>) {
        self.unwritten
            .entry(node.id().to_string())
            .or_insert_with(|| Arc::new(node));
    }

    pub fn written(&mut self) {
        let Some(source) = &self.source else {
            return;
        };
        self.pending.clear();
        source.cache(self.unwritten.drain().map(|(_, node)| node).collect());
    }

    pub fn unwritten(&self) -> impl Iterator<Item = &NodeRef> {
        self.unwritten.values()
    }

    pub fn up(&self, from: Option<&str>) -> Ancestors<'_> {
        Ancestors {
            tree: self,
            cursor: from.map(str::to_string),
            stored: Vec::new().into_iter(),
            steps: 0,
        }
    }

    pub fn children(&self, parent: &str, branch: Option<&str>) -> Vec<NodeRef> {
        let mut found = self.read(|s| s.children(parent, branch));
        found.extend(
            self.unwritten
                .values()
                .filter(|n| n.parent() == Some(parent) && n.branch() == branch)
                .cloned(),
        );
        found.sort_by_key(|n| n.seq);
        found
    }

    pub fn answers(&self, tool_call_id: &str) -> Vec<NodeRef> {
        let mut found = self.read(|s| s.answers(tool_call_id));
        found.extend(
            self.unwritten
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
            self.unwritten
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

pub struct Ancestors<'a> {
    tree: &'a Tree,
    cursor: Option<String>,
    stored: std::vec::IntoIter<NodeRef>,
    steps: usize,
}

impl Iterator for Ancestors<'_> {
    type Item = NodeRef;

    fn next(&mut self) -> Option<NodeRef> {
        if let Some(node) = self.stored.next() {
            return Some(node);
        }
        let id = self.cursor.take()?;
        match self.tree.unwritten.get(&id) {
            Some(node) => {
                self.steps += 1;
                if self.steps <= self.tree.unwritten.len() {
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

pub struct Indexed<'a> {
    pub seq: u64,
    pub node: Option<NodeRef>,
    pub started: Option<&'a BranchStarted>,
    pub branch: Option<&'a str>,
    pub leaf: Option<String>,
}

impl Indexed<'_> {
    pub fn stored(&self, known: Option<Stored>) -> Option<Stored> {
        let mut stored = match self.started {
            Some(p) if p.leaf.is_none() => Some(Stored {
                leaf: None,
                from: p.from.clone(),
            }),
            _ => known,
        };
        if let Some(leaf) = &self.leaf {
            stored.get_or_insert_with(Stored::default).leaf = Some(leaf.clone());
        }
        stored
    }

    pub fn continues(&self) -> bool {
        self.started.is_some_and(|p| p.leaf.is_some())
    }

    pub fn moves(&self) -> bool {
        self.leaf.is_some() || self.started.is_some()
    }

    pub fn answers(&self) -> Option<&str> {
        let message = self.node.as_ref()?.message()?;
        (message.role == crate::protocol::Role::Tool)
            .then_some(message.tool_call_id.as_deref())
            .flatten()
    }
}

pub fn index(seq: u64, event: &EventPayload) -> Indexed<'_> {
    let (started, branch) = match event {
        EventPayload::BranchStarted(p) => (Some(&**p), Some(p.id.as_str())),
        EventPayload::BranchFinished(p) => (None, Some(p.id.as_str())),
        EventPayload::BranchMoved(p) => (None, p.branch.as_deref()),
        EventPayload::NewMessage(m) => (None, m.branch.as_deref()),
        EventPayload::StateUpdated(v) => (None, v.node.branch.as_deref()),
        EventPayload::ConfigUpdated(v) => (None, v.node.branch.as_deref()),
        EventPayload::ConfigReset(v) => (None, v.node.branch.as_deref()),
        EventPayload::ContextPlaced(c) => (None, c.node.branch.as_deref()),
        _ => (None, None),
    };
    let node = Node::of_event(event).map(|entry| Arc::new(Logged { seq, entry }));
    let leaf = match event {
        EventPayload::BranchMoved(p) => Some(p.leaf.clone()),
        EventPayload::BranchStarted(p) if p.leaf.is_some() => p.leaf.clone(),
        _ => node.as_ref().map(|n| n.id().to_string()),
    };
    Indexed {
        seq,
        node,
        started,
        branch,
        leaf,
    }
}

impl Node {
    fn of_event(payload: &EventPayload) -> Option<Node> {
        Some(match payload {
            EventPayload::NewMessage(m) => Node::of_message(m),
            EventPayload::StateUpdated(v) => Node {
                place: v.node.clone(),
                fact: Fact::State {
                    state: v.state.clone(),
                },
            },
            EventPayload::ConfigUpdated(v) => Node {
                place: v.node.clone(),
                fact: Fact::Config {
                    config: v.config.clone(),
                },
            },
            EventPayload::ConfigReset(v) => Node {
                place: v.node.clone(),
                fact: Fact::Reset {
                    config: v.config.clone(),
                },
            },
            EventPayload::ContextPlaced(c) => Node {
                place: c.node.clone(),
                fact: Fact::Context {
                    call: c.call.clone(),
                    placed: c.placed.clone(),
                    tools: c.tools.clone(),
                },
            },
            _ => return None,
        })
    }
}
