use serde::{Deserialize, Serialize};

pub mod credential;
pub mod mcp;
pub mod oauth;
pub mod registry;

pub use crate::protocol::{AuthNeed, Issuer, Requester, Subject, Visibility};
pub use substructure_session::connectors::{filter, RemoteTool, ToolAnnotations};

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Slot {
    Shared,
    Of(Subject),
}

impl std::fmt::Display for Slot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Slot::Shared => f.write_str("shared"),
            Slot::Of(subject) => write!(f, "subject `{subject}`"),
        }
    }
}

#[async_trait::async_trait]
pub trait CredentialSource: Send + Sync {
    async fn headers(&self) -> Result<reqwest::header::HeaderMap, ConnectorError>;
}

#[derive(Debug, Clone, PartialEq)]
pub struct ConnectorError {
    pub message: String,
    pub retryable: bool,
    pub auth: Option<AuthNeed>,
}

impl ConnectorError {
    pub fn permanent(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            retryable: false,
            auth: None,
        }
    }

    pub fn retryable(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            retryable: true,
            auth: None,
        }
    }

    pub fn unauthorized(need: AuthNeed, message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            retryable: false,
            auth: Some(need),
        }
    }
}

impl std::fmt::Display for ConnectorError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for ConnectorError {}
