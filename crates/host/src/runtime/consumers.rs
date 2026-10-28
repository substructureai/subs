use std::sync::Arc;

use async_trait::async_trait;
use substructure_session::delivery::{AtLimit, Restart, Start, Subscription, Unsent, WorkReader};
use substructure_session::{Session, ToolTarget, Work, WorkId};

use crate::plugins::PluginResolver;
use crate::runtime::blob::BlobStore;
use crate::runtime::connector::{answer_locally, settle_call, ConnectorTask};
use crate::runtime::llm::LlmTask;
use crate::runtime::queue::{self, Envelope, Queue};
use crate::runtime::session::index::{SessionActivity, SessionIndexRecord, SessionIndexStore};
use crate::runtime::session::SessionEvent;
use crate::runtime::sessions::{DeliveryError, Reader, Sessions, WorkSink};
use crate::runtime::span::SpanContext;
use crate::runtime::subagent::SubagentTask;
use crate::runtime::system1::System1Task;
use crate::runtime::worker::handler::Unready;
use crate::runtime::worker::{handler, AgentDirectory, WorkerDecisionRequest};

pub struct WorkRouter {
    pub llm: Option<Arc<dyn Queue<LlmTask>>>,
    pub system1: Option<Arc<dyn Queue<System1Task>>>,
    pub connector: Arc<dyn Queue<ConnectorTask>>,
    pub subagent: Arc<dyn Queue<SubagentTask>>,
    pub decisions: Arc<dyn Queue<WorkerDecisionRequest>>,
    pub restart: Restart,
    pub agents: Arc<dyn AgentDirectory>,
    pub blobs: Arc<dyn BlobStore>,
    pub plugins: Arc<dyn PluginResolver>,
}

#[async_trait]
impl WorkSink for WorkRouter {
    fn reader(&self) -> WorkReader {
        WorkReader::new("work", self.restart)
    }

    async fn send(&self, sessions: &Sessions, session: &Session, work: Vec<Work>) -> Vec<Unsent> {
        let mut unsent = Vec::new();
        for item in work {
            let id = item.id().clone();
            if let Err(failed) = self.route(sessions, session, &id, item).await {
                tracing::warn!(work = %id, error = %failed.error, permanent = failed.permanent, "sending work failed");
                unsent.push(Unsent {
                    work: id,
                    error: failed.error,
                    permanent: failed.permanent,
                });
            }
        }
        unsent
    }
}

impl WorkRouter {
    async fn route(
        &self,
        sessions: &Sessions,
        session: &Session,
        id: &WorkId,
        work: Work,
    ) -> Result<(), Unready> {
        let tenant_id = session.tenant_id().to_string();
        let session_id = session.id().to_string();
        let span = SpanContext::root();
        match work {
            Work::CallLlm { call, .. } => {
                let Some(queue) = &self.llm else {
                    return Ok(());
                };
                let call = *call;
                let task = LlmTask {
                    session_id: session_id.clone(),
                    tenant_id,
                    agent_id: call.agent_id,
                    call_id: call.call_id,
                    attempt: call.attempt,
                    llm: call.llm,
                    request: call.request,
                    defer_tools_strategy: call.defer_tools_strategy,
                    stream: call.stream,
                    owner: call.owner,
                    ancestry: call.ancestry,
                    turn_id: call.turn_id,
                    retry: call.retry,
                };
                send(queue.as_ref(), queue::LLM, session_id, id, span, task).await
            }
            Work::CallSystem1 { call, .. } => {
                let Some(queue) = &self.system1 else {
                    return Ok(());
                };
                let call = *call;
                let task = System1Task {
                    session_id: session_id.clone(),
                    tenant_id,
                    agent_id: call.agent_id,
                    call_id: call.call_id,
                    attempt: call.attempt,
                    system1: call.system1,
                    request: call.request,
                    owner: call.owner,
                    retry: call.retry,
                };
                send(queue.as_ref(), queue::SYSTEM1, session_id, id, span, task).await
            }
            Work::CallTool { call, .. } => match call.target {
                ToolTarget::Remote {
                    path,
                    name,
                    arguments,
                    requester,
                } => {
                    let task = ConnectorTask::CallTool {
                        session_id: session_id.clone(),
                        tenant_id,
                        tool_call_id: call.tool_call_id,
                        attempt: call.attempt,
                        connection_id: path,
                        requester,
                        remote_name: name,
                        arguments,
                        retry: call.retry,
                    };
                    let queue = self.connector.as_ref();
                    send(queue, queue::CONNECTOR, session_id, id, span, task).await
                }
                ToolTarget::Script => {
                    let task = ConnectorTask::RunScript {
                        session_id: session_id.clone(),
                        tenant_id,
                        tool_call_id: call.tool_call_id,
                        attempt: call.attempt,
                        retry: call.retry,
                    };
                    let envelope = uuid::Uuid::now_v7().to_string();
                    self.connector
                        .send(Envelope::new(
                            queue::SCRIPT,
                            session_id,
                            envelope,
                            span,
                            task,
                        ))
                        .await
                        .map_err(|e| transient(e.to_string()))
                }
                ToolTarget::Local => {
                    let Some(answer) = answer_locally(
                        session,
                        &call.tool_call_id,
                        self.blobs.as_ref(),
                        self.plugins.as_ref(),
                    )
                    .await
                    else {
                        tracing::error!(%session_id, tool_call_id = %call.tool_call_id, "no local answer for a call routed to the engine");
                        return Ok(());
                    };
                    let command = settle_call(call.tool_call_id, call.attempt, Ok(answer));
                    crate::runtime::executor::settled(
                        sessions,
                        &tenant_id,
                        &session_id,
                        command,
                        span.child("connector_answer"),
                    )
                    .await
                    .map_err(transient)
                }
            },
            Work::ListTools {
                work,
                path,
                requester,
                retry,
            } => {
                let task = ConnectorTask::Sync {
                    session_id: session_id.clone(),
                    tenant_id,
                    connection_id: path,
                    requester,
                    attempt: work.attempt.unwrap_or_default(),
                    retry,
                };
                let queue = self.connector.as_ref();
                send(queue, queue::CONNECTOR, session_id, id, span, task).await
            }
            Work::AskWorker {
                request, prompt, ..
            } => {
                let request = handler::finish(
                    *request,
                    prompt.map(|p| *p),
                    self.agents.as_ref(),
                    self.blobs.as_ref(),
                )
                .await?;
                let topic = queue::decisions(&tenant_id);
                let span = request.span.clone();
                send(
                    self.decisions.as_ref(),
                    &topic,
                    session_id,
                    id,
                    span,
                    request,
                )
                .await
            }
            Work::StartSubagent { work, subagent } => {
                let subagent = *subagent;
                let task = SubagentTask {
                    parent_session_id: session_id.clone(),
                    tenant_id,
                    tool_call_id: work.id,
                    attempt: work.attempt.unwrap_or_default(),
                    child_session_id: subagent.child,
                    agent_id: subagent.agent_id,
                    owner: subagent.owner,
                    ancestry: subagent.ancestry,
                    message: subagent.message,
                    retry: subagent.retry,
                    events: subagent.events,
                };
                let queue = self.subagent.as_ref();
                send(queue, queue::SUBAGENT, session_id, id, span, task).await
            }
        }
    }
}

fn transient(error: String) -> Unready {
    Unready {
        error,
        permanent: false,
    }
}

async fn send<T: Send + 'static>(
    queue: &dyn Queue<T>,
    topic: &str,
    key: String,
    id: &WorkId,
    span: SpanContext,
    task: T,
) -> Result<(), Unready> {
    queue
        .send(Envelope::new(topic, key, id.to_string(), span, task))
        .await
        .map_err(|e| transient(e.to_string()))
}

pub struct IndexReader(pub Arc<dyn SessionIndexStore>);

#[async_trait]
impl Reader for IndexReader {
    fn subscription(&self) -> Subscription {
        Subscription::new("index", Start::Beginning, AtLimit::Skip)
    }

    async fn deliver(
        &self,
        _sessions: &Sessions,
        session: &Session,
        _events: &[SessionEvent],
    ) -> Result<(), DeliveryError> {
        let summary = session.summary();
        self.0
            .upsert_session_index(SessionIndexRecord {
                tenant_id: summary.tenant_id,
                session_id: summary.session_id,
                seq: summary.seq,
                first_event_at: summary.first_event_at,
                last_event_at: summary.last_event_at,
                wake_at: summary.wake_at,
                top_level: summary.top_level,
                agent_id: summary.agent_id,
                cost: summary.cost,
                subagent_cost: summary.subagent_cost,
                activity: SessionActivity::of(
                    &summary.status,
                    summary.running,
                    summary.last_turn_failed,
                ),
                status: summary.status,
                turn_id: summary.turn_id,
                title: summary.title,
                user_id: summary.user_id,
                turns: summary.turns,
                input_tokens: summary.usage.input,
                output_tokens: summary.usage.output,
                cache_read_tokens: summary.usage.cache_read,
                total_tokens: summary.usage.total,
            })
            .await
            .map_err(DeliveryError::from)
    }
}

pub struct MailReader;

#[async_trait]
impl Reader for MailReader {
    fn subscription(&self) -> Subscription {
        Subscription::new("mail", Start::Beginning, AtLimit::Skip)
    }

    async fn deliver(
        &self,
        sessions: &Sessions,
        session: &Session,
        events: &[SessionEvent],
    ) -> Result<(), DeliveryError> {
        for event in events {
            for mail in session.mail(std::slice::from_ref(event)) {
                let _answered = sessions
                    .run(session.tenant_id(), &mail.to, mail.command)
                    .await
                    .map_err(|e| DeliveryError::retry(Some(event.seq), e.to_string()))?;
            }
        }
        Ok(())
    }
}
