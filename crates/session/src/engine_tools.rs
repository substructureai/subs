use super::state::SessionStateAtNode;
use crate::connectors::filter;
use crate::plugins::{FileRef, PluginBundle, Skill, SkillFile};
use crate::prompts::{answers, errors};
use crate::protocol::{AgentConfig, ConnectorToolKind, StoredContent, StoredResult};

pub fn answer(
    at: SessionStateAtNode,
    kind: ConnectorToolKind,
    arguments: &str,
) -> Option<StoredResult> {
    match kind {
        ConnectorToolKind::Remote => None,
        ConnectorToolKind::Find => Some(find(at, arguments)),
        ConnectorToolKind::Call => Some(call(at, arguments)),
        ConnectorToolKind::Skill => None,
        ConnectorToolKind::Subagent => None,
        ConnectorToolKind::Attachment => None,
        ConnectorToolKind::Code => None,
    }
}

fn find(at: SessionStateAtNode, arguments: &str) -> StoredResult {
    let config = at.resolve_agent_for();
    let skills = config
        .as_ref()
        .filter(|c| c.searches_skills())
        .map(AgentConfig::named_skills)
        .unwrap_or_default();
    StoredResult::text(filter::find_answer(
        &at.searchable_tools(),
        &skills,
        &argument(arguments, "query"),
        config
            .as_ref()
            .map(AgentConfig::defer_settings)
            .unwrap_or_default()
            .max_matches,
        &config
            .as_ref()
            .map(|c| at.unavailable_connector_paths(c))
            .unwrap_or_default(),
    ))
}

/// A script's `tool_search`.
pub fn script_find(
    at: SessionStateAtNode,
    language: crate::codemode::Language,
    arguments: &str,
) -> StoredResult {
    let config = at.resolve_agent_for();
    let max = config
        .as_ref()
        .map(AgentConfig::defer_settings)
        .unwrap_or_default()
        .max_matches;
    let skills = config
        .as_ref()
        .filter(|c| c.searches_skills())
        .map(AgentConfig::named_skills)
        .unwrap_or_default();
    StoredResult::text(crate::codemode::search(
        language,
        &at.searchable_tools(),
        &skills,
        &argument(arguments, "query"),
        max,
    ))
}

fn call(at: SessionStateAtNode, arguments: &str) -> StoredResult {
    StoredResult::error(
        at.call_tool_fault(arguments)
            .unwrap_or_else(|| crate::prompts::errors::CALL_NOT_ROUTED.to_string()),
    )
}

fn argument(arguments: &str, key: &str) -> String {
    serde_json::from_str::<serde_json::Value>(arguments)
        .ok()
        .and_then(|v| v.get(key)?.as_str().map(str::to_string))
        .unwrap_or_default()
}

pub fn split_skill(named: &str) -> (&str, &str) {
    named.split_once(':').unwrap_or(("", named))
}

/// The skill a `skill` call names, by the name it gave, and the file it
/// asks for, if any.
fn requested<'b>(
    at: SessionStateAtNode,
    bundle: Option<&'b PluginBundle>,
    arguments: &str,
) -> Result<(String, &'b Skill, Option<String>), StoredResult> {
    let named = argument(arguments, "name");
    let file = Some(argument(arguments, "file")).filter(|f| !f.is_empty());
    let Some(config) = at.resolve_agent_for() else {
        return Err(StoredResult::error(errors::NO_CONFIG));
    };
    let Some(found) = config.named_skill(&named) else {
        return Err(StoredResult::error(no_such_skill(&config, &named)));
    };
    let Some(bundle) = bundle else {
        return Err(StoredResult::error(errors::skill_without_data(&named)));
    };
    let Some(skill) = bundle.skill(found.skill) else {
        return Err(StoredResult::error(errors::skill_not_in_data(&named)));
    };
    Ok((named, skill, file))
}

/// The stored text file a `skill` call asks for: the host reads it, and hands
/// its text to [`skill_answer`].
pub fn stored_file(
    at: SessionStateAtNode,
    bundle: Option<&PluginBundle>,
    arguments: &str,
) -> Option<FileRef> {
    let (_, skill, file) = requested(at, bundle, arguments).ok()?;
    match skill.files.get(&file?)? {
        SkillFile::Stored(stored) => Some(stored.clone()),
        SkillFile::Text(_) => None,
    }
}

/// A skill's instructions, or one of its files. A script reads the files as
/// files, `<skill>/<path>`, so `scripted` names them that way. `stored` is the
/// text of the [`stored_file`] the call asks for.
pub fn skill_answer(
    at: SessionStateAtNode,
    bundle: Option<&PluginBundle>,
    arguments: &str,
    scripted: bool,
    stored: Option<&str>,
) -> StoredResult {
    let (named, skill, file) = match requested(at, bundle, arguments) {
        Ok(found) => found,
        Err(refused) => return refused,
    };
    match file {
        Some(path) if scripted => StoredResult::error(
            crate::prompts::codemode::javascript::read_skill_file(&format!("{named}/{path}")),
        ),
        Some(path) => file_answer(skill, &named, &path, stored),
        None if scripted => {
            StoredResult::text(crate::prompts::codemode::skill_loaded(&named, &skill.body))
        }
        None => StoredResult::text(answers::skill(
            &named,
            &skill.description,
            &skill.body,
            &skill_files(skill),
        )),
    }
}

/// Lists what the model could have meant: a plugin's skills when it named
/// the plugin, otherwise every skill unless they are searched.
fn no_such_skill(config: &AgentConfig, named: &str) -> String {
    let (plugin_id, skill_name) = split_skill(named);
    let listing = |skills: Vec<crate::agent_config::NamedSkill>| {
        skills
            .iter()
            .map(|s| answers::skill_listed(&s.name, s.description))
            .collect::<Vec<_>>()
            .join("\n")
    };
    let skills = config.named_skills();
    if !plugin_id.is_empty() {
        let Some(plugin) = config.plugins.iter().find(|p| p.id == plugin_id) else {
            let plugins: Vec<&str> = config.plugins.iter().map(|p| p.id.as_str()).collect();
            return errors::no_such_plugin(plugin_id, &crate::prompts::declared(plugins));
        };
        let in_plugin = skills
            .into_iter()
            .filter(|s| s.plugin == Some(plugin.id.as_str()))
            .collect();
        return errors::no_such_plugin_skill(skill_name, plugin_id, &listing(in_plugin));
    }
    if config.searches_skills() {
        return errors::no_such_searched_skill(named);
    }
    errors::no_such_skill(named, &listing(skills))
}

fn file_answer(skill: &Skill, named: &str, path: &str, stored: Option<&str>) -> StoredResult {
    match (skill.files.get(path), stored) {
        (Some(SkillFile::Text(text)), _) => return StoredResult::text(text.clone()),
        (Some(SkillFile::Stored(_)), Some(text)) => return StoredResult::text(text),
        (Some(SkillFile::Stored(_)), None) => {
            return StoredResult::error(errors::skill_file_unread(path, named))
        }
        (None, _) => {}
    }
    match skill.binaries.get(path) {
        Some(binary) => StoredResult {
            content: vec![StoredContent::Blob {
                uri: binary.uri.clone(),
            }],
            ..Default::default()
        },
        None => StoredResult::error(errors::no_such_skill_file(path, named, &skill_files(skill))),
    }
}

fn skill_files(skill: &Skill) -> String {
    let text = skill.files.keys().map(|path| answers::skill_file(path));
    let binaries = skill
        .binaries
        .iter()
        .map(|(path, b)| answers::skill_binary(path, &b.mime, b.size));
    text.chain(binaries).collect::<Vec<_>>().join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::{AgentConfigUpdated, EventPayload};
    use crate::plugins::{PluginSet, Skill};
    use crate::protocol::{AgentConfig, AgentPlugin, SkillMeta};
    use crate::state::{ApplyContext, SessionState};

    fn state_with_plugin() -> SessionState {
        let mut s = SessionState::new("sess-1".to_string());
        s.apply(
            &EventPayload::AgentConfigUpdated(AgentConfigUpdated {
                config: AgentConfig {
                    model: "m1".to_string(),
                    plugins: vec![AgentPlugin {
                        id: "pdf".to_string(),
                        description: "PDF work.".to_string(),
                        skills: vec![SkillMeta {
                            name: "form-filling".to_string(),
                            description: "Fill out PDF forms.".to_string(),
                            tools: vec![],
                        }],
                        servers: vec![],
                        tools: None,
                        auth_failure: Default::default(),
                        tool_sync_failure: Default::default(),
                        approve: Default::default(),
                        source: Default::default(),
                    }],
                    ..Default::default()
                },
                anchor: None,
            }),
            &ApplyContext {
                occurred_at: chrono::Utc::now(),
                sequence: 1,
            },
        );
        s
    }

    fn bundles() -> PluginSet {
        [(
            "pdf".to_string(),
            PluginBundle {
                name: "pdf-tools".to_string(),
                skills: vec![Skill {
                    name: "form-filling".to_string(),
                    description: "Fill out PDF forms.".to_string(),
                    body: "Read references/FORMS.md first.".to_string(),
                    tools: vec![],
                    files: [(
                        "references/FORMS.md".to_string(),
                        crate::plugins::SkillFile::Text("field rules".to_string()),
                    )]
                    .into(),
                    binaries: [(
                        "references/sample.pdf".to_string(),
                        crate::plugins::FileRef {
                            uri: "blob://t1/1?mime=application/pdf&size=3".to_string(),
                            mime: "application/pdf".to_string(),
                            size: 3,
                        },
                    )]
                    .into(),
                }],
                ..Default::default()
            },
        )]
        .into()
    }

    #[test]
    fn a_script_reads_a_skills_files_as_files() {
        let answer = skill_answer(
            state_with_plugin().at(None),
            bundles().get("pdf"),
            r#"{"name":"pdf:form-filling","file":"references/FORMS.md"}"#,
            true,
            None,
        );
        assert!(answer.is_error);
        assert!(
            answer
                .rendered()
                .contains(r#"files.text("pdf:form-filling/references/FORMS.md")"#),
            "{answer:?}"
        );
        let loaded = skill_answer(
            state_with_plugin().at(None),
            bundles().get("pdf"),
            r#"{"name":"pdf:form-filling"}"#,
            true,
            None,
        );
        assert!(
            loaded
                .rendered()
                .starts_with("Base directory for this skill: pdf:form-filling/\n\n"),
            "{loaded:?}"
        );
    }

    #[test]
    fn a_skill_answers_with_its_body_and_file_listing() {
        let answer = skill_answer(
            state_with_plugin().at(None),
            bundles().get("pdf"),
            r#"{"name":"pdf:form-filling"}"#,
            false,
            None,
        );
        assert!(!answer.is_error, "expected a result; got {answer:?}");
        let text = answer.as_text();
        assert!(text.contains("Read references/FORMS.md first."), "{text}");
        assert!(text.contains("- references/FORMS.md"), "{text}");
        assert!(
            text.contains("- references/sample.pdf (application/pdf, 3 bytes)"),
            "a binary is listed with what it is: {text}"
        );
    }

    #[test]
    fn a_file_read_answers_with_the_content() {
        let answer = skill_answer(
            state_with_plugin().at(None),
            bundles().get("pdf"),
            r#"{"name":"pdf:form-filling","file":"references/FORMS.md"}"#,
            false,
            None,
        );
        assert_eq!(answer, StoredResult::text("field rules"));
    }

    #[test]
    fn a_stored_file_answers_with_the_text_the_host_read() {
        let mut set = bundles();
        let stored = crate::plugins::FileRef {
            uri: "blob://t1/2?mime=text/markdown&size=11".to_string(),
            mime: "text/markdown".to_string(),
            size: 11,
        };
        set.get_mut("pdf").unwrap().skills[0].files.insert(
            "references/FORMS.md".to_string(),
            crate::plugins::SkillFile::Stored(stored.clone()),
        );
        let state = state_with_plugin();
        let arguments = r#"{"name":"pdf:form-filling","file":"references/FORMS.md"}"#;
        assert_eq!(
            stored_file(state.at(None), set.get("pdf"), arguments),
            Some(stored),
            "the host is told which blob to read"
        );
        let answer = skill_answer(
            state.at(None),
            set.get("pdf"),
            arguments,
            false,
            Some("field rules"),
        );
        assert_eq!(answer, StoredResult::text("field rules"));
        let unread = skill_answer(state.at(None), set.get("pdf"), arguments, false, None);
        assert!(unread.is_error, "{unread:?}");
    }

    #[test]
    fn a_binary_answers_with_its_blob() {
        let answer = skill_answer(
            state_with_plugin().at(None),
            bundles().get("pdf"),
            r#"{"name":"pdf:form-filling","file":"references/sample.pdf"}"#,
            false,
            None,
        );
        assert_eq!(
            answer.content,
            vec![StoredContent::Blob {
                uri: "blob://t1/1?mime=application/pdf&size=3".to_string()
            }],
            "the bytes reach the model the way a connection's do"
        );
        assert!(!answer.is_error);
    }

    #[test]
    fn a_wrong_name_answers_with_the_directory() {
        let cases = [
            (
                r#"{"name":"form-filling"}"#,
                "no skill `form-filling`. This agent's skills:",
            ),
            (r#"{"name":"typo:form-filling"}"#, "no plugin `typo`"),
            (
                r#"{"name":"pdf:typo"}"#,
                "pdf:form-filling — Fill out PDF forms.",
            ),
            (
                r#"{"name":"pdf:form-filling","file":"typo.md"}"#,
                "- references/FORMS.md",
            ),
        ];
        for (arguments, expected) in cases {
            let answer = skill_answer(
                state_with_plugin().at(None),
                bundles().get("pdf"),
                arguments,
                false,
                None,
            );
            assert!(
                answer.is_error,
                "{arguments}: expected a fault; got {answer:?}"
            );
            let text = answer.as_text();
            assert!(text.contains(expected), "{arguments}: {text}");
        }
    }
}

#[cfg(test)]
mod split_tests {
    use super::split_skill;

    #[test]
    fn one_split_rule_for_routing_and_answering() {
        assert_eq!(split_skill("pdf:form-filling"), ("pdf", "form-filling"));
        assert_eq!(split_skill("form-filling"), ("", "form-filling"));
        assert_eq!(
            split_skill("a:b:c"),
            ("a", "b:c"),
            "only the first colon splits"
        );
        assert_eq!(split_skill(""), ("", ""));
    }
}
