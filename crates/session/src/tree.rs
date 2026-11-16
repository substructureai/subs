use std::collections::{HashMap, HashSet};

use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::decision::BranchConfig;
use crate::protocol::{AgentConfig, Message, NewMessage, WorkerState};
use crate::state::Logged;

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
    BranchStart {
        #[serde(default)]
        agent: BranchConfig,
    },
    Start,
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

#[derive(Debug, Clone, Default)]
pub struct Tree {
    nodes: Vec<Logged<Node>>,
    index: HashMap<String, usize>,
}

impl Tree {
    pub fn get(&self, id: &str) -> Option<&Logged<Node>> {
        self.index.get(id).map(|&i| &self.nodes[i])
    }

    pub fn has(&self, id: &str) -> bool {
        self.index.contains_key(id)
    }

    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }

    pub fn iter(&self) -> std::slice::Iter<'_, Logged<Node>> {
        self.nodes.iter()
    }

    pub fn push(&mut self, node: Logged<Node>) {
        if self.index.contains_key(node.id()) {
            return;
        }
        self.index.insert(node.id().to_string(), self.nodes.len());
        self.nodes.push(node);
    }

    pub fn up<'a>(&'a self, from: Option<&'a str>) -> Up<'a> {
        Up {
            tree: self,
            cursor: from,
            seen: 0,
        }
    }

    pub fn path_ids<'a>(&'a self, leaf: &'a str) -> HashSet<&'a str> {
        self.up(Some(leaf)).map(|n| n.id()).collect()
    }

    pub fn messages(&self, leaf: Option<&str>) -> Vec<Message> {
        let mut path: Vec<Message> = self.up(leaf).filter_map(|n| n.message().cloned()).collect();
        path.reverse();
        path
    }

    pub fn start_of<'a>(&'a self, node: Option<&'a str>) -> Option<&'a str> {
        self.up(node)
            .find(|n| matches!(n.fact, Fact::Start))
            .map(Node::id)
    }

    pub fn message_at<'a>(&'a self, node: Option<&'a str>) -> Option<&'a str> {
        self.up(node).find(|n| n.message().is_some()).map(Node::id)
    }

    pub fn message_nodes(&self) -> Vec<NewMessage> {
        self.nodes
            .iter()
            .filter_map(|n| match &n.fact {
                Fact::Message {
                    message,
                    copied_from,
                } => Some(NewMessage {
                    message: message.clone(),
                    parent_id: self.message_at(n.parent()).map(str::to_string),
                    copied_from: copied_from.clone(),
                    branch: n.place.branch.clone(),
                }),
                _ => None,
            })
            .collect()
    }

    pub fn retain(&mut self, keep: impl Fn(&Logged<Node>) -> bool) {
        self.nodes.retain(|n| keep(n));
        self.reindex();
    }

    pub fn extend(&mut self, nodes: impl IntoIterator<Item = Logged<Node>>) {
        for node in nodes {
            if !self.index.contains_key(node.id()) {
                self.nodes.push(node);
            }
        }
        self.nodes.sort_by_key(|n| n.seq);
        self.reindex();
    }

    fn reindex(&mut self) {
        self.index = self
            .nodes
            .iter()
            .enumerate()
            .map(|(i, n)| (n.id().to_string(), i))
            .collect();
    }
}

pub struct Up<'a> {
    tree: &'a Tree,
    cursor: Option<&'a str>,
    seen: usize,
}

impl<'a> Iterator for Up<'a> {
    type Item = &'a Node;

    fn next(&mut self) -> Option<&'a Node> {
        let node = self.tree.get(self.cursor?)?;
        self.seen += 1;
        self.cursor = match self.seen > self.tree.nodes.len() {
            true => None,
            false => node.parent(),
        };
        Some(&node.entry)
    }
}

impl Serialize for Tree {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        self.nodes.serialize(s)
    }
}

impl<'de> Deserialize<'de> for Tree {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let mut tree = Tree {
            nodes: Vec::deserialize(d)?,
            index: HashMap::new(),
        };
        tree.reindex();
        Ok(tree)
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
                    config: v.config.clone(),
                },
            },
            EventPayload::ConversationStarted(c) => Node {
                place: c.node.clone(),
                fact: Fact::Start,
            },
            EventPayload::BranchStarted(b) => Node {
                place: Place {
                    id: b.node.clone(),
                    parent_id: b.from.clone(),
                    branch: Some(b.id.clone()),
                },
                fact: Fact::BranchStart {
                    agent: b.agent.clone(),
                },
            },
            _ => return None,
        })
    }
}
