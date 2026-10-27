use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::protocol::SkillMeta;

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct PluginBundle {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    #[serde(default)]
    pub description: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub skills: Vec<Skill>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub servers: BTreeMap<String, BundleServer>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct BundleServer {
    pub url: String,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Skill {
    pub name: String,
    pub description: String,
    pub body: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub files: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub binaries: BTreeMap<String, Binary>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Binary {
    pub uri: String,
    pub mime: String,
    pub size: u64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Pending {
    pub skill: String,
    pub path: String,
    pub mime: String,
    pub bytes: Vec<u8>,
}

impl PluginBundle {
    pub fn skill(&self, name: &str) -> Option<&Skill> {
        self.skills.iter().find(|s| s.name == name)
    }

    pub fn skill_metas(&self) -> Vec<SkillMeta> {
        self.skills
            .iter()
            .map(|s| SkillMeta {
                name: s.name.clone(),
                description: s.description.clone(),
                tools: s.tools.clone(),
            })
            .collect()
    }

    pub fn hash(&self) -> String {
        use sha2::{Digest, Sha256};
        let bytes = serde_json::to_vec(self).unwrap_or_default();
        format!("{:x}", Sha256::digest(bytes))
    }
}

pub type PluginSet = BTreeMap<String, PluginBundle>;

/// The bundle that holds a project's own skills, the `[skill.<id>]` sections
/// with a `path`. It travels as a plugin does; no plugin id starts with `_`.
pub const SKILLS_BUNDLE: &str = "_skills";
