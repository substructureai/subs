use std::sync::Arc;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use serde_json::Value;
use tokio_tungstenite::tungstenite::Message as WsMessage;

use super::bot::{SlackBot, Workspace, WorkspaceResolver};
use crate::manifest::SlackAudience;
use crate::runtime::blob::BlobStore;
use crate::transport::channel::{Channel, ChannelContext, ChannelKind};

const RECONNECT_DELAY: Duration = Duration::from_secs(3);

#[derive(Debug)]
pub struct MissingEnv(Vec<(String, &'static str)>);

impl std::fmt::Display for MissingEnv {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "missing required environment variable(s):")?;
        for (name, desc) in &self.0 {
            write!(f, "\n  - {name}: {desc}")?;
        }
        write!(f, "\n\nSet them and try again, e.g.:")?;
        for (name, _) in &self.0 {
            write!(f, "\n  export {name}=...")?;
        }
        Ok(())
    }
}

impl std::error::Error for MissingEnv {}

pub struct SlackTokens {
    pub app: String,
    pub bot: String,
}

pub fn env_var(prefix: &str, agent_id: &str) -> String {
    let suffix: String = agent_id
        .chars()
        .map(|c| match c.is_ascii_alphanumeric() {
            true => c.to_ascii_uppercase(),
            false => '_',
        })
        .collect();
    format!("{prefix}_{suffix}")
}

#[derive(Clone)]
pub struct SlackChannel {
    agent_id: String,
    workspace: Arc<Workspace>,
    app_token: String,
    api_base: String,
    http: reqwest::Client,
    bot: SlackBot,
}

pub struct SlackReader(Vec<(String, SlackBot)>);

impl SlackReader {
    pub fn new(channels: &[SlackChannel]) -> Self {
        Self(
            channels
                .iter()
                .map(|c| (c.agent_id.clone(), c.bot.clone()))
                .collect(),
        )
    }
}

#[async_trait::async_trait]
impl crate::runtime::sessions::Reader for SlackReader {
    fn subscription(&self) -> substructure_session::delivery::Subscription {
        use substructure_session::delivery::{AtLimit, Start, Subscription};
        Subscription::new(super::bot::READER, Start::Beginning, AtLimit::Skip)
    }

    async fn deliver(
        &self,
        _sessions: &crate::runtime::sessions::Sessions,
        session: &substructure_session::Session,
        events: &[crate::runtime::session::SessionEvent],
    ) -> Result<(), crate::runtime::sessions::DeliveryError> {
        let agent = session.state().agent_id.as_deref();
        match self.0.iter().find(|(id, _)| Some(id.as_str()) == agent) {
            Some((_, bot)) => bot.deliver(events).await,
            None => Ok(()),
        }
    }
}

struct StaticResolver(Arc<Workspace>);

#[async_trait::async_trait]
impl WorkspaceResolver for StaticResolver {
    async fn by_tenant(&self, tenant_id: &str, agent_id: &str) -> Option<Arc<Workspace>> {
        (tenant_id == self.0.tenant_id && agent_id == self.0.agent_id).then(|| self.0.clone())
    }
}

impl SlackChannel {
    pub fn new(
        agent_id: String,
        answers: SlackAudience,
        tokens: SlackTokens,
        tenant_id: String,
        api_base: String,
        store: Option<super::StreamStore>,
        blobs: Option<Arc<dyn BlobStore>>,
    ) -> Self {
        let workspace =
            Arc::new(Workspace::new(tokens.bot, tenant_id, agent_id.clone()).answering(answers));
        Self {
            agent_id,
            workspace: workspace.clone(),
            app_token: tokens.app,
            api_base: api_base.clone(),
            http: reqwest::Client::new(),
            bot: SlackBot::new(Arc::new(StaticResolver(workspace)), api_base, store, blobs),
        }
    }

    pub fn with_consent(mut self, consent: Arc<dyn crate::transport::consent::Consent>) -> Self {
        self.bot = self.bot.with_consent(consent);
        self
    }

    pub fn agent_id(&self) -> &str {
        &self.agent_id
    }

    pub fn from_env(
        agent_id: String,
        answers: SlackAudience,
        tenant_id: String,
        store: Option<super::StreamStore>,
        blobs: Option<Arc<dyn BlobStore>>,
    ) -> Result<Self, MissingEnv> {
        let mut missing = Vec::new();
        let mut var = |name: String, desc: &'static str| {
            std::env::var(&name).unwrap_or_else(|_| {
                missing.push((name, desc));
                String::new()
            })
        };
        let app_token = var(
            env_var("SLACK_APP_TOKEN", &agent_id),
            "App-level token with connections:write, for Socket Mode (xapp-…)",
        );
        let bot_token = var(
            env_var("SLACK_BOT_TOKEN", &agent_id),
            "Bot token with app_mentions:read, chat:write, channels:history, im:history, files:read, files:write (xoxb-…)",
        );
        if !missing.is_empty() {
            return Err(MissingEnv(missing));
        }
        let api_base =
            std::env::var("SLACK_API_BASE").unwrap_or_else(|_| "https://slack.com/api".to_string());
        Ok(Self::new(
            agent_id,
            answers,
            SlackTokens {
                app: app_token,
                bot: bot_token,
            },
            tenant_id,
            api_base,
            store,
            blobs,
        ))
    }

    async fn connections_open(&self) -> anyhow::Result<String> {
        let resp: Value = self
            .http
            .post(format!("{}/apps.connections.open", self.api_base))
            .bearer_auth(&self.app_token)
            .send()
            .await?
            .json()
            .await?;
        if resp["ok"].as_bool() != Some(true) {
            anyhow::bail!("apps.connections.open failed: {}", resp["error"]);
        }
        resp["url"]
            .as_str()
            .map(str::to_string)
            .ok_or_else(|| anyhow::anyhow!("apps.connections.open returned no url"))
    }

    async fn connect_and_listen(&self) -> anyhow::Result<()> {
        let url = self.connections_open().await?;
        let (mut ws, _) = tokio_tungstenite::connect_async(&url).await?;
        tracing::info!("slack socket connected");

        while let Some(msg) = ws.next().await {
            match msg? {
                WsMessage::Ping(p) => ws.send(WsMessage::Pong(p)).await?,
                WsMessage::Text(text) => {
                    let envelope: Value = match serde_json::from_str(text.as_str()) {
                        Ok(v) => v,
                        Err(_) => continue,
                    };
                    if envelope["type"].as_str() == Some("events_api") {
                        self.bot
                            .handle_event(&self.workspace, &envelope["payload"])
                            .await;
                    }
                    if envelope["type"].as_str() == Some("interactive") {
                        self.bot
                            .handle_interaction(&self.workspace, &envelope["payload"])
                            .await;
                    }
                    if let Some(id) = envelope["envelope_id"].as_str() {
                        let ack = serde_json::json!({ "envelope_id": id }).to_string();
                        ws.send(WsMessage::text(ack)).await?;
                    }
                    if envelope["type"].as_str() == Some("disconnect") {
                        return Ok(());
                    }
                }
                _ => {}
            }
        }
        Ok(())
    }
}

#[async_trait::async_trait]
impl Channel for SlackChannel {
    fn kind(&self) -> ChannelKind {
        ChannelKind::SLACK
    }

    async fn run(&self, ctx: ChannelContext) {
        if self.bot.start(&ctx).await.is_err() {
            return;
        }
        loop {
            tokio::select! {
                _ = ctx.shutdown.cancelled() => return,
                r = self.connect_and_listen() => {
                    if let Err(e) = r {
                        tracing::warn!(error = %e, "slack socket error");
                    }
                }
            }
            tokio::select! {
                _ = ctx.shutdown.cancelled() => return,
                _ = tokio::time::sleep(RECONNECT_DELAY) => {}
            }
        }
    }
}
