use serde::{Deserialize, Serialize};
use serde_json::Value;

pub mod filter;

pub use crate::protocol::{AuthNeed, ConnectionPath, Issuer, Requester, Subject, Visibility};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RemoteTool {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default)]
    pub description: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output: Option<Value>,
    #[serde(default)]
    pub annotations: ToolAnnotations,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolAnnotations {
    pub read_only: Option<bool>,
    pub destructive: Option<bool>,
    pub idempotent: Option<bool>,
    pub open_world: Option<bool>,
}
