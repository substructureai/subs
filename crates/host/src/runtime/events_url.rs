use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, TimeDelta, Utc};
use hmac::{Hmac, Mac};
use reqwest::header::{CONTENT_TYPE, RETRY_AFTER};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use substructure_session::delivery::{AtLimit, Start, Subscription};
use substructure_session::Session;

use crate::runtime::secret::{get_or_mint, SecretPath, SecretStore};
use crate::runtime::session::events::EventPayload;
use crate::runtime::session::SessionEvent;
use crate::runtime::sessions::{DeliveryError, Reader, Sessions};
use crate::runtime::worker::AgentDirectory;

pub const READER: &str = "events";
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(10);

type HmacSha256 = Hmac<Sha256>;

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct EventsBlock {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timeout_secs: Option<std::num::NonZeroU64>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct LiveChunk {
    pub seq: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub finish_reason: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Live {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub turn_id: Option<String>,
    pub call_id: String,
    pub attempt: u32,
    pub chunks: Vec<LiveChunk>,
    pub done: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize)]
pub struct About {
    pub session_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent_id: Option<String>,
    #[serde(skip_serializing_if = "HashMap::is_empty")]
    pub metadata: HashMap<String, String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub ancestry: Vec<String>,
}

impl About {
    fn of(session: &Session) -> Self {
        let state = session.state();
        Self {
            session_id: session.id().to_string(),
            agent_id: state.agent_id.clone(),
            metadata: state
                .owner
                .as_ref()
                .map(|owner| owner.metadata.clone())
                .unwrap_or_default(),
            ancestry: state.ancestry.clone(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    pub id: String,
    pub tenant_id: String,
    pub url: String,
    pub timeout: Duration,
    pub secret: String,
    pub about: About,
}

#[derive(Serialize)]
struct Event<'a> {
    seq: u64,
    occurred_at: DateTime<Utc>,
    #[serde(skip_serializing_if = "Option::is_none")]
    turn_id: Option<&'a str>,
    payload: &'a EventPayload,
}

impl<'a> From<&'a SessionEvent> for Event<'a> {
    fn from(event: &'a SessionEvent) -> Self {
        Self {
            seq: event.seq,
            occurred_at: event.occurred_at,
            turn_id: event.meta.turn_id.as_deref(),
            payload: &event.payload,
        }
    }
}

#[derive(Serialize)]
#[serde(rename_all = "lowercase")]
enum Kind {
    Events,
    Live,
}

#[derive(Serialize)]
struct Post<'a> {
    #[serde(rename = "type")]
    kind: Kind,
    #[serde(flatten)]
    about: &'a About,
    #[serde(skip_serializing_if = "Option::is_none")]
    events: Option<Vec<Event<'a>>>,
    #[serde(flatten, skip_serializing_if = "Option::is_none")]
    live: Option<&'a Live>,
}

pub struct EventsReader {
    agents: Arc<dyn AgentDirectory>,
    secrets: Arc<dyn SecretStore>,
    http: Client,
}

impl EventsReader {
    pub fn new(agents: Arc<dyn AgentDirectory>, secrets: Arc<dyn SecretStore>) -> Self {
        Self {
            agents,
            secrets,
            http: Client::new(),
        }
    }

    pub async fn target(&self, session: &Session) -> Result<Option<Target>, DeliveryError> {
        let Some(id) = session.state().events.clone() else {
            return Ok(None);
        };
        let tenant_id = session.tenant_id().to_string();
        let block = self
            .agents
            .events(&tenant_id, &id)
            .await
            .unwrap_or_default();
        let timeout = block
            .timeout_secs
            .map_or(DEFAULT_TIMEOUT, |secs| Duration::from_secs(secs.get()));
        let Some(url) = block.url else {
            tracing::debug!(events = %id, session_id = %session.id(), "the session's [events] block is gone");
            return Ok(None);
        };
        let path = SecretPath::Events(id.clone());
        let secret = get_or_mint(&*self.secrets, &tenant_id, &path.secret_ref())
            .await
            .map_err(|e| {
                DeliveryError::retry(None, format!("reading the secret for {path} failed: {e}"))
            })?;
        Ok(Some(Target {
            id,
            tenant_id,
            url,
            timeout,
            secret,
            about: About::of(session),
        }))
    }

    pub async fn live(&self, target: &Target, live: &Live) -> Result<(), DeliveryError> {
        self.post(
            target,
            &Post {
                kind: Kind::Live,
                about: &target.about,
                events: None,
                live: Some(live),
            },
        )
        .await
    }

    async fn post(&self, target: &Target, post: &Post<'_>) -> Result<(), DeliveryError> {
        let body = serde_json::to_vec(post)
            .map_err(|e| DeliveryError::permanent(None, format!("the body did not encode: {e}")))?;
        let path = SecretPath::Events(target.id.clone());
        let mut mac = HmacSha256::new_from_slice(target.secret.as_bytes())
            .map_err(|e| DeliveryError::retry(None, format!("signing for {path} failed: {e}")))?;
        mac.update(&body);
        let signature = hex::encode(mac.finalize().into_bytes());
        let sent = self
            .http
            .post(&target.url)
            .header(CONTENT_TYPE, "application/json")
            .header("X-Substructure-Signature", format!("sha256={signature}"))
            .timeout(target.timeout)
            .body(body)
            .send()
            .await
            .map_err(|e| DeliveryError::retry(None, format!("{path} did not answer: {e}")))?;
        let status = sent.status();
        if status.is_success() {
            return Ok(());
        }
        let retry_after = sent
            .headers()
            .get(RETRY_AFTER)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.trim().parse::<i64>().ok())
            .and_then(TimeDelta::try_seconds);
        Err(DeliveryError {
            retry_after,
            ..DeliveryError::retry(None, format!("{path} answered {status}"))
        })
    }
}

#[async_trait]
impl Reader for EventsReader {
    fn subscription(&self) -> Subscription {
        Subscription::new(READER, Start::Beginning, AtLimit::Skip)
    }

    async fn deliver(
        &self,
        _sessions: &Sessions,
        session: &Session,
        events: &[SessionEvent],
    ) -> Result<(), DeliveryError> {
        let Some(target) = self.target(session).await? else {
            return Ok(());
        };
        self.post(
            &target,
            &Post {
                kind: Kind::Events,
                about: &target.about,
                events: Some(events.iter().map(Event::from).collect()),
                live: None,
            },
        )
        .await
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::sync::Mutex;

    use axum::http::{HeaderMap, StatusCode};
    use axum::response::IntoResponse;
    use axum::routing::post;
    use axum::Router;

    use super::*;
    use crate::event_store::{EventFilter, StoreError};
    use crate::protocol::{Issuer, Requester, RetryPolicy, SessionOwner, Subject};
    use crate::runtime::caller::Caller;
    use crate::runtime::llm::LlmBlocks;
    use crate::runtime::secret::SecretRef;
    use crate::runtime::sessions::ExecuteInput;
    use crate::runtime::span::SpanContext;
    use crate::runtime::worker::StaticAgentDirectory;
    use substructure_session::command::CommandPayload;

    #[derive(Default)]
    pub(crate) struct Secrets(Mutex<HashMap<String, Vec<u8>>>);

    #[async_trait]
    impl SecretStore for Secrets {
        async fn put(&self, _t: &str, r: &SecretRef, value: &[u8]) -> Result<(), StoreError> {
            self.0.lock().unwrap().insert(r.to_string(), value.to_vec());
            Ok(())
        }
        async fn get(&self, _t: &str, r: &SecretRef) -> Result<Option<Vec<u8>>, StoreError> {
            Ok(self.0.lock().unwrap().get(r.as_str()).cloned())
        }
        async fn delete(&self, _t: &str, r: &SecretRef) -> Result<bool, StoreError> {
            Ok(self.0.lock().unwrap().remove(r.as_str()).is_some())
        }
    }

    type Posts = Arc<Mutex<Vec<(String, String)>>>;

    async fn app(status: StatusCode) -> (String, Posts) {
        let posts: Posts = Arc::default();
        let seen = posts.clone();
        let router = Router::new().route(
            "/events",
            post(move |headers: HeaderMap, body: String| {
                let seen = seen.clone();
                async move {
                    let signature = headers
                        .get("X-Substructure-Signature")
                        .and_then(|v| v.to_str().ok())
                        .unwrap_or_default()
                        .to_string();
                    seen.lock().unwrap().push((signature, body));
                    (status, [("Retry-After", "7")], "").into_response()
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, router).await;
        });
        (format!("http://{addr}/events"), posts)
    }

    fn reader(url: &str, secrets: Arc<Secrets>) -> EventsReader {
        reader_with(
            EventsBlock {
                url: Some(url.to_string()),
                timeout_secs: None,
            },
            secrets,
        )
    }

    fn reader_with(block: EventsBlock, secrets: Arc<Secrets>) -> EventsReader {
        let agents =
            StaticAgentDirectory::new("t".into(), Default::default(), LlmBlocks::default())
                .with_events(BTreeMap::from([("app".to_string(), block)]));
        EventsReader::new(Arc::new(agents), secrets)
    }

    fn signed(secret: &[u8], body: &str) -> String {
        let mut mac = HmacSha256::new_from_slice(secret).unwrap();
        mac.update(body.as_bytes());
        format!("sha256={}", hex::encode(mac.finalize().into_bytes()))
    }

    fn create(session_id: &str, events: Option<&str>) -> ExecuteInput {
        ExecuteInput {
            id: None,
            session_id: session_id.into(),
            caller: Caller::System {
                tenant_id: "t".into(),
            },
            command: CommandPayload::CreateSession {
                agent_id: "agent".into(),
                owner: SessionOwner {
                    tenant_id: "t".into(),
                    requester: Requester::new(
                        Subject::new(Issuer::app(), "u".to_string()),
                        Default::default(),
                    ),
                    metadata: HashMap::from([("room".to_string(), "r1".to_string())]),
                },
                ancestry: vec![],
                worker_retry: RetryPolicy::no_retry(),
                agent: None,
                worker: None,
                events: events.map(str::to_string),
                state: None,
            },
            span: SpanContext::root(),
        }
    }

    fn live() -> Live {
        Live {
            turn_id: Some("t1".into()),
            call_id: "llm-1".into(),
            attempt: 0,
            chunks: vec![LiveChunk {
                seq: 1,
                text: Some("Hi".into()),
                reasoning: None,
                finish_reason: None,
            }],
            done: false,
        }
    }

    async fn session(sessions: &Sessions, id: &str, events: Option<&str>) -> Session {
        sessions.execute(create(id, events)).await.unwrap();
        sessions.load("t", id).await.unwrap().unwrap()
    }

    async fn logged(sessions: &Sessions, id: &str) -> Vec<SessionEvent> {
        sessions
            .query_events(&EventFilter {
                tenant_id: Some("t".into()),
                session_id: Some(id.into()),
                ..Default::default()
            })
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn a_session_sends_signed_events_and_live_text_to_its_events_url() {
        let (url, posts) = app(StatusCode::OK).await;
        let secrets = Arc::new(Secrets::default());
        let events = reader(&url, secrets.clone());
        let disk = crate::runtime::sessions::testing::disk();
        let sessions = disk.sessions();
        let s1 = session(&sessions, "s1", Some("app")).await;
        let logged = logged(&sessions, "s1").await;

        events.deliver(&sessions, &s1, &logged).await.unwrap();
        let target = events.target(&s1).await.unwrap().expect("s1 sends to app");
        events.live(&target, &live()).await.unwrap();

        let posts = posts.lock().unwrap().clone();
        assert_eq!(posts.len(), 2);
        let secret = secrets
            .0
            .lock()
            .unwrap()
            .get("events.app")
            .cloned()
            .unwrap();
        for (signature, body) in &posts {
            assert_eq!(signature, &signed(&secret, body));
        }
        let bodies: Vec<serde_json::Value> = posts
            .iter()
            .map(|(_, body)| serde_json::from_str(body).unwrap())
            .collect();
        assert_eq!(bodies[0]["type"], "events");
        assert_eq!(bodies[0]["session_id"], "s1");
        assert_eq!(bodies[0]["agent_id"], "agent");
        assert_eq!(bodies[0]["metadata"]["room"], "r1");
        assert!(bodies[0]["events"][0].get("meta").is_none());
        assert!(bodies[0]["events"][0]["payload"]["type"].is_string());
        let seqs: Vec<u64> = bodies[0]["events"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| e["seq"].as_u64().unwrap())
            .collect();
        assert_eq!(seqs, logged.iter().map(|e| e.seq).collect::<Vec<_>>());
        assert_eq!(bodies[1]["type"], "live");
        assert_eq!(bodies[1]["session_id"], "s1");
        assert_eq!(bodies[1]["metadata"]["room"], "r1");
        assert_eq!(bodies[1]["chunks"][0]["text"], "Hi");
    }

    #[tokio::test]
    async fn an_events_url_that_never_answers_fails_after_its_own_timeout() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/events", listener.local_addr().unwrap());
        tokio::spawn(async move {
            let mut held = Vec::new();
            while let Ok((socket, _)) = listener.accept().await {
                held.push(socket);
            }
        });
        let events = reader_with(
            EventsBlock {
                url: Some(url),
                timeout_secs: std::num::NonZeroU64::new(1),
            },
            Arc::default(),
        );
        let disk = crate::runtime::sessions::testing::disk();
        let sessions = disk.sessions();
        let s1 = session(&sessions, "s1", Some("app")).await;
        let logged = logged(&sessions, "s1").await;

        let target = events.target(&s1).await.unwrap().expect("s1 sends to app");
        assert_eq!(target.timeout, Duration::from_secs(1));
        let started = std::time::Instant::now();
        let failed = events
            .deliver(&sessions, &s1, &logged)
            .await
            .expect_err("no answer");
        assert!(!failed.permanent);
        assert!(started.elapsed() < DEFAULT_TIMEOUT);
    }

    #[tokio::test]
    async fn a_session_without_an_events_url_sends_nothing() {
        let (url, posts) = app(StatusCode::OK).await;
        let events = reader(&url, Arc::default());
        let disk = crate::runtime::sessions::testing::disk();
        let sessions = disk.sessions();
        let quiet = session(&sessions, "quiet", None).await;
        let gone = session(&sessions, "gone", Some("removed")).await;

        for s in [&quiet, &gone] {
            let logged = logged(&sessions, s.id()).await;
            events.deliver(&sessions, s, &logged).await.unwrap();
            assert!(events.target(s).await.unwrap().is_none());
        }
        assert!(posts.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn an_events_url_that_answers_an_error_is_tried_again_after_its_retry_after() {
        let (url, _posts) = app(StatusCode::SERVICE_UNAVAILABLE).await;
        let events = reader(&url, Arc::default());
        let disk = crate::runtime::sessions::testing::disk();
        let sessions = disk.sessions();
        let s1 = session(&sessions, "s1", Some("app")).await;
        let target = events.target(&s1).await.unwrap().expect("s1 sends to app");

        let failed = events.live(&target, &live()).await.unwrap_err();

        assert!(!failed.permanent);
        assert_eq!(failed.retry_after, TimeDelta::try_seconds(7));
    }
}
