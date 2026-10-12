use crate::state::EffectKind;
use chrono::{DateTime, TimeDelta, Utc};
use serde::{Deserialize, Serialize};

use crate::command::CommandPayload;
use crate::entry::{Command, Entry};
use crate::events::EventPayload;
use crate::protocol::RetryPolicy;
use crate::retry::RetryTarget;
use crate::session::{Error, Session, SessionEvent};
use crate::store::Store;
use crate::work::{Work, WorkId};

const BATCH: usize = 100;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Start {
    Beginning,
    Joined,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AtLimit {
    Skip,
    Park,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Restart {
    FromCursor,
    ResendOpen,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Subscription {
    pub name: String,
    pub start: Start,
    pub retry: RetryPolicy,
    pub at_limit: AtLimit,
    pub batch: usize,
}

impl Subscription {
    pub fn new(name: impl Into<String>, start: Start, at_limit: AtLimit) -> Self {
        Self {
            name: name.into(),
            start,
            retry: RetryPolicy::default_for(RetryTarget::Delivery),
            at_limit,
            batch: BATCH,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WorkReader {
    pub name: String,
    pub restart: Restart,
    pub batch: usize,
}

impl WorkReader {
    pub fn new(name: impl Into<String>, restart: Restart) -> Self {
        Self {
            name: name.into(),
            restart,
            batch: BATCH,
        }
    }
}

pub fn unreachable_retry() -> TimeDelta {
    RetryPolicy::default_for(RetryTarget::Delivery).backoff(1)
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Delivery {
    pub floor: u64,
    pub attempts: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failing_since: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retry_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parked: Option<Parked>,
    #[serde(default)]
    pub skipped: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Parked {
    pub seq: u64,
    pub error: String,
}

impl Delivery {
    pub fn failing(&self) -> bool {
        self.attempts > 0
    }

    pub fn down(&self) -> bool {
        self.failing() || self.failing_since.is_some()
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Undelivered {
    pub subscription: String,
    pub seq: u64,
    pub seen_attempts: u32,
    pub error: String,
    pub permanent: bool,
    pub retry_after_secs: Option<u32>,
    pub retry: RetryPolicy,
    pub at_limit: AtLimit,
}

#[derive(Debug, Clone)]
pub struct Batch<T> {
    pub items: Vec<T>,
    pub through: u64,
    pub attempts: u32,
}

#[derive(Debug)]
pub struct Next<T> {
    pub batch: Option<Batch<T>>,
    pub written: Vec<Entry>,
}

#[derive(Debug, Clone)]
pub struct Failure {
    pub item: usize,
    pub error: String,
    pub permanent: bool,
    pub retry_after: Option<TimeDelta>,
}

#[derive(Debug, Clone)]
pub enum Delivered {
    All,
    Failed(Failure),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Unsent {
    pub work: WorkId,
    pub error: String,
    pub permanent: bool,
}

impl EventPayload {
    pub fn is_delivery(&self) -> bool {
        matches!(
            self,
            EventPayload::DeliveryFailed(_)
                | EventPayload::DeliveryRecovered(_)
                | EventPayload::DeliverySkipped(_)
                | EventPayload::DeliveryParked(_)
                | EventPayload::DeliveryResumed(_)
        )
    }

    pub fn is_bookkeeping(&self) -> bool {
        self.is_delivery()
            || matches!(
                self,
                EventPayload::BranchLoaded(_) | EventPayload::BranchUnloaded(_)
            )
    }
}

impl<T> Next<T> {
    fn idle(written: Vec<Entry>) -> Self {
        Self {
            batch: None,
            written,
        }
    }
}

impl Session {
    pub fn delivery(&self, subscription: &str) -> Option<&Delivery> {
        self.state.deliveries.get(subscription)
    }

    pub fn join(&self, store: &dyn Store, reader: &str) -> Result<(), Error> {
        if !store.cursors()?.contains_key(reader) {
            store.set_cursor(reader, self.seq)?;
        }
        Ok(())
    }

    pub fn next_events(
        &mut self,
        store: &dyn Store,
        subscription: &Subscription,
        now: DateTime<Utc>,
    ) -> Result<Next<SessionEvent>, Error> {
        let name = &subscription.name;
        let mut written = Vec::new();
        loop {
            let delivery = self.delivery(name).cloned().unwrap_or_default();
            if delivery.parked.is_some() || delivery.retry_at.is_some_and(|at| at > now) {
                return Ok(Next::idle(written));
            }
            let base = match (store.cursors()?.get(name).copied(), subscription.start) {
                (Some(cursor), _) => cursor,
                (None, Start::Beginning) => 0,
                (None, Start::Joined) => return Ok(Next::idle(written)),
            };
            let after = base.max(delivery.floor);
            let head = self.seq;
            if after >= head {
                if !delivery.failing() {
                    return Ok(Next::idle(written));
                }
                written.extend(self.recovered(store, name, head, now)?);
                store.set_cursor(name, head)?;
                continue;
            }
            let read: Vec<SessionEvent> = Self::events(store, after, subscription.batch.max(1))?
                .into_iter()
                .filter(|e| e.seq <= head)
                .collect();
            let Some(through) = read.last().map(|e| e.seq) else {
                return Ok(Next::idle(written));
            };
            let items: Vec<SessionEvent> = read
                .into_iter()
                .filter(|e| !e.payload.is_bookkeeping())
                .collect();
            if items.is_empty() {
                store.set_cursor(name, through)?;
                continue;
            }
            return Ok(Next {
                batch: Some(Batch {
                    items,
                    through,
                    attempts: delivery.attempts,
                }),
                written,
            });
        }
    }

    pub fn ack_events(
        &mut self,
        store: &dyn Store,
        subscription: &Subscription,
        batch: &Batch<SessionEvent>,
        delivered: Delivered,
        now: DateTime<Utc>,
    ) -> Result<Vec<Entry>, Error> {
        let name = &subscription.name;
        match delivered {
            Delivered::All => {
                let written = match self.delivery(name).is_some_and(Delivery::down) {
                    true => self.recovered(store, name, batch.through, now)?,
                    false => Vec::new(),
                };
                store.set_cursor(name, batch.through)?;
                Ok(written)
            }
            Delivered::Failed(failure) => {
                let Some(seq) = batch
                    .items
                    .get(failure.item)
                    .or(batch.items.first())
                    .map(|e| e.seq)
                else {
                    return Ok(Vec::new());
                };
                let id = format!("delivery:{name}:{seq}:{}", batch.attempts);
                let payload = CommandPayload::DeliveryFailed(Undelivered {
                    subscription: name.clone(),
                    seq,
                    seen_attempts: batch.attempts,
                    error: failure.error,
                    permanent: failure.permanent,
                    retry_after_secs: failure
                        .retry_after
                        .map(|d| u32::try_from(d.num_seconds().max(0)).unwrap_or(u32::MAX)),
                    retry: subscription.retry.clone(),
                    at_limit: subscription.at_limit,
                });
                self.record_command(store, now, id, payload)
            }
        }
    }

    pub fn resume_delivery(
        &mut self,
        store: &dyn Store,
        subscription: &str,
        now: DateTime<Utc>,
    ) -> Result<Vec<Entry>, Error> {
        let Some(parked) = self.delivery(subscription).and_then(|d| d.parked.clone()) else {
            return Ok(Vec::new());
        };
        self.record_command(
            store,
            now,
            format!("delivery:{subscription}:resumed:{}", parked.seq),
            CommandPayload::ResumeDelivery {
                subscription: subscription.to_string(),
            },
        )
    }

    fn resend(&self, store: &dyn Store) -> Result<Vec<Work>, Error> {
        let seqs: Vec<u64> = self
            .state()
            .effects_of(EffectKind::Decision)
            .filter_map(|e| e.decision()?.dispatched_seq)
            .collect();
        let rebuilt = match seqs.is_empty() {
            true => Vec::new(),
            false => self.work(&store.events_at(&seqs)?),
        };
        Ok(self
            .open_work()
            .into_iter()
            .map(|work| {
                rebuilt
                    .iter()
                    .find(|again| again.id() == work.id())
                    .cloned()
                    .unwrap_or(work)
            })
            .collect())
    }

    pub fn next_work(
        &mut self,
        store: &dyn Store,
        reader: &WorkReader,
        restarted: bool,
    ) -> Result<Next<Work>, Error> {
        let head = self.seq;
        if restarted && reader.restart == Restart::ResendOpen {
            let items = self.resend(store)?;
            if !items.is_empty() {
                return Ok(Next {
                    batch: Some(Batch {
                        items,
                        through: head,
                        attempts: 0,
                    }),
                    written: Vec::new(),
                });
            }
            store.set_cursor(&reader.name, head)?;
        }
        loop {
            let after = store.cursors()?.get(&reader.name).copied().unwrap_or(0);
            if after >= head {
                return Ok(Next::idle(Vec::new()));
            }
            let read: Vec<SessionEvent> = Self::events(store, after, reader.batch.max(1))?
                .into_iter()
                .filter(|e| e.seq <= head)
                .collect();
            let Some(through) = read.last().map(|e| e.seq) else {
                return Ok(Next::idle(Vec::new()));
            };
            let items = self.work(&read);
            if items.is_empty() {
                store.set_cursor(&reader.name, through)?;
                continue;
            }
            return Ok(Next {
                batch: Some(Batch {
                    items,
                    through,
                    attempts: 0,
                }),
                written: Vec::new(),
            });
        }
    }

    pub fn ack_work(
        &mut self,
        store: &dyn Store,
        reader: &WorkReader,
        batch: &Batch<Work>,
        unsent: Vec<Unsent>,
        now: DateTime<Utc>,
    ) -> Result<Vec<Entry>, Error> {
        let written = match unsent.is_empty() {
            true => Vec::new(),
            false => {
                let id = format!("unsent:{}:{}", reader.name, batch.through);
                let payload = CommandPayload::WorkUnsent { unsent };
                self.record_command(store, now, id, payload)?
            }
        };
        store.set_cursor(&reader.name, batch.through)?;
        Ok(written)
    }

    fn recovered(
        &mut self,
        store: &dyn Store,
        name: &str,
        through: u64,
        now: DateTime<Utc>,
    ) -> Result<Vec<Entry>, Error> {
        self.record_command(
            store,
            now,
            format!("delivery:{name}:recovered:{through}"),
            CommandPayload::DeliveryRecovered {
                subscription: name.to_string(),
                through,
            },
        )
    }

    fn record_command(
        &mut self,
        store: &dyn Store,
        now: DateTime<Utc>,
        id: String,
        payload: CommandPayload,
    ) -> Result<Vec<Entry>, Error> {
        let command = Command::system(id, &self.tenant_id.clone(), payload);
        Ok(self.run(store, now, command)?.written.into_iter().collect())
    }
}
