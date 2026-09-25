//! Priority-3 recovery scenarios that the named collection tests do not cover.
//!
//! A missed native watch event cannot be injected through a public API, and
//! the daemon's poll interval is not a supported switch. These tests do not
//! sleep in place of that event. `Engine::check` has no watcher. The CLI
//! restart case mutates the tree only after the previous daemon pid is gone.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::{BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::atomic::AtomicBool;
use std::thread;
use std::time::{Duration, Instant};

use checkweave::collection::Engine;
use checkweave::compare::{
    CompareBudgets, CompareOutcome, CompareRequest, RetentionLimits, compare,
    replay as replay_compare,
};
use checkweave::execute::{DEPENDENCY_COMPLETENESS, ExecutionLimits, Target};
use checkweave::trace::{self, evidence as trace_evidence, replay as replay_trace, trace};
use checkweave::types::{CheckRequest, Limits, Predicate};
use filetime::{FileTime, set_file_mtime};
use serde_json::{Value, json};

const TIMEOUT: Duration = Duration::from_secs(60);
const KEEP: &str = "{\"k\":\"keep-1\"}\n{\"k\":\"keep-2\"}\n";
const ONLY_A: &str = "{\"k\":\"only-a\"}\n";
const ONLY_B: &str = "{\"k\":\"only-b-1\"}\n{\"k\":\"only-b-2\"}\n";

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_checkweave")
}

#[cfg(unix)]
fn kill_group(pid: u32) {
    let _ = Command::new("kill")
        .args(["-KILL", "--", &format!("-{pid}")])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

#[cfg(unix)]
fn pid_alive(pid: u32) -> bool {
    Command::new("kill")
        .args(["-0", "--", &pid.to_string()])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

struct Fixture {
    _dir: tempfile::TempDir,
    workspace: PathBuf,
    groups: Vec<u32>,
}

impl Fixture {
    fn new() -> Self {
        let dir = tempfile::TempDir::new().expect("temp workspace");
        let workspace = dir.path().to_path_buf();
        Self {
            _dir: dir,
            workspace,
            groups: Vec::new(),
        }
    }

    /// Repo and an empty hooks directory live under one temp root.
    /// The hooks path is outside the work tree so `git add -A` cannot see it.
    fn repo() -> (Self, PathBuf) {
        let dir = tempfile::TempDir::new().expect("temp workspace");
        let workspace = dir.path().join("repo");
        let hooks = dir.path().join("hooks");
        fs::create_dir_all(&workspace).unwrap();
        fs::create_dir_all(&hooks).unwrap();
        (
            Self {
                _dir: dir,
                workspace,
                groups: Vec::new(),
            },
            hooks,
        )
    }

    fn path(&self) -> &Path {
        &self.workspace
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        #[cfg(unix)]
        {
            let identity = self.workspace.join(".checkweave").join("daemon.json");
            if let Some(pid) = fs::read_to_string(&identity)
                .ok()
                .and_then(|text| serde_json::from_str::<Value>(&text).ok())
                .and_then(|value| value.get("pid").and_then(Value::as_u64))
                .map(|pid| pid as u32)
            {
                if let Ok(mut child) = command(self.path(), &["shutdown"])
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .stdin(Stdio::null())
                    .spawn()
                {
                    let shutdown_pid = child.id();
                    self.groups.push(shutdown_pid);
                    let _ = wait_child(
                        &mut child,
                        shutdown_pid,
                        Duration::from_secs(10),
                        "shutdown",
                    );
                }
                kill_group(pid);
            }
            for pid in self.groups.drain(..) {
                kill_group(pid);
            }
        }
    }
}

fn command(workspace: &Path, args: &[&str]) -> Command {
    let mut cmd = Command::new(bin());
    cmd.env("RUST_LOG", "off")
        .env("NO_COLOR", "1")
        .env("CHECKWEAVE_IDLE_SECONDS", "120")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .current_dir(workspace)
        .arg("--workspace")
        .arg(workspace)
        .args(args);
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    cmd
}

fn capture(fixture: &mut Fixture, args: &[&str]) -> (ExitStatus, String, String) {
    let mut child = command(fixture.path(), args)
        .spawn()
        .unwrap_or_else(|err| panic!("spawn {args:?}: {err}"));
    let pid = child.id();
    fixture.groups.push(pid);
    let stdout = child.stdout.take().expect("stdout");
    let stderr = child.stderr.take().expect("stderr");
    let out_thread = thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = Read::read_to_end(&mut BufReader::new(stdout), &mut buf);
        buf
    });
    let err_thread = thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = Read::read_to_end(&mut BufReader::new(stderr), &mut buf);
        buf
    });
    let status = wait_child(&mut child, pid, TIMEOUT, &format!("{args:?}"));
    let stdout = String::from_utf8_lossy(&out_thread.join().expect("stdout")).into_owned();
    let stderr = String::from_utf8_lossy(&err_thread.join().expect("stderr")).into_owned();
    (status, stdout, stderr)
}

fn wait_child(child: &mut Child, pid: u32, timeout: Duration, what: &str) -> ExitStatus {
    let start = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return status,
            Ok(None) if start.elapsed() > timeout => {
                #[cfg(unix)]
                {
                    kill_group(pid);
                }
                let _ = child.kill();
                let _ = child.wait();
                panic!("{what} exceeded {timeout:?}");
            }
            Ok(None) => thread::sleep(Duration::from_millis(20)),
            Err(err) => panic!("wait {what}: {err}"),
        }
    }
}

fn run(fixture: &mut Fixture, args: &[&str]) -> Value {
    let (status, stdout, stderr) = capture(fixture, args);
    assert!(
        status.success(),
        "{args:?} exited {:?}\nstdout:\n{stdout}\nstderr:\n{stderr}",
        status.code()
    );
    serde_json::from_str(stdout.trim()).unwrap_or_else(|err| {
        panic!("{args:?}: stdout is not JSON: {err}\n{stdout}\nstderr:\n{stderr}")
    })
}

#[cfg(unix)]
fn wait_until(mut ready: impl FnMut() -> bool, limit: Duration, what: &str) {
    let start = Instant::now();
    while start.elapsed() < limit {
        if ready() {
            return;
        }
        thread::sleep(Duration::from_millis(20));
    }
    assert!(ready(), "{what} was not reached within {limit:?}");
}

#[cfg(unix)]
fn daemon_pid(workspace: &Path) -> Option<u32> {
    let text = fs::read_to_string(workspace.join(".checkweave").join("daemon.json")).ok()?;
    let value: Value = serde_json::from_str(&text).ok()?;
    value.get("pid")?.as_u64().map(|pid| pid as u32)
}

#[cfg(unix)]
fn shutdown_and_wait(fixture: &mut Fixture) {
    let pid = daemon_pid(fixture.path()).expect("daemon identity before shutdown");
    let status = run(fixture, &["shutdown"]);
    assert_eq!(status["shutting_down"], true, "{status}");
    let identity = fixture.path().join(".checkweave").join("daemon.json");
    wait_until(
        || !identity.exists() && !pid_alive(pid),
        Duration::from_secs(10),
        "daemon exit",
    );
}

fn git(repo: &Path, hooks: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo)
        .arg("-c")
        .arg(format!("core.hooksPath={}", hooks.display()))
        .arg("-c")
        .arg("user.name=Checkweave Recovery")
        .arg("-c")
        .arg("user.email=recovery@example.invalid")
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_AUTHOR_NAME", "Checkweave Recovery")
        .env("GIT_AUTHOR_EMAIL", "recovery@example.invalid")
        .env("GIT_COMMITTER_NAME", "Checkweave Recovery")
        .env("GIT_COMMITTER_EMAIL", "recovery@example.invalid")
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .unwrap_or_else(|err| panic!("spawn git {args:?}: {err}"));
    assert!(
        output.status.success(),
        "git {args:?} exited {:?}\nstderr:\n{}",
        output.status.code(),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

fn predicate() -> String {
    json!({"op": "exists", "path": "/k"}).to_string()
}

fn check_collection(fixture: &mut Fixture) -> Value {
    let predicate = predicate();
    run(
        fixture,
        &["check", "--include", "*.jsonl", "--predicate", &predicate],
    )
}

fn assert_snapshot(report: &Value, files: &[(&str, &str)], hits: u64, misses: u64) {
    assert_eq!(report["execution"], "complete", "{report}");
    assert_eq!(report["freshness"], "validated", "{report}");
    assert_eq!(report["coverage"]["files"], files.len() as u64, "{report}");
    assert_eq!(
        report["coverage"]["cache_hits"], hits,
        "cache hits: {report}"
    );
    assert_eq!(
        report["coverage"]["cache_misses"], misses,
        "cache misses: {report}"
    );
    let mut records = 0u64;
    let mut seen = BTreeSet::new();
    let sources = report["sources"].as_array().expect("sources");
    assert_eq!(sources.len(), files.len(), "{sources:?}");
    for (path, body) in files {
        let source = sources
            .iter()
            .find(|source| source["path"] == *path)
            .unwrap_or_else(|| panic!("missing source {path} in {sources:?}"));
        let bytes = body.as_bytes();
        assert_eq!(source["bytes"], bytes.len() as u64, "{path}");
        assert_eq!(
            source["fingerprint"],
            blake3::hash(bytes).to_hex().as_str(),
            "{path}"
        );
        seen.insert((*path).to_string());
        records += body.lines().count() as u64;
    }
    assert_eq!(report["coverage"]["records"], records, "{report}");
    assert_eq!(report["coverage"]["matched"], records, "{report}");
    let items = report["items"].as_array().expect("items");
    assert_eq!(items.len() as u64, records, "{items:?}");
    for item in items {
        assert_eq!(item["matched"], true, "{item}");
        let path = item["source"]["path"].as_str().unwrap();
        assert!(seen.contains(path), "{path}");
        let fingerprint = item["source"]["fingerprint"].as_str().unwrap();
        let source = sources
            .iter()
            .find(|source| source["path"] == path)
            .unwrap();
        assert_eq!(source["fingerprint"], fingerprint);
    }
}

fn pin_mtime(path: &Path, stamp: FileTime) {
    set_file_mtime(path, stamp).unwrap_or_else(|err| panic!("mtime {}: {err}", path.display()));
}

fn engine_request() -> CheckRequest {
    CheckRequest {
        include: vec!["*.jsonl".into()],
        predicate: Predicate::Exists { path: "/k".into() },
        limits: Limits {
            max_results: 100,
            ..Limits::default()
        },
    }
}

fn require_python() {
    let status = Command::new("python3")
        .arg("--version")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    assert!(
        status.is_ok_and(|status| status.success()),
        "python3 is required for the compare and trace recovery cases"
    );
}

fn compare_request(before: Target, after: Target) -> CompareRequest {
    CompareRequest {
        before,
        after,
        inputs: vec![json!({})],
        generated: None,
        budgets: CompareBudgets {
            max_cases: 4,
            max_executions: 32,
            timeout_ms: 30_000,
        },
        per_execution: ExecutionLimits {
            timeout_ms: 5_000,
            max_output_bytes: 64 * 1024,
        },
        reduce: false,
        repeat: 2,
        before_revision: None,
        after_revision: None,
        policy: Default::default(),
        retention: RetentionLimits {
            max_entries: 4,
            max_total_bytes: 1024 * 1024,
        },
    }
}

fn script_target(argv0: &str, script: &str, env: BTreeMap<String, String>) -> Target {
    Target {
        argv: vec![argv0.into(), script.into()],
        cwd: ".".into(),
        env,
        sources: vec![script.into()],
    }
}

fn bytecode_off() -> BTreeMap<String, String> {
    let mut env = BTreeMap::new();
    env.insert("PYTHONDONTWRITEBYTECODE".into(), "1".into());
    env
}

fn interpreter_env(bin_dir: &Path) -> BTreeMap<String, String> {
    let mut env = bytecode_off();
    let inherited = std::env::var("PATH").unwrap_or_else(|_| "/usr/bin:/bin".into());
    env.insert("PATH".into(), format!("{}:{inherited}", bin_dir.display()));
    env
}

fn write_dep(dir: &Path, value: i64) {
    let _ = fs::remove_dir_all(dir.join("__pycache__"));
    fs::write(
        dir.join("dep.py"),
        format!("def n():\n    return {value}\n"),
    )
    .unwrap();
}

#[cfg(unix)]
fn write_exec(path: &Path, body: &str) {
    use std::os::unix::fs::PermissionsExt;
    fs::write(path, body).unwrap();
    let mut permissions = fs::metadata(path).unwrap().permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(path, permissions).unwrap();
}

fn source_fingerprint(report: &Value, path: &str) -> String {
    report["sources"]
        .as_array()
        .unwrap()
        .iter()
        .find(|source| source["path"] == path)
        .unwrap()["fingerprint"]
        .as_str()
        .unwrap()
        .to_string()
}

#[cfg(unix)]
#[test]
fn live_worker_follows_detached_git_checkout_of_the_checked_collection() {
    let (mut fixture, hooks) = Fixture::repo();
    let repo = fixture.path().to_path_buf();
    git(&repo, &hooks, &["init", "-q", "-b", "main"]);
    fs::write(repo.join("keep.jsonl"), KEEP).unwrap();
    fs::write(repo.join("only_a.jsonl"), ONLY_A).unwrap();
    git(&repo, &hooks, &["add", "--", "keep.jsonl", "only_a.jsonl"]);
    git(&repo, &hooks, &["commit", "-q", "-m", "commit A"]);
    let commit_a = git(&repo, &hooks, &["rev-parse", "HEAD"]);
    fs::remove_file(repo.join("only_a.jsonl")).unwrap();
    fs::write(repo.join("only_b.jsonl"), ONLY_B).unwrap();
    git(
        &repo,
        &hooks,
        &[
            "add",
            "-A",
            "--",
            "keep.jsonl",
            "only_a.jsonl",
            "only_b.jsonl",
        ],
    );
    git(&repo, &hooks, &["commit", "-q", "-m", "commit B"]);
    let commit_b = git(&repo, &hooks, &["rev-parse", "HEAD"]);
    git(&repo, &hooks, &["checkout", "-q", "--detach", &commit_a]);
    let config = fs::read_to_string(repo.join(".git").join("config")).unwrap();
    assert!(
        !config.contains("recovery@example.invalid"),
        "commit identity was written into repo config:\n{config}"
    );
    assert!(
        fs::read_dir(&hooks).unwrap().next().is_none(),
        "hooks directory must stay empty"
    );

    let predicate = predicate();
    run(&mut fixture, &["init", "--agent", "none"]);
    let warm = run(
        &mut fixture,
        &["check", "--include", "*.jsonl", "--predicate", &predicate],
    );
    assert_snapshot(
        &warm,
        &[("keep.jsonl", KEEP), ("only_a.jsonl", ONLY_A)],
        0,
        3,
    );
    let started = run(&mut fixture, &["status"]);
    let pid = started["pid"].as_u64().expect("pid");
    let instance = started["instance"].as_str().unwrap().to_string();

    git(&repo, &hooks, &["checkout", "-q", "--detach", &commit_b]);
    let during = run(&mut fixture, &["status"]);
    assert_eq!(during["pid"], pid, "checkout replaced the worker: {during}");
    assert_eq!(during["instance"], instance);
    let at_b = run(
        &mut fixture,
        &["check", "--include", "*.jsonl", "--predicate", &predicate],
    );
    assert_snapshot(
        &at_b,
        &[("keep.jsonl", KEEP), ("only_b.jsonl", ONLY_B)],
        2,
        2,
    );
    assert!(
        !at_b["sources"]
            .as_array()
            .unwrap()
            .iter()
            .any(|source| source["path"] == "only_a.jsonl")
    );

    git(&repo, &hooks, &["checkout", "-q", "--detach", &commit_a]);
    let returned = run(&mut fixture, &["status"]);
    assert_eq!(returned["pid"], pid, "{returned}");
    assert_eq!(returned["instance"], instance);
    let at_a = run(
        &mut fixture,
        &["check", "--include", "*.jsonl", "--predicate", &predicate],
    );
    assert_snapshot(
        &at_a,
        &[("keep.jsonl", KEEP), ("only_a.jsonl", ONLY_A)],
        3,
        0,
    );
    assert_eq!(
        source_fingerprint(&warm, "keep.jsonl"),
        source_fingerprint(&at_b, "keep.jsonl")
    );
    assert_eq!(
        source_fingerprint(&at_a, "keep.jsonl"),
        source_fingerprint(&warm, "keep.jsonl")
    );
}

#[test]
fn watcherless_engine_check_reads_membership_and_ignore_without_reconcile() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let a = root.join("a.jsonl");
    let b = root.join("b.jsonl");
    let c = root.join("c.jsonl");
    let ignore = root.join(".ignore");
    fs::write(&a, "{\"k\":\"a\"}\n").unwrap();
    fs::write(&b, "{\"k\":\"b\"}\n").unwrap();
    let stamp = FileTime::from_unix_time(1_700_000_000, 0);
    pin_mtime(&a, stamp);
    pin_mtime(&b, stamp);
    let mut engine = Engine::open(root).unwrap();
    let cancel = AtomicBool::new(false);
    let request = engine_request();
    let first = engine.check(&request, &cancel).unwrap();
    assert_eq!(first.coverage.cache_misses, 2);
    assert_eq!(first.coverage.cache_hits, 0);
    assert_eq!(first.sources.len(), 2);

    fs::write(&c, "{\"k\":\"c\"}\n").unwrap();
    fs::write(&ignore, "b.jsonl\n").unwrap();
    pin_mtime(&a, stamp);
    pin_mtime(&b, stamp);
    pin_mtime(&c, stamp);
    pin_mtime(&ignore, stamp);
    let second = engine.check(&request, &cancel).unwrap();
    assert_eq!(second.execution, "complete");
    assert_eq!(second.freshness, "validated");
    assert_eq!(second.coverage.matched, 2);
    assert_eq!(
        second.coverage.cache_hits, 1,
        "unchanged a.jsonl should reuse"
    );
    assert_eq!(second.coverage.cache_misses, 1);
    assert_sources_match_disk(&second, root, &["a.jsonl", "c.jsonl"]);
    assert!(second.sources.iter().all(|source| source.path != "b.jsonl"));

    let historical = engine.evidence(&first.id).unwrap().unwrap();
    assert_eq!(
        historical.freshness, "stale",
        "reading evidence rechecks membership; this does not require reconcile"
    );

    fs::write(&ignore, "").unwrap();
    pin_mtime(&ignore, stamp);
    let restored = engine.check(&request, &cancel).unwrap();
    assert_eq!(restored.coverage.matched, 3);
    assert_eq!(restored.coverage.cache_hits, 3);
    assert_eq!(restored.coverage.cache_misses, 0);
    assert_sources_match_disk(&restored, root, &["a.jsonl", "b.jsonl", "c.jsonl"]);
}

fn assert_sources_match_disk(report: &checkweave::types::CheckReport, root: &Path, paths: &[&str]) {
    assert_eq!(report.sources.len(), paths.len());
    for path in paths {
        let source = report
            .sources
            .iter()
            .find(|source| source.path == *path)
            .unwrap_or_else(|| panic!("missing {path}"));
        let bytes = fs::read(root.join(path)).unwrap();
        assert_eq!(source.bytes, bytes.len() as u64);
        assert_eq!(source.fingerprint, blake3::hash(&bytes).to_hex().as_str());
    }
}

#[cfg(unix)]
#[test]
fn restarted_cli_reads_membership_and_ignore_after_the_watcher_is_gone() {
    let mut fixture = Fixture::new();
    run(&mut fixture, &["init", "--agent", "none"]);
    fs::write(fixture.path().join("a.jsonl"), "{\"k\":\"a\"}\n").unwrap();
    fs::write(fixture.path().join("b.jsonl"), "{\"k\":\"b\"}\n").unwrap();
    let stamp = FileTime::from_unix_time(1_700_000_000, 0);
    pin_mtime(&fixture.path().join("a.jsonl"), stamp);
    pin_mtime(&fixture.path().join("b.jsonl"), stamp);
    let cold = check_collection(&mut fixture);
    assert_snapshot(
        &cold,
        &[
            ("a.jsonl", "{\"k\":\"a\"}\n"),
            ("b.jsonl", "{\"k\":\"b\"}\n"),
        ],
        0,
        2,
    );
    let first_pid = daemon_pid(fixture.path()).expect("daemon pid");
    shutdown_and_wait(&mut fixture);

    fs::write(fixture.path().join("c.jsonl"), "{\"k\":\"c\"}\n").unwrap();
    fs::write(fixture.path().join(".ignore"), "b.jsonl\n").unwrap();
    for name in ["a.jsonl", "b.jsonl", "c.jsonl", ".ignore"] {
        pin_mtime(&fixture.path().join(name), stamp);
    }
    let restarted = check_collection(&mut fixture);
    let second_pid = daemon_pid(fixture.path()).expect("restarted daemon pid");
    assert_ne!(second_pid, first_pid);
    assert_snapshot(
        &restarted,
        &[
            ("a.jsonl", "{\"k\":\"a\"}\n"),
            ("c.jsonl", "{\"k\":\"c\"}\n"),
        ],
        1,
        1,
    );
    let stale = run(&mut fixture, &["evidence", cold["id"].as_str().unwrap()]);
    assert_eq!(stale["freshness"], "stale", "{stale}");

    fs::write(fixture.path().join(".ignore"), "").unwrap();
    pin_mtime(&fixture.path().join(".ignore"), stamp);
    let live = check_collection(&mut fixture);
    assert_eq!(daemon_pid(fixture.path()), Some(second_pid));
    assert_snapshot(
        &live,
        &[
            ("a.jsonl", "{\"k\":\"a\"}\n"),
            ("b.jsonl", "{\"k\":\"b\"}\n"),
            ("c.jsonl", "{\"k\":\"c\"}\n"),
        ],
        3,
        0,
    );
}

#[tokio::test]
async fn undeclared_dependency_change_does_not_invalidate_compare_or_trace_snapshot() {
    require_python();
    let dir = tempfile::tempdir().unwrap();
    let ext = dir.path().join("outside");
    fs::create_dir_all(&ext).unwrap();
    write_dep(&ext, 1);
    let root = dir.path().join("ws");
    fs::create_dir_all(&root).unwrap();
    let literal = serde_json::to_string(ext.to_str().unwrap()).unwrap();
    let body = format!(
        "import json, sys\nsys.dont_write_bytecode = True\njson.load(sys.stdin)\nsys.path.insert(0, {literal})\nimport dep\nprint(json.dumps({{\"n\": dep.n()}}))\n"
    );
    fs::write(root.join("left.py"), &body).unwrap();
    fs::write(root.join("right.py"), &body).unwrap();
    let report = compare(
        &root,
        &compare_request(
            script_target("python3", "left.py", bytecode_off()),
            script_target("python3", "right.py", bytecode_off()),
        ),
        cancel(),
    )
    .await
    .unwrap();
    assert_eq!(
        report.outcome,
        CompareOutcome::BoundedNoDifference,
        "{:?}",
        report.warnings
    );
    assert_eq!(report.dependency_completeness, DEPENDENCY_COMPLETENESS);
    assert!(report.warnings.iter().any(|warning| {
        warning.contains("inherited environment and external state are untracked")
    }));
    assert_eq!(
        source_paths(&report.before_sources),
        vec!["left.py".to_string()]
    );
    assert_eq!(
        source_paths(&report.after_sources),
        vec!["right.py".to_string()]
    );
    assert_eq!(report.freshness, "validated");
    assert_eq!(witness_n(&report.witness_before), 1);
    assert_eq!(witness_n(&report.witness_after), 1);
    assert!(
        report
            .before_sources
            .iter()
            .all(|source| source.path != "dep.py")
    );

    write_dep(&ext, 2);
    let replayed = replay_compare(&root, &report.id, cancel()).await.unwrap();
    assert_eq!(
        replayed.outcome,
        CompareOutcome::BoundedNoDifference,
        "{:?}",
        replayed.warnings
    );
    assert!(
        replayed.changed_sources.is_empty(),
        "{:?}",
        replayed.changed_sources
    );
    assert!(replayed.source_comparisons.iter().all(|item| !item.changed));
    assert_eq!(replayed.freshness, "validated");
    assert_eq!(replayed.outcome_reproduced, Some(false));
    assert_eq!(witness_n(&replayed.witness_before), 2);
    assert_eq!(replayed.dependency_completeness, DEPENDENCY_COMPLETENESS);
    let stored = fs::read(
        root.join(".checkweave/behavior")
            .join(&report.id)
            .join("before-observation.json"),
    )
    .unwrap();
    let stored: Value = serde_json::from_slice(&stored).unwrap();
    assert_eq!(stored["output"]["n"], 1);

    let trace_body = format!(
        "import json, sys\nsys.dont_write_bytecode = True\njson.load(sys.stdin)\nsys.path.insert(0, {literal})\nimport dep\ndef marker():\n    seen = dep.n()\n    return seen\nmarker()\n"
    );
    fs::write(root.join("app.py"), trace_body).unwrap();
    write_dep(&ext, 1);
    let traced = trace(
        &root,
        &trace::TraceRequest::new("app.py", json!({})),
        cancel(),
    )
    .await
    .unwrap();
    assert_eq!(
        traced.execution, "complete",
        "{:?} {}",
        traced.warnings, traced.stderr
    );
    assert_eq!(traced.freshness, "validated");
    assert_eq!(traced.containment, trace::CONTAINMENT);
    assert!(
        traced
            .unsupported
            .iter()
            .any(|item| item == "not_sandboxed")
    );
    assert!(seen_values(&traced).contains(&json!(1)));
    assert!(traced.sources.iter().all(|source| source.path != "dep.py"));
    assert!(traced.sources.iter().any(|source| source.path == "app.py"));

    write_dep(&ext, 2);
    let reread = trace_evidence(&root, &traced.id).await.unwrap().unwrap();
    assert_eq!(reread.freshness, "validated");
    assert!(seen_values(&reread).contains(&json!(1)));
    let again = trace(
        &root,
        &trace::TraceRequest::new("app.py", json!({})),
        cancel(),
    )
    .await
    .unwrap();
    assert_eq!(again.freshness, "validated");
    assert!(seen_values(&again).contains(&json!(2)));
    let replayed_trace = replay_trace(&root, &traced.id, cancel()).await.unwrap();
    assert!(
        seen_values(&replayed_trace).contains(&json!(2)),
        "replay executes the current external module; retained events stay at the original value"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn external_interpreter_wrapper_is_outside_the_compare_snapshot() {
    require_python();
    let dir = tempfile::tempdir().unwrap();
    let outside = dir.path().join("outside");
    fs::create_dir_all(&outside).unwrap();
    let wrapper = outside.join("wrap.sh");
    write_exec(
        &wrapper,
        "#!/bin/sh\nexport CW_MARK=alpha\nexec python3 \"$@\"\n",
    );
    let root = dir.path().join("ws");
    fs::create_dir_all(&root).unwrap();
    let body = "import json, os, sys\njson.load(sys.stdin)\nprint(json.dumps({\"mark\": os.environ.get(\"CW_MARK\")}))\n";
    fs::write(root.join("left.py"), body).unwrap();
    fs::write(root.join("right.py"), body).unwrap();
    let env = interpreter_env(&outside);
    let report = compare(
        &root,
        &compare_request(
            script_target("wrap.sh", "left.py", env.clone()),
            script_target("wrap.sh", "right.py", env),
        ),
        cancel(),
    )
    .await
    .unwrap();
    assert_eq!(
        report.outcome,
        CompareOutcome::BoundedNoDifference,
        "{:?} {}",
        report.warnings,
        report
            .witness_before
            .as_ref()
            .map(|obs| obs.stderr.clone())
            .unwrap_or_default()
    );
    assert_eq!(witness_mark(&report.witness_before), "alpha");
    assert_eq!(
        source_paths(&report.before_sources),
        vec!["left.py".to_string()]
    );
    assert!(
        report
            .before_sources
            .iter()
            .all(|source| source.path == "left.py")
    );
    assert_eq!(report.freshness, "validated");
    assert_eq!(report.dependency_completeness, DEPENDENCY_COMPLETENESS);

    write_exec(
        &wrapper,
        "#!/bin/sh\nexport CW_MARK=beta\nexec python3 \"$@\"\n",
    );
    let replayed = replay_compare(&root, &report.id, cancel()).await.unwrap();
    assert!(
        replayed.changed_sources.is_empty(),
        "{:?}",
        replayed.changed_sources
    );
    assert_eq!(replayed.freshness, "validated");
    assert_eq!(replayed.outcome_reproduced, Some(false));
    assert_eq!(witness_mark(&replayed.witness_before), "beta");
    assert_eq!(replayed.dependency_completeness, DEPENDENCY_COMPLETENESS);
}

fn source_paths(sources: &[checkweave::compare::SourceIdentity]) -> Vec<String> {
    sources.iter().map(|source| source.path.clone()).collect()
}

fn witness_n(obs: &Option<checkweave::execute::Observation>) -> i64 {
    obs.as_ref()
        .and_then(|obs| obs.output.as_ref())
        .and_then(|output| output.get("n"))
        .and_then(Value::as_i64)
        .unwrap_or_else(|| panic!("missing n in {obs:?}"))
}

fn witness_mark(obs: &Option<checkweave::execute::Observation>) -> String {
    obs.as_ref()
        .and_then(|obs| obs.output.as_ref())
        .and_then(|output| output.get("mark"))
        .and_then(Value::as_str)
        .unwrap_or_else(|| panic!("missing mark in {obs:?}"))
        .to_string()
}

fn seen_values(report: &trace::TraceReport) -> Vec<Value> {
    report
        .events
        .iter()
        .filter(|event| event.function == "marker")
        .filter_map(|event| event.locals.get("seen").cloned())
        .collect()
}

fn cancel() -> std::sync::Arc<AtomicBool> {
    std::sync::Arc::new(AtomicBool::new(false))
}

#[cfg(not(unix))]
#[test]
fn recovery_process_cases_require_a_unix_release_target() {
    panic!(
        "detached checkout, daemon restart, and the external interpreter wrapper are specified for Linux and macOS/WSL"
    );
}
