use std::collections::{BTreeMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use substructure_host::event_store::Seq;
use substructure_host::llm::{
    CallContext, InMemoryTokenDeltaTransport, LlmBlock, LlmBlocks, LlmCallError, LlmCallable,
    LlmProviderRegistry, LlmProviderTrait, LlmTask,
};
use substructure_host::protocol::{
    AgentConfig, ClientInput, Content, DraftMessage, Issuer, LlmResponse, PromptRequest, QueueMode,
    Requester, RetryPolicy, Role, SessionOwner, Subject,
};
use substructure_host::providers::sqlite::{
    SqliteAlarms, SqliteDb, SqliteQueue, SqliteSecretStore, SqliteSessionIndexStore,
    SqliteSessionStores,
};
use substructure_host::retry::WorkerRetryResolver;
use substructure_host::runtime::connector::ConnectorTask;
use substructure_host::runtime::events_url::EventsBlock;
use substructure_host::runtime::queue;
use substructure_host::session::events::EventPayload;
use substructure_host::session::subscriptions::{SessionSubscriptionSpec, SubscriptionScope};
use substructure_host::span::SpanContext;
use substructure_host::subagent::SubagentTask;
use substructure_host::transport::push::PushAdapter;
use substructure_host::worker::{AgentEntry, Hosting, StaticAgentDirectory, WorkerBlock};
use substructure_host::{
    Caller, ClientInputOutput, HandleClientInput, InterruptSessionInput, Runtime, RuntimeConfig,
    RuntimeDeps,
};

const TENANT: &str = "default";

struct Model;

#[async_trait]
impl LlmCallable for Model {
    async fn call(
        &self,
        request: &PromptRequest,
        _ctx: &CallContext<'_>,
    ) -> Result<LlmResponse, LlmCallError> {
        Ok(LlmResponse {
            model: request.model.clone(),
            content: Some("done".to_string()),
            tool_calls: vec![],
            finish_reason: Some("stop".to_string()),
            usage: None,
            cost: None,
            images: vec![],
            reasoning: None,
        })
    }
}

struct Provider;

#[async_trait]
impl LlmProviderTrait for Provider {
    async fn resolve(&self, _owner: &SessionOwner) -> Result<Arc<dyn LlmCallable>, String> {
        Ok(Arc::new(Model))
    }
}

struct Retry(RetryPolicy);

#[async_trait]
impl WorkerRetryResolver for Retry {
    async fn resolve(&self, _tenant_id: &str) -> RetryPolicy {
        self.0.clone()
    }
}

#[derive(Clone, Copy)]
enum Step {
    Fail(u16),
    Slow(Duration),
}

#[derive(Default)]
struct Script {
    steps: VecDeque<Step>,
    always_fail: Option<u16>,
    seen: Vec<(String, u64)>,
}

struct Worker {
    script: Arc<Mutex<Script>>,
    addr: std::net::SocketAddr,
    _server: tokio::task::JoinHandle<()>,
}

impl Worker {
    async fn start(steps: Vec<Step>, always_fail: Option<u16>) -> Self {
        use axum::http::{header, StatusCode};
        use axum::response::IntoResponse;
        use axum::routing::post;
        use axum::{Json, Router};

        let script = Arc::new(Mutex::new(Script {
            steps: steps.into(),
            always_fail,
            seen: Vec::new(),
        }));
        let shared = script.clone();
        let app = Router::new().route(
            "/agent",
            post(move |Json(req): Json<serde_json::Value>| {
                let shared = shared.clone();
                async move {
                    let step = {
                        let mut script = shared.lock().unwrap();
                        script.seen.push((
                            req["decision_id"].as_str().unwrap_or_default().to_string(),
                            req["attempts"].as_u64().unwrap_or_default(),
                        ));
                        script
                            .steps
                            .pop_front()
                            .or(script.always_fail.map(Step::Fail))
                    };
                    match step {
                        Some(Step::Fail(status)) => (
                            StatusCode::from_u16(status).unwrap(),
                            [(header::CONTENT_TYPE, "application/json")],
                            r#"{"error":"down"}"#,
                        )
                            .into_response(),
                        Some(Step::Slow(wait)) => {
                            tokio::time::sleep(wait).await;
                            Json(req["proposed"].clone()).into_response()
                        }
                        None => Json(req["proposed"].clone()).into_response(),
                    }
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        Self {
            script,
            addr,
            _server: server,
        }
    }

    fn seen(&self) -> Vec<(String, u64)> {
        self.script.lock().unwrap().seen.clone()
    }

    fn attempts_of(&self, decision: &str) -> Vec<u64> {
        self.seen()
            .into_iter()
            .filter(|(id, _)| id == decision)
            .map(|(_, attempt)| attempt)
            .collect()
    }
}

struct Engine {
    runtime: Arc<Runtime>,
    directory: Arc<StaticAgentDirectory>,
    db: SqliteDb,
    _dir: tempfile::TempDir,
    adapter: Mutex<Option<Arc<PushAdapter>>>,
}

fn retry(max_attempts: u32, run_secs: u32) -> RetryPolicy {
    RetryPolicy {
        queue_timeout_secs: None,
        run_timeout_secs: Some(run_secs),
        total_timeout_secs: Some(120),
        max_attempts,
        backoff_base_secs: 0,
        backoff_max_secs: 0,
    }
}

async fn engine(worker: &Worker, policy: RetryPolicy, lease: Duration, push: bool) -> Engine {
    engine_with(worker, policy, lease, push, None).await
}

async fn engine_with(
    worker: &Worker,
    policy: RetryPolicy,
    lease: Duration,
    push: bool,
    events_url: Option<String>,
) -> Engine {
    let dir = tempfile::tempdir().unwrap();
    let db = SqliteDb::open(
        dir.path().join("subs.db").to_str().unwrap(),
        Duration::from_secs(5),
    )
    .unwrap();
    let agent = AgentEntry {
        config: Some(AgentConfig {
            llm: Some("claude".to_string()),
            model: "stub".to_string(),
            ..Default::default()
        }),
        hosting: Hosting::Worker("w".to_string()),
        events: events_url.as_ref().map(|_| "app".to_string()),
    };
    let directory = Arc::new(
        StaticAgentDirectory::new(
            TENANT.to_string(),
            BTreeMap::from([("assistant".to_string(), agent)]),
            LlmBlocks::from_iter([("claude".to_string(), LlmBlock::engine())]),
        )
        .with_workers(
            BTreeMap::from([(
                "w".to_string(),
                WorkerBlock {
                    url: Some(format!("http://{}/agent", worker.addr)),
                },
            )]),
            None,
        )
        .with_events(
            events_url
                .map(|url| {
                    (
                        "app".to_string(),
                        EventsBlock {
                            url: Some(url),
                            timeout_secs: None,
                        },
                    )
                })
                .into_iter()
                .collect(),
        ),
    );
    let providers = LlmProviderRegistry::new(BTreeMap::from([(
        "claude".to_string(),
        Arc::new(Provider) as Arc<dyn LlmProviderTrait>,
    )]));
    let config = RuntimeConfig {
        wake_poll_interval: Duration::from_millis(100),
        worker_retry_resolver: Arc::new(Retry(policy)),
        ..RuntimeConfig::default()
    };
    let runtime = substructure_host::start(
        RuntimeDeps {
            blobs: Arc::new(substructure_host::runtime::blob::Nowhere),
            outbound: None,
            sessions: Arc::new(SqliteSessionStores::new(db.clone())),
            readers: Vec::new(),
            secrets: Arc::new(SqliteSecretStore::new(db.clone(), None)),
            agents: directory.clone(),
            llm: Arc::new(providers),
            llm_task_queue: Arc::new(SqliteQueue::<LlmTask>::new(
                db.clone(),
                Duration::from_secs(60),
            )),
            system1: Arc::new(substructure_host::runtime::system1::System1Registry::default()),
            system1_task_queue: Arc::new(SqliteQueue::<
                substructure_host::runtime::system1::System1Task,
            >::new(db.clone(), Duration::from_secs(60))),
            subagent_task_queue: Arc::new(SqliteQueue::<SubagentTask>::new(
                db.clone(),
                Duration::from_secs(60),
            )),
            connections: None,
            plugins: Arc::new(substructure_host::plugins::StaticPlugins::default()),
            connector_task_queue: Arc::new(SqliteQueue::<ConnectorTask>::new(
                db.clone(),
                Duration::from_secs(60),
            )),
            worker_queue: Arc::new(SqliteQueue::new(db.clone(), lease)),
            session_index_store: Arc::new(SqliteSessionIndexStore::new(db.clone()).unwrap()),
            alarms: Arc::new(SqliteAlarms::new(db.clone())),
            token_delta_transport: Arc::new(InMemoryTokenDeltaTransport::new()),
        },
        config,
    )
    .await;
    let engine = Engine {
        runtime,
        directory,
        db,
        _dir: dir,
        adapter: Mutex::new(None),
    };
    if push {
        engine.push().await;
    }
    engine
}

impl Engine {
    async fn push(&self) {
        let adapter = Arc::new(PushAdapter::new(
            self.runtime.clone(),
            self.directory.clone(),
            Arc::new(SqliteSecretStore::new(self.db.clone(), None)),
            16,
        ));
        adapter.start().await;
        *self.adapter.lock().unwrap() = Some(adapter);
    }

    async fn submit(&self, session_id: &str) -> String {
        self.say(session_id, "hi", QueueMode::Refuse).await.turn_id
    }

    async fn say(&self, session_id: &str, text: &str, queue: QueueMode) -> ClientInputOutput {
        self.runtime
            .handle_client_input(HandleClientInput {
                session_id: session_id.to_string(),
                caller: caller(),
                agent: None,
                worker: None,
                owner: SessionOwner {
                    tenant_id: TENANT.to_string(),
                    requester: Requester::new(
                        Subject::new(Issuer::app(), "user-1".to_string()),
                        Default::default(),
                    ),
                    metadata: Default::default(),
                },
                input: ClientInput::Message {
                    agent_id: "assistant".to_string(),
                    turn_id: None,
                    message: DraftMessage {
                        id: None,
                        role: Role::User,
                        content: Some(Content::Text(text.to_string())),
                        tool_calls: None,
                        tool_call_id: None,
                        name: None,
                        reasoning: None,
                    },
                    stream: false,
                    queue,
                },
                span: SpanContext::root(),
            })
            .await
            .expect("the input is accepted")
    }

    async fn settle(
        &self,
        session_id: &str,
        turn_id: String,
        within: Duration,
    ) -> Vec<EventPayload> {
        let mut events = self
            .runtime
            .stream(
                SessionSubscriptionSpec {
                    session_id: session_id.to_string(),
                    caller: caller(),
                    scope: SubscriptionScope::Turn { turn_id },
                },
                Some(Seq(0)),
            )
            .await
            .expect("streams");
        let mut seen = Vec::new();
        let settled = tokio::time::timeout(within, async {
            while let Some(event) = events.recv().await {
                let done = ends(&event.payload);
                seen.push(event.payload);
                if done {
                    return;
                }
            }
        })
        .await;
        assert!(settled.is_ok(), "the turn did not settle: {seen:#?}");
        seen
    }

    async fn turn(&self, session_id: &str) -> Vec<EventPayload> {
        let turn_id = self.submit(session_id).await;
        self.settle(session_id, turn_id, Duration::from_secs(20))
            .await
    }

    fn queued(&self) -> i64 {
        self.db
            .reader
            .open()
            .unwrap()
            .query_row("SELECT COUNT(*) FROM engine_queue", [], |row| row.get(0))
            .unwrap()
    }
}

fn caller() -> Caller {
    Caller::System {
        tenant_id: TENANT.to_string(),
    }
}

fn ends(event: &EventPayload) -> bool {
    match event {
        EventPayload::TurnCompleted(_) => true,
        EventPayload::DecisionErrored(e) => !e.retryable,
        _ => false,
    }
}

fn completed(events: &[EventPayload]) -> bool {
    events
        .iter()
        .any(|e| matches!(e, EventPayload::TurnCompleted(t) if t.error.is_none()))
}

fn first_decision(worker: &Worker) -> String {
    worker
        .seen()
        .first()
        .expect("the worker was called")
        .0
        .clone()
}

#[tokio::test]
async fn a_worker_that_fails_then_recovers_gets_each_attempt_once() {
    let worker = Worker::start(vec![Step::Fail(503), Step::Fail(503)], None).await;
    let e = engine(&worker, retry(5, 10), Duration::from_secs(60), true).await;

    let events = e.turn("s1").await;

    assert!(completed(&events), "the turn completes: {events:#?}");
    assert_eq!(worker.attempts_of(&first_decision(&worker)), vec![0, 1, 2]);
    let retried = events
        .iter()
        .filter(|e| matches!(e, EventPayload::DecisionErrored(d) if d.retryable))
        .count();
    assert_eq!(retried, 2);
}

#[tokio::test]
async fn retries_that_run_out_fail_the_decision() {
    let worker = Worker::start(Vec::new(), Some(503)).await;
    let e = engine(&worker, retry(3, 10), Duration::from_secs(60), true).await;

    let events = e.turn("s1").await;

    assert!(
        events
            .iter()
            .any(|e| matches!(e, EventPayload::TurnCompleted(t) if t.error.is_some())),
        "the turn ends with the error: {events:#?}"
    );
    assert_eq!(worker.attempts_of(&first_decision(&worker)), vec![0, 1, 2]);
}

#[tokio::test]
async fn a_worker_that_hangs_past_the_deadline_is_tried_again_and_the_late_answer_is_ignored() {
    let worker = Worker::start(vec![Step::Slow(Duration::from_secs(3))], None).await;
    let e = engine(&worker, retry(5, 1), Duration::from_secs(60), true).await;

    let events = e.turn("s1").await;
    tokio::time::sleep(Duration::from_secs(3)).await;

    assert!(completed(&events), "the turn completes: {events:#?}");
    let decision = first_decision(&worker);
    assert_eq!(worker.attempts_of(&decision), vec![0, 1]);
    let log = e
        .runtime
        .read_session_events(&caller(), "s1", None, None)
        .await
        .unwrap();
    let answers = log
        .iter()
        .filter(|ev| matches!(&ev.payload, EventPayload::DecisionCompleted(d) if d.id == decision))
        .count();
    assert_eq!(answers, 1, "only the first answer counts");
}

#[tokio::test]
async fn a_push_that_runs_longer_than_its_lease_is_sent_once() {
    let worker = Worker::start(vec![Step::Slow(Duration::from_millis(2500))], None).await;
    let e = engine(&worker, retry(5, 30), Duration::from_secs(1), true).await;

    let events = e.turn("s1").await;

    assert!(completed(&events), "the turn completes: {events:#?}");
    assert_eq!(worker.attempts_of(&first_decision(&worker)), vec![0]);
}

#[tokio::test]
async fn a_decision_taken_by_a_crashed_engine_is_pushed_once_after_its_lease() {
    let worker = Worker::start(Vec::new(), None).await;
    let e = engine(&worker, retry(5, 30), Duration::from_secs(1), false).await;
    let turn_id = e.submit("s1").await;
    let decisions = e.runtime.decisions();
    let taken = tokio::time::timeout(
        Duration::from_secs(5),
        decisions.receive(&queue::decisions(TENANT)),
    )
    .await
    .expect("the decision is queued")
    .unwrap();
    let (decision, attempts) = (
        taken.envelope.task.decision_id.clone(),
        u64::from(taken.envelope.task.attempts),
    );
    drop(taken);

    e.push().await;
    let events = e.settle("s1", turn_id, Duration::from_secs(20)).await;

    assert!(completed(&events), "the turn completes: {events:#?}");
    assert_eq!(worker.attempts_of(&decision), vec![attempts]);
    assert_eq!(e.queued(), 0);
}

#[tokio::test]
async fn a_decision_no_longer_open_is_not_pushed() {
    let worker = Worker::start(Vec::new(), None).await;
    let e = engine(&worker, retry(5, 30), Duration::from_secs(60), false).await;
    e.submit("s1").await;
    let decisions = e.runtime.decisions();
    let taken = tokio::time::timeout(
        Duration::from_secs(5),
        decisions.receive(&queue::decisions(TENANT)),
    )
    .await
    .expect("the decision is queued")
    .unwrap();
    decisions
        .retry(&taken.receipt, Duration::ZERO)
        .await
        .unwrap();
    e.runtime
        .interrupt_session(InterruptSessionInput {
            session_id: "s1".to_string(),
            interrupt_id: "stop".to_string(),
            reason: "test".to_string(),
            payload: serde_json::Value::Null,
            caller: caller(),
            span: SpanContext::root(),
        })
        .await
        .unwrap();

    e.push().await;
    tokio::time::sleep(Duration::from_secs(2)).await;

    assert!(worker.seen().is_empty(), "pushed: {:?}", worker.seen());
    assert_eq!(e.queued(), 0);
}

#[tokio::test]
async fn messages_merged_during_a_turn_run_as_one_turn() {
    let worker = Worker::start(vec![Step::Slow(Duration::from_millis(1500))], None).await;
    let e = engine(&worker, retry(5, 30), Duration::from_secs(60), true).await;

    let running = e.say("s1", "hi", QueueMode::Refuse).await;
    let first = e.say("s1", "a", QueueMode::Merge).await;
    let second = e.say("s1", "b", QueueMode::Merge).await;

    assert!(!running.queued);
    assert!(first.queued && second.queued);
    assert_eq!(first.turn_id, second.turn_id, "both join one waiting turn");
    let events = e
        .settle("s1", first.turn_id.clone(), Duration::from_secs(20))
        .await;
    assert!(completed(&events), "the merged turn completes: {events:#?}");

    let log = e
        .runtime
        .read_session_events(&caller(), "s1", None, None)
        .await
        .unwrap();
    let turns = log
        .iter()
        .filter(|ev| matches!(ev.payload, EventPayload::TurnStarted(_)))
        .count();
    assert_eq!(turns, 2, "one turn for hi, one for a and b");
    let said: Vec<String> = log
        .iter()
        .filter_map(|ev| match &ev.payload {
            EventPayload::NewMessage(m) if m.message.role == Role::User => {
                m.message.content.as_ref().map(Content::text_owned)
            }
            _ => None,
        })
        .collect();
    assert_eq!(said, vec!["hi", "a", "b"]);
}

async fn events_app() -> (String, Arc<Mutex<Vec<serde_json::Value>>>) {
    use axum::routing::post;
    use axum::{Json, Router};

    let got: Arc<Mutex<Vec<serde_json::Value>>> = Arc::default();
    let seen = got.clone();
    let app = Router::new().route(
        "/events",
        post(move |Json(body): Json<serde_json::Value>| {
            let seen = seen.clone();
            async move {
                seen.lock().unwrap().push(body);
                "ok"
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    (format!("http://{addr}/events"), got)
}

#[tokio::test]
async fn an_agents_events_url_gets_every_event_of_its_sessions_in_order() {
    let (url, got) = events_app().await;
    let worker = Worker::start(Vec::new(), None).await;
    let e = engine_with(
        &worker,
        retry(5, 30),
        Duration::from_secs(60),
        true,
        Some(url),
    )
    .await;

    let events = e.turn("s1").await;
    assert!(completed(&events), "the turn completes: {events:#?}");

    let started = tokio::time::Instant::now();
    let seqs = loop {
        let seqs: Vec<u64> = got
            .lock()
            .unwrap()
            .iter()
            .filter(|post| post["type"] == "events" && post["session_id"] == "s1")
            .flat_map(|post| post["events"].as_array().cloned().unwrap_or_default())
            .filter_map(|event| event["seq"].as_u64())
            .collect();
        let done = got.lock().unwrap().iter().any(|post| {
            post["events"]
                .as_array()
                .is_some_and(|all| all.iter().any(|e| e["payload"]["type"] == "turn.completed"))
        });
        if done {
            break seqs;
        }
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "no turn.completed arrived"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    };
    assert_eq!(seqs, (1..=seqs.len() as u64).collect::<Vec<_>>());
}
