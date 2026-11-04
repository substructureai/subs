use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::codemode::{
    settle, Admission, Limit, Limits, LimitsResolver, Ran, ScriptFile, ScriptFiles, ScriptRunner,
    Stats, Step,
};
use crate::plugins::text_mime;
use crate::plugins::PluginResolver;
use crate::protocol::RetryPolicy;
use crate::runtime::blob::{BlobRef, BlobStore, NewBlob};
use crate::runtime::executor::{
    settled, spawn_bounded_executors, Bound, BoundedTask, ExecutorPool, RESULT_NOT_SAVED,
};
use crate::runtime::inbox::Inbox;
use crate::runtime::queue::{Queue, SCRIPT};
use crate::runtime::sessions::Sessions;
use crate::runtime::span::SpanContext;
use substructure_session::codemode::StoredFile;
use substructure_session::plugins::{PluginBundle, SkillFile};
use substructure_session::Session;

/// Run a `run_code` call's script again. The script and its calls are read
/// from the session when the step runs.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScriptTask {
    pub session_id: String,
    pub tenant_id: String,
    pub tool_call_id: String,
    pub attempt: u32,
    pub retry: RetryPolicy,
    #[serde(default = "Utc::now")]
    pub queued_at: DateTime<Utc>,
}

/// A step checks its own wait, after it has a slot, and reads the session
/// once, so the executor holds it to nothing.
impl BoundedTask for ScriptTask {
    fn bound(&self) -> Option<Bound> {
        None
    }
}

/// Steps waiting for a slot, per engine.
const WAITING: usize = 64;

pub struct Scripts {
    pub runner: Arc<dyn ScriptRunner>,
    pub admission: Arc<Admission>,
    pub limits: Arc<dyn LimitsResolver>,
    /// How long a step may wait for a slot, or for a runner to take it.
    pub wait: Duration,
    /// Where the scripts' files are.
    pub blobs: Arc<dyn BlobStore>,
    /// Where the skills' files are.
    pub plugins: Arc<dyn PluginResolver>,
}

pub fn spawn_script_executor(
    sessions: Arc<Sessions>,
    scripts: Arc<Scripts>,
    queue: Arc<dyn Queue<ScriptTask>>,
    cancel: CancellationToken,
) -> Vec<JoinHandle<()>> {
    let pool = ExecutorPool {
        workers: 1,
        concurrency: WAITING,
    };
    let inbox: Arc<dyn Inbox> = sessions.clone();
    spawn_bounded_executors(inbox, queue, SCRIPT, pool, cancel, move |task, span| {
        let (sessions, scripts) = (sessions.clone(), scripts.clone());
        async move { step(&sessions, &scripts, task, span).await }
    })
}

async fn step(
    sessions: &Sessions,
    scripts: &Scripts,
    task: ScriptTask,
    span: SpanContext,
) -> Result<(), String> {
    let _ticket = scripts.admission.admit(&task.tenant_id).await;
    let waited = (Utc::now() - task.queued_at).to_std().unwrap_or_default();
    let session = sessions
        .load(&task.tenant_id, &task.session_id)
        .await
        .map_err(|e| e.to_string())?;
    let Some(session) = session else {
        return Ok(());
    };
    let limits = scripts.limits.resolve(&task.tenant_id).await;
    let Some(mut script) = session.script(&task.tool_call_id, &limits) else {
        return Ok(());
    };
    let mut sources: BTreeMap<String, BlobRef> = session
        .script_files(&task.tool_call_id)
        .into_iter()
        .map(|(file, blob)| (file.id, blob))
        .collect();
    let stored = match session.skill_files_stored(&task.tool_call_id) {
        true => None,
        false => {
            let stored = store_skill_files(&session, &task, scripts).await?;
            for f in &stored {
                if let Some(blob) = BlobRef::parse(&f.uri) {
                    sources.insert(f.file.id.clone(), blob);
                }
                script.files.push(f.file.clone());
            }
            Some(stored)
        }
    };
    let files = ScriptFiles {
        tenant_id: task.tenant_id.clone(),
        sources: sources.clone(),
        store: scripts.blobs.clone(),
    };
    let known = script.calls.len() as u32;
    let records: usize = script.calls.iter().map(|c| c.output.len()).sum();
    let started = Instant::now();
    let ran = match waited > scripts.wait {
        true => late(waited, Limit::Wait),
        false => match scripts.runner.run(script, files).await {
            Ok(ran) => ran,
            Err(e) if waited + RESULT_NOT_SAVED > scripts.wait => {
                tracing::warn!(tenant_id = %task.tenant_id, error = %e, "no runner took a script step");
                late(waited, Limit::Start)
            }
            Err(e) => return Err(e.to_string()),
        },
    };
    report(
        &task,
        known,
        records,
        waited,
        started.elapsed(),
        &ran,
        &limits,
    );
    let command = settle(
        task.tool_call_id,
        task.attempt,
        known,
        ran,
        &sources,
        stored,
    );
    settled(
        sessions,
        &task.tenant_id,
        &task.session_id,
        command,
        span.child("script"),
    )
    .await
}

/// The blobs of the agent's bundle skills' files, as `<skill>/<path>`, so
/// every run of the script reads the same bytes. Text a bundle holds inline
/// is stored here.
async fn store_skill_files(
    session: &Session,
    task: &ScriptTask,
    scripts: &Scripts,
) -> Result<Vec<StoredFile>, String> {
    let mut bundles: BTreeMap<String, Option<PluginBundle>> = BTreeMap::new();
    let mut stored = Vec::new();
    for (named, bundle, skill) in session.script_skills(&task.tool_call_id) {
        if !bundles.contains_key(&bundle) {
            let found = scripts.plugins.resolve(&task.tenant_id, &bundle).await;
            bundles.insert(bundle.clone(), found);
        }
        let Some(skill) = bundles[&bundle].as_ref().and_then(|b| b.skill(&skill)) else {
            continue;
        };
        for (path, file) in &skill.files {
            let id = format!("{named}/{path}");
            let blob = match file {
                SkillFile::Stored(file) => BlobRef::parse(&file.uri),
                SkillFile::Text(text) => Some(
                    scripts
                        .blobs
                        .put(NewBlob {
                            tenant_id: task.tenant_id.clone(),
                            mime: text_mime(path).to_string(),
                            name: Some(id.clone()),
                            bytes: text.as_bytes().to_vec(),
                        })
                        .await
                        .map_err(|e| e.to_string())?,
                ),
            };
            if let Some(blob) = blob {
                stored.push(stored_file(id, &named, blob));
            }
        }
        for (path, binary) in &skill.binaries {
            let Some(blob) = BlobRef::parse(&binary.uri) else {
                continue;
            };
            stored.push(stored_file(format!("{named}/{path}"), &named, blob));
        }
    }
    Ok(stored)
}

fn stored_file(id: String, skill: &str, blob: BlobRef) -> StoredFile {
    StoredFile {
        file: ScriptFile {
            id,
            mime: blob.mime.clone(),
            size: blob.size,
            skill: Some(skill.to_string()),
        },
        uri: blob.uri(),
    }
}

fn late(waited: Duration, limit: Limit) -> Ran {
    let message = match limit {
        Limit::Wait => substructure_session::prompts::codemode::no_free_sandbox(waited.as_secs()),
        _ => substructure_session::prompts::codemode::no_runner(waited.as_secs()),
    };
    Ran {
        step: Step::Failed(message),
        stats: Stats {
            limit: Some(limit),
            ..Stats::default()
        },
        shown: Vec::new(),
    }
}

fn report(
    task: &ScriptTask,
    known: u32,
    records: usize,
    waited: Duration,
    ran_for: Duration,
    ran: &Ran,
    limits: &Limits,
) {
    let outcome = match &ran.step {
        Step::Calls(calls) => format!("calls:{}", calls.len()),
        Step::Done(_) => "done".to_string(),
        Step::Failed(_) => "failed".to_string(),
    };
    let limit = ran
        .stats
        .limit
        .map(|l| format!("{l:?}"))
        .unwrap_or_default();
    tracing::info!(
        target: "codemode",
        tenant_id = %task.tenant_id,
        session_id = %task.session_id,
        tool_call_id = %task.tool_call_id,
        step = known,
        records_bytes = records,
        wait_ms = waited.as_millis() as u64,
        run_ms = ran_for.as_millis() as u64,
        warm = ran.stats.warm,
        heap = ran.stats.heap,
        cpu_ms = ran.stats.cpu_ms,
        file_bytes = ran.stats.file_bytes,
        %outcome,
        %limit,
        "script step"
    );
    let heap = ran.stats.heap > limits.memory / 2;
    let cpu = ran.stats.cpu_ms > limits.compute_ms / 2;
    if heap || cpu {
        tracing::warn!(
            target: "codemode",
            tenant_id = %task.tenant_id,
            tool_call_id = %task.tool_call_id,
            heap = ran.stats.heap,
            cpu_ms = ran.stats.cpu_ms,
            "a script step used over half its memory or compute"
        );
    }
}
