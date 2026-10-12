use std::marker::PhantomData;
use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, TimeDelta, Utc};
use rusqlite::{params, Connection, OptionalExtension};
use serde::de::DeserializeOwned;
use serde::Serialize;
use tokio::sync::Notify;
use uuid::Uuid;

use crate::runtime::queue::{Envelope, Queue, QueueError, Receipt, Received};
use crate::runtime::span::SpanContext;

use super::{parse_dt, SqliteDb};

const POLL: Duration = Duration::from_millis(500);

pub struct SqliteQueue<T> {
    db: SqliteDb,
    lease: TimeDelta,
    held_for: Duration,
    notify: Notify,
    task: PhantomData<fn() -> T>,
}

impl<T> SqliteQueue<T> {
    pub fn new(db: SqliteDb, lease: Duration) -> Self {
        Self {
            db,
            lease: TimeDelta::from_std(lease).unwrap_or(TimeDelta::minutes(5)),
            held_for: lease,
            notify: Notify::new(),
            task: PhantomData,
        }
    }

    async fn write<R: Send + 'static>(
        &self,
        f: impl FnOnce(&mut Connection) -> rusqlite::Result<R> + Send + 'static,
    ) -> Result<R, QueueError> {
        let writer = self.db.writer.clone();
        tokio::task::spawn_blocking(move || {
            let mut conn = writer
                .lock()
                .map_err(|e| QueueError::Unavailable(e.to_string()))?;
            f(&mut conn).map_err(|e| QueueError::Unavailable(e.to_string()))
        })
        .await
        .map_err(|e| QueueError::Unavailable(e.to_string()))?
    }
}

fn later(at: DateTime<Utc>, by: TimeDelta) -> String {
    at.checked_add_signed(by).unwrap_or(at).to_rfc3339()
}

#[async_trait]
impl<T> Queue<T> for SqliteQueue<T>
where
    T: Serialize + DeserializeOwned + Send + 'static,
{
    async fn send(&self, envelope: Envelope<T>) -> Result<(), QueueError> {
        let task = serde_json::to_string(&envelope.task)
            .map_err(|e| QueueError::Unavailable(e.to_string()))?;
        let span = serde_json::to_string(&envelope.span)
            .map_err(|e| QueueError::Unavailable(e.to_string()))?;
        let enqueued_at = envelope.enqueued_at.to_rfc3339();
        self.write(move |conn| {
            conn.execute(
                "INSERT OR IGNORE INTO engine_queue
                    (id, topic, key, span, task, enqueued_at, visible_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?6)",
                params![
                    envelope.id,
                    envelope.topic,
                    envelope.key,
                    span,
                    task,
                    enqueued_at
                ],
            )
        })
        .await?;
        self.notify.notify_waiters();
        Ok(())
    }

    async fn receive(&self, topic: &str) -> Result<Received<T>, QueueError> {
        loop {
            let notified = self.notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            let topic = topic.to_string();
            let lease = self.lease;
            let taken = self
                .write(move |conn| {
                    let now = Utc::now();
                    let receipt = Uuid::now_v7().to_string();
                    conn.query_row(
                        "UPDATE engine_queue SET receipt = ?1, visible_at = ?2
                          WHERE id = (SELECT id FROM engine_queue
                                       WHERE topic = ?3 AND visible_at <= ?4
                                       ORDER BY enqueued_at, id LIMIT 1)
                          RETURNING id, key, span, task, enqueued_at, receipt",
                        params![receipt, later(now, lease), topic, now.to_rfc3339()],
                        |row| {
                            Ok((
                                row.get::<_, String>(0)?,
                                row.get::<_, String>(1)?,
                                row.get::<_, String>(2)?,
                                row.get::<_, String>(3)?,
                                row.get::<_, String>(4)?,
                                row.get::<_, String>(5)?,
                                topic.clone(),
                            ))
                        },
                    )
                    .optional()
                })
                .await?;
            if let Some((id, key, span, task, enqueued_at, receipt, topic)) = taken {
                let task: T = serde_json::from_str(&task)
                    .map_err(|e| QueueError::Unavailable(format!("{id}: {e}")))?;
                let span: SpanContext =
                    serde_json::from_str(&span).unwrap_or_else(|_| SpanContext::root());
                return Ok(Received {
                    envelope: Envelope {
                        id,
                        topic,
                        key,
                        span,
                        enqueued_at: parse_dt(&enqueued_at).unwrap_or_else(Utc::now),
                        task,
                    },
                    receipt: Receipt(receipt),
                });
            }
            tokio::select! {
                _ = notified => {}
                _ = tokio::time::sleep(POLL) => {}
            }
        }
    }

    async fn ack(&self, receipt: &Receipt) -> Result<(), QueueError> {
        let receipt = receipt.0.clone();
        self.write(move |conn| {
            conn.execute(
                "DELETE FROM engine_queue WHERE receipt = ?1",
                params![receipt],
            )
        })
        .await
        .map(|_| ())
    }

    async fn retry(&self, receipt: &Receipt, after: Duration) -> Result<(), QueueError> {
        let receipt = receipt.0.clone();
        let after = TimeDelta::from_std(after).unwrap_or(TimeDelta::zero());
        self.write(move |conn| {
            conn.execute(
                "UPDATE engine_queue SET receipt = NULL, visible_at = ?1 WHERE receipt = ?2",
                params![later(Utc::now(), after), receipt],
            )
        })
        .await?;
        self.notify.notify_waiters();
        Ok(())
    }

    async fn extend(&self, receipt: &Receipt, by: Duration) -> Result<(), QueueError> {
        let receipt = receipt.0.clone();
        let by = TimeDelta::from_std(by).unwrap_or(TimeDelta::zero());
        self.write(move |conn| {
            conn.execute(
                "UPDATE engine_queue SET visible_at = MAX(visible_at, ?1) WHERE receipt = ?2",
                params![later(Utc::now(), by), receipt],
            )
        })
        .await
        .map(|_| ())
    }

    fn lease(&self) -> Option<Duration> {
        Some(self.held_for)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn queue(lease: Duration) -> (tempfile::TempDir, SqliteQueue<u32>) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("subs.db");
        let db = SqliteDb::open(path.to_str().unwrap(), Duration::from_secs(5)).unwrap();
        (dir, SqliteQueue::new(db, lease))
    }

    fn envelope(topic: &str, id: &str, task: u32) -> Envelope<u32> {
        Envelope::new(topic, "s1", id, SpanContext::root(), task)
    }

    #[tokio::test]
    async fn a_task_is_kept_once_and_leaves_on_ack() {
        let (_dir, q) = queue(Duration::from_secs(60));
        q.send(envelope("decisions:t", "d1:0", 1)).await.unwrap();
        q.send(envelope("decisions:t", "d1:0", 2)).await.unwrap();

        let got = q.receive("decisions:t").await.unwrap();
        assert_eq!((got.envelope.id.as_str(), got.envelope.task), ("d1:0", 1));
        q.ack(&got.receipt).await.unwrap();
        q.send(envelope("decisions:t", "d1:0", 3)).await.unwrap();
        assert_eq!(q.receive("decisions:t").await.unwrap().envelope.task, 3);
    }

    #[tokio::test]
    async fn a_lease_that_runs_out_hands_the_task_out_again() {
        let (_dir, q) = queue(Duration::from_millis(50));
        q.send(envelope("llm", "x", 7)).await.unwrap();
        let first = q.receive("llm").await.unwrap();
        let second = q.receive("llm").await.unwrap();

        assert_eq!(second.envelope.id, "x");
        q.ack(&first.receipt).await.unwrap();
        q.ack(&second.receipt).await.unwrap();
    }

    #[tokio::test]
    async fn a_held_lease_keeps_the_task_from_going_out_again() {
        let (_dir, q) = queue(Duration::from_millis(80));
        q.send(envelope("llm", "x", 7)).await.unwrap();
        let first = q.receive("llm").await.unwrap();
        for _ in 0..4 {
            q.extend(&first.receipt, Duration::from_millis(80))
                .await
                .unwrap();
            tokio::time::sleep(Duration::from_millis(40)).await;
        }
        let second = tokio::time::timeout(Duration::from_millis(30), q.receive("llm")).await;
        assert!(second.is_err());
        q.ack(&first.receipt).await.unwrap();
    }

    #[tokio::test]
    async fn a_retried_task_waits_and_readers_see_only_their_topic() {
        let (_dir, q) = queue(Duration::from_secs(60));
        q.send(envelope("decisions:a", "a1", 1)).await.unwrap();
        q.send(envelope("decisions:b", "b1", 2)).await.unwrap();
        assert_eq!(q.receive("decisions:b").await.unwrap().envelope.id, "b1");

        let got = q.receive("decisions:a").await.unwrap();
        q.retry(&got.receipt, Duration::from_millis(100))
            .await
            .unwrap();
        let again = q.receive("decisions:a").await.unwrap();
        assert_eq!(again.envelope.id, "a1");
    }
}
