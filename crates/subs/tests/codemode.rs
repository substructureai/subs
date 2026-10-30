use std::collections::BTreeMap;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use substructure_host::codemode::{
    ProcessRunner, Ran, RunError, Script, ScriptFiles, ScriptRunner, ARG,
};
use substructure_host::event_store::Seq;
use substructure_host::llm::{
    CallContext, InMemoryTokenDeltaTransport, LlmBlock, LlmBlocks, LlmCallError, LlmCallable,
    LlmProviderRegistry, LlmProviderTrait, LlmTask,
};
use substructure_host::protocol::{
    AgentConfig, AgentTool, ClientInput, Content, DeferTools, DraftMessage, Issuer, LlmResponse,
    PromptRequest, QueueMode, Requester, RetryPolicy, Role, SessionOwner, Subject, ToolCall,
    ToolCallFunction,
};
use substructure_host::providers::sqlite::{
    SqliteAlarms, SqliteDb, SqliteQueue, SqliteSecretStore, SqliteSessionIndexStore,
    SqliteSessionStores,
};
use substructure_host::retry::WorkerRetryResolver;
use substructure_host::runtime::connector::ConnectorTask;
use substructure_host::session::events::EventPayload;
use substructure_host::session::subscriptions::{SessionSubscriptionSpec, SubscriptionScope};
use substructure_host::span::SpanContext;
use substructure_host::subagent::SubagentTask;
use substructure_host::transport::push::PushAdapter;
use substructure_host::worker::{AgentEntry, Hosting, StaticAgentDirectory, WorkerBlock};
use substructure_host::{Caller, HandleClientInput, Runtime, RuntimeConfig, RuntimeDeps};

const TENANT: &str = "default";
const WAIT: Duration = Duration::from_secs(60);
/// Short, so a restarted engine takes over what the crashed one held.
const LEASE: Duration = Duration::from_secs(2);

#[derive(Default)]
struct Seen {
    description: Option<String>,
    prompt: Option<String>,
    result: Option<String>,
}

/// Answers the user with `run_code` and `code`, then ends the turn.
struct Model {
    code: &'static str,
    seen: Arc<Mutex<Seen>>,
}

#[async_trait]
impl LlmCallable for Model {
    async fn call(
        &self,
        request: &PromptRequest,
        _ctx: &CallContext<'_>,
    ) -> Result<LlmResponse, LlmCallError> {
        let last = request.messages.last().expect("a prompt");
        let tool_calls = match last.role {
            Role::Tool => {
                let text = serde_json::to_string(&last.content).unwrap_or_default();
                self.seen.lock().unwrap().result = Some(text);
                vec![]
            }
            _ => {
                let description = request
                    .tools
                    .iter()
                    .flatten()
                    .find(|t| t.name == "run_code")
                    .map(|t| t.description.clone());
                let prompt = serde_json::to_string(&request.messages).unwrap_or_default();
                let mut seen = self.seen.lock().unwrap();
                seen.description = description;
                seen.prompt = Some(prompt);
                drop(seen);
                vec![ToolCall {
                    id: "tc-1".to_string(),
                    call_type: "function".to_string(),
                    function: ToolCallFunction {
                        name: "run_code".to_string(),
                        arguments: serde_json::json!({ "code": self.code }).to_string(),
                    },
                }]
            }
        };
        Ok(LlmResponse {
            model: request.model.clone(),
            content: tool_calls.is_empty().then(|| "done".to_string()),
            tool_calls,
            finish_reason: Some("stop".to_string()),
            usage: None,
            cost: None,
            images: vec![],
            reasoning: None,
        })
    }
}

struct Provider {
    code: &'static str,
    seen: Arc<Mutex<Seen>>,
}

#[async_trait]
impl LlmProviderTrait for Provider {
    async fn resolve(&self, _owner: &SessionOwner) -> Result<Arc<dyn LlmCallable>, String> {
        Ok(Arc::new(Model {
            code: self.code,
            seen: self.seen.clone(),
        }))
    }
}

struct Retry;

#[async_trait]
impl WorkerRetryResolver for Retry {
    async fn resolve(&self, _tenant_id: &str) -> RetryPolicy {
        RetryPolicy {
            queue_timeout_secs: None,
            run_timeout_secs: Some(3),
            total_timeout_secs: Some(120),
            max_attempts: 5,
            backoff_base_secs: 0,
            backoff_max_secs: 0,
        }
    }
}

fn tool(name: &str, description: &str, defer: bool) -> AgentTool {
    AgentTool {
        name: name.to_string(),
        description: description.to_string(),
        input: None,
        output: None,
        handler: None,
        defer: Some(defer),
    }
}

struct Engine {
    runtime: Arc<Runtime>,
    adapter: Arc<PushAdapter>,
}

impl Engine {
    async fn start(
        db: &Path,
        code: &'static str,
        config: AgentConfig,
        worker: Option<String>,
        seen: Arc<Mutex<Seen>>,
    ) -> Self {
        let runner = Arc::new(runner());
        Engine::with_runner(db, code, config, worker, seen, runner, WAIT).await
    }

    async fn with_runner(
        db: &Path,
        code: &'static str,
        config: AgentConfig,
        worker: Option<String>,
        seen: Arc<Mutex<Seen>>,
        runner: Arc<dyn ScriptRunner>,
        wait: Duration,
    ) -> Self {
        let db = SqliteDb::open(db.to_str().unwrap(), Duration::from_secs(5)).unwrap();
        let agent = AgentEntry {
            config: Some(config),
            hosting: match worker {
                Some(_) => Hosting::Worker("w".to_string()),
                None => Hosting::Engine,
            },
            events: None,
        };
        let directory = StaticAgentDirectory::new(
            TENANT.to_string(),
            BTreeMap::from([("assistant".to_string(), agent)]),
            LlmBlocks::from_iter([("claude".to_string(), LlmBlock::engine())]),
        );
        let directory = Arc::new(match worker {
            Some(url) => directory.with_workers(
                BTreeMap::from([("w".to_string(), WorkerBlock { url: Some(url) })]),
                None,
            ),
            None => directory,
        });
        let providers = LlmProviderRegistry::new(BTreeMap::from([(
            "claude".to_string(),
            Arc::new(Provider { code, seen }) as Arc<dyn LlmProviderTrait>,
        )]));
        let runtime = substructure_host::start(
            RuntimeDeps {
                blobs: Arc::new(substructure_host::runtime::blob::Nowhere),
                outbound: None,
                sessions: Arc::new(SqliteSessionStores::new(db.clone())),
                readers: Vec::new(),
                secrets: Arc::new(SqliteSecretStore::new(db.clone(), None)),
                agents: directory.clone(),
                llm: Arc::new(providers),
                llm_task_queue: Arc::new(SqliteQueue::<LlmTask>::new(db.clone(), LEASE)),
                system1: Arc::new(substructure_host::runtime::system1::System1Registry::default()),
                system1_task_queue: Arc::new(SqliteQueue::<
                    substructure_host::runtime::system1::System1Task,
                >::new(db.clone(), LEASE)),
                subagent_task_queue: Arc::new(SqliteQueue::<SubagentTask>::new(db.clone(), LEASE)),
                connections: None,
                plugins: Arc::new(substructure_host::plugins::StaticPlugins::default()),
                connector_task_queue: Arc::new(SqliteQueue::<ConnectorTask>::new(
                    db.clone(),
                    LEASE,
                )),
                script_task_queue: Arc::new(SqliteQueue::<
                    substructure_host::runtime::script::ScriptTask,
                >::new(db.clone(), LEASE)),
                worker_queue: Arc::new(SqliteQueue::new(db.clone(), LEASE)),
                session_index_store: Arc::new(SqliteSessionIndexStore::new(db.clone()).unwrap()),
                alarms: Arc::new(SqliteAlarms::new(db.clone())),
                token_delta_transport: Arc::new(InMemoryTokenDeltaTransport::new()),
            },
            RuntimeConfig {
                wake_poll_interval: Duration::from_millis(100),
                worker_retry_resolver: Arc::new(Retry),
                script_runner: runner,
                script_wait: wait,
                ..RuntimeConfig::default()
            },
        )
        .await;
        let adapter = Arc::new(PushAdapter::new(
            runtime.clone(),
            directory,
            Arc::new(SqliteSecretStore::new(db, None)),
            16,
        ));
        adapter.start().await;
        Engine { runtime, adapter }
    }

    async fn say(&self) -> String {
        self.runtime
            .handle_client_input(HandleClientInput {
                session_id: "s-1".to_string(),
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
                        content: Some(Content::Text("go".to_string())),
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
            .turn_id
    }

    async fn settled(&self, turn_id: String) -> Vec<EventPayload> {
        let mut events = self
            .runtime
            .stream(
                SessionSubscriptionSpec {
                    session_id: "s-1".to_string(),
                    caller: caller(),
                    scope: SubscriptionScope::Turn { turn_id },
                },
                Some(Seq(0)),
            )
            .await
            .expect("streams");
        let mut log = Vec::new();
        let settled = tokio::time::timeout(Duration::from_secs(30), async {
            while let Some(event) = events.recv().await {
                let done = matches!(event.payload, EventPayload::TurnCompleted(_));
                log.push(event.payload);
                if done {
                    return;
                }
            }
        })
        .await;
        assert!(settled.is_ok(), "the turn did not settle: {log:#?}");
        log
    }

    async fn crash(self) {
        self.adapter.stop();
        self.runtime.shutdown().await;
    }
}

fn runner() -> ProcessRunner {
    ProcessRunner::new(
        std::path::PathBuf::from(env!("CARGO_BIN_EXE_subs")),
        vec![ARG.into()],
    )
}

/// Down for its first step, then a process runner.
struct Flaky {
    down: Mutex<bool>,
    runner: ProcessRunner,
}

#[async_trait]
impl ScriptRunner for Flaky {
    async fn run(&self, script: Script, files: ScriptFiles) -> Result<Ran, RunError> {
        if std::mem::take(&mut *self.down.lock().unwrap()) {
            return Err(RunError("the code machine is restarting".into()));
        }
        self.runner.run(script, files).await
    }
}

fn caller() -> Caller {
    Caller::System {
        tenant_id: TENANT.to_string(),
    }
}

fn agent(tools: Vec<AgentTool>, defer: bool) -> AgentConfig {
    AgentConfig {
        llm: Some("claude".to_string()),
        model: "stub".to_string(),
        tools,
        defer_tools: defer.then(DeferTools::default),
        codemode: Some(substructure_host::codemode::Language::Javascript),
        ..Default::default()
    }
}

const SEARCH: &str = "const [a, b] = await Promise.all([\n  tools.tool_search({ query: 'weather' }),\n  tools.tool_search({ query: 'tides' }),\n]);\nconsole.log('searched');\nconst again = await tools.tool_search({ query: 'weather' });\nreturn { same: JSON.stringify(a) === JSON.stringify(again), differs: JSON.stringify(a) !== JSON.stringify(b) };";

#[tokio::test]
async fn a_script_calls_tools_and_only_its_result_reaches_the_model() {
    let dir = tempfile::tempdir().unwrap();
    let seen = Arc::new(Mutex::new(Seen::default()));
    let tools = vec![
        tool("weather", "Today's weather for a city", true),
        tool("tides", "High and low tides for a harbour", true),
    ];
    let engine = Engine::start(
        &dir.path().join("subs.db"),
        SEARCH,
        agent(tools, true),
        None,
        seen.clone(),
    )
    .await;
    let turn = engine.say().await;
    let log = engine.settled(turn).await;

    let children: Vec<(&str, u32)> = log
        .iter()
        .filter_map(|e| match e {
            EventPayload::ToolCallRequested(r) => {
                r.script.as_ref().map(|s| (s.parent.as_str(), s.seq))
            }
            _ => None,
        })
        .collect();
    assert_eq!(children, vec![("tc-1", 1), ("tc-1", 2), ("tc-1", 3)]);

    let seen = seen.lock().unwrap();
    let description = seen.description.as_deref().expect("run_code was offered");
    let prompt = seen.prompt.as_deref().expect("the model was prompted");
    assert!(
        !prompt.contains("weather"),
        "a script finds deferred tools with tool_search: {prompt}"
    );
    assert!(
        description.contains("tools.tool_search({ query })"),
        "{description}"
    );
    assert!(
        !description.contains("weather(args:"),
        "deferred tools are not declared"
    );
    let result = seen.result.as_deref().expect("the model heard the result");
    assert!(result.contains(r#"\"same\":true"#), "{result}");
    assert!(result.contains(r#"\"differs\":true"#), "{result}");
    assert!(result.contains("searched"), "{result}");
    assert!(
        !result.contains("Today's weather"),
        "search results stay in the script: {result}"
    );
}

/// A worker that answers `add` calls, and holds every call while `held`.
#[derive(Clone, Default)]
struct Worker {
    held: Arc<Mutex<bool>>,
    calls: Arc<Mutex<Vec<String>>>,
    asked: Arc<tokio::sync::Notify>,
}

impl Worker {
    async fn serve(&self) -> String {
        use axum::routing::post;
        use axum::Json;

        let worker = self.clone();
        let app = axum::Router::new().route(
            "/agent",
            post(move |Json(req): Json<serde_json::Value>| {
                let worker = worker.clone();
                async move {
                    let trigger = &req["trigger"];
                    if trigger["type"] != "tool.execute" {
                        return Json(req["proposed"].clone());
                    }
                    let input = match &trigger["input"]["value"] {
                        serde_json::Value::Null => trigger["arguments"].clone(),
                        value => value.clone(),
                    };
                    let input: serde_json::Value = match input {
                        serde_json::Value::String(s) => {
                            serde_json::from_str(&s).unwrap_or_default()
                        }
                        value => value,
                    };
                    worker.calls.lock().unwrap().push(input.to_string());
                    worker.asked.notify_waiters();
                    let held = *worker.held.lock().unwrap();
                    if held {
                        std::future::pending::<()>().await;
                    }
                    let sum = input["a"].as_i64().unwrap_or(0) + input["b"].as_i64().unwrap_or(0);
                    Json(serde_json::json!({
                        "actions": [{
                            "type": "tool.result",
                            "result": { "content": [{ "type": "text", "text": sum.to_string() }] }
                        }]
                    }))
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/agent", listener.local_addr().unwrap());
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        url
    }

    fn calls(&self) -> Vec<String> {
        self.calls.lock().unwrap().clone()
    }
}

const ADD: &str = "const one = await tools.add({ a: 1, b: 2 });\nconst two = await tools.add({ a: one, b: 4 });\nreturn two;";

#[tokio::test]
async fn a_script_survives_the_engine_restarting_mid_run() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("subs.db");
    let worker = Worker::default();
    let url = worker.serve().await;
    let tools = vec![tool("add", "Adds a and b.", false)];

    *worker.held.lock().unwrap() = true;
    let seen = Arc::new(Mutex::new(Seen::default()));
    let first = Engine::start(
        &db,
        ADD,
        agent(tools.clone(), false),
        Some(url.clone()),
        seen.clone(),
    )
    .await;
    let asked = worker.asked.notified();
    let turn = first.say().await;
    tokio::time::timeout(Duration::from_secs(20), asked)
        .await
        .expect("the script reaches its first call");
    first.crash().await;

    *worker.held.lock().unwrap() = false;
    let second = Engine::start(&db, ADD, agent(tools, false), Some(url), seen.clone()).await;
    second.settled(turn).await;

    let result = seen
        .lock()
        .unwrap()
        .result
        .clone()
        .expect("the model heard the result");
    assert!(result.contains(r"Returned:\n7"), "{result}");
    let calls = worker.calls();
    let first_call = calls.iter().filter(|c| c.contains(r#""a":1"#)).count();
    let second_call = calls.iter().filter(|c| c.contains(r#""a":3"#)).count();
    assert!(
        (1..=2).contains(&first_call),
        "the held call is sent again at most once: {calls:?}"
    );
    assert_eq!(second_call, 1, "the next call is made once: {calls:?}");
}

#[tokio::test]
async fn a_step_the_runner_could_not_take_is_tried_again() {
    let dir = tempfile::tempdir().unwrap();
    let seen = Arc::new(Mutex::new(Seen::default()));
    let flaky = Arc::new(Flaky {
        down: Mutex::new(true),
        runner: runner(),
    });
    let engine = Engine::with_runner(
        &dir.path().join("subs.db"),
        "return 6 * 7;",
        agent(vec![], false),
        None,
        seen.clone(),
        flaky.clone(),
        WAIT,
    )
    .await;
    let turn = engine.say().await;
    engine.settled(turn).await;
    assert!(!*flaky.down.lock().unwrap(), "the first try failed");
    let result = seen
        .lock()
        .unwrap()
        .result
        .clone()
        .expect("the model heard the result");
    assert!(result.contains(r"Returned:\n42"), "{result}");
}

#[tokio::test]
async fn a_step_that_waits_too_long_fails_and_says_why() {
    let dir = tempfile::tempdir().unwrap();
    let seen = Arc::new(Mutex::new(Seen::default()));
    let engine = Engine::with_runner(
        &dir.path().join("subs.db"),
        "return 1;",
        agent(vec![], false),
        None,
        seen.clone(),
        Arc::new(runner()),
        Duration::ZERO,
    )
    .await;
    let turn = engine.say().await;
    engine.settled(turn).await;
    let result = seen
        .lock()
        .unwrap()
        .result
        .clone()
        .expect("the model heard");
    assert!(result.contains("waited"), "{result}");
}
