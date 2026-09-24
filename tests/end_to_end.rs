//! Kernel path: init a non-git workspace, check a JSONL collection, edit one
//! record, and read evidence. Subprocesses are timed out and process groups
//! are killed so a daemon cannot linger.

use std::fs;
use std::io::{BufReader, Read, Write};
use std::net::TcpListener;
use std::path::Path;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use checkweave::types::{IMPLEMENTATION_VERSION, JSON_POINTER_ROOT, PROTOCOL_VERSION};
use checkweave::workspace::Workspace;
use serde_json::{Value, json};

const TIMEOUT: Duration = Duration::from_secs(60);

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_checkweave")
}

fn kill_group(pid: u32) {
    #[cfg(unix)]
    {
        let _ = Command::new("kill")
            .args(["-KILL", "--", &format!("-{pid}")])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
    #[cfg(not(unix))]
    {
        let _ = pid;
    }
}

struct EnvGuard {
    key: &'static str,
}

impl EnvGuard {
    fn set(key: &'static str, value: impl AsRef<std::ffi::OsStr>) -> Self {
        unsafe { std::env::set_var(key, value) };
        Self { key }
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        unsafe { std::env::remove_var(self.key) };
    }
}

struct Fixture {
    dir: tempfile::TempDir,
    groups: Vec<u32>,
}

impl Fixture {
    fn new() -> Self {
        Self {
            dir: tempfile::TempDir::new().expect("temp workspace"),
            groups: Vec::new(),
        }
    }

    fn path(&self) -> &Path {
        self.dir.path()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let identity = self.dir.path().join(".checkweave").join("daemon.json");
        if let Some(pid) = fs::read_to_string(&identity)
            .ok()
            .and_then(|text| serde_json::from_str::<Value>(&text).ok())
            .and_then(|value| value.get("pid").and_then(Value::as_u64))
            .map(|pid| pid as u32)
        {
            if let Ok(mut child) = command(self.dir.path(), &["shutdown"])
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

fn command(workspace: &Path, args: &[&str]) -> Command {
    let mut cmd = Command::new(bin());
    cmd.env("RUST_LOG", "off")
        .env("NO_COLOR", "1")
        .env(
            "CHECKWEAVE_IDLE_SECONDS",
            std::env::var("CHECKWEAVE_IDLE_SECONDS_TEST").unwrap_or_else(|_| "30".into()),
        )
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
                kill_group(pid);
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

fn write_records(dir: &Path, body: &str) {
    fs::write(dir.join("records.jsonl"), body).expect("write records");
}

const OPEN_ROWS: &str = "\
{\"id\":\"a\",\"status\":\"open\"}\n\
{\"id\":\"b\",\"status\":\"done\"}\n\
{\"id\":\"c\",\"status\":\"open\"}\n";

#[test]
fn check_max_results_zero_reports_counts_without_rows() {
    let mut fixture = Fixture::new();
    write_records(fixture.path(), OPEN_ROWS);
    run(&mut fixture, &["init", "--agent", "none"]);
    let predicate = json!({"op":"eq","path":"/status","value":"open"}).to_string();
    let report = run(
        &mut fixture,
        &[
            "check",
            "--include",
            "records.jsonl",
            "--predicate",
            &predicate,
            "--max-results",
            "0",
        ],
    );
    assert_eq!(report["execution"], "complete", "{report}");
    assert_eq!(report["coverage"]["matched"], 2, "{report}");
    assert_eq!(report["coverage"]["records"], 3, "{report}");
    let items = report["items"].as_array().expect("items");
    assert!(items.is_empty(), "{report}");
}

#[test]
fn implementation_version_is_crate_version() {
    assert_eq!(IMPLEMENTATION_VERSION, env!("CARGO_PKG_VERSION"));
    assert_eq!(PROTOCOL_VERSION, 1);
    assert_eq!(JSON_POINTER_ROOT, "");
}

#[test]
fn init_check_edit_and_evidence_on_non_git_workspace() {
    let mut fixture = Fixture::new();
    assert!(
        !fixture.path().join(".git").exists(),
        "fixture must not require git"
    );
    write_records(fixture.path(), OPEN_ROWS);

    let first = run(&mut fixture, &["init", "--agent", "none"]);
    assert!(first.get("root").and_then(Value::as_str).is_some());
    let again = run(&mut fixture, &["init", "--agent", "none"]);
    assert_eq!(first["root"], again["root"]);

    let predicate = json!({"op":"eq","path":"/status","value":"open"}).to_string();
    let report = run(
        &mut fixture,
        &[
            "check",
            "--include",
            "records.jsonl",
            "--predicate",
            &predicate,
        ],
    );
    assert_eq!(report["execution"], "complete");
    assert_eq!(report["freshness"], "validated");
    assert_eq!(report["coverage"]["matched"], 2);
    assert_eq!(report["coverage"]["unmatched"], 1);
    assert_eq!(report["coverage"]["unresolved"], 0);
    let id = report["id"].as_str().expect("report id").to_string();

    let evidence = run(&mut fixture, &["evidence", &id]);
    assert_eq!(evidence["id"], id);
    assert_eq!(evidence["coverage"]["matched"], 2);

    let repeated = run(
        &mut fixture,
        &[
            "check",
            "--include",
            "records.jsonl",
            "--predicate",
            &predicate,
        ],
    );
    assert_eq!(repeated["coverage"]["matched"], 2);
    assert!(
        repeated["coverage"]["cache_hits"].as_u64().unwrap_or(0) > 0,
        "unchanged records should be reused: {repeated}"
    );

    write_records(
        fixture.path(),
        "\
{\"id\":\"a\",\"status\":\"open\"}\n\
{\"id\":\"b\",\"status\":\"done\"}\n\
{\"id\":\"c\",\"status\":\"done\"}\n",
    );
    let edited = run(
        &mut fixture,
        &[
            "check",
            "--include",
            "records.jsonl",
            "--predicate",
            &predicate,
        ],
    );
    assert_eq!(edited["execution"], "complete");
    assert_eq!(edited["freshness"], "validated");
    assert_eq!(edited["coverage"]["matched"], 1);
    assert_eq!(edited["coverage"]["unmatched"], 2);

    let status = run(&mut fixture, &["status"]);
    assert_eq!(status["protocol"], PROTOCOL_VERSION);
    assert!(status.get("pid").and_then(Value::as_u64).is_some());
}

#[test]
fn run_cancel_marks_the_handle_without_failing_the_process() {
    let mut fixture = Fixture::new();
    write_records(fixture.path(), "{\"id\":\"a\"}\n");
    run(&mut fixture, &["init", "--agent", "none"]);
    let request = fixture.path().join("check.json");
    fs::write(
        &request,
        r#"{"operation":"check","include":["records.jsonl"],"predicate":{"op":"exists","path":"/id"}}"#,
    )
    .unwrap();
    let started = run(
        &mut fixture,
        &["run", "start", "--request-file", request.to_str().unwrap()],
    );
    let id = started["id"].as_str().expect("run id").to_string();
    let cancelled = run(&mut fixture, &["run", "cancel", &id]);
    assert_eq!(cancelled["state"], "cancelled");
    assert_eq!(cancelled["id"], id);
}

#[test]
fn user_rule_and_negated_gitignore_are_preserved() {
    let dir = tempfile::TempDir::new().expect("temp");
    let root = dir.path();
    fs::create_dir_all(root.join(".cursor/rules")).unwrap();
    fs::write(
        root.join(".cursor/rules/checkweave.mdc"),
        "user owned rule\n",
    )
    .unwrap();
    fs::write(root.join(".gitignore"), ".checkweave/\n!.checkweave/\n").unwrap();
    let value = Workspace::initialize(root, "cursor").expect("init");
    assert!(
        value["rule"]
            .as_str()
            .unwrap_or("")
            .contains("checkweave:managed"),
        "{value}"
    );
    assert_eq!(
        fs::read_to_string(root.join(".cursor/rules/checkweave.mdc")).unwrap(),
        "user owned rule\n"
    );
    let gitignore = fs::read_to_string(root.join(".gitignore")).unwrap();
    assert!(
        gitignore.ends_with(".checkweave/\n"),
        "negation must not leave state unignored: {gitignore}"
    );
}

#[test]
fn empty_json_pointer_is_the_record_root() {
    let mut fixture = Fixture::new();
    write_records(fixture.path(), "{\"status\":\"open\"}\n");
    run(&mut fixture, &["init", "--agent", "none"]);

    let whole = json!({"op":"kind","path": JSON_POINTER_ROOT,"kind":"object"}).to_string();
    let report = run(
        &mut fixture,
        &["check", "--include", "records.jsonl", "--predicate", &whole],
    );
    assert_eq!(report["coverage"]["matched"], 1, "{report}");
    assert_eq!(report["coverage"]["unresolved"], 0);

    let slash = json!({"op":"exists","path":"/"}).to_string();
    let empty_key = run(
        &mut fixture,
        &["check", "--include", "records.jsonl", "--predicate", &slash],
    );
    assert_eq!(
        empty_key["coverage"]["matched"], 0,
        "`/` is the empty-name member, not the record: {empty_key}"
    );
    assert_eq!(empty_key["coverage"]["unmatched"], 1);
}

fn poll_run(fixture: &mut Fixture, id: &str) -> Value {
    let start = Instant::now();
    loop {
        let snapshot = run(fixture, &["run", "status", id]);
        match snapshot["state"].as_str() {
            Some("queued") | Some("running") if start.elapsed() < Duration::from_secs(45) => {
                thread::sleep(Duration::from_millis(40));
            }
            Some("queued") | Some("running") => panic!("run {id} did not finish: {snapshot}"),
            _ => return snapshot,
        }
    }
}

#[test]
fn deinit_removes_only_managed_cursor_entries() {
    let mut fixture = Fixture::new();
    run(&mut fixture, &["init", "--agent", "cursor"]);
    let root = fixture.path().to_path_buf();
    fs::write(root.join(".cursor/rules/other.mdc"), "leave me\n").unwrap();
    let mcp_path = root.join(".cursor/mcp.json");
    let mut mcp: Value = serde_json::from_str(&fs::read_to_string(&mcp_path).unwrap()).unwrap();
    mcp["mcpServers"]["other"] = json!({"command": "other-tool", "args": []});
    fs::write(&mcp_path, serde_json::to_string_pretty(&mcp).unwrap()).unwrap();
    fs::write(
        root.join(".cursor/rules/checkweave.mdc"),
        "user owned rule\n",
    )
    .unwrap();
    fs::write(root.join("notes.txt"), "keep\n").unwrap();

    let preserved = run(&mut fixture, &["deinit"]);
    assert!(
        preserved["preserved"]
            .as_array()
            .unwrap()
            .iter()
            .any(|item| item.as_str().unwrap_or("").contains("checkweave.mdc")),
        "{preserved}"
    );
    assert_eq!(
        fs::read_to_string(root.join(".cursor/rules/checkweave.mdc")).unwrap(),
        "user owned rule\n"
    );
    assert_eq!(
        fs::read_to_string(root.join(".cursor/rules/other.mdc")).unwrap(),
        "leave me\n"
    );
    let after = fs::read_to_string(&mcp_path).unwrap();
    assert!(!after.contains("checkweave"), "{after}");
    assert!(after.contains("other-tool"), "{after}");
    assert!(root.join(".checkweave").exists());
    assert_eq!(
        fs::read_to_string(root.join("notes.txt")).unwrap(),
        "keep\n"
    );

    fs::write(
        root.join(".cursor/rules/checkweave.mdc"),
        "<!-- checkweave:managed -->\nmanaged\n",
    )
    .unwrap();
    run(&mut fixture, &["init", "--agent", "cursor"]);
    let removed = run(&mut fixture, &["uninit"]);
    let removed_items = removed["removed"].as_array().unwrap();
    assert!(
        removed_items
            .iter()
            .any(|item| item.as_str().unwrap_or("").contains("checkweave.mdc")),
        "{removed}"
    );
    assert!(!root.join(".cursor/rules/checkweave.mdc").exists());
    assert_eq!(
        fs::read_to_string(root.join(".cursor/rules/other.mdc")).unwrap(),
        "leave me\n"
    );
    assert!(root.join(".checkweave").exists());
}

#[test]
fn cancelling_one_shared_check_lets_the_other_finish() {
    let mut fixture = Fixture::new();
    let slow = fixture.path().join("slow");
    fs::create_dir(&slow).unwrap();
    for index in 0..250 {
        let mut body = String::new();
        for row in 0..30 {
            body.push_str(&format!("{{\"n\":{index},\"row\":{row}}}\n"));
        }
        fs::write(slow.join(format!("{index}.jsonl")), body).unwrap();
    }
    run(&mut fixture, &["init", "--agent", "none"]);
    let request = fixture.path().join("check.json");
    fs::write(
        &request,
        r#"{"operation":"check","include":["slow/*.jsonl"],"predicate":{"op":"exists","path":"/n"},"limits":{"max_files":1000,"max_bytes":32000000,"max_records":100000,"max_results":5,"timeout_ms":20000}}"#,
    )
    .unwrap();
    let path = request.to_str().unwrap();
    let first = run(&mut fixture, &["run", "start", "--request-file", path]);
    let first_id = first["id"].as_str().unwrap().to_string();
    let mut saw_running = false;
    let wait_started = Instant::now();
    while wait_started.elapsed() < Duration::from_secs(8) {
        let status_started = Instant::now();
        let status = run(&mut fixture, &["status"]);
        assert!(
            status_started.elapsed() < Duration::from_secs(2),
            "status blocked for {:?} during a check: {status}",
            status_started.elapsed()
        );
        assert!(status.get("pid").is_some(), "{status}");
        let running = status["runs"]
            .as_array()
            .unwrap()
            .iter()
            .any(|run| run["id"] == first_id && run["state"] == "running");
        if running || status["stats"]["busy"] == true {
            saw_running = true;
            break;
        }
        if status["runs"]
            .as_array()
            .unwrap()
            .iter()
            .any(|run| run["id"] == first_id && run["state"] == "complete")
        {
            break;
        }
        thread::sleep(Duration::from_millis(20));
    }
    assert!(
        saw_running,
        "check finished before a concurrent status could observe it"
    );
    let second = run(&mut fixture, &["run", "start", "--request-file", path]);
    let second_id = second["id"].as_str().unwrap().to_string();
    let cancelled = run(&mut fixture, &["run", "cancel", &first_id]);
    assert_eq!(cancelled["id"], first_id);
    assert_eq!(cancelled["state"], "cancelled");
    let finished = poll_run(&mut fixture, &second_id);
    assert_eq!(finished["state"], "complete", "{finished}");
    assert_eq!(finished["result"]["execution"], "complete", "{finished}");
    assert_eq!(finished["result"]["freshness"], "validated", "{finished}");
}

#[test]
fn compare_trace_and_semantic_predicate_round_trip() {
    let mut fixture = Fixture::new();
    let worker = fixture.path().join("mock_worker.py");
    fs::write(
        &worker,
        r#"#!/usr/bin/env python3
import json, sys
prov = {
    "provider": "local",
    "model": "fastino/gliner2.5-base-v1",
    "revision": "1a8bc24e00dc7300b9017c81d63e3dcdabb26596",
    "adapter_version": "checkweave-gliner2-1",
    "device": "cpu",
    "precision": "float32",
    "score_semantics": "predicate_unsupported",
    "input_policy": "mock",
    "runtime_versions": {},
}
sys.stdout.write(json.dumps({"version": 1, "ready": True, "provenance": prov}) + "\n")
sys.stdout.flush()
for line in sys.stdin:
    msg = json.loads(line)
    sys.stdout.write(json.dumps({"version": 1, "id": msg.get("id"), "results": [], "provenance": prov}) + "\n")
    sys.stdout.flush()
"#,
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = fs::metadata(&worker).unwrap().permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&worker, perms).unwrap();
    }
    let _python = EnvGuard::set("CHECKWEAVE_PYTHON", worker);
    fs::write(
        fixture.path().join("echo.py"),
        "import json,sys\njson.dump(json.load(sys.stdin), sys.stdout)\n",
    )
    .unwrap();
    fs::write(
        fixture.path().join("probe.py"),
        "def value(data):\n    return data.get('n', 0)\nx = value(__import__('json').load(__import__('sys').stdin))\nprint(x)\n",
    )
    .unwrap();
    write_records(fixture.path(), "{\"text\":\"hello\"}\n");
    run(&mut fixture, &["init", "--agent", "none"]);

    let compare_file = fixture.path().join("compare.json");
    fs::write(
        &compare_file,
        json!({
            "before": {"argv": ["python3", "echo.py"]},
            "after": {"argv": ["python3", "echo.py"]},
            "inputs": [{"n": 1}]
        })
        .to_string(),
    )
    .unwrap();
    let compared = run(
        &mut fixture,
        &["compare", "--request-file", compare_file.to_str().unwrap()],
    );
    assert!(
        compared.get("id").and_then(Value::as_str).is_some(),
        "{compared}"
    );
    assert_ne!(compared["execution"], "failed", "{compared}");
    let compare_id = compared["id"].as_str().unwrap().to_string();
    let replayed = run(
        &mut fixture,
        &["replay", "--kind", "compare", "--id", &compare_id],
    );
    assert!(replayed.get("id").is_some(), "{replayed}");

    let trace_file = fixture.path().join("trace.json");
    fs::write(
        &trace_file,
        json!({"script": "probe.py", "input": {"n": 3}}).to_string(),
    )
    .unwrap();
    let traced = run(
        &mut fixture,
        &["trace", "--request-file", trace_file.to_str().unwrap()],
    );
    assert_eq!(traced["basis"], "direct_observation", "{traced}");
    let trace_id = traced["id"].as_str().unwrap();
    let evidence = run(&mut fixture, &["evidence", trace_id]);
    assert_eq!(evidence["id"], trace_id);

    fs::write(
        fixture.path().join("checkweave.toml"),
        "[model]\nprovider = \"local\"\nprofile = \"lightweight\"\ndevice = \"cpu\"\n",
    )
    .unwrap();
    let semantic_file = fixture.path().join("semantic.json");
    fs::write(
        &semantic_file,
        json!({
            "globs": ["records.jsonl"],
            "text_pointer": "/text",
            "questions": [{"kind": "predicate", "id": "p", "statement": "the text is urgent"}]
        })
        .to_string(),
    )
    .unwrap();
    let judged = run(
        &mut fixture,
        &[
            "semantic",
            "--request-file",
            semantic_file.to_str().unwrap(),
        ],
    );
    assert_eq!(judged["basis"], "model_judgment", "{judged}");
    assert!(
        judged["coverage"]["unsupported"].as_u64().unwrap_or(0) >= 1
            || judged["decisions"]
                .as_array()
                .unwrap()
                .iter()
                .any(|item| item["status"] == "unsupported"),
        "{judged}"
    );
    let semantic_id = judged["id"].as_str().unwrap();
    let semantic_evidence = run(&mut fixture, &["evidence", semantic_id]);
    assert_eq!(semantic_evidence["id"], semantic_id);
    assert_eq!(semantic_evidence["basis"], "model_judgment");
}

#[test]
fn run_queue_rejects_when_eight_jobs_are_inflight() {
    let mut fixture = Fixture::new();
    fs::write(
        fixture.path().join("sleep.py"),
        "import time\ntime.sleep(30)\n",
    )
    .unwrap();
    run(&mut fixture, &["init", "--agent", "none"]);
    let request = fixture.path().join("compare.json");
    fs::write(
        &request,
        json!({
            "operation": "compare",
            "request": {
                "before": {"argv": ["python3", "sleep.py"]},
                "after": {"argv": ["python3", "sleep.py"]},
                "inputs": [{}]
            }
        })
        .to_string(),
    )
    .unwrap();
    let path = request.to_str().unwrap();
    let mut ids = Vec::new();
    for _ in 0..8 {
        let started = run(&mut fixture, &["run", "start", "--request-file", path]);
        ids.push(started["id"].as_str().unwrap().to_string());
    }
    let (status, _stdout, stderr) =
        capture(&mut fixture, &["run", "start", "--request-file", path]);
    assert!(!status.success(), "ninth run was accepted");
    assert!(
        stderr.contains("run queue is full"),
        "stderr did not report saturation: {stderr}"
    );
    for id in ids {
        let cancelled = run(&mut fixture, &["run", "cancel", &id]);
        assert_eq!(cancelled["state"], "cancelled");
    }
}

#[test]
fn detached_run_survives_idle_timeout() {
    let _idle = EnvGuard::set("CHECKWEAVE_IDLE_SECONDS_TEST", "1");
    let mut fixture = Fixture::new();
    fs::write(
        fixture.path().join("sleep.py"),
        "import time\ntime.sleep(2)\n",
    )
    .unwrap();
    run(&mut fixture, &["init", "--agent", "none"]);
    let request = fixture.path().join("compare.json");
    fs::write(
        &request,
        json!({
            "operation": "compare",
            "request": {
                "before": {"argv": ["python3", "sleep.py"]},
                "after": {"argv": ["python3", "sleep.py"]},
                "inputs": [{}]
            }
        })
        .to_string(),
    )
    .unwrap();
    let started = run(
        &mut fixture,
        &["run", "start", "--request-file", request.to_str().unwrap()],
    );
    let id = started["id"].as_str().unwrap().to_string();
    thread::sleep(Duration::from_millis(1500));
    let status = run(&mut fixture, &["status"]);
    assert!(
        status.get("pid").is_some(),
        "daemon exited during a detached run: {status}"
    );
    let finished = poll_run(&mut fixture, &id);
    assert!(
        matches!(
            finished["state"].as_str(),
            Some("complete") | Some("failed")
        ),
        "{finished}"
    );
    assert!(
        finished.get("result").is_some() || finished.get("error").is_some(),
        "{finished}"
    );
}

#[test]
fn provider_reloads_when_config_changes_from_local_to_hosted_mock() {
    let hits = Arc::new(AtomicUsize::new(0));
    let port = jev_mock(Arc::clone(&hits));
    // The daemon process inherits this from the client that starts it.
    unsafe { std::env::set_var("CHECKWEAVE_JEV_TEST_KEY", "test-token") };
    let mut fixture = Fixture::new();
    run(&mut fixture, &["init", "--agent", "none"]);
    fs::write(
        fixture.path().join("checkweave.toml"),
        "[model]\nprovider = \"local\"\nprofile = \"lightweight\"\ndevice = \"cpu\"\n",
    )
    .unwrap();
    let request = fixture.path().join("evaluate.json");
    fs::write(
        &request,
        json!({
            "states": [{"id": "s", "text": "hello"}],
            "questions": [{"kind": "predicate", "id": "holds", "statement": "the text is a greeting"}],
            "timeout_ms": 5000
        })
        .to_string(),
    )
    .unwrap();
    let path = request.to_str().unwrap();
    let local = run(&mut fixture, &["model", "evaluate", "--request-file", path]);
    assert_eq!(local["provenance"]["provider"], "local", "{local}");
    assert_eq!(hits.load(Ordering::SeqCst), 0);

    fs::write(
        fixture.path().join("checkweave.toml"),
        format!(
            "[model]\nprovider = \"jev\"\nmodel = \"jev-1.13.0\"\napi_key_env = \"CHECKWEAVE_JEV_TEST_KEY\"\nendpoint = \"http://127.0.0.1:{port}/v1\"\n"
        ),
    )
    .unwrap();
    let hosted = run(&mut fixture, &["model", "evaluate", "--request-file", path]);
    assert_eq!(hosted["provenance"]["provider"], "jev", "{hosted}");
    assert_eq!(hosted["provenance"]["model"], "jev-1.13.0");
    assert_eq!(hits.load(Ordering::SeqCst), 1);
}

fn jev_mock(hits: Arc<AtomicUsize>) -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind jev mock");
    let port = listener.local_addr().expect("mock port").port();
    thread::spawn(move || {
        for _ in 0..2 {
            let Ok((mut sock, _)) = listener.accept() else {
                break;
            };
            let _ = sock.set_read_timeout(Some(Duration::from_secs(10)));
            let mut buf = Vec::new();
            let mut tmp = [0u8; 1024];
            loop {
                match sock.read(&mut tmp) {
                    Ok(0) => break,
                    Ok(n) => {
                        buf.extend_from_slice(&tmp[..n]);
                        if buf.windows(4).any(|window| window == b"\r\n\r\n") {
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }
            let body = r#"{"model":"jev-1.13.0","answers":{"holds":{"type":"noul","noul":0.8}},"usage":{"input_tokens":1,"output_tokens":1}}"#;
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = sock.write_all(resp.as_bytes());
            hits.fetch_add(1, Ordering::SeqCst);
        }
    });
    port
}
