use std::fmt;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::caller::{Caller, CallerKind};
use crate::command::{CommandPayload, SessionError};
use crate::session::SessionEvent;
use crate::span::SpanContext;

#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct CommandId(pub String);

impl fmt::Display for CommandId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl<T: Into<String>> From<T> for CommandId {
    fn from(id: T) -> Self {
        Self(id.into())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Command {
    pub id: CommandId,
    pub caller: Caller,
    pub payload: CommandPayload,
    pub span: SpanContext,
}

impl Command {
    pub fn new(id: impl Into<CommandId>, caller: Caller, payload: CommandPayload) -> Self {
        Self {
            id: id.into(),
            caller,
            payload,
            span: SpanContext::root(),
        }
    }

    pub fn system(id: impl Into<CommandId>, tenant_id: &str, payload: CommandPayload) -> Self {
        Self::new(
            id,
            Caller::System {
                tenant_id: tenant_id.to_string(),
            },
            payload,
        )
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Accepted {
    pub seq: u64,
    pub turn_id: Option<String>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub queued: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Rejected {
    pub error: SessionError,
}

pub type CommandResult = Result<Accepted, Rejected>;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Handled {
    pub id: CommandId,
    pub kind: String,
    pub caller: CallerKind,
    pub result: CommandResult,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Entry {
    pub version: u64,
    pub at: DateTime<Utc>,
    pub commands: Vec<Handled>,
    pub events: Vec<SessionEvent>,
}

impl Entry {
    pub fn first_seq(&self) -> Option<u64> {
        self.events.first().map(|e| e.seq)
    }

    pub fn last_seq(&self) -> Option<u64> {
        self.events.last().map(|e| e.seq)
    }
}
