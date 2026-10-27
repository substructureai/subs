use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, PoisonError};
use std::time::Duration;

use serde_json::Value;
use tokio::sync::Notify;

use super::activity::{self, TurnActivity};
use super::render::{self, PromptView, Rendered};
use super::state::StreamStore;
use super::view::{self, Pick};
use super::{
    app_mention, authorize_option, block_action, build_batch, clip, display_of, dm_message, draft,
    prompt_options, resolution_text, section_block, unstamped_ours, with_attachments, Click,
    Inbound, ReplyMeta, SlackFile, AUTHORIZE_ACTION, MAX_FALLBACK, MAX_MARKDOWN, REPLY_EVENT_TYPE,
};
use crate::event_store::Seq;
use crate::manifest::SlackAudience;
use crate::protocol::{
    ClientInput, Content, Requester, Role, SessionOwner, StoredContent, Subject, Visibility,
};
use crate::runtime::blob::{BlobError, BlobRef, BlobStore, NewBlob};
use crate::runtime::session::interrupts::auth::Authorize;
use crate::runtime::sessions::DeliveryError;
use crate::session::command::SessionError;
use crate::session::events::EventPayload;
use crate::session::SessionEvent;
use crate::transport::channel::{ChannelContext, ChannelKind};
use crate::transport::consent::{CliConsent, Consent, WayIn};
use crate::{Caller, HandleClientInput, RuntimeError};

const MAX_IMAGE_BYTES: u64 = 5 * 1024 * 1024;
const MAX_PDF_BYTES: u64 = 10 * 1024 * 1024;
const MAX_TEXT_BYTES: u64 = 1024 * 1024;
const MAX_AUDIO_BYTES: u64 = 10 * 1024 * 1024;
const MAX_VIDEO_BYTES: u64 = 20 * 1024 * 1024;
const IMAGE_MIMES: [&str; 4] = ["image/png", "image/jpeg", "image/gif", "image/webp"];
const IMAGE_NOT_ATTACHED: &str = "_an image could not be attached_";

const ACTIVITY_INTERVAL: Duration = Duration::from_secs(1);
const MAX_CHUNK: usize = 256;
const WORKING_STATUS: &str = "is working…";

fn audience_of(channel: &str) -> Visibility {
    match channel.starts_with('D') {
        true => Visibility::Private,
        false => Visibility::Shared,
    }
}

pub struct Workspace {
    pub bot_token: String,
    pub tenant_id: String,
    pub agent_id: String,
    pub answers: SlackAudience,
    identity: tokio::sync::OnceCell<Identity>,
}

impl Workspace {
    pub fn new(bot_token: String, tenant_id: String, agent_id: String) -> Self {
        Self {
            bot_token,
            tenant_id,
            agent_id,
            answers: SlackAudience::Both,
            identity: tokio::sync::OnceCell::new(),
        }
    }

    pub fn answering(mut self, answers: SlackAudience) -> Self {
        self.answers = answers;
        self
    }

    fn serves(&self, channel: &str) -> bool {
        match channel.starts_with('D') {
            true => self.answers.dms(),
            false => self.answers.channels(),
        }
    }

    fn session_id(&self, channel: &str, thread_ts: &str) -> String {
        format!("slack:{}:{}:{}", self.agent_id, channel, thread_ts)
    }
}

#[derive(Default)]
struct Identity {
    ours: Vec<String>,
    team: Option<String>,
}

#[async_trait::async_trait]
pub trait WorkspaceResolver: Send + Sync {
    async fn by_tenant(&self, tenant_id: &str, agent_id: &str) -> Option<Arc<Workspace>>;
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct StreamKey {
    tenant_id: String,
    session_id: String,
    turn_id: String,
    agent_id: String,
}

impl StreamKey {
    fn new(ws: &Workspace, session_id: &str, turn_id: &str) -> Self {
        Self::parts(&ws.tenant_id, session_id, turn_id, &ws.agent_id)
    }

    fn parts(tenant_id: &str, session_id: &str, turn_id: &str, agent_id: &str) -> Self {
        Self {
            tenant_id: tenant_id.to_string(),
            session_id: session_id.to_string(),
            turn_id: turn_id.to_string(),
            agent_id: agent_id.to_string(),
        }
    }
}

#[derive(Clone)]
struct Thread {
    channel: String,
    ts: String,
}

impl Thread {
    fn new(channel: &str, ts: &str) -> Self {
        Self {
            channel: channel.to_string(),
            ts: ts.to_string(),
        }
    }
}

#[derive(Clone)]
struct Stream {
    tenant_id: String,
    session_id: String,
    turn_id: String,
    agent_id: String,
    thread: Thread,
    started_at: chrono::DateTime<chrono::Utc>,
    title: String,
    turn: TurnActivity,
    recipient: Option<String>,
    recipient_team: Option<String>,
    ts: Option<String>,
    sent: HashMap<String, Value>,
    dead: bool,
    version: u64,
}

impl Stream {
    fn key(&self) -> StreamKey {
        StreamKey::parts(
            &self.tenant_id,
            &self.session_id,
            &self.turn_id,
            &self.agent_id,
        )
    }
}

fn owner_thread(meta: &crate::session::state::EventMeta) -> Option<Thread> {
    let owner = meta.owner.as_ref()?;
    Some(Thread::new(
        owner.metadata.get("slack_channel")?,
        owner.metadata.get("slack_thread_ts")?,
    ))
}

fn attachment_cap(mime: &str) -> Option<u64> {
    if IMAGE_MIMES.contains(&mime) {
        Some(MAX_IMAGE_BYTES)
    } else if mime == "application/pdf" {
        Some(MAX_PDF_BYTES)
    } else if crate::mime::text_like(mime) {
        Some(MAX_TEXT_BYTES)
    } else {
        match crate::mime::essence(mime) {
            "audio" => Some(MAX_AUDIO_BYTES),
            "video" => Some(MAX_VIDEO_BYTES),
            _ => None,
        }
    }
}

fn without_images(blocks: &[Value]) -> Option<Vec<Value>> {
    let mut kept: Vec<Value> = blocks
        .iter()
        .filter(|b| b["type"] != "image")
        .cloned()
        .collect();
    (kept.len() != blocks.len()).then(|| {
        kept.push(render::context_block(IMAGE_NOT_ATTACHED));
        kept
    })
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

fn card_chunks(blocks: &[Value]) -> Vec<(String, Value)> {
    blocks
        .iter()
        .filter(|block| block["type"] == "task_card")
        .filter_map(|block| {
            let id = block["task_id"].as_str()?;
            let mut chunk = serde_json::json!({
                "type": "task_update",
                "id": id,
                "status": block["status"],
            });
            if let Some(title) = block["title"].as_str() {
                chunk["title"] = clip(title, render::MAX_TITLE).into();
            }
            if let Some(text) = rich_text_string(&block["details"]) {
                chunk["details"] = text.into();
            }
            Some((id.to_string(), chunk))
        })
        .collect()
}

fn rich_text_string(block: &Value) -> Option<String> {
    let sections = block["elements"].as_array()?;
    let text: String = sections
        .iter()
        .flat_map(|s| s["elements"].as_array().into_iter().flatten())
        .filter_map(|e| e["text"].as_str())
        .collect();
    (!text.trim().is_empty()).then_some(text)
}

fn wire_chunks(
    sent: &HashMap<String, Value>,
    desired: &[(String, Value)],
) -> (Vec<Value>, Vec<(String, Value)>) {
    let mut wire = Vec::new();
    let mut landed = Vec::new();
    for (id, want) in desired {
        let have = sent.get(id);
        if have == Some(want) {
            continue;
        }
        let old = have.and_then(|h| h["details"].as_str()).unwrap_or("");
        let new = want["details"].as_str().unwrap_or("");
        let delta = new.strip_prefix(old).unwrap_or("");
        let moved =
            have.is_none_or(|h| h["title"] != want["title"] || h["status"] != want["status"]);
        if delta.is_empty() && !moved {
            continue;
        }
        let mut head = serde_json::json!({
            "type": "task_update",
            "id": id,
            "status": want["status"],
        });
        if !want["title"].is_null() {
            head["title"] = want["title"].clone();
        }
        if delta.is_empty() {
            wire.push(head);
        } else {
            let pieces: Vec<String> = delta
                .chars()
                .collect::<Vec<_>>()
                .chunks(MAX_CHUNK)
                .map(|c| c.iter().collect())
                .collect();
            for piece in pieces {
                let mut chunk = head.clone();
                chunk["details"] = piece.into();
                wire.push(chunk);
            }
        }
        let mut state = want.clone();
        let grown = format!("{old}{delta}");
        if !grown.is_empty() {
            state["details"] = grown.into();
        }
        landed.push((id.clone(), state));
    }
    (wire, landed)
}

#[derive(Default)]
struct Streams {
    open: Mutex<HashMap<StreamKey, Stream>>,
    dirty: Mutex<HashSet<StreamKey>>,
    notify: Notify,
}

impl Streams {
    fn insert(&self, stream: Stream) {
        lock(&self.open).insert(stream.key(), stream);
    }

    fn track_new(&self, stream: Stream) {
        lock(&self.open).entry(stream.key()).or_insert(stream);
    }

    fn remove(&self, key: &StreamKey) {
        lock(&self.open).remove(key);
    }

    fn take_session(&self, tenant_id: &str, session_id: &str) -> Vec<Stream> {
        let mut open = lock(&self.open);
        let mine: Vec<StreamKey> = open
            .keys()
            .filter(|k| k.tenant_id == tenant_id && k.session_id == session_id)
            .cloned()
            .collect();
        mine.iter().filter_map(|key| open.remove(key)).collect()
    }

    fn take(&self, key: &StreamKey) -> Option<Stream> {
        lock(&self.open).remove(key).filter(|s| !s.dead)
    }

    fn get(&self, key: &StreamKey) -> Option<Stream> {
        lock(&self.open).get(key).cloned()
    }

    fn kill(&self, key: &StreamKey) {
        if let Some(stream) = lock(&self.open).get_mut(key) {
            stream.dead = true;
        }
    }

    fn open_elsewhere(&self, key: &StreamKey) -> bool {
        lock(&self.open).iter().any(|(k, s)| {
            k.tenant_id == key.tenant_id
                && k.session_id == key.session_id
                && k.turn_id != key.turn_id
                && !s.dead
                && s.ts.is_some()
        })
    }

    fn commit(
        &self,
        key: &StreamKey,
        ts: String,
        version: u64,
        sent: Vec<(String, Value)>,
    ) -> bool {
        let mut open = lock(&self.open);
        let Some(stream) = open.get_mut(key) else {
            return false;
        };
        stream.ts = Some(ts);
        stream.version = version;
        stream.sent.extend(sent);
        true
    }

    fn live_elsewhere(
        &self,
        tenant_id: &str,
        session_id: &str,
        turn_id: Option<&str>,
    ) -> Option<String> {
        lock(&self.open)
            .values()
            .find(|s| {
                s.tenant_id == tenant_id
                    && s.session_id == session_id
                    && Some(s.turn_id.as_str()) != turn_id
                    && !s.dead
            })
            .map(|s| s.turn_id.clone())
    }

    fn note(&self, key: &StreamKey, event: &SessionEvent) {
        if let Some(stream) = lock(&self.open).get_mut(key) {
            stream.turn.apply(event);
        }
    }

    fn mark_dirty(&self, key: StreamKey) {
        lock(&self.dirty).insert(key);
        self.notify.notify_one();
    }

    fn take_dirty(&self) -> Option<StreamKey> {
        let mut dirty = lock(&self.dirty);
        let key = dirty.iter().next().cloned()?;
        dirty.remove(&key);
        Some(key)
    }

    async fn notified(&self) {
        self.notify.notified().await;
    }
}

#[derive(Clone)]
pub struct SlackBot {
    resolver: Arc<dyn WorkspaceResolver>,
    api_base: String,
    http: reqwest::Client,
    ctx: Arc<OnceLock<ChannelContext>>,
    streams: Arc<Streams>,
    store: Option<Arc<StreamStore>>,
    blobs: Option<Arc<dyn BlobStore>>,
    consent: Arc<dyn Consent>,
}

impl SlackBot {
    pub fn new(
        resolver: Arc<dyn WorkspaceResolver>,
        api_base: String,
        store: Option<StreamStore>,
        blobs: Option<Arc<dyn BlobStore>>,
    ) -> Self {
        Self {
            resolver,
            api_base,
            http: reqwest::Client::new(),
            ctx: Arc::new(OnceLock::new()),
            streams: Arc::new(Streams::default()),
            store: store.map(Arc::new),
            blobs,
            consent: Arc::new(CliConsent),
        }
    }

    async fn requester(
        &self,
        ws: &Workspace,
        team: Option<&str>,
        user: &str,
        visibility: Visibility,
    ) -> Requester {
        let team = match team {
            Some(team) => Some(team.to_string()),
            None => self.identity(ws).await.and_then(|i| i.team.clone()),
        };
        match team {
            Some(team) => Requester::new(
                Subject::new(ChannelKind::SLACK.issuer(), format!("{team}:{user}")),
                visibility,
            ),
            None => Requester::machine(),
        }
    }

    pub fn with_consent(mut self, consent: Arc<dyn Consent>) -> Self {
        self.consent = consent;
        self
    }

    async fn way_in(&self, tenant_id: &str, payload: &serde_json::Value) -> Option<WayIn> {
        let authorize: Authorize =
            serde_json::from_value(payload.get("authorize")?.clone()).ok()?;
        self.consent.way_in(tenant_id, &authorize).await
    }

    pub async fn start(&self, ctx: &ChannelContext) -> Result<(), RuntimeError> {
        let _ = self.ctx.set(ctx.clone());
        let (this, activity_ctx) = (self.clone(), ctx.clone());
        tokio::spawn(async move { this.stream_activity(&activity_ctx).await });
        Ok(())
    }

    pub async fn handle_event(&self, ws: &Workspace, payload: &Value) {
        let Some(inbound) = app_mention(payload).or_else(|| dm_message(payload)) else {
            return;
        };
        let Some(ctx) = self.ctx.get().cloned() else {
            tracing::warn!("slack: event before start; dropped");
            return;
        };
        self.submit(&ctx, ws, inbound).await;
    }

    pub async fn handle_interaction(&self, ws: &Workspace, payload: &Value) {
        let Some(ctx) = self.ctx.get().cloned() else {
            tracing::warn!("slack: interaction before start; dropped");
            return;
        };
        let Some(click) = block_action(payload) else {
            return;
        };
        if click.action_id == AUTHORIZE_ACTION {
            return;
        }
        self.submit_click(&ctx, ws, click).await;
    }

    async fn submit_click(&self, ctx: &ChannelContext, ws: &Workspace, click: Click) {
        let agent_id = ws.agent_id.as_str();
        let session_id = click
            .session
            .clone()
            .unwrap_or_else(|| ws.session_id(&click.channel, &click.thread_ts));
        let Some(picked) = self.pick(ctx, ws, &session_id, &click).await else {
            return;
        };
        let input = match picked {
            Pick::Resume(resumption) => ClientInput::InterruptResume { resumption },
            Pick::Stale(view) => {
                let meta = ReplyMeta {
                    session_id: Some(session_id.clone()),
                    ..Default::default()
                };
                let cleared = self
                    .update(
                        ws,
                        &click.channel,
                        &click.message_ts,
                        &view.text,
                        view.blocks,
                        &meta,
                    )
                    .await;
                if let Err(e) = cleared {
                    tracing::warn!(error = %e, %session_id, "slack: stale prompt not cleared");
                }
                return;
            }
            Pick::Other => ClientInput::Action {
                agent_id: agent_id.to_string(),
                turn_id: None,
                name: click.action_id.clone(),
                args: Some(click.args()),
            },
        };
        let clicked = self
            .requester(ws, None, &click.user, audience_of(&click.channel))
            .await;
        let submitted = ctx
            .handle_client_input(HandleClientInput {
                session_id: session_id.clone(),
                agent: None,
                worker: None,
                caller: Caller::System {
                    tenant_id: ws.tenant_id.clone(),
                },
                owner: SessionOwner {
                    tenant_id: ws.tenant_id.clone(),
                    requester: clicked,
                    metadata: HashMap::from_iter(
                        [
                            Some(("slack_channel".to_string(), click.channel.clone())),
                            Some(("slack_thread_ts".to_string(), click.thread_ts.clone())),
                            Some(("slack_user".to_string(), click.user.clone())),
                        ]
                        .into_iter()
                        .flatten(),
                    ),
                },
                input,
                span: crate::span::SpanContext::root().child("slack_click"),
            })
            .await;
        if let Err(e) = submitted {
            tracing::warn!(error = %e, %session_id, "slack: click submit failed");
        }
    }

    async fn pick(
        &self,
        ctx: &ChannelContext,
        ws: &Workspace,
        session_id: &str,
        click: &Click,
    ) -> Option<Pick> {
        if serde_json::from_str::<super::ButtonValue>(&click.value).is_err() {
            return Some(Pick::Other);
        }
        match ctx.get_session(&ws.tenant_id, session_id).await {
            Ok(session) => Some(view::pick(session.state(), click)),
            Err(e) => {
                tracing::warn!(error = %e, %session_id, "slack: session not read for a click");
                None
            }
        }
    }

    async fn identity<'a>(&self, ws: &'a Workspace) -> Option<&'a Identity> {
        let loaded = ws
            .identity
            .get_or_try_init(|| async {
                let resp = self
                    .api_call(ws, "auth.test", serde_json::json!({}))
                    .await?;
                Ok::<_, Error>(Identity {
                    ours: ["user_id", "bot_id"]
                        .iter()
                        .filter_map(|k| resp[k].as_str().map(str::to_string))
                        .collect(),
                    team: resp["team_id"].as_str().map(str::to_string),
                })
            })
            .await;
        match loaded {
            Ok(identity) => Some(identity),
            Err(e) => {
                tracing::warn!(error = %e, "slack: auth.test failed; own posts unrecognised");
                None
            }
        }
    }

    async fn fetch_thread(
        &self,
        ws: &Workspace,
        thread: &Thread,
        oldest: Option<&str>,
    ) -> Result<Vec<super::SlackMsg>, Error> {
        let resp = self.fetch_replies_raw(ws, thread, oldest).await?;
        let ours = self.identity(ws).await.map_or(&[][..], |i| &i.ours);
        Ok(super::parse_replies(&resp, ours))
    }

    async fn fetch_replies_raw(
        &self,
        ws: &Workspace,
        thread: &Thread,
        oldest: Option<&str>,
    ) -> Result<Value, Error> {
        let (channel, thread_ts) = (&thread.channel, &thread.ts);
        let mut url = format!(
            "{}/conversations.replies?channel={channel}&ts={thread_ts}&limit=200&include_all_metadata=true",
            self.api_base
        );
        if let Some(oldest) = oldest {
            url.push_str(&format!("&oldest={oldest}"));
        }
        let resp: Value = self
            .http
            .get(url)
            .bearer_auth(&ws.bot_token)
            .send()
            .await
            .map_err(Error::retryable)?
            .json()
            .await
            .map_err(Error::retryable)?;
        if resp["ok"].as_bool() != Some(true) {
            return Err(Error::from_response(&resp));
        }
        Ok(resp)
    }

    async fn store_attachments(
        &self,
        ws: &Workspace,
        files: impl Iterator<Item = SlackFile>,
    ) -> HashMap<String, StoredContent> {
        let Some(blobs) = &self.blobs else {
            return HashMap::new();
        };
        let mut out = HashMap::new();
        for f in files {
            let Some(cap) = attachment_cap(&f.mimetype) else {
                continue;
            };
            if f.size > cap {
                tracing::warn!(file = %f.id, size = f.size, "slack: attachment over size cap; skipped");
                continue;
            }
            let bytes = match self.download_file(ws, &f.url_private).await {
                Ok(bytes) => bytes,
                Err(e) => {
                    tracing::warn!(error = %e, file = %f.id, "slack: file download failed (bot token lacks files:read?)");
                    continue;
                }
            };
            let stored = blobs
                .put(NewBlob {
                    tenant_id: ws.tenant_id.clone(),
                    mime: f.mimetype.clone(),
                    name: f.name.clone(),
                    bytes,
                })
                .await;
            match stored {
                Ok(r) => {
                    out.insert(f.id, StoredContent::Blob { uri: r.uri() });
                }
                Err(e) => tracing::warn!(error = %e, file = %f.id, "slack: blob put failed"),
            }
        }
        out
    }

    async fn download_file(&self, ws: &Workspace, url: &str) -> Result<Vec<u8>, Error> {
        let resp = self
            .http
            .get(url)
            .bearer_auth(&ws.bot_token)
            .send()
            .await
            .map_err(Error::retryable)?;
        if !resp.status().is_success() {
            return Err(Error::Terminal(format!("http {}", resp.status())));
        }
        let html = resp
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .is_some_and(|ct| ct.contains("text/html"));
        if html {
            return Err(Error::Terminal("got html, not the file".to_string()));
        }
        Ok(resp.bytes().await.map_err(Error::retryable)?.to_vec())
    }

    async fn turn_image_blocks(&self, ws: &Workspace, session_id: &str) -> Vec<Value> {
        if self.blobs.is_none() {
            return Vec::new();
        }
        let path = self.session_path(ws, session_id).await;
        let Some(message) = path.last().filter(|m| matches!(m.role, Role::Assistant)) else {
            return Vec::new();
        };
        let Some(Content::Parts(parts)) = &message.content else {
            return Vec::new();
        };
        let mut blocks = Vec::new();
        for part in parts {
            let StoredContent::Blob { uri } = part else {
                continue;
            };
            match self.slack_image_block(ws, uri).await {
                Ok(Some(block)) => blocks.push(block),
                Ok(None) => {}
                Err(e) => {
                    tracing::warn!(error = %e, %session_id, "slack: image not attached");
                    blocks.push(render::context_block(IMAGE_NOT_ATTACHED));
                }
            }
        }
        blocks
    }

    async fn slack_image_block(&self, ws: &Workspace, url: &str) -> Result<Option<Value>, Error> {
        if url.starts_with("https://") {
            return Ok(Some(serde_json::json!({
                "type": "image", "image_url": url, "alt_text": "image",
            })));
        }
        let Some(r) = BlobRef::parse(url) else {
            return Ok(None);
        };
        if r.tenant_id != ws.tenant_id || !r.mime.starts_with("image/") {
            return Ok(None);
        }
        let file_id = self.slack_file_for(ws, &r).await?;
        Ok(Some(serde_json::json!({
            "type": "image",
            "slack_file": { "id": file_id },
            "alt_text": r.name.as_deref().unwrap_or("image"),
        })))
    }

    async fn slack_file_for(&self, ws: &Workspace, r: &BlobRef) -> Result<String, Error> {
        if let Some(store) = self.store.as_deref() {
            if let Ok(Some(id)) = store.slack_file(&ws.tenant_id, &r.id).await {
                return Ok(id);
            }
        }
        let blobs = self
            .blobs
            .as_ref()
            .ok_or_else(|| Error::Terminal("no blob store".to_string()))?;
        let bytes = blobs.get(r).await.map_err(|e| match e {
            BlobError::Io(m) => Error::Retryable(m),
            e => Error::Terminal(e.to_string()),
        })?;
        let name = r
            .name
            .clone()
            .unwrap_or_else(|| format!("image.{}", r.mime.strip_prefix("image/").unwrap_or("bin")));
        let file_id = self.upload_file(ws, &name, bytes).await?;
        if let Some(store) = self.store.as_deref() {
            if let Err(e) = store
                .record_slack_file(&ws.tenant_id, &r.id, &file_id)
                .await
            {
                tracing::warn!(error = %e, "slack: uploaded file not recorded");
            }
        }
        Ok(file_id)
    }

    async fn upload_file(
        &self,
        ws: &Workspace,
        name: &str,
        bytes: Vec<u8>,
    ) -> Result<String, Error> {
        let query =
            serde_urlencoded::to_string([("filename", name), ("length", &bytes.len().to_string())])
                .map_err(Error::retryable)?;
        let resp: Value = self
            .http
            .get(format!(
                "{}/files.getUploadURLExternal?{query}",
                self.api_base
            ))
            .bearer_auth(&ws.bot_token)
            .send()
            .await
            .map_err(Error::retryable)?
            .json()
            .await
            .map_err(Error::retryable)?;
        if resp["ok"].as_bool() != Some(true) {
            return Err(Error::from_response(&resp));
        }
        let (Some(upload_url), Some(file_id)) =
            (resp["upload_url"].as_str(), resp["file_id"].as_str())
        else {
            return Err(Error::Terminal(
                "upload url response incomplete".to_string(),
            ));
        };
        let put = self
            .http
            .post(upload_url)
            .body(bytes)
            .send()
            .await
            .map_err(Error::retryable)?;
        if !put.status().is_success() {
            return Err(Error::Retryable(format!("upload http {}", put.status())));
        }
        let file_id = file_id.to_string();
        self.api_call(
            ws,
            "files.completeUploadExternal",
            serde_json::json!({ "files": [{ "id": file_id, "title": name }] }),
        )
        .await?;
        Ok(file_id)
    }

    async fn session_path(
        &self,
        ws: &Workspace,
        session_id: &str,
    ) -> Vec<crate::protocol::Message> {
        let Some(ctx) = self.ctx.get() else {
            return Vec::new();
        };
        match ctx.get_session(&ws.tenant_id, session_id).await {
            Ok(session) => {
                let tree = session.state().message_tree();
                match &tree.head_id {
                    Some(head) => tree.path_to(head),
                    None => Vec::new(),
                }
            }
            Err(_) => Vec::new(),
        }
    }

    fn still_waiting(payload: &Value) -> Option<String> {
        let display = display_of(payload)?;
        Some(format!("Still waiting on this:\n\n{}", display.message))
    }

    async fn remind_of_prompt(
        &self,
        ws: &Workspace,
        thread: &Thread,
        session_id: &str,
        turn_id: Option<String>,
    ) {
        let Some(ctx) = self.ctx.get() else { return };
        let Ok(session) = ctx.get_session(&ws.tenant_id, session_id).await else {
            return;
        };
        let Some(open) = session.state().at_head().active_interrupt_for() else {
            return;
        };
        let Some(text) = Self::still_waiting(&open.payload) else {
            return;
        };
        let meta = ReplyMeta {
            turn_id,
            session_id: Some(session_id.to_string()),
            ..Default::default()
        };
        if let Err(e) = self.post(ws, thread, &text, &meta).await {
            tracing::warn!(error = %e, %session_id, "slack: failed to repeat the open prompt");
        }
    }

    async fn submit(&self, ctx: &ChannelContext, ws: &Workspace, inbound: Inbound) {
        if !ws.serves(&inbound.channel) {
            tracing::debug!(channel = %inbound.channel, "slack: outside what this app answers");
            return;
        }
        let agent_id = ws.agent_id.clone();
        let thread = Thread::new(&inbound.channel, &inbound.thread_ts);
        let session_id = ws.session_id(&inbound.channel, &inbound.thread_ts);
        let turn_id = Some(format!("slack:{}:{}", inbound.channel, inbound.ts));

        let path = self.session_path(ws, &session_id).await;
        let cursor = path
            .iter()
            .filter_map(|m| m.id.strip_prefix("slack:"))
            .max();

        let fetched = self.fetch_thread(ws, &thread, cursor).await;

        let uploads = {
            let seen: HashSet<&str> = path.iter().map(|m| m.id.as_str()).collect();
            let mut unseen: HashMap<String, SlackFile> = HashMap::new();
            let mut collect = |ts: &str, files: &[SlackFile]| {
                if !seen.contains(format!("slack:{ts}").as_str()) {
                    for f in files {
                        unseen.insert(f.id.clone(), f.clone());
                    }
                }
            };
            if let Ok(replies) = &fetched {
                for msg in replies.iter().filter(|m| m.meta.is_none()) {
                    collect(&msg.ts, &msg.files);
                }
            }
            collect(&inbound.ts, &inbound.files);
            self.store_attachments(ws, unseen.into_values()).await
        };

        let input = match fetched {
            Ok(replies) => ClientInput::Append {
                agent_id: agent_id.clone(),
                turn_id: turn_id.clone(),
                messages: build_batch(&path, &replies, &inbound, &uploads),
                stream: false,
                client: Default::default(),
                queue: crate::protocol::QueueMode::Turn,
            },
            Err(e) => {
                let hint = if e.code() != "missing_scope" {
                    ""
                } else if inbound.channel.starts_with('D') {
                    " (bot token lacks im:history?)"
                } else {
                    " (bot token lacks channels:history?)"
                };
                tracing::warn!(error = %e, "slack: history fetch failed{hint}; appending message only");
                ClientInput::Message {
                    agent_id: agent_id.clone(),
                    turn_id: turn_id.clone(),
                    message: draft(
                        &format!("slack:{}", inbound.ts),
                        Role::User,
                        with_attachments(
                            format!(
                                "<@{}>: {}\n\n[note: the Slack conversation could not be fetched — \
                                 earlier messages may be missing from your context]",
                                inbound.user, inbound.text
                            ),
                            &inbound.files,
                            &uploads,
                        ),
                    ),
                    stream: false,
                    queue: crate::protocol::QueueMode::Turn,
                }
            }
        };
        let inbound_requester = self
            .requester(
                ws,
                inbound.team.as_deref(),
                &inbound.user,
                audience_of(&inbound.channel),
            )
            .await;
        let submitted = ctx
            .handle_client_input(HandleClientInput {
                session_id: session_id.clone(),
                agent: None,
                worker: None,
                caller: Caller::System {
                    tenant_id: ws.tenant_id.clone(),
                },
                owner: SessionOwner {
                    tenant_id: ws.tenant_id.clone(),
                    requester: inbound_requester,
                    metadata: HashMap::from_iter(
                        [
                            Some(("slack_channel".into(), inbound.channel.clone())),
                            Some(("slack_thread_ts".into(), inbound.thread_ts.clone())),
                            Some(("slack_user".into(), inbound.user.clone())),
                            inbound.team.clone().map(|team| ("slack_team".into(), team)),
                        ]
                        .into_iter()
                        .flatten(),
                    ),
                },
                input,
                span: crate::span::SpanContext::root().child("slack_inbound"),
            })
            .await;
        match submitted {
            Ok(_) => {}
            Err(RuntimeError::Session(
                SessionError::TurnAlreadyActive { .. } | SessionError::TurnAlreadyCompleted { .. },
            )) => {}
            Err(RuntimeError::Session(SessionError::SessionInterrupted)) => {
                self.remind_of_prompt(ws, &thread, &session_id, turn_id)
                    .await;
            }
            Err(e) => {
                let meta = ReplyMeta {
                    turn_id,
                    session_id: Some(session_id),
                    ..Default::default()
                };
                if let Err(post) = self.post(ws, &thread, &format!("Error: {e}"), &meta).await {
                    tracing::warn!(error = %post, "slack: failed to post submit error");
                }
            }
        }
    }

    async fn post(
        &self,
        ws: &Workspace,
        thread: &Thread,
        text: &str,
        meta: &ReplyMeta,
    ) -> Result<(), Error> {
        self.post_blocks(ws, thread, text, vec![section_block(text)], meta)
            .await
    }

    async fn post_blocks(
        &self,
        ws: &Workspace,
        thread: &Thread,
        text: &str,
        blocks: Vec<Value>,
        meta: &ReplyMeta,
    ) -> Result<(), Error> {
        let body = |blocks: &[Value]| {
            serde_json::json!({
                "channel": thread.channel,
                "thread_ts": thread.ts,
                "text": clip(text, MAX_FALLBACK),
                "blocks": blocks,
                "metadata": {
                    "event_type": REPLY_EVENT_TYPE,
                    "event_payload": meta,
                },
            })
        };
        match self.api_call(ws, "chat.postMessage", body(&blocks)).await {
            Err(e) if e.code() == "invalid_blocks" => {
                let Some(stripped) = without_images(&blocks) else {
                    return Err(e);
                };
                self.api_call(ws, "chat.postMessage", body(&stripped))
                    .await
                    .map(|_| ())
            }
            r => r.map(|_| ()),
        }
    }

    async fn update(
        &self,
        ws: &Workspace,
        channel: &str,
        ts: &str,
        text: &str,
        blocks: Vec<Value>,
        meta: &ReplyMeta,
    ) -> Result<(), Error> {
        let body = |blocks: &[Value]| {
            serde_json::json!({
                "channel": channel,
                "ts": ts,
                "text": clip(text, MAX_FALLBACK),
                "blocks": blocks,
                "metadata": {
                    "event_type": REPLY_EVENT_TYPE,
                    "event_payload": meta,
                },
            })
        };
        match self.api_call(ws, "chat.update", body(&blocks)).await {
            Err(e) if e.code() == "invalid_blocks" => {
                let Some(stripped) = without_images(&blocks) else {
                    return Err(e);
                };
                self.api_call(ws, "chat.update", body(&stripped))
                    .await
                    .map(|_| ())
            }
            r => r.map(|_| ()),
        }
    }

    async fn settle_status(
        &self,
        ws: &Workspace,
        thread: &Thread,
        event: &SessionEvent,
        ended: Option<&str>,
    ) {
        if self
            .streams
            .live_elsewhere(&event.tenant_id, &event.session_id, ended)
            .is_none()
        {
            self.set_status(ws, thread, "").await;
        }
    }

    async fn set_status(&self, ws: &Workspace, thread: &Thread, status: &str) {
        let body = serde_json::json!({
            "channel_id": thread.channel,
            "thread_ts": thread.ts,
            "status": status,
        });
        if let Err(e) = self.api_call(ws, "assistant.threads.setStatus", body).await {
            tracing::debug!(error = %e, channel = %thread.channel, "slack: status not set");
        }
    }

    async fn api_call(&self, ws: &Workspace, method: &str, body: Value) -> Result<Value, Error> {
        let resp = self
            .http
            .post(format!("{}/{method}", self.api_base))
            .bearer_auth(&ws.bot_token)
            .json(&body)
            .send()
            .await
            .map_err(Error::retryable)?;
        let status = resp.status();
        if status.is_server_error() || status.as_u16() == 429 {
            return Err(Error::Retryable(format!("http {status}")));
        }
        let v: Value = resp.json().await.map_err(Error::retryable)?;
        if v["ok"].as_bool() == Some(true) {
            return Ok(v);
        }
        Err(Error::from_response(&v))
    }

    async fn start_stream(
        &self,
        ws: &Workspace,
        thread: &Thread,
        recipient: Option<&str>,
        recipient_team: Option<&str>,
    ) -> Result<String, Error> {
        let mut body = serde_json::json!({
            "channel": thread.channel,
            "thread_ts": thread.ts,
            "task_display_mode": "timeline",
        });
        if let Some(recipient) = recipient {
            body["recipient_user_id"] = recipient.into();
        }
        let team = match recipient_team {
            Some(team) => Some(team.to_string()),
            None => self.identity(ws).await.and_then(|i| i.team.clone()),
        };
        if let Some(team) = team {
            body["recipient_team_id"] = team.into();
        }
        let resp = self.api_call(ws, "chat.startStream", body).await?;
        resp["ts"]
            .as_str()
            .map(str::to_string)
            .ok_or_else(|| Error::Terminal("startStream returned no ts".into()))
    }

    async fn append_stream(
        &self,
        ws: &Workspace,
        channel: &str,
        ts: &str,
        chunks: Vec<Value>,
    ) -> Result<(), Error> {
        self.api_call(
            ws,
            "chat.appendStream",
            serde_json::json!({ "channel": channel, "ts": ts, "chunks": chunks }),
        )
        .await
        .map(|_| ())
    }

    async fn stop_stream(
        &self,
        ws: &Workspace,
        channel: &str,
        ts: &str,
        chunk: Value,
        meta: &ReplyMeta,
    ) -> Result<(), Error> {
        self.api_call(
            ws,
            "chat.stopStream",
            serde_json::json!({
                "channel": channel,
                "ts": ts,
                "chunks": [chunk],
                "metadata": {
                    "event_type": REPLY_EVENT_TYPE,
                    "event_payload": meta,
                },
            }),
        )
        .await
        .map(|_| ())
    }

    async fn close_stream(&self, ws: &Workspace, channel: &str, ts: &str) {
        let body = serde_json::json!({ "channel": channel, "ts": ts });
        if let Err(e) = self.api_call(ws, "chat.stopStream", body).await {
            tracing::debug!(error = %e, channel, "slack: stream already closed");
        }
    }
}

pub const READER: &str = "slack";

#[async_trait::async_trait]
impl crate::runtime::sessions::Reader for SlackBot {
    fn subscription(&self) -> substructure_session::delivery::Subscription {
        use substructure_session::delivery::{AtLimit, Start, Subscription};
        Subscription::new(READER, Start::Beginning, AtLimit::Skip)
    }

    async fn deliver(
        &self,
        _sessions: &crate::runtime::sessions::Sessions,
        _session: &substructure_session::Session,
        events: &[SessionEvent],
    ) -> Result<(), DeliveryError> {
        SlackBot::deliver(self, events).await
    }
}

impl SlackBot {
    pub async fn deliver(&self, events: &[SessionEvent]) -> Result<(), DeliveryError> {
        if self.ctx.get().is_none() {
            return Err(DeliveryError::retry(
                None,
                "the slack channel has not started",
            ));
        }
        for event in events {
            let at = Some(event.seq);
            match self.apply(event.clone()).await {
                Ok(()) => {}
                Err(Error::Retryable(e)) => return Err(DeliveryError::retry(at, e)),
                Err(Error::Terminal(e)) => return Err(DeliveryError::permanent(at, e)),
            }
        }
        Ok(())
    }
}

impl SlackBot {
    async fn apply(&self, event: SessionEvent) -> Result<(), Error> {
        let Some(thread) = owner_thread(&event.meta) else {
            return Ok(());
        };
        let Some(agent_id) = event.meta.agent_id.as_deref() else {
            return Ok(());
        };
        let Some(ws) = self.resolver.by_tenant(&event.tenant_id, agent_id).await else {
            return Ok(());
        };
        self.track(&ws, &thread, &event)
            .await
            .map_err(Error::retryable)?;
        if matches!(event.payload, EventPayload::TurnStarted(_)) {
            self.set_status(&ws, &thread, WORKING_STATUS).await;
        }
        let result = match &event.payload {
            EventPayload::TurnCompleted(t) => {
                self.complete_turn(
                    &ws,
                    &thread,
                    &event.session_id,
                    t,
                    event.occurred_at,
                    event.seq,
                )
                .await
            }
            EventPayload::SessionInterrupted(p) => {
                self.post_interrupt(
                    &ws,
                    &thread,
                    &event.session_id,
                    event.meta.turn_id.as_deref(),
                    p,
                    event.seq,
                )
                .await
            }
            EventPayload::InterruptResumed(p) => self.settle_prompt(&ws, &thread, p).await,
            _ => return Ok(()),
        };
        match &event.payload {
            EventPayload::TurnCompleted(t) => {
                self.settle_status(&ws, &thread, &event, Some(&t.turn_id))
                    .await
            }
            EventPayload::SessionInterrupted(_) => {
                self.settle_status(&ws, &thread, &event, event.meta.turn_id.as_deref())
                    .await
            }
            _ => {}
        }
        if matches!(result, Ok(()) | Err(Error::Terminal(_))) {
            if let EventPayload::TurnCompleted(t) = &event.payload {
                if let Some(store) = self.store.as_deref() {
                    if let Err(e) = store
                        .clear_turn(&event.tenant_id, &event.session_id, &t.turn_id)
                        .await
                    {
                        tracing::warn!(session_id = %event.session_id, error = %e, "slack: stream state not cleared");
                    }
                }
            }
        }
        result
    }
}

impl SlackBot {
    async fn complete_turn(
        &self,
        ws: &Workspace,
        thread: &Thread,
        session_id: &str,
        t: &crate::session::events::TurnCompleted,
        at: chrono::DateTime<chrono::Utc>,
        seq: u64,
    ) -> Result<(), Error> {
        let key = StreamKey::new(ws, session_id, &t.turn_id);
        let live = match self.streams.take(&key) {
            Some(live) => Some(live),
            None => self.recover(ws, &key, seq).await,
        };
        let silent = live
            .as_ref()
            .is_some_and(|s| s.ts.is_none() && s.turn.is_empty());
        if silent && t.error.is_none() && t.data.is_null() {
            return Ok(());
        }
        let elapsed = live
            .as_ref()
            .and_then(|s| (at - s.started_at).to_std().ok());
        let rendered = match (&live, &t.error) {
            (Some(s), None) => {
                let footer = activity::elapsed(s.started_at, at);
                view::finished(&s.turn, &t.data, &s.title, Some(footer))
            }
            (Some(s), Some(error)) => {
                let line = format!("Error: {error}");
                let mut blocks = render::settle_cards(&view::live(&s.turn, &s.title).blocks);
                blocks.push(section_block(&line));
                Rendered { text: line, blocks }
            }
            (None, _) => render::render_turn(t, elapsed),
        };
        let mut rendered = rendered;
        if t.error.is_none() {
            rendered
                .blocks
                .extend(self.turn_image_blocks(ws, session_id).await);
        }
        let rendered = rendered;
        let since = live.as_ref().map(|s| s.started_at);
        let Some(ts) = live.as_ref().and_then(|s| s.ts.clone()) else {
            return self
                .post_turn(ws, thread, session_id, t, since, &rendered)
                .await;
        };
        if self.already_replied(ws, thread, t, since).await {
            return Ok(());
        }
        let meta = self.reply_meta(ws, session_id, t).await;
        let text = rendered.text.clone();
        let sent = live.as_ref().map(|s| s.sent.clone()).unwrap_or_default();
        let cards = card_chunks(&rendered.blocks);
        let (settle, _) = wire_chunks(&sent, &cards);
        if !settle.is_empty() {
            if let Err(e) = self.append_stream(ws, &thread.channel, &ts, settle).await {
                tracing::debug!(error = %e, %session_id, "slack: card not settled");
            }
        }
        let chunk = serde_json::json!({
            "type": "markdown_text",
            "text": clip(&text, MAX_MARKDOWN),
        });
        match self
            .stop_stream(ws, &thread.channel, &ts, chunk, &meta)
            .await
        {
            Ok(()) => {
                let images: Vec<&Value> = rendered
                    .blocks
                    .iter()
                    .filter(|b| b["type"] == "image")
                    .collect();
                if !images.is_empty() {
                    if let Err(e) = self
                        .update(ws, &thread.channel, &ts, &text, rendered.blocks, &meta)
                        .await
                    {
                        tracing::warn!(error = %e, %session_id, "slack: images not attached");
                    }
                }
                Ok(())
            }
            Err(e) if e.code() == "message_not_in_streaming_state" => {
                match self
                    .update(
                        ws,
                        &thread.channel,
                        &ts,
                        &text,
                        rendered.blocks.clone(),
                        &meta,
                    )
                    .await
                {
                    Ok(()) => Ok(()),
                    Err(e) => {
                        tracing::warn!(error = %e, %session_id, "slack: in-place finalize failed; posting reply");
                        self.post_turn(ws, thread, session_id, t, since, &rendered)
                            .await
                    }
                }
            }
            Err(e) => {
                tracing::warn!(error = %e, %session_id, "slack: stream finalize failed; posting reply");
                let posted = self
                    .post_turn(ws, thread, session_id, t, since, &rendered)
                    .await;
                self.close_stream(ws, &thread.channel, &ts).await;
                posted
            }
        }
    }

    async fn post_turn(
        &self,
        ws: &Workspace,
        thread: &Thread,
        session_id: &str,
        t: &crate::session::events::TurnCompleted,
        since: Option<chrono::DateTime<chrono::Utc>>,
        rendered: &Rendered,
    ) -> Result<(), Error> {
        if self.already_replied(ws, thread, t, since).await {
            return Ok(());
        }
        let meta = self.reply_meta(ws, session_id, t).await;
        self.post_blocks(ws, thread, &rendered.text, rendered.blocks.clone(), &meta)
            .await
    }

    async fn already_replied(
        &self,
        ws: &Workspace,
        thread: &Thread,
        t: &crate::session::events::TurnCompleted,
        since: Option<chrono::DateTime<chrono::Utc>>,
    ) -> bool {
        let oldest = since.map(|at| format!("{}.000000", at.timestamp()));
        let Ok(replies) = self.fetch_thread(ws, thread, oldest.as_deref()).await else {
            return false;
        };
        replies
            .iter()
            .any(|m| m.meta.as_ref().and_then(|r| r.turn_id.as_deref()) == Some(t.turn_id.as_str()))
    }

    async fn reply_meta(
        &self,
        ws: &Workspace,
        session_id: &str,
        t: &crate::session::events::TurnCompleted,
    ) -> ReplyMeta {
        let message_id = self
            .session_path(ws, session_id)
            .await
            .last()
            .filter(|m| matches!(m.role, Role::Assistant))
            .map(|m| m.id.clone());
        ReplyMeta {
            turn_id: Some(t.turn_id.clone()),
            message_id,
            session_id: Some(session_id.to_string()),
            ..Default::default()
        }
    }

    async fn post_interrupt(
        &self,
        ws: &Workspace,
        thread: &Thread,
        session_id: &str,
        turn_id: Option<&str>,
        p: &crate::session::events::SessionInterrupted,
        seq: u64,
    ) -> Result<(), Error> {
        if let Ok(replies) = self.fetch_thread(ws, thread, None).await {
            if replies.iter().any(|m| {
                m.meta.as_ref().and_then(|r| r.interrupt_id.as_deref())
                    == Some(p.interrupt_id.as_str())
            }) {
                return Ok(());
            }
        }
        let meta = ReplyMeta {
            interrupt_id: Some(p.interrupt_id.clone()),
            session_id: Some(session_id.to_string()),
            ..Default::default()
        };
        let key = turn_id.map(|t| StreamKey::new(ws, session_id, t));
        let (text, blocks) = match display_of(&p.payload) {
            Some(display) => {
                let mut message = display.message.clone();
                let mut options = Vec::new();
                match self.way_in(&ws.tenant_id, &p.payload).await {
                    Some(WayIn::Link { url, label }) => options.push(authorize_option(url, label)),
                    Some(WayIn::Command(words)) => message = format!("{message}\n\n{words}"),
                    None => {}
                }
                options.extend(prompt_options(&display, &p.interrupt_id));
                let blocks = render::prompt_blocks(&PromptView {
                    message: &message,
                    options: &options,
                    expires_at: display.expires_at.as_deref(),
                });
                (message, blocks)
            }
            None => {
                let text = format!("Paused: {}", p.reason);
                let block = section_block(&text);
                (text, vec![block])
            }
        };
        let open = match &key {
            Some(key) => match self.streams.take(key) {
                Some(open) => Some(open),
                None => self.recover(ws, key, seq).await,
            },
            None => None,
        };
        let posted = 'post: {
            if let Some(ts) = open.as_ref().and_then(|s| s.ts.as_deref()) {
                let chunk = serde_json::json!({ "type": "blocks", "blocks": blocks.clone() });
                match self
                    .stop_stream(ws, &thread.channel, ts, chunk, &meta)
                    .await
                {
                    Ok(()) => break 'post Ok(()),
                    Err(e) => {
                        tracing::warn!(error = %e, %session_id, "slack: prompt into stream failed; posting it")
                    }
                }
            }
            self.post_blocks(ws, thread, &text, blocks, &meta).await
        };
        if posted.is_ok() {
            if let (Some(store), Some(open)) = (self.store.as_deref(), &open) {
                if let Err(e) = store
                    .clear_turn(&ws.tenant_id, session_id, &open.turn_id)
                    .await
                {
                    tracing::warn!(%session_id, error = %e, "slack: stream state not cleared");
                }
            }
        }
        posted
    }

    async fn settle_prompt(
        &self,
        ws: &Workspace,
        thread: &Thread,
        p: &crate::session::events::InterruptResumed,
    ) -> Result<(), Error> {
        let session_id = format!("slack:{}:{}", thread.channel, thread.ts);
        let Ok(replies) = self.fetch_thread(ws, thread, None).await else {
            tracing::warn!(interrupt_id = %p.interrupt_id, "slack: fetch failed; prompt not settled");
            return Ok(());
        };
        let Some(msg) = replies.iter().find(|m| {
            m.meta.as_ref().and_then(|r| r.interrupt_id.as_deref()) == Some(p.interrupt_id.as_str())
        }) else {
            return Ok(());
        };
        let resolution = resolution_text(&p.payload);
        let text = format!("{}\n\n{resolution}", msg.text);
        let meta = ReplyMeta {
            interrupt_id: Some(p.interrupt_id.clone()),
            session_id: Some(session_id),
            ..Default::default()
        };
        let blocks = render::settled_prompt_blocks(&msg.blocks, &msg.text, &resolution);
        self.update(ws, &thread.channel, &msg.ts, &text, blocks, &meta)
            .await
    }
}

impl SlackBot {
    async fn track(
        &self,
        ws: &Workspace,
        thread: &Thread,
        event: &SessionEvent,
    ) -> Result<(), Error> {
        let turn_id = match &event.payload {
            EventPayload::TurnStarted(t) => Some(t.turn_id.clone()),
            _ => event.meta.turn_id.clone(),
        };
        match &event.payload {
            EventPayload::TurnStarted(t) => {
                let title = view::title_of(&self.session_path(ws, &event.session_id).await);
                let owner = event.meta.owner.as_ref();
                let recipient = owner.and_then(|o| o.metadata.get("slack_user")).cloned();
                let recipient_team = owner.and_then(|o| o.metadata.get("slack_team")).cloned();
                let mut stream = Stream {
                    tenant_id: event.tenant_id.clone(),
                    session_id: event.session_id.clone(),
                    turn_id: t.turn_id.clone(),
                    agent_id: ws.agent_id.clone(),
                    thread: thread.clone(),
                    started_at: event.occurred_at,
                    title,
                    turn: TurnActivity::new(&t.turn_id),
                    recipient,
                    recipient_team,
                    ts: None,
                    sent: HashMap::new(),
                    dead: false,
                    version: 0,
                };
                if let Some(store) = self.store.as_deref() {
                    let slot = store
                        .upsert_turn(
                            &event.tenant_id,
                            &event.session_id,
                            &t.turn_id,
                            event.seq,
                            event.occurred_at,
                        )
                        .await
                        .map_err(|e| Error::Retryable(e.to_string()))?;
                    stream.version = slot.version;
                    if slot.resumed {
                        if let Some(recovered) = self.recover(ws, &stream.key(), event.seq).await {
                            stream = recovered;
                        }
                    }
                }
                self.streams.track_new(stream);
                return Ok(());
            }
            EventPayload::LlmCallCompleted(_)
            | EventPayload::ToolCallRequested(_)
            | EventPayload::ToolCallCompleted(_)
            | EventPayload::ToolCallErrored(_)
            | EventPayload::SubagentRequested(_)
            | EventPayload::SubagentTurnCompleted(_)
            | EventPayload::SubagentErrored(_)
            | EventPayload::InterruptResumed(_) => {}
            EventPayload::SessionCancelled => {
                let open = self
                    .streams
                    .take_session(&event.tenant_id, &event.session_id);
                self.close_turns(ws, &event.session_id, thread, open).await;
                if let Some(store) = self.store.as_deref() {
                    if let Err(e) = store.clear(&event.tenant_id, &event.session_id).await {
                        tracing::warn!(session_id = %event.session_id, error = %e, "slack: stream state not cleared");
                    }
                }
                return Ok(());
            }
            _ => return Ok(()),
        }
        if let Some(turn_id) = turn_id {
            let key = StreamKey::new(ws, &event.session_id, &turn_id);
            self.streams.note(&key, event);
            self.streams.mark_dirty(key);
        }
        Ok(())
    }

    async fn stream_activity(&self, ctx: &ChannelContext) {
        loop {
            let Some(key) = self.streams.take_dirty() else {
                tokio::select! {
                    _ = ctx.shutdown.cancelled() => return,
                    _ = self.streams.notified() => {}
                }
                continue;
            };
            let Some(ws) = self.resolver.by_tenant(&key.tenant_id, &key.agent_id).await else {
                continue;
            };
            self.stream_turn(&ws, &key).await;
            tokio::select! {
                _ = ctx.shutdown.cancelled() => return,
                _ = tokio::time::sleep(ACTIVITY_INTERVAL) => {}
            }
        }
    }

    async fn stream_turn(&self, ws: &Workspace, key: &StreamKey) {
        let open = match self.streams.get(key) {
            Some(open) if open.dead => return,
            Some(open) => open,
            None => match self.recover(ws, key, u64::MAX).await {
                Some(recovered) => {
                    self.streams.insert(recovered.clone());
                    recovered
                }
                None => return,
            },
        };
        let thread = open.thread.clone();
        let view = view::live(&open.turn, &open.title);
        let (chunks, landed) = wire_chunks(&open.sent, &card_chunks(&view.blocks));
        if chunks.is_empty() {
            return;
        }

        let mut opened = false;
        let (ts, version) = match open.ts.clone() {
            Some(ts) => (ts, open.version),
            None => match self.recorded_ts(key).await {
                Some(recorded) => recorded,
                None => {
                    if self.stream_open_elsewhere(key).await {
                        self.streams.mark_dirty(key.clone());
                        return;
                    }
                    let ts = match self
                        .start_stream(
                            ws,
                            &thread,
                            open.recipient.as_deref(),
                            open.recipient_team.as_deref(),
                        )
                        .await
                    {
                        Ok(ts) => ts,
                        Err(e) => return self.kill_stream(key, e),
                    };
                    match self.persist_ts(&open, &ts).await {
                        TsPersist::Kept(version) => {
                            if self.streams.get(key).is_none() {
                                self.discard_stream(ws, &thread.channel, &ts).await;
                                return;
                            }
                            opened = true;
                            (ts, version)
                        }
                        TsPersist::Adopted(theirs, version) => {
                            self.discard_stream(ws, &thread.channel, &ts).await;
                            (theirs, version)
                        }
                        TsPersist::TurnOver => {
                            self.discard_stream(ws, &thread.channel, &ts).await;
                            self.streams.remove(key);
                            return;
                        }
                    }
                }
            },
        };
        if let Err(e) = self.append_stream(ws, &thread.channel, &ts, chunks).await {
            match opened {
                true => self.discard_stream(ws, &thread.channel, &ts).await,
                false => self.close_stream(ws, &thread.channel, &ts).await,
            }
            return self.kill_stream(key, e);
        }
        if opened {
            self.set_status(ws, &thread, "").await;
        }
        if !self.streams.commit(key, ts.clone(), version, landed) && opened {
            self.discard_stream(ws, &thread.channel, &ts).await;
        }
    }

    async fn recorded_ts(&self, key: &StreamKey) -> Option<(String, u64)> {
        let row = self
            .store
            .as_deref()?
            .load(&key.tenant_id, &key.session_id, &key.turn_id)
            .await
            .ok()??;
        Some((row.ts?, row.version))
    }

    async fn discard_stream(&self, ws: &Workspace, channel: &str, ts: &str) {
        self.close_stream(ws, channel, ts).await;
        self.delete_message(ws, channel, ts).await;
    }

    async fn stream_open_elsewhere(&self, key: &StreamKey) -> bool {
        if self.streams.open_elsewhere(key) {
            return true;
        }
        let Some(store) = self.store.as_deref() else {
            return false;
        };
        let other = store
            .open_other(&key.tenant_id, &key.session_id, &key.turn_id)
            .await;
        let Ok(Some(row)) = other else {
            return false;
        };
        !self.turn_is_over(key, &row).await
    }

    async fn turn_is_over(&self, key: &StreamKey, row: &super::state::StreamRow) -> bool {
        let Some(ctx) = self.ctx.get() else {
            return false;
        };
        let caller = Caller::System {
            tenant_id: key.tenant_id.clone(),
        };
        let events = ctx
            .read_session_events(&caller, &key.session_id, Some(Seq(row.start_seq)), None)
            .await;
        let Ok(events) = events else {
            return false;
        };
        events.iter().any(|e| match &e.payload {
            EventPayload::TurnCompleted(t) => t.turn_id == row.turn_id,
            EventPayload::SessionCancelled => true,
            _ => false,
        })
    }

    async fn close_turns(
        &self,
        ws: &Workspace,
        session_id: &str,
        thread: &Thread,
        open: Vec<Stream>,
    ) {
        for stream in open {
            let Some(ts) = stream.ts.clone() else {
                continue;
            };
            let blocks = view::live(&stream.turn, &stream.title).blocks;
            let rendered = render::render_cancelled(&blocks);
            let meta = ReplyMeta {
                session_id: Some(session_id.to_string()),
                ..Default::default()
            };
            let cards = card_chunks(&rendered.blocks);
            let (settle, _) = wire_chunks(&stream.sent, &cards);
            if !settle.is_empty() {
                if let Err(e) = self
                    .append_stream(ws, &stream.thread.channel, &ts, settle)
                    .await
                {
                    tracing::debug!(error = %e, %session_id, "slack: cancelled card not settled");
                }
            }
            let chunk = serde_json::json!({
                "type": "markdown_text",
                "text": rendered.text,
            });
            if let Err(e) = self
                .stop_stream(ws, &stream.thread.channel, &ts, chunk, &meta)
                .await
            {
                tracing::warn!(error = %e, %session_id, "slack: cancelled turn not settled");
                self.close_stream(ws, &stream.thread.channel, &ts).await;
            }
        }
        self.set_status(ws, thread, "").await;
    }

    fn kill_stream(&self, key: &StreamKey, error: Error) {
        tracing::warn!(
            error = %error,
            session_id = %key.session_id,
            turn_id = %key.turn_id,
            "slack: activity stream failed"
        );
        self.streams.kill(key);
    }

    async fn recover(&self, ws: &Workspace, key: &StreamKey, before: u64) -> Option<Stream> {
        let (store, ctx) = (self.store.as_deref()?, self.ctx.get()?);
        let row = store
            .load(&key.tenant_id, &key.session_id, &key.turn_id)
            .await
            .ok()
            .flatten()?;
        let caller = Caller::System {
            tenant_id: key.tenant_id.clone(),
        };
        let events = ctx
            .read_session_events(&caller, &key.session_id, Some(Seq(row.start_seq)), None)
            .await
            .ok()?;
        let mut turn = TurnActivity::new(&row.turn_id);
        for event in events.iter().filter(|e| e.seq < before) {
            turn.apply(event);
            match &event.payload {
                EventPayload::SessionCancelled => {
                    let _ = store
                        .clear_turn(&key.tenant_id, &key.session_id, &row.turn_id)
                        .await;
                    return None;
                }
                EventPayload::TurnCompleted(t) if t.turn_id == row.turn_id => {
                    let _ = store
                        .clear_turn(&key.tenant_id, &key.session_id, &row.turn_id)
                        .await;
                    return None;
                }
                EventPayload::SessionInterrupted(_) => return None,
                _ => {}
            }
        }
        let title = view::title_of(&self.session_path(ws, &key.session_id).await);
        let sent: HashMap<String, Value> = card_chunks(&view::live(&turn, &title).blocks)
            .into_iter()
            .collect();
        let owner = events.iter().rev().find_map(|e| e.meta.owner.as_ref());
        let thread = Thread::new(
            owner.and_then(|o| o.metadata.get("slack_channel"))?,
            owner.and_then(|o| o.metadata.get("slack_thread_ts"))?,
        );
        let recipient = owner.and_then(|o| o.metadata.get("slack_user")).cloned();
        let recipient_team = owner.and_then(|o| o.metadata.get("slack_team")).cloned();
        let mut stream = Stream {
            tenant_id: key.tenant_id.clone(),
            session_id: key.session_id.clone(),
            agent_id: key.agent_id.clone(),
            thread,
            turn_id: row.turn_id,
            started_at: row.started_at,
            title,
            turn,
            recipient,
            recipient_team,
            ts: row.ts,
            sent,
            dead: false,
            version: row.version,
        };
        if stream.ts.is_none() {
            self.heal_ts(ws, key, &mut stream).await;
        }
        Some(stream)
    }

    async fn heal_ts(&self, ws: &Workspace, key: &StreamKey, stream: &mut Stream) {
        let thread = stream.thread.clone();
        let oldest = format!("{}.000000", stream.started_at.timestamp());
        let Ok(resp) = self.fetch_replies_raw(ws, &thread, Some(&oldest)).await else {
            return;
        };
        let ours = self
            .identity(ws)
            .await
            .map(|i| i.ours.clone())
            .unwrap_or_default();
        let Some(ts) = unstamped_ours(&resp, &ours) else {
            return;
        };
        if let Some(store) = self.store.as_deref() {
            let recorded = store
                .set_ts(
                    &key.tenant_id,
                    &key.session_id,
                    &stream.turn_id,
                    &ts,
                    stream.version,
                )
                .await;
            if matches!(recorded, Ok(true)) {
                stream.version += 1;
            }
        }
        stream.ts = Some(ts);
    }

    async fn persist_ts(&self, open: &Stream, ts: &str) -> TsPersist {
        let Some(store) = self.store.as_deref() else {
            return TsPersist::Kept(open.version);
        };
        let mut expected = open.version;
        for _ in 0..2 {
            match store
                .set_ts(
                    &open.tenant_id,
                    &open.session_id,
                    &open.turn_id,
                    ts,
                    expected,
                )
                .await
            {
                Ok(true) => return TsPersist::Kept(expected + 1),
                Ok(false) => match store
                    .load(&open.tenant_id, &open.session_id, &open.turn_id)
                    .await
                {
                    Ok(Some(row)) => match row.ts {
                        Some(theirs) if theirs != ts => {
                            return TsPersist::Adopted(theirs, row.version)
                        }
                        Some(_) => return TsPersist::Kept(row.version),
                        None => expected = row.version,
                    },
                    Ok(None) => return TsPersist::TurnOver,
                    Err(e) => {
                        tracing::warn!(session_id = %open.session_id, error = %e, "slack: stream ts unrecorded");
                        return TsPersist::Kept(open.version);
                    }
                },
                Err(e) => {
                    tracing::warn!(session_id = %open.session_id, error = %e, "slack: stream ts unrecorded");
                    return TsPersist::Kept(open.version);
                }
            }
        }
        TsPersist::Kept(expected)
    }

    async fn delete_message(&self, ws: &Workspace, channel: &str, ts: &str) {
        let body = serde_json::json!({ "channel": channel, "ts": ts });
        if let Err(e) = self.api_call(ws, "chat.delete", body).await {
            tracing::warn!(error = %e, "slack: duplicate stream not deleted");
        }
    }
}

enum TsPersist {
    Kept(u64),
    Adopted(String, u64),
    TurnOver,
}

#[derive(Debug, thiserror::Error)]
pub(super) enum Error {
    #[error("{0}")]
    Retryable(String),
    #[error("{0}")]
    Terminal(String),
}

impl Error {
    fn retryable(e: impl std::fmt::Display) -> Self {
        Error::Retryable(e.to_string())
    }

    fn from_response(resp: &Value) -> Self {
        match resp["error"].as_str().unwrap_or("unknown_error") {
            code @ ("rate_limited" | "ratelimited" | "internal_error" | "service_unavailable") => {
                Error::Retryable(code.to_string())
            }
            code => Error::Terminal(code.to_string()),
        }
    }

    fn code(&self) -> &str {
        match self {
            Error::Retryable(e) | Error::Terminal(e) => e,
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::transport::slack::activity::tests::{said, tool_requested};

    #[test]
    fn only_a_direct_message_is_private() {
        use crate::protocol::Visibility;
        assert_eq!(super::audience_of("D0AAAAAAA"), Visibility::Private);
        assert_eq!(super::audience_of("C0AAAAAAA"), Visibility::Shared);
        assert_eq!(super::audience_of("G0AAAAAAA"), Visibility::Shared, "mpim");
        assert_eq!(super::audience_of(""), Visibility::Shared);
    }

    #[test]
    fn audio_and_video_are_accepted_within_their_caps() {
        assert_eq!(super::attachment_cap("audio/mpeg"), Some(MAX_AUDIO_BYTES));
        assert_eq!(super::attachment_cap("audio/ogg"), Some(MAX_AUDIO_BYTES));
        assert_eq!(super::attachment_cap("video/mp4"), Some(MAX_VIDEO_BYTES));
        assert_eq!(
            super::attachment_cap("video/quicktime"),
            Some(MAX_VIDEO_BYTES)
        );
        assert_eq!(
            super::attachment_cap("video/x-matroska"),
            Some(MAX_VIDEO_BYTES),
            "which containers a model reads is the adapter's call, not the bot's"
        );
        assert_eq!(super::attachment_cap("application/zip"), None);
    }

    use super::*;
    use crate::protocol::{ErrorCode, ErrorInfo};
    use crate::session::events::TurnCompleted;
    use std::sync::Mutex as StdMutex;

    const SESSION: &str = "slack:C1:1.0";

    #[derive(Clone, Default)]
    struct Calls(Arc<StdMutex<Vec<(String, Value)>>>);

    impl Calls {
        fn record(&self, method: &str, body: Value) {
            self.0.lock().unwrap().push((method.to_string(), body));
        }

        fn methods(&self) -> Vec<String> {
            self.0
                .lock()
                .unwrap()
                .iter()
                .map(|(m, _)| m.clone())
                .collect()
        }

        fn to(&self, method: &str) -> Vec<Value> {
            self.0
                .lock()
                .unwrap()
                .iter()
                .filter(|(m, _)| m == method)
                .map(|(_, b)| b.clone())
                .collect()
        }
    }

    async fn fake_slack_with(stops: Value, appends: Value) -> (String, Calls) {
        use axum::routing::post;
        use axum::{Json, Router};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let calls = Calls::default();
        let route = |app: Router, method: &'static str, answer: Value| {
            let calls = calls.clone();
            app.route(
                &format!("/{method}"),
                post(move |Json(body): Json<Value>| {
                    let (calls, answer) = (calls.clone(), answer.clone());
                    async move {
                        calls.record(method, body);
                        Json(answer)
                    }
                }),
            )
        };
        let ok = serde_json::json!({"ok": true, "ts": "1.1"});
        let app = Router::new();
        let app = route(app, "chat.postMessage", ok.clone());
        let app = route(app, "chat.update", ok.clone());
        let app = route(app, "chat.startStream", ok.clone());
        let app = route(app, "chat.appendStream", appends);
        let app = route(app, "chat.stopStream", stops);
        let app = route(
            app,
            "conversations.replies",
            serde_json::json!({"ok": true, "messages": []}),
        );
        let app = route(
            app,
            "auth.test",
            serde_json::json!({"ok": true, "user_id": "U0", "bot_id": "B0"}),
        );
        let app = route(
            app,
            "assistant.threads.setStatus",
            serde_json::json!({"ok": true}),
        );
        let app = {
            let calls = calls.clone();
            app.route(
                "/files.getUploadURLExternal",
                axum::routing::get(move |axum::extract::RawQuery(q): axum::extract::RawQuery| {
                    let calls = calls.clone();
                    async move {
                        calls.record(
                            "files.getUploadURLExternal",
                            serde_json::json!({"query": q}),
                        );
                        Json(serde_json::json!({
                            "ok": true,
                            "upload_url": format!("http://{addr}/upload"),
                            "file_id": "F123",
                        }))
                    }
                }),
            )
        };
        let app = {
            let calls = calls.clone();
            app.route(
                "/upload",
                post(move |body: axum::body::Bytes| {
                    let calls = calls.clone();
                    async move {
                        calls.record("upload", serde_json::json!({"len": body.len()}));
                        "OK"
                    }
                }),
            )
        };
        let app = route(
            app,
            "files.completeUploadExternal",
            serde_json::json!({"ok": true, "files": [{"id": "F123"}]}),
        );

        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        (format!("http://{addr}"), calls)
    }

    async fn fake_slack() -> (String, Calls) {
        fake_slack_with(
            serde_json::json!({"ok": true}),
            serde_json::json!({"ok": true}),
        )
        .await
    }

    struct OneWorkspace(Arc<Workspace>);

    #[async_trait::async_trait]
    impl WorkspaceResolver for OneWorkspace {
        async fn by_tenant(&self, _: &str, _: &str) -> Option<Arc<Workspace>> {
            Some(self.0.clone())
        }
    }

    fn failed_turn(error: ErrorInfo) -> TurnCompleted {
        TurnCompleted {
            turn_id: "turn-1".into(),
            data: Value::Null,
            turn_cost: Default::default(),
            turn_token_usage: Default::default(),
            turn_calls: Default::default(),
            turn_started_at: None,
            error: Some(error),
        }
    }

    mod way_in {
        use super::super::SlackBot;
        use crate::connectors::Requester;
        use crate::protocol::{ConnectorProtocol, Visibility};
        use crate::protocol::{Issuer, Subject};
        use crate::providers::sqlite::{SqliteAuthFlows, SqliteDb};
        use crate::runtime::session::interrupts::auth;
        use crate::transport::consent::WayIn;
        use crate::transport::consent::{CliConsent, Consent, DashboardConsent, EngineConsent};
        use crate::transport::mcp_auth::AuthorizeLinks;
        use std::sync::Arc;

        fn payload(requester: Requester) -> serde_json::Value {
            serde_json::json!({
                "message": "*gmail* is not authorized yet.",
                "authorize": auth::Authorize { connection: crate::connectors::registry::ConnectionPath::Mcp("gmail".into()), requester },
            })
        }

        fn person(audience: Visibility) -> Requester {
            Requester::new(Subject::new(Issuer::new("slack"), "T1:U1"), audience)
        }

        fn shown(way: Option<WayIn>) -> Option<String> {
            way.map(|way| match way {
                WayIn::Link { url, label } => format!("<{url}|{label}>"),
                WayIn::Command(words) => words,
            })
        }

        fn bot(consent: Arc<dyn Consent>) -> SlackBot {
            SlackBot::new(
                Arc::new(super::OneWorkspace(Arc::new(super::Workspace::new(
                    "xoxb-test".into(),
                    "t".into(),
                    "a".into(),
                )))),
                "http://127.0.0.1:1".into(),
                None,
                None,
            )
            .with_consent(consent)
        }

        fn engine(path: &std::path::Path) -> Arc<dyn Consent> {
            let db =
                SqliteDb::open(path.to_str().unwrap(), std::time::Duration::from_secs(5)).unwrap();
            let spec = crate::connectors::registry::ConnectionDecl {
                url: "https://mcp.gmail.test/mcp".into(),
                auth: None,
                header: None,
                credential: Some(crate::connectors::registry::CredentialScope::User),
                scopes: Vec::new(),
                client_id_env: None,
                client_secret_env: None,
                prefix_tools: true,
            }
            .at(
                crate::connectors::registry::ConnectionPath::Mcp("x".into()),
                ConnectorProtocol::Mcp,
            );
            Arc::new(EngineConsent(Arc::new(AuthorizeLinks::new(
                "https://agent.test",
                Arc::new(SqliteAuthFlows::new(db)),
                [(
                    crate::connectors::registry::ConnectionPath::Mcp("gmail".into()),
                    spec,
                )]
                .into(),
            ))))
        }

        fn temp() -> std::path::PathBuf {
            std::env::temp_dir().join(format!("core-way-in-{}.db", uuid::Uuid::now_v7()))
        }

        fn cleanup(path: &std::path::Path) {
            for suffix in ["", "-wal", "-shm"] {
                let _ = std::fs::remove_file(format!("{}{suffix}", path.display()));
            }
        }

        #[tokio::test]
        async fn an_engine_that_hosts_the_flow_mints_a_link() {
            let path = temp();
            let bot = bot(engine(&path));
            let how = shown(bot.way_in("t", &payload(person(Visibility::Private))).await)
                .expect("a private person gets a link");
            assert!(
                how.starts_with("<https://agent.test/mcp/authorize/"),
                "got {how}"
            );
            cleanup(&path);
        }

        #[tokio::test]
        async fn a_requester_the_call_would_refuse_gets_no_link() {
            let path = temp();
            let bot = bot(engine(&path));
            for refused in [Requester::machine(), person(Visibility::Shared)] {
                let how = shown(bot.way_in("t", &payload(refused)).await).unwrap();
                assert_eq!(how, "Run `subs auth mcp.gmail` to authorize it.");
            }
            cleanup(&path);
        }

        #[tokio::test]
        async fn a_hosted_project_points_at_the_dashboard_and_a_laptop_at_the_command() {
            let dashboard = bot(Arc::new(DashboardConsent(
                "https://app.test/overview".into(),
            )));
            assert_eq!(
                shown(
                    dashboard
                        .way_in("t", &payload(person(Visibility::Private)))
                        .await
                )
                .unwrap(),
                "<https://app.test/overview|Authorize it in the dashboard>"
            );
            assert_eq!(
                shown(
                    bot(Arc::new(CliConsent))
                        .way_in("t", &payload(person(Visibility::Private)))
                        .await
                )
                .unwrap(),
                "Run `subs auth mcp.gmail` to authorize it."
            );
        }

        #[tokio::test]
        async fn a_prompt_without_the_facts_is_left_alone() {
            let bot = bot(Arc::new(CliConsent));
            let bare = serde_json::json!({ "message": "*gmail* rejected its token." });
            assert_eq!(shown(bot.way_in("t", &bare).await), None);
        }

        #[tokio::test]
        async fn each_person_gets_their_own_link() {
            let path = temp();
            let bot = bot(engine(&path));

            let first =
                shown(bot.way_in("t", &payload(person(Visibility::Private))).await).unwrap();
            let second = shown(
                bot.way_in(
                    "t",
                    &payload(Requester::new(
                        Subject::new(Issuer::new("slack"), "T1:U2"),
                        Visibility::Private,
                    )),
                )
                .await,
            )
            .unwrap();
            assert_ne!(first, second);
            cleanup(&path);
        }
    }

    fn bot_for(api_base: String) -> (SlackBot, Arc<Workspace>) {
        let ws = Arc::new(Workspace::new("xoxb-test".into(), "t".into(), "a".into()));
        let bot = SlackBot::new(Arc::new(OneWorkspace(ws.clone())), api_base, None, None);
        (bot, ws)
    }

    async fn bot_with_blobs(
        api_base: String,
        dir: &std::path::Path,
    ) -> (SlackBot, Arc<Workspace>, Arc<dyn BlobStore>) {
        let ws = Arc::new(Workspace::new("xoxb-test".into(), "t".into(), "a".into()));
        let db = crate::providers::sqlite::SqliteDb::open(
            dir.join("test.db").to_str().unwrap(),
            std::time::Duration::from_secs(5),
        )
        .unwrap();
        let store = StreamStore::new(db.clone()).unwrap();
        let blobs: Arc<dyn BlobStore> =
            Arc::new(crate::providers::sqlite::SqliteBlobStore::new(db));
        let bot = SlackBot::new(
            Arc::new(OneWorkspace(ws.clone())),
            api_base,
            Some(store),
            Some(blobs.clone()),
        );
        (bot, ws, blobs)
    }

    #[tokio::test]
    async fn a_stored_image_uploads_once_and_embeds_the_slack_file() {
        let (api_base, calls) = fake_slack().await;
        let dir = tempfile::tempdir().unwrap();
        let (bot, ws, blobs) = bot_with_blobs(api_base, dir.path()).await;
        let r = blobs
            .put(NewBlob {
                tenant_id: "t".into(),
                mime: "image/png".into(),
                name: Some("chart.png".into()),
                bytes: vec![1, 2, 3],
            })
            .await
            .unwrap();

        let block = bot.slack_image_block(&ws, &r.uri()).await.unwrap().unwrap();
        assert_eq!(block["type"], "image");
        assert_eq!(block["slack_file"]["id"], "F123");
        assert_eq!(block["alt_text"], "chart.png");
        assert_eq!(calls.to("upload").len(), 1);
        assert_eq!(calls.to("files.completeUploadExternal").len(), 1);

        let again = bot.slack_image_block(&ws, &r.uri()).await.unwrap().unwrap();
        assert_eq!(again["slack_file"]["id"], "F123");
        assert_eq!(calls.to("upload").len(), 1);
    }

    #[tokio::test]
    async fn foreign_and_non_image_refs_do_not_embed() {
        let (api_base, calls) = fake_slack().await;
        let dir = tempfile::tempdir().unwrap();
        let (bot, ws, blobs) = bot_with_blobs(api_base, dir.path()).await;
        let foreign = blobs
            .put(NewBlob {
                tenant_id: "other".into(),
                mime: "image/png".into(),
                name: None,
                bytes: vec![1],
            })
            .await
            .unwrap();
        let pdf = blobs
            .put(NewBlob {
                tenant_id: "t".into(),
                mime: "application/pdf".into(),
                name: None,
                bytes: vec![1],
            })
            .await
            .unwrap();
        assert!(bot
            .slack_image_block(&ws, &foreign.uri())
            .await
            .unwrap()
            .is_none());
        assert!(bot
            .slack_image_block(&ws, &pdf.uri())
            .await
            .unwrap()
            .is_none());
        assert!(calls.to("upload").is_empty());

        let block = bot
            .slack_image_block(&ws, "https://example.com/a.png")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(block["image_url"], "https://example.com/a.png");
        assert!(calls.to("upload").is_empty());
    }

    #[test]
    fn stripping_images_leaves_a_note_and_only_fires_when_needed() {
        let blocks = vec![
            section_block("answer"),
            serde_json::json!({"type": "image", "slack_file": {"id": "F1"}, "alt_text": "x"}),
        ];
        let stripped = without_images(&blocks).unwrap();
        assert_eq!(stripped.len(), 2);
        assert_eq!(stripped[0]["type"], "section");
        assert_eq!(stripped[1]["type"], "context");
        assert!(without_images(&[section_block("answer")]).is_none());
    }

    fn thread() -> Thread {
        Thread {
            channel: "C1".into(),
            ts: "1.0".into(),
        }
    }

    #[test]
    fn a_refused_message_gets_the_open_prompt_back() {
        let text = SlackBot::still_waiting(&serde_json::json!({
            "message": "*sentry* needs to be authorized again.\n\n<https://app.test/x|Authorize>",
            "metadata": { "options": [{ "label": "Retry", "value": {} }] },
        }))
        .expect("a prompt to repeat");

        assert!(text.starts_with("Still waiting on this:"), "got {text}");
        assert!(text.contains("https://app.test/x"), "got {text}");
    }

    #[test]
    fn an_interrupt_with_nothing_to_say_is_not_repeated() {
        assert!(SlackBot::still_waiting(&serde_json::json!({ "metadata": {} })).is_none());
    }

    #[tokio::test]
    async fn a_failed_turn_posts_its_sentence() {
        let (api_base, calls) = fake_slack().await;
        let (bot, ws) = bot_for(api_base);
        let t = failed_turn(ErrorInfo::new(ErrorCode::BudgetExceeded, "budget spent"));
        bot.complete_turn(&ws, &thread(), SESSION, &t, chrono::Utc::now(), 0)
            .await
            .expect("posts");
        let body = calls
            .to("chat.postMessage")
            .first()
            .cloned()
            .expect("one message posted");
        assert_eq!(body["text"], "Error: budget spent");
        assert_eq!(body["blocks"].as_array().map(Vec::len), Some(1));
        assert_eq!(body["metadata"]["event_payload"]["turn_id"], "turn-1");
    }

    #[tokio::test]
    async fn a_finished_turn_settles_its_card_in_the_stream() {
        let (api_base, calls) = fake_slack().await;
        let (bot, ws) = bot_for(api_base);
        let mut live = stream("turn-1", Some("1.1"));
        live.turn.apply(&said("llm1", "First.", true, 2));
        live.sent = wire_chunks(&HashMap::new(), &desired(Some("First.")))
            .1
            .into_iter()
            .collect();
        live.turn.apply(&tool_requested("tc1", "a", "{}", 3));
        bot.streams.insert(live);

        let t = TurnCompleted {
            turn_id: "turn-1".into(),
            data: Value::String("the answer".into()),
            turn_cost: Default::default(),
            turn_token_usage: Default::default(),
            turn_calls: Default::default(),
            turn_started_at: None,
            error: None,
        };
        bot.complete_turn(&ws, &thread(), SESSION, &t, chrono::Utc::now(), 0)
            .await
            .expect("finishes");

        let appended = calls.to("chat.appendStream");
        assert_eq!(appended.len(), 1, "one settle append");
        let chunks = appended[0]["chunks"].as_array().unwrap();
        assert!(chunks.iter().all(|c| c["type"] == "task_update"));
        assert_eq!(chunks[0]["status"], "complete");
        assert_eq!(chunks[0]["details"], "\n• a `{}`", "the log's tail");
        let stopped = calls.to("chat.stopStream");
        assert_eq!(stopped.len(), 1);
        assert!(stopped[0]["chunks"][0]["text"]
            .as_str()
            .unwrap()
            .starts_with("the answer\n\n_"));
        assert!(calls.to("chat.update").is_empty(), "no flattening rewrite");
    }

    #[tokio::test]
    async fn a_tool_call_grows_the_card_of_its_turn() {
        let (api_base, _) = fake_slack().await;
        let (bot, ws) = bot_for(api_base);
        let started = EventPayload::TurnStarted(crate::runtime::session::events::TurnStarted {
            turn_id: "turn-1".into(),
            metadata: Default::default(),
        });
        bot.track(&ws, &thread(), &turn_event(started, Some("turn-1"), None))
            .await
            .expect("tracks");
        assert!(
            bot.streams.take_dirty().is_none(),
            "a bare start draws nothing"
        );

        let mut call = tool_requested("tc1", "search_web", r#"{"q":"x"}"#, 2);
        call.meta.turn_id = Some("turn-1".into());
        bot.track(&ws, &thread(), &call).await.expect("tracks");

        assert_eq!(bot.streams.take_dirty(), Some(key("turn-1")));
        let stream = bot.streams.get(&key("turn-1")).expect("the slot");
        let card = &view::live(&stream.turn, &stream.title).blocks[0];
        assert_eq!(card["status"], "in_progress");
        assert!(card["title"].as_str().unwrap().contains("search_web"));
    }

    #[tokio::test]
    async fn a_tracked_turn_takes_its_recipient_from_the_owner() {
        let (api_base, _) = fake_slack().await;
        let (bot, ws) = bot_for(api_base);

        bot.track(
            &ws,
            &thread(),
            &turn_event(
                EventPayload::TurnStarted(crate::runtime::session::events::TurnStarted {
                    turn_id: "turn-1".into(),
                    metadata: Default::default(),
                }),
                Some("turn-1"),
                None,
            ),
        )
        .await
        .expect("tracks");

        let stream = bot.streams.get(&key("turn-1")).expect("the slot");
        assert_eq!(stream.recipient.as_deref(), Some("U1"));
    }

    #[tokio::test]
    async fn an_opened_stream_names_its_recipient() {
        let (api_base, calls) = fake_slack().await;
        let (bot, ws) = bot_for(api_base);

        bot.start_stream(&ws, &thread(), Some("U1"), Some("T1"))
            .await
            .expect("opens");

        let body = calls
            .to("chat.startStream")
            .first()
            .cloned()
            .expect("one stream opened");
        assert_eq!(body["recipient_user_id"], "U1");
        assert_eq!(body["recipient_team_id"], "T1");
    }

    #[tokio::test]
    async fn a_failed_turn_writes_its_error_over_the_view() {
        let (api_base, calls) = fake_slack().await;
        let (bot, ws) = bot_for(api_base);
        let mut live = stream("turn-1", None);
        live.turn
            .apply(&tool_requested("tc1", "search_web", "{}", 2));
        bot.streams.insert(live);

        let t = failed_turn(ErrorInfo::new(ErrorCode::BudgetExceeded, "budget spent"));
        bot.complete_turn(&ws, &thread(), SESSION, &t, chrono::Utc::now(), 0)
            .await
            .expect("posts");

        let body = calls
            .to("chat.postMessage")
            .first()
            .cloned()
            .expect("one message posted");
        assert_eq!(body["text"], "Error: budget spent");
        let blocks = body["blocks"].as_array().unwrap();
        assert_eq!(blocks[0]["type"], "task_card", "the work is kept");
        assert_eq!(blocks[0]["status"], "error");
        assert_eq!(blocks[1]["text"]["text"], "Error: budget spent");
    }

    #[tokio::test]
    async fn a_stream_that_refuses_the_answer_is_closed_behind_it() {
        let (api_base, calls) = fake_slack_with(
            serde_json::json!({"ok": false, "error": "invalid_arguments"}),
            serde_json::json!({"ok": true}),
        )
        .await;
        let (bot, ws) = bot_for(api_base);
        bot.streams.insert(stream("turn-1", Some("1.1")));

        let t = failed_turn(ErrorInfo::new(ErrorCode::InvalidResponse, "bad decision"));
        bot.complete_turn(&ws, &thread(), SESSION, &t, chrono::Utc::now(), 0)
            .await
            .expect("the reply lands as a message");

        let posted = calls.to("chat.postMessage");
        assert_eq!(posted.len(), 1, "the answer is not lost");
        assert!(posted[0]["text"]
            .as_str()
            .is_some_and(|t| t.starts_with("Error: bad decision")));
        assert_eq!(calls.to("chat.stopStream").len(), 2);
    }

    #[tokio::test]
    async fn a_cancelled_session_settles_and_closes_every_message_it_held() {
        let (api_base, calls) = fake_slack().await;
        let (bot, ws) = bot_for(api_base);

        let open = vec![stream("turn-1", Some("1.1")), stream("turn-2", None)];
        bot.close_turns(&ws, SESSION, &thread(), open).await;

        let stopped = calls.to("chat.stopStream");
        assert_eq!(stopped.len(), 1, "only the turn that opened a message");
        assert_eq!(stopped[0]["ts"], "1.1");
        assert_eq!(
            stopped[0]["chunks"][0]["text"], "Cancelled.",
            "the stop says why, keeping the streamed card intact"
        );
        assert!(calls.to("chat.update").is_empty(), "no flattening rewrite");
        let status = calls.to("assistant.threads.setStatus");
        assert_eq!(status.len(), 1);
        assert_eq!(status[0]["status"], "");
    }

    #[test]
    fn a_turn_ending_hands_the_indicator_to_the_one_behind_it() {
        let streams = Streams::default();
        streams.insert(stream("turn-1", Some("1.1")));
        streams.insert(stream("turn-2", None));

        assert_eq!(
            streams.live_elsewhere("t", SESSION, Some("turn-1")),
            Some("turn-2".to_string()),
            "the queued turn is still working"
        );
        streams.remove(&key("turn-2"));
        assert_eq!(streams.live_elsewhere("t", SESSION, Some("turn-1")), None);
        streams.insert(stream("turn-2", None));
        streams.kill(&key("turn-2"));
        assert_eq!(streams.live_elsewhere("t", SESSION, Some("turn-1")), None);
        assert_eq!(streams.live_elsewhere("t", "slack:C9:9.0", None), None);
    }

    #[tokio::test]
    async fn a_turn_whose_slot_lost_its_message_takes_it_back() {
        let (api_base, calls) = fake_slack().await;
        let path = std::env::temp_dir().join(format!("core-slack-bot-{}.db", uuid::Uuid::now_v7()));
        let db = crate::providers::sqlite::SqliteDb::open(
            path.to_str().expect("a path"),
            std::time::Duration::from_secs(5),
        )
        .expect("a db");
        let store = StreamStore::new(db).expect("a store");
        let slot = store
            .upsert_turn("t", SESSION, "turn-1", 1, chrono::Utc::now())
            .await
            .expect("a row");
        store
            .set_ts("t", SESSION, "turn-1", "1.1", slot.version)
            .await
            .expect("the message is recorded");

        let ws = Arc::new(Workspace::new("xoxb-test".into(), "t".into(), "a".into()));
        let bot = SlackBot::new(
            Arc::new(OneWorkspace(ws.clone())),
            api_base,
            Some(store),
            None,
        );
        bot.streams.insert(stream("turn-1", None));

        bot.stream_turn(&ws, &key("turn-1")).await;

        assert!(
            calls.to("chat.startStream").is_empty(),
            "no second message opens"
        );
        let appended = calls.to("chat.appendStream");
        assert_eq!(appended.len(), 1);
        assert_eq!(appended[0]["ts"], "1.1", "the message the row names");
    }

    #[test]
    fn a_turn_that_starts_twice_keeps_the_message_it_opened() {
        let streams = Streams::default();
        streams.track_new(stream("turn-1", None));
        streams.commit(&key("turn-1"), "1.1".into(), 1, vec![]);

        streams.track_new(stream("turn-1", None));
        let slot = streams.get(&key("turn-1")).expect("the slot");
        assert_eq!(slot.ts.as_deref(), Some("1.1"));
    }

    #[tokio::test]
    async fn a_stream_that_refuses_a_card_is_closed() {
        let (api_base, calls) = fake_slack_with(
            serde_json::json!({"ok": true}),
            serde_json::json!({"ok": false, "error": "invalid_blocks"}),
        )
        .await;
        let (bot, ws) = bot_for(api_base);
        bot.streams.insert(stream("turn-1", Some("1.1")));

        bot.stream_turn(&ws, &key("turn-1")).await;

        assert_eq!(calls.to("chat.appendStream").len(), 1);
        let stopped = calls.to("chat.stopStream");
        assert_eq!(
            stopped.len(),
            1,
            "the message it cannot append to is stopped"
        );
        assert_eq!(stopped[0]["ts"], "1.1");
        assert!(bot.streams.get(&key("turn-1")).expect("the slot").dead);
    }

    fn desired(details: Option<&str>) -> Vec<(String, Value)> {
        card_chunks(&[render::turn_card(
            "turn-1",
            "Find x",
            "in_progress",
            details,
        )])
    }

    #[test]
    fn a_new_card_streams_its_details_in_pieces() {
        let long = "x".repeat(MAX_CHUNK + 100);
        let (wire, landed) = wire_chunks(&HashMap::new(), &desired(Some(&long)));
        assert_eq!(wire.len(), 2, "split to the cap");
        assert_eq!(wire[0]["type"], "task_update");
        assert_eq!(wire[0]["id"], "turn-1");
        assert_eq!(wire[0]["title"], "Find x");
        assert_eq!(wire[0]["status"], "in_progress");
        assert_eq!(
            wire[0]["details"].as_str().unwrap().chars().count(),
            MAX_CHUNK
        );
        assert_eq!(wire[1]["details"].as_str().unwrap().chars().count(), 100);
        assert_eq!(landed[0].1["details"], long.as_str(), "sent in full");
    }

    #[test]
    fn a_grown_log_streams_only_the_delta() {
        let (_, sent) = wire_chunks(&HashMap::new(), &desired(Some("First.\n• a {}")));
        let sent: HashMap<String, Value> = sent.into_iter().collect();
        let (wire, landed) = wire_chunks(&sent, &desired(Some("First.\n• a {}\nSecond.\n• b {}")));
        assert_eq!(wire.len(), 1);
        assert_eq!(wire[0]["details"], "\nSecond.\n• b {}");
        assert_eq!(landed[0].1["details"], "First.\n• a {}\nSecond.\n• b {}");
        let sent: HashMap<String, Value> = landed.into_iter().collect();
        let (wire, _) = wire_chunks(&sent, &desired(Some("First.\n• a {}\nSecond.\n• b {}")));
        assert!(wire.is_empty());
    }

    #[test]
    fn a_rewritten_log_sends_the_status_and_no_text() {
        let (_, sent) = wire_chunks(&HashMap::new(), &desired(Some("First.")));
        let sent: HashMap<String, Value> = sent.into_iter().collect();
        let (wire, _) = wire_chunks(&sent, &desired(Some("Rewritten.")));
        assert!(wire.is_empty(), "same head, unappendable text: nothing");
        let done = card_chunks(&[render::turn_card(
            "turn-1",
            "Find x",
            "complete",
            Some("Rewritten."),
        )]);
        let (wire, _) = wire_chunks(&sent, &done);
        assert_eq!(wire.len(), 1, "the status change goes out alone");
        assert_eq!(wire[0]["status"], "complete");
        assert!(wire[0].get("details").is_none());
    }

    #[test]
    fn a_commit_after_the_reply_took_the_slot_says_so() {
        let streams = Streams::default();
        streams.insert(stream("turn-1", None));
        assert!(streams.commit(&key("turn-1"), "1.1".into(), 1, vec![]));

        streams.take(&key("turn-1"));
        assert!(!streams.commit(&key("turn-1"), "1.2".into(), 2, vec![]));
    }

    #[tokio::test]
    async fn the_indicator_goes_out_after_the_reply_not_before_it() {
        let (api_base, calls) = fake_slack().await;
        let (bot, _ws) = bot_for(api_base);

        let done = EventPayload::TurnCompleted(TurnCompleted {
            turn_id: "turn-1".into(),
            data: Value::Null,
            turn_cost: Default::default(),
            turn_token_usage: Default::default(),
            turn_calls: Default::default(),
            turn_started_at: None,
            error: None,
        });
        bot.apply(turn_event(done, None, None))
            .await
            .expect("applies");

        let status = calls.to("assistant.threads.setStatus");
        assert_eq!(status.len(), 1);
        assert_eq!(status[0]["status"], "");
        assert_eq!(
            calls.methods().last().map(String::as_str),
            Some("assistant.threads.setStatus"),
            "the thread's last word is that it stopped working"
        );
    }

    #[tokio::test]
    async fn a_card_lands_with_no_status_beside_it() {
        let (api_base, calls) = fake_slack().await;
        let (bot, ws) = bot_for(api_base);
        bot.streams.insert(stream("turn-1", Some("1.1")));

        bot.stream_turn(&ws, &key("turn-1")).await;

        assert_eq!(calls.to("chat.appendStream").len(), 1);
        assert!(calls.to("assistant.threads.setStatus").is_empty());
    }

    #[tokio::test]
    async fn a_turn_starting_lights_the_status() {
        let (api_base, calls) = fake_slack().await;
        let (bot, _ws) = bot_for(api_base);

        let started = EventPayload::TurnStarted(crate::session::events::TurnStarted {
            turn_id: "turn-1".into(),
            metadata: Default::default(),
        });
        bot.apply(turn_event(started, Some("turn-1"), None))
            .await
            .expect("applies");

        let status = calls.to("assistant.threads.setStatus");
        assert_eq!(status.len(), 1);
        assert_eq!(status[0]["status"], WORKING_STATUS);
    }

    #[tokio::test]
    async fn a_running_turn_sets_no_status() {
        let (api_base, calls) = fake_slack().await;
        let (bot, _ws) = bot_for(api_base);

        let resumed = EventPayload::InterruptResumed(crate::session::events::InterruptResumed {
            interrupt_id: "i1".into(),
            payload: Value::Null,
        });
        bot.apply(turn_event(resumed, Some("turn-1"), None))
            .await
            .expect("applies");

        bot.streams.insert(stream("turn-1", Some("1.1")));
        let work = EventPayload::DecisionCompleted(crate::session::events::DecisionCompleted {
            id: "d1".into(),
        });
        bot.apply(turn_event(work, Some("turn-1"), None))
            .await
            .expect("applies");

        assert!(calls.to("assistant.threads.setStatus").is_empty());
    }

    #[tokio::test]
    async fn the_events_after_a_turn_do_not_light_it_again() {
        let (api_base, calls) = fake_slack().await;
        let (bot, _ws) = bot_for(api_base);
        bot.streams.insert(stream("turn-1", None));

        let done = EventPayload::TurnCompleted(TurnCompleted {
            turn_id: "turn-1".into(),
            data: Value::Null,
            turn_cost: Default::default(),
            turn_token_usage: Default::default(),
            turn_calls: Default::default(),
            turn_started_at: None,
            error: None,
        });
        bot.apply(turn_event(done, None, None))
            .await
            .expect("applies");
        let after = EventPayload::SessionDone(crate::session::events::SessionDone {});
        bot.apply(turn_event(after, Some("turn-1"), None))
            .await
            .expect("applies");

        let status = calls.to("assistant.threads.setStatus");
        assert_eq!(status.len(), 1, "the reply put it out, and it stayed out");
        assert_eq!(status[0]["status"], "");
    }

    #[tokio::test]
    async fn a_parked_or_turnless_event_leaves_the_indicator_out() {
        let (api_base, calls) = fake_slack().await;
        let (bot, _ws) = bot_for(api_base);

        let event = || {
            EventPayload::DecisionCompleted(crate::session::events::DecisionCompleted {
                id: "d1".into(),
            })
        };
        bot.apply(turn_event(event(), Some("turn-1"), Some("i1")))
            .await
            .expect("applies");
        bot.apply(turn_event(event(), None, None))
            .await
            .expect("applies");

        assert!(calls.to("assistant.threads.setStatus").is_empty());
    }

    fn turn_event(
        payload: EventPayload,
        turn_id: Option<&str>,
        parked: Option<&str>,
    ) -> SessionEvent {
        let status = match parked {
            Some(interrupt_id) => crate::session::state::SessionStatus::Interrupted {
                interrupt_id: interrupt_id.into(),
                origin: crate::protocol::InterruptOrigin::Machine,
                reason: "confirmation".into(),
            },
            None => crate::session::state::SessionStatus::Idle,
        };
        let meta = crate::session::state::EventMeta {
            status,
            wake_at: None,
            owner: Some(SessionOwner {
                tenant_id: "t".into(),
                requester: Requester::machine(),
                metadata: [
                    ("slack_channel".to_string(), "C1".to_string()),
                    ("slack_thread_ts".to_string(), "1.0".to_string()),
                    ("slack_user".to_string(), "U1".to_string()),
                ]
                .into(),
            }),
            agent_id: Some("a".into()),
            ancestry: Vec::new(),
            turn_id: turn_id.map(str::to_string),
            cost: Default::default(),
            subagent_cost: Default::default(),
            head_id: None,
            calls: Vec::new(),
            decisions: Vec::new(),
        };
        SessionEvent {
            id: uuid::Uuid::nil(),
            tenant_id: "t".into(),
            session_id: SESSION.into(),
            seq: 1,
            span: crate::span::SpanContext::root(),
            occurred_at: chrono::Utc::now(),
            payload,
            meta,
            start_time: chrono::Utc::now(),
            end_time: chrono::Utc::now(),
            command_id: None,
        }
    }

    fn stream(turn_id: &str, ts: Option<&str>) -> Stream {
        Stream {
            tenant_id: "t".into(),
            session_id: SESSION.into(),
            turn_id: turn_id.into(),
            agent_id: "a".into(),
            thread: thread(),
            started_at: chrono::Utc::now(),
            title: "Find x".into(),
            turn: TurnActivity::new(turn_id),
            recipient: None,
            recipient_team: None,
            ts: ts.map(str::to_string),
            sent: HashMap::new(),
            dead: false,
            version: 0,
        }
    }

    fn key(turn_id: &str) -> StreamKey {
        StreamKey::parts("t", SESSION, turn_id, "a")
    }

    #[test]
    fn delivery_is_gated_on_the_owners_address_not_the_id() {
        let meta = |metadata: &[(&str, &str)]| crate::session::state::EventMeta {
            status: crate::session::state::SessionStatus::Idle,
            wake_at: None,
            owner: Some(SessionOwner {
                tenant_id: "t".into(),
                requester: Requester::machine(),
                metadata: metadata
                    .iter()
                    .map(|(k, v)| (k.to_string(), v.to_string()))
                    .collect(),
            }),
            agent_id: None,
            ancestry: Vec::new(),
            turn_id: None,
            cost: Default::default(),
            subagent_cost: Default::default(),
            head_id: None,
            calls: Vec::new(),
            decisions: Vec::new(),
        };
        let addressed = meta(&[("slack_channel", "C1"), ("slack_thread_ts", "1.0")]);
        let thread = owner_thread(&addressed).expect("addressed at a thread");
        assert_eq!(thread.channel, "C1");
        assert_eq!(thread.ts, "1.0");
        assert!(owner_thread(&meta(&[("slack_channel", "C1")])).is_none());
        assert!(owner_thread(&meta(&[])).is_none());
    }

    #[test]
    fn two_turns_of_one_session_hold_their_own_slots() {
        let streams = Streams::default();
        streams.insert(stream("turn-1", Some("100.1")));
        streams.insert(stream("turn-2", None));

        streams.kill(&key("turn-1"));
        assert!(streams.get(&key("turn-1")).unwrap().dead);
        assert!(!streams.get(&key("turn-2")).unwrap().dead);

        streams.commit(&key("turn-1"), "9.9".into(), 7, vec![]);
        assert_eq!(streams.get(&key("turn-2")).unwrap().ts, None);

        streams.remove(&key("turn-1"));
        assert!(streams.get(&key("turn-2")).is_some());
    }

    #[test]
    fn an_open_message_is_visible_to_the_turn_behind_it() {
        let streams = Streams::default();
        streams.insert(stream("turn-2", None));
        assert!(!streams.open_elsewhere(&key("turn-2")));

        streams.insert(stream("turn-1", Some("100.1")));
        assert!(streams.open_elsewhere(&key("turn-2")));
        assert!(!streams.open_elsewhere(&key("turn-1")));
        streams.insert(stream("turn-1", None));
        assert!(!streams.open_elsewhere(&key("turn-2")));
        streams.insert(stream("turn-1", Some("100.1")));
        streams.kill(&key("turn-1"));
        assert!(!streams.open_elsewhere(&key("turn-2")));

        let taken = streams.take_session("t", SESSION);
        assert_eq!(taken.len(), 2);
        assert!(taken.iter().any(|s| s.ts.as_deref() == Some("100.1")));
        assert!(streams.get(&key("turn-1")).is_none());
        assert!(streams.get(&key("turn-2")).is_none());
    }
}
