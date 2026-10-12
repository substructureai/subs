use std::hash::{DefaultHasher, Hash, Hasher};
use std::sync::{Arc, OnceLock, Weak};

use async_trait::async_trait;
use chrono::{DateTime, TimeDelta, Utc};
use substructure_session::delivery::{
    unreachable_retry, Delivered, Failure, Start, Subscription, Unsent, WorkReader,
};
use substructure_session::{Command, CommandResult, Entry, Session, Settings, Store, Work, WorkId};
use tokio::sync::{mpsc, oneshot};

use crate::protocol::MessageTree;

use crate::runtime::event_store::{BroadcastBus, EventBus, EventFilter, EventTap, StoreError};
use crate::runtime::inbox::Inbox;
pub use crate::runtime::inbox::{execute, ExecuteError, ExecuteInput, ExecuteOutput};
use crate::runtime::session::SessionEvent;
use crate::runtime::wake::Alarms;

pub type SharedStore = Arc<dyn Store + Send + Sync>;

pub type SessionKey = (String, String);

pub trait SessionStores: Send + Sync {
    fn open(&self, tenant_id: &str, session_id: &str) -> SharedStore;
    fn behind(&self, readers: &[(String, Start)]) -> Result<Vec<SessionKey>, StoreError>;
}

#[derive(Default, Debug, Clone)]
pub struct DeliveryError {
    pub at: Option<u64>,
    pub error: String,
    pub permanent: bool,
    pub retry_after: Option<TimeDelta>,
}

impl DeliveryError {
    pub fn retry(at: Option<u64>, error: impl Into<String>) -> Self {
        Self {
            at,
            error: error.into(),
            permanent: false,
            retry_after: None,
        }
    }

    pub fn permanent(at: Option<u64>, error: impl Into<String>) -> Self {
        Self {
            permanent: true,
            ..Self::retry(at, error)
        }
    }
}

impl From<String> for DeliveryError {
    fn from(error: String) -> Self {
        Self::retry(None, error)
    }
}

#[async_trait]
pub trait Reader: Send + Sync {
    fn subscription(&self) -> Subscription;

    async fn deliver(
        &self,
        sessions: &Sessions,
        session: &Session,
        events: &[SessionEvent],
    ) -> Result<(), DeliveryError>;
}

#[async_trait]
pub trait WorkSink: Send + Sync {
    fn reader(&self) -> WorkReader;

    async fn send(&self, sessions: &Sessions, session: &Session, work: Vec<Work>) -> Vec<Unsent>;
}

pub struct Hint {
    pub tenant_id: String,
    pub session_id: String,
    pub session: Option<Session>,
    pub alarm: bool,
    pub restart: bool,
    pub now: DateTime<Utc>,
}

impl Hint {
    fn of(session: Session, alarm: bool, now: DateTime<Utc>) -> Self {
        Self {
            tenant_id: session.tenant_id().to_string(),
            session_id: session.id().to_string(),
            session: Some(session),
            alarm,
            restart: false,
            now,
        }
    }
}

pub struct OutsideHint {
    pub tenant_id: String,
    pub session_id: String,
    pub reader: String,
    pub session: Option<Arc<Session>>,
    pub now: DateTime<Utc>,
}

#[async_trait]
pub trait Dispatch: Send + Sync {
    fn send(&self, sessions: &Weak<Sessions>, hint: Hint);

    fn send_outside(&self, sessions: &Weak<Sessions>, hint: OutsideHint);

    async fn flushed(&self);
}

const SHARDS: usize = 16;
const OUTSIDE_SHARDS: usize = 64;
pub const DEFAULT_SEND_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

#[derive(Debug, Clone)]
pub struct SessionsConfig {
    pub session: Settings,
    pub send_timeout: std::time::Duration,
}

impl Default for SessionsConfig {
    fn default() -> Self {
        Self {
            session: Settings::default(),
            send_timeout: DEFAULT_SEND_TIMEOUT,
        }
    }
}

enum Job {
    Deliver(Box<Hint>),
    Outside(Box<OutsideHint>),
    Flush(oneshot::Sender<()>),
}

#[derive(Default)]
pub struct Shards {
    main: OnceLock<Vec<mpsc::UnboundedSender<Job>>>,
    outside: OnceLock<Vec<mpsc::UnboundedSender<Job>>>,
}

fn shard_of(
    shards: &[mpsc::UnboundedSender<Job>],
    key: impl Hash,
) -> Option<&mpsc::UnboundedSender<Job>> {
    let mut hasher = DefaultHasher::new();
    key.hash(&mut hasher);
    shards.get(hasher.finish() as usize % shards.len().max(1))
}

async fn flush(shards: Option<&Vec<mpsc::UnboundedSender<Job>>>) {
    for shard in shards.into_iter().flatten() {
        let (done, wait) = oneshot::channel();
        if shard.send(Job::Flush(done)).is_ok() {
            let _ = wait.await;
        }
    }
}

impl Shards {
    fn spawn(sessions: &Weak<Sessions>, count: usize) -> Vec<mpsc::UnboundedSender<Job>> {
        (0..count)
            .map(|_| {
                let (tx, mut rx) = mpsc::unbounded_channel();
                let sessions = sessions.clone();
                tokio::spawn(async move {
                    while let Some(job) = rx.recv().await {
                        match job {
                            Job::Deliver(hint) => {
                                let Some(sessions) = sessions.upgrade() else {
                                    return;
                                };
                                sessions.deliver(*hint).await;
                            }
                            Job::Outside(hint) => {
                                let Some(sessions) = sessions.upgrade() else {
                                    return;
                                };
                                sessions.deliver_outside(*hint).await;
                            }
                            Job::Flush(done) => {
                                let _ = done.send(());
                            }
                        }
                    }
                });
                tx
            })
            .collect()
    }
}

#[async_trait]
impl Dispatch for Shards {
    fn send(&self, sessions: &Weak<Sessions>, hint: Hint) {
        let shards = self.main.get_or_init(|| Self::spawn(sessions, SHARDS));
        let Some(shard) = shard_of(shards, (&hint.tenant_id, &hint.session_id)) else {
            return;
        };
        if shard.send(Job::Deliver(Box::new(hint))).is_err() {
            tracing::warn!("a delivery shard stopped; the next boot catches up");
        }
    }

    fn send_outside(&self, sessions: &Weak<Sessions>, hint: OutsideHint) {
        let shards = self
            .outside
            .get_or_init(|| Self::spawn(sessions, OUTSIDE_SHARDS));
        let key = (&hint.tenant_id, &hint.session_id, &hint.reader);
        let Some(shard) = shard_of(shards, key) else {
            return;
        };
        if shard.send(Job::Outside(Box::new(hint))).is_err() {
            tracing::warn!("an outside shard stopped; the next boot catches up");
        }
    }

    async fn flushed(&self) {
        flush(self.main.get()).await;
        flush(self.outside.get()).await;
    }
}

struct Stop {
    session: Option<Session>,
    error: substructure_session::Error,
}

pub struct Sessions {
    stores: Arc<dyn SessionStores>,
    alarms: Arc<dyn Alarms>,
    readers: Vec<(Subscription, Arc<dyn Reader>)>,
    outside: Vec<(Subscription, Arc<dyn Reader>)>,
    work: Option<(WorkReader, Arc<dyn WorkSink>)>,
    settings: Settings,
    send_timeout: std::time::Duration,
    bus: BroadcastBus,
    me: Weak<Sessions>,
    dispatch: Arc<dyn Dispatch>,
}

impl Sessions {
    pub fn new(
        stores: Arc<dyn SessionStores>,
        alarms: Arc<dyn Alarms>,
        readers: Vec<Arc<dyn Reader>>,
        outside: Vec<Arc<dyn Reader>>,
        work: Option<Arc<dyn WorkSink>>,
        config: SessionsConfig,
    ) -> Arc<Self> {
        Self::with_dispatch(
            stores,
            alarms,
            readers,
            outside,
            work,
            Arc::new(Shards::default()),
            config,
        )
    }

    pub fn with_dispatch(
        stores: Arc<dyn SessionStores>,
        alarms: Arc<dyn Alarms>,
        readers: Vec<Arc<dyn Reader>>,
        outside: Vec<Arc<dyn Reader>>,
        work: Option<Arc<dyn WorkSink>>,
        dispatch: Arc<dyn Dispatch>,
        config: SessionsConfig,
    ) -> Arc<Self> {
        Arc::new_cyclic(|me| Self {
            stores,
            alarms,
            readers: readers
                .into_iter()
                .map(|reader| (reader.subscription(), reader))
                .collect(),
            outside: outside
                .into_iter()
                .map(|reader| (reader.subscription(), reader))
                .collect(),
            work: work.map(|sink| (sink.reader(), sink)),
            settings: config.session,
            send_timeout: config.send_timeout,
            bus: BroadcastBus::default(),
            me: me.clone(),
            dispatch,
        })
    }

    pub async fn run(
        &self,
        tenant_id: &str,
        session_id: &str,
        command: Command,
    ) -> Result<CommandResult, substructure_session::Error> {
        let store = self.stores.open(tenant_id, session_id);
        let (id, tenant, settings) = (
            session_id.to_string(),
            tenant_id.to_string(),
            self.settings.clone(),
        );
        let now = Utc::now();
        let (session, ran) = blocking(store, move |store| {
            let mut session = Session::load(store, id, tenant, settings)?;
            let ran = session.run(store, now, command)?;
            Ok((session, ran))
        })
        .await?;
        if let Some(entry) = ran.written {
            self.publish(vec![entry]);
            self.dispatch
                .send(&self.me, Hint::of(session, ran.wake_moved, now));
        }
        Ok(ran.result)
    }

    pub async fn execute(&self, input: ExecuteInput) -> Result<ExecuteOutput, ExecuteError> {
        execute(self, input).await
    }

    pub async fn wake(&self, tenant_id: &str, session_id: &str, now: DateTime<Utc>) {
        let store = self.stores.open(tenant_id, session_id);
        let (id, tenant, settings) = (
            session_id.to_string(),
            tenant_id.to_string(),
            self.settings.clone(),
        );
        let woken = blocking(store, move |store| {
            let mut session = Session::load(store, id, tenant, settings)?;
            let written = session.wake(store, now)?;
            Ok((session, written))
        })
        .await;
        match woken {
            Ok((session, written)) => {
                self.publish(written.into_iter().collect());
                self.dispatch.send(&self.me, Hint::of(session, true, now));
            }
            Err(error) => {
                tracing::warn!(%tenant_id, %session_id, %error, "waking a session failed")
            }
        }
    }

    pub async fn resume_delivery(
        &self,
        tenant_id: &str,
        session_id: &str,
        reader: &str,
    ) -> Result<bool, substructure_session::Error> {
        let store = self.stores.open(tenant_id, session_id);
        let (id, tenant, settings, reader) = (
            session_id.to_string(),
            tenant_id.to_string(),
            self.settings.clone(),
            reader.to_string(),
        );
        let now = Utc::now();
        let (session, written) = blocking(store, move |store| {
            let mut session = Session::load(store, id, tenant, settings)?;
            let written = session.resume_delivery(store, &reader, now)?;
            Ok((session, written))
        })
        .await?;
        if written.is_empty() {
            return Ok(false);
        }
        self.publish(written);
        self.dispatch.send(&self.me, Hint::of(session, true, now));
        Ok(true)
    }

    pub async fn behind(&self) -> Result<Vec<SessionKey>, StoreError> {
        let mut readers: Vec<(String, Start)> = self
            .readers
            .iter()
            .map(|(sub, _)| (sub.name.clone(), sub.start))
            .collect();
        if let Some((work, _)) = &self.work {
            readers.push((work.name.clone(), Start::Beginning));
        }
        readers.extend(
            self.outside
                .iter()
                .map(|(sub, _)| (sub.name.clone(), sub.start)),
        );
        self.stores.behind(&readers)
    }

    pub async fn resume(&self, sessions: impl IntoIterator<Item = SessionKey>) {
        let now = Utc::now();
        for (tenant_id, session_id) in sessions {
            self.dispatch.send(
                &self.me,
                Hint {
                    tenant_id,
                    session_id,
                    session: None,
                    alarm: true,
                    restart: true,
                    now,
                },
            );
        }
    }

    pub async fn flushed(&self) {
        self.dispatch.flushed().await;
    }

    pub async fn load(
        &self,
        tenant_id: &str,
        session_id: &str,
    ) -> Result<Option<Session>, substructure_session::Error> {
        let store = self.stores.open(tenant_id, session_id);
        let (id, tenant, settings) = (
            session_id.to_string(),
            tenant_id.to_string(),
            self.settings.clone(),
        );
        let session = blocking(store, move |store| {
            Session::load(store, id, tenant, settings)
        })
        .await?;
        Ok(session.exists().then_some(session))
    }

    pub async fn is_open(&self, tenant_id: &str, session_id: &str, work: &WorkId) -> bool {
        match self.load(tenant_id, session_id).await {
            Ok(Some(session)) => session.is_open(work),
            Ok(None) => false,
            Err(error) => {
                tracing::warn!(%tenant_id, %session_id, %error, "checking work failed; running it");
                true
            }
        }
    }

    pub async fn query_events(
        &self,
        filter: &EventFilter,
    ) -> Result<Vec<SessionEvent>, StoreError> {
        let (Some(tenant_id), Some(session_id)) = (&filter.tenant_id, &filter.session_id) else {
            return Err(StoreError::Other(
                "events are read one session at a time".into(),
            ));
        };
        let store = self.stores.open(tenant_id, session_id);
        let after = filter.after_seq.map_or(0, |s| s.0);
        let limit = filter.limit.unwrap_or(usize::MAX);
        blocking(store, move |store| Session::events(store, after, limit))
            .await
            .map_err(|e| StoreError::Other(e.to_string()))
    }

    pub async fn whole_tree(
        &self,
        tenant_id: &str,
        session_id: &str,
        head_id: Option<String>,
    ) -> Result<MessageTree, StoreError> {
        let store = self.stores.open(tenant_id, session_id);
        let nodes = blocking(store, move |store| Ok(store.messages()?))
            .await
            .map_err(|e| StoreError::Other(e.to_string()))?;
        Ok(MessageTree { nodes, head_id })
    }

    pub fn subscribe(&self) -> EventTap {
        self.bus.subscribe()
    }

    pub fn listeners(&self) -> usize {
        self.bus.listeners()
    }

    fn publish(&self, written: Vec<Entry>) {
        for entry in written {
            self.bus.publish(Arc::new(entry.events));
        }
    }

    pub async fn deliver(&self, hint: Hint) {
        let Hint {
            tenant_id,
            session_id,
            session,
            alarm,
            restart,
            now,
        } = hint;
        let session = match session {
            Some(session) => session,
            None => match self.load(&tenant_id, &session_id).await {
                Ok(Some(session)) => session,
                Ok(None) => return,
                Err(error) => {
                    tracing::warn!(%tenant_id, %session_id, %error, "loading a session to deliver failed");
                    return;
                }
            },
        };
        let version = session.version();
        let store = self.stores.open(&tenant_id, &session_id);
        match self.deliver_all(&store, session, restart, now).await {
            Ok(session) if alarm || session.version() != version => {
                self.set_alarm(
                    &tenant_id,
                    &session_id,
                    session.version(),
                    session.wake_at(),
                )
                .await
            }
            Ok(_) => {}
            Err(Stop { session, error }) => {
                tracing::warn!(%tenant_id, %session_id, %error, "delivering stopped; an alarm retries it");
                let retry = now.checked_add_signed(unreachable_retry());
                let (version, wake_at) = session
                    .map(|s| (s.version(), s.wake_at()))
                    .unwrap_or((version, None));
                let at = match (wake_at, retry) {
                    (Some(a), Some(b)) => Some(a.min(b)),
                    (a, b) => a.or(b),
                };
                self.set_alarm(&tenant_id, &session_id, version, at).await
            }
        }
    }

    async fn set_alarm(
        &self,
        tenant_id: &str,
        session_id: &str,
        version: u64,
        at: Option<DateTime<Utc>>,
    ) {
        if let Err(error) = self.alarms.set(tenant_id, session_id, version, at).await {
            tracing::warn!(%tenant_id, %session_id, %error, "setting the alarm failed");
        }
    }

    async fn deliver_all(
        &self,
        store: &SharedStore,
        mut session: Session,
        restart: bool,
        now: DateTime<Utc>,
    ) -> Result<Session, Stop> {
        if let Some((reader, sink)) = &self.work {
            let mut restarted = restart;
            loop {
                let r = reader.clone();
                let (s, next) = step(store, session, move |store, s| {
                    s.next_work(store, &r, restarted)
                })
                .await?;
                session = s;
                restarted = false;
                self.publish(next.written);
                let Some(mut batch) = next.batch else {
                    break;
                };
                let items = std::mem::take(&mut batch.items);
                let ids: Vec<WorkId> = items.iter().map(|w| w.id().clone()).collect();
                let unsent =
                    match tokio::time::timeout(self.send_timeout, sink.send(self, &session, items))
                        .await
                    {
                        Ok(unsent) => unsent,
                        Err(_) => ids
                            .into_iter()
                            .map(|work| Unsent {
                                work,
                                error: "sending work timed out".to_string(),
                                permanent: false,
                            })
                            .collect(),
                    };
                let r = reader.clone();
                let (s, written) = step(store, session, move |store, s| {
                    s.ack_work(store, &r, &batch, unsent, now)
                })
                .await?;
                session = s;
                self.publish(written);
            }
        }
        for (subscription, reader) in &self.readers {
            session = self
                .deliver_to(store, session, subscription, reader.as_ref(), now)
                .await?;
        }
        let shared = Arc::new(session.clone());
        for (subscription, _) in &self.outside {
            self.dispatch.send_outside(
                &self.me,
                OutsideHint {
                    tenant_id: session.tenant_id().to_string(),
                    session_id: session.id().to_string(),
                    reader: subscription.name.clone(),
                    session: Some(shared.clone()),
                    now,
                },
            );
        }
        Ok(session)
    }
}

impl Sessions {
    pub async fn deliver_outside(&self, hint: OutsideHint) {
        let OutsideHint {
            tenant_id,
            session_id,
            reader,
            session,
            now,
        } = hint;
        let Some((subscription, reader)) = self
            .outside
            .iter()
            .find(|(subscription, _)| subscription.name == reader)
        else {
            return;
        };
        let session = match session {
            Some(session) => Arc::unwrap_or_clone(session),
            None => match self.load(&tenant_id, &session_id).await {
                Ok(Some(session)) => session,
                Ok(None) => return,
                Err(error) => {
                    tracing::warn!(%tenant_id, %session_id, %error, "loading a session to deliver outside failed");
                    return;
                }
            },
        };
        let version = session.version();
        let store = self.stores.open(&tenant_id, &session_id);
        match self
            .deliver_to(&store, session, subscription, reader.as_ref(), now)
            .await
        {
            Ok(session) if session.version() != version => {
                self.dispatch.send(&self.me, Hint::of(session, true, now))
            }
            Ok(_) => {}
            Err(Stop { session, error }) => {
                tracing::warn!(%tenant_id, %session_id, reader = %subscription.name, %error, "delivering outside stopped; an alarm retries it");
                let retry = now.checked_add_signed(unreachable_retry());
                let (version, wake_at) = session
                    .map(|s| (s.version(), s.wake_at()))
                    .unwrap_or((version, None));
                let at = match (wake_at, retry) {
                    (Some(a), Some(b)) => Some(a.min(b)),
                    (a, b) => a.or(b),
                };
                self.set_alarm(&tenant_id, &session_id, version, at).await
            }
        }
    }

    async fn deliver_to(
        &self,
        store: &SharedStore,
        mut session: Session,
        subscription: &Subscription,
        reader: &dyn Reader,
        now: DateTime<Utc>,
    ) -> Result<Session, Stop> {
        loop {
            let sub = subscription.clone();
            let (s, next) = step(store, session, move |store, s| {
                s.next_events(store, &sub, now)
            })
            .await?;
            session = s;
            self.publish(next.written);
            let Some(batch) = next.batch else {
                return Ok(session);
            };
            let sent = tokio::time::timeout(
                self.send_timeout,
                reader.deliver(self, &session, &batch.items),
            )
            .await
            .unwrap_or_else(|_| Err(DeliveryError::retry(None, "delivery timed out")));
            let delivered = match sent {
                Ok(()) => Delivered::All,
                Err(failed) => {
                    tracing::warn!(subscription = %subscription.name, session_id = %session.id(), error = %failed.error, "a delivery failed");
                    Delivered::Failed(Failure {
                        item: failed
                            .at
                            .and_then(|seq| batch.items.iter().position(|e| e.seq == seq))
                            .unwrap_or(0),
                        error: failed.error,
                        permanent: failed.permanent,
                        retry_after: failed.retry_after,
                    })
                }
            };
            let failed = matches!(delivered, Delivered::Failed(_));
            let sub = subscription.clone();
            let (s, written) = step(store, session, move |store, s| {
                s.ack_events(store, &sub, &batch, delivered, now)
            })
            .await?;
            session = s;
            self.publish(written);
            if failed {
                return Ok(session);
            }
        }
    }
}

#[async_trait]
impl Inbox for Sessions {
    async fn submit(
        &self,
        tenant_id: &str,
        session_id: &str,
        command: Command,
    ) -> Result<CommandResult, substructure_session::Error> {
        self.run(tenant_id, session_id, command).await
    }

    async fn is_open(&self, tenant_id: &str, session_id: &str, work: &WorkId) -> bool {
        Sessions::is_open(self, tenant_id, session_id, work).await
    }
}

async fn step<T: Send + 'static>(
    store: &SharedStore,
    mut session: Session,
    f: impl FnOnce(&dyn Store, &mut Session) -> Result<T, substructure_session::Error> + Send + 'static,
) -> Result<(Session, T), Stop> {
    let store = store.clone();
    let joined = tokio::task::spawn_blocking(move || {
        let result = f(store.as_ref(), &mut session);
        (session, result)
    })
    .await;
    match joined {
        Ok((session, Ok(value))) => Ok((session, value)),
        Ok((session, Err(error))) => Err(Stop {
            session: Some(session),
            error,
        }),
        Err(e) => Err(Stop {
            session: None,
            error: substructure_session::Error::Store(
                substructure_session::StoreError::Unavailable(e.to_string()),
            ),
        }),
    }
}

async fn blocking<T: Send + 'static>(
    store: SharedStore,
    f: impl FnOnce(&dyn Store) -> Result<T, substructure_session::Error> + Send + 'static,
) -> Result<T, substructure_session::Error> {
    tokio::task::spawn_blocking(move || f(store.as_ref()))
        .await
        .unwrap_or_else(|e| {
            Err(substructure_session::Error::Store(
                substructure_session::StoreError::Unavailable(e.to_string()),
            ))
        })
}

#[cfg(test)]
pub(crate) mod testing {
    use super::*;
    use crate::providers::sqlite::{SqliteAlarms, SqliteDb, SqliteSessionStores};

    pub(crate) struct Disk {
        _dir: tempfile::TempDir,
        pub(crate) db: SqliteDb,
    }

    pub(crate) fn disk() -> Disk {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("subs.db");
        let db = SqliteDb::open(path.to_str().unwrap(), std::time::Duration::from_secs(5)).unwrap();
        Disk { _dir: dir, db }
    }

    impl Disk {
        pub(crate) fn stores(&self) -> Arc<SqliteSessionStores> {
            Arc::new(SqliteSessionStores::new(self.db.clone()))
        }

        pub(crate) fn alarms(&self) -> Arc<SqliteAlarms> {
            Arc::new(SqliteAlarms::new(self.db.clone()))
        }

        pub(crate) fn with(
            &self,
            readers: Vec<Arc<dyn Reader>>,
            outside: Vec<Arc<dyn Reader>>,
            work: Option<Arc<dyn WorkSink>>,
        ) -> Arc<Sessions> {
            Sessions::new(
                self.stores(),
                self.alarms(),
                readers,
                outside,
                work,
                SessionsConfig::default(),
            )
        }

        pub(crate) fn sessions(&self) -> Arc<Sessions> {
            self.with(Vec::new(), Vec::new(), None)
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, Ordering};

    use substructure_session::delivery::AtLimit;

    use std::collections::HashMap;
    use std::sync::Mutex;

    use super::*;
    use crate::protocol::{Issuer, Requester, RetryPolicy, SessionOwner};
    use crate::runtime::caller::Caller;
    use crate::runtime::session::command::{CommandPayload, SessionError};
    use crate::runtime::span::SpanContext;

    #[derive(Default)]
    struct Seen {
        tail: bool,
        failing: AtomicBool,
        seqs: Mutex<Vec<u64>>,
    }

    #[async_trait]
    impl Reader for Seen {
        fn subscription(&self) -> Subscription {
            match self.tail {
                true => Subscription::new("tail", Start::Joined, AtLimit::Skip),
                false => Subscription::new("seen", Start::Beginning, AtLimit::Skip),
            }
        }

        async fn deliver(
            &self,
            _sessions: &Sessions,
            _session: &Session,
            events: &[SessionEvent],
        ) -> Result<(), DeliveryError> {
            if self.failing.load(Ordering::SeqCst) {
                return Err(DeliveryError::retry(None, "down"));
            }
            self.seqs
                .lock()
                .unwrap()
                .extend(events.iter().map(|e| e.seq));
            Ok(())
        }
    }

    fn sessions(
        stores: Arc<crate::providers::sqlite::SqliteSessionStores>,
        alarms: Arc<crate::providers::sqlite::SqliteAlarms>,
        readers: Vec<Arc<dyn Reader>>,
    ) -> Arc<Sessions> {
        Sessions::new(
            stores,
            alarms,
            readers,
            Vec::new(),
            None,
            SessionsConfig::default(),
        )
    }

    fn input(payload: CommandPayload) -> ExecuteInput {
        ExecuteInput {
            id: None,
            session_id: "s1".into(),
            caller: Caller::System {
                tenant_id: "t".into(),
            },
            command: payload,
            span: SpanContext::root(),
        }
    }

    fn create() -> ExecuteInput {
        input(CommandPayload::CreateSession {
            agent_id: "agent".into(),
            owner: SessionOwner {
                tenant_id: "t".into(),
                requester: Requester::new(
                    crate::protocol::Subject::new(Issuer::app(), "u".to_string()),
                    Default::default(),
                ),
                metadata: HashMap::new(),
            },
            ancestry: vec![],
            worker_retry: RetryPolicy::no_retry(),
            agent: None,
            worker: None,
            events: None,
            state: None,
        })
    }

    fn interrupt(n: u32) -> ExecuteInput {
        input(CommandPayload::Interrupt {
            interrupt_id: format!("i{n}"),
            reason: "test".into(),
            payload: serde_json::Value::Null,
        })
    }

    #[tokio::test]
    async fn a_command_runs_and_a_second_create_is_refused() {
        let disk = testing::disk();
        let s = disk.sessions();
        s.execute(create()).await.unwrap();
        let again = s.execute(create()).await;

        assert!(matches!(
            again,
            Err(ExecuteError::Command(SessionError::SessionAlreadyCreated))
        ));
        let loaded = s.load("t", "s1").await.unwrap().unwrap();
        assert_eq!(loaded.state().agent_id.as_deref(), Some("agent"));
        assert!(s.load("t", "nobody").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn each_consumer_gets_each_event_once() {
        let disk = testing::disk();
        let seen = Arc::new(Seen::default());
        let s = sessions(disk.stores(), disk.alarms(), vec![seen.clone()]);
        s.execute(create()).await.unwrap();
        s.execute(interrupt(1)).await.unwrap();
        s.flushed().await;

        let head = s.load("t", "s1").await.unwrap().unwrap().seq();
        assert_eq!(*seen.seqs.lock().unwrap(), (1..=head).collect::<Vec<_>>());
    }

    #[tokio::test]
    async fn a_session_behind_after_a_crash_is_caught_up_at_boot() {
        let disk = testing::disk();
        let stores = disk.stores();
        let alarms = disk.alarms();
        sessions(stores.clone(), alarms.clone(), vec![])
            .execute(create())
            .await
            .unwrap();

        let seen = Arc::new(Seen::default());
        let booted = sessions(stores, alarms, vec![seen.clone()]);
        let behind = booted.behind().await.unwrap();
        assert_eq!(behind, vec![("t".to_string(), "s1".to_string())]);
        booted.resume(behind).await;
        booted.flushed().await;

        let head = booted.load("t", "s1").await.unwrap().unwrap().seq();
        assert_eq!(*seen.seqs.lock().unwrap(), (1..=head).collect::<Vec<_>>());
        assert!(booted.behind().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_failed_delivery_sets_an_alarm_and_waking_delivers_it() {
        let disk = testing::disk();
        let seen = Arc::new(Seen::default());
        seen.failing.store(true, Ordering::SeqCst);
        let alarms = disk.alarms();
        let s = sessions(disk.stores(), alarms.clone(), vec![seen.clone()]);
        s.execute(create()).await.unwrap();
        s.flushed().await;
        assert!(seen.seqs.lock().unwrap().is_empty());
        let later = Utc::now() + chrono::Duration::seconds(10);
        assert_eq!(alarms.due(later, 10).await.unwrap().len(), 1);

        seen.failing.store(false, Ordering::SeqCst);
        s.wake("t", "s1", later).await;
        s.flushed().await;
        let events = s
            .query_events(&EventFilter {
                session_id: Some("s1".into()),
                tenant_id: Some("t".into()),
                after_seq: None,
                limit: None,
            })
            .await
            .unwrap();
        let (logged, delivered): (Vec<_>, Vec<_>) =
            events.iter().partition(|e| e.payload.is_delivery());
        assert_eq!(
            *seen.seqs.lock().unwrap(),
            delivered.iter().map(|e| e.seq).collect::<Vec<_>>()
        );
        assert_eq!(
            logged.iter().map(|e| e.payload_type()).collect::<Vec<_>>(),
            vec!["delivery.failed", "delivery.recovered"]
        );
        assert!(alarms.due(later, 10).await.unwrap().is_empty());
    }

    #[derive(Default)]
    struct Outside {
        name: &'static str,
        failing: AtomicBool,
        hangs: bool,
        parks: bool,
        seqs: Mutex<Vec<u64>>,
    }

    impl Outside {
        fn named(name: &'static str) -> Arc<Self> {
            Arc::new(Self {
                name,
                ..Self::default()
            })
        }

        fn seqs(&self) -> Vec<u64> {
            self.seqs.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl Reader for Outside {
        fn subscription(&self) -> Subscription {
            let at_limit = match self.parks {
                true => AtLimit::Park,
                false => AtLimit::Skip,
            };
            Subscription::new(self.name, Start::Beginning, at_limit)
        }

        async fn deliver(
            &self,
            _sessions: &Sessions,
            _session: &Session,
            events: &[SessionEvent],
        ) -> Result<(), DeliveryError> {
            if self.hangs {
                tokio::time::sleep(std::time::Duration::from_secs(30)).await;
            }
            if self.failing.load(Ordering::SeqCst) {
                return Err(match self.parks {
                    true => DeliveryError::permanent(None, "gone"),
                    false => DeliveryError::retry(None, "down"),
                });
            }
            self.seqs
                .lock()
                .unwrap()
                .extend(events.iter().map(|e| e.seq));
            Ok(())
        }
    }

    fn with_outside(
        disk: &testing::Disk,
        readers: Vec<Arc<dyn Reader>>,
        outside: Vec<Arc<dyn Reader>>,
    ) -> Arc<Sessions> {
        disk.with(readers, outside, None)
    }

    async fn logged(s: &Sessions, session_id: &str) -> Vec<u64> {
        s.query_events(&EventFilter {
            tenant_id: Some("t".into()),
            session_id: Some(session_id.into()),
            ..Default::default()
        })
        .await
        .unwrap()
        .iter()
        .filter(|e| !e.payload.is_delivery())
        .map(|e| e.seq)
        .collect()
    }

    #[tokio::test]
    async fn an_outside_reader_that_hangs_does_not_hold_up_other_sessions() {
        let disk = testing::disk();
        let seen = Arc::new(Seen::default());
        let stuck = Arc::new(Outside {
            name: "stuck",
            hangs: true,
            ..Outside::default()
        });
        let s = with_outside(&disk, vec![seen.clone()], vec![stuck]);
        for n in 0..32 {
            let mut create = create();
            create.session_id = format!("s{n}");
            s.execute(create).await.unwrap();
        }

        let started = tokio::time::Instant::now();
        loop {
            let head = s.load("t", "s31").await.unwrap().unwrap().seq() as usize;
            if seen.seqs.lock().unwrap().len() >= 32 * head {
                break;
            }
            assert!(
                started.elapsed() < std::time::Duration::from_secs(2),
                "the session list waited on a stuck outside reader"
            );
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    }

    #[tokio::test]
    async fn one_failing_outside_reader_does_not_hold_up_another() {
        let disk = testing::disk();
        let (down, up) = (Outside::named("down"), Outside::named("up"));
        down.failing.store(true, Ordering::SeqCst);
        let s = with_outside(&disk, Vec::new(), vec![down.clone(), up.clone()]);
        s.execute(create()).await.unwrap();
        s.flushed().await;

        assert!(down.seqs().is_empty());
        assert_eq!(up.seqs(), logged(&s, "s1").await, "up got every event");
        let delivery = s
            .load("t", "s1")
            .await
            .unwrap()
            .unwrap()
            .delivery("down")
            .cloned()
            .unwrap_or_default();
        assert!(delivery.failing(), "the failure is in the log");
    }

    #[tokio::test]
    async fn a_parked_reader_gets_its_events_once_it_is_resumed() {
        let disk = testing::disk();
        let parked = Arc::new(Outside {
            name: "parked",
            parks: true,
            ..Outside::default()
        });
        parked.failing.store(true, Ordering::SeqCst);
        let s = with_outside(&disk, Vec::new(), vec![parked.clone()]);
        s.execute(create()).await.unwrap();
        s.flushed().await;
        assert!(parked.seqs().is_empty());
        assert!(!s.resume_delivery("t", "s1", "other").await.unwrap());

        parked.failing.store(false, Ordering::SeqCst);
        assert!(s.resume_delivery("t", "s1", "parked").await.unwrap());
        s.flushed().await;
        assert_eq!(parked.seqs(), logged(&s, "s1").await);
        assert!(!s.resume_delivery("t", "s1", "parked").await.unwrap());
    }

    #[tokio::test]
    async fn a_session_behind_on_an_outside_reader_is_caught_up_at_boot() {
        let disk = testing::disk();
        let down = Outside::named("down");
        down.failing.store(true, Ordering::SeqCst);
        let s = with_outside(&disk, Vec::new(), vec![down.clone()]);
        s.execute(create()).await.unwrap();
        s.flushed().await;

        assert_eq!(
            s.behind().await.unwrap(),
            vec![("t".to_string(), "s1".to_string())]
        );
    }

    #[tokio::test]
    async fn the_whole_tree_comes_from_the_log_when_only_the_active_branch_is_loaded() {
        let disk = testing::disk();
        use crate::protocol::{
            ClientMessage, ClientPayload, Content, DraftMessage, QueueMode, Role,
        };
        use substructure_session::command::TurnTarget;
        use substructure_session::Work;

        let s = disk.sessions();
        s.execute(create()).await.unwrap();
        let draft = |id: &str, role: Role, text: &str| DraftMessage {
            id: Some(id.to_string()),
            role,
            content: Some(Content::Text(text.to_string())),
            tool_calls: None,
            tool_call_id: None,
            name: None,
            reasoning: None,
        };
        let decide = |transcript: Vec<DraftMessage>| {
            let s = s.clone();
            async move {
                let session = s.load("t", "s1").await.unwrap().unwrap();
                let decision_id = session
                    .open_work()
                    .into_iter()
                    .find_map(|w| match w {
                        Work::AskWorker { work, .. } => Some(work.id),
                        _ => None,
                    })
                    .unwrap();
                s.execute(input(CommandPayload::SubmitWorkerDecision {
                    decision_id,
                    transcript,
                    actions: vec![substructure_session::decision::Action::Done {
                        data: serde_json::Value::Null,
                    }],
                    state: None,
                    agent: None,
                }))
                .await
                .unwrap();
            }
        };
        let say = |turn: &str, text: &str| {
            input(CommandPayload::SubmitClientPayload {
                payload: ClientPayload::Message(ClientMessage {
                    message: DraftMessage {
                        id: None,
                        ..draft("", Role::User, text)
                    },
                    stream: false,
                }),
                turn: TurnTarget::Open(turn.to_string()),
                queue: QueueMode::Refuse,
            })
        };
        decide(vec![]).await;
        s.execute(say("t1", "hi")).await.unwrap();
        decide(vec![
            draft("u1", Role::User, "hi"),
            draft("a1", Role::Assistant, "hello"),
        ])
        .await;
        s.execute(say("t2", "edit")).await.unwrap();
        decide(vec![
            draft("u1", Role::User, "hi"),
            draft("b1", Role::User, "hi, edited"),
        ])
        .await;

        let session = s.load("t", "s1").await.unwrap().unwrap();
        assert!(!session.state().has_node("a1"));
        let tree = s
            .whole_tree("t", "s1", session.state().head_id.clone())
            .await
            .unwrap();
        let mut ids: Vec<&str> = tree.nodes.iter().map(|n| n.message.id.as_str()).collect();
        ids.sort();
        assert_eq!(ids, vec!["a1", "b1", "u1"]);
        assert_eq!(tree.head_id.as_deref(), Some("b1"));
    }

    #[tokio::test]
    async fn a_command_on_a_session_never_created_writes_nothing() {
        let disk = testing::disk();
        let s = disk.sessions();
        let refused = s.execute(input(CommandPayload::CancelSession)).await;

        assert!(matches!(
            refused,
            Err(ExecuteError::Command(SessionError::SessionNotCreated))
        ));
        assert!(s.load("t", "s1").await.unwrap().is_none());
    }

    #[derive(Default)]
    struct Flaky {
        down: AtomicBool,
        hangs: AtomicBool,
        sent: Mutex<Vec<WorkId>>,
    }

    #[async_trait]
    impl WorkSink for Flaky {
        fn reader(&self) -> WorkReader {
            WorkReader::new("work", substructure_session::delivery::Restart::FromCursor)
        }

        async fn send(
            &self,
            _sessions: &Sessions,
            _session: &Session,
            work: Vec<Work>,
        ) -> Vec<Unsent> {
            let ids: Vec<WorkId> = work.iter().map(|w| w.id().clone()).collect();
            if self.hangs.load(Ordering::SeqCst) {
                std::future::pending::<()>().await;
            }
            if self.down.load(Ordering::SeqCst) {
                return ids
                    .into_iter()
                    .map(|work| Unsent {
                        work,
                        error: "queue down".into(),
                        permanent: false,
                    })
                    .collect();
            }
            self.sent.lock().unwrap().extend(ids);
            Vec::new()
        }
    }

    #[tokio::test]
    async fn work_that_could_not_be_sent_goes_out_again_as_the_same_attempt() {
        let disk = testing::disk();
        let alarms = disk.alarms();
        let sink = Arc::new(Flaky::default());
        sink.down.store(true, Ordering::SeqCst);
        let s = Sessions::new(
            disk.stores(),
            alarms.clone(),
            Vec::new(),
            Vec::new(),
            Some(sink.clone()),
            SessionsConfig::default(),
        );
        s.execute(create()).await.unwrap();
        s.flushed().await;
        assert!(sink.sent.lock().unwrap().is_empty());
        let due = alarms.next().await.unwrap().unwrap();

        sink.down.store(false, Ordering::SeqCst);
        s.wake("t", "s1", due).await;
        s.flushed().await;
        let sent = sink.sent.lock().unwrap().clone();
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0].attempt, Some(0));
    }

    #[tokio::test(start_paused = true)]
    async fn work_that_hangs_while_sending_goes_out_again_as_the_same_attempt() {
        let disk = testing::disk();
        let alarms = disk.alarms();
        let sink = Arc::new(Flaky::default());
        sink.hangs.store(true, Ordering::SeqCst);
        let s = Sessions::new(
            disk.stores(),
            alarms.clone(),
            Vec::new(),
            Vec::new(),
            Some(sink.clone()),
            SessionsConfig::default(),
        );
        s.execute(create()).await.unwrap();
        s.flushed().await;
        assert!(sink.sent.lock().unwrap().is_empty());
        let due = alarms.next().await.unwrap().unwrap();

        sink.hangs.store(false, Ordering::SeqCst);
        s.wake("t", "s1", due).await;
        s.flushed().await;
        let sent = sink.sent.lock().unwrap().clone();
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0].attempt, Some(0));
    }
}
