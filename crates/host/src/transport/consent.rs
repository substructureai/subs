use std::sync::Arc;

use crate::connectors::registry::ConnectionPath;
use crate::connectors::Requester;
use crate::runtime::session::interrupts::auth::Authorize;

pub enum WayIn {
    Link { url: String, label: String },
    Command(String),
}

#[async_trait::async_trait]
pub trait Consent: Send + Sync {
    async fn way_in(&self, tenant_id: &str, authorize: &Authorize) -> Option<WayIn>;
}

pub struct DashboardConsent(pub String);

#[async_trait::async_trait]
impl Consent for DashboardConsent {
    async fn way_in(&self, _tenant_id: &str, _authorize: &Authorize) -> Option<WayIn> {
        Some(WayIn::Link {
            url: self.0.clone(),
            label: "Authorize it in the dashboard".to_string(),
        })
    }
}

pub struct EngineConsent(pub Arc<crate::transport::mcp_auth::AuthorizeLinks>);

#[async_trait::async_trait]
impl Consent for EngineConsent {
    async fn way_in(&self, tenant_id: &str, authorize: &Authorize) -> Option<WayIn> {
        let minted = self
            .0
            .mint_for(
                tenant_id,
                &authorize.connection.to_string(),
                &authorize.requester,
                chrono::Utc::now(),
            )
            .await;
        match minted {
            Ok(None) => Some(cli(&authorize.connection)),
            Ok(Some(url)) => Some(WayIn::Link {
                url,
                label: format!("Authorize {}", authorize.connection),
            }),
            Err(e) => {
                tracing::warn!(error = %e, connection = %authorize.connection, "minting an authorize link failed");
                Some(cli(&authorize.connection))
            }
        }
    }
}

#[derive(Default)]
pub struct CliConsent;

#[async_trait::async_trait]
impl Consent for CliConsent {
    async fn way_in(&self, _tenant_id: &str, authorize: &Authorize) -> Option<WayIn> {
        Some(cli(&authorize.connection))
    }
}

fn cli(connection: &ConnectionPath) -> WayIn {
    WayIn::Command(format!("Run `subs auth {connection}` to authorize it."))
}

pub fn refuses(requester: &Requester) -> bool {
    requester.subject.is_none()
}
