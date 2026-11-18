use serde::{Deserialize, Serialize};

use crate::protocol::{Content, McpAnnounce, Message, Role, StoredContent};
use crate::state::At;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Placement {
    System,
    Inline,
    Own,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Placed {
    pub call: String,
    pub message: String,
    pub placement: Placement,
    pub content: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub after: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ids: Vec<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct PromptContext {
    pub id: String,
    pub placement: Placement,
    pub content: String,
}

pub type Contributor = fn(At) -> Vec<PromptContext>;

const CONTRIBUTORS: &[Contributor] = &[
    plugin_catalog,
    skill_catalog,
    announce_servers,
    announce_unavailable,
];

pub fn owed(at: At) -> Vec<PromptContext> {
    let said = at.said();
    CONTRIBUTORS
        .iter()
        .flat_map(|contribute| contribute(at))
        .filter(|c| !said.contains(&c.id))
        .collect()
}

fn announce_servers(at: At) -> Vec<PromptContext> {
    let Some(config) = at.config() else {
        return Vec::new();
    };
    if config.mcp_announce == McpAnnounce::Never {
        return Vec::new();
    }
    at.session()
        .servers_for(&config)
        .into_iter()
        .map(|server| (server.path.to_string(), server))
        .filter_map(|(id, server)| {
            Some(PromptContext {
                id,
                placement: Placement::System,
                content: at.connection_summary(&server.path)?,
            })
        })
        .collect()
}

fn announce_unavailable(at: At) -> Vec<PromptContext> {
    let Some(config) = at.config() else {
        return Vec::new();
    };
    let mut owed = Vec::new();
    for (path, auth) in at.unavailable_connectors(&config) {
        let reason = crate::prompts::context::unavailable_reason(auth);
        let id = format!("{path}:unavailable:{reason}");
        let content = serde_json::to_string(&Unavailable {
            mcp_server: &path,
            unavailable: true,
            reason,
        })
        .unwrap_or_default();
        owed.push(PromptContext {
            id,
            placement: Placement::System,
            content,
        });
    }
    owed
}

#[derive(serde::Serialize)]
struct Unavailable<'a> {
    mcp_server: &'a crate::protocol::ConnectionPath,
    unavailable: bool,
    reason: &'static str,
}

fn plugin_catalog(at: At) -> Vec<PromptContext> {
    let Some(config) = at.config() else {
        return Vec::new();
    };
    let code = config.codemode.is_some();
    let searched = config.searches_skills() && !code;
    config
        .plugins
        .iter()
        .map(|plugin| (plugin, format!("plugin.{}", plugin.id)))
        .map(|(plugin, id)| {
            let skills: Vec<SkillListing> = match searched {
                true => Vec::new(),
                false => plugin
                    .skills
                    .iter()
                    .map(|s| SkillListing {
                        name: format!("{}:{}", plugin.id, s.name),
                        description: s.description.clone(),
                        tools: match code {
                            true => s.tools.clone(),
                            false => Vec::new(),
                        },
                    })
                    .collect(),
            };
            let content = serde_json::to_string(&PluginListing {
                plugin: &plugin.id,
                about: (!plugin.description.is_empty()).then_some(&plugin.description),
                skills,
                usage: match searched {
                    true => crate::prompts::context::searched_skills(),
                    false => crate::prompts::context::plugin_usage(code),
                },
            })
            .unwrap_or_default();
            PromptContext {
                id,
                placement: Placement::System,
                content,
            }
        })
        .collect()
}

/// One entry for each of the agent's own skills, so a skill the worker adds
/// later lists itself without rewriting what the model has read.
fn skill_catalog(at: At) -> Vec<PromptContext> {
    let Some(config) = at.config() else {
        return Vec::new();
    };
    if config.searches_skills() && config.codemode.is_none() {
        return Vec::new();
    }
    config
        .skills
        .iter()
        .map(|skill| (skill, format!("skill.{}", skill.name)))
        .map(|(skill, id)| PromptContext {
            id,
            placement: Placement::System,
            content: serde_json::to_string(&OwnSkillListing {
                skill: &skill.name,
                about: &skill.description,
                tools: match config.codemode.is_some() {
                    true => &skill.tools,
                    false => &[],
                },
                usage: crate::prompts::context::plugin_usage(config.codemode.is_some()),
            })
            .unwrap_or_default(),
        })
        .collect()
}

#[derive(serde::Serialize)]
struct PluginListing<'a> {
    plugin: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    about: Option<&'a String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    skills: Vec<SkillListing>,
    usage: String,
}

#[derive(serde::Serialize)]
struct OwnSkillListing<'a> {
    skill: &'a str,
    #[serde(skip_serializing_if = "str::is_empty")]
    about: &'a str,
    /// The tools it names, in code mode.
    #[serde(skip_serializing_if = "<[String]>::is_empty")]
    tools: &'a [String],
    usage: String,
}

#[derive(serde::Serialize)]
struct SkillListing {
    name: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    description: String,
    /// The tools it names, in code mode.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tools: Vec<String>,
}

pub fn restore(prompt: &mut Vec<Message>, placed: &[Placed]) {
    let mut own = std::collections::HashSet::new();
    for p in placed {
        match p.placement {
            Placement::System => {
                let at = leading_system(prompt);
                prompt.insert(at, message(&p.message, Role::System, &p.content));
            }
            Placement::Inline => {
                if let Some(m) = prompt.iter_mut().find(|m| m.id == p.message) {
                    append(m, &p.content);
                }
            }
            Placement::Own => {
                let Some(start) = start(prompt, p) else {
                    continue;
                };
                let at = start
                    + prompt[start..]
                        .iter()
                        .take_while(|m| own.contains(&m.id))
                        .count();
                prompt.insert(at, message(&p.message, Role::User, &p.content));
                own.insert(p.message.clone());
            }
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct Open {
    pub system: bool,
    pub inline: bool,
}

pub fn merge(
    prompt: &mut Vec<Message>,
    placed: &mut Vec<Placed>,
    call_id: &str,
    open: Open,
    owed: Vec<PromptContext>,
) {
    let mut groups: Vec<(Placement, Vec<PromptContext>)> = Vec::new();
    for c in owed {
        if groups.iter().any(|(_, g)| g.iter().any(|o| o.id == c.id)) {
            continue;
        }
        let placement = resolve(c.placement, prompt, open);
        match groups.iter_mut().find(|(p, _)| *p == placement) {
            Some((_, group)) => group.push(c),
            None => groups.push((placement, vec![c])),
        }
    }
    for (placement, group) in groups {
        let content = group
            .iter()
            .map(|c| c.content.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        let id = format!("{call_id}-ctx-{}", group[0].id);
        let ids: Vec<String> = group.into_iter().map(|c| c.id).collect();
        placed.extend(
            place(prompt, call_id, id, content, placement).map(|p| Placed {
                ids: ids.clone(),
                ..p
            }),
        );
    }
}

fn resolve(wanted: Placement, prompt: &[Message], open: Open) -> Placement {
    match wanted {
        Placement::System if open.system => Placement::System,
        Placement::System | Placement::Inline => {
            let host = open.inline && prompt.last().is_some_and(|m| m.role == Role::User);
            if host {
                Placement::Inline
            } else {
                Placement::Own
            }
        }
        Placement::Own => Placement::Own,
    }
}

fn place(
    prompt: &mut Vec<Message>,
    call_id: &str,
    id: String,
    content: String,
    placement: Placement,
) -> Option<Placed> {
    let (message_id, after) = match placement {
        Placement::System => {
            let at = leading_system(prompt);
            prompt.insert(at, message(&id, Role::System, &content));
            (id, None)
        }
        Placement::Inline => {
            let last = prompt.last_mut()?;
            append(last, &content);
            (last.id.clone(), None)
        }
        Placement::Own => {
            let after = prompt
                .last()
                .filter(|_| leading_system(prompt) < prompt.len())
                .map(|m| m.id.clone());
            prompt.push(message(&id, Role::User, &content));
            (id, after)
        }
    };
    Some(Placed {
        call: call_id.to_string(),
        message: message_id,
        placement,
        content,
        after,
        ids: Vec::new(),
    })
}

fn start(prompt: &[Message], p: &Placed) -> Option<usize> {
    match &p.after {
        None => Some(leading_system(prompt)),
        Some(after) => prompt.iter().position(|m| &m.id == after).map(|i| i + 1),
    }
}

fn leading_system(prompt: &[Message]) -> usize {
    prompt
        .iter()
        .position(|m| m.role != Role::System)
        .unwrap_or(prompt.len())
}

fn append(m: &mut Message, content: &str) {
    m.content = Some(match m.content.take() {
        Some(Content::Parts(mut parts)) => {
            parts.push(StoredContent::Text {
                text: content.to_string(),
            });
            Content::Parts(parts)
        }
        Some(Content::Text(head)) => Content::Text(format!("{head}\n\n{content}")),
        None => Content::Text(format!("\n\n{content}")),
    });
}

fn message(id: &str, role: Role, content: &str) -> Message {
    Message {
        id: id.to_string(),
        role,
        content: Some(Content::Text(content.to_string())),
        tool_calls: Vec::new(),
        tool_call_id: None,
        name: None,
        reasoning: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::connectors::ConnectionPath;
    use crate::connectors::{AuthNeed, RemoteTool};
    use crate::events::{
        ConfigUpdated, ConnectorSyncCompleted, ConnectorSyncErrored, ConnectorSyncRequested,
        EventPayload,
    };
    use crate::prompt::fingerprint;
    use crate::protocol::{
        AgentConfig, AgentPlugin, ErrorCode, ErrorInfo, McpServer, McpToolSyncFailure, RetryPolicy,
        SkillMeta, StoredContent,
    };
    use crate::state::BranchId;
    use crate::state::{ApplyContext, SessionState};

    fn apply(state: &mut SessionState, seq: u64, payload: EventPayload) {
        state.apply(
            &payload,
            &ApplyContext {
                occurred_at: chrono::Utc::now(),
                sequence: seq,
            },
        );
    }

    fn plugin_state(settled: bool) -> SessionState {
        let mut s = SessionState::new("sess-1".to_string());
        apply(
            &mut s,
            1,
            EventPayload::ConfigUpdated(ConfigUpdated {
                config: AgentConfig {
                    model: "m1".to_string(),
                    plugins: vec![AgentPlugin {
                        id: "pdf".to_string(),
                        description: "PDF work.".to_string(),
                        skills: vec![
                            SkillMeta {
                                name: "form-filling".to_string(),
                                description: "long ".repeat(500),
                                tools: vec![],
                            },
                            SkillMeta {
                                name: "text-extraction".to_string(),
                                description: "also long ".repeat(500),
                                tools: vec![],
                            },
                        ],
                        servers: vec!["renderer".to_string()],
                        tools: None,
                        auth_failure: Default::default(),
                        tool_sync_failure: Default::default(),
                        approve: Default::default(),
                        source: Default::default(),
                    }],
                    ..Default::default()
                },
                node: crate::tree::Place {
                    id: uuid::Uuid::now_v7().to_string(),
                    parent_id: None,
                    branch: None,
                },
            }),
        );
        if settled {
            apply(
                &mut s,
                3,
                EventPayload::ConnectorSyncRequested(ConnectorSyncRequested {
                    path: ConnectionPath::PluginServer {
                        plugin: "pdf".into(),
                        server: "renderer".into(),
                    },
                    attempt: 0,
                    retry: RetryPolicy::no_retry(),
                }),
            );
            apply(
                &mut s,
                4,
                EventPayload::ConnectorSyncCompleted(Box::new(ConnectorSyncCompleted {
                    path: ConnectionPath::PluginServer {
                        plugin: "pdf".into(),
                        server: "renderer".into(),
                    },
                    server: None,
                    prefix: Some("pdf_renderer".to_string()),
                    tools: vec![RemoteTool {
                        name: "fill_form".to_string(),
                        title: None,
                        description: String::new(),
                        input: None,
                        output: None,
                        annotations: Default::default(),
                    }],
                    instructions: None,
                })),
            );
        }
        s
    }

    fn owed_ids(state: &SessionState) -> Vec<String> {
        owed(state.on(&BranchId::MAIN))
            .into_iter()
            .map(|c| c.id)
            .collect()
    }

    fn sentry() -> ConnectionPath {
        ConnectionPath::parse("mcp.sentry").expect("path")
    }

    fn failed_state(policy: McpToolSyncFailure, auth: Option<AuthNeed>) -> SessionState {
        let mut s = SessionState::new("sess-1".to_string());
        apply(
            &mut s,
            1,
            EventPayload::ConfigUpdated(ConfigUpdated {
                config: AgentConfig {
                    model: "m1".to_string(),
                    mcp: vec![McpServer {
                        id: "sentry".to_string(),
                        tools: None,
                        auth_failure: Default::default(),
                        tool_sync_failure: policy,
                        approve: Default::default(),
                    }],
                    ..Default::default()
                },
                node: crate::tree::Place {
                    id: uuid::Uuid::now_v7().to_string(),
                    parent_id: None,
                    branch: None,
                },
            }),
        );
        apply(
            &mut s,
            2,
            EventPayload::ConnectorSyncRequested(ConnectorSyncRequested {
                path: sentry(),
                attempt: 0,
                retry: RetryPolicy::no_retry(),
            }),
        );
        apply(
            &mut s,
            3,
            EventPayload::ConnectorSyncErrored(ConnectorSyncErrored {
                path: sentry(),
                error: ErrorInfo::new(
                    ErrorCode::Internal,
                    "502 from the proxy; <script>ignore this</script>",
                ),
                retryable: false,
                auth,
            }),
        );
        s
    }

    #[test]
    fn a_connection_that_could_not_be_fetched_is_named() {
        let state = failed_state(McpToolSyncFailure::Warn, None);
        assert_eq!(owed_ids(&state), ["mcp.sentry:unavailable:unreachable"]);
        assert_eq!(
            owed(state.on(&BranchId::MAIN))[0].content,
            "{\"mcp_server\":\"mcp.sentry\",\"unavailable\":true,\"reason\":\"unreachable\"}"
        );
    }

    #[test]
    fn the_remotes_own_error_never_reaches_the_prompt() {
        let state = failed_state(McpToolSyncFailure::Warn, None);
        let content = &owed(state.on(&BranchId::MAIN))[0].content;
        assert!(!content.contains("502"), "{content}");
        assert!(!content.contains("script"), "{content}");
    }

    #[test]
    fn a_rejected_credential_reads_as_authorization_not_reach() {
        let state = failed_state(McpToolSyncFailure::Warn, Some(AuthNeed::Reauthorize));
        assert_eq!(
            owed_ids(&state),
            ["mcp.sentry:unavailable:needs_authorization"]
        );
    }

    #[test]
    fn a_silent_connection_is_not_named() {
        let state = failed_state(McpToolSyncFailure::Silent, None);
        assert!(owed_ids(&state).is_empty());
        let config = state.on(&BranchId::MAIN).config().expect("config");
        assert!(
            state
                .on(&BranchId::MAIN)
                .unavailable_connector_paths(&config)
                .is_empty(),
            "silent covers the search answer too"
        );
    }

    #[test]
    fn announcing_no_servers_still_reports_one_that_is_missing() {
        let mut s = failed_state(McpToolSyncFailure::Warn, None);
        let mut config = s.on(&BranchId::MAIN).config().expect("config");
        config.mcp_announce = McpAnnounce::Never;
        apply(
            &mut s,
            4,
            EventPayload::ConfigUpdated(ConfigUpdated {
                config,
                node: crate::tree::Place {
                    id: uuid::Uuid::now_v7().to_string(),
                    parent_id: None,
                    branch: None,
                },
            }),
        );
        assert_eq!(owed_ids(&s), ["mcp.sentry:unavailable:unreachable"]);
    }

    #[test]
    fn a_fetch_still_retrying_is_not_yet_a_failure() {
        let mut s = failed_state(McpToolSyncFailure::Warn, None);
        apply(
            &mut s,
            4,
            EventPayload::ConnectorSyncRequested(ConnectorSyncRequested {
                path: sentry(),
                attempt: 1,
                retry: RetryPolicy::no_retry(),
            }),
        );
        assert!(owed_ids(&s).is_empty(), "the fetch is in flight again");
    }

    #[test]
    fn a_down_connection_is_named_in_the_search_answer() {
        let state = failed_state(McpToolSyncFailure::Warn, None);
        let answered = crate::engine_tools::answer(
            state.on(&BranchId::MAIN),
            crate::protocol::ConnectorToolKind::Find,
            "{\"query\":\"issues\"}",
        )
        .expect("the engine answers a search");
        let text = match answered.content.first().expect("content") {
            StoredContent::Text { text } => text.clone(),
            other => panic!("expected text, got {other:?}"),
        };
        let answer: serde_json::Value = serde_json::from_str(&text).expect("json");
        assert_eq!(answer["matched"], 0);
        assert_eq!(
            answer["unavailable"][0], "mcp.sentry",
            "an answer of nothing must not read as nothing being there"
        );
    }

    #[test]
    fn a_call_to_an_unknown_tool_says_a_connection_is_down_without_claiming_the_tool() {
        let state = failed_state(McpToolSyncFailure::Warn, None);
        let fault = state
            .on(&BranchId::MAIN)
            .call_tool_fault("{\"name\":\"sentry_list_issues\"}")
            .expect("fault");
        assert!(fault.contains("no tool `sentry_list_issues`"), "{fault}");
        assert!(fault.contains("`mcp.sentry` is unavailable"), "{fault}");
    }

    #[test]
    fn a_config_that_never_named_the_server_is_not_warned_about_it() {
        let mut s = failed_state(McpToolSyncFailure::Warn, None);
        apply(
            &mut s,
            4,
            EventPayload::ConfigUpdated(ConfigUpdated {
                config: AgentConfig {
                    model: "m1".to_string(),
                    ..Default::default()
                },
                node: crate::tree::Place {
                    id: uuid::Uuid::now_v7().to_string(),
                    parent_id: None,
                    branch: None,
                },
            }),
        );
        assert!(
            owed_ids(&s).is_empty(),
            "the fetch is unanchored; this config never named it"
        );
    }

    #[test]
    fn the_notice_falls_out_of_the_prefix_once_a_call_commits_it() {
        let mut prompt = vec![Message {
            id: "m1".into(),
            role: Role::User,
            content: Some(Content::Text("hello".into())),
            tool_calls: Vec::new(),
            tool_call_id: None,
            name: None,
            reasoning: None,
        }];
        let c = PromptContext {
            id: "mcp.sentry:unavailable:unreachable".into(),
            placement: Placement::System,
            content: "gone".into(),
        };
        merge(
            &mut prompt,
            &mut Vec::new(),
            "call-2",
            Open {
                system: false,
                inline: true,
            },
            vec![c],
        );
        assert_eq!(prompt.len(), 1, "it rode the user turn, not the prefix");
        assert!(matches!(
            prompt[0].content.as_ref().expect("content"),
            Content::Text(t) if t.ends_with("gone")
        ));
    }

    #[test]
    fn a_plugins_settled_server_is_announced_like_any_connection() {
        let state = plugin_state(true);
        assert_eq!(owed_ids(&state), ["plugin.pdf", "plugin.pdf.mcp.renderer"]);
        let owed = owed(state.on(&BranchId::MAIN));
        assert!(
            owed[1]
                .content
                .starts_with("{\"mcp_server\":\"plugin.pdf.mcp.renderer\""),
            "{}",
            owed[1].content
        );
    }

    #[test]
    fn a_plugins_unsettled_server_is_not_announced() {
        let state = plugin_state(false);
        assert_eq!(
            owed_ids(&state),
            ["plugin.pdf"],
            "the catalog speaks; the server has not answered yet"
        );
    }

    #[test]
    fn the_catalog_carries_every_skill_description() {
        let state = plugin_state(false);
        let catalog = &owed(state.on(&BranchId::MAIN))[0].content;
        assert!(catalog.matches("long long").count() >= 2, "{catalog}");
        assert!(catalog.contains("pdf:text-extraction"), "{catalog}");
    }

    #[test]
    fn inline_context_keeps_image_parts() {
        let mut prompt = vec![Message {
            id: "m1".into(),
            role: Role::User,
            content: Some(Content::Parts(vec![
                StoredContent::Text {
                    text: "look".into(),
                },
                StoredContent::Blob {
                    uri: "blob://t/ab?mime=image%2Fpng&size=1".into(),
                },
            ])),
            tool_calls: Vec::new(),
            tool_call_id: None,
            name: None,
            reasoning: None,
        }];
        place(
            &mut prompt,
            "c1",
            "c1-ctx-ctx1".into(),
            "server up".into(),
            Placement::Inline,
        );
        match prompt[0].content.as_ref().unwrap() {
            Content::Parts(parts) => {
                assert_eq!(parts.len(), 3);
                assert!(matches!(&parts[1], StoredContent::Blob { .. }));
                assert!(matches!(&parts[2], StoredContent::Text { text } if text == "server up"));
            }
            _ => panic!("expected parts"),
        }
    }

    fn text(id: &str, role: Role, text: &str) -> Message {
        Message {
            id: id.into(),
            role,
            content: Some(Content::Text(text.into())),
            tool_calls: Vec::new(),
            tool_call_id: None,
            name: None,
            reasoning: None,
        }
    }

    fn shape(prompt: &[Message]) -> Vec<(String, String)> {
        prompt
            .iter()
            .map(|m| match &m.content {
                Some(Content::Text(t)) => (m.id.clone(), t.clone()),
                _ => (m.id.clone(), String::new()),
            })
            .collect()
    }

    fn first_turn() -> (Vec<Message>, Vec<Placed>) {
        let mut prompt = vec![
            text("sys", Role::System, "be brief"),
            text("u1", Role::User, "hi"),
        ];
        let mut placed = Vec::new();
        let ctx = |id: &str, placement| PromptContext {
            id: id.into(),
            placement,
            content: format!("about {id}"),
        };
        merge(
            &mut prompt,
            &mut placed,
            "c1",
            Open {
                system: true,
                inline: true,
            },
            vec![ctx("plugin.a", Placement::System)],
        );
        merge(
            &mut prompt,
            &mut placed,
            "c1",
            Open {
                system: false,
                inline: true,
            },
            vec![ctx("mcp.b", Placement::Inline)],
        );
        prompt.push(text("a1", Role::Assistant, "calling"));
        prompt.push(text("t1", Role::Tool, "done"));
        merge(
            &mut prompt,
            &mut placed,
            "c2",
            Open {
                system: false,
                inline: true,
            },
            vec![ctx("mcp.c", Placement::Inline)],
        );
        (prompt, placed)
    }

    fn request_and_send(state: &mut SessionState, seq: u64, id: &str) -> Vec<Message> {
        let request = crate::protocol::LlmRequest {
            model: "m1".to_string(),
            messages: Vec::new(),
            tools: None,
            temperature: None,
            max_completion_tokens: None,
            reasoning: None,
        };
        apply(
            state,
            seq,
            EventPayload::LlmCallRequested(crate::events::LlmCallRequested {
                id: id.to_string(),
                attempt: 0,
                llm: "claude".to_string(),
                spec: crate::state::LlmCallSpec::from(&request),
                at: state.leaf(&BranchId::MAIN).map(str::to_string),
                stream: false,
                retry: RetryPolicy::no_retry(),
                handler: Default::default(),
                format: None,
                defer_tools_strategy: Default::default(),
                branch: None,
            }),
        );
        let ids = crate::ids::Ids::new("s", seq, chrono::Utc::now());
        let dispatched = crate::effects::KindSpec::dispatch(
            &crate::effects::llm::LlmSpec,
            &crate::cx::Cx::new(state, &ids),
            id,
        );
        for (i, event) in dispatched.into_iter().enumerate() {
            apply(state, seq + 1 + i as u64, event);
        }
        state.prompt(id).unwrap()
    }

    #[test]
    fn a_call_sent_again_from_its_first_request_keeps_its_context() {
        let mut state = plugin_state(true);
        let first = request_and_send(&mut state, 10, "call-1");
        assert!(!first.is_empty(), "the first send places the catalog");
        let again = request_and_send(&mut state, 20, "call-1");
        assert_eq!(
            shape(&again),
            shape(&first),
            "a held call sent again from the request it was held with reads what it read"
        );
        assert_eq!(
            state.placed().iter().filter(|p| p.call == "call-1").count(),
            1,
            "placed once"
        );
    }

    #[test]
    fn placed_context_survives_a_snapshot() {
        let mut state = plugin_state(true);
        let first = request_and_send(&mut state, 10, "call-1");
        let mut loaded: SessionState =
            serde_json::from_value(serde_json::to_value(&state).unwrap()).unwrap();
        loaded.tree = state.tree.clone();
        let next = request_and_send(&mut loaded, 20, "call-2");
        assert_eq!(
            shape(&next),
            shape(&first),
            "a session loaded from its snapshot carries what was placed before it"
        );
    }

    fn notice() -> Vec<PromptContext> {
        vec![PromptContext {
            id: "notice".into(),
            placement: Placement::System,
            content: "a server is down".into(),
        }]
    }

    #[test]
    fn context_placed_after_only_a_system_prompt_is_rebuilt_for_the_next_call() {
        let mut sent = vec![text("c1:0", Role::System, "be brief")];
        let mut placed = Vec::new();
        merge(
            &mut sent,
            &mut placed,
            "c1",
            Open {
                system: false,
                inline: true,
            },
            notice(),
        );
        let mut next = vec![text("c2:0", Role::System, "be brief")];
        restore(&mut next, &placed);
        assert_eq!(fingerprint(&next, &None), fingerprint(&sent, &None));
    }

    #[test]
    fn context_placed_into_an_empty_prompt_is_rebuilt_after_a_system_prompt() {
        let mut sent = Vec::new();
        let mut placed = Vec::new();
        merge(
            &mut sent,
            &mut placed,
            "c1",
            Open {
                system: false,
                inline: true,
            },
            notice(),
        );
        let mut next = vec![
            text("c2:0", Role::System, "be brief"),
            text("u1", Role::User, "hi"),
        ];
        restore(&mut next, &placed);
        let ids: Vec<&str> = next.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(ids, ["c2:0", placed[0].message.as_str(), "u1"]);
    }

    #[test]
    fn a_prompt_built_from_the_path_reads_what_the_last_call_sent() {
        let (sent, placed) = first_turn();
        let mut next = vec![
            text("sys", Role::System, "be brief"),
            text("u1", Role::User, "hi"),
            text("a1", Role::Assistant, "calling"),
            text("t1", Role::Tool, "done"),
        ];
        restore(&mut next, &placed);
        assert_eq!(shape(&next), shape(&sent));
        assert_eq!(fingerprint(&next, &None), fingerprint(&sent, &None));
    }

    #[test]
    fn own_messages_after_one_message_keep_their_order() {
        let own = |id: &str, after: &str| Placed {
            call: "c".into(),
            message: id.into(),
            placement: Placement::Own,
            content: id.into(),
            after: Some(after.into()),
            ids: vec![id.into()],
        };
        let mut prompt = vec![
            text("t1", Role::Tool, "done"),
            text("a2", Role::Assistant, "ok"),
        ];
        restore(
            &mut prompt,
            &[
                own("x", "t1"),
                own("y", "t1"),
                own("z", "x"),
                own("w", "gone"),
            ],
        );
        let ids: Vec<&str> = prompt.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(ids, ["t1", "x", "y", "z", "a2"], "in the order placed");
    }

    #[test]
    fn contexts_placed_together_share_one_message() {
        let ctx = |id: &str| PromptContext {
            id: id.into(),
            placement: Placement::System,
            content: format!("{{\"about\":\"{id}\"}}"),
        };
        let mut prompt = Vec::new();
        let mut placed = Vec::new();
        merge(
            &mut prompt,
            &mut placed,
            "c1",
            Open {
                system: true,
                inline: true,
            },
            vec![ctx("a"), ctx("b"), ctx("a")],
        );
        assert_eq!(prompt.len(), 1);
        assert_eq!(prompt[0].id, "c1-ctx-a");
        assert!(matches!(&prompt[0].content,
            Some(Content::Text(t)) if t == "{\"about\":\"a\"}\n{\"about\":\"b\"}"));
        assert_eq!(
            placed,
            [Placed {
                ids: vec!["a".into(), "b".into()],
                call: "c1".into(),
                message: "c1-ctx-a".into(),
                placement: Placement::System,
                content: "{\"about\":\"a\"}\n{\"about\":\"b\"}".into(),
                after: None,
            }],
            "the one message is recorded once, with what it holds"
        );
    }

    #[test]
    fn code_mode_lists_a_plugins_skills_with_their_tools_even_when_skills_are_searched() {
        let config = AgentConfig {
            model: "m1".to_string(),
            codemode: Some(crate::codemode::Language::Javascript),
            defer_tools: Some(crate::protocol::DeferTools {
                skills: crate::protocol::SkillListing::Search,
                ..Default::default()
            }),
            plugins: vec![AgentPlugin {
                id: "org".to_string(),
                description: "The org.".to_string(),
                skills: vec![SkillMeta {
                    name: "audiences".to_string(),
                    description: "Its audiences.".to_string(),
                    tools: vec!["create_audience".to_string()],
                }],
                servers: vec![],
                tools: None,
                auth_failure: Default::default(),
                tool_sync_failure: Default::default(),
                approve: Default::default(),
                source: Default::default(),
            }],
            ..Default::default()
        };
        let mut s = SessionState::new("sess-1".to_string());
        apply(
            &mut s,
            1,
            EventPayload::ConfigUpdated(ConfigUpdated {
                config,
                node: crate::tree::Place {
                    id: uuid::Uuid::now_v7().to_string(),
                    parent_id: None,
                    branch: None,
                },
            }),
        );
        let owed = owed(s.on(&BranchId::MAIN));
        let plugin = owed
            .iter()
            .find(|c| c.id == "plugin.org")
            .expect("the plugin is listed");
        assert_eq!(
            plugin.content,
            r#"{"plugin":"org","about":"The org.","skills":[{"name":"org:audiences","description":"Its audiences.","tools":["create_audience"]}],"usage":"load a skill in `run_code` with `await tools.skill({ name })`"}"#
        );
    }
}
