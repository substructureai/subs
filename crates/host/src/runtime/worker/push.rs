use async_trait::async_trait;

use crate::json::JsonParseError;
use crate::protocol::{DecisionResponse, ErrorCode, ErrorInfo};
use crate::runtime::llm::TokenDeltaTransport;

use super::WorkerDecisionRequest;

#[async_trait]
pub trait Decider: Send + Sync {
    async fn push(
        &self,
        decision: &WorkerDecisionRequest,
        token_delta_transport: std::sync::Arc<dyn TokenDeltaTransport>,
    ) -> Result<DecisionResponse, PushError>;
}

#[derive(Debug)]
pub struct PushError {
    pub error: ErrorInfo,
    pub retryable: bool,
}

impl PushError {
    pub fn retryable(code: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            error: ErrorInfo::new(code, message),
            retryable: true,
        }
    }

    pub fn fatal(code: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            error: ErrorInfo::new(code, message),
            retryable: false,
        }
    }

    pub fn with_param(mut self, param: impl Into<String>) -> Self {
        self.error = self.error.with_param(param);
        self
    }

    pub fn with_detail(mut self, detail: serde_json::Value) -> Self {
        self.error = self.error.with_detail(detail);
        self
    }

    pub fn retryable_if(mut self, retryable: bool) -> Self {
        self.retryable = retryable;
        self
    }
}

impl From<JsonParseError> for PushError {
    fn from(e: JsonParseError) -> Self {
        PushError {
            error: ErrorInfo::from(e),
            retryable: false,
        }
    }
}

impl std::fmt::Display for PushError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.error)
    }
}
