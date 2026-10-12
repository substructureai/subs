use std::env;
use std::path::PathBuf;

use anyhow::{bail, Context as _, Result};

use super::context::Context;
use super::pickers;
use super::project_config::{self, ProjectConfig, FILENAME};
use super::{print, CloudGlobals};

#[derive(Debug, clap::Args)]
pub struct LinkCommand {
    /// Org id to pin. Skips the org picker.
    #[arg(long)]
    pub org: Option<String>,
    /// Project id to pin. Skips the project picker. Pass `--project=` (empty)
    /// to omit pinning a project while still pinning an org.
    #[arg(long)]
    pub project: Option<String>,
    /// Repin a file that already names an org or project.
    #[arg(long)]
    pub force: bool,
    #[command(flatten)]
    pub globals: CloudGlobals,
}

fn target(globals: &CloudGlobals) -> Result<(PathBuf, ProjectConfig)> {
    let found = match globals.config.as_deref() {
        Some(path) if !path.exists() => None,
        path => project_config::resolve(path)?,
    };
    match found {
        Some(found) => Ok((found.path, found.config)),
        None => {
            let path = match globals.config.clone() {
                Some(path) => path,
                None => env::current_dir()
                    .context("could not determine cwd")?
                    .join(FILENAME),
            };
            Ok((path, ProjectConfig::default()))
        }
    }
}

pub async fn run(cmd: LinkCommand) -> Result<()> {
    let (path, existing) = target(&cmd.globals)?;
    if !cmd.force {
        if let Some(pinned) = existing.org().or(existing.project()) {
            bail!(
                "{} is already linked to {pinned}. Pass --force to relink.",
                path.display()
            );
        }
    }

    let ctx = Context::with_config(&cmd.globals, Some(existing.clone()))?;
    let interactive = pickers::interactive(&cmd.globals);

    let org = if let Some(o) = cmd.org.clone() {
        o
    } else if let Some(o) = ctx.server_default_org().await? {
        o
    } else if interactive {
        pickers::pick_org(&ctx).await?
    } else {
        bail!("no org to pin. Pass --org <id>.")
    };

    let project: Option<String> = match cmd.project.clone() {
        Some(s) if s.is_empty() => None,
        Some(s) => Some(s),
        None => match ctx.server_default_project().await.unwrap_or(None) {
            Some(a) => Some(a),
            None if interactive => pickers::pick_project(&ctx, &org).await?,
            None => None,
        },
    };

    let mut config = existing;
    let remote = config.remote_mut();
    remote.org = Some(org.clone());
    remote.project = project.clone();
    remote.url = cmd.globals.url.clone().or(remote.url.take());
    let url = remote.url.clone();
    project_config::write(&path, &config)?;

    if cmd.globals.json {
        return print::json(&serde_json::json!({
            "wrote": path,
            "org": org,
            "project": project,
            "url": url,
        }));
    }

    println!("Wrote {}", path.display());
    println!("  org = {org}");
    if let Some(a) = &project {
        println!("  project = {a}");
    }
    if let Some(u) = &url {
        println!("  url = {u}");
    }
    if project.is_none() {
        println!();
        println!("No project pinned. `subs apply` creates one from this file and pins it.");
    } else {
        println!();
        println!("`subs apply` pushes what this file declares.");
    }
    Ok(())
}
