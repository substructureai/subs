use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use tokio::sync::Semaphore;
use tokio::task::JoinHandle;

use crate::protocol::{DecisionResponse, ErrorCode};
use crate::runtime::queue::{self, holding, Received};
use crate::runtime::secret::{get_or_mint, SecretPath, SecretStore};
use crate::runtime::session::state::EffectKind;
use crate::session::wire::resolve_response;
use crate::transport::http_push::HttpDecider;
use crate::worker::push::{Decider, PushError};
use crate::worker::{AgentDirectory, FailDecision, Route, SubmitDecision, WorkerDecisionRequest};
use crate::{Caller, Runtime, RuntimeError};
use substructure_session::WorkId;

const NOT_SAVED: std::time::Duration = std::time::Duration::from_secs(5);

pub struct PushAdapter {
    pub runtime: Arc<Runtime>,
    router: Arc<Router>,
    handles: Mutex<HashMap<String, JoinHandle<()>>>,
    semaphore: Arc<Semaphore>,
}

impl PushAdapter {
    pub fn new(
        runtime: Arc<Runtime>,
        agents: Arc<dyn AgentDirectory>,
        secrets: Arc<dyn SecretStore>,
        concurrency: usize,
    ) -> Self {
        Self {
            runtime,
            router: Arc::new(Router {
                agents,
                secrets,
                http: HttpDecider::client(),
            }),
            handles: Mutex::new(HashMap::new()),
            semaphore: Arc::new(Semaphore::new(concurrency)),
        }
    }

    pub async fn start(&self) {
        for tenant_id in self.router.agents.tenants().await {
            self.spawn_loop(tenant_id);
        }
    }

    fn spawn_loop(&self, tenant_id: String) {
        let mut handles = self.handles.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(existing) = handles.get(&tenant_id) {
            existing.abort();
        }

        let runtime = self.runtime.clone();
        let router = self.router.clone();
        let semaphore = self.semaphore.clone();
        let tid = tenant_id.clone();

        let handle = tokio::spawn(async move {
            let topic = queue::decisions(&tenant_id);
            let decisions = runtime.decisions();
            loop {
                let Received { envelope, receipt } = match decisions.receive(&topic).await {
                    Ok(received) => received,
                    Err(error) => {
                        tracing::warn!(%topic, %error, "receiving a decision failed");
                        tokio::time::sleep(NOT_SAVED).await;
                        continue;
                    }
                };
                let permit = match semaphore.clone().acquire_owned().await {
                    Ok(p) => p,
                    Err(_) => return,
                };
                let router = router.clone();
                let runtime = runtime.clone();
                let decisions = decisions.clone();
                tokio::spawn(async move {
                    let _permit = permit;
                    let mut decision = envelope.task;
                    decision.span = envelope.span;
                    let work = WorkId::new(
                        EffectKind::Decision,
                        decision.decision_id.clone(),
                        Some(decision.attempts),
                    );
                    let open = runtime
                        .is_open(decision.tenant_id(), &decision.session_id, &work)
                        .await;
                    let decided = match open {
                        true => {
                            holding(
                                decisions.as_ref(),
                                &receipt,
                                decide(&runtime, &router, decision),
                            )
                            .await
                        }
                        false => Ok(()),
                    };
                    let finished = match decided {
                        Ok(()) => decisions.ack(&receipt).await,
                        Err(error) => {
                            tracing::warn!(id = %envelope.id, %error, "a decision's outcome was not saved; it is pushed again");
                            decisions.retry(&receipt, NOT_SAVED).await
                        }
                    };
                    if let Err(error) = finished {
                        tracing::warn!(id = %envelope.id, %error, "finishing a decision failed");
                    }
                });
            }
        });

        handles.insert(tid, handle);
    }
}

struct Router {
    agents: Arc<dyn AgentDirectory>,
    secrets: Arc<dyn SecretStore>,
    http: reqwest::Client,
}

impl Router {
    async fn route(
        &self,
        tenant_id: &str,
        decision: &WorkerDecisionRequest,
    ) -> Result<Route, String> {
        self.agents.tenant(tenant_id).await.route(decision)
    }

    async fn transport(
        &self,
        tenant_id: &str,
        id: &str,
        url: String,
    ) -> Result<HttpDecider, PushError> {
        let path = SecretPath::Worker(id.to_string());
        let secret = get_or_mint(&*self.secrets, tenant_id, &path.secret_ref())
            .await
            .map_err(|e| {
                PushError::retryable(
                    ErrorCode::Internal,
                    format!("reading the signing secret for worker.{id} failed: {e}"),
                )
            })?;
        Ok(HttpDecider::new(self.http.clone(), url, secret))
    }
}

fn saved(result: Result<(), RuntimeError>) -> Result<(), String> {
    match result {
        Ok(()) | Err(RuntimeError::Session(_) | RuntimeError::Invalid(_)) => Ok(()),
        Err(RuntimeError::Internal(error)) => Err(error),
    }
}

async fn decide(
    runtime: &Runtime,
    router: &Router,
    decision: WorkerDecisionRequest,
) -> Result<(), String> {
    let tenant_id = decision.tenant_id().to_string();

    match router.route(&tenant_id, &decision).await {
        Err(e) => {
            tracing::warn!(agent_id = %decision.agent_id, error = %e, "decision is unroutable");
            let failure = PushError::fatal(ErrorCode::Unroutable, e);
            record_failure(runtime, &decision, &tenant_id, failure).await
        }
        Ok(Route::Engine) => decide_in_engine(runtime, decision, &tenant_id).await,
        Ok(Route::Worker { id, url }) => {
            let transport = match router.transport(&tenant_id, &id, url).await {
                Ok(t) => t,
                Err(e) => {
                    tracing::warn!(decision_id = %decision.decision_id, error = %e, "no transport");
                    return record_failure(runtime, &decision, &tenant_id, e).await;
                }
            };
            let token_delta_transport = runtime.token_delta_transport();
            match transport.push(&decision, token_delta_transport).await {
                Ok(resp) => submit(runtime, decision, &tenant_id, resp, "push_worker").await,
                Err(e) => {
                    tracing::warn!(
                        decision_id = %decision.decision_id,
                        error = %e,
                        code = ?e.error.code,
                        param = ?e.error.param,
                        "push dispatch failed"
                    );
                    record_failure(runtime, &decision, &tenant_id, e).await
                }
            }
        }
    }
}

async fn decide_in_engine(
    runtime: &Runtime,
    decision: WorkerDecisionRequest,
    tenant_id: &str,
) -> Result<(), String> {
    let span = decision.span.child("engine_decide");
    if decision.proposed.authors_nothing() {
        let error = format!(
            "the engine has no proposal for `{}`, and no worker is attached to agent `{}`",
            decision.trigger.kind(),
            decision.agent_id
        );
        tracing::warn!(decision_id = %decision.decision_id, %error, "engine cannot decide");
        let failure = PushError::fatal(ErrorCode::Unroutable, error);
        return record_failure(runtime, &decision, tenant_id, failure).await;
    }
    tracing::debug!(
        parent: None,
        decision_id = %decision.decision_id,
        agent_id = %decision.agent_id,
        trigger = decision.trigger.kind(),
        traceparent = %span.traceparent(),
        "deciding in engine"
    );
    let proposed = decision.proposed.clone();
    submit(runtime, decision, tenant_id, proposed, "engine_decide").await
}

async fn record_failure(
    runtime: &Runtime,
    decision: &WorkerDecisionRequest,
    tenant_id: &str,
    error: PushError,
) -> Result<(), String> {
    let failed = runtime
        .fail_decision(FailDecision {
            session_id: decision.session_id.clone(),
            caller: Caller::System {
                tenant_id: tenant_id.to_string(),
            },
            decision_id: decision.decision_id.clone(),
            attempt: Some(decision.attempts),
            error: error.error,
            retryable: error.retryable,
            span: decision.span.child("push_fail_decision"),
        })
        .await;
    if let Err(e) = &failed {
        tracing::warn!(
            decision_id = %decision.decision_id,
            error = %e,
            "recording worker decision failure failed"
        );
    }
    saved(failed)
}

async fn submit(
    runtime: &Runtime,
    decision: WorkerDecisionRequest,
    tenant_id: &str,
    response: DecisionResponse,
    span_name: &str,
) -> Result<(), String> {
    let resolved = match resolve_response(
        response,
        decision.agent.as_ref(),
        Some(&decision.trigger),
        &runtime.llm_blocks(tenant_id).await,
        runtime.blob_store(),
        tenant_id,
    )
    .await
    {
        Ok(resolved) => resolved,
        Err(e) => {
            tracing::warn!(
                decision_id = %decision.decision_id,
                error = %e,
                "unresolvable decision"
            );
            let mut failure =
                PushError::fatal(ErrorCode::InvalidResponse, e.to_string()).with_detail(e.detail());
            if let Some(param) = e.param() {
                failure = failure.with_param(param);
            }
            return record_failure(runtime, &decision, tenant_id, failure).await;
        }
    };
    let submit = SubmitDecision {
        session_id: decision.session_id,
        caller: Caller::System {
            tenant_id: tenant_id.to_string(),
        },
        decision_id: decision.decision_id.clone(),
        transcript: resolved.messages,
        actions: resolved.actions,
        state: resolved.state,
        agent: resolved.agent,
        span: decision.span.child(span_name),
    };
    let submitted = runtime.submit_decision(submit).await;
    if let Err(e) = &submitted {
        tracing::warn!(
            decision_id = %decision.decision_id,
            error = %e,
            "decision submit failed"
        );
    }
    saved(submitted)
}
