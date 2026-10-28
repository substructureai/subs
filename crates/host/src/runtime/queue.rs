use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use tokio::time::Instant;

use crate::runtime::span::SpanContext;

pub const LLM: &str = "llm";
pub const CONNECTOR: &str = "connector";
pub const SCRIPT: &str = "script";
pub const SUBAGENT: &str = "subagent";
pub const SYSTEM1: &str = "system1";

pub fn decisions(tenant_id: &str) -> String {
    format!("decisions:{tenant_id}")
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Envelope<T> {
    pub id: String,
    pub topic: String,
    pub key: String,
    pub span: SpanContext,
    pub enqueued_at: DateTime<Utc>,
    pub task: T,
}

impl<T> Envelope<T> {
    pub fn new(
        topic: impl Into<String>,
        key: impl Into<String>,
        id: impl Into<String>,
        span: SpanContext,
        task: T,
    ) -> Self {
        Self {
            id: id.into(),
            topic: topic.into(),
            key: key.into(),
            span,
            enqueued_at: Utc::now(),
            task,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Receipt(pub String);

#[derive(Debug)]
pub struct Received<T> {
    pub envelope: Envelope<T>,
    pub receipt: Receipt,
}

#[derive(Debug, thiserror::Error)]
pub enum QueueError {
    #[error("the queue is closed")]
    Closed,
    #[error("the queue is unavailable: {0}")]
    Unavailable(String),
}

#[async_trait]
pub trait Queue<T: Send + 'static>: Send + Sync {
    async fn send(&self, envelope: Envelope<T>) -> Result<(), QueueError>;
    async fn receive(&self, topic: &str) -> Result<Received<T>, QueueError>;
    async fn ack(&self, receipt: &Receipt) -> Result<(), QueueError>;
    async fn retry(&self, receipt: &Receipt, after: Duration) -> Result<(), QueueError>;
    async fn extend(&self, receipt: &Receipt, by: Duration) -> Result<(), QueueError>;
    fn lease(&self) -> Option<Duration>;
}

pub async fn holding<T, F>(queue: &dyn Queue<T>, receipt: &Receipt, work: F) -> F::Output
where
    T: Send + 'static,
    F: std::future::Future,
{
    tokio::pin!(work);
    let Some(lease) = queue.lease() else {
        return work.await;
    };
    let every = (lease / 3).max(Duration::from_millis(10));
    let mut renew = tokio::time::interval_at(later(every), every);
    loop {
        tokio::select! {
            done = &mut work => return done,
            _ = renew.tick() => {
                if let Err(error) = queue.extend(receipt, lease).await {
                    tracing::warn!(%error, "renewing a lease failed");
                }
            }
        }
    }
}

fn later(after: Duration) -> Instant {
    let now = Instant::now();
    now.checked_add(after)
        .or_else(|| now.checked_add(Duration::from_secs(3600)))
        .unwrap_or(now)
}
