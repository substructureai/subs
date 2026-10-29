use std::ffi::OsString;
use std::path::PathBuf;
use std::time::Duration;

use subs::codemode::{
    ProcessRunner, RunError, Script, ScriptFiles, ScriptRunner, Step, ARG, PROBE, PROBE_LOCK,
    VERSION,
};

fn binary() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_subs"))
}

trait Steps {
    async fn step(&self, script: Script) -> Result<Step, RunError>;
}

impl Steps for ProcessRunner {
    async fn step(&self, script: Script) -> Result<Step, RunError> {
        self.run(script, ScriptFiles::none())
            .await
            .map(|ran| ran.step)
    }
}

fn runner() -> ProcessRunner {
    ProcessRunner::new(binary(), vec![ARG.into()])
}

fn script(code: &str) -> Script {
    Script {
        version: VERSION,
        language: Default::default(),
        limits: Default::default(),
        cpu_used_ms: 0,
        files: vec![],
        file_bytes_used: 0,
        code: code.to_string(),
        calls: vec![],
        seed: 1,
        now_ms: 0,
        spent: false,
        tools: vec![],
    }
}

async fn probe(mode: &str) -> serde_json::Value {
    let args: Vec<OsString> = vec![ARG.into(), mode.into()];
    std::env::set_var("SUBS_SANDBOX_TEST_SECRET", "hunter2");
    let output = ProcessRunner::new(binary(), args)
        .command()
        .output()
        .await
        .expect("the probe runs");
    serde_json::from_slice(&output.stdout).expect("the probe reports JSON")
}

fn assert_sealed(report: &serde_json::Value) {
    for reach in [
        "read_file",
        "proc_environ",
        "parent_environ",
        "write_file",
        "list_dir",
        "tcp",
        "udp",
        "unix_socket",
        "spawn",
        "signal_parent",
    ] {
        let result = report[reach].as_str().unwrap_or_default();
        assert!(
            result.starts_with("denied"),
            "{reach}: {result} in {report}"
        );
    }
}

#[tokio::test]
async fn a_sandbox_process_reaches_nothing_of_the_host() {
    assert_sealed(&probe(PROBE).await);
}

#[tokio::test]
async fn the_os_lock_alone_reaches_nothing_of_the_host() {
    assert_sealed(&probe(PROBE_LOCK).await);
}

#[tokio::test]
async fn a_sandbox_process_sees_none_of_the_hosts_environment() {
    let report = probe(PROBE).await;
    let names: Vec<&str> = report["env"]
        .as_array()
        .expect("env names")
        .iter()
        .filter_map(|v| v.as_str())
        .filter(|name| *name != "__CF_USER_TEXT_ENCODING")
        .collect();
    assert!(names.is_empty(), "the sandbox saw {names:?}");
}

#[tokio::test]
async fn a_sandbox_process_is_never_root() {
    assert_ne!(probe(PROBE).await["uid"], 0);
}

#[tokio::test]
async fn scripts_run_in_sandbox_processes() {
    let runner = runner();
    let step = runner.step(script("return [1 + 1, typeof fetch];")).await;
    assert_eq!(step, Ok(Step::Done(r#"{"result":[2,"undefined"]}"#.into())));
    let step = runner.step(script("return 'again';")).await;
    assert_eq!(step, Ok(Step::Done(r#"{"result":"again"}"#.into())));
}

#[tokio::test]
async fn scripts_run_at_once() {
    let runner = std::sync::Arc::new(runner());
    let runs: Vec<_> = (0..20)
        .map(|i| {
            let runner = runner.clone();
            tokio::spawn(async move { runner.step(script(&format!("return {i} * 2;"))).await })
        })
        .collect();
    for (i, run) in runs.into_iter().enumerate() {
        let step = run.await.unwrap();
        assert_eq!(step, Ok(Step::Done(format!(r#"{{"result":{}}}"#, i * 2))));
    }
}

#[tokio::test]
async fn a_script_past_its_deadline_fails_and_the_next_runs() {
    let runner = runner().with_deadline(Duration::from_millis(300));
    let step = runner.step(script("while (true) {}")).await;
    assert!(
        matches!(&step, Ok(Step::Failed(m)) if m.contains("ran too long")),
        "{step:?}"
    );
    let step = runner.step(script("return 'next';")).await;
    assert_eq!(step, Ok(Step::Done(r#"{"result":"next"}"#.into())));
}

#[tokio::test]
async fn a_script_of_a_version_the_runner_does_not_take_is_tried_again() {
    let mut newer = script("return 1;");
    newer.version = VERSION + 1;
    let step = runner().step(newer).await;
    assert!(
        matches!(&step, Err(RunError(m)) if m.contains("version")),
        "{step:?}"
    );
}

#[tokio::test]
async fn a_runner_that_cannot_start_now_is_tried_again() {
    let runner = ProcessRunner::new(PathBuf::from("/nonexistent/subs"), vec![]);
    let step = runner.step(script("return 1;")).await;
    assert!(
        matches!(&step, Err(RunError(m)) if m.contains("did not start")),
        "{step:?}"
    );
}

#[tokio::test]
async fn a_program_that_is_not_a_runner_fails_the_script() {
    let runner = ProcessRunner::new(PathBuf::from("/bin/echo"), vec!["hi".into()]);
    let step = runner.step(script("return 1;")).await;
    assert!(
        matches!(&step, Ok(Step::Failed(m)) if m.contains("dispatch()")),
        "{step:?}"
    );
}

mod files {
    use super::*;
    use std::collections::BTreeMap;
    use std::sync::Arc;
    use subs::codemode::{FileSource, ScriptFile};
    use subs::runtime::blob::{BlobError, BlobRef, BlobStore, MemoryBlobStore, NewBlob};

    async fn put(store: &MemoryBlobStore, tenant: &str, text: &str) -> BlobRef {
        store
            .put(NewBlob {
                tenant_id: tenant.to_string(),
                mime: "text/csv".to_string(),
                name: None,
                bytes: text.as_bytes().to_vec(),
            })
            .await
            .unwrap()
    }

    fn reading(code: &str, files: &[(&str, &BlobRef)]) -> Script {
        let mut s = script(code);
        s.files = files
            .iter()
            .map(|(id, r)| ScriptFile {
                id: id.to_string(),
                mime: r.mime.clone(),
                size: r.size,
                skill: None,
            })
            .collect();
        s
    }

    fn grant(store: Arc<dyn BlobStore>, files: &[(&str, &BlobRef)]) -> ScriptFiles {
        ScriptFiles {
            tenant_id: "t1".to_string(),
            sources: files
                .iter()
                .map(|(id, r)| (id.to_string(), FileSource::Blob((*r).clone())))
                .collect::<BTreeMap<_, _>>(),
            store,
        }
    }

    #[tokio::test]
    async fn a_sandboxed_script_streams_a_file_through_its_runner() {
        let store = Arc::new(MemoryBlobStore::new());
        let mut csv = String::from("region,amount\n");
        for i in 0..20_000 {
            csv.push_str(&format!("{},{i}\n", ["EU", "US"][i % 2]));
        }
        let sales = put(&store, "t1", &csv).await;
        let code = "let n = 0, total = 0;\n\
            for await (const r of files.csv('sales.csv')) { n++; total += Number(r.amount); }\n\
            return [n, total];";
        let ran = runner()
            .run(
                reading(code, &[("sales.csv", &sales)]),
                grant(store, &[("sales.csv", &sales)]),
            )
            .await
            .unwrap();
        let total: u64 = (0..20_000).sum();
        assert_eq!(
            ran.step,
            Step::Done(format!(r#"{{"result":[20000,{total}]}}"#))
        );
        assert_eq!(ran.stats.file_bytes, csv.len() as u64);
    }

    #[tokio::test]
    async fn a_script_reads_no_blob_of_another_tenant() {
        let store = Arc::new(MemoryBlobStore::new());
        let theirs = put(&store, "t2", "secret").await;
        let code = "try { return await files.text('x.csv'); } catch (e) { return e.message; }";
        let ran = runner()
            .run(
                reading(code, &[("x.csv", &theirs)]),
                grant(store, &[("x.csv", &theirs)]),
            )
            .await
            .unwrap();
        assert!(
            matches!(&ran.step, Step::Done(t) if t.contains("There is no file") && !t.contains("secret")),
            "{ran:?}"
        );
    }

    #[tokio::test]
    async fn a_script_reads_a_skills_file_by_its_path() {
        let id = "runbooks:respond/references/escalation.md";
        let mut s = script(
            "return [files.list().map(f => f.skill), await files.text('runbooks:respond/references/escalation.md')];",
        );
        s.files = vec![ScriptFile {
            id: id.to_string(),
            mime: "text/markdown".to_string(),
            size: 13,
            skill: Some("runbooks:respond".to_string()),
        }];
        let files = ScriptFiles {
            tenant_id: "t1".to_string(),
            sources: BTreeMap::from([(
                id.to_string(),
                FileSource::Text(Arc::from("Page the lead")),
            )]),
            store: Arc::new(MemoryBlobStore::new()),
        };
        let ran = runner().run(s, files).await.unwrap();
        assert_eq!(
            ran.step,
            Step::Done(r#"{"result":[["runbooks:respond"],"Page the lead"]}"#.into()),
            "a skill's file is listed with its skill, and read by id"
        );
    }

    struct Down;

    #[async_trait::async_trait]
    impl BlobStore for Down {
        async fn put(&self, _: NewBlob) -> Result<BlobRef, BlobError> {
            Err(BlobError::Io("down".into()))
        }
        async fn get(&self, _: &BlobRef) -> Result<Vec<u8>, BlobError> {
            Err(BlobError::Io("down".into()))
        }
        async fn read(&self, _: &BlobRef, _: std::ops::Range<u64>) -> Result<Vec<u8>, BlobError> {
            Err(BlobError::Io("down".into()))
        }
    }

    #[tokio::test]
    async fn a_store_that_is_down_tries_the_step_again() {
        let store = MemoryBlobStore::new();
        let sales = put(&store, "t1", "a,b\n1,2\n").await;
        let result = runner()
            .run(
                reading(
                    "return await files.text('sales.csv');",
                    &[("sales.csv", &sales)],
                ),
                grant(Arc::new(Down), &[("sales.csv", &sales)]),
            )
            .await;
        assert!(
            matches!(&result, Err(RunError(m)) if m.contains("could not be read")),
            "{result:?}"
        );
    }

    #[tokio::test]
    async fn a_run_reads_no_more_than_its_budget() {
        let store = Arc::new(MemoryBlobStore::new());
        let line = format!("{}\n", "x".repeat(1023));
        let big = put(&store, "t1", &line.repeat(2048)).await;
        let mut s = reading(
            "let n = 0; for await (const l of files.lines('big.txt')) n++; return n;",
            &[("big.txt", &big)],
        );
        s.limits.file_bytes = 1 << 20;
        let ran = runner()
            .run(s, grant(store, &[("big.txt", &big)]))
            .await
            .unwrap();
        assert!(
            matches!(&ran.step, Step::Failed(m) if m.contains("limit of file bytes")),
            "{ran:?}"
        );
        assert!(ran.stats.file_bytes <= 1 << 20);
    }
}
