use checkweave::execute::{ExecutionLimits, Target, run};
use serde_json::json;
use std::sync::atomic::AtomicU64;
use std::{
    collections::BTreeMap,
    path::Path,
    process::Command,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

struct TempWork(std::path::PathBuf);
impl TempWork {
    fn new() -> Self {
        static N: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "checkweave-exec-{}-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&path).unwrap();
        Self(path)
    }
    fn path(&self) -> &Path {
        &self.0
    }
}
impl Drop for TempWork {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn target(argv: &[&str]) -> Target {
    Target {
        argv: argv.iter().map(|s| (*s).to_string()).collect(),
        cwd: ".".into(),
        env: BTreeMap::new(),
        sources: Vec::new(),
    }
}

fn limits(timeout_ms: u64, max_output_bytes: usize) -> ExecutionLimits {
    ExecutionLimits {
        timeout_ms,
        max_output_bytes,
    }
}

#[tokio::test]
async fn json_round_trip_uses_argv_without_a_shell() {
    let dir = TempWork::new();
    let cancel = Arc::new(AtomicBool::new(false));
    let program = target(&[
        "python3",
        "-c",
        "import json,sys; value=json.load(sys.stdin); json.dump({'argv': sys.argv[1], 'n': value['n']}, sys.stdout)",
        "literal $(id); rm -rf /",
    ]);
    let obs = run(
        dir.path(),
        &program,
        &json!({"n": 4}),
        &limits(5_000, 64_000),
        cancel,
    )
    .await
    .unwrap();
    assert_eq!(obs.outcome, "completed");
    let output = obs.output.unwrap();
    assert_eq!(output["argv"], "literal $(id); rm -rf /");
    assert_eq!(output["n"], 4);
}

#[tokio::test]
async fn invalid_json_is_unsupported_not_a_parsed_prefix() {
    let dir = TempWork::new();
    let program = target(&[
        "python3",
        "-c",
        "import sys; sys.stdout.write('{\"n\": 1} trailing')",
    ]);
    let obs = run(
        dir.path(),
        &program,
        &json!({}),
        &limits(5_000, 64_000),
        Arc::new(AtomicBool::new(false)),
    )
    .await
    .unwrap();
    assert_eq!(obs.outcome, "unsupported_output");
    assert!(obs.output.is_none());
}

#[tokio::test]
async fn crash_without_json_stays_failed_and_json_on_failure_is_kept() {
    let dir = TempWork::new();
    let cancel = Arc::new(AtomicBool::new(false));
    let crash = target(&[
        "python3",
        "-c",
        "import sys; sys.stderr.write('boom'); sys.exit(3)",
    ]);
    let crashed = run(
        dir.path(),
        &crash,
        &json!({}),
        &limits(5_000, 64_000),
        cancel.clone(),
    )
    .await
    .unwrap();
    assert_eq!(crashed.outcome, "failed");
    assert_eq!(crashed.exit_code, Some(3));
    assert!(crashed.output.is_none());

    let coded = target(&[
        "python3",
        "-c",
        "import json,sys; json.dump({'ok': False}, sys.stdout); sys.exit(4)",
    ]);
    let failed = run(
        dir.path(),
        &coded,
        &json!({}),
        &limits(5_000, 64_000),
        cancel,
    )
    .await
    .unwrap();
    assert_eq!(failed.outcome, "failed");
    assert_eq!(failed.exit_code, Some(4));
    assert_eq!(failed.output.unwrap()["ok"], false);
}

#[tokio::test]
async fn stdout_and_stderr_are_bounded_and_do_not_become_success() {
    let dir = TempWork::new();
    let program = target(&[
        "python3",
        "-c",
        "import sys; sys.stderr.write('e'*100000); sys.stdout.write('{\"n\": 1}')",
    ]);
    let obs = run(
        dir.path(),
        &program,
        &json!({}),
        &limits(5_000, 128),
        Arc::new(AtomicBool::new(false)),
    )
    .await
    .unwrap();
    assert_eq!(obs.outcome, "output_limit");
    assert!(obs.stdout.len() <= 128);
    assert!(obs.stderr.len() <= 128);
    assert!(obs.output.is_none());
}

#[tokio::test]
async fn timeout_and_cancel_stop_the_command() {
    let dir = TempWork::new();
    let sleepy = target(&["python3", "-c", "import time; time.sleep(30)"]);
    let timed = run(
        dir.path(),
        &sleepy,
        &json!({}),
        &limits(400, 8_000),
        Arc::new(AtomicBool::new(false)),
    )
    .await
    .unwrap();
    assert_eq!(timed.outcome, "timeout");
    assert!(timed.elapsed_ms < 5_000);

    let cancel = Arc::new(AtomicBool::new(false));
    let flag = cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(80)).await;
        flag.store(true, Ordering::Release);
    });
    let cancelled = run(
        dir.path(),
        &sleepy,
        &json!({}),
        &limits(5_000, 8_000),
        cancel.clone(),
    )
    .await
    .unwrap();
    assert_eq!(cancelled.outcome, "cancelled");
    assert!(cancelled.elapsed_ms < 5_000);

    let early = run(
        dir.path(),
        &sleepy,
        &json!({}),
        &limits(5_000, 8_000),
        Arc::new(AtomicBool::new(true)),
    )
    .await
    .unwrap();
    assert_eq!(early.outcome, "cancelled");
    assert!(early.exit_code.is_none());
}

#[cfg(unix)]
#[tokio::test]
async fn dropping_the_run_future_kills_a_descendant() {
    let dir = TempWork::new();
    let marker = dir.path().join("child.pid");
    let script = "import os,sys,time\npid=os.fork()\nif pid==0:\n time.sleep(60)\n os._exit(0)\nopen(sys.argv[1],'w').write(str(pid))\ntime.sleep(60)\n";
    let marker_arg = marker.to_str().unwrap().to_string();
    let root = dir.path().to_path_buf();
    let task = tokio::spawn(async move {
        let program = target(&["python3", "-c", script, &marker_arg]);
        run(
            &root,
            &program,
            &json!({}),
            &limits(30_000, 8_000),
            Arc::new(AtomicBool::new(false)),
        )
        .await
    });
    for _ in 0..50 {
        if marker.exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let pid =
        std::fs::read_to_string(&marker).expect("child pid was not published before cancellation");
    task.abort();
    let _ = task.await;
    let proc = Path::new("/proc").join(pid.trim());
    for _ in 0..40 {
        if !proc.exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert!(
        !proc.exists(),
        "descendant {} survived dropping run",
        pid.trim()
    );
}

#[cfg(unix)]
#[tokio::test]
async fn unix_process_group_kills_a_grandchild_holding_the_pipe() {
    let dir = TempWork::new();
    let marker = dir.path().join("child.pid");
    let script = "import os,sys,time\npid=os.fork()\nif pid==0:\n time.sleep(60)\n os._exit(0)\nopen(sys.argv[1],'w').write(str(pid))\nos._exit(0)\n";
    let program = target(&["python3", "-c", script, marker.to_str().unwrap()]);
    let obs = run(
        dir.path(),
        &program,
        &json!({}),
        &limits(5_000, 8_000),
        Arc::new(AtomicBool::new(false)),
    )
    .await
    .unwrap();
    assert_ne!(obs.outcome, "timeout");
    assert!(
        obs.elapsed_ms < 5_000,
        "capture waited on a grandchild: {obs:?}"
    );
    let pid = std::fs::read_to_string(&marker).unwrap();
    let proc = Path::new("/proc").join(pid.trim());
    for _ in 0..20 {
        if !proc.exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert!(!proc.exists(), "grandchild {} is still alive", pid.trim());
    assert!(obs.containment.contains("unix_process_group_sigkill"));
    assert!(obs.dependency_completeness.contains("not a sandbox"));
}

#[tokio::test]
async fn rejects_paths_outside_the_declared_workspace_and_records_sources() {
    let dir = TempWork::new();
    std::fs::write(dir.path().join("input.txt"), b"alpha").unwrap();
    let mut program = target(&[
        "python3",
        "-c",
        "import json,sys; json.dump({'ok': True}, sys.stdout)",
    ]);
    program.sources = vec!["input.txt".into()];
    program
        .env
        .insert("CHECKWEAVE_SENTINEL".into(), "from-request".into());
    let seen = target(&[
        "python3",
        "-c",
        "import json,os,sys; json.dump({'env': os.environ.get('CHECKWEAVE_SENTINEL')}, sys.stdout)",
    ]);
    let mut seen = seen;
    seen.env
        .insert("CHECKWEAVE_SENTINEL".into(), "from-request".into());
    let obs = run(
        dir.path(),
        &seen,
        &json!({}),
        &limits(5_000, 8_000),
        Arc::new(AtomicBool::new(false)),
    )
    .await
    .unwrap();
    assert_eq!(obs.output.unwrap()["env"], "from-request");

    let fingerprinted = run(
        dir.path(),
        &program,
        &json!({}),
        &limits(5_000, 8_000),
        Arc::new(AtomicBool::new(false)),
    )
    .await
    .unwrap();
    let expected = checkweave::execute::source_fingerprints(dir.path(), &program).unwrap();
    assert_eq!(fingerprinted.source_fingerprints, expected);
    assert!(!expected.get("input.txt").unwrap().is_empty());
    assert_eq!(fingerprinted.source_freshness, "validated");

    program.sources = vec!["../outside.txt".into()];
    assert!(
        run(
            dir.path(),
            &program,
            &json!({}),
            &limits(5_000, 8_000),
            Arc::new(AtomicBool::new(false))
        )
        .await
        .is_err()
    );
    program.sources = vec![".git/config".into()];
    assert!(
        run(
            dir.path(),
            &program,
            &json!({}),
            &limits(5_000, 8_000),
            Arc::new(AtomicBool::new(false))
        )
        .await
        .is_err()
    );
    program.cwd = "/tmp".into();
    program.sources.clear();
    assert!(
        run(
            dir.path(),
            &program,
            &json!({}),
            &limits(5_000, 8_000),
            Arc::new(AtomicBool::new(false))
        )
        .await
        .is_err()
    );
}

#[test]
fn containment_claim_matches_this_host() {
    let model = checkweave::execute::containment_model();
    if cfg!(unix) {
        assert!(model.contains("unix_process_group_sigkill"));
    }
    if cfg!(windows) {
        assert!(model.contains("windows_direct_child_only"));
    }
    let _ = Command::new("python3")
        .arg("--version")
        .status()
        .expect("python3 is required for execution tests");
}

#[cfg(unix)]
#[test]
fn sources_resolve_through_a_symlinked_workspace_root() {
    let base = tempfile::tempdir().unwrap();
    let real = base.path().join("real");
    std::fs::create_dir_all(&real).unwrap();
    std::fs::write(real.join("a.py"), b"print(1)\n").unwrap();
    let linked = base.path().join("linked");
    std::os::unix::fs::symlink(&real, &linked).unwrap();
    let mut remaining = 1024;
    let hash = checkweave::execute::fingerprint_file(&linked, "a.py", &mut remaining).unwrap();
    assert_eq!(hash, blake3::hash(b"print(1)\n").to_hex().to_string());
    assert!(
        checkweave::execute::fingerprint_file(&linked, "../real/a.py", &mut remaining).is_err()
    );
}
