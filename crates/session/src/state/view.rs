use super::*;

/// A session's state beside its tree. The tree is not serialized: a load
/// replays the state, then reads the tree from the store.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(transparent)]
pub struct View {
    pub state: SessionState,
    #[serde(skip)]
    pub tree: crate::tree::Tree,
}

impl View {
    pub fn new(session_id: String) -> Self {
        Self {
            state: SessionState::new(session_id),
            tree: Default::default(),
        }
    }

    pub fn apply(&mut self, event: &EventPayload, ctx: &ApplyContext) {
        let indexed = self.state.apply(event, ctx);
        self.tree.push(&indexed);
    }

    pub fn at<'a, 'n>(&'a self, node: Option<&'n str>) -> At<'a, 'n> {
        At::new(self, node)
    }
}

impl View {
    #[cfg(test)]
    pub fn seed(&mut self, node: crate::tree::Node) {
        let seq = self.tree.unwritten().count() as u64;
        self.state.grow(&node);
        self.tree.seed(Logged { seq, entry: node });
    }

    #[cfg(test)]
    pub fn seed_config(&mut self, config: AgentConfig) {
        let parent = self.leaf(&BranchId::MAIN).map(str::to_string);
        let id = format!("seed-config-{}", self.tree.unwritten().count());
        self.seed(crate::tree::Node {
            place: crate::tree::Place {
                id,
                parent_id: parent,
                branch: None,
            },
            fact: crate::tree::Fact::Config { config },
        });
    }

    #[cfg(test)]
    pub fn seed_message(&mut self, message: crate::protocol::NewMessage) {
        self.seed(crate::tree::Node::of_message(&message));
    }
}

impl std::ops::Deref for View {
    type Target = SessionState;
    fn deref(&self) -> &SessionState {
        &self.state
    }
}

impl std::ops::DerefMut for View {
    fn deref_mut(&mut self) -> &mut SessionState {
        &mut self.state
    }
}
