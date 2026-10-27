use anyhow::{bail, Result};
use serde::Serialize;

use super::cloud::context::Context;
use super::cloud::project_config::ProjectConfig;
use super::cloud::{notices, pickers, print, ProjectScope};
use super::connections::Row;
use super::env_value;
use super::target::target;
use crate::api::v1::{LlmKeyRequest, System1BlockView};

#[derive(Debug, Serialize)]
struct KeyResult<'a> {
    block: &'a str,
    key_bound: bool,
}

pub(crate) async fn set_key_at(block: String, scope: ProjectScope) -> Result<()> {
    match target(&scope.globals)?.here() {
        Some(config) => key_is_a_variable(&block, &config, "set"),
        None => set_key(block, scope).await,
    }
}

pub(crate) async fn delete_key_at(block: String, scope: ProjectScope) -> Result<()> {
    match target(&scope.globals)?.here() {
        Some(config) => key_is_a_variable(&block, &config, "delete"),
        None => delete_key(block, scope).await,
    }
}

fn key_is_a_variable(block: &str, config: &ProjectConfig, verb: &str) -> Result<()> {
    let Some(spec) = config.system1.get(block) else {
        bail!(
            "no [system1.{block}] in subs.toml. Declared: {}",
            crate::copy::declared(config.system1.keys())
        );
    };
    let Some(var) = spec.api_key_env() else {
        bail!("[system1.{block}] does not use a key. Set `api_key_env` to send one.");
    };
    let how = match verb {
        "delete" => format!("Remove it there:\n\n  unset {var}"),
        _ => format!("Set it there:\n\n  export {var}=..."),
    };
    bail!("This project runs locally and reads the key from the environment. {how}")
}

async fn set_key(block: String, scope: ProjectScope) -> Result<()> {
    let key = pickers::read_secret(&scope.globals, "Paste the key")?;
    if key.is_empty() {
        bail!("no key given. Pipe it in.");
    }
    let (ctx, project) = Context::from_project(&scope).await?;
    ctx.client
        .put_discard(
            &format!("/api/v1/projects/{project}/system1/{block}/key"),
            &LlmKeyRequest { key },
        )
        .await?;
    if scope.globals.json {
        return print::json(&KeyResult {
            block: &block,
            key_bound: true,
        });
    }
    println!("Key set for [system1.{block}].");
    notices::remaining(&ctx, &project, &scope.globals).await;
    Ok(())
}

async fn delete_key(block: String, scope: ProjectScope) -> Result<()> {
    let (ctx, project) = Context::from_project(&scope).await?;
    ctx.client
        .delete_discard(&format!("/api/v1/projects/{project}/system1/{block}/key"))
        .await?;
    if scope.globals.json {
        return print::json(&KeyResult {
            block: &block,
            key_bound: false,
        });
    }
    println!("Key removed for [system1.{block}]. Calls on it will fail until another is set.");
    Ok(())
}

pub(crate) async fn rows(
    cfg: &ProjectConfig,
    scope: &ProjectScope,
    here: bool,
) -> Result<Vec<Row>> {
    if cfg.system1.is_empty() {
        return Ok(Vec::new());
    }
    if here {
        return Ok(cfg
            .system1
            .iter()
            .map(|(name, spec)| Row {
                path: format!("system1.{name}"),
                what: spec.kind.as_str().to_string(),
                credential: match spec.api_key_env() {
                    None => "none".to_string(),
                    Some(var) => match env_value(&var).is_some() {
                        true => format!("${var}"),
                        false => format!("${var} (not set)"),
                    },
                },
            })
            .collect());
    }
    let (ctx, project) = Context::from_project(scope).await?;
    let blocks: Vec<System1BlockView> = ctx
        .client
        .get(&format!("/api/v1/projects/{project}/system1"))
        .await?;
    Ok(blocks
        .iter()
        .map(|b| Row {
            path: format!("system1.{}", b.name),
            what: b.kind.clone(),
            credential: match (b.needs_key, b.key_bound) {
                (false, _) => "none".to_string(),
                (true, true) => "set".to_string(),
                (true, false) => "not set".to_string(),
            },
        })
        .collect())
}
