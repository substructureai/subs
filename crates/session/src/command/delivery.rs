use crate::command::{Outcome, SessionError, SettleError, Working};
use crate::delivery::{AtLimit, Undelivered, Unsent};
use crate::events::{
    DeliveryFailed, DeliveryParked, DeliveryRecovered, DeliveryResumed, DeliverySkipped,
    EventPayload, WorkUnsent,
};
use crate::protocol::{EffectStatus, ErrorInfo, RetryPolicy};
use crate::retry::RetryTarget;

impl Working {
    pub(super) fn delivery_failed(&mut self, undelivered: Undelivered) -> Result<(), SessionError> {
        let Undelivered {
            subscription,
            seq,
            seen_attempts,
            error,
            permanent,
            retry_after_secs,
            retry,
            at_limit,
        } = undelivered;
        let delivery = self
            .state
            .deliveries
            .get(&subscription)
            .cloned()
            .unwrap_or_default();
        if delivery.parked.is_some() || seq <= delivery.floor || delivery.attempts != seen_attempts
        {
            return Ok(());
        }
        let attempt = seen_attempts.saturating_add(1);
        let since = delivery.failing_since.unwrap_or(self.now);
        let expired = retry
            .total_deadline(since)
            .is_some_and(|deadline| deadline <= self.now);
        if !(permanent || expired || attempt >= retry.max_attempts) {
            let wait = retry_after_secs
                .map(|s| chrono::TimeDelta::seconds(i64::from(s)))
                .unwrap_or_else(|| retry.backoff(attempt));
            let retry_at = self.now.checked_add_signed(wait).unwrap_or(self.now);
            self.emit(EventPayload::DeliveryFailed(DeliveryFailed {
                subscription,
                seq,
                attempt,
                error,
                retry_at,
            }));
            return Ok(());
        }
        match at_limit {
            AtLimit::Park => self.emit(EventPayload::DeliveryParked(DeliveryParked {
                subscription,
                seq,
                attempt,
                error,
            })),
            AtLimit::Skip => self.emit(EventPayload::DeliverySkipped(DeliverySkipped {
                subscription,
                seq,
                attempt,
                error,
            })),
        }
        Ok(())
    }

    pub(super) fn delivery_recovered(&mut self, subscription: String, through: u64) {
        let Some(delivery) = self.state.deliveries.get(&subscription) else {
            return;
        };
        if !delivery.down() {
            return;
        }
        let attempts = delivery.attempts;
        self.emit(EventPayload::DeliveryRecovered(DeliveryRecovered {
            subscription,
            through,
            attempts,
        }));
    }

    pub(super) fn resume_delivery(&mut self, subscription: String) {
        if self
            .state
            .deliveries
            .get(&subscription)
            .is_some_and(|d| d.parked.is_some())
        {
            self.emit(EventPayload::DeliveryResumed(DeliveryResumed {
                subscription,
            }));
        }
    }

    pub(super) fn work_unsent(&mut self, unsent: Vec<Unsent>) {
        let backoff = RetryPolicy::default_for(RetryTarget::Delivery);
        for Unsent {
            work: id,
            error,
            permanent,
        } in unsent
        {
            let Some(tracking) = self.state.tracking(id.kind, &id.id) else {
                continue;
            };
            let current = id.attempt.is_none_or(|a| a == tracking.retry.attempts);
            if tracking.status() != EffectStatus::Pending || !current {
                continue;
            }
            let spec = id.kind.spec();
            if permanent {
                let failed = SettleError::new(
                    ErrorInfo::internal(format!("could not send the work: {error}")),
                    false,
                );
                self.then(|s| spec.settle(s, &id.id, Outcome::Error(failed)));
                continue;
            }
            if tracking.total_expired(self.now) {
                self.then(|s| spec.settle(s, &id.id, Outcome::Error(spec.timeout_error(true))));
                continue;
            }
            let unsent = tracking.retry.unsent.saturating_add(1);
            let retry_at = self
                .now
                .checked_add_signed(backoff.backoff(unsent))
                .unwrap_or(self.now);
            self.emit(EventPayload::WorkUnsent(WorkUnsent {
                kind: id.kind,
                id: id.id,
                attempt: id.attempt,
                error,
                retry_at,
            }));
        }
    }
}
