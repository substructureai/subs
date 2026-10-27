use std::collections::BTreeMap;
use std::sync::Arc;

use async_trait::async_trait;

use crate::llm::LlmCallError;
use crate::protocol::SessionOwner;
use substructure_session::system1::{System1Request, System1Response};

#[derive(Debug, Clone)]
pub struct Media {
    pub mime: String,
    pub bytes: Vec<u8>,
}

#[async_trait]
pub trait System1Callable: Send + Sync + 'static {
    async fn call(
        &self,
        request: &System1Request,
        assets: &BTreeMap<String, Media>,
    ) -> Result<System1Response, LlmCallError>;
}

#[async_trait]
pub trait System1Resolver: Send + Sync {
    async fn resolve(
        &self,
        block: &str,
        owner: &SessionOwner,
    ) -> Result<Arc<dyn System1Callable>, String>;

    fn is_empty(&self) -> bool {
        false
    }
}

#[derive(Default)]
pub struct System1Registry {
    blocks: BTreeMap<String, Arc<dyn System1Callable>>,
}

impl System1Registry {
    pub fn new(blocks: BTreeMap<String, Arc<dyn System1Callable>>) -> Self {
        Self { blocks }
    }
}

#[async_trait]
impl System1Resolver for System1Registry {
    async fn resolve(
        &self,
        block: &str,
        _owner: &SessionOwner,
    ) -> Result<Arc<dyn System1Callable>, String> {
        self.blocks.get(block).cloned().ok_or_else(|| {
            format!(
                "no [system1.{block}] block — declared: {}",
                crate::copy::declared(self.blocks.keys())
            )
        })
    }

    fn is_empty(&self) -> bool {
        self.blocks.is_empty()
    }
}
