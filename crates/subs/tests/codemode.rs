use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use substructure_host::codemode::{sandbox, Sandbox};
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
use substructure_host::worker::{AgentEntry, Hosting, StaticAgentDirectory};
use substructure_host::{Caller, HandleClientInput, Runtime, RuntimeConfig, RuntimeDeps};

const TENANT: &str = "default";

const CODE: &str = "const [a, b] = await Promise.all([\n  tools.tool_search({ query: 'weather' }),\n  tools.tool_search({ query: 'tides' }),\n]);\nconsole.log('searched');\nconst again = await tools.tool_search({ query: 'weather' });\nreturn { same: JSON.stringify(a) === JSON.stringify(again), differs: JSON.stringify(a) !== JSON.stringify(b) };";

#[derive(Default)]
struct Seen {
    description: Option<String>,
    result: Option<String>,
}

struct Model(Arc<Mutex<Seen>>);

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
                self.0.lock().unwrap().result = Some(text);
                vec![]
            }
            _ => {
                let description = request
                    .tools
                    .iter()
                    .flatten()
                    .find(|t| t.name == "run_code")
                    .map(|t| t.description.clone());
                self.0.lock().unwrap().description = description;
                vec![ToolCall {
                    id: "tc-1".to_string(),
                    call_type: "function".to_string(),
                    function: ToolCallFunction {
                        name: "run_code".to_string(),
                        arguments: serde_json::json!({ "code": CODE }).to_string(),
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

struct Provider(Arc<Mutex<Seen>>);

#[async_trait]
impl LlmProviderTrait for Provider {
    async fn resolve(&self, _owner: &SessionOwner) -> Result<Arc<dyn LlmCallable>, String> {
        Ok(Arc::new(Model(self.0.clone())))
    }
}

struct Retry;

#[async_trait]
impl WorkerRetryResolver for Retry {
    async fn resolve(&self, _tenant_id: &str) -> RetryPolicy {
        RetryPolicy::no_retry()
    }
}

fn tool(name: &str, description: &str) -> AgentTool {
    AgentTool {
        name: name.to_string(),
        description: description.to_string(),
        input: None,
        output: None,
        handler: None,
        defer: Some(true),
    }
}

async fn engine(seen: Arc<Mutex<Seen>>) -> (Arc<Runtime>, Arc<PushAdapter>, tempfile::TempDir) {
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
            tools: vec![
                tool("weather", "Today's weather for a city"),
                tool("tides", "High and low tides for a harbour"),
            ],
            defer_tools: Some(DeferTools::default()),
            code_mode: true,
            ..Default::default()
        }),
        hosting: Hosting::Engine,
        events: None,
    };
    let directory = Arc::new(StaticAgentDirectory::new(
        TENANT.to_string(),
        BTreeMap::from([("assistant".to_string(), agent)]),
        LlmBlocks::from_iter([("claude".to_string(), LlmBlock::engine())]),
    ));
    let providers = LlmProviderRegistry::new(BTreeMap::from([(
        "claude".to_string(),
        Arc::new(Provider(seen)) as Arc<dyn LlmProviderTrait>,
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
            worker_queue: Arc::new(SqliteQueue::new(db.clone(), Duration::from_secs(60))),
            session_index_store: Arc::new(SqliteSessionIndexStore::new(db.clone()).unwrap()),
            alarms: Arc::new(SqliteAlarms::new(db.clone())),
            token_delta_transport: Arc::new(InMemoryTokenDeltaTransport::new()),
        },
        RuntimeConfig {
            wake_poll_interval: Duration::from_millis(100),
            worker_retry_resolver: Arc::new(Retry),
            sandbox: Arc::new(Sandbox::new(
                std::path::PathBuf::from(env!("CARGO_BIN_EXE_subs")),
                vec![sandbox::ARG.into()],
            )),
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
    (runtime, adapter, dir)
}

fn caller() -> Caller {
    Caller::System {
        tenant_id: TENANT.to_string(),
    }
}

#[tokio::test]
async fn a_script_calls_tools_and_only_its_result_reaches_the_model() {
    let seen = Arc::new(Mutex::new(Seen::default()));
    let (runtime, _adapter, _dir) = engine(seen.clone()).await;
    let out = runtime
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
        .expect("the input is accepted");
    let mut events = runtime
        .stream(
            SessionSubscriptionSpec {
                session_id: "s-1".to_string(),
                caller: caller(),
                scope: SubscriptionScope::Turn {
                    turn_id: out.turn_id,
                },
            },
            Some(Seq(0)),
        )
        .await
        .expect("streams");
    let mut log = Vec::new();
    let settled = tokio::time::timeout(Duration::from_secs(20), async {
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
    assert!(description.contains("declare const tools"), "{description}");
    assert!(description.contains("tool_search(args:"), "{description}");
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
