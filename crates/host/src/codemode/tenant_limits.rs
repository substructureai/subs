use async_trait::async_trait;

use super::Limits;

/// What a tenant's scripts may use, as its plan gives it.
#[async_trait]
pub trait LimitsResolver: Send + Sync {
    async fn resolve(&self, tenant_id: &str) -> Limits;
}

/// The same limits for every tenant.
pub struct FixedLimits(pub Limits);

#[async_trait]
impl LimitsResolver for FixedLimits {
    async fn resolve(&self, _tenant_id: &str) -> Limits {
        self.0.clone()
    }
}
