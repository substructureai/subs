use std::collections::BTreeMap;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use super::{Media, System1Resolver};
use crate::llm::LlmCallError;
use crate::protocol::{ErrorCode, ErrorInfo, RetryPolicy, SessionOwner};
use crate::runtime::blob::BlobStore;
use crate::runtime::executor::{
    settled, spawn_bounded_executors, Bound, BoundedTask, ExecutorPool,
};
use crate::runtime::inbox::Inbox;
use crate::runtime::queue::{Queue, SYSTEM1};
use crate::runtime::session::command::{CommandPayload, Outcome, SettleError};
use crate::runtime::session::state::EffectKind;
use crate::runtime::span::SpanContext;
use substructure_session::blob::{BlobError, BlobRef, BLOB_SCHEME};
use substructure_session::system1::System1Request;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct System1Task {
    pub session_id: String,
    pub tenant_id: String,
    pub agent_id: String,
    pub call_id: String,
    pub attempt: u32,
    pub system1: String,
    pub request: System1Request,
    pub owner: SessionOwner,
    pub retry: RetryPolicy,
}

impl BoundedTask for System1Task {
    fn bound(&self) -> Option<Bound> {
        Some(Bound {
            tenant_id: self.tenant_id.clone(),
            session_id: self.session_id.clone(),
            kind: EffectKind::System1Call,
            id: self.call_id.clone(),
            attempt: Some(self.attempt),
            queue_timeout: self.retry.queue_timeout(),
            run_timeout: self.retry.run_timeout(),
        })
    }
}

pub fn spawn_system1_task_executor(
    store: Arc<dyn Inbox>,
    blocks: Arc<dyn System1Resolver>,
    queue: Arc<dyn Queue<System1Task>>,
    blobs: Arc<dyn BlobStore>,
    pool: ExecutorPool,
    cancel: CancellationToken,
) -> Vec<JoinHandle<()>> {
    let inner = store.clone();
    spawn_bounded_executors(store, queue, SYSTEM1, pool, cancel, move |task, span| {
        let store = inner.clone();
        let blocks = blocks.clone();
        let blobs = blobs.clone();
        async move { handle_task(store.as_ref(), blocks.as_ref(), blobs.as_ref(), task, span).await }
    })
}

async fn handle_task(
    store: &dyn Inbox,
    blocks: &dyn System1Resolver,
    blobs: &dyn BlobStore,
    task: System1Task,
    span: SpanContext,
) -> Result<(), String> {
    let outcome = match call(blocks, blobs, &task).await {
        Ok(response) => Outcome::System1(Box::new(response)),
        Err(err) => SettleError::new(err.error, err.retryable).into(),
    };
    let command = CommandPayload::settle(
        EffectKind::System1Call,
        task.call_id.clone(),
        Some(task.attempt),
        outcome,
    );
    settled(
        store,
        &task.tenant_id,
        &task.session_id,
        command,
        span.child("system1_call"),
    )
    .await
}

async fn call(
    blocks: &dyn System1Resolver,
    blobs: &dyn BlobStore,
    task: &System1Task,
) -> Result<substructure_session::system1::System1Response, LlmCallError> {
    let client = blocks
        .resolve(&task.system1)
        .await
        .map_err(|e| LlmCallError::new(ErrorCode::ProviderError, e, false))?;
    let mut assets = BTreeMap::new();
    for (key, asset) in &task.request.assets {
        assets.insert(
            key.clone(),
            media(asset.uri(), blobs, &task.tenant_id).await?,
        );
    }
    client.call(&task.request, &assets).await
}

async fn media(uri: &str, blobs: &dyn BlobStore, tenant_id: &str) -> Result<Media, LlmCallError> {
    let fail = |message: &str, retryable: bool| LlmCallError {
        error: ErrorInfo::new(ErrorCode::Internal, message),
        retryable,
    };
    if !uri.starts_with(BLOB_SCHEME) {
        return Err(fail("an asset is not a blob URI", false));
    }
    let r = BlobRef::parse(uri).ok_or_else(|| fail("unparsable blob ref", false))?;
    if r.tenant_id != tenant_id {
        return Err(fail("blob ref for another tenant", false));
    }
    let bytes = blobs
        .get(&r)
        .await
        .map_err(|e| fail(&e.to_string(), matches!(e, BlobError::Io(_))))?;
    Ok(Media {
        mime: r.mime,
        bytes,
    })
}
