//! CLI and MCP interface tests. Every subprocess has a timeout, and every
//! spawned process group is killed on drop so a workspace daemon cannot linger.

use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use checkweave::types::{CheckRequest, Limits, Predicate};
use serde_json::{Value, json};

const FAST: Duration = Duration::from_secs(15);
const SLOW: Duration = Duration::from_secs(60);

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

struct Captured {
    status: ExitStatus,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
}

struct Fixture {
    dir: tempfile::TempDir,
    groups: Mutex<Vec<u32>>,
}

impl Fixture {
    fn new() -> Self {
        Self {
            dir: tempfile::TempDir::new().expect("temp workspace"),
            groups: Mutex::new(Vec::new()),
        }
    }

    fn path(&self) -> &Path {
        self.dir.path()
    }

    fn track(&self, pid: u32) {
        self.groups.lock().expect("pid list").push(pid);
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let identity = self.dir.path().join(".checkweave").join("daemon.json");
        let daemon_pid = std::fs::read_to_string(&identity)
            .ok()
            .and_then(|text| serde_json::from_str::<Value>(&text).ok())
            .and_then(|value| value.get("pid").and_then(Value::as_u64))
            .map(|pid| pid as u32);
        if daemon_pid.is_some()
            && let Ok(mut child) = command(Some(self.dir.path()), &["shutdown"])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .stdin(Stdio::null())
                .spawn()
        {
            let pid = child.id();
            let start = Instant::now();
            loop {
                if child.try_wait().ok().flatten().is_some() {
                    break;
                }
                if start.elapsed() > Duration::from_secs(5) {
                    kill_group(pid);
                    let _ = child.kill();
                    let _ = child.wait();
                    break;
                }
                thread::sleep(Duration::from_millis(30));
            }
        }
        if let Some(pid) = daemon_pid {
            kill_group(pid);
        }
        for pid in self.groups.lock().expect("pid list").iter().copied() {
            kill_group(pid);
        }
    }
}

fn command(workspace: Option<&Path>, args: &[&str]) -> Command {
    let mut cmd = Command::new(bin());
    cmd.env("RUST_LOG", "off")
        .env("NO_COLOR", "1")
        .env("CHECKWEAVE_IDLE_SECONDS", "30")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    if let Some(workspace) = workspace {
        cmd.arg("--workspace").arg(workspace);
        cmd.current_dir(workspace);
    }
    cmd.args(args);
    cmd
}

fn capture(workspace: Option<&Path>, args: &[&str], timeout: Duration) -> Captured {
    let mut child = command(workspace, args)
        .spawn()
        .unwrap_or_else(|err| panic!("spawn {args:?}: {err}"));
    let pid = child.id();
    let stdout = child.stdout.take().expect("stdout");
    let stderr = child.stderr.take().expect("stderr");
    let out_thread = thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = std::io::Read::read_to_end(&mut BufReader::new(stdout), &mut buf);
        buf
    });
    let err_thread = thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = std::io::Read::read_to_end(&mut BufReader::new(stderr), &mut buf);
        buf
    });
    let status = wait_child(&mut child, pid, timeout, &format!("{args:?}"));
    Captured {
        status,
        stdout: out_thread.join().expect("stdout thread"),
        stderr: err_thread.join().expect("stderr thread"),
    }
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

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

fn parse_json(bytes: &[u8], context: &str) -> Value {
    let raw = text(bytes);
    serde_json::from_str(raw.trim())
        .unwrap_or_else(|err| panic!("{context}: stdout is not one JSON document: {err}\n{raw}"))
}

fn expect_success(captured: &Captured, context: &str) -> Value {
    assert!(
        captured.status.success(),
        "{context} exited {:?}\nstdout:\n{}\nstderr:\n{}",
        captured.status.code(),
        text(&captured.stdout),
        text(&captured.stderr)
    );
    let value = parse_json(&captured.stdout, context);
    let stderr = text(&captured.stderr);
    assert!(
        !stderr.contains("panic") && !stderr.contains("RUST_BACKTRACE"),
        "{context} stderr looks like a crash:\n{stderr}"
    );
    value
}

fn expect_usage(captured: &Captured, context: &str) {
    assert_eq!(
        captured.status.code(),
        Some(2),
        "{context} should be a usage error, got {:?}\nstdout:\n{}\nstderr:\n{}",
        captured.status.code(),
        text(&captured.stdout),
        text(&captured.stderr)
    );
    assert!(
        text(&captured.stdout).trim().is_empty(),
        "{context} wrote to stdout:\n{}",
        text(&captured.stdout)
    );
    assert!(
        !text(&captured.stderr).trim().is_empty(),
        "{context} should explain the problem on stderr"
    );
}

fn predicate_open() -> Predicate {
    Predicate::Eq {
        path: "/status".into(),
        value: json!("open"),
    }
}

fn predicate_json() -> String {
    serde_json::to_string(&predicate_open()).expect("predicate")
}

fn write_collection(dir: &Path) {
    std::fs::write(
        dir.join("items.jsonl"),
        concat!(
            "{\"status\":\"open\",\"n\":1}\n",
            "{\"status\":\"closed\",\"n\":2}\n",
            "{\"status\":\"open\",\"n\":3}\n",
            "{not-json}\n",
        ),
    )
    .expect("write collection");
}

fn stable(value: &Value) -> Value {
    match value {
        Value::Object(map) => {
            let mut out = serde_json::Map::new();
            for (key, child) in map {
                if matches!(
                    key.as_str(),
                    "elapsed_ms"
                        | "cache_hits"
                        | "cache_misses"
                        | "generation"
                        | "timestamp"
                        | "created_at"
                        | "updated_at"
                ) {
                    continue;
                }
                out.insert(key.clone(), stable(child));
            }
            Value::Object(out)
        }
        Value::Array(items) => Value::Array(items.iter().map(stable).collect()),
        other => other.clone(),
    }
}

fn stable_report(value: &Value) -> Value {
    let mut value = stable(value);
    if let Some(object) = value.as_object_mut() {
        object.remove("id");
        if let Some(items) = object.get_mut("items").and_then(Value::as_array_mut) {
            items.sort_by_key(item_key);
        }
    }
    value
}

fn item_key(value: &Value) -> (String, u64, String) {
    (
        value["source"]["path"].as_str().unwrap_or("").to_string(),
        value["source"]["line"].as_u64().unwrap_or(0),
        value["matched"].to_string(),
    )
}

#[test]
fn help_lists_operations_and_hides_the_worker() {
    let help = capture(None, &["--help"], FAST);
    assert!(help.status.success(), "{}", text(&help.stderr));
    let stdout = text(&help.stdout);
    for word in [
        "init",
        "check",
        "evidence",
        "status",
        "shutdown",
        "mcp",
        "--workspace",
    ] {
        assert!(stdout.contains(word), "help missing {word}:\n{stdout}");
    }
    assert!(
        !stdout
            .lines()
            .any(|line| line.trim_start().starts_with("daemon")),
        "hidden daemon command leaked into help:\n{stdout}"
    );
    assert!(
        text(&help.stderr).trim().is_empty(),
        "{}",
        text(&help.stderr)
    );

    let check_help = capture(None, &["check", "--help"], FAST);
    assert!(check_help.status.success(), "{}", text(&check_help.stderr));
    let stdout = text(&check_help.stdout);
    for flag in [
        "--include",
        "--predicate",
        "--predicate-file",
        "--max-files",
        "--max-bytes",
        "--max-records",
        "--max-results",
        "--timeout-ms",
    ] {
        assert!(
            stdout.contains(flag),
            "check help missing {flag}:\n{stdout}"
        );
    }
    let limits = Limits::default();
    for number in [
        limits.max_files.to_string(),
        limits.max_bytes.to_string(),
        limits.max_records.to_string(),
        limits.max_results.to_string(),
        limits.timeout_ms.to_string(),
    ] {
        assert!(
            stdout.contains(&number),
            "check help should show shared default {number}:\n{stdout}"
        );
    }

    let daemon_help = capture(None, &["daemon", "--help"], FAST);
    assert!(
        daemon_help.status.success(),
        "{}",
        text(&daemon_help.stderr)
    );
    assert!(text(&daemon_help.stdout).contains("300"));

    let missing = capture(None, &[], FAST);
    assert_eq!(missing.status.code(), Some(2));
    let combined = format!("{}{}", text(&missing.stdout), text(&missing.stderr));
    assert!(combined.contains("checkweave") || combined.contains("init"));
}

#[test]
fn invalid_invocations_exit_nonzero_with_clean_stdout() {
    let dir = tempfile::TempDir::new().unwrap();
    let workspace = dir.path();
    let predicate = predicate_json();
    let missing_predicate = dir.path().join("missing-predicate.json");
    let missing_predicate = missing_predicate.to_str().expect("utf8 path");
    let usage = |args: &[&str], label: &str| {
        let captured = capture(Some(workspace), args, FAST);
        expect_usage(&captured, label);
    };
    usage(&["check"], "check without arguments");
    usage(&["check", "--include", "items.jsonl"], "missing predicate");
    usage(&["check", "--predicate", &predicate], "missing include");
    usage(
        &[
            "check",
            "--include",
            "items.jsonl",
            "--predicate",
            &predicate,
            "--predicate-file",
            "predicate.json",
        ],
        "both predicate sources",
    );
    usage(
        &["check", "--include", "items.jsonl", "--predicate", "{"],
        "broken predicate json",
    );
    usage(
        &[
            "check",
            "--include",
            "items.jsonl",
            "--predicate",
            r#"{"op":"nope"}"#,
        ],
        "unknown predicate",
    );
    usage(
        &[
            "check",
            "--include",
            "items.jsonl",
            "--predicate",
            r#"{"op":"eq","path":"/status","value":"open","extra":true}"#,
        ],
        "unknown predicate field",
    );
    usage(
        &[
            "check",
            "--include",
            "/tmp/items.jsonl",
            "--predicate",
            &predicate,
        ],
        "absolute include",
    );
    usage(
        &[
            "check",
            "--include",
            "../items.jsonl",
            "--predicate",
            &predicate,
        ],
        "parent include",
    );
    usage(
        &["check", "--include", "", "--predicate", &predicate],
        "empty include",
    );
    usage(
        &[
            "check",
            "--include",
            "items.jsonl",
            "--predicate",
            &predicate,
            "--max-files",
            "0",
        ],
        "zero max-files",
    );
    usage(
        &[
            "check",
            "--include",
            "items.jsonl",
            "--predicate",
            &predicate,
            "--max-bytes",
            "0",
        ],
        "zero max-bytes",
    );
    usage(
        &[
            "check",
            "--include",
            "items.jsonl",
            "--predicate",
            &predicate,
            "--timeout-ms",
            "0",
        ],
        "zero timeout",
    );
    usage(&["init", "--agent", "other"], "unknown agent");
    usage(&["evidence"], "missing evidence id");
    usage(&["evidence", ""], "empty evidence id");
    usage(&["evidence", "../secret"], "path-like evidence id");
    usage(&["no-such-command"], "unknown command");
    usage(
        &[
            "check",
            "--include",
            "items.jsonl",
            "--predicate-file",
            missing_predicate,
        ],
        "missing predicate file",
    );

    let file_workspace = dir.path().join("not-a-directory");
    std::fs::write(&file_workspace, "x").unwrap();
    let captured = capture(
        None,
        &["--workspace", file_workspace.to_str().unwrap(), "status"],
        FAST,
    );
    expect_usage(&captured, "workspace path is a file");

    let captured = capture(
        None,
        &["--workspace", "/no/such/checkweave-workspace", "status"],
        FAST,
    );
    expect_usage(&captured, "missing workspace");
}

#[test]
fn init_check_and_evidence_match_across_cli_and_mcp() {
    let fix = Fixture::new();
    write_collection(fix.path());
    let predicate = predicate_json();
    let predicate_path = fix.path().join("predicate.json");
    std::fs::write(&predicate_path, &predicate).unwrap();

    expect_success(&capture(Some(fix.path()), &["init"], SLOW), "init");
    expect_success(
        &capture(Some(fix.path()), &["init", "--agent", "none"], SLOW),
        "init again",
    );

    let status = expect_success(&capture(Some(fix.path()), &["status"], SLOW), "status");
    assert!(
        status.is_object(),
        "status should be a JSON object: {status}"
    );

    let inline = expect_success(
        &capture(
            Some(fix.path()),
            &[
                "check",
                "--include",
                "items.jsonl",
                "--predicate",
                &predicate,
            ],
            SLOW,
        ),
        "check predicate",
    );
    assert_check_report(&inline);

    let from_file = expect_success(
        &capture(
            Some(fix.path()),
            &[
                "check",
                "--include",
                "items.jsonl",
                "--predicate-file",
                predicate_path.to_str().unwrap(),
            ],
            SLOW,
        ),
        "check predicate file",
    );
    assert_eq!(
        stable_report(&inline),
        stable_report(&from_file),
        "predicate file diverged from inline predicate\ninline={inline}\nfile={from_file}"
    );

    let bounded = expect_success(
        &capture(
            Some(fix.path()),
            &[
                "check",
                "--include",
                "items.jsonl",
                "--predicate",
                &predicate,
                "--max-results",
                "1",
            ],
            SLOW,
        ),
        "bounded check",
    );
    let bounded_items = bounded["items"].as_array().expect("items");
    assert!(
        bounded_items.len() <= 1,
        "max-results was not applied: {bounded}"
    );
    assert!(
        bounded["coverage"]["matched"].as_u64().unwrap_or(0) <= 1
            || bounded["truncated"] == true
            || bounded["execution"] == "partial",
        "a bounded check must say it is partial or truncated: {bounded}"
    );

    let id = inline["id"]
        .as_str()
        .expect("check evidence id")
        .to_string();
    let evidence = expect_success(
        &capture(Some(fix.path()), &["evidence", &id], SLOW),
        "evidence",
    );

    let mut mcp = McpSession::spawn(&fix);
    let initialized = mcp.request(
        1,
        "initialize",
        json!({
            "protocolVersion": "2025-11-25",
            "capabilities": {},
            "clientInfo": {"name": "checkweave-interface-test", "version": "0.0.0"}
        }),
    );
    assert!(initialized.get("error").is_none(), "{initialized}");
    let instructions = initialized["result"]["instructions"]
        .as_str()
        .expect("server instructions")
        .to_ascii_lowercase();
    for phrase in [
        "source pointer",
        "fingerprint",
        "not model accuracy",
        "predicate",
        "recorded snapshot",
        "not an execution sandbox",
    ] {
        assert!(
            instructions.contains(phrase),
            "instructions missing {phrase}: {instructions}"
        );
    }
    assert_eq!(initialized["result"]["serverInfo"]["name"], "checkweave");
    assert!(initialized["result"]["capabilities"]["tools"].is_object());
    mcp.notify(json!({"jsonrpc": "2.0", "method": "notifications/initialized"}));

    let listed = mcp.request(2, "tools/list", json!({}));
    let tools = listed["result"]["tools"].as_array().expect("tools");
    assert_eq!(tools.len(), 13, "{listed}");
    for name in [
        "checkweave_check",
        "checkweave_evidence",
        "checkweave_trace_page",
        "checkweave_status",
        "checkweave_run_status",
    ] {
        let tool = tools
            .iter()
            .find(|tool| tool["name"] == name)
            .unwrap_or_else(|| panic!("missing {name}: {listed}"));
        assert_eq!(tool["annotations"]["readOnlyHint"], true, "{tool}");
        assert_eq!(tool["annotations"]["destructiveHint"], false, "{tool}");
        assert_eq!(tool["annotations"]["openWorldHint"], false, "{tool}");
        assert_eq!(tool["inputSchema"]["type"], "object", "{tool}");
    }
    for name in [
        "checkweave_compare",
        "checkweave_replay",
        "checkweave_trace",
        "checkweave_model_evaluate",
        "checkweave_semantic",
        "checkweave_run_start",
    ] {
        let tool = tools
            .iter()
            .find(|tool| tool["name"] == name)
            .unwrap_or_else(|| panic!("missing {name}: {listed}"));
        assert_eq!(tool["annotations"]["readOnlyHint"], false, "{tool}");
        assert_eq!(tool["annotations"]["idempotentHint"], false, "{tool}");
    }
    let check_tool = tools
        .iter()
        .find(|tool| tool["name"] == "checkweave_check")
        .unwrap();
    let properties = &check_tool["inputSchema"]["properties"];
    assert!(properties.get("include").is_some(), "{check_tool}");
    assert!(properties.get("predicate").is_some(), "{check_tool}");
    assert!(properties.get("limits").is_some(), "{check_tool}");
    let required = check_tool["inputSchema"]["required"]
        .as_array()
        .expect("required");
    assert!(required.iter().any(|item| item == "include"));
    assert!(required.iter().any(|item| item == "predicate"));

    let request = CheckRequest {
        include: vec!["items.jsonl".into()],
        predicate: predicate_open(),
        limits: Limits::default(),
    };
    let mcp_check_response = mcp.request(
        3,
        "tools/call",
        json!({
            "name": "checkweave_check",
            "arguments": serde_json::to_value(&request).unwrap()
        }),
    );
    let mcp_check = tool_success(&mcp_check_response, "mcp check");
    assert_eq!(
        stable_report(&inline),
        stable_report(mcp_check),
        "CLI and MCP checks diverged\ncli={inline}\nmcp={mcp_check}"
    );

    let mcp_evidence_response = mcp.request(
        4,
        "tools/call",
        json!({"name": "checkweave_evidence", "arguments": {"id": id}}),
    );
    let mcp_evidence = tool_success(&mcp_evidence_response, "mcp evidence");
    assert_eq!(
        stable(&evidence),
        stable(mcp_evidence),
        "CLI and MCP evidence diverged\ncli={evidence}\nmcp={mcp_evidence}"
    );

    let mcp_status_response = mcp.request(
        5,
        "tools/call",
        json!({"name": "checkweave_status", "arguments": {}}),
    );
    let mcp_status = tool_success(&mcp_status_response, "mcp status");
    assert!(mcp_status.is_object(), "{mcp_status}");

    let bad_evidence = mcp.request(
        6,
        "tools/call",
        json!({"name": "checkweave_evidence", "arguments": {"id": ""}}),
    );
    assert_tool_error(&bad_evidence);
    let unknown_field = mcp.request(
        7,
        "tools/call",
        json!({"name": "checkweave_status", "arguments": {"extra": true}}),
    );
    assert_tool_error(&unknown_field);
    let unknown_tool = mcp.request(
        8,
        "tools/call",
        json!({"name": "checkweave_missing", "arguments": {}}),
    );
    assert!(
        unknown_tool.get("error").is_some(),
        "unknown tool should be a protocol error: {unknown_tool}"
    );

    let stdout_lines = mcp.stdout_lines.lock().expect("lines").clone();
    assert!(!stdout_lines.is_empty());
    for line in stdout_lines {
        if line.is_empty() {
            continue;
        }
        let parsed: Value = serde_json::from_str(&line)
            .unwrap_or_else(|err| panic!("stdout is not JSON-RPC: {err}\n{line}"));
        assert_eq!(parsed["jsonrpc"], "2.0", "{line}");
    }
    drop(mcp);

    let shutdown = expect_success(&capture(Some(fix.path()), &["shutdown"], SLOW), "shutdown");
    let _ = shutdown;
}

#[test]
fn mcp_stdin_close_exits_cleanly() {
    let fix = Fixture::new();
    expect_success(
        &capture(Some(fix.path()), &["init", "--agent", "cursor"], SLOW),
        "init",
    );
    let mut session = McpSession::spawn(&fix);
    session.close_stdin();
    let status = session.wait(FAST);
    assert!(
        status.success(),
        "closing stdin should end the server cleanly, got {:?}\nstderr:\n{}",
        status.code(),
        session.stderr_text()
    );
    assert!(
        session.stdout_text().trim().is_empty(),
        "stdout before initialize must stay empty:\n{}",
        session.stdout_text()
    );
}

#[test]
fn daemon_ctrl_c_exits() {
    let fix = Fixture::new();
    expect_success(&capture(Some(fix.path()), &["init"], SLOW), "init");
    let mut child = command(Some(fix.path()), &["daemon", "--idle-seconds", "300"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn daemon");
    let pid = child.id();
    fix.track(pid);
    let stdout = child.stdout.take().expect("stdout");
    let stderr = child.stderr.take().expect("stderr");
    let _out = thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = std::io::Read::read_to_end(&mut BufReader::new(stdout), &mut buf);
    });
    let _err = thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = std::io::Read::read_to_end(&mut BufReader::new(stderr), &mut buf);
    });
    thread::sleep(Duration::from_millis(200));
    if child.try_wait().ok().flatten().is_none() {
        let _ = Command::new("kill")
            .args(["-INT", "--", &format!("-{pid}")])
            .status();
    }
    let status = wait_child(&mut child, pid, Duration::from_secs(10), "daemon ctrl-c");
    let code = status.code();
    assert!(
        code == Some(0) || code == Some(130),
        "Ctrl-C should shut the daemon down, got {code:?}"
    );
}

fn assert_check_report(report: &Value) {
    let execution = report["execution"].as_str().expect("execution");
    assert!(
        matches!(execution, "complete" | "partial"),
        "unexpected execution: {report}"
    );
    assert!(report["freshness"].is_string(), "{report}");
    assert!(report["coverage"].is_object(), "{report}");
    let items = report["items"].as_array().expect("items");
    let matched = report["coverage"]["matched"].as_u64().expect("matched");
    assert!(
        matched >= 1,
        "matching records must not become an error: {report}"
    );
    let unresolved = report["coverage"]["unresolved"].as_u64().unwrap_or(0);
    let null_matches = items
        .iter()
        .filter(|item| item["matched"].is_null())
        .count();
    assert!(
        unresolved >= 1 || null_matches >= 1,
        "invalid JSON should stay unresolved: {report}"
    );
    assert!(
        items.iter().any(|item| item["source"]["path"].is_string()),
        "items need source pointers: {report}"
    );
}

fn tool_success<'a>(response: &'a Value, context: &str) -> &'a Value {
    assert!(
        response.get("error").is_none(),
        "{context} protocol error: {response}"
    );
    let result = &response["result"];
    assert_ne!(result["isError"], true, "{context} tool error: {result}");
    assert_eq!(result["content"][0]["type"], "text", "{result}");
    let text = result["content"][0]["text"]
        .as_str()
        .expect("text fallback");
    assert!(!text.is_empty(), "{context} missing text fallback");
    assert!(
        text.len() <= 420,
        "{context} text fallback is not small: {text}"
    );
    result
        .get("structuredContent")
        .unwrap_or_else(|| panic!("{context} missing structured content: {result}"))
}

fn assert_tool_error(response: &Value) {
    if let Some(error) = response.get("error") {
        let message = error["message"].as_str().unwrap_or("");
        assert!(!message.is_empty(), "{response}");
        return;
    }
    let result = &response["result"];
    assert_eq!(result["isError"], true, "{response}");
    let text = result["content"][0]["text"].as_str().unwrap_or("");
    assert!(!text.is_empty(), "tool error needs text: {result}");
    if result["structuredContent"]["error"].is_string() {
        assert!(
            text.starts_with("checkweave:"),
            "tool error needs an actionable text fallback: {result}"
        );
    }
}

struct McpSession {
    child: Option<Child>,
    stdin: Option<std::process::ChildStdin>,
    lines: Receiver<String>,
    stdout_lines: Arc<Mutex<Vec<String>>>,
    stderr_text: Arc<Mutex<String>>,
    stdout_thread: Option<JoinHandle<()>>,
    stderr_thread: Option<JoinHandle<()>>,
    pid: u32,
}

impl McpSession {
    fn spawn(fix: &Fixture) -> Self {
        let mut child = command(Some(fix.path()), &["mcp"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn mcp");
        let pid = child.id();
        fix.track(pid);
        let stdin = child.stdin.take().expect("stdin");
        let stdout = child.stdout.take().expect("stdout");
        let stderr = child.stderr.take().expect("stderr");
        let (tx, rx) = mpsc::channel();
        let stdout_lines = Arc::new(Mutex::new(Vec::new()));
        let recorded = Arc::clone(&stdout_lines);
        let stdout_thread = thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else { break };
                recorded.lock().expect("stdout").push(line.clone());
                if tx.send(line).is_err() {
                    break;
                }
            }
        });
        let stderr_text = Arc::new(Mutex::new(String::new()));
        let recorded_err = Arc::clone(&stderr_text);
        let stderr_thread = thread::spawn(move || {
            for line in BufReader::new(stderr).lines() {
                let Ok(line) = line else { break };
                let mut buffer = recorded_err.lock().expect("stderr");
                buffer.push_str(&line);
                buffer.push('\n');
            }
        });
        Self {
            child: Some(child),
            stdin: Some(stdin),
            lines: rx,
            stdout_lines,
            stderr_text,
            stdout_thread: Some(stdout_thread),
            stderr_thread: Some(stderr_thread),
            pid,
        }
    }

    fn send(&mut self, value: &Value) {
        let mut stdin = self.stdin.as_mut().expect("stdin open");
        serde_json::to_writer(&mut stdin, value).expect("write json");
        stdin.write_all(b"\n").expect("write newline");
        stdin.flush().expect("flush");
    }

    fn notify(&mut self, value: Value) {
        self.send(&value);
    }

    fn request(&mut self, id: u64, method: &str, params: Value) -> Value {
        self.send(&json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params
        }));
        let deadline = Instant::now() + SLOW;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                panic!(
                    "timed out waiting for MCP id {id}\nstderr:\n{}",
                    self.stderr_text()
                );
            }
            let line = self.lines.recv_timeout(remaining).unwrap_or_else(|_| {
                panic!(
                    "MCP stdout closed while waiting for id {id}\nstderr:\n{}",
                    self.stderr_text()
                )
            });
            if line.is_empty() {
                continue;
            }
            let value: Value = serde_json::from_str(&line)
                .unwrap_or_else(|err| panic!("MCP line is not JSON: {err}\n{line}"));
            assert_eq!(value["jsonrpc"], "2.0", "{line}");
            if value.get("id").and_then(Value::as_u64) == Some(id) {
                return value;
            }
            panic!("unexpected MCP message while waiting for {id}: {value}");
        }
    }

    fn close_stdin(&mut self) {
        self.stdin.take();
    }

    fn wait(&mut self, timeout: Duration) -> ExitStatus {
        let pid = self.pid;
        let child = self.child.as_mut().expect("child");
        wait_child(child, pid, timeout, "mcp")
    }

    fn stdout_text(&self) -> String {
        self.stdout_lines.lock().expect("stdout").join("\n")
    }

    fn stderr_text(&self) -> String {
        self.stderr_text.lock().expect("stderr").clone()
    }
}

impl Drop for McpSession {
    fn drop(&mut self) {
        self.stdin.take();
        if let Some(child) = self.child.as_mut() {
            kill_group(self.pid);
            let _ = child.kill();
            let _ = child.wait();
        }
        if let Some(thread) = self.stdout_thread.take() {
            let _ = thread.join();
        }
        if let Some(thread) = self.stderr_thread.take() {
            let _ = thread.join();
        }
    }
}
