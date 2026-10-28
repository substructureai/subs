use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use tokio::sync::{Notify, OwnedSemaphorePermit, Semaphore};

/// How many scripts run at once. A tenant may take every slot while no other
/// tenant waits; while another does, it keeps at most all but one.
pub struct Admission {
    slots: usize,
    permits: Arc<Semaphore>,
    tenants: Mutex<Tenants>,
    turn: Notify,
}

#[derive(Default)]
struct Tenants {
    running: HashMap<String, usize>,
    waiting: HashMap<String, usize>,
}

impl Tenants {
    fn others_wait(&self, tenant: &str) -> bool {
        self.waiting.keys().any(|t| t != tenant)
    }

    fn change(map: &mut HashMap<String, usize>, tenant: &str, up: bool) {
        let count = map.entry(tenant.to_string()).or_default();
        match up {
            true => *count += 1,
            false => *count = count.saturating_sub(1),
        }
        if *count == 0 {
            map.remove(tenant);
        }
    }
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

    pub async fn admit(self: &Arc<Self>, tenant: &str) -> Ticket {
        let mut waiting: Option<Waiting> = None;
        loop {
            let notified = self.turn.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            let admitted = {
                let mut tenants = self.tenants.lock().expect("tenants");
                let cap = match tenants.others_wait(tenant) {
                    true => self.slots.saturating_sub(1).max(1),
                    false => self.slots,
                };
                let running = tenants.running.get(tenant).copied().unwrap_or(0);
                let admitted = running < cap && self.permits.available_permits() > 0;
                if admitted {
                    Tenants::change(&mut tenants.running, tenant, true);
                } else if waiting.is_none() {
                    Tenants::change(&mut tenants.waiting, tenant, true);
                }
                admitted
            };
            if admitted {
                drop(waiting.take());
                let permit = self.permits.clone().acquire_owned().await.expect("open");
                return Ticket {
                    admission: self.clone(),
                    tenant: tenant.to_string(),
                    _permit: permit,
                };
            }
            waiting.get_or_insert_with(|| Waiting {
                admission: self.clone(),
                tenant: tenant.to_string(),
            });
            notified.await;
        }
    }
}

/// A tenant waiting for a slot, until dropped.
struct Waiting {
    admission: Arc<Admission>,
    tenant: String,
}

impl Drop for Waiting {
    fn drop(&mut self) {
        let mut tenants = self.admission.tenants.lock().expect("tenants");
        Tenants::change(&mut tenants.waiting, &self.tenant, false);
    }
}

impl Drop for Ticket {
    fn drop(&mut self) {
        let mut tenants = self.admission.tenants.lock().expect("tenants");
        Tenants::change(&mut tenants.running, &self.tenant, false);
        drop(tenants);
        self.admission.turn.notify_waiters();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    async fn soon<T>(f: impl std::future::Future<Output = T>) -> Option<T> {
        tokio::time::timeout(Duration::from_millis(50), f)
            .await
            .ok()
    }

    #[tokio::test]
    async fn a_tenant_alone_takes_every_slot() {
        let admission = Arc::new(Admission::new(3));
        let a = admission.admit("t").await;
        let b = admission.admit("t").await;
        let c = soon(admission.admit("t")).await;
        assert!(
            c.is_some(),
            "no one else waits, so the last slot is its too"
        );
        drop((a, b, c));
    }

    #[tokio::test]
    async fn a_waiting_tenant_gets_the_next_slot() {
        let admission = Arc::new(Admission::new(2));
        let a = admission.admit("greedy").await;
        let b = admission.admit("greedy").await;
        let polite = {
            let admission = admission.clone();
            tokio::spawn(async move { admission.admit("polite").await })
        };
        tokio::time::sleep(Duration::from_millis(20)).await;
        let greedy = {
            let admission = admission.clone();
            tokio::spawn(async move { admission.admit("greedy").await })
        };
        tokio::time::sleep(Duration::from_millis(20)).await;
        drop(a);
        let polite = tokio::time::timeout(Duration::from_secs(1), polite)
            .await
            .expect("the waiting tenant gets the freed slot")
            .unwrap();
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert!(!greedy.is_finished(), "the greedy tenant keeps all but one");
        drop((b, polite));
        tokio::time::timeout(Duration::from_secs(1), greedy)
            .await
            .expect("and gets a slot once one is free")
            .unwrap();
    }

    #[tokio::test]
    async fn a_tenant_that_stops_waiting_is_no_longer_counted() {
        let admission = Arc::new(Admission::new(2));
        let a = admission.admit("greedy").await;
        let b = admission.admit("greedy").await;
        assert!(soon(admission.admit("gone")).await.is_none());
        drop(a);
        let c = soon(admission.admit("greedy")).await;
        assert!(
            c.is_some(),
            "the tenant that gave up does not hold a slot back"
        );
        drop((b, c));
    }

    #[tokio::test]
    async fn one_slot_still_runs_every_tenant() {
        let admission = Arc::new(Admission::new(1));
        drop(admission.admit("a").await);
        drop(admission.admit("b").await);
    }
}
