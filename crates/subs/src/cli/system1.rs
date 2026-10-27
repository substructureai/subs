use anyhow::{bail, Result};

use super::cloud::project_config::ProjectConfig;
use super::cloud::ProjectScope;
use super::connections::Row;
use super::env_value;
use super::target::target;

pub(crate) fn key_at(block: &str, scope: &ProjectScope, verb: &str) -> Result<()> {
    let Some(config) = target(&scope.globals)?.here() else {
        bail!("The hosted engine does not run [system1.*] blocks yet. Run the project locally.");
    };
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

pub(crate) fn rows(cfg: &ProjectConfig) -> Vec<Row> {
    cfg.system1
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
        .collect()
}
