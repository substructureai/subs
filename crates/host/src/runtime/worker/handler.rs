use substructure_session::request::PendingPrompt;

use crate::protocol::{AgentConfig, DecisionTrigger};
use crate::runtime::blob::BlobStore;
use crate::runtime::session::wire::finish_prompt;

use super::{AgentDirectory, WorkerDecisionRequest};

async fn seed_config(
    agents: &dyn AgentDirectory,
    tenant_id: &str,
    agent_id: &str,
    resolved: Option<AgentConfig>,
) -> Option<AgentConfig> {
    match resolved {
        Some(config) => Some(config),
        None => agents
            .tenant(tenant_id)
            .await
            .agent(agent_id)
            .and_then(|e| e.config.clone()),
    }
}

#[derive(Debug)]
pub struct Unready {
    pub error: String,
    pub permanent: bool,
}

impl std::fmt::Display for Unready {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.error)
    }
}

pub async fn finish(
    mut request: WorkerDecisionRequest,
    prompt: Option<PendingPrompt>,
    agents: &dyn AgentDirectory,
    blobs: &dyn BlobStore,
) -> Result<WorkerDecisionRequest, Unready> {
    let tenant_id = request.tenant_id().to_string();
    finish_prompt(&mut request.trigger, prompt, blobs, &tenant_id)
        .await
        .map_err(|e| Unready {
            error: format!("resolving a decision for delivery: {}", e.error.message),
            permanent: true,
        })?;
    let unproposed = request.proposed.agent.is_none()
        && request.proposed.actions.is_empty()
        && request.proposed.messages.is_empty();
    if unproposed && matches!(request.trigger, DecisionTrigger::SessionStart) {
        request.proposed.agent =
            seed_config(agents, &tenant_id, &request.agent_id, request.agent.clone()).await;
    }
    Ok(request)
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use chrono::Utc;

    use std::collections::BTreeMap;

    use substructure_session::{Command, MemoryStore, Session, Settings, Work};

    use super::{finish, AgentDirectory};
    use crate::connectors::RemoteTool;
    use crate::protocol::{
        AgentConfig, Approve, ClientMessage, ClientPayload, Content, DraftMessage, EffectKind,
        Issuer, LlmRequest, LlmResponse, McpServer, Requester, RetryPolicy, Role, SessionOwner,
        StoredContent, StoredResult, Subject, ToolCall, ToolCallFunction,
    };
    use crate::runtime::llm::{LlmBlock, LlmBlocks};
    use crate::runtime::session::command::{CommandPayload, TurnTarget};
    use crate::runtime::session::decision::LlmHandler;
    use crate::runtime::session::effects::Outcome;
    use crate::runtime::session::events::EventPayload;
    use crate::runtime::session::wire::resolve_response;
    use crate::runtime::session::SessionEvent;
    use crate::runtime::worker::directory::{AgentEntry, StaticAgentDirectory};
    use crate::runtime::worker::EmptyAgentDirectory;
    use crate::runtime::Caller;

    struct Agg {
        store: MemoryStore,
        session: Session,
        commands: u32,
    }

    impl Agg {
        fn new() -> Self {
            let store = MemoryStore::new();
            let session = Session::load(&store, "sess-1", "tenant-a", Settings::default()).unwrap();
            Self {
                store,
                session,
                commands: 0,
            }
        }
    }

    fn dispatch(agg: &mut Agg, cmd: CommandPayload, caller: &Caller) -> Vec<SessionEvent> {
        agg.commands += 1;
        let ran = agg
            .session
            .run(
                &agg.store,
                Utc::now(),
                Command::new(format!("c{}", agg.commands), caller.clone(), cmd),
            )
            .unwrap();
        ran.result.expect("setup command failed");
        ran.written.map(|e| e.events).unwrap_or_default()
    }

    async fn request_for(
        agg: &Agg,
        event: &SessionEvent,
        agents: &dyn AgentDirectory,
    ) -> super::WorkerDecisionRequest {
        let work = agg.session.work(std::slice::from_ref(event));
        let Some(Work::AskWorker {
            request, prompt, ..
        }) = work.into_iter().next()
        else {
            panic!("a deliverable request");
        };
        finish(
            *request,
            prompt.map(|p| *p),
            agents,
            &crate::runtime::blob::NOWHERE,
        )
        .await
        .expect("finishing succeeds")
    }

    fn system() -> Caller {
        Caller::System {
            tenant_id: "tenant-a".to_string(),
        }
    }

    fn user_msg(text: &str) -> DraftMessage {
        DraftMessage {
            id: None,
            role: Role::User,
            content: Some(Content::Text(text.into())),
            tool_calls: None,
            tool_call_id: None,
            name: None,
            reasoning: None,
        }
    }

    fn config(model: &str) -> AgentConfig {
        AgentConfig {
            llm: Some("claude".to_string()),
            model: model.to_string(),
            ..Default::default()
        }
    }

    fn directory(config: AgentConfig) -> StaticAgentDirectory {
        StaticAgentDirectory::new(
            "tenant-a".to_string(),
            std::collections::BTreeMap::from([(
                "agent-1".to_string(),
                AgentEntry {
                    config: Some(config),
                    hosting: crate::worker::Hosting::Engine,
                    events: None,
                },
            )]),
            Default::default(),
        )
    }

    async fn wire_request_for_client_message(agent: Option<AgentConfig>) -> serde_json::Value {
        wire_request_for(agent, user_msg("hi")).await
    }

    async fn wire_request_for(
        agent: Option<AgentConfig>,
        message: DraftMessage,
    ) -> serde_json::Value {
        let mut agg = Agg::new();
        let created = dispatch(
            &mut agg,
            CommandPayload::CreateSession {
                agent_id: "agent-1".to_string(),
                owner: SessionOwner {
                    tenant_id: "tenant-a".to_string(),
                    requester: Requester::new(
                        Subject::new(Issuer::app(), "user-1".to_string()),
                        Default::default(),
                    ),
                    metadata: HashMap::new(),
                },
                ancestry: vec![],
                worker_retry: RetryPolicy::no_retry(),
                agent: None,
                worker: None,
                events: None,
                state: None,
            },
            &system(),
        );
        let start = created
            .iter()
            .find_map(|e| match &e.payload {
                EventPayload::DecisionDispatched(w) => Some(w.id.clone()),
                _ => None,
            })
            .expect("CreateSession opens a session.start decision");
        dispatch(
            &mut agg,
            CommandPayload::SubmitWorkerDecision {
                decision_id: start,
                transcript: vec![],
                actions: vec![],
                state: None,
                agent,
            },
            &system(),
        );
        let event = dispatch(
            &mut agg,
            CommandPayload::SubmitClientPayload {
                payload: ClientPayload::Message(ClientMessage {
                    message,
                    stream: false,
                }),
                turn: TurnTarget::Detached,
                queue: Default::default(),
            },
            &system(),
        )
        .into_iter()
        .find(|e| matches!(e.payload, EventPayload::DecisionDispatched(_)))
        .expect("the client decision goes live");

        let req = request_for(&agg, &event, &EmptyAgentDirectory).await;
        serde_json::to_value(&req).expect("wire request serializes")
    }

    async fn wire_request_for_session_start(agents: &dyn AgentDirectory) -> serde_json::Value {
        let mut agg = Agg::new();
        let event = dispatch(
            &mut agg,
            CommandPayload::CreateSession {
                agent_id: "agent-1".to_string(),
                owner: SessionOwner {
                    tenant_id: "tenant-a".to_string(),
                    requester: Requester::new(
                        Subject::new(Issuer::app(), "user-1".to_string()),
                        Default::default(),
                    ),
                    metadata: HashMap::new(),
                },
                ancestry: vec![],
                worker_retry: RetryPolicy::no_retry(),
                agent: None,
                worker: None,
                events: None,
                state: None,
            },
            &system(),
        )
        .into_iter()
        .find(|e| matches!(e.payload, EventPayload::DecisionDispatched(_)))
        .expect("CreateSession opens a session.start decision");

        let req = request_for(&agg, &event, agents).await;
        serde_json::to_value(&req).expect("wire request serializes")
    }

    #[tokio::test]
    async fn session_start_is_seeded_with_the_declared_config() {
        let wire = wire_request_for_session_start(&directory(config("m1"))).await;
        assert_eq!(wire["proposed"]["agent"]["model"], "m1");
        assert_eq!(wire["proposed"]["agent"]["llm"], "claude");
    }

    #[tokio::test]
    async fn session_start_without_a_declaration_carries_no_proposal() {
        let wire = wire_request_for_session_start(&EmptyAgentDirectory).await;
        assert!(
            wire["proposed"]["agent"].is_null(),
            "got {}",
            wire["proposed"]
        );
        assert!(wire["proposed"]["actions"]
            .as_array()
            .is_none_or(|a| a.is_empty()));
    }

    #[tokio::test]
    async fn the_workers_attachment_rules_reach_the_first_message() {
        use crate::attachments::{Attachments, Disposition};
        let agent = AgentConfig {
            attachments: Some(Attachments {
                rules: BTreeMap::from([("*/*".to_string(), Disposition::Attachment)]),
                ..Default::default()
            }),
            ..config("m1")
        };
        let uri = crate::runtime::blob::BlobRef {
            tenant_id: "tenant-a".to_string(),
            id: "0198b2a0-3c5d-7f00-8000-0123456789ab".to_string(),
            mime: "text/csv".to_string(),
            name: Some("sales.csv".to_string()),
            size: 12,
        }
        .uri();
        let mut message = user_msg("here");
        message.content = Some(Content::Parts(vec![StoredContent::Blob { uri }]));

        let wire = wire_request_for(Some(agent), message).await;

        let sent = wire["trigger"]["messages"]
            .as_array()
            .and_then(|m| m.last())
            .expect("the new message is last");
        assert_eq!(sent["content"][0]["type"], "attachment", "{sent}");
        assert_eq!(sent["content"][0]["id"], "sales.csv");
    }

    #[tokio::test]
    async fn a_non_start_trigger_is_not_seeded() {
        let wire = wire_request_for_client_message(None).await;
        assert!(
            wire["proposed"]["agent"].is_null(),
            "the directory does not reach a client.messages proposal; got {}",
            wire["proposed"]
        );
    }

    #[tokio::test]
    async fn no_config_client_message_proposes_an_interrupt() {
        let wire = wire_request_for_client_message(None).await;
        let actions = wire["proposed"]["actions"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        assert!(
            actions.iter().any(|a| a["type"] == "interrupt"),
            "with no config the proposal pauses the session so a blind echoer \
             fails loudly instead of submitting a silent no-op; got {}",
            wire["proposed"]
        );
    }

    async fn drive(agg: &mut Agg, events: Vec<SessionEvent>) -> Vec<SessionEvent> {
        let mut queue: Vec<SessionEvent> = events;
        let mut settled = Vec::new();
        while let Some(event) = queue.pop() {
            if !matches!(event.payload, EventPayload::DecisionDispatched(_)) {
                continue;
            }
            let request = request_for(agg, &event, &EmptyAgentDirectory).await;
            let blocks =
                LlmBlocks::new(BTreeMap::from([("claude".to_string(), LlmBlock::engine())]));
            let resolved = resolve_response(
                request.proposed,
                request.agent.as_ref(),
                Some(&request.trigger),
                &blocks,
                &crate::runtime::blob::NOWHERE,
                "tenant-a",
            )
            .await
            .expect("the proposal resolves");
            let next = dispatch(
                agg,
                CommandPayload::SubmitWorkerDecision {
                    decision_id: request.decision_id,
                    transcript: resolved.messages,
                    actions: resolved.actions,
                    state: resolved.state,
                    agent: resolved.agent,
                },
                &system(),
            );
            queue.extend(next.iter().cloned());
            settled.extend(next);
        }
        settled
    }

    async fn two_calls_awaiting_approval() -> Agg {
        let mut agg = Agg::new();
        let created = dispatch(
            &mut agg,
            CommandPayload::CreateSession {
                agent_id: "agent-1".to_string(),
                owner: SessionOwner {
                    tenant_id: "tenant-a".to_string(),
                    requester: Requester::new(
                        Subject::new(Issuer::app(), "user-1".to_string()),
                        Default::default(),
                    ),
                    metadata: HashMap::new(),
                },
                ancestry: vec![],
                worker_retry: RetryPolicy::no_retry(),
                agent: None,
                worker: None,
                events: None,
                state: None,
            },
            &system(),
        );
        let start = created
            .iter()
            .find_map(|e| match &e.payload {
                EventPayload::DecisionDispatched(w) => Some(w.id.clone()),
                _ => None,
            })
            .expect("CreateSession opens a session.start decision");
        dispatch(
            &mut agg,
            CommandPayload::SubmitWorkerDecision {
                decision_id: start,
                transcript: vec![],
                actions: vec![],
                state: None,
                agent: Some(AgentConfig {
                    mcp: vec![McpServer {
                        id: "sentry".into(),
                        tools: None,
                        auth_failure: Default::default(),
                        tool_sync_failure: Default::default(),
                        approve: Approve::Destructive,
                    }],
                    ..config("m1")
                }),
            },
            &system(),
        );
        dispatch(
            &mut agg,
            CommandPayload::settle(
                EffectKind::ConnectorSync,
                "mcp.sentry".to_string(),
                None,
                Outcome::Connector {
                    server: None,
                    prefix: Some("sentry".to_string()),
                    tools: vec![RemoteTool {
                        name: "delete_issue".to_string(),
                        title: None,
                        description: "Delete an issue.".to_string(),
                        input: None,
                        output: None,
                        annotations: crate::connectors::ToolAnnotations {
                            destructive: Some(true),
                            ..Default::default()
                        },
                    }],
                    instructions: None,
                },
            ),
            &system(),
        );
        dispatch(
            &mut agg,
            CommandPayload::RequestLlmCall {
                llm: "claude".to_string(),
                call_id: "call-1".to_string(),
                request: LlmRequest {
                    model: "m1".to_string(),
                    messages: vec![],
                    tools: None,
                    temperature: None,
                    max_completion_tokens: None,
                    reasoning: None,
                },
                stream: false,
                retry: RetryPolicy::no_retry(),
                handler: LlmHandler::Server,
                format: None,
            },
            &system(),
        );
        let finished = dispatch(
            &mut agg,
            CommandPayload::settle(
                EffectKind::LlmCall,
                "call-1".to_string(),
                Some(0),
                Outcome::Llm(Box::new(LlmResponse {
                    model: "m1".to_string(),
                    content: None,
                    tool_calls: vec![delete_call("tc-1", "7"), delete_call("tc-2", "9")],
                    finish_reason: Some("tool_calls".to_string()),
                    usage: None,
                    cost: None,
                    images: vec![],
                    reasoning: None,
                })),
            ),
            &system(),
        );
        drive(&mut agg, finished).await;
        agg
    }

    fn delete_call(id: &str, issue: &str) -> ToolCall {
        ToolCall {
            id: id.to_string(),
            call_type: "function".to_string(),
            function: ToolCallFunction {
                name: "sentry__delete_issue".to_string(),
                arguments: serde_json::json!({ "id": issue }).to_string(),
            },
        }
    }

    fn resume(interrupt_id: &str, approved: bool) -> CommandPayload {
        CommandPayload::ResumeInterrupt {
            interrupt_id: interrupt_id.to_string(),
            payload: serde_json::json!({ "approved": approved }),
        }
    }

    #[tokio::test]
    async fn parallel_calls_are_asked_about_one_at_a_time() {
        let mut agg = two_calls_awaiting_approval().await;
        assert!(
            agg.session
                .state()
                .open_interrupt("mcp-approve:tc-1")
                .is_some(),
            "the first call is asked about; open: {:?}",
            agg.session.state().open_interrupts
        );
        assert!(
            agg.session.state().tool_call("tc-1").is_none()
                && agg.session.state().tool_call("tc-2").is_none(),
            "nothing runs while a question is open"
        );

        let events = dispatch(&mut agg, resume("mcp-approve:tc-1", true), &system());
        drive(&mut agg, events).await;
        assert!(
            agg.session.state().tool_call("tc-1").is_some(),
            "the approved call is dispatched"
        );
        assert!(
            agg.session
                .state()
                .open_interrupt("mcp-approve:tc-2")
                .is_some(),
            "and the next question follows it; open: {:?}",
            agg.session.state().open_interrupts
        );

        let events = dispatch(
            &mut agg,
            CommandPayload::settle(
                EffectKind::ToolCall,
                "tc-1".to_string(),
                Some(0),
                Outcome::Tool {
                    result: StoredResult::text("deleted issue 7"),
                },
            ),
            &system(),
        );
        assert!(
            !events
                .iter()
                .any(|e| matches!(e.payload, EventPayload::DecisionDispatched(_))),
            "a result lands, but its decision waits for the open question"
        );

        let events = dispatch(&mut agg, resume("mcp-approve:tc-2", false), &system());
        drive(&mut agg, events).await;

        assert!(
            agg.session.state().open_interrupts.is_empty(),
            "a call already away must not be asked about again; open: {:?}",
            agg.session.state().open_interrupts
        );
        assert!(
            agg.session.state().tool_call("tc-2").is_none(),
            "the declined call never runs"
        );
        let prompt = last_prompt(&agg).expect("the model is prompted with both outcomes");
        let answers: Vec<(&str, String)> = prompt
            .iter()
            .filter(|m| m.role == Role::Tool)
            .map(|m| {
                (
                    m.tool_call_id.as_deref().unwrap_or_default(),
                    m.content
                        .as_ref()
                        .and_then(Content::text)
                        .unwrap_or_default()
                        .to_string(),
                )
            })
            .collect();
        assert_eq!(
            answers.len(),
            2,
            "every call the model made is answered exactly once; got {answers:?}"
        );
        assert!(answers
            .iter()
            .any(|(id, text)| *id == "tc-1" && text.contains("deleted issue 7")));
        assert!(answers
            .iter()
            .any(|(id, text)| *id == "tc-2" && text.contains("declined")));
    }

    fn last_prompt(agg: &Agg) -> Option<Vec<crate::protocol::Message>> {
        agg.session
            .state()
            .effects_of(EffectKind::LlmCall)
            .filter(|e| e.id != "call-1")
            .last()
            .and_then(|e| e.llm())
            .map(|c| c.prompt.clone())
    }

    #[tokio::test]
    async fn configured_client_message_still_carries_a_proposal() {
        let wire = wire_request_for_client_message(Some(config("m1"))).await;
        assert!(
            wire["proposed"]["actions"]
                .as_array()
                .is_some_and(|a| !a.is_empty()),
            "a configured client.message proposes the LLM continuation; got {}",
            wire["proposed"]
        );
    }
}
