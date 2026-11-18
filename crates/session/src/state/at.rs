use super::*;

impl SessionState {
    pub fn leaf(&self, branch: &BranchId) -> Option<&str> {
        let open = self.branches.get(branch)?;
        open.leaf
            .as_deref()
            .or_else(|| open.run.as_ref()?.from.as_deref())
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

#[derive(Clone, Copy)]
pub struct At<'a, 'n> {
    state: &'a View,
    node: Option<&'n str>,
}

impl<'a, 'n> At<'a, 'n> {
    pub(super) fn new(state: &'a View, node: Option<&'n str>) -> Self {
        Self { state, node }
    }

    pub fn session(&self) -> &'a View {
        self.state
    }

    pub fn node(&self) -> Option<&'n str> {
        self.node
    }

    pub(crate) fn up(&self) -> crate::tree::Ancestors<'a> {
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

    pub fn worker_state(&self) -> WorkerState {
        self.up()
            .find_map(|n| match &n.fact {
                crate::tree::Fact::State { state } => Some(state.clone()),
                _ => None,
            })
            .unwrap_or_default()
    }

    pub fn config(&self) -> Option<AgentConfig> {
        self.up()
            .find_map(|n| match &n.fact {
                crate::tree::Fact::Config { config } => Some(Some(config.clone())),
                crate::tree::Fact::Reset { config } => Some(config.clone()),
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
        let config = self.config();
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
        self.config().and_then(|c| c.retry).map(|b| *b)
    }

    pub fn connector_tools(&self) -> filter::Merged {
        let Some(config) = self.config() else {
            return filter::Merged {
                tools: Vec::new(),
                collisions: Vec::new(),
            };
        };
        self.state.connector_tools_for_config(&config)
    }

    fn connector_source(&self, connector_id: &ConnectionPath) -> Option<Source> {
        let config = self.config()?;
        let server = self
            .state
            .servers_for(&config)
            .into_iter()
            .find(|c| c.path == *connector_id)?;
        let sync = self.state.connection(connector_id)?;
        Some(Source {
            server,
            offered: sync.tools.clone(),
            instructions: sync.instructions.clone(),
        })
    }

    fn searchable_connectors(&self) -> Vec<Source> {
        let Some(config) = self.config() else {
            return Vec::new();
        };
        self.state
            .servers_for(&config)
            .iter()
            .filter(|c| {
                self.state
                    .connection(&c.path)
                    .is_some_and(|s| s.status == Synced::Ready)
            })
            .filter_map(|c| self.connector_source(&c.path))
            .collect()
    }

    /// The tools `tool_search` reaches: the agent's own and its connections',
    /// but those a skill holds back. In code mode, these are the tools a
    /// script calls.
    pub fn searchable_tools(&self) -> Vec<LlmTool> {
        let Some(config) = self.config() else {
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

    pub fn run_code(&self) -> Option<LlmTool> {
        let language = self.config()?.codemode?;
        let tools = self.searchable_tools();
        let direct: Vec<&LlmTool> = tools.iter().filter(|t| !t.defer).collect();
        Some(crate::codemode::offered(language, &direct))
    }

    /// The tools a script can call that the skill `named` names.
    pub fn skill_tools(&self, named: &str) -> Vec<LlmTool> {
        let Some(config) = self.config() else {
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
                .filter(|e| SessionState::anchored_on(&path, e.asked_at.as_deref()))
                .filter(|e| e.tracking.status() == EffectStatus::Completed)
                .filter_map(EffectState::tool)
                .filter(|tc| tc.name == SKILL)
                .filter_map(|tc| named(&tc.arguments)),
        );
        loaded
    }

    pub fn locked_tools(&self) -> BTreeMap<String, Vec<String>> {
        self.config()
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
                let connection = self.state.connection(&c.path)?;
                (connection.status == Synced::Failed).then(|| (c.path.clone(), connection.auth))
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
                crate::tree::Fact::Reset { .. } => break,
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

    pub fn call_tool_target(&self, named: &str) -> Option<CallTarget> {
        let config = self.config()?;
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
            let config = self.config();
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
        let Some(config) = self.config() else {
            return Vec::new();
        };
        self.state
            .servers_for(&config)
            .iter()
            .filter(|c| !self.state.connections.contains_key(&c.path))
            .map(|c| c.path.clone())
            .collect()
    }

    pub fn has_pending_connector_sync(&self) -> bool {
        let Some(config) = self.config() else {
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
}

impl View {
    #[cfg(test)]
    pub fn placed(&self) -> Vec<prompt_context::Placed> {
        let mut nodes: Vec<&crate::tree::NodeRef> = self.tree.unwritten().collect();
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

    pub fn message_tree(&self) -> MessageTree {
        self.on(&BranchId::MAIN).message_tree()
    }

    pub fn on(&self, branch: &BranchId) -> At<'_, '_> {
        self.at(self.leaf(branch))
    }

    pub fn path_ids(&self, node: &str) -> std::collections::HashSet<String> {
        self.tree.path_ids(node)
    }

    pub fn has_node(&self, id: &str) -> bool {
        self.tree.has(id)
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

#[cfg(test)]
mod tree_tests {
    use super::*;
    use crate::tree::Landing;
    use crate::tree::{Fact, Node, Place};

    fn asked() -> crate::tree::Asked {
        crate::tree::Asked {
            llm: "llm".into(),
            model: "m1".into(),
            temperature: None,
            max_completion_tokens: None,
            reasoning: None,
            stream: false,
            retry: crate::protocol::RetryPolicy::no_retry(),
            tools: crate::tree::Tools::Set(None),
        }
    }

    fn config(model: &str) -> AgentConfig {
        serde_json::from_value(serde_json::json!({ "model": model })).unwrap()
    }

    fn user(id: &str) -> Message {
        serde_json::from_value(serde_json::json!({ "id": id, "role": "user", "content": id }))
            .unwrap()
    }

    fn put(s: &mut View, id: &str, parent: Option<&str>, fact: Fact) {
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

    fn read(s: &View, at: &str) -> serde_json::Value {
        s.at(Some(at)).worker_state().0
    }

    #[test]
    fn the_nearest_write_above_a_node_is_what_it_reads() {
        let mut s = View::new("s".into());
        put(
            &mut s,
            "c0",
            None,
            Fact::Config {
                config: config("m1"),
            },
        );
        put(&mut s, "s1", Some("c0"), state(1));
        put(&mut s, "u1", Some("s1"), message("u1"));
        put(&mut s, "s2", Some("u1"), state(2));
        put(&mut s, "u2", Some("s2"), message("u2"));
        assert_eq!(read(&s, "u1"), serde_json::json!({ "v": 1 }));
        assert_eq!(read(&s, "u2"), serde_json::json!({ "v": 2 }));
        assert_eq!(
            s.at(Some("u2")).config().map(|c| c.model),
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
        let mut s = View::new("s".into());
        put(&mut s, "u1", None, message("u1"));
        put(&mut s, "s1", Some("u1"), state(1));
        put(&mut s, "x1", Some("u1"), message("x1"));
        assert_eq!(read(&s, "s1"), serde_json::json!({ "v": 1 }));
        assert_eq!(read(&s, "x1"), serde_json::Value::Null);
    }

    #[test]
    fn a_branch_with_its_own_config_reads_it_and_inherits_state() {
        let mut s = View::new("s".into());
        put(
            &mut s,
            "c0",
            None,
            Fact::Config {
                config: config("m1"),
            },
        );
        put(&mut s, "s1", Some("c0"), state(1));
        put(&mut s, "u1", Some("s1"), message("u1"));
        let start = |config| Fact::Reset { config };
        put(&mut s, "b1", Some("u1"), start(Some(config("m2"))));
        put(&mut s, "b2", Some("u1"), start(None));
        let model = |at| s.at(Some(at)).config().map(|c| c.model);
        assert_eq!(model("b1"), Some("m2".into()));
        assert_eq!(model("b2"), None);
        assert_eq!(model("u1"), Some("m1".into()));
        assert_eq!(read(&s, "b1"), serde_json::json!({ "v": 1 }));
    }

    #[test]
    fn an_update_keeps_what_was_placed_above_and_a_reset_cuts_it() {
        let mut s = View::new("s".into());
        put(
            &mut s,
            "c0",
            None,
            Fact::Config {
                config: config("m1"),
            },
        );
        put(&mut s, "s1", Some("c0"), state(1));
        put(&mut s, "u1", Some("s1"), message("u1"));
        let placed = crate::prompt_context::Placed {
            call: "call-1".into(),
            message: "call-1-ctx".into(),
            placement: crate::prompt_context::Placement::System,
            content: "a server".into(),
            after: None,
            ids: vec!["a server".into()],
        };
        put(
            &mut s,
            "x1",
            Some("u1"),
            Fact::Context {
                call: "call-1".into(),
                placed: vec![placed],
                tools: crate::tree::Tools::Set(None),
                asked: asked(),
            },
        );
        put(
            &mut s,
            "up",
            Some("x1"),
            Fact::Config {
                config: config("m2"),
            },
        );
        put(
            &mut s,
            "re",
            Some("x1"),
            Fact::Reset {
                config: Some(config("m3")),
            },
        );

        let updated = s.at(Some("up"));
        assert_eq!(updated.placed_on_path().len(), 1);
        assert!(!updated.system_prefix_open());

        let reset = s.at(Some("re"));
        assert!(reset.placed_on_path().is_empty());
        assert!(reset.system_prefix_open());
        assert_eq!(reset.config().map(|c| c.model), Some("m3".into()));
        assert_eq!(read(&s, "re"), serde_json::json!({ "v": 1 }));
    }

    #[test]
    fn context_owed_after_an_earlier_call_takes_a_message_of_its_own() {
        let mut s = View::new("s".into());
        let skilled = AgentConfig {
            skills: vec![crate::protocol::AgentSkill {
                name: "visits".to_string(),
                description: "Who visited".to_string(),
                source: crate::protocol::SkillSource::Bundle,
                tools: Vec::new(),
            }],
            ..config("m1")
        };
        put(&mut s, "c0", None, Fact::Config { config: skilled });
        put(&mut s, "u1", Some("c0"), message("u1"));
        put(
            &mut s,
            "x1",
            Some("u1"),
            Fact::Context {
                call: "call-1".into(),
                placed: Vec::new(),
                tools: crate::tree::Tools::Set(None),
                asked: asked(),
            },
        );
        let text = |prompt: Vec<Message>| -> Vec<String> {
            prompt
                .iter()
                .map(|m| {
                    m.content
                        .as_ref()
                        .map(|c| c.text_owned())
                        .unwrap_or_default()
                })
                .collect()
        };
        let sent = text(s.at(Some("x1")).prompt("call-1"));
        let placing = s.at(Some("x1")).place("call-2");
        assert!(!placing.placed.is_empty(), "the catalog is owed");
        assert!(placing
            .placed
            .iter()
            .all(|p| p.placement == crate::prompt_context::Placement::Own));
        let next = text(placing.prompt);
        assert_eq!(
            next[..sent.len()],
            sent[..],
            "the earlier prompt is a prefix"
        );
    }

    #[test]
    fn a_call_records_its_tools_once_and_refers_back_while_they_hold() {
        let tool = |name: &str| LlmTool {
            name: name.into(),
            description: String::new(),
            input: None,
            output: None,
            defer: false,
        };
        let context = |tools| Fact::Context {
            call: "call".into(),
            placed: Vec::new(),
            tools,
            asked: asked(),
        };
        let mut s = View::new("s".into());
        put(
            &mut s,
            "c0",
            None,
            Fact::Config {
                config: config("m1"),
            },
        );
        put(&mut s, "u1", Some("c0"), message("u1"));
        let at = s.at(Some("u1"));
        assert_eq!(
            at.sends(Some(vec![tool("a")])).1,
            crate::tree::Tools::Set(Some(vec![tool("a")]))
        );
        put(
            &mut s,
            "x1",
            Some("u1"),
            context(crate::tree::Tools::Set(Some(vec![tool("a")]))),
        );
        put(
            &mut s,
            "x2",
            Some("x1"),
            context(crate::tree::Tools::As("x1".into())),
        );
        let at = s.at(Some("x2"));
        assert_eq!(
            at.sends(Some(vec![tool("a")])).1,
            crate::tree::Tools::As("x1".into())
        );
        assert_eq!(
            at.sends(Some(vec![tool("b")])).1,
            crate::tree::Tools::Set(Some(vec![tool("b")]))
        );
        assert_eq!(s.tree.tools("x2"), Some(vec![tool("a")]));
    }

    #[test]
    fn what_a_call_asked_and_was_sent_is_read_from_the_path() {
        let tool = |name: &str| LlmTool {
            name: name.into(),
            description: String::new(),
            input: None,
            output: None,
            defer: false,
        };
        let mut s = View::new("s".into());
        put(&mut s, "u1", None, message("u1"));
        put(
            &mut s,
            "x1",
            Some("u1"),
            Fact::Context {
                call: "call-1".into(),
                placed: Vec::new(),
                tools: crate::tree::Tools::Set(Some(vec![tool("skill")])),
                asked: crate::tree::Asked {
                    temperature: Some(0.5),
                    tools: crate::tree::Tools::Set(Some(vec![tool("find"), tool("skill")])),
                    ..asked()
                },
            },
        );
        put(&mut s, "call-1", Some("x1"), message("call-1"));
        put(&mut s, "u2", Some("call-1"), message("u2"));

        let sent = s.at(Some("u2")).sent("call-1").expect("on the path");
        assert_eq!(sent.asked.model, "m1");
        assert_eq!(sent.asked.temperature, Some(0.5));
        assert_eq!(sent.asked_tools, Some(vec![tool("find"), tool("skill")]));
        assert_eq!(sent.tools, Some(vec![tool("skill")]));
        assert!(s.llm_call("call-1").is_none(), "no effect holds the call");
        assert_eq!(s.tools_sent("call-1"), Some(vec![tool("skill")]));
        assert!(s.at(Some("u1")).sent("call-1").is_none(), "nor above it");
    }

    fn land(s: &View, view: &[&str], leaf: &str, floor: Option<&str>, regenerate: bool) -> Landing {
        let view: Vec<DraftMessage> = view
            .iter()
            .map(|id| {
                let role = match id.starts_with('a') {
                    true => "assistant",
                    false => "user",
                };
                serde_json::from_value(serde_json::json!({ "id": id, "role": role, "content": id }))
                    .unwrap()
            })
            .collect();
        let onto = crate::tree::Onto {
            branch: &BranchId::MAIN,
            leaf: Some(leaf),
            floor,
        };
        s.tree.landing(&view, onto, regenerate)
    }

    fn landed(at: Option<&str>, shared: usize) -> Landing {
        Landing {
            at: at.map(str::to_string),
            shared,
        }
    }

    #[test]
    fn a_view_lands_after_what_it_shares_and_the_facts_that_trail_it() {
        let mut s = View::new("s".into());
        put(&mut s, "u1", None, message("u1"));
        put(&mut s, "s1", Some("u1"), state(1));
        put(&mut s, "u2", Some("s1"), message("u2"));
        put(&mut s, "s2", Some("u2"), state(2));
        put(&mut s, "e2", Some("u1"), message("e2"));
        put(&mut s, "x2", Some("e2"), state(3));

        assert_eq!(
            land(&s, &["u1"], "s2", None, false),
            landed(Some("s1"), 1),
            "on the path: just above the next message"
        );
        assert_eq!(
            land(&s, &["u1", "u2"], "s2", None, false),
            landed(Some("s2"), 2),
            "at the end of the path: the leaf"
        );
        assert_eq!(
            land(&s, &["u1", "e2"], "s2", None, false),
            landed(Some("x2"), 2),
            "another version: the end of it"
        );
        assert_eq!(
            land(&s, &["n1"], "s2", None, false),
            landed(None, 0),
            "nothing shared: above the first message"
        );
        assert_eq!(
            land(&s, &["n1"], "s2", Some("s1"), false),
            landed(Some("s1"), 0),
            "never above the fork point"
        );
        assert!(s
            .tree
            .message_follows(Some("s1"), Some("s2"), &BranchId::MAIN));
        assert!(!s
            .tree
            .message_follows(Some("s2"), Some("s2"), &BranchId::MAIN));
    }

    #[test]
    fn a_regenerated_view_lands_after_its_last_reply() {
        let mut s = View::new("s".into());
        put(&mut s, "u1", None, message("u1"));
        put(
            &mut s,
            "a1",
            Some("u1"),
            Fact::Message {
                message: serde_json::from_value(
                    serde_json::json!({ "id": "a1", "role": "assistant", "content": "a1" }),
                )
                .unwrap(),
                copied_from: None,
            },
        );
        put(&mut s, "u2", Some("a1"), message("u2"));
        assert_eq!(
            land(&s, &["u1", "a1", "u2"], "u2", None, true),
            landed(Some("a1"), 2)
        );
    }

    #[test]
    fn a_reset_to_no_config_reads_none() {
        let mut s = View::new("s".into());
        put(
            &mut s,
            "c0",
            None,
            Fact::Config {
                config: config("m1"),
            },
        );
        put(&mut s, "u1", Some("c0"), message("u1"));
        put(&mut s, "re", Some("u1"), Fact::Reset { config: None });
        assert!(s.at(Some("re")).config().is_none());
        assert_eq!(
            s.at(Some("u1")).config().map(|c| c.model),
            Some("m1".into())
        );
    }

    #[test]
    fn the_message_view_skips_other_nodes() {
        let mut s = View::new("s".into());
        put(&mut s, "u1", None, message("u1"));
        put(&mut s, "s1", Some("u1"), state(1));
        put(&mut s, "u2", Some("s1"), message("u2"));
        let tree = s.message_tree();
        assert_eq!(tree.head_id.as_deref(), Some("u2"));
        let parents: Vec<_> = tree.nodes.iter().map(|n| n.parent_id.clone()).collect();
        assert_eq!(parents, vec![None, Some("u1".to_string())]);
    }
}
