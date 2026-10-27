use std::collections::{BTreeMap, HashSet};

use crate::plugins::SKILLS_BUNDLE;
use crate::protocol::{
    AgentConfig, AgentTool, ConnectorTool, Content, DraftMessage, LlmTool, Role, SkillListing,
    SkillSource, CALL_TOOL, SKILL, TOOL_SEARCH,
};

/// A skill as the model names it: bare for the agent's own, `<plugin>:<skill>`
/// for a plugin's.
#[derive(Debug, Clone, PartialEq)]
pub struct NamedSkill<'a> {
    pub name: String,
    pub description: &'a str,
    pub tools: &'a [String],
    pub source: SkillSource,
    pub plugin: Option<&'a str>,
    /// The bundle that holds it: its plugin's, or the project's skills bundle.
    pub bundle: &'a str,
    /// Its name in that bundle.
    pub skill: &'a str,
}

impl AgentTool {
    pub fn to_llm_tool(&self, default: bool) -> LlmTool {
        LlmTool {
            name: self.name.clone(),
            description: self.description.clone(),
            input: self.input.clone(),
            output: self.output.clone(),
            defer: self.defer.unwrap_or(default),
        }
    }
}

impl ConnectorTool {
    pub fn to_llm_tool(&self) -> LlmTool {
        LlmTool {
            name: self.name.clone(),
            description: self.description.clone(),
            input: self.input.clone(),
            output: self.output.clone(),
            defer: self.defer,
        }
    }
}

impl AgentConfig {
    pub fn tools_as_llm(&self) -> Option<Vec<LlmTool>> {
        if self.tools.is_empty() {
            None
        } else {
            Some(
                self.tools
                    .iter()
                    .map(|t| t.to_llm_tool(self.defers_tools()))
                    .collect(),
            )
        }
    }

    pub fn with_client_tools(&self, client_tools: &[AgentTool]) -> Option<AgentConfig> {
        let mut tools = self.tools.clone();
        for t in client_tools {
            let taken = tools.iter().any(|e| e.name == t.name)
                || self.subagents.iter().any(|s| s.offered_name() == t.name);
            if !taken {
                tools.push(t.clone());
            }
        }
        if tools.len() == self.tools.len() {
            None
        } else {
            Some(AgentConfig {
                tools,
                ..self.clone()
            })
        }
    }

    pub fn tool(&self, tool_name: &str) -> Option<&AgentTool> {
        self.tools.iter().find(|t| t.name == tool_name)
    }

    pub fn named_skills(&self) -> Vec<NamedSkill<'_>> {
        let own = self.skills.iter().map(|s| NamedSkill {
            name: s.name.clone(),
            description: &s.description,
            tools: &s.tools,
            source: s.source,
            plugin: None,
            bundle: SKILLS_BUNDLE,
            skill: &s.name,
        });
        let in_plugins = self.plugins.iter().flat_map(|p| {
            p.skills.iter().map(move |s| NamedSkill {
                name: format!("{}:{}", p.id, s.name),
                description: &s.description,
                tools: &s.tools,
                source: p.source,
                plugin: Some(&p.id),
                bundle: &p.id,
                skill: &s.name,
            })
        });
        own.chain(in_plugins).collect()
    }

    pub fn named_skill(&self, name: &str) -> Option<NamedSkill<'_>> {
        self.named_skills().into_iter().find(|s| s.name == name)
    }

    pub fn has_skills(&self) -> bool {
        !self.plugins.is_empty() || !self.skills.is_empty()
    }

    pub fn searches_skills(&self) -> bool {
        self.defer_tools
            .is_some_and(|d| d.skills == SkillListing::Search)
    }

    /// Each tool a skill holds back while none of its skills is in `loaded`,
    /// with the skills that would bring it.
    pub fn locked_tools(&self, loaded: &HashSet<String>) -> BTreeMap<String, Vec<String>> {
        let skills = self.named_skills();
        let unlocked: HashSet<&str> = skills
            .iter()
            .filter(|s| loaded.contains(&s.name))
            .flat_map(|s| s.tools.iter().map(String::as_str))
            .collect();
        let mut locked: BTreeMap<String, Vec<String>> = BTreeMap::new();
        for skill in &skills {
            for tool in skill.tools {
                if unlocked.contains(tool.as_str())
                    || [SKILL, TOOL_SEARCH, CALL_TOOL].contains(&tool.as_str())
                {
                    continue;
                }
                locked
                    .entry(tool.clone())
                    .or_default()
                    .push(skill.name.clone());
            }
        }
        locked
    }

    pub fn prompt_for(&self, view: &[DraftMessage]) -> Vec<DraftMessage> {
        match &self.system {
            Some(system) => {
                let mut messages = Vec::with_capacity(view.len() + 1);
                messages.push(DraftMessage {
                    id: None,
                    role: Role::System,
                    content: Some(Content::Text(system.clone())),
                    tool_calls: None,
                    tool_call_id: None,
                    name: None,
                    reasoning: None,
                });
                messages.extend(view.iter().cloned());
                messages
            }
            None => view.to_vec(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{Handler, Subagent};

    fn sub(id: &str, description: &str) -> Subagent {
        Subagent {
            id: id.to_string(),
            description: description.to_string(),
            defer: None,
            prefix: None,
            mode: None,
        }
    }

    fn function_tool(name: &str, handler: Option<Handler>) -> AgentTool {
        AgentTool {
            name: name.to_string(),
            description: String::new(),
            input: None,
            output: None,
            handler,
            defer: None,
        }
    }

    fn config(tools: Vec<AgentTool>, subagents: Vec<Subagent>) -> AgentConfig {
        AgentConfig {
            llm: Some("claude".to_string()),
            model: "m".to_string(),
            tools,
            subagents,
            ..Default::default()
        }
    }

    #[test]
    fn no_tools_offers_nothing() {
        assert!(config(vec![], vec![]).tools_as_llm().is_none());
        assert!(
            config(vec![], vec![sub("researcher", "")])
                .tools_as_llm()
                .is_none(),
            "subagent tools ride with the connector tools, not the declared list"
        );
    }

    #[test]
    fn tools_as_llm_offers_the_declared_tools() {
        let cfg = config(
            vec![function_tool("get_time", None)],
            vec![sub("researcher", "")],
        );
        let names: Vec<_> = cfg
            .tools_as_llm()
            .expect("declared ⇒ some")
            .iter()
            .map(|t| t.name.clone())
            .collect();
        assert_eq!(names, ["get_time"]);
    }

    #[test]
    fn a_prefixed_subagent_is_offered_under_the_agent_prefix() {
        let mut prefixed = sub("researcher", "");
        prefixed.prefix = Some(true);
        assert_eq!(prefixed.offered_name(), "agent__researcher");
        assert_eq!(sub("researcher", "").offered_name(), "researcher");
    }

    #[test]
    fn with_client_tools_appends_new_tools_and_is_idempotent() {
        let cfg = config(vec![function_tool("get_time", None)], vec![]);
        let client = [function_tool("get_tz", Some(Handler::Client))];
        let merged = cfg
            .with_client_tools(&client)
            .expect("a new tool ⇒ a rewrite");
        assert_eq!(
            merged
                .tools
                .iter()
                .map(|t| t.name.clone())
                .collect::<Vec<_>>(),
            ["get_time", "get_tz"],
            "client tools appended after config tools"
        );
        assert_eq!(
            merged.tool("get_tz").and_then(|t| t.handler),
            Some(Handler::Client)
        );
        assert!(
            merged.with_client_tools(&client).is_none(),
            "already present ⇒ no rewrite"
        );
        assert!(
            cfg.with_client_tools(&[]).is_none(),
            "nothing declared ⇒ no rewrite"
        );
    }

    #[test]
    fn with_client_tools_never_shadows_a_taken_name() {
        let cfg = config(
            vec![function_tool("confirm", Some(Handler::Client))],
            vec![sub("researcher", "")],
        );
        let client = [
            function_tool("confirm", Some(Handler::Client)),
            function_tool("researcher", Some(Handler::Client)),
        ];
        assert!(
            cfg.with_client_tools(&client).is_none(),
            "names taken by a tool or subagent are skipped"
        );
    }

    #[test]
    fn lookups_keep_the_two_namespaces_distinct() {
        let cfg = config(
            vec![function_tool("confirm", Some(Handler::Client))],
            vec![sub("researcher", "")],
        );
        assert_eq!(
            cfg.tool("confirm").and_then(|t| t.handler),
            Some(Handler::Client)
        );
        assert!(
            cfg.tool("researcher").is_none(),
            "a subagent is not a function tool"
        );
    }

    #[test]
    fn a_tool_two_skills_name_comes_with_either() {
        use crate::protocol::{AgentPlugin, AgentSkill, SkillMeta};
        let config = AgentConfig {
            skills: vec![AgentSkill {
                name: "automations".to_string(),
                description: String::new(),
                source: Default::default(),
                tools: vec!["get_audience".to_string(), "edit_automation".to_string()],
            }],
            plugins: vec![AgentPlugin {
                id: "org".to_string(),
                description: String::new(),
                skills: vec![SkillMeta {
                    name: "audiences".to_string(),
                    description: String::new(),
                    tools: vec!["get_audience".to_string(), "skill".to_string()],
                }],
                servers: vec![],
                tools: None,
                auth_failure: Default::default(),
                tool_sync_failure: Default::default(),
                approve: Default::default(),
                source: Default::default(),
            }],
            ..config(vec![], vec![])
        };
        let locked = config.locked_tools(&HashSet::new());
        assert_eq!(
            locked.get("get_audience"),
            Some(&vec![
                "automations".to_string(),
                "org:audiences".to_string()
            ])
        );
        assert!(
            !locked.contains_key("skill"),
            "an engine tool is never held back"
        );

        let loaded = HashSet::from(["org:audiences".to_string()]);
        let locked = config.locked_tools(&loaded);
        assert!(
            !locked.contains_key("get_audience"),
            "either skill brings it"
        );
        assert!(locked.contains_key("edit_automation"));
    }
}
