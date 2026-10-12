use std::sync::Arc;

use async_trait::async_trait;

use axum::Router;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::event_store::Seq;
use crate::protocol::{Issuer, SessionOwner, TokenDelta};
use crate::session::subscriptions::SessionSubscriptionSpec;
use crate::session::{Session, SessionEvent};
use crate::{Caller, ClientInputOutput, HandleClientInput, Runtime, RuntimeError};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ChannelKind(&'static str);

impl ChannelKind {
    pub const SLACK: Self = Self("slack");
    pub const AG_UI: Self = Self("ag-ui");
    pub const CLI: Self = Self("cli");

    pub const fn as_str(&self) -> &'static str {
        self.0
    }

    pub fn issuer(&self) -> Issuer {
        Issuer::new(self.0)
    }

    pub fn owning(owner: &SessionOwner) -> Option<Self> {
        let issuer = owner.requester.subject.as_ref()?.issuer.as_str();
        [Self::SLACK, Self::AG_UI, Self::CLI]
            .into_iter()
            .find(|kind| kind.as_str() == issuer)
    }
}

impl std::fmt::Display for ChannelKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.0)
    }
}

#[derive(Clone)]
pub struct ChannelContext {
    runtime: Arc<Runtime>,
    pub shutdown: CancellationToken,
}

impl ChannelContext {
    pub fn new(runtime: Arc<Runtime>, shutdown: CancellationToken) -> Self {
        Self { runtime, shutdown }
    }

    pub async fn handle_client_input(
        &self,
        input: HandleClientInput,
    ) -> Result<ClientInputOutput, RuntimeError> {
        self.runtime.handle_client_input(input).await
    }

    pub async fn submit_client_payload(
        &self,
        input: crate::SubmitClientPayload,
    ) -> Result<crate::SubmitClientPayloadOutput, RuntimeError> {
        self.runtime.submit_client_payload(input).await
    }

    pub async fn stream(
        &self,
        spec: SessionSubscriptionSpec,
        after: Option<Seq>,
    ) -> Result<mpsc::Receiver<SessionEvent>, RuntimeError> {
        self.runtime.stream(spec, after).await
    }

    pub async fn subscribe_token_deltas(
        &self,
        caller: &Caller,
        root_session_id: &str,
    ) -> mpsc::Receiver<TokenDelta> {
        self.runtime
            .subscribe_token_deltas(caller, root_session_id)
            .await
    }

    pub async fn read_session_events(
        &self,
        caller: &Caller,
        session_id: &str,
        after: Option<Seq>,
        limit: Option<usize>,
    ) -> Result<Vec<SessionEvent>, RuntimeError> {
        self.runtime
            .read_session_events(caller, session_id, after, limit)
            .await
    }

    pub async fn get_session(
        &self,
        tenant_id: &str,
        session_id: &str,
    ) -> Result<Session, RuntimeError> {
        self.runtime.get_session(tenant_id, session_id).await
    }
}

#[async_trait]
pub trait Channel: Send + Sync {
    fn kind(&self) -> ChannelKind;

    fn router(&self, ctx: ChannelContext) -> Option<Router> {
        let _ = ctx;
        None
    }

    async fn run(&self, ctx: ChannelContext) {
        let _ = ctx;
    }
}

pub fn start_channels(channels: Vec<Arc<dyn Channel>>, ctx: ChannelContext) -> Vec<Router> {
    let mut routers = Vec::new();
    for channel in channels {
        if let Some(router) = channel.router(ctx.clone()) {
            routers.push(Router::new().nest(&format!("/api/channels/{}", channel.kind()), router));
        }
        let ctx = ctx.clone();
        tokio::spawn(async move { channel.run(ctx).await });
    }
    routers
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{Requester, Subject, Visibility};

    fn owned_by(issuer: Issuer) -> SessionOwner {
        SessionOwner {
            tenant_id: "t1".into(),
            requester: Requester::new(Subject::new(issuer, "u1"), Visibility::Private),
            metadata: Default::default(),
        }
    }

    #[test]
    fn a_session_belongs_to_the_channel_that_opened_it() {
        assert_eq!(
            ChannelKind::owning(&owned_by(ChannelKind::SLACK.issuer())),
            Some(ChannelKind::SLACK)
        );
        assert_eq!(
            ChannelKind::owning(&owned_by(Issuer::cli())),
            Some(ChannelKind::CLI)
        );
    }

    #[test]
    fn a_browser_frontend_names_no_channel() {
        assert_eq!(ChannelKind::owning(&owned_by(Issuer::app())), None);
        assert_ne!(Issuer::app().as_str(), ChannelKind::AG_UI.as_str());
    }

    #[test]
    fn a_session_no_channel_opened_belongs_to_none() {
        assert_eq!(ChannelKind::owning(&owned_by(Issuer::operator())), None);
        let anonymous = SessionOwner {
            tenant_id: "t1".into(),
            requester: Requester::machine(),
            metadata: Default::default(),
        };
        assert_eq!(ChannelKind::owning(&anonymous), None);
    }
}
