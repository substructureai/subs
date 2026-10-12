use std::sync::Arc;

use tokio::sync::mpsc;

use crate::runtime::event_store::{EventFilter, Seq};
use crate::runtime::session::events::EventPayload;
use crate::runtime::sessions::Sessions;
use crate::runtime::Caller;

use super::SessionEvent;

pub struct SessionSubscriptions {
    store: Arc<Sessions>,
}

#[derive(Debug, Clone)]
pub struct SessionSubscriptionSpec {
    pub session_id: String,
    pub caller: Caller,
    pub scope: SubscriptionScope,
}

#[derive(Debug, Clone)]
pub enum SubscriptionScope {
    Turn { turn_id: String },
    All,
}

impl SessionSubscriptionSpec {
    fn include(&self, event: &SessionEvent) -> bool {
        match &self.scope {
            SubscriptionScope::Turn { turn_id } => {
                event.meta.turn_id.as_deref() == Some(turn_id.as_str())
            }
            SubscriptionScope::All => true,
        }
    }

    fn is_terminal(&self, event: &SessionEvent) -> bool {
        match &self.scope {
            SubscriptionScope::Turn { turn_id } => {
                event.session_id == self.session_id
                    && matches!(
                        &event.payload,
                        EventPayload::TurnCompleted(tc) if tc.turn_id == *turn_id
                    )
            }
            SubscriptionScope::All => false,
        }
    }
}

impl SessionSubscriptions {
    pub fn new(store: Arc<Sessions>) -> Self {
        Self { store }
    }

    pub async fn stream(
        &self,
        spec: SessionSubscriptionSpec,
        after: Option<Seq>,
    ) -> mpsc::Receiver<SessionEvent> {
        let mut live = self.store.subscribe();
        let history = match after {
            Some(cursor) => self.load_raw(&spec, cursor.0).await,
            None => Vec::new(),
        };
        let store = self.store.clone();
        let (tx, rx) = mpsc::channel(64);
        tokio::spawn(async move {
            let mut last = after.map(|s| s.0);
            for event in history {
                last = Some(event.seq);
                match forward(&tx, &spec, event).await {
                    Flow::Continue => {}
                    Flow::Stop => return,
                }
            }
            loop {
                let batch = tokio::select! {
                    _ = tx.closed() => return,
                    batch = live.recv() => match batch {
                        Some(batch) => batch,
                        None => return,
                    },
                };
                for event in batch.iter() {
                    if event.tenant_id != spec.caller.tenant_id()
                        || event.session_id != spec.session_id
                    {
                        continue;
                    }
                    if last.is_some_and(|last| event.seq <= last) {
                        continue;
                    }
                    let mut ordered = Vec::new();
                    if let Some(from) = last.filter(|last| event.seq > last + 1) {
                        ordered = load_raw(&store, &spec, from).await;
                        ordered.retain(|e| e.seq < event.seq);
                    }
                    ordered.push(event.clone());
                    for event in ordered {
                        last = Some(event.seq);
                        match forward(&tx, &spec, event).await {
                            Flow::Continue => {}
                            Flow::Stop => return,
                        }
                    }
                }
            }
        });
        rx
    }

    async fn load_raw(&self, spec: &SessionSubscriptionSpec, after: u64) -> Vec<SessionEvent> {
        load_raw(&self.store, spec, after).await
    }
}

enum Flow {
    Continue,
    Stop,
}

async fn forward(
    tx: &mpsc::Sender<SessionEvent>,
    spec: &SessionSubscriptionSpec,
    event: SessionEvent,
) -> Flow {
    if !spec.include(&event) {
        return Flow::Continue;
    }
    let terminal = spec.is_terminal(&event);
    if tx.send(event).await.is_err() || terminal {
        return Flow::Stop;
    }
    Flow::Continue
}

async fn load_raw(
    store: &Sessions,
    spec: &SessionSubscriptionSpec,
    after: u64,
) -> Vec<SessionEvent> {
    store
        .query_events(&EventFilter {
            tenant_id: Some(spec.caller.tenant_id().to_string()),
            session_id: Some(spec.session_id.clone()),
            after_seq: Some(Seq(after)),
            ..Default::default()
        })
        .await
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec() -> SessionSubscriptionSpec {
        SessionSubscriptionSpec {
            session_id: "s1".to_string(),
            caller: Caller::System {
                tenant_id: "t1".to_string(),
            },
            scope: SubscriptionScope::All,
        }
    }

    async fn settle() {
        for _ in 0..8 {
            tokio::task::yield_now().await;
        }
    }

    #[tokio::test]
    async fn dropping_the_stream_releases_its_hold_on_the_bus() {
        let disk = crate::runtime::sessions::testing::disk();
        let sessions = disk.sessions();
        let subs = SessionSubscriptions::new(sessions.clone());

        let rx = subs.stream(spec(), None).await;
        settle().await;
        assert_eq!(sessions.listeners(), 1, "the stream should be listening");

        drop(rx);
        settle().await;
        assert_eq!(
            sessions.listeners(),
            0,
            "a dropped stream must not keep filtering every event in the deployment"
        );
    }

    #[tokio::test]
    async fn a_stream_nobody_reads_does_not_wait_for_an_event_to_notice() {
        let disk = crate::runtime::sessions::testing::disk();
        let sessions = disk.sessions();
        let subs = SessionSubscriptions::new(sessions.clone());

        drop(subs.stream(spec(), None).await);
        settle().await;
        assert_eq!(sessions.listeners(), 0);
    }
}
