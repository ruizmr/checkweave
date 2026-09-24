//! Checkweave command line. Operations print one JSON document on stdout.
//! Diagnostics go to stderr. Evaluation stays in the daemon.

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;

use anyhow::Context;
use clap::{ArgGroup, Args, Parser, Subcommand, ValueEnum};
use serde_json::Value;

use checkweave::daemon;
use checkweave::mcp::{self, InvalidInput};
use checkweave::types::{CheckRequest, Limits, Predicate, ReplayKind, Request, WorkRequest};
use checkweave::workspace::Workspace;

#[derive(Parser)]
#[command(
    name = "checkweave",
    version,
    about = "Local incremental checks for JSON Lines collections",
    arg_required_else_help = true
)]
struct Cli {
    /// Workspace root. Defaults to the current directory.
    #[arg(long, global = true, value_name = "PATH", default_value = ".")]
    workspace: PathBuf,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Create local state and configure an agent integration.
    Init {
        /// Agent integration to configure.
        #[arg(long, value_enum, default_value_t = Agent::Cursor)]
        agent: Agent,
    },
    /// Check JSON Lines records. A match is success; partial and unresolved results stay in the JSON.
    Check(CheckArgs),
    /// Print retained evidence for an id from check, or a later trace event page.
    Evidence {
        /// Evidence id returned by check or trace. Not a filesystem path.
        id: String,
        /// Stored trace events to skip. Check evidence ignores this when it is 0 and limit is omitted.
        #[arg(long, default_value_t = 0)]
        offset: usize,
        /// Maximum trace events in this page. Omit it to keep the previous evidence-by-id behavior.
        #[arg(long)]
        limit: Option<usize>,
    },
    /// Print workspace and worker status.
    Status,
    /// Ask the workspace worker to shut down.
    Shutdown,
    /// Remove the managed Cursor integration. Keeps workspace state, custom rules, and unrelated config.
    #[command(visible_alias = "uninit")]
    Deinit,
    /// Compare before and after behavior from a JSON request file.
    Compare {
        /// JSON file containing a CompareRequest object.
        #[arg(long, value_name = "PATH")]
        request_file: PathBuf,
    },
    /// Replay retained compare or trace evidence.
    Replay {
        /// Which retained run to replay.
        #[arg(long, value_enum)]
        kind: ReplayArg,
        /// Evidence id returned by the original run.
        #[arg(long)]
        id: String,
    },
    /// Judge JSON Lines records with the configured model.
    Semantic {
        /// JSON file containing a SemanticCheckRequest object.
        #[arg(long, value_name = "PATH")]
        request_file: PathBuf,
    },
    /// Capture Python execution evidence from a JSON request file.
    Trace {
        /// JSON file containing a TraceRequest object.
        #[arg(long, value_name = "PATH")]
        request_file: PathBuf,
    },
    /// Configure or call the decision model.
    Model {
        #[command(subcommand)]
        command: ModelCommand,
    },
    /// Start, inspect, or cancel a run handle.
    Run {
        #[command(subcommand)]
        command: RunCommand,
    },
    /// Serve the Checkweave MCP server on stdio.
    Mcp,
    /// Run the workspace worker until it is idle.
    #[command(hide = true)]
    Daemon {
        /// Shut down after this many seconds without work.
        #[arg(long, default_value_t = 300)]
        idle_seconds: u64,
    },
}

#[derive(Copy, Clone, Debug, Eq, PartialEq, ValueEnum)]
enum Agent {
    #[value(name = "cursor")]
    Cursor,
    #[value(name = "none")]
    None,
}

impl Agent {
    fn as_integration(self) -> &'static str {
        match self {
            Self::Cursor => "cursor",
            Self::None => "none",
        }
    }
}

#[derive(Copy, Clone, Debug, Eq, PartialEq, ValueEnum)]
enum ReplayArg {
    #[value(name = "compare")]
    Compare,
    #[value(name = "trace")]
    Trace,
}

impl ReplayArg {
    fn kind(self) -> ReplayKind {
        match self {
            Self::Compare => ReplayKind::Compare,
            Self::Trace => ReplayKind::Trace,
        }
    }
}

#[derive(Subcommand)]
enum ModelCommand {
    /// Install or reuse the configured model runtime.
    Setup {
        /// Use only a cache that is already present.
        #[arg(long)]
        offline: bool,
    },
    /// Evaluate typed decisions from a JSON request file.
    Evaluate {
        /// JSON object with states, questions, and timeout_ms.
        #[arg(long, value_name = "PATH")]
        request_file: PathBuf,
    },
}

#[derive(Subcommand)]
enum RunCommand {
    /// Start work and print a run handle without waiting for the result.
    Start {
        /// JSON Request object, excluding run_start, run_status, run_cancel, and shutdown.
        #[arg(long, value_name = "PATH")]
        request_file: PathBuf,
    },
    /// Print the current run snapshot.
    Status {
        /// Run id returned by run start.
        id: String,
    },
    /// Cancel one owned run. Shared deterministic checks keep running for other clients.
    Cancel {
        /// Run id returned by run start.
        id: String,
    },
}

impl std::fmt::Display for Agent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_integration())
    }
}

#[derive(Args)]
#[command(group(
    ArgGroup::new("predicate_source")
        .required(true)
        .args(["predicate", "predicate_file"])
))]
struct CheckArgs {
    /// Workspace-relative glob. Repeat for more than one pattern.
    #[arg(long, required = true, action = clap::ArgAction::Append, value_name = "GLOB")]
    include: Vec<String>,

    /// Predicate JSON object. Paths are JSON Pointers: empty string is the whole record, and `/status` is the status member. `/` is the empty-name member, not the record.
    #[arg(long, value_name = "JSON")]
    predicate: Option<String>,

    /// File containing predicate JSON.
    #[arg(long, value_name = "PATH")]
    predicate_file: Option<PathBuf>,

    /// Maximum files selected by the globs.
    #[arg(long, default_value_t = Limits::default().max_files, value_name = "N")]
    max_files: usize,

    /// Maximum bytes read across selected files.
    #[arg(long, default_value_t = Limits::default().max_bytes, value_name = "N")]
    max_bytes: u64,

    /// Maximum JSON Lines records to evaluate.
    #[arg(long, default_value_t = Limits::default().max_records, value_name = "N")]
    max_records: usize,

    /// Maximum matched or unresolved records returned in the result.
    #[arg(long, default_value_t = Limits::default().max_results, value_name = "N")]
    max_results: usize,

    /// Check deadline in milliseconds.
    #[arg(long, default_value_t = Limits::default().timeout_ms, value_name = "N")]
    timeout_ms: u64,
}

enum CliError {
    Usage(String),
    Runtime(anyhow::Error),
    Interrupted,
}

#[tokio::main]
async fn main() -> ExitCode {
    match run().await {
        Ok(()) => ExitCode::SUCCESS,
        Err(CliError::Usage(message)) => {
            eprintln!("checkweave: {message}");
            ExitCode::from(2)
        }
        Err(CliError::Runtime(err)) => {
            eprintln!("checkweave: {err:#}");
            ExitCode::from(1)
        }
        Err(CliError::Interrupted) => {
            eprintln!("checkweave: interrupted");
            ExitCode::from(130)
        }
    }
}

async fn run() -> Result<(), CliError> {
    let cli = Cli::parse();
    let mcp = matches!(cli.command, Command::Mcp);
    init_tracing(mcp);

    match cli.command {
        Command::Init { agent } => {
            let path = resolve_workspace(&cli.workspace, true)?;
            let label = path.display().to_string();
            let integration = agent.as_integration();
            let value = interrupt(async move {
                tokio::task::spawn_blocking(move || Workspace::initialize(&path, integration))
                    .await
                    .context("initialize task failed")?
                    .with_context(|| format!("failed to initialize {label}"))
            })
            .await?;
            write_json(&value)
        }
        Command::Check(args) => {
            let check = build_check(args)?;
            reject_invalid(&Request::Check(check.clone()))?;
            let workspace = open_workspace(&cli.workspace)?;
            // Submit a run so Ctrl-C cancels this client. A shared check that
            // another client is still waiting on keeps running.
            let value = run_owned(&workspace, WorkRequest::Check(check)).await?;
            write_json(&value)
        }
        Command::Evidence { id, offset, limit } => {
            let request = match limit {
                None if offset == 0 => Request::Evidence { id },
                limit => Request::TracePage {
                    id,
                    offset,
                    limit: limit.unwrap_or(checkweave::trace::INITIAL_EVENT_PAGE),
                },
            };
            reject_invalid(&request)?;
            let workspace = open_workspace(&cli.workspace)?;
            let value = interrupt(mcp::dispatch(&workspace, request)).await?;
            write_json(&value)
        }
        Command::Status => {
            let workspace = open_workspace(&cli.workspace)?;
            let value = interrupt(mcp::dispatch(&workspace, Request::Status)).await?;
            write_json(&value)
        }
        Command::Shutdown => {
            let workspace = open_workspace(&cli.workspace)?;
            let value = interrupt(mcp::dispatch(&workspace, Request::Shutdown)).await?;
            write_json(&value)
        }
        Command::Deinit => {
            let path = resolve_workspace(&cli.workspace, false)?;
            let value = interrupt(async move {
                if let Ok(workspace) = Workspace::discover(&path) {
                    daemon::shutdown_if_running(&workspace)
                        .await
                        .context("stop workspace daemon before deinit")?;
                }
                tokio::task::spawn_blocking(move || Workspace::remove_integration(&path))
                    .await
                    .context("deinit task failed")?
            })
            .await?;
            write_json(&value)
        }
        Command::Semantic { request_file } => {
            let request = load_request_object(&request_file)?;
            let workspace = open_workspace(&cli.workspace)?;
            let value = run_owned(&workspace, WorkRequest::Semantic { request }).await?;
            write_json(&value)
        }
        Command::Compare { request_file } => {
            let request = load_request_object(&request_file)?;
            let workspace = open_workspace(&cli.workspace)?;
            let value = run_owned(&workspace, WorkRequest::Compare { request }).await?;
            write_json(&value)
        }
        Command::Replay { kind, id } => {
            let workspace = open_workspace(&cli.workspace)?;
            let value = run_owned(
                &workspace,
                WorkRequest::Replay {
                    kind: kind.kind(),
                    id,
                },
            )
            .await?;
            write_json(&value)
        }
        Command::Trace { request_file } => {
            let request = load_request_object(&request_file)?;
            let workspace = open_workspace(&cli.workspace)?;
            let value = run_owned(&workspace, WorkRequest::Trace { request }).await?;
            write_json(&value)
        }
        Command::Model { command } => match command {
            ModelCommand::Setup { offline } => {
                let workspace = open_workspace(&cli.workspace)?;
                let value = run_owned(&workspace, WorkRequest::ModelSetup { offline }).await?;
                write_json(&value)
            }
            ModelCommand::Evaluate { request_file } => {
                let request = load_request_object(&request_file)?;
                let workspace = open_workspace(&cli.workspace)?;
                let value = run_owned(&workspace, WorkRequest::ModelEvaluate { request }).await?;
                write_json(&value)
            }
        },
        Command::Run { command } => match command {
            RunCommand::Start { request_file } => {
                let request = load_work_request(&request_file)?;
                let workspace = open_workspace(&cli.workspace)?;
                let value =
                    interrupt(mcp::dispatch(&workspace, Request::RunStart { request })).await?;
                write_json(&value)
            }
            RunCommand::Status { id } => {
                let request = Request::RunStatus { id };
                reject_invalid(&request)?;
                let workspace = open_workspace(&cli.workspace)?;
                let value = interrupt(mcp::dispatch(&workspace, request)).await?;
                write_json(&value)
            }
            RunCommand::Cancel { id } => {
                let request = Request::RunCancel { id };
                reject_invalid(&request)?;
                let workspace = open_workspace(&cli.workspace)?;
                let value = interrupt(mcp::dispatch(&workspace, request)).await?;
                write_json(&value)
            }
        },
        Command::Mcp => {
            let workspace = open_workspace(&cli.workspace)?;
            mcp::serve(workspace).await.map_err(CliError::Runtime)
        }
        Command::Daemon { idle_seconds } => {
            let workspace = open_workspace(&cli.workspace)?;
            tokio::select! {
                result = checkweave::daemon::serve(workspace, idle_seconds) => {
                    result.map_err(CliError::Runtime)
                }
                _ = ctrl_c() => Err(CliError::Interrupted),
            }
        }
    }
}

fn build_check(args: CheckArgs) -> Result<CheckRequest, CliError> {
    let predicate = load_predicate(&args)?;
    Ok(CheckRequest {
        include: args.include,
        predicate,
        limits: Limits {
            max_files: args.max_files,
            max_bytes: args.max_bytes,
            max_records: args.max_records,
            max_results: args.max_results,
            timeout_ms: args.timeout_ms,
        },
    })
}

fn load_predicate(args: &CheckArgs) -> Result<Predicate, CliError> {
    let (origin, raw) = if let Some(text) = &args.predicate {
        ("--predicate", text.clone())
    } else if let Some(path) = &args.predicate_file {
        if !path.is_file() {
            return Err(CliError::Usage(format!(
                "predicate file is not a file: {}",
                path.display()
            )));
        }
        let metadata = std::fs::metadata(path).map_err(|err| {
            CliError::Usage(format!(
                "cannot read predicate file {}: {err}",
                path.display()
            ))
        })?;
        if metadata.len() > 1024 * 1024 {
            return Err(CliError::Usage(format!(
                "predicate file exceeds 1 MiB: {}",
                path.display()
            )));
        }
        let text = std::fs::read_to_string(path).map_err(|err| {
            CliError::Usage(format!(
                "cannot read predicate file {}: {err}",
                path.display()
            ))
        })?;
        ("--predicate-file", text)
    } else {
        return Err(CliError::Usage(
            "pass exactly one of --predicate or --predicate-file".into(),
        ));
    };
    let raw = raw.trim();
    if raw.is_empty() {
        return Err(CliError::Usage(format!("{origin} JSON must not be empty")));
    }
    serde_json::from_str(raw).map_err(|err| {
        CliError::Usage(format!(
            "invalid predicate JSON in {origin}: {err}. Expected an object with an op field, such as {{\"op\":\"exists\",\"path\":\"/id\"}}"
        ))
    })
}

fn reject_invalid(request: &Request) -> Result<(), CliError> {
    mcp::validate_request(request).map_err(|InvalidInput(message)| CliError::Usage(message))
}

fn open_workspace(path: &Path) -> Result<Workspace, CliError> {
    let path = resolve_workspace(path, false)?;
    Workspace::discover(&path)
        .with_context(|| {
            format!(
                "cannot open workspace at {}. Run `checkweave init --workspace {}` first",
                path.display(),
                path.display()
            )
        })
        .map_err(CliError::Runtime)
}

fn resolve_workspace(path: &Path, allow_missing: bool) -> Result<PathBuf, CliError> {
    if path.as_os_str().is_empty() {
        return Err(CliError::Usage("workspace path must not be empty".into()));
    }
    if path.exists() {
        if !path.is_dir() {
            return Err(CliError::Usage(format!(
                "workspace path is not a directory: {}",
                path.display()
            )));
        }
        return path.canonicalize().map_err(|err| {
            CliError::Usage(format!(
                "cannot resolve workspace path {}: {err}",
                path.display()
            ))
        });
    }
    if !allow_missing {
        return Err(CliError::Usage(format!(
            "workspace path does not exist: {}",
            path.display()
        )));
    }
    if path.is_absolute() {
        Ok(path.to_path_buf())
    } else {
        std::env::current_dir()
            .map(|cwd| cwd.join(path))
            .map_err(|err| CliError::Runtime(err.into()))
    }
}

async fn interrupt<T>(
    fut: impl std::future::Future<Output = anyhow::Result<T>>,
) -> Result<T, CliError> {
    tokio::select! {
        result = fut => result.map_err(classify),
        _ = ctrl_c() => Err(CliError::Interrupted),
    }
}

fn classify(err: anyhow::Error) -> CliError {
    match err.downcast::<InvalidInput>() {
        Ok(InvalidInput(message)) => CliError::Usage(message),
        Err(err) => CliError::Runtime(err),
    }
}

async fn ctrl_c() {
    if tokio::signal::ctrl_c().await.is_err() {
        std::future::pending::<()>().await;
    }
}

const MAX_REQUEST_BYTES: u64 = 1024 * 1024;

fn load_request_object(path: &Path) -> Result<Value, CliError> {
    if !path.is_file() {
        return Err(CliError::Usage(format!(
            "request file is not a file: {}",
            path.display()
        )));
    }
    let metadata = std::fs::metadata(path).map_err(|err| {
        CliError::Usage(format!(
            "cannot read request file {}: {err}",
            path.display()
        ))
    })?;
    if metadata.len() > MAX_REQUEST_BYTES {
        return Err(CliError::Usage(format!(
            "request file exceeds 1 MiB: {}",
            path.display()
        )));
    }
    let text = std::fs::read_to_string(path).map_err(|err| {
        CliError::Usage(format!(
            "cannot read request file {}: {err}",
            path.display()
        ))
    })?;
    let value: Value = serde_json::from_str(text.trim()).map_err(|err| {
        CliError::Usage(format!("invalid request JSON in {}: {err}", path.display()))
    })?;
    if !value.is_object() {
        return Err(CliError::Usage("request JSON must be an object".into()));
    }
    Ok(value)
}

fn load_work_request(path: &Path) -> Result<WorkRequest, CliError> {
    let value = load_request_object(path)?;
    serde_json::from_value(value).map_err(|err| {
        CliError::Usage(format!(
            "invalid work request in {}: {err}. Expected an operation of check, evidence, compare, replay, trace, model_setup, model_evaluate, or semantic",
            path.display()
        ))
    })
}

/// Submit work, poll its handle, and cancel that handle on Ctrl-C.
/// A shared deterministic check is not cancelled for other clients; the daemon
/// owns that distinction.
async fn run_owned(workspace: &Workspace, work: WorkRequest) -> Result<Value, CliError> {
    let started = tokio::select! {
        result = mcp::dispatch(workspace, Request::RunStart { request: work }) => {
            result.map_err(classify)?
        }
        _ = ctrl_c() => return Err(CliError::Interrupted),
    };
    let id = started
        .get("id")
        .and_then(Value::as_str)
        .ok_or_else(|| {
            CliError::Runtime(anyhow::anyhow!("run start did not return an id: {started}"))
        })?
        .to_string();
    let mut delay_ms = 5u64;
    loop {
        tokio::select! {
            _ = ctrl_c() => {
                let _ = mcp::dispatch(workspace, Request::RunCancel { id: id.clone() }).await;
                return Err(CliError::Interrupted);
            }
            result = mcp::dispatch(workspace, Request::RunStatus { id: id.clone() }) => {
                let snapshot = result.map_err(classify)?;
                if let Some(done) = terminal_snapshot(&snapshot) {
                    return done;
                }
                tokio::time::sleep(Duration::from_millis(delay_ms)).await;
                delay_ms = (delay_ms * 2).min(200);
            }
        }
    }
}

fn terminal_snapshot(snapshot: &Value) -> Option<Result<Value, CliError>> {
    let Some(state) = snapshot.get("state").and_then(Value::as_str) else {
        return Some(Err(CliError::Runtime(anyhow::anyhow!(
            "run status is missing state: {snapshot}"
        ))));
    };
    match state {
        "queued" | "running" => None,
        "complete" => Some(Ok(snapshot.get("result").cloned().unwrap_or(Value::Null))),
        "cancelled" => Some(Err(CliError::Interrupted)),
        "failed" => {
            let message = snapshot
                .get("error")
                .and_then(Value::as_str)
                .unwrap_or("run failed");
            Some(Err(CliError::Runtime(anyhow::anyhow!(message.to_string()))))
        }
        other => Some(Err(CliError::Runtime(anyhow::anyhow!(
            "unknown run state {other}"
        )))),
    }
}

fn write_json(value: &Value) -> Result<(), CliError> {
    use std::io::{self, Write};
    let mut stdout = std::io::stdout().lock();
    let write = (|| -> io::Result<()> {
        serde_json::to_writer_pretty(&mut stdout, value).map_err(io::Error::other)?;
        stdout.write_all(b"\n")?;
        stdout.flush()
    })();
    match write {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == io::ErrorKind::BrokenPipe => Ok(()),
        Err(err) => Err(CliError::Runtime(err.into())),
    }
}

fn init_tracing(mcp: bool) {
    let filter = match std::env::var("RUST_LOG") {
        Ok(value) if !value.is_empty() => tracing_subscriber::EnvFilter::new(value),
        _ => tracing_subscriber::EnvFilter::new(if mcp { "off" } else { "warn" }),
    };
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .with_target(false)
        .try_init();
}
