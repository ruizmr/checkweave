//! Python execution evidence.
//!
//! `trace` runs a workspace script as `__main__` with JSON on stdin and records
//! call, line, return, and exception events for workspace files. Events carry
//! the bytecode and source line that actually ran. Sequence is observation
//! order on a thread, not a cause. Native calls, subprocesses, and async task
//! scheduling are outside this adapter. The process-group timeout and cancel
//! path is the execute adapter; it is not a sandbox.

use anyhow::{Context, Result, bail, ensure};
use fs2::FileExt;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    collections::BTreeMap,
    fs::{self, File},
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::{Arc, OnceLock, atomic::AtomicBool},
    time::SystemTime,
};

pub const TRACE_OPERATOR_VERSION: &str = "trace-python-v1";
pub const BASIS_DIRECT_OBSERVATION: &str = "direct_observation";
pub const CONTAINMENT: &str = "process-group timeout and cancellation through the execute adapter; inherited environment; not a sandbox";

const UNSUPPORTED: &[&str] = &[
    "native_calls",
    "subprocess_events",
    "async_scheduling",
    "threads_started_before_the_hook",
    "event_order_is_not_causal",
    "not_sandboxed",
];
const MAX_RETAINED_TRACES: usize = 32;
const MAX_RETAINED_BYTES: u64 = 64 * 1024 * 1024;
/// Returned reports must fit the daemon IPC frame.
pub const MAX_IPC_BYTES: usize = 8 * 1024 * 1024;
/// Returned trace JSON, before the daemon frame and run snapshot wrappers.
pub const TRACE_RESPONSE_MAX: usize = 6 * 1024 * 1024;
/// Events included in a fresh trace response. Later events stay in `events.jsonl`.
pub const INITIAL_EVENT_PAGE: usize = 64;
/// Captured JSONL plus one event line stays within this total.
pub const MAX_CAPTURED_EVENT_BYTES: usize = 8 * 1024 * 1024;
const MAX_EVENT_LINE: usize = 64 * 1024;
pub const MAX_SOURCE_FILE_BYTES: u64 = 8 * 1024 * 1024;
pub const MAX_SOURCE_TOTAL_BYTES: u64 = 8 * 1024 * 1024;
pub const MAX_SOURCE_FILES: usize = 32;
const EMBEDDED_HELPER: &[u8] = include_bytes!("../python/checkweave_trace.py");

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct TraceLimits {
    pub timeout_ms: u64,
    pub max_events: usize,
    pub max_value_bytes: usize,
    pub max_output_bytes: usize,
}

impl Default for TraceLimits {
    fn default() -> Self {
        Self {
            timeout_ms: 5_000,
            max_events: 5_000,
            max_value_bytes: 4_096,
            max_output_bytes: 65_536,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TraceRequest {
    /// Workspace-relative Python script, executed as `__main__`.
    pub script: String,
    /// JSON value written to the script's stdin.
    pub input: Value,
    /// Record events only from these function names when non-empty.
    #[serde(default)]
    pub functions: Vec<String>,
    /// Record events only from these workspace-relative paths (or prefixes) when non-empty.
    #[serde(default)]
    pub paths: Vec<String>,
    #[serde(default)]
    pub limits: TraceLimits,
    /// Measure one untraced run and one traced run. The script executes twice.
    #[serde(default)]
    pub baseline: bool,
}

impl TraceRequest {
    pub fn new(script: impl Into<String>, input: Value) -> Self {
        Self {
            script: script.into(),
            input,
            functions: Vec::new(),
            paths: Vec::new(),
            limits: TraceLimits::default(),
            baseline: false,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct TracedSource {
    pub path: String,
    pub fingerprint: String,
    pub bytes: u64,
    pub retained: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TraceEvent {
    pub path: String,
    pub line: u32,
    pub function: String,
    pub event: String,
    #[serde(default)]
    pub locals: BTreeMap<String, Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<Value>,
    pub code_hash: String,
    pub line_hash: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct TraceReport {
    pub id: String,
    /// complete, partial, failed, cancelled, or timeout
    pub execution: String,
    /// Always `direct_observation` for this adapter.
    pub basis: String,
    /// validated, stale, or unknown. A run that saw source bytes change is never validated.
    pub freshness: String,
    pub operator_version: String,
    pub generation: String,
    pub script: String,
    pub sources: Vec<TracedSource>,
    pub events: Vec<TraceEvent>,
    pub dropped: u64,
    pub elapsed_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub baseline_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub overhead_ms: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub baseline_us: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub traced_us: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub overhead_us: Option<i64>,
    pub stdout: String,
    pub stderr: String,
    pub stdout_truncated: bool,
    /// Bytes of the captured stdout not included in `stdout`. The file is kept.
    #[serde(default)]
    pub stdout_omitted_bytes: u64,
    /// Bytes of the captured stderr not included in `stderr`. The file is kept.
    #[serde(default)]
    pub stderr_omitted_bytes: u64,
    pub warnings: Vec<String>,
    pub modified_during_run: bool,
    /// Workspace files compiled during the run whose bytes were not retained.
    #[serde(default)]
    pub sources_dropped: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub replay_of: Option<String>,
    pub unsupported: Vec<String>,
    pub containment: String,
    #[serde(default)]
    pub event_offset: usize,
    #[serde(default)]
    pub event_total: usize,
}

#[derive(Debug, Deserialize)]
struct TracerResult {
    #[serde(default = "failed_status")]
    status: String,
    #[serde(default)]
    error: Option<String>,
    #[serde(default)]
    dropped: u64,
    #[serde(default)]
    stdout_truncated: bool,
    #[serde(default)]
    stderr_truncated: bool,
    #[serde(default)]
    modified_during_run: bool,
    #[serde(default)]
    sources_dropped: u64,
    #[serde(default)]
    baseline_us: Option<u64>,
    #[serde(default)]
    traced_us: Option<u64>,
}

fn failed_status() -> String {
    "failed".into()
}

#[derive(Serialize)]
struct TracerConfig<'a> {
    workspace: &'a str,
    script: &'a str,
    script_path: &'a str,
    events_path: &'a str,
    stdout_path: &'a str,
    stderr_path: &'a str,
    result_path: &'a str,
    snapshot_dir: &'a str,
    max_events: usize,
    max_event_bytes: usize,
    max_value_bytes: usize,
    max_output_bytes: usize,
    max_source_file_bytes: u64,
    max_source_total_bytes: u64,
    max_source_files: usize,
    functions: &'a [String],
    paths: &'a [String],
    baseline: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    replay_snapshot: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    replay_overlay: Option<&'a str>,
}

pub async fn trace(
    root: &Path,
    request: &TraceRequest,
    cancel: Arc<AtomicBool>,
) -> Result<TraceReport> {
    trace_with(root, request, cancel, None, None).await
}

pub async fn evidence(root: &Path, id: &str) -> Result<Option<TraceReport>> {
    evidence_page(root, id, 0, usize::MAX).await
}

/// Return a page of stored events. `evidence` is the first page.
/// The page is shortened until the report JSON fits [`TRACE_RESPONSE_MAX`].
/// Freshness is recomputed from current workspace bytes. A report marked
/// `modified_during_run` stays stale.
pub async fn evidence_page(
    root: &Path,
    id: &str,
    offset: usize,
    limit: usize,
) -> Result<Option<TraceReport>> {
    let root = root.canonicalize().context("canonicalize workspace")?;
    let id = id.to_string();
    tokio::task::spawn_blocking(move || evidence_page_sync(&root, &id, offset, limit))
        .await
        .context("evidence task")?
}

fn evidence_page_sync(
    root: &Path,
    id: &str,
    offset: usize,
    limit: usize,
) -> Result<Option<TraceReport>> {
    let dir = trace_dir(root, id)?;
    let path = dir.join("report.json");
    if !path.is_file() {
        return Ok(None);
    }
    let bytes = read_bounded(&path, MAX_IPC_BYTES as u64)?;
    let mut report: TraceReport = serde_json::from_slice(&bytes)
        .with_context(|| format!("read trace report {}", path.display()))?;
    let (events, total) = load_event_page(&dir.join("events.jsonl"), offset, limit)?;
    report.events = events;
    report.event_total = total;
    report.event_offset = offset.min(total);
    report.freshness = freshness_of(root, report.modified_during_run, &report.sources);
    fit_ipc(&mut report);
    Ok(Some(report))
}

pub async fn replay(root: &Path, id: &str, cancel: Arc<AtomicBool>) -> Result<TraceReport> {
    let root = root.canonicalize().context("canonicalize workspace")?;
    let dir = trace_dir(&root, id)?;
    let request_path = dir.join("request.json");
    if !request_path.is_file() {
        if dir.join("report.json").is_file() {
            bail!("trace {id} has no retained request");
        }
        bail!("trace evidence not found");
    }
    let mut request: TraceRequest = serde_json::from_slice(&fs::read(&request_path)?)?;
    request.baseline = false;
    let snapshot = dir.join("snapshot").join(&request.script);
    ensure!(
        snapshot.is_file(),
        "snapshot bytes for {} were not retained",
        request.script
    );
    let overlay = dir.join("snapshot");
    trace_with(&root, &request, cancel, Some(id.to_string()), Some(overlay)).await
}

async fn trace_with(
    root: &Path,
    request: &TraceRequest,
    cancel: Arc<AtomicBool>,
    replay_of: Option<String>,
    overlay: Option<PathBuf>,
) -> Result<TraceReport> {
    let root = root.canonicalize().context("canonicalize workspace")?;
    let script = normalize_rel(&request.script)?;
    validate_request(request)?;
    let script_abs = crate::execute::resolve_source(&root, &script)?;
    ensure!(script_abs.is_file(), "script must be a regular file");
    let script_for_hash = script_abs.clone();
    let before = tokio::task::spawn_blocking(move || {
        fingerprint_file(&script_for_hash, MAX_SOURCE_FILE_BYTES)
    })
    .await
    .context("fingerprint task")??;

    let id = uuid::Uuid::new_v4().simple().to_string();
    let dir = root.join(".checkweave").join("traces").join(&id);
    let snapshot_dir = dir.join("snapshot");
    fs::create_dir_all(&snapshot_dir)?;
    if let Some(parent) = Path::new(&script).parent()
        && !parent.as_os_str().is_empty()
    {
        fs::create_dir_all(snapshot_dir.join(parent))?;
    }
    let snapshot_file = snapshot_dir.join(&script);
    if let Some(overlay) = &overlay {
        let from = overlay.join(&script);
        ensure!(from.is_file(), "replay snapshot missing for {script}");
        let from_c = from.clone();
        let to_c = snapshot_file.clone();
        tokio::task::spawn_blocking(move || copy_bounded(&from_c, &to_c, MAX_SOURCE_FILE_BYTES))
            .await
            .context("snapshot copy")??;
    } else {
        let from_c = script_abs.clone();
        let to_c = snapshot_file.clone();
        tokio::task::spawn_blocking(move || copy_bounded(&from_c, &to_c, MAX_SOURCE_FILE_BYTES))
            .await
            .context("snapshot copy")??;
    }

    let events_path = dir.join("events.jsonl");
    let stdout_path = dir.join("stdout.txt");
    let stderr_path = dir.join("stderr.txt");
    let result_path = dir.join("result.json");
    let config_path = dir.join("config.json");
    let workspace = root.to_string_lossy().to_string();
    let script_path = script_abs.to_string_lossy().to_string();
    let events_s = events_path.to_string_lossy().to_string();
    let stdout_s = stdout_path.to_string_lossy().to_string();
    let stderr_s = stderr_path.to_string_lossy().to_string();
    let result_s = result_path.to_string_lossy().to_string();
    let snapshot_s = snapshot_dir.to_string_lossy().to_string();
    let replay_snapshot = overlay.as_ref().map(|_| snapshot_dir.join(&script));
    let replay_snapshot_s = replay_snapshot
        .as_ref()
        .map(|p| p.to_string_lossy().to_string());
    let overlay_s = overlay.as_ref().map(|p| p.to_string_lossy().to_string());
    let config = TracerConfig {
        workspace: &workspace,
        script: &script,
        script_path: &script_path,
        events_path: &events_s,
        stdout_path: &stdout_s,
        stderr_path: &stderr_s,
        result_path: &result_s,
        snapshot_dir: &snapshot_s,
        max_events: request.limits.max_events,
        max_event_bytes: MAX_CAPTURED_EVENT_BYTES,
        max_value_bytes: request.limits.max_value_bytes,
        max_output_bytes: request.limits.max_output_bytes,
        max_source_file_bytes: MAX_SOURCE_FILE_BYTES,
        max_source_total_bytes: MAX_SOURCE_TOTAL_BYTES,
        max_source_files: MAX_SOURCE_FILES,
        functions: &request.functions,
        paths: &request.paths,
        baseline: request.baseline && replay_of.is_none(),
        replay_snapshot: replay_snapshot_s.as_deref(),
        replay_overlay: overlay_s.as_deref(),
    };
    write_atomic(&config_path, &serde_json::to_vec_pretty(&config)?)?;

    let python = python_program()?;
    let helper = helper_script()?;
    let mut env = BTreeMap::new();
    env.insert(
        "CHECKWEAVE_TRACE_CONFIG".into(),
        config_path.to_string_lossy().to_string(),
    );
    env.insert("PYTHONUNBUFFERED".into(), "1".into());
    let target = crate::execute::Target {
        argv: vec![python, helper.to_string_lossy().to_string()],
        cwd: ".".into(),
        env,
        sources: vec![script.clone()],
    };
    let pipe_budget = request
        .limits
        .max_output_bytes
        .clamp(8 * 1024, 4 * 1024 * 1024);
    let limits = crate::execute::ExecutionLimits {
        timeout_ms: request.limits.timeout_ms,
        max_output_bytes: pipe_budget,
    };
    let observed = crate::execute::run(&root, &target, &request.input, &limits, cancel).await?;
    let script_for_after = script_abs.clone();
    let after = tokio::task::spawn_blocking(move || {
        fingerprint_file(&script_for_after, MAX_SOURCE_FILE_BYTES)
    })
    .await
    .context("fingerprint task")?
    .ok();
    let changed_during = after
        .as_ref()
        .map(|item| item.0 != before.0)
        .unwrap_or(true);

    let tracer = read_tracer_result(&result_path);
    let (
        mut execution,
        tracer_status,
        dropped,
        mut stdout_truncated,
        stderr_truncated,
        mut modified,
        sources_dropped,
        baseline_us,
        traced_us,
        tracer_error,
    ) = if let Some(tracer) = tracer {
        let process = match observed.outcome.as_str() {
            "timeout" | "cancelled" | "output_limit" => observed.outcome.clone(),
            _ => "completed".into(),
        };
        (
            process,
            tracer.status,
            tracer.dropped,
            tracer.stdout_truncated,
            tracer.stderr_truncated,
            tracer.modified_during_run,
            tracer.sources_dropped,
            tracer.baseline_us,
            tracer.traced_us,
            tracer.error,
        )
    } else {
        (
            observed.outcome.clone(),
            String::new(),
            0,
            false,
            false,
            false,
            0,
            None,
            None,
            None,
        )
    };
    if changed_during {
        modified = true;
    }
    execution = classify(
        &execution,
        &tracer_status,
        dropped,
        stdout_truncated || stderr_truncated,
        sources_dropped > 0,
    );

    let (events, mut warnings) = load_events(&events_path, request.limits.max_events)?;
    if tracer_status.is_empty()
        && matches!(execution.as_str(), "complete" | "partial")
        && !result_path.is_file()
    {
        execution = "failed".into();
        warnings.push("tracer result was not written".into());
    }
    if let Some(error) = tracer_error {
        warnings.push(format!("script status {tracer_status}: {error}"));
    }
    if request.baseline && replay_of.is_none() {
        warnings.push(
            "baseline executed the script once without the tracer and once with it in the same process; imported modules stay loaded"
                .into(),
        );
    }
    if replay_of.is_some() {
        warnings.push("replay executed retained snapshot bytes; the current workspace file was not the executed source".into());
    }
    if dropped > 0 {
        warnings.push(format!(
            "{dropped} events were dropped for the event or value budget"
        ));
    }
    if sources_dropped > 0 {
        warnings.push(format!(
            "{sources_dropped} source files were not retained; per-file limit is {MAX_SOURCE_FILE_BYTES} bytes, total {MAX_SOURCE_TOTAL_BYTES} bytes, at most {MAX_SOURCE_FILES} files"
        ));
    }
    if stdout_truncated {
        warnings.push("stdout reached max_output_bytes".into());
    }
    if observed.outcome != "completed"
        && observed.outcome != "timeout"
        && observed.outcome != "cancelled"
    {
        warnings.push(format!("process outcome {}", observed.outcome));
    }
    if !observed.stderr.is_empty() && !stderr_path.is_file() {
        warnings.push("process stderr was captured on the execute pipe".into());
    }

    let snapshot_for_sources = snapshot_dir.clone();
    let (sources, walk_omitted) =
        tokio::task::spawn_blocking(move || collect_sources(&snapshot_for_sources))
            .await
            .context("snapshot scan")??;
    if walk_omitted > 0 && sources_dropped == 0 {
        warnings.push(format!(
            "{walk_omitted} retained snapshots were skipped while scanning"
        ));
    }
    let generation = generation_of(&sources);
    let stdout_path_c = stdout_path.clone();
    let stderr_path_c = stderr_path.clone();
    let output_budget = request.limits.max_output_bytes;
    let ((stdout, stdout_cut), (mut stderr, _)) = tokio::task::spawn_blocking(move || {
        (
            read_text_limited(&stdout_path_c, output_budget),
            read_text_limited(&stderr_path_c, output_budget),
        )
    })
    .await
    .context("read captured output")?;
    if stdout_cut {
        stdout_truncated = true;
    }
    if stderr.is_empty() && !observed.stderr.is_empty() {
        stderr = truncate_utf8(&observed.stderr, request.limits.max_output_bytes);
    }

    let (baseline_us, traced_us) = if request.baseline && replay_of.is_none() {
        (baseline_us, traced_us)
    } else {
        (None, None)
    };
    let overhead_us = match (baseline_us, traced_us) {
        (Some(base), Some(traced)) => Some(traced as i64 - base as i64),
        _ => None,
    };
    let elapsed_ms = observed.elapsed_ms;
    let event_total = events.len();

    let mut report = TraceReport {
        id: id.clone(),
        execution,
        basis: BASIS_DIRECT_OBSERVATION.into(),
        freshness: "unknown".into(),
        operator_version: TRACE_OPERATOR_VERSION.into(),
        generation,
        script: script.clone(),
        sources,
        events,
        dropped,
        elapsed_ms,
        baseline_ms: baseline_us.map(|us| us / 1000),
        overhead_ms: overhead_us.map(|us| us / 1000),
        baseline_us,
        traced_us,
        overhead_us,
        stdout,
        stderr,
        stdout_truncated,
        stdout_omitted_bytes: 0,
        stderr_omitted_bytes: 0,
        warnings,
        modified_during_run: modified,
        sources_dropped,
        replay_of: replay_of.clone(),
        unsupported: UNSUPPORTED.iter().map(|item| (*item).to_string()).collect(),
        containment: CONTAINMENT.into(),
        event_offset: 0,
        event_total,
    };
    let fresh_root = root.clone();
    let fresh_modified = report.modified_during_run;
    let fresh_sources = report.sources.clone();
    report.freshness = tokio::task::spawn_blocking(move || {
        freshness_of(&fresh_root, fresh_modified, &fresh_sources)
    })
    .await
    .context("freshness task")?;
    if report.events.len() > INITIAL_EVENT_PAGE {
        let shown = INITIAL_EVENT_PAGE;
        let next = shown;
        report.events.truncate(shown);
        report.warnings.push(format!(
            "showing {shown} of {event_total} stored events (offset 0); next page: checkweave evidence {id} --offset {next} --limit {shown}"
        ));
    }
    fit_ipc(&mut report);
    let mut stored_request = request.clone();
    stored_request.script = script;
    if replay_of.is_some() {
        stored_request.baseline = false;
    }
    publish(&root, &dir, &report, &stored_request)?;
    Ok(report)
}

fn classify(
    process: &str,
    tracer_status: &str,
    dropped: u64,
    truncated: bool,
    sources_omitted: bool,
) -> String {
    match process {
        "cancelled" => "cancelled",
        "timeout" => "timeout",
        _ if tracer_status == "failed" || process == "failed" => "failed",
        "output_limit" => "partial",
        _ if dropped > 0 || truncated || sources_omitted => "partial",
        _ => "complete",
    }
    .into()
}

fn publish(root: &Path, dir: &Path, report: &TraceReport, request: &TraceRequest) -> Result<()> {
    write_atomic(
        &dir.join("request.json"),
        &serde_json::to_vec_pretty(request)?,
    )?;
    let encoded = serde_json::to_vec(report)?;
    ensure!(
        encoded.len() <= TRACE_RESPONSE_MAX,
        "trace report is {} bytes; response limit is {TRACE_RESPONSE_MAX}",
        encoded.len()
    );
    write_atomic(&dir.join("report.json"), &encoded)?;
    let keep = report.id.clone();
    if let Err(err) = with_trace_lock(root, || {
        prune(&root.join(".checkweave").join("traces"), &keep)
    }) {
        // The published report is still readable if retention cleanup fails.
        let _ = err;
    }
    Ok(())
}

fn freshness_of(root: &Path, modified_during_run: bool, sources: &[TracedSource]) -> String {
    if modified_during_run {
        return "stale".into();
    }
    if sources.is_empty() {
        return "unknown".into();
    }
    for source in sources {
        match fingerprint_workspace(root, &source.path) {
            Ok((hash, _)) if hash == source.fingerprint => {}
            _ => return "stale".into(),
        }
    }
    "validated".into()
}

fn validate_request(request: &TraceRequest) -> Result<()> {
    let limits = &request.limits;
    ensure!(
        (1..=300_000).contains(&limits.timeout_ms),
        "timeout_ms must be 1..=300000"
    );
    ensure!(
        (1..=100_000).contains(&limits.max_events),
        "max_events must be 1..=100000"
    );
    ensure!(
        (1..=1_048_576).contains(&limits.max_value_bytes),
        "max_value_bytes must be 1..=1048576"
    );
    ensure!(
        (1..=4_194_304).contains(&limits.max_output_bytes),
        "max_output_bytes must be 1..=4194304"
    );
    ensure!(request.functions.len() <= 64, "at most 64 function filters");
    ensure!(request.paths.len() <= 64, "at most 64 path filters");
    for name in &request.functions {
        ensure!(
            name.len() <= 256 && !name.is_empty(),
            "invalid function filter"
        );
    }
    for path in &request.paths {
        normalize_rel(path)?;
    }
    Ok(())
}

fn normalize_rel(path: &str) -> Result<String> {
    ensure!(
        !Path::new(path).is_absolute(),
        "path must be workspace-relative"
    );
    let path = path.replace('\\', "/");
    let path = path.trim_start_matches("./");
    ensure!(
        !path.is_empty() && !path.starts_with('/'),
        "path must be workspace-relative"
    );
    ensure!(!path.contains('\0'), "path contains NUL");
    ensure!(path.len() <= 4096, "path is too long");
    ensure!(
        !path.split('/').any(|part| part == ".."),
        "path may not contain '..'"
    );
    ensure!(
        !path
            .split('/')
            .any(|part| part == ".git" || part == ".checkweave"),
        "internal state cannot be traced"
    );
    Ok(path.to_string())
}

fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 80
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

fn trace_dir(root: &Path, id: &str) -> Result<PathBuf> {
    ensure!(valid_id(id), "invalid trace id");
    Ok(root.join(".checkweave").join("traces").join(id))
}

fn fingerprint_file(path: &Path, max_bytes: u64) -> Result<(String, u64)> {
    let meta = fs::metadata(path).with_context(|| format!("stat {}", path.display()))?;
    ensure!(
        meta.is_file() && meta.len() <= max_bytes,
        "{} is {} bytes; source limit is {max_bytes}",
        path.display(),
        meta.len()
    );
    let mut file = File::open(path).with_context(|| format!("read {}", path.display()))?;
    let mut hasher = blake3::Hasher::new();
    let mut buf = [0u8; 64 * 1024];
    let mut total = 0u64;
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        total += n as u64;
        ensure!(
            total <= max_bytes,
            "{} exceeds {max_bytes} bytes",
            path.display()
        );
        hasher.update(&buf[..n]);
    }
    Ok((hasher.finalize().to_hex().to_string(), total))
}

fn copy_bounded(from: &Path, to: &Path, max_bytes: u64) -> Result<u64> {
    let (_, len) = fingerprint_file(from, max_bytes)?;
    if let Some(parent) = to.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut input = File::open(from)?;
    let mut output = File::create(to)?;
    let mut buf = [0u8; 64 * 1024];
    let mut copied = 0u64;
    loop {
        let n = input.read(&mut buf)?;
        if n == 0 {
            break;
        }
        copied += n as u64;
        ensure!(
            copied <= max_bytes,
            "snapshot copy exceeds {max_bytes} bytes"
        );
        output.write_all(&buf[..n])?;
    }
    output.sync_all()?;
    ensure!(
        copied == len,
        "snapshot copy did not match the source length"
    );
    Ok(copied)
}

fn fingerprint_workspace(root: &Path, relative: &str) -> Result<(String, u64)> {
    let relative = normalize_rel(relative)?;
    let path = crate::execute::resolve_source(root, &relative)?;
    fingerprint_file(&path, MAX_SOURCE_FILE_BYTES)
}

fn collect_sources(snapshot_dir: &Path) -> Result<(Vec<TracedSource>, u64)> {
    let mut sources = Vec::new();
    let mut omitted = 0u64;
    let mut total = 0u64;
    if snapshot_dir.is_dir() {
        walk_snapshots(
            snapshot_dir,
            snapshot_dir,
            &mut sources,
            &mut total,
            &mut omitted,
        )?;
    }
    sources.sort_by(|a, b| a.path.cmp(&b.path));
    Ok((sources, omitted))
}

fn walk_snapshots(
    root: &Path,
    dir: &Path,
    out: &mut Vec<TracedSource>,
    total: &mut u64,
    omitted: &mut u64,
) -> Result<()> {
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let meta = entry.metadata()?;
        if meta.is_dir() {
            walk_snapshots(root, &entry.path(), out, total, omitted)?;
            continue;
        }
        if !meta.is_file() {
            continue;
        }
        if meta.len() > MAX_SOURCE_FILE_BYTES
            || out.len() >= MAX_SOURCE_FILES
            || total.saturating_add(meta.len()) > MAX_SOURCE_TOTAL_BYTES
        {
            *omitted += 1;
            continue;
        }
        let rel = entry
            .path()
            .strip_prefix(root)?
            .to_string_lossy()
            .replace('\\', "/");
        if rel
            .split('/')
            .any(|part| part == ".git" || part == ".checkweave")
        {
            continue;
        }
        let (fingerprint, bytes) = fingerprint_file(&entry.path(), MAX_SOURCE_FILE_BYTES)?;
        *total += bytes;
        out.push(TracedSource {
            path: rel,
            fingerprint,
            bytes,
            retained: true,
        });
    }
    Ok(())
}

fn generation_of(sources: &[TracedSource]) -> String {
    let mut material = String::new();
    for source in sources {
        material.push_str(&source.path);
        material.push('\0');
        material.push_str(&source.fingerprint);
        material.push('\n');
    }
    blake3::hash(material.as_bytes()).to_hex().to_string()
}

fn load_events(path: &Path, max_events: usize) -> Result<(Vec<TraceEvent>, Vec<String>)> {
    let (events, _, warnings) = read_events(path, 0, max_events, true)?;
    Ok((events, warnings))
}

fn load_event_page(path: &Path, offset: usize, limit: usize) -> Result<(Vec<TraceEvent>, usize)> {
    let (events, total, _) = read_events(path, offset, limit, false)?;
    Ok((events, total))
}

fn read_events(
    path: &Path,
    offset: usize,
    limit: usize,
    warn: bool,
) -> Result<(Vec<TraceEvent>, usize, Vec<String>)> {
    let mut file = match File::open(path) {
        Ok(file) => file,
        Err(_) => return Ok((Vec::new(), 0, Vec::new())),
    };
    let meta_len = file.metadata().map(|meta| meta.len()).unwrap_or(0);
    let mut kept = Vec::new();
    let mut pending: Vec<u8> = Vec::new();
    let mut read_bytes = 0usize;
    let mut seen = 0usize;
    let mut warnings = Vec::new();
    let mut buf = [0u8; 8192];
    let mut stopped = false;
    while read_bytes < MAX_CAPTURED_EVENT_BYTES {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        let room = MAX_CAPTURED_EVENT_BYTES - read_bytes;
        let take = n.min(room);
        pending.extend_from_slice(&buf[..take]);
        read_bytes += take;
        if take < n {
            stopped = true;
            break;
        }
        while let Some(split) = pending.iter().position(|byte| *byte == b'\n') {
            let line: Vec<u8> = pending.drain(..=split).collect();
            let line = &line[..line.len().saturating_sub(1)];
            if line.is_empty() {
                continue;
            }
            if line.len() > MAX_EVENT_LINE {
                if warn {
                    warnings.push("skipped an event line over 64 KiB".into());
                }
                continue;
            }
            let text = String::from_utf8_lossy(line);
            match serde_json::from_str::<TraceEvent>(text.trim_end()) {
                Ok(event)
                    if matches!(
                        event.event.as_str(),
                        "call" | "line" | "return" | "exception"
                    ) =>
                {
                    if seen >= offset && kept.len() < limit {
                        kept.push(event);
                    }
                    seen += 1;
                }
                Ok(_) if warn => warnings.push(format!("ignored unsupported event after {seen}")),
                Err(err) if warn => warnings.push(format!("skipped malformed event: {err}")),
                _ => {}
            }
        }
    }
    if !pending.is_empty() && warn {
        warnings.push("trailing partial event line ignored".into());
    }
    if (stopped || meta_len > MAX_CAPTURED_EVENT_BYTES as u64) && warn {
        warnings.push(format!(
            "event capture stopped at {MAX_CAPTURED_EVENT_BYTES} bytes"
        ));
    }
    Ok((kept, seen, warnings))
}

fn read_tracer_result(path: &Path) -> Option<TracerResult> {
    let bytes = read_bounded(path, 1024 * 1024).ok()?;
    serde_json::from_slice(&bytes).ok()
}

fn read_bounded(path: &Path, max_bytes: u64) -> Result<Vec<u8>> {
    let meta = fs::metadata(path).with_context(|| format!("stat {}", path.display()))?;
    ensure!(
        meta.len() <= max_bytes,
        "{} is {} bytes; limit is {max_bytes}",
        path.display(),
        meta.len()
    );
    let file = File::open(path)?;
    let mut buf = Vec::new();
    file.take(max_bytes).read_to_end(&mut buf)?;
    Ok(buf)
}

fn read_text_limited(path: &Path, limit: usize) -> (String, bool) {
    let limit = limit.min(MAX_IPC_BYTES);
    let Ok(meta) = fs::metadata(path) else {
        return (String::new(), false);
    };
    let Ok(mut file) = File::open(path) else {
        return (String::new(), false);
    };
    let want = (meta.len() as usize).min(limit);
    let mut buf = vec![0u8; want];
    let mut read = 0;
    while read < want {
        match file.read(&mut buf[read..]) {
            Ok(0) => break,
            Ok(n) => read += n,
            Err(_) => break,
        }
    }
    let mut extra = [0u8; 1];
    let file_truncated =
        meta.len() > limit as u64 || file.read(&mut extra).ok().is_some_and(|n| n > 0);
    let lossy = String::from_utf8_lossy(&buf[..read]);
    let text = truncate_utf8(lossy.as_ref(), limit);
    let cut = file_truncated || text.len() < lossy.len();
    (text, cut)
}

fn truncate_utf8(text: &str, limit: usize) -> String {
    if text.len() <= limit {
        return text.to_string();
    }
    let mut end = limit;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    text[..end].to_string()
}

fn fit_ipc(report: &mut TraceReport) {
    let total = report.event_total;
    let offset = report.event_offset;
    let all = std::mem::take(&mut report.events);
    let fits = |report: &TraceReport| {
        serde_json::to_vec(report)
            .map(|bytes| bytes.len())
            .unwrap_or(usize::MAX)
            <= TRACE_RESPONSE_MAX
    };
    if !all.is_empty() {
        report.events = all.clone();
    }
    if !fits(report) {
        let mut lo = 0;
        let mut hi = all.len();
        while lo < hi {
            let mid = lo + (hi - lo).div_ceil(2);
            report.events = all[..mid].to_vec();
            if fits(report) {
                lo = mid;
            } else if mid == 0 {
                break;
            } else {
                hi = mid - 1;
            }
        }
        report.events = all[..lo].to_vec();
        if lo < all.len() {
            report.warnings.push(format!(
                "returned {lo} of {total} stored events so the report fits the {TRACE_RESPONSE_MAX} byte response budget; next page: checkweave evidence <id> --offset {} --limit {lo}",
                offset + lo
            ));
        }
    }
    report.event_total = total;
    report.event_offset = offset;
    if fits(report) {
        return;
    }
    let stdout_before = report.stdout.len();
    let stderr_before = report.stderr.len();
    report.stdout = truncate_utf8(&report.stdout, 64 * 1024);
    report.stderr = truncate_utf8(&report.stderr, 64 * 1024);
    report.stdout_omitted_bytes = report
        .stdout_omitted_bytes
        .saturating_add((stdout_before - report.stdout.len()) as u64);
    report.stderr_omitted_bytes = report
        .stderr_omitted_bytes
        .saturating_add((stderr_before - report.stderr.len()) as u64);
    if report.stdout_omitted_bytes > 0 || report.stderr_omitted_bytes > 0 {
        report.stdout_truncated = true;
    }
    report.warnings.push(format!(
        "stdout and stderr in the report were shortened to fit the response budget (stdout omitted {} bytes, stderr omitted {} bytes); retained captures are unchanged",
        report.stdout_omitted_bytes, report.stderr_omitted_bytes
    ));
}

fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("tmp");
    {
        let mut file = File::create(&tmp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
    }
    fs::rename(&tmp, path)?;
    Ok(())
}

fn python_program() -> Result<String> {
    if let Ok(raw) = std::env::var("CHECKWEAVE_TRACE_PYTHON") {
        ensure!(!raw.is_empty(), "CHECKWEAVE_TRACE_PYTHON is empty");
        return Ok(raw);
    }
    static FOUND: OnceLock<String> = OnceLock::new();
    if let Some(found) = FOUND.get() {
        return Ok(found.clone());
    }
    for name in ["python3", "python"] {
        let ok = std::process::Command::new(name)
            .arg("--version")
            .output()
            .ok()
            .is_some_and(|output| output.status.success());
        if ok {
            let _ = FOUND.set(name.to_string());
            return Ok(name.to_string());
        }
    }
    bail!("Python runtime not found; set CHECKWEAVE_TRACE_PYTHON to a Python 3 executable");
}

fn helper_script() -> Result<PathBuf> {
    if let Some(raw) = std::env::var_os("CHECKWEAVE_TRACE_HELPER") {
        let path = PathBuf::from(raw);
        ensure!(
            path.is_file(),
            "CHECKWEAVE_TRACE_HELPER is not a file: {}",
            path.display()
        );
        return Ok(path);
    }
    let dir = cache_root()?
        .join("trace-helper")
        .join(TRACE_OPERATOR_VERSION);
    materialize_helper(&dir)
}

fn cache_root() -> Result<PathBuf> {
    if let Some(dir) = std::env::var_os("CHECKWEAVE_CACHE_DIR") {
        ensure!(!dir.is_empty(), "CHECKWEAVE_CACHE_DIR is empty");
        return Ok(PathBuf::from(dir));
    }
    if let Some(xdg) = std::env::var_os("XDG_CACHE_HOME")
        && !xdg.is_empty()
    {
        return Ok(PathBuf::from(xdg).join("checkweave"));
    }
    let home = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .context("HOME is not set; set CHECKWEAVE_CACHE_DIR or CHECKWEAVE_TRACE_HELPER")?;
    Ok(PathBuf::from(home).join(".cache").join("checkweave"))
}

fn materialize_helper(dir: &Path) -> Result<PathBuf> {
    fs::create_dir_all(dir)?;
    let dest = dir.join("checkweave_trace.py");
    if dest.is_file()
        && fs::metadata(&dest)
            .ok()
            .is_some_and(|meta| meta.len() == EMBEDDED_HELPER.len() as u64)
        && read_bounded(&dest, EMBEDDED_HELPER.len() as u64)
            .ok()
            .as_deref()
            == Some(EMBEDDED_HELPER)
    {
        return Ok(dest);
    }
    let tmp = dir.join(format!("checkweave_trace.py.{}.tmp", std::process::id()));
    {
        let mut file = File::create(&tmp)?;
        file.write_all(EMBEDDED_HELPER)?;
        file.sync_all()?;
    }
    fs::rename(&tmp, &dest)?;
    Ok(dest)
}

fn with_trace_lock(root: &Path, body: impl FnOnce() -> Result<()>) -> Result<()> {
    fs::create_dir_all(root.join(".checkweave"))?;
    let lock_path = root.join(".checkweave").join("trace.lock");
    let file = File::options()
        .create(true)
        .truncate(false)
        .write(true)
        .open(lock_path)?;
    file.lock_exclusive()?;
    let result = body();
    let _ = fs2::FileExt::unlock(&file);
    result
}

fn prune(dir: &Path, keep: &str) -> Result<()> {
    if !dir.is_dir() {
        return Ok(());
    }
    let mut entries = Vec::new();
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().to_string();
        if !valid_id(&name) || !entry.path().join("report.json").is_file() {
            continue;
        }
        let modified = entry
            .metadata()?
            .modified()
            .unwrap_or(SystemTime::UNIX_EPOCH);
        let size = dir_size(&entry.path());
        entries.push((modified, name, entry.path(), size));
    }
    entries.sort_by_key(|item| item.0);
    let mut total: u64 = entries.iter().map(|item| item.3).sum();
    while entries.len() > MAX_RETAINED_TRACES || (total > MAX_RETAINED_BYTES && entries.len() > 1) {
        let index = entries.iter().position(|item| item.1 != keep).unwrap_or(0);
        if entries[index].1 == keep {
            break;
        }
        let (_, _, path, size) = entries.remove(index);
        let _ = fs::remove_dir_all(path);
        total = total.saturating_sub(size);
    }
    Ok(())
}

fn dir_size(path: &Path) -> u64 {
    let mut total = 0u64;
    let mut stack = vec![path.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let entries = match fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(_) => continue,
        };
        for entry in entries.flatten() {
            let meta = match fs::symlink_metadata(entry.path()) {
                Ok(meta) => meta,
                Err(_) => continue,
            };
            if meta.is_symlink() {
                continue;
            }
            if meta.is_dir() {
                stack.push(entry.path());
            } else {
                total = total.saturating_add(meta.len());
            }
        }
    }
    total
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedded_helper_materializes_without_the_source_tree() {
        let dir = tempfile::tempdir().unwrap();
        let path = materialize_helper(dir.path()).unwrap();
        assert_eq!(fs::read(&path).unwrap(), EMBEDDED_HELPER);
        let source = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("python/checkweave_trace.py");
        assert_ne!(path.canonicalize().unwrap(), source.canonicalize().unwrap());
        let again = materialize_helper(dir.path()).unwrap();
        assert_eq!(again, path);
    }

    #[test]
    fn lossy_stdout_truncates_on_a_char_boundary() {
        let raw = vec![0xffu8; 10];
        let lossy = String::from_utf8_lossy(&raw);
        assert!(lossy.len() > 10);
        let text = truncate_utf8(lossy.as_ref(), 10);
        assert!(text.len() <= 10);
        assert!(text.is_char_boundary(text.len()));
        assert!(text.chars().all(|ch| ch == '\u{FFFD}'));
    }

    #[test]
    fn response_leaves_room_for_wire_and_run_wrappers() {
        let mut report = TraceReport {
            id: "t".into(),
            execution: "partial".into(),
            basis: "direct_observation".into(),
            freshness: "validated".into(),
            operator_version: "v".into(),
            generation: "g".into(),
            script: "s.py".into(),
            sources: Vec::new(),
            events: Vec::new(),
            dropped: 0,
            elapsed_ms: 1,
            baseline_ms: None,
            overhead_ms: None,
            baseline_us: None,
            traced_us: None,
            overhead_us: None,
            stdout: "y".repeat(7 * 1024 * 1024),
            stderr: String::new(),
            stdout_truncated: false,
            stdout_omitted_bytes: 0,
            stderr_omitted_bytes: 0,
            warnings: Vec::new(),
            modified_during_run: false,
            sources_dropped: 0,
            replay_of: None,
            unsupported: Vec::new(),
            containment: "process".into(),
            event_offset: 0,
            event_total: 0,
        };
        fit_ipc(&mut report);
        let body = serde_json::to_vec(&report).unwrap();
        assert!(body.len() <= TRACE_RESPONSE_MAX, "{}", body.len());
        assert!(report.stdout_omitted_bytes > 0);
        let wire = crate::types::WireResponse::success(&report).unwrap();
        let frame = serde_json::to_vec(&wire).unwrap();
        assert!(frame.len() <= MAX_IPC_BYTES, "wire {}", frame.len());
        let snapshot = serde_json::json!({
            "id": report.id,
            "state": "complete",
            "operation": "trace",
            "result": report,
        });
        let wrapped = serde_json::to_vec(&snapshot).unwrap();
        assert!(wrapped.len() <= MAX_IPC_BYTES, "run {}", wrapped.len());
    }
}
