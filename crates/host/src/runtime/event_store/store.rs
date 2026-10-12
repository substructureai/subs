use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Seq(pub u64);

#[derive(Debug, Clone, Default, Deserialize)]
pub struct EventFilter {
    pub session_id: Option<String>,
    pub tenant_id: Option<String>,
    pub after_seq: Option<Seq>,
    pub limit: Option<usize>,
}
