use super::*;

impl SessionState {
    pub fn at<'a, 'n>(&'a self, node: Option<&'n str>) -> SessionStateAtNode<'a, 'n> {
        SessionStateAtNode { state: self, node }
    }

    pub fn at_head(&self) -> SessionStateAtNode<'_, '_> {
        self.at(self.head_id.as_deref())
    }

    pub fn at_scope<'a>(&'a self, branch: Option<&'a str>) -> SessionStateAtNode<'a, 'a> {
        match branch.and_then(|b| self.branches.get(b)) {
            Some(b) => self.at(b.at()),
            None => self.at_head(),
        }
    }

    pub fn scope_node(&self, branch: Option<&str>) -> Option<String> {
        match branch.and_then(|b| self.branches.get(b)) {
            Some(b) => b.at().map(str::to_string),
            None => self.head_id.clone(),
        }
    }

    pub fn outgoing(&self, id: &str) -> Option<Outgoing> {
        let effect = self.effect(EffectKind::LlmCall, id)?;
        let call = effect.llm()?;
        let node = call.context.clone().or_else(|| match &call.source {
            PromptSource::Tree { at } => at.clone(),
            _ => effect.anchor.clone().or_else(|| self.head_id.clone()),
        });
        Some(
            self.at(node.as_deref())
                .outgoing(id, &call.source, &call.prompt),
        )
    }

    pub fn path_ids(&self, node: &str) -> std::collections::HashSet<String> {
        self.tree.path_ids(node)
    }

    #[cfg(test)]
    pub fn placed(&self) -> Vec<prompt_context::Placed> {
        let mut nodes: Vec<&crate::tree::Shared> = self.tree.fresh().collect();
        nodes.sort_by_key(|n| n.seq);
        nodes
            .into_iter()
            .filter_map(|n| match &n.fact {
                crate::tree::Fact::Context { placed, .. } => Some(placed.clone()),
                _ => None,
            })
            .flatten()
            .collect()
    }

    pub fn has_node(&self, id: &str) -> bool {
        self.tree.has(id)
    }

    pub fn message_tree(&self) -> MessageTree {
        self.at_head().message_tree()
    }

    pub fn versions(&self, tree: &MessageTree) -> BTreeMap<String, Vec<String>> {
        tree.nodes
            .iter()
            .filter_map(|n| {
                let versions = self.tree.versions(&n.message.id);
                (versions.len() > 1).then(|| (n.message.id.clone(), versions))
            })
            .collect()
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Source {
    pub server: BoundServer,
    pub offered: Vec<RemoteTool>,
    pub instructions: Option<String>,
}

#[derive(serde::Serialize)]
struct Summary<'a> {
    mcp_server: &'a ConnectionPath,
    tools: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    about: Option<&'a str>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum CallTarget {
    Connector(ConnectorTool),
    Declared(AgentTool),
    Subagent(ConnectorTool),
}

impl CallTarget {
    pub fn input(&self) -> Option<serde_json::Value> {
        match self {
            CallTarget::Connector(t) | CallTarget::Subagent(t) => t.input.clone(),
            CallTarget::Declared(t) => t.input.clone(),
        }
    }
}

pub fn inner_arguments(raw: &serde_json::Value) -> String {
    match raw.get("arguments") {
        Some(serde_json::Value::String(text)) => text.clone(),
        Some(value) => value.to_string(),
        None => "{}".to_string(),
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct SkillCall {
    pub node: Option<String>,
    pub plugin_id: String,
    pub arguments: String,
    /// A script made the call.
    pub scripted: bool,
}

#[derive(Debug, Clone)]
pub struct Outgoing {
    pub node: Option<String>,
    pub prompt: Vec<Message>,
    pub placed: Vec<prompt_context::Placed>,
    pub places: bool,
}

#[derive(Clone, Copy)]
pub struct SessionStateAtNode<'a, 'n> {
    state: &'a SessionState,
    node: Option<&'n str>,
}

impl<'a, 'n> SessionStateAtNode<'a, 'n> {
    pub fn state(&self) -> &'a SessionState {
        self.state
    }

    pub fn node(&self) -> Option<&'n str> {
        self.node
    }

    fn up(&self) -> crate::tree::Up<'a> {
        self.state.tree.up(self.node)
    }

    pub fn message_tree(&self) -> MessageTree {
        let mut nodes: Vec<NewMessage> = Vec::new();
        for node in self.up() {
            let crate::tree::Fact::Message {
                message,
                copied_from,
            } = &node.fact
            else {
                continue;
            };
            if let Some(child) = nodes.last_mut() {
                child.parent_id = Some(message.id.clone());
            }
            nodes.push(NewMessage {
                message: message.clone(),
                parent_id: None,
                copied_from: copied_from.clone(),
                branch: node.branch().map(str::to_string),
            });
        }
        nodes.reverse();
        MessageTree {
            head_id: nodes.last().map(|n| n.message.id.clone()),
            nodes,
        }
    }

    fn path_set(&self) -> std::collections::HashSet<String> {
        self.node
            .map(|n| self.state.path_ids(n))
            .unwrap_or_default()
    }

    pub fn active_interrupt_for(&self) -> Option<&'a OpenInterrupt> {
        if self.state.open_interrupts.is_empty() {
            return None;
        }
        resolve_on_path(&self.state.open_interrupts, &self.path_set())
    }

    pub fn interrupts_for(&self) -> Vec<&'a OpenInterrupt> {
        if self.state.open_interrupts.is_empty() {
            return Vec::new();
        }
        let on_path = self.path_set();
        self.state
            .open_interrupts
            .iter()
            .filter(|i| match i.anchor.as_deref() {
                None => true,
                Some(a) => on_path.contains(a),
            })
            .collect()
    }

    pub fn anchor_on_path(&self, anchor: Option<&str>) -> bool {
        SessionState::anchored_on(&self.path_set(), anchor)
    }

    pub fn resolve_state_for(&self) -> WorkerState {
        self.up()
            .find_map(|n| match &n.fact {
                crate::tree::Fact::State { state } => Some(state.clone()),
                _ => None,
            })
            .unwrap_or_default()
    }

    pub fn resolve_agent_for(&self) -> Option<AgentConfig> {
        self.up()
            .find_map(|n| match &n.fact {
                crate::tree::Fact::Config { config, .. } => Some(config.clone()),
                _ => None,
            })
            .flatten()
    }

    pub fn connector_tool(&self, name: &str) -> Option<ConnectorTool> {
        self.connector_tools()
            .tools
            .into_iter()
            .find(|t| t.name == name)
    }

    pub fn tool_handler(&self, name: &str) -> ToolHandler {
        let config = self.resolve_agent_for();
        let connector = || {
            self.connector_tool(name)
                .is_some_and(|t| t.kind != ConnectorToolKind::Subagent)
        };
        match config.as_ref().and_then(|c| c.tool(name)) {
            Some(t) => ToolHandler::declared(t.handler),
            None if connector() => ToolHandler::Server,
            None => ToolHandler::Worker,
        }
    }

    pub fn retry_config(&self) -> Option<RetryConfig> {
        self.resolve_agent_for().and_then(|c| c.retry).map(|b| *b)
    }

    pub fn connector_tools(&self) -> filter::Merged {
        let Some(config) = self.resolve_agent_for() else {
            return filter::Merged {
                tools: Vec::new(),
                collisions: Vec::new(),
            };
        };
        self.state.connector_tools_for_config(&config)
    }

    fn connector_source(&self, connector_id: &ConnectionPath) -> Option<Source> {
        let config = self.resolve_agent_for()?;
        let server = self
            .state
            .servers_for(&config)
            .into_iter()
            .find(|c| c.path == *connector_id)?;
        let sync = self.state.connector_sync(connector_id)?;
        Some(Source {
            server,
            offered: sync.tools.clone(),
            instructions: sync.instructions.clone(),
        })
    }

    fn searchable_connectors(&self) -> Vec<Source> {
        let Some(config) = self.resolve_agent_for() else {
            return Vec::new();
        };
        self.state
            .servers_for(&config)
            .iter()
            .filter(|c| {
                self.state
                    .effect(EffectKind::ConnectorSync, &c.path.to_string())
                    .is_some_and(|e| e.tracking.is_ready())
            })
            .filter_map(|c| self.connector_source(&c.path))
            .collect()
    }

    /// The tools `tool_search` reaches: the agent's own and its connections',
    /// but those a skill holds back. In code mode, these are the tools a
    /// script calls.
    pub fn searchable_tools(&self) -> Vec<LlmTool> {
        let Some(config) = self.resolve_agent_for() else {
            return Vec::new();
        };
        let locked = config.locked_tools(&self.loaded_skills());
        let connector_tools = self.state.connector_tools_for_config(&config).tools;
        let declared = config
            .tools
            .iter()
            .map(|t| t.to_llm_tool(config.defers_tools()));
        let connector = connector_tools
            .iter()
            .filter(|t| {
                matches!(
                    t.kind,
                    ConnectorToolKind::Remote | ConnectorToolKind::Subagent
                )
            })
            .map(|t| t.to_llm_tool());
        declared
            .chain(connector)
            .filter(|t| !locked.contains_key(&t.name))
            .collect()
    }

    /// In code mode, the one tool the model gets: `run_code`, declaring the
    /// tools a script calls that are not deferred. It does not change while
    /// the session runs, but for tools that connect later.
    pub fn run_code(&self) -> Option<LlmTool> {
        let language = self.resolve_agent_for()?.codemode?;
        let tools = self.searchable_tools();
        let direct: Vec<&LlmTool> = tools.iter().filter(|t| !t.defer).collect();
        Some(crate::codemode::offered(language, &direct))
    }

    /// The tools a script can call that the skill `named` names.
    pub fn skill_tools(&self, named: &str) -> Vec<LlmTool> {
        let Some(config) = self.resolve_agent_for() else {
            return Vec::new();
        };
        let named_tools: Vec<String> = config
            .named_skills()
            .iter()
            .filter(|s| s.name == named)
            .flat_map(|s| s.tools.iter().cloned())
            .collect();
        self.searchable_tools()
            .into_iter()
            .filter(|t| named_tools.contains(&t.name))
            .collect()
    }

    /// The skills loaded on this path: each `skill` call that answered.
    pub fn loaded_skills(&self) -> std::collections::HashSet<String> {
        let named = |arguments: &str| -> Option<String> {
            let arguments: serde_json::Value = serde_json::from_str(arguments).ok()?;
            Some(arguments.get("name")?.as_str()?.to_string())
        };
        let mut answered = std::collections::HashSet::new();
        let mut loaded = std::collections::HashSet::new();
        for node in self.up() {
            let Some(m) = node.message() else { continue };
            if let Some(id) = &m.tool_call_id {
                answered.insert(id.clone());
            }
            loaded.extend(
                m.tool_calls
                    .iter()
                    .filter(|c| c.function.name == SKILL && answered.contains(&c.id))
                    .filter_map(|c| named(&c.function.arguments)),
            );
        }
        let path = self.path_set();
        loaded.extend(
            self.state
                .effects
                .values()
                .filter(|e| SessionState::anchored_on(&path, e.anchor.as_deref()))
                .filter(|e| e.tracking.status() == EffectStatus::Completed)
                .filter_map(EffectState::tool)
                .filter(|tc| tc.name == SKILL)
                .filter_map(|tc| named(&tc.arguments)),
        );
        loaded
    }

    pub fn locked_tools(&self) -> BTreeMap<String, Vec<String>> {
        self.resolve_agent_for()
            .map(|config| config.locked_tools(&self.loaded_skills()))
            .unwrap_or_default()
    }

    pub fn locked_fault(&self, named: &str) -> Option<String> {
        let locked = self.locked_tools();
        Some(crate::prompts::errors::locked(named, locked.get(named)?))
    }

    pub fn connection_summary(&self, path: &ConnectionPath) -> Option<String> {
        let source = self
            .searchable_connectors()
            .into_iter()
            .find(|source| source.server.path == *path)?;
        serde_json::to_string(&Summary {
            mcp_server: &source.server.path,
            tools: filter::callable(&source.server, &source.offered).len(),
            about: source.instructions.as_deref(),
        })
        .ok()
    }

    pub fn unavailable_connectors(
        &self,
        config: &AgentConfig,
    ) -> Vec<(ConnectionPath, Option<AuthNeed>)> {
        self.state
            .servers_for(config)
            .into_iter()
            .filter(|c| c.tool_sync_failure.warns())
            .filter_map(|c| {
                let effect = self
                    .state
                    .effect(EffectKind::ConnectorSync, &c.path.to_string())?;
                if effect.tracking.status() != EffectStatus::Failed {
                    return None;
                }
                Some((c.path.clone(), effect.connector().and_then(|s| s.auth)))
            })
            .collect()
    }

    pub fn unavailable_connector_paths(&self, config: &AgentConfig) -> Vec<ConnectionPath> {
        self.unavailable_connectors(config)
            .into_iter()
            .map(|(path, _)| path)
            .collect()
    }

    pub fn placed_on_path(&self) -> Vec<prompt_context::Placed> {
        let mut placed: Vec<Vec<prompt_context::Placed>> = Vec::new();
        for node in self.up() {
            match &node.fact {
                crate::tree::Fact::Context { placed: here, .. } => placed.push(here.clone()),
                crate::tree::Fact::Config { root: true, .. } => break,
                _ => {}
            }
        }
        placed.into_iter().rev().flatten().collect()
    }

    pub fn said(&self) -> std::collections::HashSet<String> {
        self.placed_on_path()
            .into_iter()
            .flat_map(|p| p.ids)
            .collect()
    }

    pub fn outgoing(&self, id: &str, source: &PromptSource, given: &[Message]) -> Outgoing {
        let node = self.node.map(str::to_string);
        let mut out = Outgoing {
            node,
            prompt: given.to_vec(),
            placed: Vec::new(),
            places: false,
        };
        match source {
            PromptSource::Explicit => return out,
            PromptSource::Tree { .. } => {
                let view: Vec<DraftMessage> = self
                    .transcript()
                    .into_iter()
                    .map(DraftMessage::from)
                    .collect();
                let prompt = match self.resolve_agent_for() {
                    Some(config) => config.prompt_for(&view),
                    None => view,
                };
                out.prompt = prompt
                    .into_iter()
                    .enumerate()
                    .map(|(i, m)| m.recorded(format!("{id}:{i}")))
                    .collect();
                prompt_context::restore(&mut out.prompt, &self.placed_on_path());
                let open = self.system_prefix_open(id);
                prompt_context::merge(
                    &mut out.prompt,
                    &mut Vec::new(),
                    &mut out.placed,
                    id,
                    open,
                    prompt_context::owed(*self),
                );
                out.places = open || !out.placed.is_empty();
            }
            PromptSource::Legacy => {
                if !self.state.is_summary_call(id) {
                    prompt_context::carry(&mut out.prompt, &self.placed_on_path());
                }
                let open = self.system_prefix_open(id);
                prompt_context::merge(
                    &mut out.prompt,
                    &mut Vec::new(),
                    &mut out.placed,
                    id,
                    open,
                    prompt_context::owed(*self),
                );
                out.places = open || !out.placed.is_empty();
            }
        }
        out
    }

    pub fn system_prefix_open(&self, exclude: &str) -> bool {
        let mut path = std::collections::HashSet::new();
        for node in self.up() {
            match &node.fact {
                crate::tree::Fact::Context { call, .. } if call != exclude => return false,
                crate::tree::Fact::Config { root: true, .. } => break,
                _ => {}
            }
            path.insert(node.id().to_string());
        }
        !self.state.effects.values().any(|e| {
            e.id != exclude
                && e.llm()
                    .is_some_and(|c| c.source != PromptSource::Explicit && c.context.is_none())
                && SessionState::anchored_on(&path, e.anchor.as_deref())
                && e.tracking.status() != EffectStatus::Queued
        })
    }

    pub fn call_tool_target(&self, named: &str) -> Option<CallTarget> {
        let config = self.resolve_agent_for()?;
        if config
            .locked_tools(&self.loaded_skills())
            .contains_key(named)
        {
            return None;
        }
        if let Some(declared) = config.tool(named) {
            return Some(CallTarget::Declared(declared.clone()));
        }
        self.connector_tool(named).and_then(|t| match t.kind {
            ConnectorToolKind::Remote => Some(CallTarget::Connector(t)),
            ConnectorToolKind::Subagent => Some(CallTarget::Subagent(t)),
            _ => None,
        })
    }

    pub fn call_tool_fault(&self, arguments: &str) -> Option<String> {
        let raw: serde_json::Value = serde_json::from_str(arguments).unwrap_or_default();
        let named = raw
            .get("name")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string();

        if named == CALL_TOOL {
            return Some(crate::prompts::errors::call_tool_in_call());
        }
        if let Some(fault) = self.locked_fault(&named) {
            return Some(fault);
        }
        let Some(target) = self.call_tool_target(&named) else {
            let config = self.resolve_agent_for();
            let tools = self.searchable_tools();
            let cap = config
                .as_ref()
                .map(AgentConfig::defer_settings)
                .unwrap_or_default()
                .max_matches;
            let near = filter::closest(&tools, &named, cap);
            let unavailable = config
                .as_ref()
                .map(|c| self.unavailable_connector_paths(c))
                .unwrap_or_default();
            return Some(crate::prompts::errors::no_such_tool(
                &named,
                &near,
                &unavailable,
            ));
        };
        let input = target.input();
        let fault = match raw.get("arguments") {
            Some(_) => crate::prompts::errors::bad_arguments,
            None => crate::prompts::errors::no_arguments,
        };
        classify_arguments(&inner_arguments(&raw), input.as_ref())
            .error()
            .map(|e| fault(&named, e, input.as_ref()))
    }

    pub fn unsynced_connectors(&self) -> Vec<ConnectionPath> {
        let Some(config) = self.resolve_agent_for() else {
            return Vec::new();
        };
        self.state
            .servers_for(&config)
            .iter()
            .filter(|c| {
                !self
                    .state
                    .has_effect(EffectKind::ConnectorSync, &c.path.to_string())
            })
            .map(|c| c.path.clone())
            .collect()
    }

    pub fn has_pending_connector_sync(&self) -> bool {
        let Some(config) = self.resolve_agent_for() else {
            return false;
        };
        self.state.servers_for(&config).iter().any(|c| {
            self.state
                .tracking(EffectKind::ConnectorSync, &c.path.to_string())
                .is_some_and(EffectTracking::is_in_flight)
        })
    }

    pub fn transcript(&self) -> Vec<Message> {
        self.state.tree.messages(self.node)
    }

    pub fn open_llm_calls(&self) -> HashMap<String, EffectState> {
        let path = self.transcript();
        let answered: std::collections::HashSet<&str> = path
            .iter()
            .filter_map(|m| m.tool_call_id.as_deref())
            .collect();
        path.iter()
            .filter(|m| {
                m.tool_calls
                    .iter()
                    .any(|c| !answered.contains(c.id.as_str()))
            })
            .filter_map(|m| {
                self.state
                    .effect(EffectKind::LlmCall, &m.id)
                    .map(|e| (m.id.clone(), e.clone()))
            })
            .collect()
    }
}

#[cfg(test)]
mod open_llm_calls_tests {
    use super::*;
    use crate::protocol::{NewMessage, ToolCall, ToolCallFunction};

    fn node(message: Message, parent_id: Option<&str>) -> Logged<NewMessage> {
        Logged {
            seq: 0,
            entry: NewMessage {
                message,
                parent_id: parent_id.map(str::to_string),
                copied_from: None,
                branch: None,
            },
        }
    }

    fn user(id: &str) -> Message {
        Message {
            id: id.to_string(),
            role: Role::User,
            content: None,
            tool_calls: vec![],
            tool_call_id: None,
            name: None,
            reasoning: None,
        }
    }

    fn assistant_calling(id: &str, tool_call_id: &str) -> Message {
        Message {
            id: id.to_string(),
            role: Role::Assistant,
            content: None,
            tool_calls: vec![ToolCall {
                id: tool_call_id.to_string(),
                call_type: "function".to_string(),
                function: ToolCallFunction {
                    name: "t".to_string(),
                    arguments: "{}".to_string(),
                },
            }],
            tool_call_id: None,
            name: None,
            reasoning: None,
        }
    }

    fn tool_answer(id: &str, tool_call_id: &str) -> Message {
        Message {
            id: id.to_string(),
            role: Role::Tool,
            content: None,
            tool_calls: vec![],
            tool_call_id: Some(tool_call_id.to_string()),
            name: None,
            reasoning: None,
        }
    }

    fn call_state(call_id: &str) -> EffectState {
        EffectState::new(
            call_id,
            EffectTracking::new(RetryPolicy::no_retry(), Utc::now()),
            EffectPayload::LlmCall(LlmCallState {
                source: Default::default(),
                defer_tools_strategy: Default::default(),
                context: None,
                sent_tools: None,
                unanswered: Vec::new(),
                format: None,
                llm: "claude".to_string(),
                prompt: vec![],
                spec: LlmCallSpec {
                    model: "m".to_string(),
                    tools: None,
                    temperature: None,
                    max_completion_tokens: None,
                    reasoning: None,
                },
                stream: false,
                handler: LlmHandler::Server,
                dispatched_seq: None,
            }),
        )
    }

    #[test]
    fn a_call_is_open_until_its_tool_answer_is_recorded() {
        let mut s = SessionState::new("sess-1".to_string());
        s.seed_message(node(user("u1"), None).entry);
        s.seed_message(node(assistant_calling("call-1", "tc-1"), Some("u1")).entry);
        s.head_id = Some("call-1".to_string());
        s.put_effect(call_state("call-1"));

        assert!(
            s.at_head().open_llm_calls().contains_key("call-1"),
            "unanswered tool call keeps the parent open"
        );

        s.seed_message(node(tool_answer("t1", "tc-1"), Some("call-1")).entry);
        s.head_id = Some("t1".to_string());
        assert!(
            s.at_head().open_llm_calls().is_empty(),
            "recorded answer closes the call"
        );
    }
}

#[cfg(test)]
mod tree_tests {
    use super::*;
    use crate::tree::{Fact, Node, Place};

    fn config(model: &str) -> AgentConfig {
        serde_json::from_value(serde_json::json!({ "model": model })).unwrap()
    }

    fn user(id: &str) -> Message {
        serde_json::from_value(serde_json::json!({ "id": id, "role": "user", "content": id }))
            .unwrap()
    }

    fn put(s: &mut SessionState, id: &str, parent: Option<&str>, fact: Fact) {
        s.seed(Node {
            place: Place {
                id: id.to_string(),
                parent_id: parent.map(str::to_string),
                branch: None,
            },
            fact,
        });
    }

    fn state(v: i64) -> Fact {
        Fact::State {
            state: WorkerState(serde_json::json!({ "v": v })),
        }
    }

    fn message(id: &str) -> Fact {
        Fact::Message {
            message: user(id),
            copied_from: None,
        }
    }

    fn read(s: &SessionState, at: &str) -> serde_json::Value {
        s.at(Some(at)).resolve_state_for().0
    }

    #[test]
    fn the_nearest_write_above_a_node_is_what_it_reads() {
        let mut s = SessionState::new("s".into());
        put(
            &mut s,
            "c0",
            None,
            Fact::Config {
                config: Some(config("m1")),
                root: false,
            },
        );
        put(&mut s, "s1", Some("c0"), state(1));
        put(&mut s, "u1", Some("s1"), message("u1"));
        put(&mut s, "s2", Some("u1"), state(2));
        put(&mut s, "u2", Some("s2"), message("u2"));
        assert_eq!(read(&s, "u1"), serde_json::json!({ "v": 1 }));
        assert_eq!(read(&s, "u2"), serde_json::json!({ "v": 2 }));
        assert_eq!(
            s.at(Some("u2")).resolve_agent_for().map(|c| c.model),
            Some("m1".into())
        );
        assert_eq!(
            s.at(Some("u2")).transcript().len(),
            2,
            "only messages are the transcript"
        );
    }

    #[test]
    fn a_write_after_a_fork_is_not_on_the_fork() {
        let mut s = SessionState::new("s".into());
        put(&mut s, "u1", None, message("u1"));
        put(&mut s, "s1", Some("u1"), state(1));
        put(&mut s, "x1", Some("u1"), message("x1"));
        assert_eq!(read(&s, "s1"), serde_json::json!({ "v": 1 }));
        assert_eq!(read(&s, "x1"), serde_json::Value::Null);
    }

    #[test]
    fn a_branch_with_its_own_config_reads_it_and_inherits_state() {
        let mut s = SessionState::new("s".into());
        put(
            &mut s,
            "c0",
            None,
            Fact::Config {
                config: Some(config("m1")),
                root: false,
            },
        );
        put(&mut s, "s1", Some("c0"), state(1));
        put(&mut s, "u1", Some("s1"), message("u1"));
        let start = |config| Fact::Config { config, root: true };
        put(&mut s, "b1", Some("u1"), start(Some(config("m2"))));
        put(&mut s, "b2", Some("u1"), start(None));
        let model = |at| s.at(Some(at)).resolve_agent_for().map(|c| c.model);
        assert_eq!(model("b1"), Some("m2".into()));
        assert_eq!(model("b2"), None);
        assert_eq!(model("u1"), Some("m1".into()));
        assert_eq!(read(&s, "b1"), serde_json::json!({ "v": 1 }));
    }

    #[test]
    fn the_message_view_skips_other_nodes() {
        let mut s = SessionState::new("s".into());
        put(&mut s, "u1", None, message("u1"));
        put(&mut s, "s1", Some("u1"), state(1));
        put(&mut s, "u2", Some("s1"), message("u2"));
        let tree = s.message_tree();
        assert_eq!(tree.head_id.as_deref(), Some("u2"));
        let parents: Vec<_> = tree.nodes.iter().map(|n| n.parent_id.clone()).collect();
        assert_eq!(parents, vec![None, Some("u1".to_string())]);
    }
}

/// The tools a call sends: those it asked for, then its connections', but
/// those a skill holds back.
pub(super) fn sent_tools(
    asked: Option<Vec<LlmTool>>,
    connector_tools: &[ConnectorTool],
    locked: &BTreeMap<String, Vec<String>>,
) -> Option<Vec<LlmTool>> {
    let mut sent = asked;
    if !connector_tools.is_empty() {
        let tools = sent.get_or_insert_with(Vec::new);
        for tool in connector_tools {
            if !tools.iter().any(|t| t.name == tool.name) {
                tools.push(tool.to_llm_tool());
            }
        }
    }
    if let Some(tools) = sent.as_mut() {
        tools.retain(|t| !locked.contains_key(&t.name));
    }
    sent
}
