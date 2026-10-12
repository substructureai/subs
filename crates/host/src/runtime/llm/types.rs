use std::collections::BTreeMap;
use std::sync::Arc;

use async_trait::async_trait;

use crate::protocol::{
    DeferToolsStrategy, ErrorCode, ErrorInfo, LlmResponse, PromptRequest, SessionOwner, StreamDelta,
};

#[async_trait]
pub trait LlmProviderTrait: Send + Sync {
    async fn resolve(&self, owner: &SessionOwner) -> Result<Arc<dyn LlmCallable>, String>;
}

#[async_trait]
pub trait LlmResolver: Send + Sync {
    async fn resolve(
        &self,
        llm: &str,
        owner: &SessionOwner,
    ) -> Result<Arc<dyn LlmCallable>, String>;

    fn is_empty(&self) -> bool {
        false
    }
}

#[derive(Default)]
pub struct LlmProviderRegistry {
    providers: BTreeMap<String, Arc<dyn LlmProviderTrait>>,
}

impl LlmProviderRegistry {
    pub fn new(providers: BTreeMap<String, Arc<dyn LlmProviderTrait>>) -> Self {
        Self { providers }
    }
}

#[async_trait]
impl LlmResolver for LlmProviderRegistry {
    fn is_empty(&self) -> bool {
        self.providers.is_empty()
    }

    async fn resolve(
        &self,
        llm: &str,
        owner: &SessionOwner,
    ) -> Result<Arc<dyn LlmCallable>, String> {
        match self.providers.get(llm) {
            Some(p) => p.resolve(owner).await,
            None => Err(format!(
                "no engine-run llm block `{llm}` — declared: {}",
                match self.providers.is_empty() {
                    true => "none".to_string(),
                    false => self
                        .providers
                        .keys()
                        .map(String::as_str)
                        .collect::<Vec<_>>()
                        .join(", "),
                }
            )),
        }
    }
}

#[derive(Debug, Clone)]
pub struct CallContext<'a> {
    pub session_id: &'a str,
    pub tenant_id: &'a str,
    pub agent_id: &'a str,
    pub call_id: &'a str,
    pub attempt: u32,
    pub owner: &'a SessionOwner,
    pub ancestry: &'a [String],
    pub defer_tools_strategy: DeferToolsStrategy,
}

impl CallContext<'_> {
    pub fn root_session_id(&self) -> &str {
        self.ancestry
            .first()
            .map(String::as_str)
            .unwrap_or(self.session_id)
    }
}

#[async_trait]
pub trait LlmCallable: Send + Sync + 'static {
    async fn call(
        &self,
        request: &PromptRequest,
        ctx: &CallContext<'_>,
    ) -> Result<LlmResponse, LlmCallError>;

    async fn call_streaming(
        &self,
        request: &PromptRequest,
        ctx: &CallContext<'_>,
        _tx: tokio::sync::mpsc::UnboundedSender<StreamDelta>,
    ) -> Result<LlmResponse, LlmCallError> {
        self.call(request, ctx).await
    }
}

#[derive(Debug, Clone)]
pub struct LlmCallError {
    pub error: ErrorInfo,
    pub retryable: bool,
}

impl LlmCallError {
    pub fn new(code: ErrorCode, message: impl Into<String>, retryable: bool) -> Self {
        Self {
            error: ErrorInfo::new(code, message),
            retryable,
        }
    }
}

impl From<crate::json::JsonParseError> for LlmCallError {
    fn from(e: crate::json::JsonParseError) -> Self {
        Self {
            error: ErrorInfo::from(e),
            retryable: false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{Issuer, Requester, Subject};

    fn ctx<'a>(owner: &'a SessionOwner, ancestry: &'a [String]) -> CallContext<'a> {
        CallContext {
            session_id: "grandchild",
            tenant_id: "t1",
            agent_id: "a1",
            call_id: "c1",
            attempt: 0,
            owner,
            ancestry,
            defer_tools_strategy: Default::default(),
        }
    }

    #[test]
    fn root_session_id_is_the_first_ancestor_or_the_session_itself() {
        let owner = SessionOwner {
            tenant_id: "t1".to_string(),
            requester: Requester::new(
                Subject::new(Issuer::app(), "user-1".to_string()),
                Default::default(),
            ),
            metadata: Default::default(),
        };
        let ancestry = vec!["root".to_string(), "child".to_string()];
        assert_eq!(ctx(&owner, &ancestry).root_session_id(), "root");
        assert_eq!(ctx(&owner, &[]).root_session_id(), "grandchild");
    }
}
