use std::ops::Deref;

use crate::ids::Ids;
use crate::state::{BranchId, SessionState, MAIN_BRANCH};

#[derive(Clone, Copy)]
pub struct Cx<'a> {
    state: &'a SessionState,
    ids: &'a Ids,
    scope: &'a BranchId,
}

impl<'a> Cx<'a> {
    pub fn new(state: &'a SessionState, ids: &'a Ids) -> Self {
        Self {
            state,
            ids,
            scope: &MAIN_BRANCH,
        }
    }

    pub fn scoped(self, branch: &'a BranchId) -> Self {
        Self {
            scope: branch,
            ..self
        }
    }

    pub fn scope(&self) -> &'a BranchId {
        self.scope
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
