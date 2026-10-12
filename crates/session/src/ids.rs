use std::sync::atomic::{AtomicU64, Ordering};

use chrono::{DateTime, Utc};
use uuid::{Builder, Uuid};
use xxhash_rust::xxh3::xxh3_128_with_seed;

use crate::span::{SpanContext, SpanId, TraceId};

#[derive(Debug, Default)]
pub struct Ids {
    seed: u64,
    millis: u64,
    next: AtomicU64,
}

impl Clone for Ids {
    fn clone(&self) -> Self {
        Self {
            seed: self.seed,
            millis: self.millis,
            next: AtomicU64::new(self.next.load(Ordering::Relaxed)),
        }
    }
}

impl Ids {
    pub fn new(session_id: &str, seq: u64, now: DateTime<Utc>) -> Self {
        Self {
            seed: xxhash_rust::xxh3::xxh3_64_with_seed(session_id.as_bytes(), seq),
            millis: now.timestamp_millis().max(0) as u64,
            next: AtomicU64::new(0),
        }
    }

    fn bytes(&self) -> [u8; 16] {
        let n = self.next.fetch_add(1, Ordering::Relaxed);
        xxh3_128_with_seed(&n.to_le_bytes(), self.seed).to_le_bytes()
    }

    pub fn uuid(&self) -> Uuid {
        let bytes = self.bytes();
        let mut random = [0u8; 10];
        random.copy_from_slice(&bytes[..10]);
        Builder::from_unix_timestamp_millis(self.millis, &random).into_uuid()
    }

    pub fn next(&self) -> String {
        self.uuid().to_string()
    }

    pub fn span(&self, name: &str) -> SpanContext {
        let trace = self.bytes();
        let mut span = [0u8; 8];
        span.copy_from_slice(&self.bytes()[..8]);
        SpanContext::from_ids(TraceId::from_bytes(trace), SpanId::from_bytes(span), name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_same_inputs_give_the_same_ids() {
        let now = Utc::now();
        let a = Ids::new("s1", 4, now);
        let b = Ids::new("s1", 4, now);
        assert_eq!(a.next(), b.next());
        assert_eq!(a.next(), b.next());
        assert_ne!(Ids::new("s1", 4, now).next(), Ids::new("s1", 5, now).next());
        assert_ne!(Ids::new("s1", 4, now).next(), Ids::new("s2", 4, now).next());
    }

    #[test]
    fn ids_follow_the_clock() {
        let now = Utc::now();
        let earlier = Ids::new("s1", 9, now - chrono::Duration::seconds(5)).next();
        let later = Ids::new("s1", 1, now).next();
        assert!(earlier < later);
    }
}
