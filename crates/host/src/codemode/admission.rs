use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use tokio::sync::{Notify, OwnedSemaphorePermit, Semaphore};

/// How many scripts run at once, and how many of them one tenant may hold.
pub struct Admission {
    slots: usize,
    permits: Arc<Semaphore>,
    tenants: Mutex<HashMap<String, usize>>,
    turn: Notify,
}

/// A slot, held until dropped.
pub struct Ticket {
    admission: Arc<Admission>,
    tenant: String,
    _permit: OwnedSemaphorePermit,
}

impl Admission {
    pub fn new(slots: usize) -> Self {
        let slots = slots.max(1);
        Admission {
            slots,
            permits: Arc::new(Semaphore::new(slots)),
            tenants: Default::default(),
            turn: Notify::new(),
        }
    }

    /// A tenant takes all slots but one, so another tenant always gets a turn.
    fn per_tenant(&self) -> usize {
        self.slots.saturating_sub(1).max(1)
    }

    pub async fn admit(self: &Arc<Self>, tenant: &str) -> Ticket {
        loop {
            let notified = self.turn.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            let admitted = {
                let mut tenants = self.tenants.lock().expect("tenants");
                let running = tenants.entry(tenant.to_string()).or_default();
                let admitted = *running < self.per_tenant();
                if admitted {
                    *running += 1;
                }
                admitted
            };
            if admitted {
                let permit = self.permits.clone().acquire_owned().await.expect("open");
                return Ticket {
                    admission: self.clone(),
                    tenant: tenant.to_string(),
                    _permit: permit,
                };
            }
            notified.await;
        }
    }
}

impl Drop for Ticket {
    fn drop(&mut self) {
        let mut tenants = self.admission.tenants.lock().expect("tenants");
        if let Some(running) = tenants.get_mut(&self.tenant) {
            *running -= 1;
            if *running == 0 {
                tenants.remove(&self.tenant);
            }
        }
        drop(tenants);
        self.admission.turn.notify_waiters();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[tokio::test]
    async fn one_tenant_cannot_take_every_slot() {
        let admission = Arc::new(Admission::new(3));
        let a = admission.admit("greedy").await;
        let b = admission.admit("greedy").await;
        let third = tokio::time::timeout(Duration::from_millis(50), admission.admit("greedy"));
        assert!(
            third.await.is_err(),
            "the greedy tenant waits for its own slot"
        );
        let polite = tokio::time::timeout(Duration::from_millis(50), admission.admit("polite"));
        assert!(polite.await.is_ok(), "another tenant gets the slot left");
        drop((a, b));
    }

    #[tokio::test]
    async fn a_slot_is_freed_when_its_ticket_drops() {
        let admission = Arc::new(Admission::new(1));
        let held = admission.admit("t").await;
        let next = {
            let admission = admission.clone();
            tokio::spawn(async move { admission.admit("t").await })
        };
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert!(!next.is_finished());
        drop(held);
        tokio::time::timeout(Duration::from_secs(1), next)
            .await
            .expect("the waiting script is admitted")
            .unwrap();
    }

    #[tokio::test]
    async fn one_slot_still_runs_every_tenant() {
        let admission = Arc::new(Admission::new(1));
        drop(admission.admit("a").await);
        drop(admission.admit("b").await);
    }
}
