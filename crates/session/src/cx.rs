use std::ops::Deref;

use crate::ids::Ids;
use crate::state::SessionState;

#[derive(Clone, Copy)]
pub struct Cx<'a> {
    state: &'a SessionState,
    ids: &'a Ids,
}

impl<'a> Cx<'a> {
    pub fn new(state: &'a SessionState, ids: &'a Ids) -> Self {
        Self { state, ids }
    }

    pub fn state(&self) -> &'a SessionState {
        self.state
    }

    pub fn ids(&self) -> &'a Ids {
        self.ids
    }

    pub fn next_id(&self) -> String {
        self.ids.next()
    }
}

impl Deref for Cx<'_> {
    type Target = SessionState;

    fn deref(&self) -> &SessionState {
        self.state
    }
}
