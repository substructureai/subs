use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use rust_decimal::Decimal;
use serde_json::{json, Value};
use substructure_host::event_store::Seq;
use substructure_host::llm::{
    CallContext, InMemoryTokenDeltaTransport, LlmBlock, LlmBlocks, LlmCallError, LlmCallable,
    LlmProviderRegistry, LlmProviderTrait, LlmTask,
};
use substructure_host::protocol::{
    AgentConfig, ClientInput, Content, DraftMessage, Issuer, LlmResponse, PromptRequest, QueueMode,
    Requester, Role, SessionOwner, Subject, Usage,
};
use substructure_host::providers::sqlite::{
    SqliteAlarms, SqliteDb, SqliteQueue, SqliteSecretStore, SqliteSessionIndexStore,
    SqliteSessionStores,
};
use substructure_host::runtime::connector::ConnectorTask;
use substructure_host::runtime::system1::{
    Media, System1Block, System1Blocks, System1Callable, System1Kind, System1Registry, System1Task,
};
use substructure_host::session::events::EventPayload;
use substructure_host::session::subscriptions::{SessionSubscriptionSpec, SubscriptionScope};
use substructure_host::span::SpanContext;
use substructure_host::subagent::SubagentTask;
use substructure_host::transport::push::PushAdapter;
use substructure_host::worker::{AgentEntry, Hosting, StaticAgentDirectory, WorkerBlock};
use substructure_host::{Caller, HandleClientInput, RuntimeConfig, RuntimeDeps};
use substructure_session::system1::{System1Request, System1Response};

const TENANT: &str = "default";

#[derive(Default)]
struct Seen {
    models: Vec<String>,
    asked: Vec<System1Request>,
    finished: Vec<Value>,
}

struct Model(Arc<Mutex<Seen>>);

#[async_trait]
impl LlmCallable for Model {
    async fn call(
        &self,
        request: &PromptRequest,
        _ctx: &CallContext<'_>,
    ) -> Result<LlmResponse, LlmCallError> {
        self.0.lock().unwrap().models.push(request.model.clone());
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

struct Provider(Arc<Mutex<Seen>>);

#[async_trait]
impl LlmProviderTrait for Provider {
    async fn resolve(&self, _owner: &SessionOwner) -> Result<Arc<dyn LlmCallable>, String> {
        Ok(Arc::new(Model(self.0.clone())))
    }
}

struct Judge(Arc<Mutex<Seen>>);

#[async_trait]
impl System1Callable for Judge {
    async fn call(
        &self,
        request: &System1Request,
        _assets: &BTreeMap<String, Media>,
    ) -> Result<System1Response, LlmCallError> {
        self.0.lock().unwrap().asked.push(request.clone());
        Ok(System1Response {
            model: "jev-1.13".to_string(),
            answers: BTreeMap::from([(
                "hard".to_string(),
                json!({ "type": "score", "score": 1.7, "confidence": 0.8,
                        "legend": { "0": "easy", "1": "normal", "2": "hard" },
                        "probabilities": { "0": 0.1, "1": 0.1, "2": 0.8 } }),
            )]),
            usage: Usage::new(120, 0, 0, 12),
            cost: Some(Decimal::new(5, 3)),
        })
    }
}

fn decide(req: &Value, seen: &Mutex<Seen>, block: &str) -> Value {
    let trigger = &req["trigger"];
    match trigger["type"].as_str() {
        Some("client.messages") => json!({
            "messages": req["proposed"]["messages"],
            "actions": [{
                "type": "system1.call",
                "system1": block,
                "questions": { "hard": {
                    "type": "score",
                    "instructions": "How hard is this request?",
                    "criteria": ["easy", "normal", "hard"]
                } }
            }]
        }),
        Some("system1.finished") => {
            seen.lock().unwrap().finished.push(trigger.clone());
            let hard = trigger["answers"]["hard"]["score"].as_f64().unwrap_or(0.0) >= 1.5;
            json!({
                "messages": req["messages"],
                "actions": [{ "type": "llm.call", "model": if hard { "big" } else { "small" } }]
            })
        }
        _ => req["proposed"].clone(),
    }
}

struct Engine {
    runtime: Arc<substructure_host::Runtime>,
    seen: Arc<Mutex<Seen>>,
    _adapter: Arc<PushAdapter>,
    _dir: tempfile::TempDir,
    _server: tokio::task::JoinHandle<()>,
}

async fn engine(block: &'static str) -> Engine {
    use axum::routing::post;
    use axum::{Json, Router};

    let seen = Arc::new(Mutex::new(Seen::default()));
    let shared = seen.clone();
    let app = Router::new().route(
        "/agent",
        post(move |Json(req): Json<Value>| {
            let shared = shared.clone();
            async move { Json(decide(&req, &shared, block)) }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });

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
        events: None,
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
                    url: Some(format!("http://{addr}/agent")),
                },
            )]),
            None,
        )
        .with_system1(System1Blocks::from_iter([(
            "ts".to_string(),
            System1Block {
                kind: System1Kind::Typesafe,
                model: Some("jev-latest".to_string()),
                questions: None,
            },
        )])),
    );
    let lease = Duration::from_secs(60);
    let runtime = substructure_host::start(
        RuntimeDeps {
            blobs: Arc::new(substructure_host::runtime::blob::Nowhere),
            outbound: None,
            sessions: Arc::new(SqliteSessionStores::new(db.clone())),
            readers: Vec::new(),
            secrets: Arc::new(SqliteSecretStore::new(db.clone(), None)),
            agents: directory.clone(),
            llm: Arc::new(LlmProviderRegistry::new(BTreeMap::from([(
                "claude".to_string(),
                Arc::new(Provider(seen.clone())) as Arc<dyn LlmProviderTrait>,
            )]))),
            llm_task_queue: Arc::new(SqliteQueue::<LlmTask>::new(db.clone(), lease)),
            system1: Arc::new(System1Registry::new(BTreeMap::from([(
                "ts".to_string(),
                Arc::new(Judge(seen.clone())) as Arc<dyn System1Callable>,
            )]))),
            system1_task_queue: Arc::new(SqliteQueue::<System1Task>::new(db.clone(), lease)),
            subagent_task_queue: Arc::new(SqliteQueue::<SubagentTask>::new(db.clone(), lease)),
            connections: None,
            plugins: Arc::new(substructure_host::plugins::StaticPlugins::default()),
            connector_task_queue: Arc::new(SqliteQueue::<ConnectorTask>::new(db.clone(), lease)),
            script_task_queue: Arc::new(SqliteQueue::<
                substructure_host::runtime::script::ScriptTask,
            >::new(db.clone(), lease)),
            worker_queue: Arc::new(SqliteQueue::new(db.clone(), lease)),
            session_index_store: Arc::new(SqliteSessionIndexStore::new(db.clone()).unwrap()),
            alarms: Arc::new(SqliteAlarms::new(db.clone())),
            token_delta_transport: Arc::new(InMemoryTokenDeltaTransport::new()),
        },
        RuntimeConfig::default(),
    )
    .await;
    let adapter = Arc::new(PushAdapter::new(
        runtime.clone(),
        directory,
        Arc::new(SqliteSecretStore::new(db.clone(), None)),
        16,
    ));
    adapter.start().await;
    Engine {
        runtime,
        seen,
        _adapter: adapter,
        _dir: dir,
        _server: server,
    }
}

fn caller() -> Caller {
    Caller::System {
        tenant_id: TENANT.to_string(),
    }
}

impl Engine {
    async fn turn(&self, session_id: &str, text: &str) -> Vec<EventPayload> {
        let turn_id = self
            .runtime
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
                    queue: QueueMode::Refuse,
                },
                span: SpanContext::root(),
            })
            .await
            .expect("the input is accepted")
            .turn_id;
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
        let settled = tokio::time::timeout(Duration::from_secs(20), async {
            while let Some(event) = events.recv().await {
                let done = matches!(event.payload, EventPayload::TurnCompleted(_));
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
}

#[tokio::test]
async fn a_worker_asks_a_system1_block_and_routes_on_the_answer() {
    let e = engine("ts").await;

    let events = e.turn("s1", "prove the four colour theorem").await;

    let ended = events.iter().find_map(|ev| match ev {
        EventPayload::TurnCompleted(t) => Some(t),
        _ => None,
    });
    let ended = ended.expect("the turn completes");
    assert!(ended.error.is_none(), "{events:#?}");
    assert_eq!(ended.turn_cost, Decimal::new(5, 3));

    let seen = e.seen.lock().unwrap();
    let asked = seen.asked.first().expect("the block was asked");
    assert_eq!(asked.model, "jev-latest");
    assert!(
        asked
            .state
            .as_ref()
            .is_some_and(|s| s.to_string().contains("four colour")),
        "the state is the conversation: {:?}",
        asked.state
    );
    let finished = seen
        .finished
        .first()
        .expect("the worker got system1.finished");
    assert_eq!(finished["ok"], json!(true));
    assert_eq!(finished["answers"]["hard"]["score"], json!(1.7));
    assert_eq!(seen.models, vec!["big".to_string()]);
}

#[tokio::test]
async fn a_call_to_an_undeclared_block_fails_the_turn() {
    let e = engine("missing").await;

    let events = e.turn("s1", "hi").await;

    let error = events.iter().find_map(|ev| match ev {
        EventPayload::TurnCompleted(t) => t.error.clone(),
        _ => None,
    });
    let error = error.expect("the turn ends with the error");
    assert!(
        error.message.contains("no system1 block `missing`"),
        "{error:?}"
    );
}
