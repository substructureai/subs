use std::collections::BTreeMap;

use async_trait::async_trait;
use base64::Engine as _;
use rust_decimal::Decimal;
use serde_json::{json, Map, Value};

use crate::llm::LlmCallError;
use crate::protocol::{ErrorCode, Usage};
use crate::runtime::system1::{Capability, Media, System1Callable, System1Kind};
use substructure_session::system1::{System1Request, System1Response};

const TYPESAFE_URL: &str = "https://api.typesafe.ai";
const CLOUDFLARE_URL: &str = "https://api.cloudflare.com/client/v4";

#[derive(Debug, Clone)]
pub struct System1Config {
    pub kind: System1Kind,
    pub api_key: Option<String>,
    pub base_url: Option<String>,
    pub url: Option<String>,
    pub account_id: Option<String>,
    pub price_input: Option<Decimal>,
}

pub struct System1Client {
    config: System1Config,
    http: reqwest::Client,
}

impl System1Client {
    pub fn new(config: System1Config) -> Self {
        Self {
            config,
            http: super::http_client(),
        }
    }

    fn endpoint(&self, model: &str) -> Result<String, LlmCallError> {
        let base = |default: &str| {
            self.config
                .base_url
                .clone()
                .unwrap_or_else(|| default.to_string())
                .trim_end_matches('/')
                .to_string()
        };
        match self.config.kind {
            System1Kind::Typesafe => Ok(format!("{}/v1/systemone", base(TYPESAFE_URL))),
            System1Kind::WorkersAi => {
                let account = self.config.account_id.as_deref().ok_or_else(|| {
                    LlmCallError::new(
                        ErrorCode::ProviderError,
                        "a workers-ai block needs an account id",
                        false,
                    )
                })?;
                let model = match model.starts_with("@cf/") {
                    true => model.to_string(),
                    false => format!("@cf/cloudflare/{model}"),
                };
                Ok(format!(
                    "{}/accounts/{account}/ai/run/{model}",
                    base(CLOUDFLARE_URL)
                ))
            }
            System1Kind::Http => self.config.url.clone().ok_or_else(|| {
                LlmCallError::new(ErrorCode::ProviderError, "an http block needs a url", false)
            }),
        }
    }

    fn body(&self, request: &System1Request, assets: &BTreeMap<String, Media>) -> Value {
        let model = match self.config.kind {
            System1Kind::WorkersAi => crate::runtime::system1::clef_model(&request.model),
            _ => &request.model,
        };
        let mut body = json!({
            "model": model,
            "state": request.state.clone().unwrap_or(Value::String(String::new())),
            "questions": request.questions,
        });
        if assets.is_empty() {
            return body;
        }
        let encode = |m: &Media| base64::engine::general_purpose::STANDARD.encode(&m.bytes);
        match self.config.kind {
            System1Kind::WorkersAi => {
                let images = assets
                    .values()
                    .map(|m| json!({ "content_type": m.mime, "base64": encode(m) }))
                    .collect();
                body["images"] = Value::Array(images);
            }
            _ => {
                let assets: Map<String, Value> = assets
                    .iter()
                    .map(|(key, m)| {
                        let format = m.mime.strip_prefix("image/").unwrap_or("png");
                        let asset =
                            json!({ "type": "image", "format": format, "data_base64": encode(m) });
                        (key.clone(), asset)
                    })
                    .collect();
                body["assets"] = Value::Object(assets);
            }
        }
        body
    }

    async fn send(
        &self,
        request: &System1Request,
        assets: &BTreeMap<String, Media>,
    ) -> Result<System1Response, LlmCallError> {
        let provider = self.config.kind.as_str();
        let mut call = self
            .http
            .post(self.endpoint(&request.model)?)
            .json(&self.body(request, assets));
        if let Some(key) = &self.config.api_key {
            call = call.bearer_auth(key);
        }
        if !assets.is_empty() && self.config.kind == System1Kind::Typesafe {
            call = call.header("TypeSafe-Danger-Zone-Images", "true");
        }
        let response = call.send().await.map_err(|e| {
            LlmCallError::new(ErrorCode::ProviderError, format!("{provider}: {e}"), true)
        })?;
        let status = response.status();
        let text = response.text().await.map_err(|e| {
            LlmCallError::new(ErrorCode::ProviderError, format!("{provider}: {e}"), true)
        })?;
        if !status.is_success() {
            return Err(super::classify_error(provider, status, &text));
        }
        let body: Value = crate::json::from_str("a system1 response", &text)?;
        parse(provider, body, &request.model)
    }

    fn cost(&self, usage: &Usage) -> Option<Decimal> {
        let price = self.config.price_input?;
        Some(price * Decimal::from(usage.input) / Decimal::from(1_000_000u32))
    }
}

fn invalid(provider: &str, message: &str) -> LlmCallError {
    LlmCallError::new(
        ErrorCode::InvalidResponse,
        format!("{provider} returned {message}"),
        false,
    )
}

fn parse(provider: &str, body: Value, asked: &str) -> Result<System1Response, LlmCallError> {
    let body = match body {
        Value::Object(mut wrapped) if !wrapped.contains_key("answers") => {
            wrapped.remove("result").unwrap_or(Value::Object(wrapped))
        }
        body => body,
    };
    let Value::Object(mut body) = body else {
        return Err(invalid(provider, "a body that is not an object"));
    };
    let Some(Value::Object(answers)) = body.remove("answers") else {
        return Err(invalid(provider, "no answers"));
    };
    let count = |names: &[&str], raw: &Value| {
        names
            .iter()
            .find_map(|n| raw.get(*n).and_then(Value::as_u64))
            .unwrap_or_default()
    };
    let usage = match body.remove("usage") {
        Some(raw) => Usage {
            provider: Some(raw.clone()),
            ..Usage::new(
                count(&["input_tokens", "prompt_tokens"], &raw),
                0,
                0,
                count(&["output_tokens", "completion_tokens"], &raw),
            )
        },
        None => Usage::default(),
    };
    let model = body
        .remove("model")
        .and_then(|m| m.as_str().map(str::to_string))
        .unwrap_or_else(|| asked.to_string());
    Ok(System1Response {
        model,
        answers: answers.into_iter().collect(),
        usage,
        cost: None,
    })
}

fn chunks(request: &System1Request, most: Option<usize>) -> Vec<System1Request> {
    let Some(most) = most.filter(|m| *m > 0 && request.questions.len() > *m) else {
        return vec![request.clone()];
    };
    let names: Vec<&String> = request.questions.keys().collect();
    names
        .chunks(most)
        .map(|part| System1Request {
            questions: part
                .iter()
                .map(|n| ((*n).clone(), request.questions[*n].clone()))
                .collect(),
            ..request.clone()
        })
        .collect()
}

fn merge(parts: Vec<System1Response>) -> System1Response {
    let mut parts = parts.into_iter();
    let mut merged = parts.next().unwrap_or(System1Response {
        model: String::new(),
        answers: BTreeMap::new(),
        usage: Usage::default(),
        cost: None,
    });
    for part in parts {
        merged.answers.extend(part.answers);
        merged.usage = Usage::new(
            merged.usage.uncached_input + part.usage.uncached_input,
            0,
            0,
            merged.usage.output + part.usage.output,
        );
    }
    merged
}

#[async_trait]
impl System1Callable for System1Client {
    async fn call(
        &self,
        request: &System1Request,
        assets: &BTreeMap<String, Media>,
    ) -> Result<System1Response, LlmCallError> {
        let most =
            Capability::known(self.config.kind, &request.model).and_then(|c| c.max_questions);
        let parts = chunks(request, most);
        let calls = parts.iter().map(|part| self.send(part, assets));
        let mut response = merge(futures_util::future::try_join_all(calls).await?);
        response.cost = self.cost(&response.usage);
        Ok(response)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::Question;

    fn noul() -> Question {
        Question::Noul {
            instructions: None,
            criteria: None,
        }
    }

    #[test]
    fn a_typesafe_body_reads_its_answers_and_usage() {
        let body = json!({
            "model": "jev-1.13",
            "answers": { "billing": { "type": "noul", "noul": 0.98 } },
            "usage": { "input_tokens": 120, "output_tokens": 12 }
        });
        let r = parse("typesafe", body, "jev-latest").unwrap();
        assert_eq!(r.model, "jev-1.13");
        assert_eq!(r.answers["billing"]["noul"], json!(0.98));
        assert_eq!((r.usage.input, r.usage.output), (120, 12));
    }

    #[test]
    fn a_cloudflare_envelope_is_unwrapped() {
        let body = json!({
            "result": { "answers": { "urgent": { "type": "noul", "noul": 0.7 } } },
            "success": true,
            "errors": []
        });
        let r = parse("workers-ai", body, "clef").unwrap();
        assert_eq!(r.model, "clef");
        assert_eq!(r.answers["urgent"]["noul"], json!(0.7));
    }

    #[test]
    fn a_body_without_answers_is_invalid() {
        let err = parse("http", json!({ "model": "x" }), "x").unwrap_err();
        assert_eq!(err.error.code, ErrorCode::InvalidResponse);
    }

    #[test]
    fn too_many_questions_split_and_their_answers_merge() {
        let request = System1Request {
            model: "clef".to_string(),
            state: None,
            questions: (0..130).map(|i| (format!("q{i:03}"), noul())).collect(),
            assets: Default::default(),
        };
        let parts = chunks(&request, Some(64));
        assert_eq!(
            parts.iter().map(|p| p.questions.len()).collect::<Vec<_>>(),
            [64, 64, 2]
        );
        let answered = parts
            .iter()
            .map(|p| System1Response {
                model: "clef".to_string(),
                answers: p
                    .questions
                    .keys()
                    .map(|k| (k.clone(), json!({ "type": "noul", "noul": 0.5 })))
                    .collect(),
                usage: Usage::new(10, 0, 0, 1),
                cost: None,
            })
            .collect();
        let merged = merge(answered);
        assert_eq!(merged.answers.len(), 130);
        assert_eq!((merged.usage.input, merged.usage.output), (30, 3));
    }

    #[test]
    fn the_price_is_per_million_input_tokens() {
        let client = System1Client::new(System1Config {
            kind: System1Kind::WorkersAi,
            api_key: None,
            base_url: None,
            url: None,
            account_id: None,
            price_input: Some(Decimal::new(24, 2)),
        });
        let cost = client.cost(&Usage::new(1_000_000, 0, 0, 5)).unwrap();
        assert_eq!(cost, Decimal::new(24, 2));
    }

    #[test]
    fn a_workers_ai_call_goes_to_the_models_route() {
        let client = System1Client::new(System1Config {
            kind: System1Kind::WorkersAi,
            api_key: None,
            base_url: None,
            url: None,
            account_id: Some("acc".to_string()),
            price_input: None,
        });
        assert_eq!(
            client.endpoint("clef-flash").unwrap(),
            "https://api.cloudflare.com/client/v4/accounts/acc/ai/run/@cf/cloudflare/clef-flash"
        );
        let body = client.body(
            &System1Request {
                model: "@cf/cloudflare/clef".to_string(),
                state: Some(json!("hi")),
                questions: BTreeMap::from([("q".to_string(), noul())]),
                assets: Default::default(),
            },
            &BTreeMap::new(),
        );
        assert_eq!(body["model"], "clef");
    }

    #[test]
    fn workers_ai_sends_images_in_key_order_and_typesafe_keys_them() {
        let assets = BTreeMap::from([
            (
                "b".to_string(),
                Media {
                    mime: "image/jpeg".to_string(),
                    bytes: vec![2],
                },
            ),
            (
                "a".to_string(),
                Media {
                    mime: "image/png".to_string(),
                    bytes: vec![1],
                },
            ),
        ]);
        let request = System1Request {
            model: "clef".to_string(),
            state: Some(json!("hi")),
            questions: BTreeMap::from([("q".to_string(), noul())]),
            assets: Default::default(),
        };
        let config = |kind| System1Config {
            kind,
            api_key: None,
            base_url: None,
            url: None,
            account_id: None,
            price_input: None,
        };
        let body = System1Client::new(config(System1Kind::WorkersAi)).body(&request, &assets);
        assert_eq!(
            body["images"],
            json!([
                { "content_type": "image/png", "base64": "AQ==" },
                { "content_type": "image/jpeg", "base64": "Ag==" }
            ])
        );
        assert!(body.get("assets").is_none());
        let body = System1Client::new(config(System1Kind::Typesafe)).body(&request, &assets);
        assert_eq!(body["assets"]["b"]["format"], "jpeg");
        assert!(body.get("images").is_none());
    }
}
