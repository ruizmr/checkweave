use std::fs::{self, OpenOptions};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use checkweave::daemon;
use checkweave::types::{IMPLEMENTATION_VERSION, PROTOCOL_VERSION, Request, WireRequest};
use checkweave::workspace::Workspace;
use fs2::FileExt;

fn scratch() -> PathBuf {
    static N: AtomicU64 = AtomicU64::new(0);
    let n = N.fetch_add(1, Ordering::Relaxed);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let path = std::env::temp_dir().join(format!("checkweave-daemon-{nanos}-{n}"));
    fs::create_dir_all(&path).unwrap();
    path
}

struct TempTree(PathBuf);

impl TempTree {
    fn new() -> Self {
        Self(scratch())
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempTree {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn workspace() -> (TempTree, Workspace) {
    let dir = TempTree::new();
    Workspace::initialize(dir.path(), "none").unwrap();
    let ws = Workspace::discover(dir.path()).unwrap();
    (dir, ws)
}

fn socket_of(ws: &Workspace) -> PathBuf {
    ws.state_dir.join("daemon.sock")
}

async fn wait_until(mut ready: impl FnMut() -> bool, limit: Duration) -> bool {
    let start = Instant::now();
    while start.elapsed() < limit {
        if ready() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    ready()
}

async fn start_server(
    ws: Workspace,
    idle_seconds: u64,
) -> tokio::task::JoinHandle<anyhow::Result<()>> {
    tokio::spawn(async move { daemon::serve(ws, idle_seconds).await })
}

fn bin() -> Option<&'static str> {
    let path = option_env!("CARGO_BIN_EXE_checkweave")?;
    Path::new(path).is_file().then_some(path)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn status_reports_identity_and_reconnection() {
    let (_dir, ws) = workspace();
    let server = start_server(ws.clone(), 30).await;
    let first = tokio::time::timeout(
        Duration::from_secs(10),
        daemon::request(&ws, Request::Status),
    )
    .await
    .expect("status timed out")
    .expect("status failed");
    assert_eq!(first["pid"], std::process::id());
    assert_eq!(first["protocol"], PROTOCOL_VERSION);
    assert_eq!(first["workspace"], ws.root.to_str().unwrap());
    let instance = first["instance"].as_str().unwrap().to_string();
    assert!(!instance.is_empty());
    let watcher = first["watcher"].as_str().unwrap();
    assert!(watcher == "native" || watcher == "poll", "{watcher}");
    assert!(first.get("stats").is_some());

    let second = daemon::request(&ws, Request::Status).await.unwrap();
    assert_eq!(second["instance"], instance);
    assert_eq!(second["pid"], first["pid"]);

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = fs::metadata(socket_of(&ws)).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }
    drop(server);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn malformed_and_disconnected_clients_do_not_poison_worker() {
    let (_dir, ws) = workspace();
    let _server = start_server(ws.clone(), 30).await;
    assert!(
        wait_until(|| socket_of(&ws).exists(), Duration::from_secs(10)).await,
        "socket did not appear"
    );

    let mut partial = tokio::net::UnixStream::connect(socket_of(&ws))
        .await
        .unwrap();
    use tokio::io::AsyncWriteExt;
    partial.write_all(b"{\"version\":1").await.unwrap();
    drop(partial);

    let mut broken = tokio::net::UnixStream::connect(socket_of(&ws))
        .await
        .unwrap();
    broken.write_all(b"{not-json\n").await.unwrap();
    let (reader, _writer) = broken.into_split();
    let mut reader = tokio::io::BufReader::new(reader);
    let mut line = String::new();
    tokio::time::timeout(
        Duration::from_secs(5),
        tokio::io::AsyncBufReadExt::read_line(&mut reader, &mut line),
    )
    .await
    .expect("malformed response timed out")
    .unwrap();
    assert!(line.contains("malformed"), "{line}");

    let status = daemon::request(&ws, Request::Status).await.unwrap();
    assert_eq!(status["protocol"], PROTOCOL_VERSION);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn protocol_version_mismatch_is_rejected_and_worker_stays_up() {
    let (_dir, ws) = workspace();
    let _server = start_server(ws.clone(), 30).await;
    assert!(wait_until(|| socket_of(&ws).exists(), Duration::from_secs(10)).await);
    let mut stream = tokio::net::UnixStream::connect(socket_of(&ws))
        .await
        .unwrap();
    let wire = serde_json::json!({
        "version": PROTOCOL_VERSION + 9,
        "request": {"operation": "status"}
    });
    use tokio::io::AsyncWriteExt;
    let mut bytes = serde_json::to_vec(&wire).unwrap();
    bytes.push(b'\n');
    stream.write_all(&bytes).await.unwrap();
    let (reader, _writer) = stream.into_split();
    let mut reader = tokio::io::BufReader::new(reader);
    let mut line = String::new();
    tokio::io::AsyncBufReadExt::read_line(&mut reader, &mut line)
        .await
        .unwrap();
    assert!(line.contains("protocol version mismatch"), "{line}");

    let status = daemon::request(&ws, Request::Status).await.unwrap();
    assert_eq!(status["protocol"], PROTOCOL_VERSION);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn client_refuses_wrong_protocol_and_workspace() {
    let (_dir, ws) = workspace();
    let lock_path = ws.state_dir.join("daemon.lock");
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&lock_path)
        .unwrap();
    lock.try_lock_exclusive().unwrap();
    let endpoint = socket_of(&ws);
    let identity = serde_json::json!({
        "pid": std::process::id(),
        "instance": "foreign",
        "protocol": PROTOCOL_VERSION + 3,
        "workspace": ws.root,
        "endpoint": endpoint,
    });
    fs::write(
        ws.state_dir.join("daemon.json"),
        serde_json::to_vec(&identity).unwrap(),
    )
    .unwrap();
    let error = tokio::time::timeout(
        Duration::from_secs(3),
        daemon::request(&ws, Request::Status),
    )
    .await
    .expect("version refusal hung")
    .unwrap_err();
    assert!(
        format!("{error:#}").contains("protocol version mismatch"),
        "{error:#}"
    );

    let mismatch = serde_json::json!({
        "pid": std::process::id(),
        "instance": "foreign",
        "protocol": PROTOCOL_VERSION,
        "implementation": IMPLEMENTATION_VERSION,
        "workspace": "/not/this/workspace",
        "endpoint": "/not/this/workspace/daemon.sock",
    });
    fs::write(
        ws.state_dir.join("daemon.json"),
        serde_json::to_vec(&mismatch).unwrap(),
    )
    .unwrap();
    let error = tokio::time::timeout(
        Duration::from_secs(3),
        daemon::request(&ws, Request::Status),
    )
    .await
    .expect("workspace refusal hung")
    .unwrap_err();
    assert!(
        format!("{error:#}").contains("refusing daemon"),
        "{error:#}"
    );
    drop(lock);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn slow_client_does_not_block_status() {
    let (_dir, ws) = workspace();
    let _server = start_server(ws.clone(), 30).await;
    assert!(wait_until(|| socket_of(&ws).exists(), Duration::from_secs(10)).await);
    let _slow = tokio::net::UnixStream::connect(socket_of(&ws))
        .await
        .unwrap();
    let status = tokio::time::timeout(
        Duration::from_secs(3),
        daemon::request(&ws, Request::Status),
    )
    .await
    .expect("slow client blocked status")
    .unwrap();
    assert_eq!(status["pid"], std::process::id());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn oversized_frame_does_not_kill_worker() {
    let (_dir, ws) = workspace();
    let _server = start_server(ws.clone(), 30).await;
    assert!(wait_until(|| socket_of(&ws).exists(), Duration::from_secs(10)).await);
    let mut stream = tokio::net::UnixStream::connect(socket_of(&ws))
        .await
        .unwrap();
    let payload = vec![b'A'; 8 * 1024 * 1024 + 64];
    let send = async {
        use tokio::io::AsyncWriteExt;
        let _ = stream.write_all(&payload).await;
    };
    let _ = tokio::time::timeout(Duration::from_secs(5), send).await;
    drop(stream);
    let status = tokio::time::timeout(
        Duration::from_secs(5),
        daemon::request(&ws, Request::Status),
    )
    .await
    .expect("worker died after oversized frame")
    .unwrap();
    assert_eq!(status["protocol"], PROTOCOL_VERSION);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn concurrent_clients_share_one_daemon() {
    let (_dir, ws) = workspace();
    let _server = start_server(ws.clone(), 30).await;
    let mut tasks = Vec::new();
    for _ in 0..12 {
        let ws = ws.clone();
        tasks.push(tokio::spawn(async move {
            daemon::request(&ws, Request::Status).await.unwrap()
        }));
    }
    let mut instances = std::collections::HashSet::new();
    for task in tasks {
        let status = task.await.unwrap();
        instances.insert(status["instance"].as_str().unwrap().to_string());
        assert_eq!(status["pid"], std::process::id());
    }
    assert_eq!(instances.len(), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn second_serve_does_not_replace_the_lock_owner() {
    let (_dir, ws) = workspace();
    let _server = start_server(ws.clone(), 30).await;
    let first = tokio::time::timeout(
        Duration::from_secs(10),
        daemon::request(&ws, Request::Status),
    )
    .await
    .unwrap()
    .unwrap();
    let started = Instant::now();
    let ws2 = ws.clone();
    let second = tokio::spawn(async move { daemon::serve(ws2, 30).await });
    let result = tokio::time::timeout(Duration::from_secs(3), second)
        .await
        .expect("second daemon blocked on the lock");
    assert!(result.unwrap().is_ok());
    assert!(started.elapsed() < Duration::from_secs(3));
    let again = daemon::request(&ws, Request::Status).await.unwrap();
    assert_eq!(again["instance"], first["instance"]);
    assert_eq!(again["pid"], first["pid"]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn idle_shutdown_removes_socket() {
    let (_dir, ws) = workspace();
    let server = start_server(ws.clone(), 2).await;
    assert!(
        wait_until(|| socket_of(&ws).exists(), Duration::from_secs(10)).await,
        "socket did not appear"
    );
    assert!(
        wait_until(|| !socket_of(&ws).exists(), Duration::from_secs(8)).await,
        "socket survived idle shutdown"
    );
    let finished = tokio::time::timeout(Duration::from_secs(3), server)
        .await
        .expect("serve did not return");
    assert!(finished.unwrap().is_ok());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stale_socket_is_replaced_after_lock() {
    let (_dir, ws) = workspace();
    fs::write(socket_of(&ws), b"stale").unwrap();
    let _server = start_server(ws.clone(), 30).await;
    let status = tokio::time::timeout(
        Duration::from_secs(10),
        daemon::request(&ws, Request::Status),
    )
    .await
    .expect("stale socket was not replaced")
    .unwrap();
    assert_eq!(status["workspace"], ws.root.to_str().unwrap());
    #[cfg(unix)]
    {
        use std::os::unix::fs::FileTypeExt;
        assert!(
            fs::metadata(socket_of(&ws))
                .unwrap()
                .file_type()
                .is_socket()
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn missing_evidence_is_an_explicit_error() {
    let (_dir, ws) = workspace();
    let _server = start_server(ws.clone(), 30).await;
    let error = tokio::time::timeout(
        Duration::from_secs(10),
        daemon::request(
            &ws,
            Request::Evidence {
                id: "missing-evidence".into(),
            },
        ),
    )
    .await
    .expect("evidence request timed out")
    .unwrap_err();
    let message = format!("{error:#}").to_lowercase();
    assert!(
        message.contains("expired") || message.contains("not found"),
        "{message}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shutdown_request_clears_owned_socket() {
    let (_dir, ws) = workspace();
    let server = start_server(ws.clone(), 30).await;
    let value = tokio::time::timeout(
        Duration::from_secs(10),
        daemon::request(&ws, Request::Shutdown),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(value["shutting_down"], true);
    assert!(
        wait_until(|| !socket_of(&ws).exists(), Duration::from_secs(5)).await,
        "socket remained after shutdown"
    );
    let finished = tokio::time::timeout(Duration::from_secs(3), server)
        .await
        .expect("serve hung after shutdown");
    assert!(finished.unwrap().is_ok());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn process_lifecycle_when_binary_is_built() {
    let Some(bin) = bin() else {
        return;
    };
    let (_dir, ws) = workspace();
    let log_path = ws.state_dir.join("daemon.log");
    let log = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .unwrap();
    let mut child = tokio::process::Command::new(bin)
        .arg("--workspace")
        .arg(&ws.root)
        .arg("daemon")
        .arg("--idle-seconds")
        .arg("30")
        .current_dir(&ws.root)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(log)
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    assert!(
        wait_until(|| socket_of(&ws).exists(), Duration::from_secs(10)).await,
        "daemon process did not create a socket"
    );

    let mut tasks = Vec::new();
    for _ in 0..8 {
        let ws = ws.clone();
        tasks.push(tokio::spawn(async move {
            daemon::request(&ws, Request::Status).await.unwrap()
        }));
    }
    let mut pids = std::collections::HashSet::new();
    let mut instances = std::collections::HashSet::new();
    for task in tasks {
        let status = task.await.unwrap();
        pids.insert(status["pid"].as_u64().unwrap());
        instances.insert(status["instance"].as_str().unwrap().to_string());
        assert_eq!(status["protocol"], PROTOCOL_VERSION);
        assert_eq!(status["workspace"], ws.root.to_str().unwrap());
    }
    assert_eq!(pids.len(), 1, "concurrent clients did not share one daemon");
    assert_eq!(instances.len(), 1);

    let again = daemon::request(&ws, Request::Status).await.unwrap();
    assert_eq!(again["instance"], instances.iter().next().unwrap().as_str());

    let mut raced = Vec::new();
    for _ in 0..3 {
        let root = ws.root.clone();
        let log = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log_path)
            .unwrap();
        raced.push(
            tokio::process::Command::new(bin)
                .arg("--workspace")
                .arg(&root)
                .arg("daemon")
                .arg("--idle-seconds")
                .arg("30")
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(log)
                .kill_on_drop(true)
                .spawn()
                .unwrap(),
        );
    }
    for mut extra in raced {
        let status = tokio::time::timeout(Duration::from_secs(5), extra.wait())
            .await
            .expect("duplicate daemon hung")
            .unwrap();
        assert!(status.success(), "duplicate daemon exited {status}");
    }
    let still = daemon::request(&ws, Request::Status).await.unwrap();
    assert_eq!(still["pid"], again["pid"]);

    let mut bad = tokio::net::UnixStream::connect(socket_of(&ws))
        .await
        .unwrap();
    let mut request = WireRequest::current(Request::Status);
    request.version = PROTOCOL_VERSION + 4;
    let mut frame = serde_json::to_vec(&request).unwrap();
    frame.push(b'\n');
    use tokio::io::AsyncWriteExt;
    bad.write_all(&frame).await.unwrap();
    let (reader, _writer) = bad.into_split();
    let mut reader = tokio::io::BufReader::new(reader);
    let mut line = String::new();
    tokio::io::AsyncBufReadExt::read_line(&mut reader, &mut line)
        .await
        .unwrap();
    assert!(line.contains("protocol version mismatch"), "{line}");
    assert!(daemon::request(&ws, Request::Status).await.is_ok());

    child.kill().await.unwrap();
    let _ = tokio::time::timeout(Duration::from_secs(3), child.wait()).await;
    assert!(
        socket_of(&ws).exists(),
        "killed daemon removed its socket; crash recovery needs the stale file"
    );

    let log = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .unwrap();
    let mut restarted = tokio::process::Command::new(bin)
        .arg("--workspace")
        .arg(&ws.root)
        .arg("daemon")
        .arg("--idle-seconds")
        .arg("30")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(log)
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let recovered = tokio::time::timeout(
        Duration::from_secs(10),
        daemon::request(&ws, Request::Status),
    )
    .await
    .expect("restart after crash timed out")
    .unwrap();
    assert_ne!(recovered["pid"], again["pid"]);
    assert_ne!(recovered["instance"], again["instance"]);
    restarted.kill().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn process_idle_shutdown_when_binary_is_built() {
    let Some(bin) = bin() else {
        return;
    };
    let (_dir, ws) = workspace();
    let log = OpenOptions::new()
        .create(true)
        .append(true)
        .open(ws.state_dir.join("daemon.log"))
        .unwrap();
    let mut child = tokio::process::Command::new(bin)
        .arg("--workspace")
        .arg(&ws.root)
        .arg("daemon")
        .arg("--idle-seconds")
        .arg("1")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(log)
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let status = tokio::time::timeout(Duration::from_secs(15), child.wait())
        .await
        .expect("idle daemon did not exit")
        .unwrap();
    assert!(status.success(), "{status}");
    assert!(
        !socket_of(&ws).exists(),
        "idle shutdown left the socket behind"
    );
    let log_text = fs::read(ws.state_dir.join("daemon.log")).unwrap_or_default();
    assert!(log_text.len() < 2 * 1024 * 1024);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn client_waits_past_ready_timeout_while_lock_is_held() {
    let Some(bin) = bin() else {
        return;
    };
    let (_dir, ws) = workspace();
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(ws.state_dir.join("daemon.lock"))
        .unwrap();
    lock.lock_exclusive().unwrap();
    let client = tokio::process::Command::new(bin)
        .arg("--workspace")
        .arg(&ws.root)
        .arg("status")
        .env("CHECKWEAVE_IDLE_SECONDS", "5")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    // Longer than the 15 s ready timeout: a slow cache open must not fail clients.
    tokio::time::sleep(Duration::from_secs(17)).await;
    fs2::FileExt::unlock(&lock).unwrap();
    let output = tokio::time::timeout(Duration::from_secs(30), client.wait_with_output())
        .await
        .expect("client did not finish")
        .unwrap();
    assert!(
        output.status.success(),
        "stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let _ = daemon::shutdown_if_running(&ws).await;
}

#[test]
fn wire_request_round_trip_matches_protocol() {
    let wire = WireRequest::current(Request::Status);
    let bytes = serde_json::to_vec(&wire).unwrap();
    assert!(bytes.len() < 8 * 1024 * 1024);
    let parsed: WireRequest = serde_json::from_slice(&bytes).unwrap();
    assert!(parsed.same_implementation());
    assert_eq!(parsed.implementation, IMPLEMENTATION_VERSION);
    assert!(matches!(parsed.request, Request::Status));
}
