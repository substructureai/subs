use std::io::IsTerminal;

use clap::ValueEnum;
use serde::{Deserialize, Serialize};

pub use substructure_host::manifest::ProviderKind;

/// How `subs run` renders a turn.
#[derive(Copy, Clone, Debug, PartialEq, Eq, ValueEnum, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum OutputFormat {
    /// Stream AG-UI protocol events, one JSON object per line.
    AgUi,
    /// Stream raw persisted engine events, one JSON object per line.
    Jsonl,
    /// Human-readable text: streamed replies, tool calls, and results.
    Pretty,
}

impl OutputFormat {
    pub fn for_stdout() -> Self {
        match std::io::stdout().is_terminal() {
            true => Self::Pretty,
            false => Self::AgUi,
        }
    }
}

pub struct ProviderEnv {
    pub name: String,
    pub kind: ProviderKind,
    pub api_key: String,
    pub base_url: Option<String>,
    pub cache_ttl: Option<String>,
}

pub struct ProviderBinding {
    pub name: String,
    pub kind: ProviderKind,
    pub api_key_env: String,
    pub base_url: Option<String>,
    pub cache_ttl: Option<String>,
}

pub struct AuthEnvVars {
    pub client_token_issuer: String,
    pub client_token_audience: String,
    pub client_token_hs256_secret: String,
    pub substructure_api_key: String,
}

pub struct EnvVars {
    pub providers: Vec<ProviderEnv>,
    pub auth: Option<AuthEnvVars>,
}

impl EnvVars {
    pub fn load(bindings: Vec<ProviderBinding>, authenticate: bool) -> Option<Self> {
        let provider_specs: Vec<(String, String)> = bindings
            .iter()
            .map(|b| {
                let console = b
                    .kind
                    .console_url()
                    .map(|u| format!(" ({u})"))
                    .unwrap_or_default();
                (
                    b.api_key_env.clone(),
                    format!(
                        "API key for [llm.{}], a {} block{console}",
                        b.name,
                        b.kind.as_str()
                    ),
                )
            })
            .collect();

        let auth_specs: &[(&str, &str)] = &[
            ("CLIENT_TOKEN_ISSUER", "JWT 'iss' claim for client tokens"),
            ("CLIENT_TOKEN_AUDIENCE", "JWT 'aud' claim for client tokens"),
            (
                "CLIENT_TOKEN_HS256_SECRET",
                "HS256 secret used to sign client tokens",
            ),
            (
                "SUBSTRUCTURE_API_KEY",
                "Bearer API key SDK clients present to reach worker and admin HTTP APIs",
            ),
        ];

        let mut missing: Vec<(String, String)> = Vec::new();
        let mut provider_values: Vec<String> = Vec::with_capacity(provider_specs.len());
        let mut auth_values: Vec<String> = Vec::with_capacity(auth_specs.len());

        for (name, desc) in &provider_specs {
            match std::env::var(name) {
                Ok(v) => provider_values.push(v),
                Err(_) => missing.push((name.clone(), desc.clone())),
            }
        }
        if authenticate {
            for (name, desc) in auth_specs {
                match std::env::var(name) {
                    Ok(v) => auth_values.push(v),
                    Err(_) => missing.push((name.to_string(), desc.to_string())),
                }
            }
        }

        if !missing.is_empty() {
            eprintln!("error: missing required environment variable(s):");
            for (name, desc) in &missing {
                eprintln!("  - {name}: {desc}");
            }
            eprintln!("\nSet them and try again, e.g.:");
            for (name, _) in &missing {
                eprintln!("  export {name}=...");
            }
            return None;
        }

        let providers = bindings
            .into_iter()
            .zip(provider_values)
            .map(|(b, api_key)| ProviderEnv {
                name: b.name,
                kind: b.kind,
                api_key,
                base_url: b.base_url,
                cache_ttl: b.cache_ttl,
            })
            .collect();

        let auth = if !authenticate {
            None
        } else {
            let [issuer, audience, secret, api_key]: [String; 4] = auth_values.try_into().ok()?;
            Some(AuthEnvVars {
                client_token_issuer: issuer,
                client_token_audience: audience,
                client_token_hs256_secret: secret,
                substructure_api_key: api_key,
            })
        };

        Some(Self { providers, auth })
    }
}
