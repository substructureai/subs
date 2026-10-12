use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use tokio::sync::{broadcast, mpsc};
use tokio::task::JoinHandle;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use crate::protocol::TokenDelta;
use crate::runtime::events_url::{EventsReader, Live, LiveChunk, Target};
use crate::runtime::llm::TokenDeltaTransport;
use crate::runtime::sessions::{DeliveryError, Sessions};

pub const EVERY: Duration = Duration::from_millis(250);
const IDLE: Duration = Duration::from_secs(60);
const HERE_CAPACITY: usize = 1024;

#[async_trait]
pub trait LiveOut: Send + Sync {
    async fn target(
        &self,
        sessions: &Sessions,
        tenant_id: &str,
        session_id: &str,
    ) -> Option<Target>;

    async fn send(&self, target: &Target, live: &Live) -> Result<(), DeliveryError>;
}

#[async_trait]
impl LiveOut for EventsReader {
    async fn target(
        &self,
        sessions: &Sessions,
        tenant_id: &str,
        session_id: &str,
    ) -> Option<Target> {
        let session = sessions.load(tenant_id, session_id).await.ok()??;
        match EventsReader::target(self, &session).await {
            Ok(target) => target,
            Err(failed) => {
                tracing::debug!(%session_id, error = %failed.error, "no live target");
                None
            }
        }
    }

    async fn send(&self, target: &Target, live: &Live) -> Result<(), DeliveryError> {
        self.live(target, live).await
    }
}

pub struct LocalTap {
    inner: Arc<dyn TokenDeltaTransport>,
    here: broadcast::Sender<TokenDelta>,
}

impl LocalTap {
    pub fn new(inner: Arc<dyn TokenDeltaTransport>) -> Self {
        let (here, _) = broadcast::channel(HERE_CAPACITY);
        Self { inner, here }
    }

    pub fn here(&self) -> broadcast::Receiver<TokenDelta> {
        self.here.subscribe()
    }
}

#[async_trait]
impl TokenDeltaTransport for LocalTap {
    async fn publish(&self, delta: TokenDelta) {
        if self.here.receiver_count() > 0 {
            let _ = self.here.send(delta.clone());
        }
        self.inner.publish(delta).await;
    }

    async fn subscribe(
        &self,
        tenant_id: &str,
        root_session_id: &str,
    ) -> mpsc::Receiver<TokenDelta> {
        self.inner.subscribe(tenant_id, root_session_id).await
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct Key {
    tenant_id: String,
    session_id: String,
    call_id: String,
    attempt: u32,
}

struct Outlet {
    target: Arc<Target>,
    unconfirmed: Vec<LiveChunk>,
    sending: bool,
}

struct Call {
    turn_id: Option<String>,
    outlets: Option<Vec<Outlet>>,
    waiting: Vec<LiveChunk>,
    done: bool,
    seen: Instant,
}

impl Call {
    fn finished(&self, now: Instant) -> bool {
        let idle = now.duration_since(self.seen) > IDLE;
        match &self.outlets {
            None => idle,
            Some(outlets) => {
                let quiet = outlets.iter().all(|o| !o.sending);
                let sent = outlets.iter().all(|o| o.unconfirmed.is_empty());
                quiet && ((self.done && sent) || idle)
            }
        }
    }
}

enum Report {
    Found(Key, Option<Arc<Target>>),
    Sent {
        key: Key,
        outlet: usize,
        count: usize,
        ok: bool,
    },
}

pub fn spawn_live(
    sessions: Arc<Sessions>,
    tap: &LocalTap,
    out: Arc<dyn LiveOut>,
    every: Duration,
    cancel: CancellationToken,
) -> JoinHandle<()> {
    let mut deltas = tap.here();
    tokio::spawn(async move {
        let (report, mut reports) = mpsc::unbounded_channel();
        let mut calls: HashMap<Key, Call> = HashMap::new();
        let mut tick = tokio::time::interval(every);
        loop {
            tokio::select! {
                _ = cancel.cancelled() => return,
                delta = deltas.recv() => match delta {
                    Ok(delta) => take(&mut calls, &sessions, &out, &report, delta),
                    Err(broadcast::error::RecvError::Lagged(missed)) => {
                        tracing::debug!(missed, "live text fell behind");
                    }
                    Err(broadcast::error::RecvError::Closed) => return,
                },
                Some(done) = reports.recv() => settle(&mut calls, done),
                _ = tick.tick() => flush(&mut calls, &out, &report),
            }
        }
    })
}

fn chunk_of(delta: &TokenDelta) -> Option<LiveChunk> {
    if delta.text.is_none() && delta.reasoning.is_none() && delta.finish_reason.is_none() {
        return None;
    }
    Some(LiveChunk {
        seq: delta.seq,
        text: delta.text.clone(),
        reasoning: delta.reasoning.clone(),
        finish_reason: delta.finish_reason.clone(),
    })
}

fn take(
    calls: &mut HashMap<Key, Call>,
    sessions: &Arc<Sessions>,
    out: &Arc<dyn LiveOut>,
    report: &mpsc::UnboundedSender<Report>,
    delta: TokenDelta,
) {
    let key = Key {
        tenant_id: delta.tenant_id.clone(),
        session_id: delta.session_id.clone(),
        call_id: delta.call_id.clone(),
        attempt: delta.attempt,
    };
    let call = calls.entry(key.clone()).or_insert_with(|| {
        let (sessions, out, report, key) =
            (sessions.clone(), out.clone(), report.clone(), key.clone());
        tokio::spawn(async move {
            let found = out.target(&sessions, &key.tenant_id, &key.session_id).await;
            let _ = report.send(Report::Found(key, found.map(Arc::new)));
        });
        Call {
            turn_id: delta.turn_id.clone(),
            outlets: None,
            waiting: Vec::new(),
            done: false,
            seen: Instant::now(),
        }
    });
    call.seen = Instant::now();
    call.done |= delta.finish_reason.is_some();
    let Some(chunk) = chunk_of(&delta) else {
        return;
    };
    match &mut call.outlets {
        Some(outlets) => {
            for outlet in outlets {
                outlet.unconfirmed.push(chunk.clone());
            }
        }
        None => call.waiting.push(chunk),
    }
}

fn settle(calls: &mut HashMap<Key, Call>, report: Report) {
    match report {
        Report::Found(key, target) => {
            let Some(call) = calls.get_mut(&key) else {
                return;
            };
            let waiting = std::mem::take(&mut call.waiting);
            call.outlets = Some(
                target
                    .into_iter()
                    .map(|target| Outlet {
                        target,
                        unconfirmed: waiting.clone(),
                        sending: false,
                    })
                    .collect(),
            );
        }
        Report::Sent {
            key,
            outlet,
            count,
            ok,
        } => {
            let Some(outlet) = calls
                .get_mut(&key)
                .and_then(|call| call.outlets.as_mut())
                .and_then(|outlets| outlets.get_mut(outlet))
            else {
                return;
            };
            outlet.sending = false;
            if ok {
                let count = count.min(outlet.unconfirmed.len());
                outlet.unconfirmed.drain(..count);
            }
        }
    }
}

fn flush(
    calls: &mut HashMap<Key, Call>,
    out: &Arc<dyn LiveOut>,
    report: &mpsc::UnboundedSender<Report>,
) {
    let now = Instant::now();
    calls.retain(|_, call| !call.finished(now));
    for (key, call) in calls.iter_mut() {
        let Some(outlets) = call.outlets.as_mut() else {
            continue;
        };
        for (index, outlet) in outlets.iter_mut().enumerate() {
            if outlet.sending || outlet.unconfirmed.is_empty() {
                continue;
            }
            outlet.sending = true;
            let live = Live {
                turn_id: call.turn_id.clone(),
                call_id: key.call_id.clone(),
                attempt: key.attempt,
                chunks: outlet.unconfirmed.clone(),
                done: call.done,
            };
            let (target, out, report, key) = (
                outlet.target.clone(),
                out.clone(),
                report.clone(),
                key.clone(),
            );
            tokio::spawn(async move {
                let count = live.chunks.len();
                let ok = match out.send(&target, &live).await {
                    Ok(()) => true,
                    Err(failed) => {
                        tracing::debug!(session_id = %target.about.session_id, call_id = %live.call_id, error = %failed.error, "live text was not taken");
                        false
                    }
                };
                let _ = report.send(Report::Sent {
                    key,
                    outlet: index,
                    count,
                    ok,
                });
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;
    use crate::runtime::llm::InMemoryTokenDeltaTransport;
    use crate::runtime::sessions::testing::{disk, Disk};

    struct Screen {
        fail_first: AtomicUsize,
        sent: mpsc::UnboundedSender<(Live, bool)>,
    }

    #[async_trait]
    impl LiveOut for Screen {
        async fn target(
            &self,
            _sessions: &Sessions,
            tenant_id: &str,
            session_id: &str,
        ) -> Option<Target> {
            (session_id == "s1").then(|| Target {
                id: "app".into(),
                tenant_id: tenant_id.into(),
                url: "http://app.test/events".into(),
                timeout: crate::runtime::events_url::DEFAULT_TIMEOUT,
                secret: String::new(),
                about: crate::runtime::events_url::About {
                    session_id: session_id.into(),
                    ..Default::default()
                },
            })
        }

        async fn send(&self, _target: &Target, live: &Live) -> Result<(), DeliveryError> {
            let left = self.fail_first.load(Ordering::SeqCst);
            let ok = left == 0;
            if !ok {
                self.fail_first.store(left - 1, Ordering::SeqCst);
            }
            let _ = self.sent.send((live.clone(), ok));
            match ok {
                true => Ok(()),
                false => Err(DeliveryError::retry(None, "busy")),
            }
        }
    }

    struct Running {
        _disk: Disk,
        shared: Arc<InMemoryTokenDeltaTransport>,
        tap: LocalTap,
        sent: mpsc::UnboundedReceiver<(Live, bool)>,
    }

    fn running(fail_first: usize) -> Running {
        let disk = disk();
        let (sent, received) = mpsc::unbounded_channel();
        let shared = Arc::new(InMemoryTokenDeltaTransport::new());
        let tap = LocalTap::new(shared.clone());
        spawn_live(
            disk.sessions(),
            &tap,
            Arc::new(Screen {
                fail_first: AtomicUsize::new(fail_first),
                sent,
            }),
            Duration::from_millis(20),
            CancellationToken::new(),
        );
        Running {
            _disk: disk,
            shared,
            tap,
            sent: received,
        }
    }

    impl Running {
        async fn until_done(&mut self) -> Vec<(Live, bool)> {
            let mut got = Vec::new();
            loop {
                let next = tokio::time::timeout(Duration::from_secs(5), self.sent.recv())
                    .await
                    .expect("live text arrives")
                    .expect("the sender is open");
                let finished = next.0.done && next.1;
                got.push(next);
                if finished {
                    return got;
                }
            }
        }

        async fn nothing_sent(&mut self) -> bool {
            tokio::time::timeout(Duration::from_millis(200), self.sent.recv())
                .await
                .is_err()
        }
    }

    fn delta(session_id: &str, seq: u32, text: &str, finish: bool) -> TokenDelta {
        TokenDelta {
            tenant_id: "t".into(),
            root_session_id: session_id.into(),
            session_id: session_id.into(),
            agent_id: "agent".into(),
            turn_id: Some("turn-1".into()),
            call_id: "llm-1".into(),
            attempt: 0,
            seq,
            text: Some(text.into()),
            reasoning: None,
            tool_calls: Vec::new(),
            finish_reason: finish.then(|| "stop".to_string()),
        }
    }

    fn applied(got: &[(Live, bool)]) -> Vec<u32> {
        let mut last = 0;
        let mut kept = Vec::new();
        for (live, ok) in got {
            if !ok {
                continue;
            }
            for chunk in &live.chunks {
                if chunk.seq > last {
                    last = chunk.seq;
                    kept.push(chunk.seq);
                }
            }
        }
        kept
    }

    #[tokio::test]
    async fn chunks_reach_the_events_url_in_order_and_the_last_says_done() {
        let mut live = running(0);
        for (seq, text) in ["Log", "ged ", "120", "ml."].iter().enumerate() {
            live.tap
                .publish(delta("s1", seq as u32 + 1, text, seq == 3))
                .await;
        }

        let got = live.until_done().await;

        assert_eq!(applied(&got), vec![1, 2, 3, 4]);
        assert!(got
            .iter()
            .all(|(l, _)| l.call_id == "llm-1" && l.turn_id.as_deref() == Some("turn-1")));
    }

    #[tokio::test]
    async fn chunks_not_taken_are_sent_again() {
        let mut live = running(2);
        live.tap.publish(delta("s1", 1, "Hi", false)).await;
        live.tap.publish(delta("s1", 2, " there", true)).await;

        let got = live.until_done().await;

        let failed: Vec<&(Live, bool)> = got.iter().filter(|(_, ok)| !ok).collect();
        assert_eq!(failed.len(), 2);
        let first_ok = got.iter().find(|(_, ok)| *ok).expect("a send got through");
        assert_eq!(first_ok.0.chunks[0].seq, 1, "the first chunk is sent again");
        assert_eq!(applied(&got), vec![1, 2]);
    }

    #[tokio::test]
    async fn a_session_with_no_events_url_sends_nothing() {
        let mut live = running(0);
        live.tap.publish(delta("other", 1, "Hi", true)).await;

        assert!(live.nothing_sent().await);
    }

    #[tokio::test]
    async fn text_made_in_another_process_is_not_sent_from_here() {
        let mut live = running(0);
        live.shared.publish(delta("s1", 1, "Hi", true)).await;

        assert!(live.nothing_sent().await);
    }
}
