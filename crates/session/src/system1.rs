use std::collections::BTreeMap;

use rust_decimal::Decimal;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::protocol::{Content, Message, Usage};

/// A named question about a `system1.call`'s state. Its type sets the type
/// of its answer.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
#[schemars(title = "Question")]
pub enum Question {
    /// Is it true? The answer is the probability of yes.
    Noul {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        instructions: Option<Value>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        criteria: Option<NoulCriteria>,
    },
    /// Which option? `criteria` names each option and says when it applies.
    Choice {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        instructions: Option<Value>,
        criteria: BTreeMap<String, Value>,
    },
    /// Where on a scale? `criteria` lists the levels in order. The first is 0.
    Score {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        instructions: Option<Value>,
        criteria: Vec<Value>,
    },
    /// Where in the image?
    #[serde(rename = "box")]
    Locate {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        instructions: Option<Value>,
    },
}

impl Question {
    pub fn kind(&self) -> QuestionKind {
        match self {
            Question::Noul { .. } => QuestionKind::Noul,
            Question::Choice { .. } => QuestionKind::Choice,
            Question::Score { .. } => QuestionKind::Score,
            Question::Locate { .. } => QuestionKind::Box,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
#[schemars(title = "QuestionKind")]
pub enum QuestionKind {
    Noul,
    Choice,
    Score,
    Box,
}

impl QuestionKind {
    pub fn as_str(self) -> &'static str {
        match self {
            QuestionKind::Noul => "noul",
            QuestionKind::Choice => "choice",
            QuestionKind::Score => "score",
            QuestionKind::Box => "box",
        }
    }
}

/// What counts as yes and what counts as no.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[schemars(title = "NoulCriteria")]
pub struct NoulCriteria {
    #[serde(rename = "true", default, skip_serializing_if = "Option::is_none")]
    pub yes: Option<Value>,
    #[serde(rename = "false", default, skip_serializing_if = "Option::is_none")]
    pub no: Option<Value>,
}

/// The answer to one question. A field the engine does not name stays on
/// the answer as the provider sent it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
#[schemars(title = "Answer")]
pub enum Answer {
    /// `noul` is the probability of yes, from 0 to 1.
    Noul {
        #[serde(deserialize_with = "crate::json::f64")]
        noul: f64,
        #[serde(flatten)]
        extra: Map<String, Value>,
    },
    Choice {
        choice: String,
        #[serde(deserialize_with = "crate::json::f64")]
        confidence: f64,
        #[serde(deserialize_with = "crate::json::f64_map")]
        probabilities: BTreeMap<String, f64>,
        #[serde(flatten)]
        extra: Map<String, Value>,
    },
    /// `score` is the expected level. It can fall between two levels.
    Score {
        #[serde(deserialize_with = "crate::json::f64")]
        score: f64,
        #[serde(deserialize_with = "crate::json::f64")]
        confidence: f64,
        #[serde(default)]
        legend: BTreeMap<String, Value>,
        #[serde(deserialize_with = "crate::json::f64_map")]
        probabilities: BTreeMap<String, f64>,
        #[serde(flatten)]
        extra: Map<String, Value>,
    },
    /// The edges are fractions of the image's height and width.
    #[serde(rename = "box")]
    Locate {
        #[serde(rename = "box")]
        edges: Edges,
        #[serde(default, deserialize_with = "crate::json::option_f64")]
        confidence: Option<f64>,
        #[serde(flatten)]
        extra: Map<String, Value>,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[schemars(title = "Edges")]
pub struct Edges {
    #[serde(deserialize_with = "crate::json::f64")]
    pub top: f64,
    #[serde(deserialize_with = "crate::json::f64")]
    pub left: f64,
    #[serde(deserialize_with = "crate::json::f64")]
    pub bottom: f64,
    #[serde(deserialize_with = "crate::json::f64")]
    pub right: f64,
}

/// Media a question can refer to. Its key is a placeholder that stands for
/// it in the state, the instructions or the criteria.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
#[schemars(title = "Asset")]
pub enum Asset {
    /// `uri` is a `blob://` URI from a message, or a `data:` URI.
    Image { uri: String },
}

impl Asset {
    pub fn uri(&self) -> &str {
        match self {
            Asset::Image { uri } => uri,
        }
    }

    pub fn with_uri(&self, uri: String) -> Self {
        match self {
            Asset::Image { .. } => Asset::Image { uri },
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct System1Request {
    pub model: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state: Option<Value>,
    pub questions: BTreeMap<String, Question>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub assets: BTreeMap<String, Asset>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct System1Response {
    pub model: String,
    pub answers: BTreeMap<String, Value>,
    #[serde(default)]
    pub usage: Usage,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost: Option<Decimal>,
}

/// The state a call that names none reads: the conversation as role and text.
pub fn transcript_state(messages: &[Message]) -> Value {
    let lines: Vec<Value> = messages
        .iter()
        .filter_map(|m| {
            let mut line = Map::new();
            line.insert("role".into(), serde_json::to_value(&m.role).ok()?);
            if let Some(name) = &m.name {
                line.insert("name".into(), Value::String(name.clone()));
            }
            let text = m.content.as_ref().map(Content::text_owned);
            if let Some(text) = text.filter(|t| !t.is_empty()) {
                line.insert("content".into(), Value::String(text));
            }
            if !m.tool_calls.is_empty() {
                let calls = m
                    .tool_calls
                    .iter()
                    .map(|c| {
                        serde_json::json!({
                            "name": c.function.name,
                            "arguments": c.function.arguments,
                        })
                    })
                    .collect();
                line.insert("tool_calls".into(), Value::Array(calls));
            }
            (line.len() > 1).then_some(Value::Object(line))
        })
        .collect();
    Value::Array(lines)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_answer_keeps_a_field_it_does_not_name() {
        let wire = serde_json::json!({ "type": "noul", "noul": 0.9, "rationale_id": "r7" });
        let answer: Answer = serde_json::from_value(wire.clone()).unwrap();
        match &answer {
            Answer::Noul { noul, extra } => {
                assert_eq!(*noul, 0.9);
                assert_eq!(extra["rationale_id"], "r7");
            }
            other => panic!("expected noul, got {other:?}"),
        }
        assert_eq!(serde_json::to_value(&answer).unwrap(), wire);
    }

    #[test]
    fn every_answer_type_reads_from_a_body() {
        let body = r#"{
            "c": {"type":"choice","choice":"angry","confidence":0.9,"probabilities":{"angry":0.8,"calm":0.2}},
            "s": {"type":"score","score":1.7,"confidence":0.8,"legend":{"0":"low","1":"mid","2":"high"},"probabilities":{"0":0.1,"1":0.1,"2":0.8}},
            "b": {"type":"box","box":{"top":0.2,"left":0.19,"bottom":0.66,"right":0.68},"confidence":null}
        }"#;
        let answers: BTreeMap<String, Answer> = serde_json::from_str(body).unwrap();
        assert!(matches!(answers["c"], Answer::Choice { ref choice, .. } if choice == "angry"));
        assert!(matches!(answers["s"], Answer::Score { score, .. } if score == 1.7));
        assert!(
            matches!(answers["b"], Answer::Locate { ref edges, confidence: None, .. } if edges.top == 0.2)
        );
    }

    #[test]
    fn a_question_writes_the_providers_shape() {
        let q: Question = serde_json::from_value(serde_json::json!({
            "type": "noul",
            "instructions": "Is this spam?",
            "criteria": { "true": "advertising", "false": "a conversation" }
        }))
        .unwrap();
        assert_eq!(q.kind(), QuestionKind::Noul);
        assert_eq!(
            serde_json::to_value(&q).unwrap()["criteria"]["true"],
            "advertising"
        );
        let located: Question =
            serde_json::from_value(serde_json::json!({ "type": "box" })).unwrap();
        assert_eq!(located.kind(), QuestionKind::Box);
    }
}
