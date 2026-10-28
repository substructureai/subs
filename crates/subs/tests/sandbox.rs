use std::ffi::OsString;
use std::path::PathBuf;
use std::time::Duration;

use subs::codemode::{
    ProcessRunner, RunError, Script, ScriptRunner, Step, ARG, PROBE, PROBE_LOCK, VERSION,
};

fn binary() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_subs"))
}

trait Steps {
    async fn step(&self, script: Script) -> Result<Step, RunError>;
}

impl Steps for ProcessRunner {
    async fn step(&self, script: Script) -> Result<Step, RunError> {
        self.run(script).await.map(|ran| ran.step)
    }
}

fn runner() -> ProcessRunner {
    ProcessRunner::new(binary(), vec![ARG.into()])
}

fn script(code: &str) -> Script {
    Script {
        version: VERSION,
        limits: Default::default(),
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
