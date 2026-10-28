use std::ffi::OsString;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use subs::codemode::sandbox::{self, Sandbox};
use subs::codemode::{Script, Step};

fn binary() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_subs"))
}

fn sandbox() -> Sandbox {
    Sandbox::new(binary(), vec![sandbox::ARG.into()])
}

fn script(code: &str) -> Script {
    Script {
        code: code.to_string(),
        calls: vec![],
        seed: 1,
        now_ms: 0,
        spent: false,
        tools: vec![],
    }
}

async fn probe(mode: &str) -> serde_json::Value {
    let args: Vec<OsString> = vec![sandbox::ARG.into(), mode.into()];
    std::env::set_var("SUBS_SANDBOX_TEST_SECRET", "hunter2");
    let output = Sandbox::new(binary(), args)
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
    assert_sealed(&probe(sandbox::PROBE).await);
}

#[tokio::test]
async fn the_os_lock_alone_reaches_nothing_of_the_host() {
    assert_sealed(&probe(sandbox::PROBE_LOCK).await);
}

#[tokio::test]
async fn a_sandbox_process_sees_none_of_the_hosts_environment() {
    let report = probe(sandbox::PROBE).await;
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
    assert_ne!(probe(sandbox::PROBE).await["uid"], 0);
}

#[tokio::test]
async fn scripts_run_in_sandbox_processes() {
    let sandbox = Arc::new(sandbox());
    let step = sandbox
        .run("t", script("return [1 + 1, typeof fetch];"))
        .await;
    assert_eq!(step, Step::Done(r#"{"result":[2,"undefined"]}"#.into()));
    let step = sandbox.run("t", script("return 'again';")).await;
    assert_eq!(step, Step::Done(r#"{"result":"again"}"#.into()));
}

#[tokio::test]
async fn scripts_run_at_once() {
    let sandbox = Arc::new(sandbox().with_slots(4));
    let runs: Vec<_> = (0..20)
        .map(|i| {
            let sandbox = sandbox.clone();
            let tenant = format!("t{}", i % 5);
            tokio::spawn(async move {
                sandbox
                    .run(&tenant, script(&format!("return {i} * 2;")))
                    .await
            })
        })
        .collect();
    for (i, run) in runs.into_iter().enumerate() {
        let step = run.await.unwrap();
        assert_eq!(step, Step::Done(format!(r#"{{"result":{}}}"#, i * 2)));
    }
}

#[tokio::test]
async fn a_script_past_its_deadline_is_killed_and_the_next_runs() {
    let mut sandbox = sandbox();
    sandbox.deadline = Duration::from_millis(300);
    let sandbox = Arc::new(sandbox);
    let step = sandbox.run("t", script("while (true) {}")).await;
    assert!(
        matches!(&step, Step::Failed(m) if m.contains("ran too long")),
        "{step:?}"
    );
    let step = sandbox.run("t", script("return 'next';")).await;
    assert_eq!(step, Step::Done(r#"{"result":"next"}"#.into()));
}

#[tokio::test]
async fn one_tenant_cannot_take_every_slot() {
    let mut sandbox = sandbox().with_slots(2);
    sandbox.deadline = Duration::from_secs(3);
    let sandbox = Arc::new(sandbox);
    let busy: Vec<_> = (0..3)
        .map(|_| {
            let sandbox = sandbox.clone();
            tokio::spawn(async move { sandbox.run("greedy", script("while (true) {}")).await })
        })
        .collect();
    tokio::time::sleep(Duration::from_millis(500)).await;
    let started = Instant::now();
    let step = sandbox.run("polite", script("return 1;")).await;
    assert_eq!(step, Step::Done(r#"{"result":1}"#.into()));
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "waited {:?}",
        started.elapsed()
    );
    for run in busy {
        run.abort();
    }
}

#[tokio::test]
async fn a_program_that_cannot_start_fails_the_run() {
    let sandbox = Arc::new(Sandbox::new(PathBuf::from("/nonexistent/subs"), vec![]));
    let step = sandbox.run("t", script("return 1;")).await;
    assert!(
        matches!(&step, Step::Failed(m) if m.contains("could not start")),
        "{step:?}"
    );
}

#[tokio::test]
async fn a_program_that_does_not_dispatch_is_refused() {
    let sandbox = Arc::new(Sandbox::new(PathBuf::from("/bin/echo"), vec!["hi".into()]));
    let step = sandbox.run("t", script("return 1;")).await;
    assert!(
        matches!(&step, Step::Failed(m) if m.contains("dispatch()")),
        "{step:?}"
    );
}
