//! Bounded JSON-in/JSON-out process adapter.
//!
//! A command runs only when [`run`] is called. Filesystem events never start a
//! process. Arguments are passed to `exec` directly: this module does not
//! invoke a shell and does not interpolate metacharacters.
//!
//! A working directory and, on Unix, a process group are not a sandbox. The
//! command can read and write the host. Inherited environment variables and
//! other external state are untracked.

use anyhow::{Context, Result, ensure};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    collections::BTreeMap,
    io::Read,
    path::{Component, Path, PathBuf},
    process::Stdio,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWriteExt},
    process::Command,
    task::JoinHandle,
};

pub const DEPENDENCY_COMPLETENESS: &str = "declared_sources_only; inherited environment and external state untracked; worktree is not a sandbox; commands run only on explicit request";

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Target {
    /// Executable and arguments, passed directly without a shell.
    pub argv: Vec<String>,
    #[serde(default = "default_cwd")]
    pub cwd: String,
    /// Explicit environment overrides. Other variables are inherited and untracked.
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// Workspace-relative files to fingerprint and retain for reproduction.
    #[serde(default)]
    pub sources: Vec<String>,
}

fn default_cwd() -> String {
    ".".into()
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct ExecutionLimits {
    pub timeout_ms: u64,
    pub max_output_bytes: usize,
}

impl Default for ExecutionLimits {
    fn default() -> Self {
        Self {
            timeout_ms: 5_000,
            max_output_bytes: 256 * 1024,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct Observation {
    /// completed, failed, unsupported_output, timeout, cancelled, output_limit, containment_failure.
    pub outcome: String,
    pub exit_code: Option<i32>,
    pub output: Option<Value>,
    pub stdout: String,
    pub stderr: String,
    pub elapsed_ms: u64,
    pub source_fingerprints: BTreeMap<String, String>,
    pub source_freshness: String,
    pub dependency_completeness: String,
    /// How child processes were contained on this host. Not a sandbox claim.
    #[serde(default)]
    pub containment: String,
    /// BLAKE3 of the parsed JSON object. Present even when a later report truncates `output`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_fingerprint: Option<String>,
}

/// Unix kills the spawned command's process group. Windows terminates only the
/// direct child (`kill_on_drop`); descendant processes are not contained.
/// This workspace's tests run on the host OS only.
pub fn containment_model() -> &'static str {
    #[cfg(unix)]
    {
        "unix_process_group_sigkill; descendants in the spawned group are killed with the leader; not a sandbox; verified only on hosts that ran the suite"
    }
    #[cfg(windows)]
    {
        "windows_direct_child_only; kill_on_drop does not contain descendant processes; Job Objects are not enabled; not tested on Windows; not a sandbox"
    }
}

pub fn validate_workspace_relative(relative: &str) -> Result<()> {
    let path = Path::new(relative);
    ensure!(!path.is_absolute(), "source/cwd must be workspace-relative");
    ensure!(
        !path
            .components()
            .any(|component| matches!(component, Component::ParentDir)),
        "source/cwd may not contain '..'"
    );
    ensure!(
        !path.components().any(|component| {
            let name = component.as_os_str();
            name == ".git" || name == ".checkweave"
        }),
        "internal state cannot be an execution source/cwd"
    );
    Ok(())
}

pub fn resolve_source(root: &Path, relative: &str) -> Result<PathBuf> {
    validate_workspace_relative(relative)?;
    let root = root.canonicalize().context("resolve workspace root")?;
    let resolved = root
        .join(relative)
        .canonicalize()
        .context("resolve source/cwd")?;
    ensure!(resolved.starts_with(&root), "source/cwd escapes workspace");
    Ok(resolved)
}

pub fn fingerprint_file(root: &Path, relative: &str, remaining: &mut u64) -> Result<String> {
    let path = resolve_source(root, relative)?;
    ensure!(path.is_file(), "declared source must be a regular file");
    let file = std::fs::File::open(path)?;
    let mut bytes = Vec::new();
    file.take(*remaining + 1).read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() as u64 <= *remaining,
        "declared source budget exceeds 16 MiB"
    );
    *remaining -= bytes.len() as u64;
    Ok(blake3::hash(&bytes).to_hex().to_string())
}

pub fn source_fingerprints(root: &Path, target: &Target) -> Result<BTreeMap<String, String>> {
    ensure!(
        target.sources.len() <= 128,
        "at most 128 declared source files"
    );
    let mut sources = BTreeMap::new();
    let mut remaining = 16 * 1024 * 1024;
    for source in &target.sources {
        let fingerprint = fingerprint_file(root, source, &mut remaining)?;
        sources.insert(source.clone(), fingerprint);
    }
    Ok(sources)
}

/// Armed until the group has been signalled. Killing after the leader exits
/// also stops descendants that inherited the group. `kill(-pgid)` fails with
/// ESRCH when the group is already empty; a recycled pid is a narrow race
/// shared by process-group supervision, not a sandbox boundary.
struct ProcessGroup {
    #[cfg(unix)]
    pid: Option<u32>,
    armed: bool,
}

impl ProcessGroup {
    fn arm(pid: Option<u32>) -> Self {
        #[cfg(not(unix))]
        let _ = pid;
        Self {
            #[cfg(unix)]
            pid,
            armed: true,
        }
    }

    fn kill(&mut self) {
        if !self.armed {
            return;
        }
        self.armed = false;
        #[cfg(unix)]
        if let Some(pid) = self.pid {
            // SAFETY: the child was spawned with process_group(0), so its pid is the pgid.
            // A negative pid selects that group. The call is ignored when the group is gone.
            unsafe {
                libc::kill(-(pid as i32), libc::SIGKILL);
            }
        }
    }
}

impl Drop for ProcessGroup {
    fn drop(&mut self) {
        self.kill();
    }
}

struct AbortOnDrop<T>(JoinHandle<T>);

impl<T> Drop for AbortOnDrop<T> {
    fn drop(&mut self) {
        self.0.abort();
    }
}

async fn capture(
    mut reader: impl AsyncRead + Unpin,
    limit: usize,
    exceeded: Arc<AtomicBool>,
) -> Result<Vec<u8>> {
    let mut result = Vec::new();
    let mut buffer = [0; 8192];
    loop {
        let n = reader.read(&mut buffer).await?;
        if n == 0 {
            break;
        }
        let available = limit.saturating_sub(result.len());
        result.extend_from_slice(&buffer[..n.min(available)]);
        if n > available {
            exceeded.store(true, Ordering::Release);
            break;
        }
    }
    Ok(result)
}

#[allow(clippy::too_many_arguments)]
fn observation(
    outcome: &str,
    exit_code: Option<i32>,
    output: Option<Value>,
    stdout: String,
    stderr: String,
    started: Instant,
    source_fingerprints: BTreeMap<String, String>,
    fresh: bool,
) -> Observation {
    let output_fingerprint = output.as_ref().and_then(|value| {
        serde_json::to_vec(value)
            .ok()
            .map(|bytes| blake3::hash(&bytes).to_hex().to_string())
    });
    Observation {
        outcome: outcome.into(),
        exit_code,
        output,
        stdout,
        stderr,
        elapsed_ms: started.elapsed().as_millis() as u64,
        source_fingerprints,
        source_freshness: if fresh { "validated" } else { "stale" }.into(),
        dependency_completeness: DEPENDENCY_COMPLETENESS.into(),
        containment: containment_model().into(),
        output_fingerprint,
    }
}

pub async fn run(
    root: &Path,
    target: &Target,
    input: &Value,
    limits: &ExecutionLimits,
    cancel: Arc<AtomicBool>,
) -> Result<Observation> {
    ensure!(
        !target.argv.is_empty() && target.argv.len() <= 128,
        "target argv requires 1..128 entries"
    );
    ensure!(
        target.argv.iter().map(String::len).sum::<usize>() <= 65536,
        "target arguments exceed 64 KiB"
    );
    ensure!(!target.argv[0].is_empty(), "target executable is empty");
    ensure!(
        target.env.len() <= 128
            && target
                .env
                .iter()
                .map(|(k, v)| k.len() + v.len())
                .sum::<usize>()
                <= 65536,
        "target environment overrides exceed budget"
    );
    ensure!(
        target
            .env
            .keys()
            .all(|key| !key.is_empty() && !key.contains('=') && !key.contains('\0')),
        "invalid environment variable name"
    );
    ensure!(
        (1..=300_000).contains(&limits.timeout_ms),
        "execution timeout must be 1..300000 ms"
    );
    ensure!(
        (1..=4 * 1024 * 1024).contains(&limits.max_output_bytes),
        "output budget must be 1 byte..4 MiB per stream"
    );
    let root = root.canonicalize()?;
    let cwd = resolve_source(&root, &target.cwd)?;
    ensure!(cwd.is_dir(), "target cwd must be a directory");
    let source_fingerprints = source_fingerprints(&root, target)?;
    let mut bytes = serde_json::to_vec(input)?;
    ensure!(bytes.len() <= 1024 * 1024, "execution input exceeds 1 MiB");
    bytes.push(b'\n');
    let started = Instant::now();
    if cancel.load(Ordering::Acquire) {
        return Ok(observation(
            "cancelled",
            None,
            None,
            String::new(),
            String::new(),
            started,
            source_fingerprints,
            true,
        ));
    }

    let mut command = Command::new(&target.argv[0]);
    command
        .args(&target.argv[1..])
        .current_dir(&cwd)
        .envs(&target.env)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    #[cfg(unix)]
    command.process_group(0);
    let mut child = command
        .spawn()
        .with_context(|| format!("start target {}", target.argv[0]))?;
    let mut group = ProcessGroup::arm(child.id());
    let mut stdin = child.stdin.take().context("target stdin")?;
    let stdin_task = AbortOnDrop(tokio::spawn(async move {
        let _ = stdin.write_all(&bytes).await;
        let _ = stdin.shutdown().await;
    }));
    let exceeded = Arc::new(AtomicBool::new(false));
    let mut stdout_task = AbortOnDrop(tokio::spawn(capture(
        child.stdout.take().context("target stdout")?,
        limits.max_output_bytes,
        exceeded.clone(),
    )));
    let mut stderr_task = AbortOnDrop(tokio::spawn(capture(
        child.stderr.take().context("target stderr")?,
        limits.max_output_bytes,
        exceeded.clone(),
    )));

    let deadline = tokio::time::sleep(Duration::from_millis(limits.timeout_ms));
    tokio::pin!(deadline);
    let mut poll = tokio::time::interval(Duration::from_millis(5));
    poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut outcome = "completed";
    let status = loop {
        tokio::select! {
            result = child.wait() => { break Some(result?); },
            _ = &mut deadline => { outcome = "timeout"; break None; },
            _ = poll.tick() => {
                if cancel.load(Ordering::Acquire) { outcome = "cancelled"; break None; }
                if exceeded.load(Ordering::Acquire) { outcome = "output_limit"; break None; }
            }
        }
    };

    // Stop descendants before draining. Bytes the leader already wrote stay in
    // the pipe; a grandchild holding the pipe open must not stall capture.
    group.kill();
    if status.is_none() {
        let _ = child.kill().await;
        let _ = tokio::time::timeout(Duration::from_secs(1), child.wait()).await;
    }
    drop(stdin_task);

    let streams = tokio::time::timeout(Duration::from_secs(2), async {
        let stdout = (&mut stdout_task.0).await??;
        let stderr = (&mut stderr_task.0).await??;
        Ok::<_, anyhow::Error>((stdout, stderr))
    })
    .await;
    let (stdout, stderr) = match streams {
        Ok(result) => result?,
        Err(_) => {
            drop(stdout_task);
            drop(stderr_task);
            if outcome == "completed" {
                outcome = "containment_failure";
            }
            (Vec::new(), Vec::new())
        }
    };

    if exceeded.load(Ordering::Acquire) && outcome == "completed" {
        outcome = "output_limit";
    }
    if outcome == "completed" && !status.as_ref().is_some_and(|status| status.success()) {
        outcome = "failed";
    }
    // Parse a single JSON document only when the process exited and the stream
    // was not cut by the output budget. A parser failure on a successful exit
    // is unsupported_output. A crashing process that did not emit JSON stays
    // failed and is not treated as a semantic JSON result. Trailing non-whitespace
    // is rejected by serde_json, so a prefix of a larger stream is not accepted.
    let mut output = None;
    if matches!(outcome, "completed" | "failed") {
        match serde_json::from_slice::<Value>(&stdout) {
            Ok(value) => output = Some(value),
            Err(_) if outcome == "completed" => outcome = "unsupported_output",
            Err(_) => {}
        }
    }
    let current = self::source_fingerprints(&root, target);
    let fresh = current.is_ok_and(|current| current == source_fingerprints);
    Ok(observation(
        outcome,
        status.and_then(|status| status.code()),
        output,
        String::from_utf8_lossy(&stdout).into_owned(),
        String::from_utf8_lossy(&stderr).into_owned(),
        started,
        source_fingerprints,
        fresh,
    ))
}
