//! One shared daemon per canonical workspace root.
//!
//! Clients auto-start `checkweave --workspace ROOT daemon --idle-seconds N`
//! only when no daemon is reachable. The process holds an exclusive `fs2` lock
//! for its lifetime, speaks newline-delimited JSON on a Unix socket or Windows
//! named pipe, and is the only writer of the collection engine.

use std::collections::HashMap;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, anyhow, bail};
use fs2::FileExt;
use notify::Watcher;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::{Notify, Semaphore, mpsc, watch};
use tokio::time::Instant;

use crate::types::{
    CheckReport, CheckRequest, IMPLEMENTATION_VERSION, PROTOCOL_VERSION, ReplayKind, Request,
    WireRequest, WireResponse, WorkRequest,
};
use crate::workspace::{self, Workspace};

const MAX_FRAME: usize = 8 * 1024 * 1024;
const MAX_CLIENTS: usize = 32;
const MAX_ENGINE_JOBS: usize = 8;
const MAX_INFLIGHT_RUNS: usize = 8;
const MAX_RETAINED_RUNS: usize = 32;
const MAX_RETAINED_RESULT_BYTES: usize = 8 * 1024 * 1024;
const READ_TIMEOUT: Duration = Duration::from_secs(30);
const WRITE_TIMEOUT: Duration = Duration::from_secs(30);
const READY_TIMEOUT: Duration = Duration::from_secs(15);
const STARTING_TIMEOUT: Duration = Duration::from_secs(120);
const RESPAWN_INTERVAL: Duration = Duration::from_millis(500);
const MAX_SPAWNS: u32 = 5;
const ENGINE_WAIT: Duration = Duration::from_secs(5);
const LOG_LIMIT: u64 = 1024 * 1024;
const LOG_KEEP: usize = 256 * 1024;
const COALESCE: Duration = Duration::from_millis(200);
const POLL_FALLBACK: Duration = Duration::from_secs(5);
const DEFAULT_IDLE_SECONDS: u64 = 300;

#[derive(Debug, Clone, Serialize, Deserialize)]
struct DaemonIdentity {
    pid: u32,
    instance: String,
    protocol: u32,
    #[serde(default)]
    implementation: String,
    workspace: PathBuf,
    endpoint: String,
}

struct Shared {
    workspace: Workspace,
    instance: String,
    engine: Arc<Mutex<crate::collection::Engine>>,
    engine_ops: Arc<AtomicUsize>,
    queue: Arc<Semaphore>,
    cancel: Arc<AtomicBool>,
    inflight: Arc<Mutex<HashMap<String, SharedCheck>>>,
    runs: Mutex<HashMap<String, RunRecord>>,
    run_seq: AtomicU64,
    tasks: Mutex<Vec<tokio::task::JoinHandle<()>>>,
    model: tokio::sync::Mutex<Option<crate::models::ModelProvider>>,
    watcher_mode: Mutex<String>,
    activity: Notify,
    shutdown: Notify,
    stop: AtomicBool,
    active: AtomicUsize,
    log_path: PathBuf,
}

struct SharedCheck {
    receiver: watch::Receiver<Option<Result<CheckReport, String>>>,
    stop: Arc<AtomicBool>,
    sync_waiters: Arc<AtomicUsize>,
    run_waiters: Arc<AtomicUsize>,
}

struct RunRecord {
    ord: u64,
    state: String,
    operation: String,
    detail: Option<String>,
    result: Option<Value>,
    error: Option<String>,
    abandoned: Arc<AtomicBool>,
    job_key: Option<String>,
}

struct WaiterGuard {
    as_run: bool,
    abandoned: Option<Arc<AtomicBool>>,
    stop: Arc<AtomicBool>,
    sync_waiters: Arc<AtomicUsize>,
    run_waiters: Arc<AtomicUsize>,
}

impl Drop for WaiterGuard {
    fn drop(&mut self) {
        let sync_left = if self.as_run {
            self.sync_waiters.load(Ordering::Acquire)
        } else {
            self.sync_waiters.fetch_sub(1, Ordering::AcqRel) - 1
        };
        let run_left = if self.as_run {
            self.run_waiters.fetch_sub(1, Ordering::AcqRel) - 1
        } else {
            self.run_waiters.load(Ordering::Acquire)
        };
        let abandoned = self
            .abandoned
            .as_ref()
            .is_some_and(|flag| flag.load(Ordering::Acquire));
        if abandoned && sync_left == 0 && run_left == 0 {
            self.stop.store(true, Ordering::Relaxed);
        }
    }
}

struct Cleanup {
    identity_path: PathBuf,
    endpoint: String,
    instance: String,
}

impl Drop for Cleanup {
    fn drop(&mut self) {
        if std::thread::panicking() {
            return;
        }
        if !identity_matches(&self.identity_path, &self.instance) {
            return;
        }
        remove_endpoint(&self.endpoint);
        let _ = fs::remove_file(&self.identity_path);
    }
}

struct EngineOpGuard(Arc<AtomicUsize>);

impl Drop for EngineOpGuard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

pub async fn request(workspace: &Workspace, request: Request) -> anyhow::Result<Value> {
    let idle_seconds = idle_seconds_from_env();
    let wire = WireRequest::current(request);
    let started = Instant::now();
    let limit = started + STARTING_TIMEOUT;
    let mut deadline = started + READY_TIMEOUT;
    let mut last_spawn: Option<Instant> = None;
    let mut spawns = 0;
    let (last_error, starting) = loop {
        match assess(workspace) {
            Assess::Fatal(error) => return Err(error),
            Assess::Ready | Assess::Starting | Assess::Absent | Assess::Stale => {}
        }
        let error = match exchange(workspace, &wire).await {
            Ok(value) => return Ok(value),
            Err(ExchangeError::Fatal(error) | ExchangeError::Application(error)) => {
                return Err(error);
            }
            Err(ExchangeError::Retry(error)) => error,
        };
        // A spawned daemon exits when another process holds the lock, and that
        // holder may itself be shutting down. Spawn again whenever nothing is
        // running; the lock keeps extra spawns harmless.
        let state = assess(workspace);
        let starting = matches!(state, Assess::Starting);
        let now = Instant::now();
        if matches!(state, Assess::Absent | Assess::Stale)
            && spawns < MAX_SPAWNS
            && last_spawn.is_none_or(|at| now.duration_since(at) >= RESPAWN_INTERVAL)
        {
            spawn_daemon(workspace, idle_seconds)?;
            spawns += 1;
            last_spawn = Some(now);
            deadline = deadline.max(now + READY_TIMEOUT);
        }
        // A held lock means a live process is still opening the cache, which
        // can take longer than READY_TIMEOUT under heavy disk load.
        if starting {
            deadline = deadline.max(now + READY_TIMEOUT);
        }
        if now >= deadline.min(limit) {
            break (error, starting);
        }
        tokio::time::sleep(Duration::from_millis(40)).await;
    };
    let log = workspace.state_dir.join("daemon.log");
    let mut detail = last_error.to_string();
    if let Some(line) = fs::read_to_string(&log).ok().and_then(|text| {
        text.lines()
            .rev()
            .find(|line| !line.trim().is_empty())
            .map(str::to_owned)
    }) {
        detail = format!("{detail}; daemon log: {line}");
    }
    let waited = started.elapsed().as_secs();
    if starting {
        Err(anyhow!(
            "daemon still starting after {waited}s while holding the initialization lock ({detail}); see {}",
            log.display()
        ))
    } else {
        Err(anyhow!(
            "daemon did not become ready after {waited}s ({detail}); see {}",
            log.display()
        ))
    }
}

/// Stop a daemon that is already running. Does not start one.
pub async fn shutdown_if_running(workspace: &Workspace) -> anyhow::Result<bool> {
    match assess(workspace) {
        Assess::Absent | Assess::Stale => Ok(false),
        Assess::Fatal(error) => Err(error),
        Assess::Ready | Assess::Starting => {
            let wire = WireRequest::current(Request::Shutdown);
            let deadline = Instant::now() + Duration::from_secs(5);
            loop {
                match exchange(workspace, &wire).await {
                    Ok(_) => return Ok(true),
                    Err(ExchangeError::Fatal(error) | ExchangeError::Application(error)) => {
                        return Err(error);
                    }
                    Err(ExchangeError::Retry(error)) if Instant::now() >= deadline => {
                        return Err(error);
                    }
                    Err(ExchangeError::Retry(_)) => {
                        tokio::time::sleep(Duration::from_millis(40)).await;
                    }
                }
            }
        }
    }
}

pub async fn serve(workspace: Workspace, idle_seconds: u64) -> anyhow::Result<()> {
    fs::create_dir_all(&workspace.state_dir)
        .with_context(|| format!("create {}", workspace.state_dir.display()))?;
    let lock_path = workspace.state_dir.join("daemon.lock");
    let lock_file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&lock_path)
        .with_context(|| format!("open {}", lock_path.display()))?;
    if let Err(error) = lock_file.try_lock_exclusive() {
        if lock_contended(&error) {
            log_line(
                &workspace.state_dir.join("daemon.log"),
                "daemon already running",
            );
            return Ok(());
        }
        return Err(error).context("lock daemon");
    }

    let log_path = workspace.state_dir.join("daemon.log");
    log_line(
        &log_path,
        &format!("daemon starting pid={}, opening cache", std::process::id()),
    );
    let opening = Instant::now();
    let root = workspace.root.clone();
    let engine = tokio::task::spawn_blocking(move || crate::collection::Engine::open(&root))
        .await
        .context("engine open task")?
        .inspect_err(|error| log_line(&log_path, &format!("cache open failed: {error:#}")))?;
    log_line(
        &log_path,
        &format!("cache opened in {} ms", opening.elapsed().as_millis()),
    );

    let instance = uuid::Uuid::new_v4().to_string();
    let identity_path = workspace.state_dir.join("daemon.json");
    let endpoint = endpoint_name(&workspace);
    let _ = fs::remove_file(&identity_path);
    let listener = bind_listener(&endpoint)?;
    let identity = DaemonIdentity {
        pid: std::process::id(),
        instance: instance.clone(),
        protocol: PROTOCOL_VERSION,
        implementation: IMPLEMENTATION_VERSION.to_string(),
        workspace: workspace.root.clone(),
        endpoint: endpoint.clone(),
    };
    write_identity(&identity_path, &identity)?;
    // Declared after the lock file so cleanup runs while the lock is still held.
    let _cleanup = Cleanup {
        identity_path,
        endpoint,
        instance: instance.clone(),
    };

    let shared = Arc::new(Shared {
        workspace: workspace.clone(),
        instance,
        engine: Arc::new(Mutex::new(engine)),
        engine_ops: Arc::new(AtomicUsize::new(0)),
        queue: Arc::new(Semaphore::new(MAX_ENGINE_JOBS)),
        cancel: Arc::new(AtomicBool::new(false)),
        inflight: Arc::new(Mutex::new(HashMap::new())),
        runs: Mutex::new(HashMap::new()),
        run_seq: AtomicU64::new(1),
        tasks: Mutex::new(Vec::new()),
        model: tokio::sync::Mutex::new(None),
        watcher_mode: Mutex::new("poll".to_string()),
        activity: Notify::new(),
        shutdown: Notify::new(),
        stop: AtomicBool::new(false),
        active: AtomicUsize::new(0),
        log_path: workspace.state_dir.join("daemon.log"),
    });
    log_line(
        &shared.log_path,
        &format!(
            "daemon started pid={} instance={} protocol={PROTOCOL_VERSION}",
            std::process::id(),
            shared.instance
        ),
    );

    let changes = Arc::new(Notify::new());
    let watcher = install_watcher(&workspace.root, &changes);
    let mode = match &watcher {
        Some(_) => native_or_poll_label(),
        None => "poll",
    };
    *shared
        .watcher_mode
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = mode.to_string();

    let reconcile = tokio::spawn(reconcile_loop(Arc::clone(&shared), Arc::clone(&changes)));
    let signals = tokio::spawn(watch_signals(Arc::clone(&shared)));

    let (tx, rx) = mpsc::channel(MAX_CLIENTS);
    let accept = tokio::spawn(accept_loop(listener, tx, Arc::clone(&shared)));
    let session = run_session(rx, Arc::clone(&shared), idle_seconds).await;

    shared.stop.store(true, Ordering::Relaxed);
    shared.cancel.store(true, Ordering::Relaxed);
    stop_checks(&shared);
    shared.shutdown.notify_waiters();
    accept.abort();
    let _ = accept.await;
    let _ = reconcile.await;
    signals.abort();
    let _ = signals.await;
    drop(watcher);
    reap_background(&shared).await;
    wait_for_engine(&shared).await;
    log_line(&shared.log_path, "daemon stopped");
    session
}

enum Assess {
    Ready,
    Starting,
    Absent,
    Stale,
    Fatal(anyhow::Error),
}

fn assess(workspace: &Workspace) -> Assess {
    let lock_held = daemon_lock_held(workspace);
    let Some(identity) = read_identity(&workspace.state_dir.join("daemon.json")) else {
        return if lock_held {
            Assess::Starting
        } else {
            Assess::Absent
        };
    };
    let protocol_ok = identity.protocol == PROTOCOL_VERSION;
    let implementation_ok = identity.implementation == IMPLEMENTATION_VERSION;
    let workspace_ok = same_path(&identity.workspace, &workspace.root);
    let endpoint_ok = identity.endpoint == endpoint_name(workspace);
    let pid_ok = pid_alive(identity.pid);
    if lock_held && pid_ok && !protocol_ok {
        return Assess::Fatal(anyhow!(
            "protocol version mismatch: daemon speaks v{}, client speaks v{PROTOCOL_VERSION}",
            identity.protocol
        ));
    }
    if lock_held && pid_ok && !implementation_ok {
        return Assess::Fatal(anyhow!(
            "protocol version mismatch: daemon implementation {}, client implementation {IMPLEMENTATION_VERSION}",
            identity.implementation
        ));
    }
    if lock_held && pid_ok && (!workspace_ok || !endpoint_ok) {
        return Assess::Fatal(anyhow!(
            "refusing daemon for workspace {}, expected {}",
            identity.workspace.display(),
            workspace.root.display()
        ));
    }
    if lock_held && pid_ok && protocol_ok && implementation_ok && workspace_ok && endpoint_ok {
        return Assess::Ready;
    }
    if lock_held {
        return Assess::Starting;
    }
    Assess::Stale
}

enum ExchangeError {
    Fatal(anyhow::Error),
    Application(anyhow::Error),
    Retry(anyhow::Error),
}

async fn exchange(workspace: &Workspace, wire: &WireRequest) -> Result<Value, ExchangeError> {
    let stream = connect_endpoint(&endpoint_name(workspace))
        .await
        .map_err(|error| ExchangeError::Retry(error.into()))?;
    let (mut reader, mut writer) = tokio::io::split(stream);
    let mut frame = serde_json::to_vec(wire).map_err(|error| ExchangeError::Fatal(error.into()))?;
    if frame.len() + 1 > MAX_FRAME {
        return Err(ExchangeError::Fatal(anyhow!("request exceeds 8MiB")));
    }
    frame.push(b'\n');
    tokio::time::timeout(WRITE_TIMEOUT, async {
        writer.write_all(&frame).await?;
        writer.flush().await?;
        Ok::<(), io::Error>(())
    })
    .await
    .map_err(|_| ExchangeError::Retry(anyhow!("daemon write timed out")))?
    .map_err(|error| ExchangeError::Retry(error.into()))?;

    let bytes = tokio::time::timeout(response_timeout(&wire.request), read_frame(&mut reader))
        .await
        .map_err(|_| ExchangeError::Retry(anyhow!("daemon read timed out")))?
        .map_err(|error| {
            if error.kind() == io::ErrorKind::InvalidData {
                ExchangeError::Fatal(error.into())
            } else {
                ExchangeError::Retry(error.into())
            }
        })?;
    let Some(bytes) = bytes else {
        return Err(ExchangeError::Retry(anyhow!(
            "daemon closed the connection"
        )));
    };
    let response: WireResponse = serde_json::from_slice(&bytes).map_err(|error| {
        ExchangeError::Application(anyhow!("malformed daemon response: {error}"))
    })?;
    if response.version != PROTOCOL_VERSION || response.implementation != IMPLEMENTATION_VERSION {
        return Err(ExchangeError::Fatal(anyhow!(
            "protocol version mismatch: response v{} implementation {} , client v{PROTOCOL_VERSION} implementation {IMPLEMENTATION_VERSION}",
            response.version,
            response.implementation
        )));
    }
    if let Some(error) = response.error {
        return Err(ExchangeError::Application(anyhow!(error)));
    }
    response
        .result
        .ok_or_else(|| ExchangeError::Application(anyhow!("empty daemon response")))
}

fn spawn_daemon(workspace: &Workspace, idle_seconds: u64) -> anyhow::Result<()> {
    fs::create_dir_all(&workspace.state_dir)?;
    let log_path = workspace.state_dir.join("daemon.log");
    trim_log(&log_path);
    let log_file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .with_context(|| format!("open {}", log_path.display()))?;
    let exe = std::env::current_exe().context("current executable")?;
    let mut command = Command::new(exe);
    command
        .arg("--workspace")
        .arg(&workspace.root)
        .arg("daemon")
        .arg("--idle-seconds")
        .arg(idle_seconds.to_string())
        .current_dir(&workspace.root)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::from(log_file));
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        command.creation_flags(CREATE_NEW_PROCESS_GROUP);
    }
    let child = command.spawn().context("spawn checkweave daemon")?;
    std::thread::spawn(move || {
        let mut child = child;
        let _ = child.wait();
    });
    Ok(())
}

fn session_busy(shared: &Shared) -> bool {
    if shared.active.load(Ordering::Relaxed) > 0 {
        return true;
    }
    let runs = shared
        .runs
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    runs.values()
        .any(|run| matches!(run.state.as_str(), "queued" | "running"))
}

fn idle_seconds_from_env() -> u64 {
    std::env::var("CHECKWEAVE_IDLE_SECONDS")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(DEFAULT_IDLE_SECONDS)
}

async fn run_session(
    mut incoming: mpsc::Receiver<ServerConn>,
    shared: Arc<Shared>,
    idle_seconds: u64,
) -> anyhow::Result<()> {
    let slots = Arc::new(Semaphore::new(MAX_CLIENTS));
    let mut deadline = Instant::now() + Duration::from_secs(idle_seconds);
    loop {
        if shared.stop.load(Ordering::Relaxed) {
            break;
        }
        let now = Instant::now();
        if session_busy(&shared) {
            deadline = now + Duration::from_secs(idle_seconds);
        } else if now >= deadline {
            break;
        }
        let sleep_for = if session_busy(&shared) {
            Duration::from_secs(1)
        } else {
            deadline.saturating_duration_since(now)
        };
        tokio::select! {
            _ = shared.shutdown.notified() => break,
            _ = shared.activity.notified() => {
                deadline = Instant::now() + Duration::from_secs(idle_seconds);
            }
            _ = tokio::time::sleep(sleep_for) => {
                if !session_busy(&shared) && Instant::now() >= deadline {
                    break;
                }
            }
            accepted = incoming.recv() => {
                let Some(stream) = accepted else { break };
                deadline = Instant::now() + Duration::from_secs(idle_seconds);
                shared.activity.notify_one();
                if let Ok(permit) = slots.clone().try_acquire_owned() {
                    let shared = Arc::clone(&shared);
                    tokio::spawn(async move {
                        let _permit = permit;
                        handle_client(stream, shared).await;
                    });
                } else {
                    tokio::spawn(async move {
                        reject_busy(stream).await;
                    });
                }
            }
        }
    }
    Ok(())
}

async fn handle_client<S>(stream: S, shared: Arc<Shared>)
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    shared.active.fetch_add(1, Ordering::AcqRel);
    shared.activity.notify_one();
    let _active = ActiveGuard(Arc::clone(&shared));
    let (mut reader, mut writer) = tokio::io::split(stream);
    let frame = match tokio::time::timeout(READ_TIMEOUT, read_frame(&mut reader)).await {
        Ok(Ok(Some(frame))) => frame,
        Ok(Ok(None)) | Err(_) => return,
        Ok(Err(error)) => {
            let _ = write_response(&mut writer, &WireResponse::failure(error)).await;
            return;
        }
    };
    let wire: WireRequest = match serde_json::from_slice(&frame) {
        Ok(wire) => wire,
        Err(error) => {
            let _ = write_response(
                &mut writer,
                &WireResponse::failure(format!("malformed request: {error}")),
            )
            .await;
            return;
        }
    };
    if !wire.same_implementation() {
        let _ = write_response(
            &mut writer,
            &WireResponse::failure(format!(
                "protocol version mismatch: request v{} implementation {} , daemon v{PROTOCOL_VERSION} implementation {IMPLEMENTATION_VERSION}",
                wire.version, wire.implementation
            )),
        )
        .await;
        return;
    }
    let response = match dispatch(&shared, wire.request).await {
        Ok(value) => match WireResponse::success(value) {
            Ok(response) => response,
            Err(error) => WireResponse::failure(error),
        },
        Err(error) => WireResponse::failure(error),
    };
    let _ = write_response(&mut writer, &response).await;
    if matches!(response.result, Some(ref value) if value.get("shutting_down").and_then(Value::as_bool) == Some(true))
    {
        shared.stop.store(true, Ordering::Relaxed);
        shared.cancel.store(true, Ordering::Relaxed);
        stop_checks(&shared);
        shared.shutdown.notify_waiters();
    }
}

struct ActiveGuard(Arc<Shared>);

impl Drop for ActiveGuard {
    fn drop(&mut self) {
        self.0.active.fetch_sub(1, Ordering::AcqRel);
        self.0.activity.notify_one();
    }
}

async fn reject_busy<S>(stream: S)
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let (_reader, mut writer) = tokio::io::split(stream);
    let _ = write_response(&mut writer, &WireResponse::failure("too many clients")).await;
}

async fn dispatch(shared: &Arc<Shared>, request: Request) -> anyhow::Result<Value> {
    match request {
        Request::Check(check) => {
            let report = dedup_check(shared, check, false, None).await?;
            Ok(serde_json::to_value(report)?)
        }
        Request::Evidence { id } => evidence_any(shared, &id).await,
        Request::TracePage { id, offset, limit } => trace_page(shared, &id, offset, limit).await,
        Request::Status => status(shared).await,
        Request::Shutdown => Ok(json!({ "shutting_down": true })),
        Request::RunStart { request } => start_run(Arc::clone(shared), request),
        Request::RunStatus { id } => run_snapshot(shared, &id),
        Request::RunCancel { id } => cancel_run(shared, &id),
        Request::Compare { request } => {
            run_compare(shared, request, Arc::clone(&shared.cancel)).await
        }
        Request::Replay { kind, id } => {
            run_replay(shared, kind, &id, Arc::clone(&shared.cancel)).await
        }
        Request::Trace { request } => run_trace(shared, request, Arc::clone(&shared.cancel)).await,
        Request::ModelSetup { offline } => {
            run_model_setup(shared, offline, Arc::clone(&shared.cancel)).await
        }
        Request::ModelEvaluate { request } => {
            run_model_evaluate(shared, request, Arc::clone(&shared.cancel)).await
        }
        Request::Semantic { request } => {
            run_semantic(shared, request, Arc::clone(&shared.cancel)).await
        }
    }
}

async fn dedup_check(
    shared: &Shared,
    request: CheckRequest,
    as_run: bool,
    abandoned: Option<Arc<AtomicBool>>,
) -> anyhow::Result<CheckReport> {
    let key = serde_json::to_string(&request).context("serialize check request")?;
    enum Role {
        Lead(
            watch::Sender<Option<Result<CheckReport, String>>>,
            Arc<AtomicBool>,
        ),
        Follow(watch::Receiver<Option<Result<CheckReport, String>>>),
    }
    let (role, guard) = {
        let mut map = shared
            .inflight
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(existing) = map.get(&key) {
            if as_run {
                existing.run_waiters.fetch_add(1, Ordering::AcqRel);
            } else {
                existing.sync_waiters.fetch_add(1, Ordering::AcqRel);
            }
            let guard = WaiterGuard {
                as_run,
                abandoned: abandoned.clone(),
                stop: Arc::clone(&existing.stop),
                sync_waiters: Arc::clone(&existing.sync_waiters),
                run_waiters: Arc::clone(&existing.run_waiters),
            };
            (Role::Follow(existing.receiver.clone()), guard)
        } else {
            let (sender, receiver) = watch::channel(None);
            let stop = Arc::new(AtomicBool::new(false));
            let sync_waiters = Arc::new(AtomicUsize::new(if as_run { 0 } else { 1 }));
            let run_waiters = Arc::new(AtomicUsize::new(if as_run { 1 } else { 0 }));
            let guard = WaiterGuard {
                as_run,
                abandoned: abandoned.clone(),
                stop: Arc::clone(&stop),
                sync_waiters: Arc::clone(&sync_waiters),
                run_waiters: Arc::clone(&run_waiters),
            };
            map.insert(
                key.clone(),
                SharedCheck {
                    receiver,
                    stop: Arc::clone(&stop),
                    sync_waiters,
                    run_waiters,
                },
            );
            (Role::Lead(sender, stop), guard)
        }
    };
    let _guard = guard;
    if abandoned
        .as_ref()
        .is_some_and(|flag| flag.load(Ordering::Acquire))
    {
        if let Role::Lead(_, stop) = &role {
            let job = shared
                .inflight
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if let Some(existing) = job.get(&key)
                && existing.sync_waiters.load(Ordering::Acquire) == 0
                && existing.run_waiters.load(Ordering::Acquire) <= 1
            {
                stop.store(true, Ordering::Relaxed);
            }
        } else {
            bail!("cancelled");
        }
    }
    match role {
        Role::Follow(receiver) => wait_for_check(receiver).await,
        Role::Lead(sender, stop) => lead_check(shared, key, sender, stop, request).await,
    }
}

async fn lead_check(
    shared: &Shared,
    key: String,
    sender: watch::Sender<Option<Result<CheckReport, String>>>,
    stop: Arc<AtomicBool>,
    request: CheckRequest,
) -> anyhow::Result<CheckReport> {
    let _pop = InflightPop {
        key,
        map: Arc::clone(&shared.inflight),
    };
    let result = engine_check(shared, request, stop).await;
    let published = match &result {
        Ok(report) => Ok(report.clone()),
        Err(error) => Err(error.to_string()),
    };
    let _ = sender.send(Some(published));
    result
}

struct InflightPop {
    key: String,
    map: Arc<Mutex<HashMap<String, SharedCheck>>>,
}

impl Drop for InflightPop {
    fn drop(&mut self) {
        let mut map = self
            .map
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        map.remove(&self.key);
    }
}

async fn wait_for_check(
    mut receiver: watch::Receiver<Option<Result<CheckReport, String>>>,
) -> anyhow::Result<CheckReport> {
    loop {
        if let Some(value) = receiver.borrow().clone() {
            return match value {
                Ok(report) => Ok(report),
                Err(error) => Err(anyhow!(error)),
            };
        }
        if receiver.changed().await.is_err() {
            if let Some(value) = receiver.borrow().clone() {
                return match value {
                    Ok(report) => Ok(report),
                    Err(error) => Err(anyhow!(error)),
                };
            }
            bail!("check ended before a result was published");
        }
    }
}

async fn status(shared: &Shared) -> anyhow::Result<Value> {
    prune_tasks(shared);
    let stats = match shared.engine.try_lock() {
        Ok(engine) => engine.stats()?,
        Err(std::sync::TryLockError::WouldBlock) => json!({ "busy": true }),
        Err(std::sync::TryLockError::Poisoned(poisoned)) => poisoned.into_inner().stats()?,
    };
    let watcher = shared
        .watcher_mode
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone();
    let runs = {
        let runs = shared
            .runs
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        runs.iter()
            .map(|(id, run)| snapshot_run(id, run, false))
            .collect::<Vec<_>>()
    };
    Ok(json!({
        "pid": std::process::id(),
        "instance": shared.instance,
        "workspace": path_string(&shared.workspace.root)?,
        "protocol": PROTOCOL_VERSION,
        "implementation": IMPLEMENTATION_VERSION,
        "watcher": watcher,
        "stats": stats,
        "runs": runs,
    }))
}

async fn engine_check(
    shared: &Shared,
    request: CheckRequest,
    stop: Arc<AtomicBool>,
) -> anyhow::Result<CheckReport> {
    let shutdown = Arc::clone(&shared.cancel);
    engine_call(shared, move |engine| {
        if shutdown.load(Ordering::Relaxed) {
            stop.store(true, Ordering::Relaxed);
        }
        engine.check(&request, stop.as_ref())
    })
    .await
}

async fn engine_call<T, F>(shared: &Shared, work: F) -> anyhow::Result<T>
where
    T: Send + 'static,
    F: FnOnce(&mut crate::collection::Engine) -> anyhow::Result<T> + Send + 'static,
{
    let permit = tokio::time::timeout(
        Duration::from_secs(60),
        Arc::clone(&shared.queue).acquire_owned(),
    )
    .await
    .context("work queue timeout")?
    .context("work queue closed")?;
    let engine = Arc::clone(&shared.engine);
    let ops = Arc::clone(&shared.engine_ops);
    let cancel = Arc::clone(&shared.cancel);
    ops.fetch_add(1, Ordering::AcqRel);
    let guard = EngineOpGuard(ops);

    tokio::task::spawn_blocking(move || {
        let _permit = permit;
        let _guard = guard;
        let mut engine = engine
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if cancel.load(Ordering::Relaxed) {
            bail!("daemon is shutting down");
        }
        work(&mut engine)
    })
    .await
    .context("engine task failed")?
}

async fn reconcile_loop(shared: Arc<Shared>, changes: Arc<Notify>) {
    let _ = run_reconcile(&shared).await;
    let mut ticker = tokio::time::interval(POLL_FALLBACK);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    ticker.tick().await;
    loop {
        tokio::select! {
            _ = shared.shutdown.notified() => break,
            _ = changes.notified() => {
                tokio::select! {
                    _ = tokio::time::sleep(COALESCE) => {}
                    _ = shared.shutdown.notified() => break,
                }
            }
            _ = ticker.tick() => {}
        }
        if shared.stop.load(Ordering::Relaxed) {
            break;
        }
        let _ = run_reconcile(&shared).await;
    }
}

async fn run_reconcile(shared: &Shared) -> anyhow::Result<Value> {
    match engine_call(shared, |engine| engine.reconcile()).await {
        Ok(value) => Ok(value),
        Err(error) => {
            log_line(&shared.log_path, &format!("reconcile: {error:#}"));
            Err(error)
        }
    }
}

fn install_watcher(root: &Path, changes: &Arc<Notify>) -> Option<notify::RecommendedWatcher> {
    let notify_changes = Arc::clone(changes);
    let callback_root = root.to_path_buf();
    let mut watcher = notify::recommended_watcher(
        move |event: Result<notify::Event, notify::Error>| match event {
            Ok(event) => {
                let relevant = event.need_rescan()
                    || event.paths.is_empty()
                    || event
                        .paths
                        .iter()
                        .any(|path| !is_noise(path, &callback_root));
                if relevant {
                    notify_changes.notify_one();
                }
            }
            Err(_) => notify_changes.notify_one(),
        },
    )
    .ok()?;
    watcher.watch(root, notify::RecursiveMode::Recursive).ok()?;
    Some(watcher)
}

fn is_noise(path: &Path, root: &Path) -> bool {
    let relative = path.strip_prefix(root).unwrap_or(path);
    relative.components().any(|component| {
        matches!(component, Component::Normal(name) if name == ".checkweave" || name == ".git")
    })
}

fn native_or_poll_label() -> &'static str {
    match notify::RecommendedWatcher::kind() {
        notify::WatcherKind::PollWatcher => "poll",
        _ => "native",
    }
}

async fn wait_for_engine(shared: &Shared) {
    let deadline = Instant::now() + ENGINE_WAIT;
    while shared.engine_ops.load(Ordering::Relaxed) > 0 && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

async fn read_frame<R>(reader: &mut R) -> io::Result<Option<Vec<u8>>>
where
    R: AsyncRead + Unpin,
{
    let mut buf = Vec::new();
    let mut chunk = [0_u8; 8192];
    loop {
        let read = reader.read(&mut chunk).await?;
        if read == 0 {
            if buf.is_empty() {
                return Ok(None);
            }
            break;
        }
        let bytes = &chunk[..read];
        if let Some(position) = bytes.iter().position(|byte| *byte == b'\n') {
            let take = &bytes[..position];
            if buf.len() + take.len() > MAX_FRAME {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "frame exceeds 8MiB",
                ));
            }
            buf.extend_from_slice(take);
            return Ok(Some(buf));
        }
        if buf.len() + bytes.len() > MAX_FRAME {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "frame exceeds 8MiB",
            ));
        }
        buf.extend_from_slice(bytes);
    }
    Ok(Some(buf))
}

async fn write_response<W>(writer: &mut W, response: &WireResponse) -> anyhow::Result<()>
where
    W: AsyncWrite + Unpin,
{
    let mut bytes = serde_json::to_vec(response)?;
    if bytes.len() + 1 > MAX_FRAME {
        bytes = serde_json::to_vec(&WireResponse::failure("response exceeds 8MiB"))?;
    }
    bytes.push(b'\n');
    tokio::time::timeout(WRITE_TIMEOUT, async {
        writer.write_all(&bytes).await?;
        writer.flush().await?;
        Ok::<(), io::Error>(())
    })
    .await
    .context("write timed out")??;
    Ok(())
}

/// Socket paths are capped at 104 bytes on macOS and 108 on Linux, including
/// the terminator.
#[cfg(unix)]
const MAX_SOCKET_PATH: usize = 100;

/// Where the daemon listens. Unix uses `.checkweave/daemon.sock` unless that
/// path is too long for a socket, then a per-user directory under the temp dir.
pub fn endpoint_name(workspace: &Workspace) -> String {
    #[cfg(unix)]
    {
        let local = workspace.state_dir.join("daemon.sock");
        if local.as_os_str().len() <= MAX_SOCKET_PATH {
            return local.to_string_lossy().into_owned();
        }
        let hash = blake3::hash(workspace.root.as_os_str().as_encoded_bytes());
        fallback_socket_dir()
            .join(format!("{}.sock", &hash.to_hex()[..24]))
            .to_string_lossy()
            .into_owned()
    }
    #[cfg(windows)]
    {
        let hash = blake3::hash(workspace.root.as_os_str().as_encoded_bytes());
        format!(r"\\.\pipe\checkweave-{}", hash.to_hex())
    }
}

#[cfg(unix)]
type Listener = tokio::net::UnixListener;
#[cfg(unix)]
type ServerConn = tokio::net::UnixStream;
#[cfg(unix)]
type ClientConn = tokio::net::UnixStream;

#[cfg(windows)]
type Listener = tokio::net::windows::named_pipe::NamedPipeServer;
#[cfg(windows)]
type ServerConn = tokio::net::windows::named_pipe::NamedPipeServer;
#[cfg(windows)]
type ClientConn = tokio::net::windows::named_pipe::NamedPipeClient;

#[cfg(unix)]
fn fallback_socket_dir() -> PathBuf {
    // SAFETY: getuid has no preconditions and cannot fail.
    let uid = unsafe { libc::getuid() };
    let name = format!("checkweave-{uid}");
    let temp = std::env::temp_dir().join(&name);
    // Leave room for "/<24 hex>.sock".
    if temp.as_os_str().len() + 30 <= MAX_SOCKET_PATH {
        temp
    } else {
        Path::new("/tmp").join(name)
    }
}

/// The fallback directory may sit in a shared /tmp, so it must be a real
/// directory owned by this user and closed to everyone else.
#[cfg(unix)]
fn ensure_private_dir(dir: &Path) -> anyhow::Result<()> {
    use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
    match fs::DirBuilder::new().mode(0o700).create(dir) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(error).with_context(|| format!("create {}", dir.display())),
    }
    let meta = fs::symlink_metadata(dir).with_context(|| format!("stat {}", dir.display()))?;
    // SAFETY: getuid has no preconditions and cannot fail.
    let uid = unsafe { libc::getuid() };
    if !meta.is_dir() || meta.uid() != uid {
        bail!(
            "{} is not a directory owned by the current user",
            dir.display()
        );
    }
    if meta.permissions().mode() & 0o077 != 0 {
        fs::set_permissions(dir, fs::Permissions::from_mode(0o700))
            .with_context(|| format!("chmod 0700 {}", dir.display()))?;
    }
    Ok(())
}

fn bind_listener(endpoint: &str) -> anyhow::Result<Listener> {
    #[cfg(unix)]
    {
        let path = Path::new(endpoint);
        if let Some(parent) = path.parent() {
            if parent == fallback_socket_dir() {
                ensure_private_dir(parent)?;
            } else {
                fs::create_dir_all(parent)?;
            }
        }
        let _ = fs::remove_file(path);
        let listener = tokio::net::UnixListener::bind(path)
            .with_context(|| format!("bind {}", path.display()))?;
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))
            .with_context(|| format!("chmod 0600 {}", path.display()))?;
        Ok(listener)
    }
    #[cfg(windows)]
    {
        use tokio::net::windows::named_pipe::ServerOptions;
        let listener = ServerOptions::new()
            .first_pipe_instance(true)
            .reject_remote_clients(true)
            .create(endpoint)
            .with_context(|| format!("create named pipe {endpoint}"))?;
        Ok(listener)
    }
}

async fn connect_endpoint(endpoint: &str) -> io::Result<ClientConn> {
    #[cfg(unix)]
    {
        let path = Path::new(endpoint);
        if let Some(parent) = path.parent().filter(|dir| *dir == fallback_socket_dir()) {
            use std::os::unix::fs::{MetadataExt, PermissionsExt};
            let meta = fs::symlink_metadata(parent)?;
            // SAFETY: getuid has no preconditions and cannot fail.
            let uid = unsafe { libc::getuid() };
            if !meta.is_dir() || meta.uid() != uid || meta.permissions().mode() & 0o077 != 0 {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    format!("{} is not private to the current user", parent.display()),
                ));
            }
        }
        tokio::net::UnixStream::connect(path).await
    }
    #[cfg(windows)]
    {
        use tokio::net::windows::named_pipe::ClientOptions;
        ClientOptions::new().open(endpoint)
    }
}

fn remove_endpoint(endpoint: &str) {
    #[cfg(unix)]
    {
        let _ = fs::remove_file(endpoint);
    }
    #[cfg(windows)]
    {
        let _ = endpoint;
    }
}

async fn accept_loop(listener: Listener, tx: mpsc::Sender<ServerConn>, shared: Arc<Shared>) {
    #[cfg(unix)]
    {
        let listener = listener;
        loop {
            tokio::select! {
                _ = shared.shutdown.notified() => break,
                accepted = listener.accept() => {
                    match accepted {
                        Ok((stream, _)) => {
                            if tx.send(stream).await.is_err() {
                                break;
                            }
                        }
                        Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                        Err(error) => {
                            log_line(&shared.log_path, &format!("accept: {error}"));
                            break;
                        }
                    }
                }
            }
        }
    }
    #[cfg(windows)]
    {
        let mut listener = listener;
        let pipe_name = endpoint_name(&shared.workspace);
        loop {
            tokio::select! {
                _ = shared.shutdown.notified() => break,
                connected = listener.connect() => {
                    if let Err(error) = connected {
                        log_line(&shared.log_path, &format!("pipe connect: {error}"));
                        break;
                    }
                    let ready = listener;
                    listener = match tokio::net::windows::named_pipe::ServerOptions::new()
                        .reject_remote_clients(true)
                        .create(&pipe_name)
                    {
                        Ok(next) => next,
                        Err(error) => {
                            log_line(&shared.log_path, &format!("pipe create: {error}"));
                            break;
                        }
                    };
                    if tx.send(ready).await.is_err() {
                        break;
                    }
                }
            }
        }
    }
}

async fn watch_signals(shared: Arc<Shared>) {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        let mut terminate = match signal(SignalKind::terminate()) {
            Ok(signal) => signal,
            Err(error) => {
                log_line(&shared.log_path, &format!("signal: {error}"));
                return;
            }
        };
        let mut interrupt = match signal(SignalKind::interrupt()) {
            Ok(signal) => signal,
            Err(error) => {
                log_line(&shared.log_path, &format!("signal: {error}"));
                return;
            }
        };
        tokio::select! {
            _ = terminate.recv() => {}
            _ = interrupt.recv() => {}
            _ = shared.shutdown.notified() => return,
        }
        shared.stop.store(true, Ordering::Relaxed);
        shared.cancel.store(true, Ordering::Relaxed);
        stop_checks(&shared);
        shared.shutdown.notify_waiters();
    }
    #[cfg(windows)]
    {
        tokio::select! {
            result = tokio::signal::ctrl_c() => {
                if result.is_err() {
                    return;
                }
            }
            _ = shared.shutdown.notified() => return,
        }
        shared.stop.store(true, Ordering::Relaxed);
        shared.cancel.store(true, Ordering::Relaxed);
        stop_checks(&shared);
        shared.shutdown.notify_waiters();
    }
}

fn stop_checks(shared: &Shared) {
    let map = shared
        .inflight
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    for job in map.values() {
        job.stop.store(true, Ordering::Relaxed);
    }
}

fn response_timeout(request: &Request) -> Duration {
    let budget_ms = match request {
        Request::Check(check) => check.limits.timeout_ms,
        Request::RunStart {
            request: WorkRequest::Check(check),
        } => check.limits.timeout_ms,
        Request::Compare { request } | Request::Trace { request } => request
            .get("timeout_ms")
            .and_then(Value::as_u64)
            .unwrap_or(30_000),
        Request::ModelEvaluate { request } => request
            .get("timeout_ms")
            .and_then(Value::as_u64)
            .unwrap_or(180_000),
        Request::Semantic { request }
        | Request::RunStart {
            request: WorkRequest::Semantic { request },
        } => request
            .pointer("/limits/timeout_ms")
            .and_then(Value::as_u64)
            .unwrap_or(30_000),
        Request::Replay { .. } | Request::ModelSetup { .. } => 120_000,
        Request::Status
        | Request::Evidence { .. }
        | Request::TracePage { .. }
        | Request::Shutdown
        | Request::RunStatus { .. }
        | Request::RunCancel { .. }
        | Request::RunStart { .. } => 5_000,
    };
    Duration::from_millis(budget_ms.saturating_add(5_000))
}

fn work_name(request: &WorkRequest) -> &'static str {
    match request {
        WorkRequest::Check(_) => "check",
        WorkRequest::Evidence { .. } => "evidence",
        WorkRequest::Compare { .. } => "compare",
        WorkRequest::Replay { .. } => "replay",
        WorkRequest::Trace { .. } => "trace",
        WorkRequest::ModelSetup { .. } => "model_setup",
        WorkRequest::ModelEvaluate { .. } => "model_evaluate",
        WorkRequest::Semantic { .. } => "semantic",
    }
}

fn snapshot_run(id: &str, run: &RunRecord, include_result: bool) -> Value {
    let mut value = json!({
        "id": id,
        "state": run.state,
        "operation": run.operation,
    });
    if let Some(detail) = &run.detail {
        value["detail"] = json!(detail);
    }
    if include_result && let Some(result) = &run.result {
        value["result"] = result.clone();
    }
    if let Some(error) = &run.error {
        value["error"] = json!(error);
    }
    value
}

fn run_snapshot(shared: &Shared, id: &str) -> anyhow::Result<Value> {
    let runs = shared
        .runs
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    runs.get(id)
        .map(|run| snapshot_run(id, run, true))
        .ok_or_else(|| anyhow!("run not found"))
}

fn start_run(shared: Arc<Shared>, request: WorkRequest) -> anyhow::Result<Value> {
    prune_tasks(&shared);
    let id = uuid::Uuid::new_v4().to_string();
    let operation = work_name(&request).to_string();
    let abandoned = Arc::new(AtomicBool::new(false));
    let ord = shared.run_seq.fetch_add(1, Ordering::Relaxed);
    {
        let mut runs = shared
            .runs
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let inflight = runs
            .values()
            .filter(|run| matches!(run.state.as_str(), "queued" | "running"))
            .count();
        if inflight >= MAX_INFLIGHT_RUNS {
            bail!("run queue is full ({MAX_INFLIGHT_RUNS} active or queued)");
        }
        runs.insert(
            id.clone(),
            RunRecord {
                ord,
                state: "queued".into(),
                operation,
                detail: None,
                result: None,
                error: None,
                abandoned: Arc::clone(&abandoned),
                job_key: None,
            },
        );
    }
    trim_runs(&shared);
    let snapshot = run_snapshot(&shared, &id)?;
    let run_id = id.clone();
    let task_shared = Arc::clone(&shared);
    let handle = tokio::spawn(async move {
        set_run(&task_shared, &run_id, "running", None, None);
        let outcome = execute_work(&task_shared, &run_id, request, Arc::clone(&abandoned)).await;
        let abandoned = abandoned.load(Ordering::Acquire);
        if abandoned {
            set_run(&task_shared, &run_id, "cancelled", None, None);
        } else {
            match outcome {
                Ok(value) => set_run(&task_shared, &run_id, "complete", Some(value), None),
                Err(error) => set_run(
                    &task_shared,
                    &run_id,
                    "failed",
                    None,
                    Some(error.to_string()),
                ),
            }
        }
        trim_runs(&task_shared);
    });
    shared
        .tasks
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .push(handle);
    Ok(snapshot)
}

fn prune_tasks(shared: &Shared) {
    shared
        .tasks
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .retain(|handle| !handle.is_finished());
}

fn trim_runs(shared: &Shared) {
    let mut runs = shared
        .runs
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    loop {
        let result_bytes: usize = runs
            .values()
            .filter_map(|run| run.result.as_ref())
            .map(|value| {
                serde_json::to_vec(value)
                    .map(|bytes| bytes.len())
                    .unwrap_or(0)
            })
            .sum();
        let over_count = runs.len() > MAX_RETAINED_RUNS;
        let over_bytes = result_bytes > MAX_RETAINED_RESULT_BYTES;
        if !over_count && !over_bytes {
            break;
        }
        let oldest = runs
            .iter()
            .filter(|(_, run)| matches!(run.state.as_str(), "complete" | "cancelled" | "failed"))
            .min_by_key(|(_, run)| run.ord)
            .map(|(id, _)| id.clone());
        let Some(id) = oldest else {
            break;
        };
        runs.remove(&id);
    }
}

async fn reap_background(shared: &Shared) {
    {
        let runs = shared
            .runs
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        for run in runs.values() {
            run.abandoned.store(true, Ordering::Release);
        }
    }
    stop_checks(shared);
    let handles = std::mem::take(
        &mut *shared
            .tasks
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()),
    );
    let deadline = Instant::now() + Duration::from_secs(5);
    for mut handle in handles {
        let remaining = deadline.saturating_duration_since(Instant::now());
        tokio::select! {
            _ = &mut handle => {}
            _ = tokio::time::sleep(remaining) => {
                handle.abort();
                let _ = handle.await;
            }
        }
    }
    shared.model.lock().await.take();
}

fn set_run(shared: &Shared, id: &str, state: &str, result: Option<Value>, error: Option<String>) {
    let mut runs = shared
        .runs
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let Some(run) = runs.get_mut(id) else {
        return;
    };
    if run.abandoned.load(Ordering::Acquire) && state != "cancelled" {
        return;
    }
    run.state = state.to_string();
    if let Some(result) = result {
        run.result = Some(result);
    }
    if let Some(error) = error {
        run.error = Some(error);
    }
}

fn cancel_run(shared: &Shared, id: &str) -> anyhow::Result<Value> {
    let mut runs = shared
        .runs
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let Some(run) = runs.get_mut(id) else {
        bail!("run not found");
    };
    run.abandoned.store(true, Ordering::Release);
    run.state = "cancelled".into();
    let key = run.job_key.clone();
    let snapshot = snapshot_run(id, run, true);
    drop(runs);
    if let Some(key) = key {
        let jobs = shared
            .inflight
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(job) = jobs.get(&key)
            && job.sync_waiters.load(Ordering::Acquire) == 0
            && job.run_waiters.load(Ordering::Acquire) <= 1
        {
            job.stop.store(true, Ordering::Relaxed);
        }
    }
    Ok(snapshot)
}

async fn execute_work(
    shared: &Shared,
    run_id: &str,
    request: WorkRequest,
    abandoned: Arc<AtomicBool>,
) -> anyhow::Result<Value> {
    if abandoned.load(Ordering::Acquire) {
        bail!("cancelled");
    }
    match request {
        WorkRequest::Check(check) => {
            let key = serde_json::to_string(&check)?;
            {
                let mut runs = shared
                    .runs
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                if let Some(run) = runs.get_mut(run_id) {
                    run.job_key = Some(key);
                }
            }
            let report = dedup_check(shared, check, true, Some(abandoned)).await?;
            Ok(serde_json::to_value(report)?)
        }
        WorkRequest::Evidence { id } => evidence_any(shared, &id).await,
        WorkRequest::Compare { request } => run_compare(shared, request, abandoned).await,
        WorkRequest::Replay { kind, id } => run_replay(shared, kind, &id, abandoned).await,
        WorkRequest::Trace { request } => run_trace(shared, request, abandoned).await,
        WorkRequest::ModelSetup { offline } => run_model_setup(shared, offline, abandoned).await,
        WorkRequest::ModelEvaluate { request } => {
            run_model_evaluate(shared, request, abandoned).await
        }
        WorkRequest::Semantic { request } => run_semantic(shared, request, abandoned).await,
    }
}

async fn evidence_any(shared: &Shared, id: &str) -> anyhow::Result<Value> {
    let id_owned = id.to_string();
    let found = engine_call(shared, move |engine| engine.evidence(&id_owned)).await?;
    if let Some(report) = found {
        return Ok(serde_json::to_value(report)?);
    }
    if let Some(report) = crate::trace::evidence(&shared.workspace.root, id).await? {
        return Ok(serde_json::to_value(report)?);
    }
    let root = shared.workspace.root.clone();
    let semantic_id = id.to_string();
    if let Some(report) =
        tokio::task::spawn_blocking(move || crate::semantic::evidence(&root, &semantic_id))
            .await
            .context("semantic evidence task")??
    {
        return Ok(serde_json::to_value(report)?);
    }
    bail!("evidence expired or not found")
}

async fn trace_page(
    shared: &Shared,
    id: &str,
    offset: usize,
    limit: usize,
) -> anyhow::Result<Value> {
    let Some(report) =
        crate::trace::evidence_page(&shared.workspace.root, id, offset, limit).await?
    else {
        bail!("trace evidence not found")
    };
    Ok(serde_json::to_value(report)?)
}

async fn run_compare(
    shared: &Shared,
    request: Value,
    cancel: Arc<AtomicBool>,
) -> anyhow::Result<Value> {
    let parsed: crate::compare::CompareRequest =
        serde_json::from_value(request).context("compare request does not match CompareRequest")?;
    let report = crate::compare::compare(&shared.workspace.root, &parsed, cancel).await?;
    Ok(serde_json::to_value(report)?)
}

async fn run_replay(
    shared: &Shared,
    kind: ReplayKind,
    id: &str,
    cancel: Arc<AtomicBool>,
) -> anyhow::Result<Value> {
    match kind {
        ReplayKind::Compare => {
            let report = crate::compare::replay(&shared.workspace.root, id, cancel).await?;
            Ok(serde_json::to_value(report)?)
        }
        ReplayKind::Trace => {
            let report = crate::trace::replay(&shared.workspace.root, id, cancel).await?;
            Ok(serde_json::to_value(report)?)
        }
    }
}

async fn run_trace(
    shared: &Shared,
    request: Value,
    cancel: Arc<AtomicBool>,
) -> anyhow::Result<Value> {
    let parsed: crate::trace::TraceRequest =
        serde_json::from_value(request).context("trace request does not match TraceRequest")?;
    let report = crate::trace::trace(&shared.workspace.root, &parsed, cancel).await?;
    Ok(serde_json::to_value(report)?)
}

async fn run_semantic(
    shared: &Shared,
    request: Value,
    cancel: Arc<AtomicBool>,
) -> anyhow::Result<Value> {
    let timeout_ms = request
        .pointer("/limits/timeout_ms")
        .and_then(Value::as_u64)
        .unwrap_or(30_000);
    let parsed: crate::semantic::SemanticCheckRequest = serde_json::from_value(request)
        .context("semantic request does not match SemanticCheckRequest")?;
    let deadline = Instant::now() + Duration::from_millis(timeout_ms);
    let mut provider = refresh_provider(shared, &cancel, deadline).await?;
    let report = crate::semantic::check(
        &shared.workspace.root,
        &parsed,
        provider.as_mut().expect("model provider"),
        cancel,
    )
    .await?;
    Ok(serde_json::to_value(report)?)
}

async fn run_model_setup(
    shared: &Shared,
    offline: bool,
    cancel: Arc<AtomicBool>,
) -> anyhow::Result<Value> {
    if cancel.load(Ordering::Acquire) {
        bail!("cancelled");
    }
    let loaded = crate::models::load_model_config(&shared.workspace.root)?;
    let value = crate::models::setup_cancellable(&loaded.config, offline, cancel.as_ref()).await?;
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut provider = lock_model(shared, &cancel, deadline).await?;
    provider.take();
    if cancel.load(Ordering::Acquire) {
        bail!("cancelled");
    }
    Ok(value)
}

async fn run_model_evaluate(
    shared: &Shared,
    request: Value,
    cancel: Arc<AtomicBool>,
) -> anyhow::Result<Value> {
    #[derive(serde::Deserialize)]
    struct EvaluateBody {
        states: Vec<crate::models::ModelState>,
        questions: Vec<crate::models::ModelQuestion>,
        #[serde(default = "default_model_timeout")]
        timeout_ms: u64,
    }
    fn default_model_timeout() -> u64 {
        180_000
    }
    let body: EvaluateBody =
        serde_json::from_value(request).context("model evaluate request is invalid")?;
    let deadline = Instant::now() + Duration::from_millis(body.timeout_ms);
    let mut provider = refresh_provider(shared, &cancel, deadline).await?;
    let response = provider
        .as_mut()
        .expect("model provider")
        .evaluate(body.states, body.questions, body.timeout_ms, cancel)
        .await?;
    Ok(serde_json::to_value(response)?)
}

async fn lock_model<'a>(
    shared: &'a Shared,
    cancel: &AtomicBool,
    deadline: Instant,
) -> anyhow::Result<tokio::sync::MutexGuard<'a, Option<crate::models::ModelProvider>>> {
    loop {
        if cancel.load(Ordering::Acquire) {
            bail!("cancelled while waiting for the model provider");
        }
        if Instant::now() >= deadline {
            bail!("timed out waiting for the model provider");
        }
        let slice = deadline
            .saturating_duration_since(Instant::now())
            .min(Duration::from_millis(200));
        match tokio::time::timeout(slice, shared.model.lock()).await {
            Ok(guard) => return Ok(guard),
            Err(_) => continue,
        }
    }
}

async fn refresh_provider<'a>(
    shared: &'a Shared,
    cancel: &AtomicBool,
    deadline: Instant,
) -> anyhow::Result<tokio::sync::MutexGuard<'a, Option<crate::models::ModelProvider>>> {
    let loaded = crate::models::load_model_config(&shared.workspace.root)?;
    let mut guard = lock_model(shared, cancel, deadline).await?;
    let stale = match guard.as_ref() {
        None => true,
        Some(provider) => {
            provider.source_fingerprint() != loaded.source_fingerprint
                || provider.settings_fingerprint() != loaded.settings_fingerprint
        }
    };
    if stale {
        guard.take();
        if cancel.load(Ordering::Acquire) {
            bail!("cancelled");
        }
        *guard = Some(crate::models::ModelProvider::new(&shared.workspace.root).await?);
    }
    Ok(guard)
}

fn write_identity(path: &Path, identity: &DaemonIdentity) -> anyhow::Result<()> {
    let mut bytes = serde_json::to_vec_pretty(identity)?;
    bytes.push(b'\n');
    workspace::write_atomic(path, &bytes)
}

fn read_identity(path: &Path) -> Option<DaemonIdentity> {
    let text = fs::read_to_string(path).ok()?;
    serde_json::from_str(&text).ok()
}

fn identity_matches(path: &Path, instance: &str) -> bool {
    read_identity(path)
        .map(|identity| identity.instance == instance)
        .unwrap_or(false)
}

fn daemon_lock_held(workspace: &Workspace) -> bool {
    let path = workspace.state_dir.join("daemon.lock");
    // flock is per-process on Linux: probing with try_lock and then unlock from
    // the daemon process would release the server lock. Read the kernel table
    // instead, and only fall back to try_lock when that table is unavailable.
    #[cfg(target_os = "linux")]
    {
        linux_flock_held(&path)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let Ok(file) = OpenOptions::new().read(true).write(true).open(&path) else {
            return false;
        };
        match file.try_lock_exclusive() {
            Ok(()) => {
                let _ = fs2::FileExt::unlock(&file);
                false
            }
            Err(error) if lock_contended(&error) => true,
            Err(_) => false,
        }
    }
}

#[cfg(target_os = "linux")]
fn linux_flock_held(path: &Path) -> bool {
    let Ok(metadata) = fs::metadata(path) else {
        return false;
    };
    let Ok(text) = fs::read_to_string("/proc/locks") else {
        let Ok(file) = OpenOptions::new().read(true).write(true).open(path) else {
            return false;
        };
        return match file.try_lock_exclusive() {
            Ok(()) => {
                let _ = fs2::FileExt::unlock(&file);
                false
            }
            Err(error) if lock_contended(&error) => true,
            Err(_) => false,
        };
    };
    use std::os::unix::fs::MetadataExt;
    let inode = metadata.ino();
    text.lines().any(|line| {
        let mut parts = line.split_whitespace();
        let _id = parts.next();
        if parts.next() != Some("FLOCK") {
            return false;
        }
        let _advisory = parts.next();
        let _mode = parts.next();
        let _pid = parts.next();
        parts
            .next()
            .and_then(|device| device.rsplit(':').next())
            .and_then(|inode_text| inode_text.parse::<u64>().ok())
            == Some(inode)
    })
}

fn lock_contended(error: &std::io::Error) -> bool {
    let contended = fs2::lock_contended_error();
    error.kind() == contended.kind() || error.raw_os_error() == contended.raw_os_error()
}

fn same_path(left: &Path, right: &Path) -> bool {
    fn canon(path: &Path) -> PathBuf {
        path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
    }
    canon(left) == canon(right)
}

fn path_string(path: &Path) -> anyhow::Result<String> {
    path.to_str()
        .map(str::to_string)
        .ok_or_else(|| anyhow!("path is not utf-8: {}", path.display()))
}

#[cfg(unix)]
fn pid_alive(pid: u32) -> bool {
    if pid == 0 {
        return false;
    }
    unsafe extern "C" {
        fn kill(pid: i32, sig: i32) -> i32;
    }
    let rc = unsafe { kill(pid as i32, 0) };
    if rc == 0 {
        return true;
    }
    std::io::Error::last_os_error().raw_os_error() == Some(1)
}

#[cfg(windows)]
fn pid_alive(pid: u32) -> bool {
    if pid == 0 {
        return false;
    }
    unsafe extern "system" {
        fn OpenProcess(access: u32, inherit: i32, pid: u32) -> *mut std::ffi::c_void;
        fn CloseHandle(handle: *mut std::ffi::c_void) -> i32;
    }
    const PROCESS_QUERY_LIMITED_INFORMATION: u32 = 0x1000;
    unsafe {
        let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if handle.is_null() {
            return false;
        }
        CloseHandle(handle);
        true
    }
}

fn trim_log(path: &Path) {
    let Ok(metadata) = fs::metadata(path) else {
        return;
    };
    if metadata.len() <= LOG_LIMIT {
        return;
    }
    let Ok(data) = fs::read(path) else {
        return;
    };
    let start = data.len().saturating_sub(LOG_KEEP);
    let _ = fs::write(path, &data[start..]);
}

fn log_line(path: &Path, message: &str) {
    trim_log(path);
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    if let Ok(mut file) = OpenOptions::new().create(true).append(true).open(path) {
        let _ = writeln!(file, "{message}");
    }
}
