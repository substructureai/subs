use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::connectors::registry::Connections;
use crate::protocol::{
    AgentConfigPatch, ClientAction, ClientAppend, ClientInput, ClientMessage, ClientMessages,
    ClientPayload, ErrorCode, ErrorInfo, InterruptResumption, QueueMode, SessionOwner, TokenDelta,
    WorkerRef, WorkerState,
};
use crate::runtime::blob::BlobStore;
use connector::{spawn_connector_task_executor, ConnectorTask};
use event_store::Seq;
use llm::{spawn_llm_task_executor, LlmBlocks, LlmResolver, LlmTask, TokenDeltaTransport};
use queue::Queue;
use retry::{DefaultWorkerRetryResolver, WorkerRetryResolver};
use script::{spawn_script_executor, ScriptTask};
use session::command::{CommandPayload, Outcome, SessionError, SettleError, TurnTarget};
use session::decision::{EffectResultPayload, WorkKind};
use session::index::{SessionFilter, SessionIndexStore, SessionPage, SessionTotals};
use session::read::SessionReader;
use session::state::EffectKind;
use session::subscriptions::SessionSubscriptionSpec;
use session::{execute, ExecuteError, ExecuteInput, Session, SessionEvent};
use span::SpanContext;
use subagent::{spawn_subagent_task_executor, SubagentTask};
use substructure_session::delivery::Restart;
use system1::{spawn_system1_task_executor, System1Blocks, System1Resolver, System1Task};
use wake::{spawn_wake_dispatcher, Alarms};
use worker::{AgentDirectory, FailDecision, SessionSeed, SubmitDecision, WorkerDecisionRequest};

pub mod blob;
pub mod blob_cache;
pub mod events_url;
pub use substructure_session::caller;
pub mod connector;
pub mod consumers;
pub mod event_store;
pub mod executor;
pub mod inbox;
pub mod live;
pub mod llm;
pub mod queue;
pub mod script;
pub use substructure_session::retry;
pub mod secret;
pub mod session;
pub mod sessions;
pub use substructure_session::span;
pub mod subagent;
pub mod system1;
pub mod wake;
pub mod worker;

pub use caller::Caller;

pub struct RuntimeConfig {
    pub llm_executor_workers: usize,
    pub system1_executor_workers: usize,
    pub subagent_executor_workers: usize,
    pub connector_executor_workers: usize,
    pub executor_concurrency: usize,
    pub wake_poll_interval: std::time::Duration,
    pub shutdown_timeout: std::time::Duration,
    pub worker_retry_resolver: Arc<dyn WorkerRetryResolver>,
    pub session: substructure_session::Settings,
    pub send_timeout: std::time::Duration,
    /// Runs `run_code` scripts. A process of this binary by default.
    pub script_runner: Arc<dyn crate::codemode::ScriptRunner>,
    /// Scripts that run at once.
    pub script_slots: usize,
    /// What a script may use.
    pub script_limits: crate::codemode::Limits,
    /// How long a script step may wait for a slot, or for a runner to take it.
    pub script_wait: std::time::Duration,
}

impl Default for RuntimeConfig {
    fn default() -> Self {
        Self {
            llm_executor_workers: 32,
            system1_executor_workers: 16,
            subagent_executor_workers: 32,
            connector_executor_workers: 64,
            executor_concurrency: 32,
            wake_poll_interval: std::time::Duration::from_secs(30),
            shutdown_timeout: std::time::Duration::from_secs(5),
            worker_retry_resolver: Arc::new(DefaultWorkerRetryResolver),
            session: substructure_session::Settings::default(),
            send_timeout: sessions::DEFAULT_SEND_TIMEOUT,
            script_runner: Arc::new(crate::codemode::ProcessRunner::current()),
            script_slots: std::thread::available_parallelism().map_or(2, |n| n.get().clamp(2, 8)),
            script_limits: crate::codemode::Limits::default(),
            script_wait: std::time::Duration::from_secs(60),
        }
    }
}

impl RuntimeConfig {
    fn pool(&self, workers: usize) -> executor::ExecutorPool {
        executor::ExecutorPool {
            workers,
            concurrency: self.executor_concurrency,
        }
    }
}

pub struct Runtime {
    sessions: Arc<sessions::Sessions>,
    decisions: Arc<dyn Queue<WorkerDecisionRequest>>,
    agents: Arc<dyn AgentDirectory>,
    session_index: Arc<dyn SessionIndexStore>,
    session_subscriptions: session::subscriptions::SessionSubscriptions,
    token_delta_transport: Arc<dyn TokenDeltaTransport>,
    cancel: CancellationToken,
    handles: tokio::sync::Mutex<Vec<JoinHandle<()>>>,
    shutdown_timeout: Duration,
    worker_retry_resolver: Arc<dyn WorkerRetryResolver>,
    blobs: Arc<dyn BlobStore>,
}

pub struct SubmitClientPayload {
    pub session_id: String,
    pub caller: Caller,
    pub owner: SessionOwner,
    pub agent_id: String,
    pub payload: ClientPayload,

    pub agent: Option<AgentConfigPatch>,

    pub state: Option<WorkerState>,

    pub worker: Option<WorkerRef>,

    pub turn_id: Option<String>,

    pub continue_turn: bool,

    pub queue: QueueMode,

    pub metadata: BTreeMap<String, String>,
}

pub struct SubmitClientPayloadOutput {
    pub session_id: String,
    pub turn_id: String,

    pub queued: bool,
}

pub struct HandleClientInput {
    pub session_id: String,
    pub caller: Caller,
    pub owner: SessionOwner,
    pub input: ClientInput,
    pub agent: Option<AgentConfigPatch>,
    pub worker: Option<WorkerRef>,
    pub span: SpanContext,
}

pub struct ClientInputOutput {
    pub session_id: String,
    pub turn_id: String,

    pub queued: bool,
}

#[derive(Debug)]
pub enum EffectSettlement {
    Result(EffectResultPayload),
    Error {
        error: String,
        retryable: bool,
        code: Option<ErrorCode>,
        detail: Option<serde_json::Value>,
    },
}

pub struct SettleEffectInput {
    pub session_id: String,
    pub kind: WorkKind,
    pub id: String,

    pub attempt: Option<u32>,
    pub settlement: EffectSettlement,
    pub caller: Caller,
    pub span: SpanContext,
}

pub struct InterruptSessionInput {
    pub session_id: String,
    pub interrupt_id: String,
    pub reason: String,
    pub payload: serde_json::Value,
    pub caller: Caller,
    pub span: SpanContext,
}

pub struct ResumeInterruptInput {
    pub session_id: String,
    pub interrupt_id: String,
    pub payload: serde_json::Value,
    pub caller: Caller,
    pub span: SpanContext,
}

#[derive(Debug, thiserror::Error)]
pub enum RuntimeError {
    #[error(transparent)]
    Session(#[from] SessionError),
    #[error("{0}")]
    Invalid(String),
    #[error("{0}")]
    Internal(String),
}

impl From<ExecuteError> for RuntimeError {
    fn from(e: ExecuteError) -> Self {
        match e {
            ExecuteError::Command(c) => RuntimeError::Session(c),
            other => RuntimeError::Internal(other.to_string()),
        }
    }
}

impl Runtime {
    pub async fn shutdown(&self) {
        self.cancel.cancel();

        let mut guard = self.handles.lock().await;
        let handles: Vec<_> = guard.drain(..).collect();
        drop(guard);

        let abort_handles: Vec<_> = handles.iter().map(|h| h.abort_handle()).collect();

        let join_all = async {
            for handle in handles {
                let _ = handle.await;
            }
        };

        if tokio::time::timeout(self.shutdown_timeout, join_all)
            .await
            .is_err()
        {
            tracing::warn!("shutdown timed out, aborting remaining tasks");
            for handle in &abort_handles {
                handle.abort();
            }
        }
    }

    pub async fn is_open(
        &self,
        tenant_id: &str,
        session_id: &str,
        work: &substructure_session::WorkId,
    ) -> bool {
        self.sessions.is_open(tenant_id, session_id, work).await
    }

    pub async fn settled(&self) {
        self.sessions.settled().await;
    }

    pub fn decisions(&self) -> Arc<dyn Queue<WorkerDecisionRequest>> {
        self.decisions.clone()
    }

    pub async fn create_session(
        &self,
        session_id: &str,
        caller: &Caller,
        agent_id: String,
        owner: SessionOwner,
        seed: SessionSeed,
    ) -> Result<bool, RuntimeError> {
        let worker_retry = self.worker_retry_resolver.resolve(caller.tenant_id()).await;
        let result = execute(
            self.sessions.as_ref(),
            ExecuteInput {
                id: None,
                session_id: session_id.to_string(),
                caller: caller.clone(),
                command: worker::directory::create_session_command(
                    &*self.agents,
                    agent_id,
                    owner,
                    vec![],
                    worker_retry,
                    seed,
                )
                .await
                .map_err(RuntimeError::Invalid)?,
                span: SpanContext::root().child("create_session"),
            },
        )
        .await;

        match result {
            Ok(_) => Ok(true),
            Err(ExecuteError::Command(SessionError::SessionAlreadyCreated)) => Ok(false),
            Err(e) => Err(e.into()),
        }
    }

    pub async fn submit_client_payload(
        &self,
        input: SubmitClientPayload,
    ) -> Result<SubmitClientPayloadOutput, RuntimeError> {
        let mut input = input;
        blob::intern_payload(
            &mut input.payload,
            self.blob_store(),
            input.caller.tenant_id(),
        )
        .await;

        let session_id = input.session_id;
        let turn_id = input.turn_id.unwrap_or_else(|| Uuid::now_v7().to_string());
        let caller = input.caller.clone();

        let span = SpanContext::root();

        self.create_session(
            &session_id,
            &input.caller,
            input.agent_id,
            input.owner,
            SessionSeed {
                agent: input.agent,
                state: input.state,
                worker: input.worker,
                events: None,
            },
        )
        .await?;

        let action = matches!(input.payload, ClientPayload::Action(_));
        let send_result = execute(
            self.sessions.as_ref(),
            ExecuteInput {
                id: None,
                session_id: session_id.clone(),
                caller: input.caller,
                command: CommandPayload::SubmitClientPayload {
                    turn: match (&input.payload, input.continue_turn) {
                        (ClientPayload::Action(_), _) | (_, false) => {
                            TurnTarget::Open(turn_id.clone())
                        }
                        (_, true) => TurnTarget::Continue(turn_id.clone()),
                    },
                    payload: input.payload,
                    queue: input.queue,
                    metadata: input.metadata,
                },
                span: span.child("submit_client_payload"),
            },
        )
        .await;

        let (effective_turn_id, queued) = match send_result {
            Ok(out) => (
                out.turn_id.unwrap_or(turn_id),
                out.queued && !input.continue_turn,
            ),

            Err(ExecuteError::Command(e @ SessionError::TurnAlreadyActive { .. })) if action => {
                return Err(ExecuteError::Command(e).into())
            }
            Err(ExecuteError::Command(SessionError::TurnAlreadyActive { turn_id: held })) => {
                let queued = input.queue.holds()
                    && held == turn_id
                    && !matches!(
                        self.active_turn_id(&caller, &session_id).await,
                        Ok(active) if active == held
                    );
                (held, queued)
            }
            Err(ExecuteError::Command(SessionError::TurnAlreadyCompleted { turn_id })) => {
                (turn_id, false)
            }
            Err(e) => return Err(e.into()),
        };

        Ok(SubmitClientPayloadOutput {
            session_id,
            turn_id: effective_turn_id,
            queued,
        })
    }

    pub async fn handle_client_input(
        &self,
        input: HandleClientInput,
    ) -> Result<ClientInputOutput, RuntimeError> {
        let HandleClientInput {
            session_id,
            caller,
            owner,
            mut input,
            agent,
            worker,
            span,
        } = input;

        match &mut input {
            ClientInput::Message { message, .. } => {
                blob::intern(
                    std::slice::from_mut(message),
                    self.blob_store(),
                    caller.tenant_id(),
                )
                .await
            }
            ClientInput::Messages { messages, .. } | ClientInput::Append { messages, .. } => {
                blob::intern(messages, self.blob_store(), caller.tenant_id()).await
            }
            ClientInput::Action { .. }
            | ClientInput::InterruptResume { .. }
            | ClientInput::ToolResult { .. }
            | ClientInput::ToolError { .. } => {}
        }

        let (agent_id, turn_id, payload, queue) = match input {
            ClientInput::Message {
                agent_id,
                turn_id,
                message,
                stream,
                queue,
            } => (
                agent_id,
                turn_id,
                ClientPayload::Message(ClientMessage { message, stream }),
                queue,
            ),
            ClientInput::Messages {
                agent_id,
                turn_id,
                messages,
                stream,
                client,
            } => (
                agent_id,
                turn_id,
                ClientPayload::Messages(ClientMessages {
                    messages,
                    stream,
                    client,
                }),
                QueueMode::Refuse,
            ),
            ClientInput::Append {
                agent_id,
                turn_id,
                messages,
                stream,
                client,
                queue,
            } => (
                agent_id,
                turn_id,
                ClientPayload::Append(ClientAppend {
                    messages,
                    stream,
                    client,
                }),
                queue,
            ),
            ClientInput::Action {
                agent_id,
                turn_id,
                name,
                args,
            } => (
                agent_id,
                turn_id,
                ClientPayload::Action(ClientAction { name, args }),
                QueueMode::Refuse,
            ),
            ClientInput::InterruptResume {
                resumption:
                    InterruptResumption {
                        interrupt_id,
                        payload,
                    },
            } => {
                let turn_id = self.active_turn_id(&caller, &session_id).await?;
                self.resume_interrupt(ResumeInterruptInput {
                    session_id: session_id.clone(),
                    interrupt_id,
                    payload,
                    caller,
                    span,
                })
                .await?;
                return Ok(ClientInputOutput {
                    session_id,
                    turn_id,
                    queued: false,
                });
            }
            ClientInput::ToolResult {
                id,
                attempt,
                result,
                content,
                structured_content,
                is_error,
            } => {
                let answered = crate::protocol::ToolResult::from_action(
                    result,
                    content,
                    structured_content,
                    is_error,
                )
                .map_err(|e| RuntimeError::Internal(e.to_string()))?;
                return self
                    .settle_client_tool(
                        session_id,
                        caller,
                        span,
                        id,
                        attempt,
                        EffectSettlement::Result(EffectResultPayload::ToolCall {
                            result: answered,
                        }),
                    )
                    .await;
            }
            ClientInput::ToolError {
                id,
                error,
                retryable,
                attempt,
            } => {
                return self
                    .settle_client_tool(
                        session_id,
                        caller,
                        span,
                        id,
                        attempt,
                        EffectSettlement::Error {
                            error,
                            retryable,
                            code: None,
                            detail: None,
                        },
                    )
                    .await;
            }
        };

        let out = self
            .submit_client_payload(SubmitClientPayload {
                session_id,
                caller,
                owner,
                agent_id,
                payload,
                agent,
                state: None,
                worker,
                turn_id,
                continue_turn: false,
                queue,
                metadata: BTreeMap::new(),
            })
            .await?;
        Ok(ClientInputOutput {
            session_id: out.session_id,
            turn_id: out.turn_id,
            queued: out.queued,
        })
    }

    async fn settle_client_tool(
        &self,
        session_id: String,
        caller: Caller,
        span: SpanContext,
        id: String,
        attempt: Option<u32>,
        settlement: EffectSettlement,
    ) -> Result<ClientInputOutput, RuntimeError> {
        let turn_id = self.active_turn_id(&caller, &session_id).await?;
        self.settle_effect(SettleEffectInput {
            session_id: session_id.clone(),
            kind: WorkKind::ToolCall,
            id,
            attempt,
            settlement,
            caller,
            span,
        })
        .await?;
        Ok(ClientInputOutput {
            session_id,
            turn_id,
            queued: false,
        })
    }

    async fn active_turn_id(
        &self,
        caller: &Caller,
        session_id: &str,
    ) -> Result<String, RuntimeError> {
        match self.get_session(caller.tenant_id(), session_id).await {
            Ok(session) => session
                .state()
                .turn_id()
                .map(str::to_string)
                .ok_or(RuntimeError::Session(SessionError::NoActiveTurn)),
            Err(_) => Err(RuntimeError::Session(SessionError::NoActiveTurn)),
        }
    }

    pub async fn stream(
        &self,
        spec: SessionSubscriptionSpec,
        after: Option<Seq>,
    ) -> Result<mpsc::Receiver<SessionEvent>, RuntimeError> {
        self.authorize_session_read(&spec.session_id, &spec.caller)
            .await?;
        Ok(self.session_subscriptions.stream(spec, after).await)
    }

    pub async fn authorize_session_read(
        &self,
        session_id: &str,
        caller: &Caller,
    ) -> Result<(), RuntimeError> {
        self.reader().authorize(session_id, caller).await
    }

    pub fn sessions(&self) -> Arc<sessions::Sessions> {
        self.sessions.clone()
    }

    pub async fn subscribe_token_deltas(
        &self,
        caller: &Caller,
        root_session_id: &str,
    ) -> mpsc::Receiver<TokenDelta> {
        self.token_delta_transport
            .subscribe(caller.tenant_id(), root_session_id)
            .await
    }

    pub fn token_delta_transport(&self) -> Arc<dyn TokenDeltaTransport> {
        self.token_delta_transport.clone()
    }

    pub fn agents(&self) -> Arc<dyn AgentDirectory> {
        self.agents.clone()
    }

    pub async fn llm_blocks(&self, tenant_id: &str) -> LlmBlocks {
        self.agents.tenant(tenant_id).await.llm
    }

    pub async fn system1_blocks(&self, tenant_id: &str) -> System1Blocks {
        self.agents.tenant(tenant_id).await.system1
    }

    pub fn reader(&self) -> SessionReader {
        SessionReader::new(self.sessions.clone(), self.session_index.clone())
    }

    pub async fn list_sessions(&self, filter: &SessionFilter) -> Result<SessionPage, RuntimeError> {
        self.reader().list(filter).await
    }

    pub async fn count_sessions(&self, filter: &SessionFilter) -> Result<u64, RuntimeError> {
        self.reader().count(filter).await
    }

    pub async fn aggregate_sessions(
        &self,
        filter: &SessionFilter,
    ) -> Result<SessionTotals, RuntimeError> {
        self.reader().aggregate(filter).await
    }

    pub async fn get_session(
        &self,
        tenant_id: &str,
        session_id: &str,
    ) -> Result<Session, RuntimeError> {
        self.reader().session(tenant_id, session_id).await
    }

    pub async fn resume_delivery(
        &self,
        tenant_id: &str,
        session_id: &str,
        reader: &str,
    ) -> Result<bool, RuntimeError> {
        self.sessions
            .resume_delivery(tenant_id, session_id, reader)
            .await
            .map_err(|e| RuntimeError::Internal(e.to_string()))
    }

    pub async fn whole_tree(
        &self,
        tenant_id: &str,
        session_id: &str,
        head_id: Option<String>,
    ) -> Result<crate::protocol::MessageTree, RuntimeError> {
        self.sessions
            .whole_tree(tenant_id, session_id, head_id)
            .await
            .map_err(|e| RuntimeError::Internal(e.to_string()))
    }

    pub async fn read_session_events(
        &self,
        caller: &Caller,
        session_id: &str,
        after: Option<Seq>,
        limit: Option<usize>,
    ) -> Result<Vec<SessionEvent>, RuntimeError> {
        self.reader().events(caller, session_id, after, limit).await
    }

    pub async fn submit_decision(&self, input: SubmitDecision) -> Result<(), RuntimeError> {
        execute(
            self.sessions.as_ref(),
            ExecuteInput {
                id: None,
                session_id: input.session_id.clone(),
                caller: input.caller,
                command: CommandPayload::SubmitWorkerDecision {
                    decision_id: input.decision_id,
                    transcript: input.transcript,
                    actions: input.actions,
                    state: input.state,
                    agent: input.agent,
                },
                span: input.span,
            },
        )
        .await
        .map(|_| ())
        .map_err(RuntimeError::from)
    }

    pub(crate) fn blob_store(&self) -> &dyn BlobStore {
        self.blobs.as_ref()
    }

    pub(crate) async fn stored(
        &self,
        result: crate::protocol::ToolResult,
        tenant_id: &str,
    ) -> crate::protocol::StoredResult {
        blob::store(result, self.blob_store(), tenant_id).await
    }

    pub async fn settle_effect(&self, input: SettleEffectInput) -> Result<(), RuntimeError> {
        let outcome = match input.settlement {
            EffectSettlement::Result(EffectResultPayload::ToolCall { result }) => Outcome::Tool {
                result: self.stored(result, input.caller.tenant_id()).await,
            },
            EffectSettlement::Result(EffectResultPayload::LlmCall { response }) => {
                Outcome::Llm(response)
            }

            EffectSettlement::Error {
                error,
                retryable,
                code,
                detail,
            } => SettleError::new(
                ErrorInfo::handler(error).or_code(code).or_detail(detail),
                retryable,
            )
            .into(),
        };
        let kind = match input.kind {
            WorkKind::ToolCall => EffectKind::ToolCall,
            WorkKind::LlmCall => EffectKind::LlmCall,
        };
        let command = CommandPayload::settle(kind, input.id, input.attempt, outcome);

        execute(
            self.sessions.as_ref(),
            ExecuteInput {
                id: None,
                session_id: input.session_id,
                caller: input.caller,
                command,
                span: input.span,
            },
        )
        .await
        .map(|_| ())
        .map_err(RuntimeError::from)
    }

    pub async fn interrupt_session(
        &self,
        input: InterruptSessionInput,
    ) -> Result<(), RuntimeError> {
        execute(
            self.sessions.as_ref(),
            ExecuteInput {
                id: None,
                session_id: input.session_id,
                caller: input.caller,
                command: CommandPayload::Interrupt {
                    interrupt_id: input.interrupt_id,
                    reason: input.reason,
                    payload: input.payload,
                },
                span: input.span,
            },
        )
        .await
        .map(|_| ())
        .map_err(RuntimeError::from)
    }

    pub async fn cancel_session(
        &self,
        session_id: String,
        caller: Caller,
        span: SpanContext,
    ) -> Result<(), RuntimeError> {
        execute(
            self.sessions.as_ref(),
            ExecuteInput {
                id: None,
                session_id,
                caller,
                command: CommandPayload::CancelSession,
                span,
            },
        )
        .await
        .map(|_| ())
        .map_err(RuntimeError::from)
    }

    pub async fn resume_interrupt(&self, input: ResumeInterruptInput) -> Result<(), RuntimeError> {
        execute(
            self.sessions.as_ref(),
            ExecuteInput {
                id: None,
                session_id: input.session_id,
                caller: input.caller,
                command: CommandPayload::ResumeInterrupt {
                    interrupt_id: input.interrupt_id,
                    payload: input.payload,
                },
                span: input.span,
            },
        )
        .await
        .map(|_| ())
        .map_err(RuntimeError::from)
    }

    pub async fn fail_decision(&self, input: FailDecision) -> Result<(), RuntimeError> {
        execute(
            self.sessions.as_ref(),
            ExecuteInput {
                id: None,
                session_id: input.session_id.clone(),
                caller: input.caller,
                command: CommandPayload::settle(
                    EffectKind::Decision,
                    input.decision_id,
                    input.attempt,
                    SettleError::new(input.error, input.retryable),
                ),
                span: input.span,
            },
        )
        .await
        .map(|_| ())
        .map_err(RuntimeError::from)
    }
}

pub struct RuntimeDeps {
    pub sessions: Arc<dyn sessions::SessionStores>,
    pub readers: Vec<Arc<dyn sessions::Reader>>,
    pub secrets: Arc<dyn crate::runtime::secret::SecretStore>,

    pub agents: Arc<dyn AgentDirectory>,

    pub llm: Arc<dyn LlmResolver>,
    pub llm_task_queue: Arc<dyn Queue<LlmTask>>,
    pub system1: Arc<dyn System1Resolver>,
    pub system1_task_queue: Arc<dyn Queue<System1Task>>,
    pub subagent_task_queue: Arc<dyn Queue<SubagentTask>>,
    pub connections: Option<Arc<Connections>>,

    pub plugins: Arc<dyn crate::plugins::PluginResolver>,
    pub connector_task_queue: Arc<dyn Queue<ConnectorTask>>,
    pub script_task_queue: Arc<dyn Queue<ScriptTask>>,
    pub worker_queue: Arc<dyn Queue<WorkerDecisionRequest>>,
    pub session_index_store: Arc<dyn SessionIndexStore>,
    pub alarms: Arc<dyn Alarms>,
    pub token_delta_transport: Arc<dyn TokenDeltaTransport>,
    pub blobs: Arc<dyn BlobStore>,
    pub outbound: Option<reqwest::Client>,
}

pub async fn start(deps: RuntimeDeps, config: RuntimeConfig) -> Arc<Runtime> {
    let RuntimeDeps {
        sessions: stores,
        readers: outside,
        secrets,
        agents,
        llm,
        llm_task_queue,
        system1,
        system1_task_queue,
        subagent_task_queue,
        connections,
        plugins,
        connector_task_queue,
        script_task_queue,
        worker_queue,
        session_index_store,
        alarms,
        token_delta_transport,
        blobs,
        outbound,
    } = deps;
    let cancel = CancellationToken::new();
    let tap = Arc::new(live::LocalTap::new(token_delta_transport));
    let token_delta_transport: Arc<dyn TokenDeltaTransport> = tap.clone();

    let work = Arc::new(consumers::WorkRouter {
        llm: (!llm.is_empty()).then(|| llm_task_queue.clone()),
        system1: (!system1.is_empty()).then(|| system1_task_queue.clone()),
        connector: connector_task_queue.clone(),
        scripts: script_task_queue.clone(),
        subagent: subagent_task_queue.clone(),
        decisions: worker_queue.clone(),
        restart: Restart::ResendOpen,
        agents: agents.clone(),
        blobs: blobs.clone(),
        plugins: plugins.clone(),
    });
    let readers: Vec<Arc<dyn sessions::Reader>> = vec![
        Arc::new(consumers::IndexReader(session_index_store.clone())),
        Arc::new(consumers::MailReader),
    ];
    let events = Arc::new(events_url::EventsReader::new(
        agents.clone(),
        secrets,
        outbound,
    ));
    let mut outside: Vec<Arc<dyn sessions::Reader>> = outside;
    outside.push(events.clone());
    let sessions = sessions::Sessions::new(
        stores,
        alarms.clone(),
        readers,
        outside,
        Some(work),
        sessions::SessionsConfig {
            session: config.session.clone(),
            send_timeout: config.send_timeout,
        },
    );
    let behind = wake::behind_at_boot(&sessions, alarms.as_ref()).await;

    let mut handles: Vec<tokio::task::JoinHandle<()>> = Vec::new();
    if !llm.is_empty() {
        handles.extend(spawn_llm_task_executor(
            sessions.clone(),
            llm,
            llm_task_queue,
            token_delta_transport.clone(),
            blobs.clone(),
            config.pool(config.llm_executor_workers),
            cancel.clone(),
        ));
    }
    if !system1.is_empty() {
        handles.extend(spawn_system1_task_executor(
            sessions.clone(),
            system1,
            system1_task_queue,
            blobs.clone(),
            config.pool(config.system1_executor_workers),
            cancel.clone(),
        ));
    }
    handles.extend(spawn_script_executor(
        sessions.clone(),
        Arc::new(script::Scripts {
            runner: config.script_runner.clone(),
            admission: Arc::new(crate::codemode::Admission::new(config.script_slots)),
            limits: config.script_limits.clone(),
            wait: config.script_wait,
            blobs: blobs.clone(),
            plugins: plugins.clone(),
        }),
        script_task_queue,
        cancel.clone(),
    ));
    handles.extend(spawn_connector_task_executor(
        sessions.clone(),
        connections,
        connector_task_queue,
        config.pool(config.connector_executor_workers),
        cancel.clone(),
    ));
    handles.extend(spawn_subagent_task_executor(
        sessions.clone(),
        agents.clone(),
        subagent_task_queue,
        config.pool(config.subagent_executor_workers),
        cancel.clone(),
    ));
    handles.push(live::spawn_live(
        sessions.clone(),
        &tap,
        events,
        live::EVERY,
        cancel.clone(),
    ));
    handles.push(spawn_wake_dispatcher(
        sessions.clone(),
        alarms,
        config.wake_poll_interval,
        cancel.clone(),
    ));

    sessions.resume(behind).await;
    let session_subscriptions = session::subscriptions::SessionSubscriptions::new(sessions.clone());

    Arc::new(Runtime {
        sessions,
        decisions: worker_queue,
        agents,
        session_index: session_index_store,
        session_subscriptions,
        token_delta_transport,
        cancel,
        handles: tokio::sync::Mutex::new(handles),
        shutdown_timeout: config.shutdown_timeout,
        worker_retry_resolver: config.worker_retry_resolver,
        blobs,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_worker_count_is_at_least_one() {
        let config = RuntimeConfig::default();
        for workers in [
            config.llm_executor_workers,
            config.system1_executor_workers,
            config.subagent_executor_workers,
            config.connector_executor_workers,
        ] {
            assert!(workers >= 1, "a subsystem with no worker drains nothing");
        }
    }
}
