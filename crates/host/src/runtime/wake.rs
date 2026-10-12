use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::runtime::session::SessionEvent;
use crate::runtime::sessions::{SessionKey, Sessions};

#[derive(Debug, Clone)]
pub struct Alarm {
    pub tenant_id: String,
    pub session_id: String,
    pub at: DateTime<Utc>,
}

#[async_trait::async_trait]
pub trait Alarms: Send + Sync {
    async fn set(
        &self,
        tenant_id: &str,
        session_id: &str,
        version: u64,
        at: Option<DateTime<Utc>>,
    ) -> Result<(), String>;
    async fn due(&self, now: DateTime<Utc>, limit: usize) -> Result<Vec<Alarm>, String>;
    async fn next(&self) -> Result<Option<DateTime<Utc>>, String>;
}

pub async fn behind_at_boot(sessions: &Sessions, alarms: &dyn Alarms) -> Vec<SessionKey> {
    let end =
        DateTime::<Utc>::from_timestamp(253_402_300_799, 0).unwrap_or(DateTime::<Utc>::MAX_UTC);
    let mut keys: Vec<SessionKey> = match alarms.due(end, usize::MAX).await {
        Ok(due) => due
            .into_iter()
            .map(|a| (a.tenant_id, a.session_id))
            .collect(),
        Err(error) => {
            tracing::warn!(%error, "listing sessions with open work failed");
            Vec::new()
        }
    };
    match sessions.behind().await {
        Ok(behind) => keys.extend(behind),
        Err(error) => tracing::warn!(%error, "listing sessions behind failed"),
    }
    keys.sort();
    keys.dedup();
    keys
}

pub fn spawn_wake_dispatcher(
    store: Arc<Sessions>,
    alarms: Arc<dyn Alarms>,
    poll_interval: Duration,
    cancel: CancellationToken,
) -> JoinHandle<()> {
    let mut rx = store.subscribe();
    tokio::spawn(async move {
        loop {
            if cancel.is_cancelled() {
                break;
            }
            let now = Utc::now();
            fire_due(&store, &alarms, now).await;
            let poll = chrono::Duration::from_std(poll_interval)
                .ok()
                .and_then(|d| now.checked_add_signed(d))
                .unwrap_or(DateTime::<Utc>::MAX_UTC);
            let mut deadline = match alarms.next().await.unwrap_or(None) {
                Some(at) if at <= now => continue,
                Some(at) => at.min(poll),
                None => poll,
            };

            loop {
                tokio::select! {
                    _ = tokio::time::sleep(chrono_to_std(deadline - Utc::now())) => break,
                    batch = rx.recv() => {
                        if let Some(events) = batch {
                            let earliest = events
                                .iter()
                                .filter_map(extract_wake_at)
                                .min();
                            if let Some(at) = earliest {
                                deadline = deadline.min(at);
                            }
                        }
                    }
                    _ = cancel.cancelled() => return,
                }
            }
        }
    })
}

async fn fire_due(store: &Arc<Sessions>, alarms: &Arc<dyn Alarms>, now: DateTime<Utc>) {
    let due = match alarms.due(now, 256).await {
        Ok(items) => items,
        Err(_) => return,
    };
    for item in due {
        store.wake(&item.tenant_id, &item.session_id, now).await;
    }
}

fn extract_wake_at(event: &SessionEvent) -> Option<DateTime<Utc>> {
    event.meta.wake_at
}

fn chrono_to_std(d: chrono::Duration) -> Duration {
    d.to_std().unwrap_or(Duration::ZERO)
}
