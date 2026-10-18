use std::collections::BTreeMap;

use crate::protocol::{AgentConfig, RetryPolicy, SessionOwner, WorkerRef, WorkerState};
use crate::runtime::events_url::EventsBlock;
use crate::runtime::llm::LlmBlocks;
use crate::runtime::session::command::CommandPayload;
use crate::runtime::worker::WorkerDecisionRequest;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Hosting {
    Engine,
    Worker(String),
}

#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct WorkerBlock {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
}

#[derive(Debug, Clone)]
pub struct AgentEntry {
    pub config: Option<AgentConfig>,
    pub hosting: Hosting,
    pub events: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Route {
    Worker { id: String, url: String },
    Engine,
}

#[derive(Debug, Clone, Default)]
pub struct TenantDirectory {
    pub agents: BTreeMap<String, AgentEntry>,
    pub workers: BTreeMap<String, WorkerBlock>,
    pub events: BTreeMap<String, EventsBlock>,
    pub default_worker: Option<String>,
    pub llm: LlmBlocks,
}

impl TenantDirectory {
    pub fn agent(&self, agent_id: &str) -> Option<&AgentEntry> {
        self.agents.get(agent_id)
    }

    pub fn declares(&self, agent_id: &str) -> bool {
        self.agents.contains_key(agent_id)
    }

    pub fn worker(&self, id: &str) -> Option<&WorkerBlock> {
        self.workers.get(id)
    }

    pub fn agent_ids(&self) -> Vec<String> {
        self.agents.keys().cloned().collect()
    }

    pub fn resolve_worker(&self, agent_id: &str, named: Option<WorkerRef>) -> Option<WorkerRef> {
        named.or_else(|| match self.declares(agent_id) {
            true => None,
            false => self
                .default_worker
                .clone()
                .map(|id| WorkerRef { id, url: None }),
        })
    }

    pub fn route(&self, decision: &WorkerDecisionRequest) -> Result<Route, String> {
        let agent_id = &decision.agent_id;
        let hosting = match &decision.worker {
            Some(w) => Some(Hosting::Worker(w.id.clone())),
            None => self.agent(agent_id).map(|e| e.hosting.clone()),
        };
        let worker_id = match hosting {
            Some(Hosting::Engine) => return Ok(Route::Engine),
            Some(Hosting::Worker(id)) => id,
            None if decision.agent.is_some() => return Ok(Route::Engine),
            None => {
                return Err(format!(
                    "no [agent.{agent_id}], no worker on the session, and no config on the \
                     session. Declared agents: {}",
                    crate::copy::declared(self.agent_ids())
                ))
            }
        };
        let Some(block) = self.worker(&worker_id) else {
            return Err(format!("no [worker.{worker_id}] in subs.toml"));
        };
        let Some(url) = decision
            .worker
            .as_ref()
            .and_then(|w| w.url.clone())
            .or_else(|| block.url.clone())
        else {
            return Err(format!(
                "[worker.{worker_id}] has no `url` and the session brought none"
            ));
        };
        Ok(Route::Worker { id: worker_id, url })
    }
}

#[async_trait::async_trait]
pub trait AgentDirectory: Send + Sync {
    async fn tenant(&self, tenant_id: &str) -> TenantDirectory;

    async fn tenants(&self) -> Vec<String>;

    async fn events(&self, tenant_id: &str, id: &str) -> Result<Option<EventsBlock>, String> {
        Ok(self.tenant(tenant_id).await.events.get(id).cloned())
    }
}

#[derive(Debug, Clone, Default)]
pub struct SessionSeed {
    pub agent: Option<AgentConfig>,
    pub state: Option<WorkerState>,
    pub worker: Option<WorkerRef>,
    pub events: Option<String>,
}

pub async fn create_session_command(
    agents: &dyn AgentDirectory,
    agent_id: String,
    owner: SessionOwner,
    ancestry: Vec<String>,
    worker_retry: RetryPolicy,
    seed: SessionSeed,
) -> CommandPayload {
    let directory = agents.tenant(&owner.tenant_id).await;
    CommandPayload::CreateSession {
        worker: directory.resolve_worker(&agent_id, seed.worker),
        events: directory
            .agent(&agent_id)
            .and_then(|entry| entry.events.clone())
            .or(seed.events),
        agent_id,
        owner,
        ancestry,
        worker_retry,
        agent: seed.agent.map(Box::new),
        state: seed.state,
    }
}

pub struct StaticAgentDirectory {
    tenant_id: String,
    directory: TenantDirectory,
}

impl StaticAgentDirectory {
    pub fn new(tenant_id: String, agents: BTreeMap<String, AgentEntry>, llm: LlmBlocks) -> Self {
        Self {
            tenant_id,
            directory: TenantDirectory {
                agents,
                llm,
                ..Default::default()
            },
        }
    }

    pub fn with_workers(
        mut self,
        workers: BTreeMap<String, WorkerBlock>,
        default_worker: Option<String>,
    ) -> Self {
        self.directory.workers = workers;
        self.directory.default_worker = default_worker;
        self
    }

    pub fn with_events(mut self, events: BTreeMap<String, EventsBlock>) -> Self {
        self.directory.events = events;
        self
    }
}

#[async_trait::async_trait]
impl AgentDirectory for StaticAgentDirectory {
    async fn tenant(&self, tenant_id: &str) -> TenantDirectory {
        match tenant_id == self.tenant_id {
            true => self.directory.clone(),
            false => TenantDirectory::default(),
        }
    }

    async fn tenants(&self) -> Vec<String> {
        vec![self.tenant_id.clone()]
    }

    async fn events(&self, tenant_id: &str, id: &str) -> Result<Option<EventsBlock>, String> {
        Ok(match tenant_id == self.tenant_id {
            true => self.directory.events.get(id).cloned(),
            false => None,
        })
    }
}

pub struct EmptyAgentDirectory;

#[async_trait::async_trait]
impl AgentDirectory for EmptyAgentDirectory {
    async fn tenant(&self, _tenant_id: &str) -> TenantDirectory {
        TenantDirectory::default()
    }

    async fn tenants(&self) -> Vec<String> {
        Vec::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{DecisionResponse, DecisionTrigger, WorkerState};
    use crate::runtime::llm::LlmBlock;
    use crate::runtime::span::SpanContext;

    fn config(llm: &str) -> AgentConfig {
        AgentConfig {
            llm: Some(llm.to_string()),
            model: "m".to_string(),
            ..Default::default()
        }
    }

    fn directory() -> StaticAgentDirectory {
        StaticAgentDirectory::new(
            "default".to_string(),
            BTreeMap::from([
                (
                    "assistant".to_string(),
                    AgentEntry {
                        config: Some(config("claude")),
                        hosting: Hosting::Engine,
                        events: None,
                    },
                ),
                (
                    "triage".to_string(),
                    AgentEntry {
                        config: None,
                        hosting: Hosting::Worker("triage".to_string()),
                        events: None,
                    },
                ),
            ]),
            LlmBlocks::from_iter([("claude".to_string(), LlmBlock::engine())]),
        )
        .with_workers(
            BTreeMap::from([
                (
                    "triage".to_string(),
                    WorkerBlock {
                        url: Some("https://triage.internal/agent".to_string()),
                    },
                ),
                ("customers".to_string(), WorkerBlock { url: None }),
            ]),
            Some("customers".to_string()),
        )
    }

    fn decision(agent_id: &str) -> WorkerDecisionRequest {
        WorkerDecisionRequest {
            session_id: "s-1".to_string(),
            decision_id: "d-1".to_string(),
            agent_id: agent_id.to_string(),
            identity: SessionOwner::default(),
            trigger: DecisionTrigger::SessionStart,
            proposed: DecisionResponse::default(),
            state: WorkerState::default(),
            agent: None,
            worker: None,
            calls: Vec::new(),
            pending_calls: 0,
            transcript: Vec::new(),
            ancestry: Vec::new(),
            span: SpanContext::root(),
            attempts: 0,
            deadline: None,
            turn: None,
        }
    }

    #[tokio::test]
    async fn a_session_sends_to_its_agents_events_url_or_else_its_parents() {
        let mut agents = BTreeMap::new();
        for (id, events) in [("bodi", Some("app")), ("helper", None)] {
            agents.insert(
                id.to_string(),
                AgentEntry {
                    config: Some(config("claude")),
                    hosting: Hosting::Engine,
                    events: events.map(str::to_string),
                },
            );
        }
        let directory =
            StaticAgentDirectory::new("default".to_string(), agents, LlmBlocks::default());
        let events_of = |agent: &'static str| {
            let directory = &directory;
            async move {
                let created = create_session_command(
                    directory,
                    agent.to_string(),
                    SessionOwner {
                        tenant_id: "default".to_string(),
                        ..SessionOwner::default()
                    },
                    vec!["parent".to_string()],
                    RetryPolicy::no_retry(),
                    SessionSeed {
                        events: Some("parent-app".to_string()),
                        ..SessionSeed::default()
                    },
                )
                .await;
                match created {
                    CommandPayload::CreateSession { events, .. } => events,
                    _ => None,
                }
            }
        };

        assert_eq!(events_of("bodi").await.as_deref(), Some("app"));
        assert_eq!(events_of("helper").await.as_deref(), Some("parent-app"));
    }

    #[tokio::test]
    async fn an_agent_carries_its_config_and_its_hosting() {
        let d = directory().tenant("default").await;
        let engine_hosted = d.agent("assistant").expect("declared");
        assert_eq!(
            engine_hosted.config.as_ref().and_then(|c| c.llm.as_deref()),
            Some("claude")
        );
        assert_eq!(engine_hosted.hosting, Hosting::Engine);

        let pushed = d.agent("triage").expect("declared");
        assert_eq!(pushed.hosting, Hosting::Worker("triage".to_string()));
        let block = d.worker("triage").expect("declared");
        assert_eq!(block.url.as_deref(), Some("https://triage.internal/agent"));
    }

    #[tokio::test]
    async fn the_default_worker_is_the_one_the_file_names() {
        let d = directory();
        assert_eq!(
            d.tenant("default").await.default_worker.as_deref(),
            Some("customers")
        );
        assert!(d.tenant("other").await.default_worker.is_none());
        assert!(d.tenant("other").await.worker("triage").is_none());
    }

    #[tokio::test]
    async fn an_undeclared_agent_and_another_tenant_are_both_absent() {
        let d = directory();
        assert!(d.tenant("default").await.agent("typo").is_none());
        assert!(d.tenant("other").await.agent("assistant").is_none());
        assert!(d.tenant("other").await.llm.is_empty());
        assert_eq!(d.tenants().await, vec!["default".to_string()]);
        assert_eq!(
            d.tenant("default").await.agent_ids(),
            ["assistant", "triage"]
        );
    }

    #[tokio::test]
    async fn creation_resolves_the_worker_once() {
        let d = directory().tenant("default").await;
        let named = WorkerRef {
            id: "triage".to_string(),
            url: None,
        };
        assert_eq!(
            d.resolve_worker("invented", Some(named.clone())),
            Some(named),
            "a named worker wins"
        );
        assert_eq!(
            d.resolve_worker("assistant", None),
            None,
            "a declared agent follows the file, not a stamp"
        );
        assert_eq!(
            d.resolve_worker("invented", None),
            Some(WorkerRef {
                id: "customers".to_string(),
                url: None,
            }),
            "an undeclared agent is stamped with the default at creation"
        );
        assert_eq!(
            EmptyAgentDirectory
                .tenant("default")
                .await
                .resolve_worker("invented", None),
            None,
            "no default, no stamp"
        );
    }

    #[tokio::test]
    async fn a_decision_follows_its_agents_hosting() {
        let d = directory().tenant("default").await;
        assert_eq!(d.route(&decision("assistant")), Ok(Route::Engine));
        assert_eq!(
            d.route(&decision("triage")),
            Ok(Route::Worker {
                id: "triage".to_string(),
                url: "https://triage.internal/agent".to_string(),
            })
        );
    }

    #[tokio::test]
    async fn a_session_can_pin_a_worker_and_bring_its_address() {
        let d = directory().tenant("default").await;
        let pinned = |id: &str, url: Option<&str>| WorkerDecisionRequest {
            worker: Some(WorkerRef {
                id: id.to_string(),
                url: url.map(str::to_string),
            }),
            ..decision("assistant")
        };
        assert_eq!(
            d.route(&pinned("customers", Some("https://brought.test"))),
            Ok(Route::Worker {
                id: "customers".to_string(),
                url: "https://brought.test".to_string(),
            }),
            "the session's address wins over the agent's hosting"
        );
        assert!(
            d.route(&pinned("customers", None))
                .is_err_and(|e| e.contains("has no `url`")),
            "a block with no address and a session that brought none is unroutable"
        );
        assert!(d
            .route(&pinned("invented", None))
            .is_err_and(|e| e.contains("no [worker.invented]")));
    }

    #[tokio::test]
    async fn an_undeclared_agent_needs_a_config_on_the_session() {
        let d = directory().tenant("default").await;
        assert!(d
            .route(&decision("typo"))
            .is_err_and(|e| e.contains("no [agent.typo]")));
        assert_eq!(
            d.route(&WorkerDecisionRequest {
                agent: Some(config("claude")),
                ..decision("typo")
            }),
            Ok(Route::Engine)
        );
    }

    #[tokio::test]
    async fn an_empty_directory_declares_nothing() {
        let d = EmptyAgentDirectory;
        assert!(d.tenant("default").await.agent("assistant").is_none());
        assert!(d.tenant("default").await.worker("main").is_none());
        assert!(d.tenant("default").await.default_worker.is_none());
        assert!(d.tenants().await.is_empty());
        assert_eq!(
            crate::copy::declared(d.tenant("default").await.agent_ids()),
            "none"
        );
    }
}
