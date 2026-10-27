use std::collections::BTreeMap;
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::protocol::{Asset, Question, QuestionKind};
use serde_json::Value;
use substructure_session::system1::System1Request;

#[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum System1Kind {
    Typesafe,
    WorkersAi,
    Http,
}

impl System1Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Typesafe => "typesafe",
            Self::WorkersAi => "workers-ai",
            Self::Http => "http",
        }
    }

    pub fn default_api_key_env(self) -> Option<&'static str> {
        match self {
            Self::Typesafe => Some("TYPESAFE_API_KEY"),
            Self::WorkersAi => Some("CLOUDFLARE_API_TOKEN"),
            Self::Http => None,
        }
    }

    pub fn console_url(self) -> Option<&'static str> {
        match self {
            Self::Typesafe => Some("https://typesafe.ai"),
            Self::WorkersAi => Some("https://dash.cloudflare.com/profile/api-tokens"),
            Self::Http => None,
        }
    }
}

const ALL: &[QuestionKind] = &[
    QuestionKind::Noul,
    QuestionKind::Choice,
    QuestionKind::Score,
    QuestionKind::Box,
];
const DECISIONS: &[QuestionKind] = &[
    QuestionKind::Noul,
    QuestionKind::Choice,
    QuestionKind::Score,
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Capability {
    pub questions: Vec<QuestionKind>,
    pub max_questions: Option<usize>,
    pub max_assets: Option<usize>,
    pub strict: bool,
}

impl Capability {
    pub fn known(kind: System1Kind, model: &str) -> Option<Self> {
        match kind {
            System1Kind::Typesafe if model.starts_with("jev") => Some(Self {
                questions: ALL.to_vec(),
                max_questions: None,
                max_assets: None,
                strict: false,
            }),
            System1Kind::WorkersAi if matches!(clef_model(model), "clef" | "clef-flash") => {
                Some(Self {
                    questions: DECISIONS.to_vec(),
                    max_questions: Some(64),
                    max_assets: Some(4),
                    strict: true,
                })
            }
            _ => None,
        }
    }
}

pub fn clef_model(model: &str) -> &str {
    model.strip_prefix("@cf/cloudflare/").unwrap_or(model)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct System1Block {
    pub kind: System1Kind,
    pub model: Option<String>,
    pub questions: Option<Vec<QuestionKind>>,
}

impl System1Block {
    pub fn capability(&self, model: &str) -> Option<Capability> {
        let known = Capability::known(self.kind, model);
        match &self.questions {
            Some(declared) => Some(Capability {
                questions: declared.clone(),
                ..known.unwrap_or(Capability {
                    questions: Vec::new(),
                    max_questions: None,
                    max_assets: None,
                    strict: false,
                })
            }),
            None => known,
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct System1Blocks(Arc<BTreeMap<String, System1Block>>);

impl System1Blocks {
    pub fn new(blocks: BTreeMap<String, System1Block>) -> Self {
        Self(Arc::new(blocks))
    }

    pub fn get(&self, name: &str) -> Option<&System1Block> {
        self.0.get(name)
    }

    pub fn only(&self) -> Option<&str> {
        match self.0.len() {
            1 => self.0.keys().next().map(String::as_str),
            _ => None,
        }
    }

    pub fn declared(&self) -> String {
        crate::copy::declared(self.0.keys())
    }
}

impl FromIterator<(String, System1Block)> for System1Blocks {
    fn from_iter<I: IntoIterator<Item = (String, System1Block)>>(iter: I) -> Self {
        Self::new(iter.into_iter().collect())
    }
}

fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 100
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-'))
}

fn strict(name: &str, question: &Question, model: &str) -> Result<(), String> {
    if !valid_name(name) {
        return Err(format!(
            "question name `{name}` must be 1 to 100 letters, digits, `_`, `.` or `-` for model `{model}`"
        ));
    }
    let (instructions, options) = match question {
        Question::Noul { instructions, .. } | Question::Locate { instructions } => {
            (instructions, None)
        }
        Question::Choice {
            instructions,
            criteria,
        } => (instructions, Some(criteria.len())),
        Question::Score {
            instructions,
            criteria,
        } => (instructions, Some(criteria.len())),
    };
    if instructions.as_ref().is_none_or(Value::is_null) {
        return Err(format!(
            "question `{name}` needs `instructions` for model `{model}`"
        ));
    }
    match options {
        Some(n) if n < 2 => Err(format!(
            "question `{name}` needs at least 2 options or levels for model `{model}`"
        )),
        _ => Ok(()),
    }
}

pub fn check(
    block: &str,
    request: &System1Request,
    capability: Option<&Capability>,
) -> Result<(), String> {
    if request.questions.is_empty() {
        return Err("a system1.call needs at least one question".to_string());
    }
    for (name, question) in &request.questions {
        match question {
            Question::Choice { criteria, .. } if criteria.is_empty() || criteria.len() > 255 => {
                return Err(format!("choice question `{name}` needs 1 to 255 options"))
            }
            Question::Score { criteria, .. } if criteria.is_empty() || criteria.len() > 10 => {
                return Err(format!("score question `{name}` needs 1 to 10 levels"))
            }
            Question::Locate { .. } if request.assets.is_empty() => {
                return Err(format!("box question `{name}` needs an image in `assets`"))
            }
            _ => {}
        }
    }
    for (key, asset) in &request.assets {
        let Asset::Image { uri } = asset;
        if !uri.starts_with(substructure_session::blob::BLOB_SCHEME) && !uri.starts_with("data:") {
            return Err(format!(
                "asset `{key}` must be a blob URI from a message or a data URI"
            ));
        }
    }
    let Some(capability) = capability else {
        return Ok(());
    };
    let model = &request.model;
    for (name, question) in &request.questions {
        let kind = question.kind();
        if !capability.questions.contains(&kind) {
            return Err(format!(
                "block `{block}` with model `{model}` does not answer `{}` questions (question `{name}`)",
                kind.as_str()
            ));
        }
        if capability.strict {
            strict(name, question, model)?;
        }
    }
    match capability.max_assets {
        Some(0) if !request.assets.is_empty() => Err(format!(
            "block `{block}` with model `{model}` does not take assets"
        )),
        Some(most) if request.assets.len() > most => Err(format!(
            "block `{block}` with model `{model}` takes at most {most} assets"
        )),
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::Question;

    fn request(model: &str, questions: &[(&str, Question)]) -> System1Request {
        System1Request {
            model: model.to_string(),
            state: None,
            questions: questions
                .iter()
                .map(|(n, q)| (n.to_string(), q.clone()))
                .collect(),
            assets: Default::default(),
        }
    }

    fn noul() -> Question {
        Question::Noul {
            instructions: None,
            criteria: None,
        }
    }

    fn locate() -> Question {
        Question::Locate { instructions: None }
    }

    #[test]
    fn a_known_model_refuses_a_question_it_does_not_answer() {
        let block = System1Block {
            kind: System1Kind::WorkersAi,
            model: None,
            questions: None,
        };
        let cap = block.capability("clef");
        let err = check("cf", &request("clef", &[("where", locate())]), cap.as_ref()).unwrap_err();
        assert!(err.contains("needs an image"), "{err}");
        let mut with_image = request("clef", &[("where", locate())]);
        with_image.assets.insert(
            "img".to_string(),
            crate::protocol::Asset::Image {
                uri: "data:image/png;base64,AA==".to_string(),
            },
        );
        let err = check("cf", &with_image, cap.as_ref()).unwrap_err();
        assert!(err.contains("does not answer `box`"), "{err}");
    }

    #[test]
    fn clef_needs_instructions_and_two_options() {
        let cap = Capability::known(System1Kind::WorkersAi, "clef");
        let err = check("cf", &request("clef", &[("q", noul())]), cap.as_ref()).unwrap_err();
        assert!(err.contains("needs `instructions`"), "{err}");
        let one = Question::Score {
            instructions: Some(serde_json::json!("How bad?")),
            criteria: vec![serde_json::json!("fine")],
        };
        let err = check("cf", &request("clef", &[("q", one.clone())]), cap.as_ref()).unwrap_err();
        assert!(err.contains("at least 2"), "{err}");
        assert!(check("ts", &request("jev-latest", &[("q", one)]), None).is_ok());
    }

    #[test]
    fn an_unknown_model_is_left_to_the_provider() {
        let block = System1Block {
            kind: System1Kind::WorkersAi,
            model: None,
            questions: None,
        };
        assert_eq!(block.capability("clef-2"), None);
        assert!(check("cf", &request("clef-2", &[("a b", noul())]), None).is_ok());
    }

    #[test]
    fn a_block_can_declare_what_a_model_answers() {
        let block = System1Block {
            kind: System1Kind::WorkersAi,
            model: None,
            questions: Some(vec![QuestionKind::Noul]),
        };
        let cap = block.capability("clef-2").expect("declared");
        assert_eq!(cap.questions, vec![QuestionKind::Noul]);
        let known = block.capability("clef").expect("known");
        assert_eq!(known.max_questions, Some(64));
    }

    #[test]
    fn clef_names_follow_its_rule() {
        let cap = Capability::known(System1Kind::WorkersAi, "@cf/cloudflare/clef");
        let err = check("cf", &request("clef", &[("a b", noul())]), cap.as_ref()).unwrap_err();
        assert!(err.contains("question name"), "{err}");
    }
}
