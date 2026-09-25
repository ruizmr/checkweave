//! MCP server for Checkweave. Tools call [`dispatch`], which is the same daemon
//! entry the CLI uses. Nothing in this module evaluates predicates itself.

use std::path::{Component, Path};

use anyhow::Context;
use rmcp::{
    ServerHandler, ServiceExt,
    handler::server::{router::tool::ToolRouter, wrapper::Parameters},
    model::{CallToolResult, ContentBlock},
    service::{QuitReason, ServerInitializeError},
    tool, tool_handler, tool_router,
    transport::stdio,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::types::{CheckRequest, Limits, ReplayKind, Request, WorkRequest};
use crate::workspace::Workspace;

const TEXT_LIMIT: usize = 400;

/// Rejected before a daemon request. The CLI maps this to a usage exit.
#[derive(Debug)]
pub struct InvalidInput(pub String);

impl std::fmt::Display for InvalidInput {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for InvalidInput {}

/// Run one task through the shared daemon.
///
/// Local argument problems return [`InvalidInput`]. A check that matches, or a
/// partial or unresolved report, is a successful value so callers can show it.
pub async fn dispatch(workspace: &Workspace, request: Request) -> anyhow::Result<Value> {
    validate_request(&request)?;
    crate::daemon::request(workspace, request)
        .await
        .context("checkweave request failed")
}

pub fn validate_request(request: &Request) -> Result<(), InvalidInput> {
    match request {
        Request::Check(check) => validate_check(check),
        Request::Evidence { id } | Request::RunStatus { id } | Request::RunCancel { id } => {
            validate_evidence_id(id)
        }
        Request::TracePage { id, limit, .. } => {
            validate_evidence_id(id)?;
            if *limit == 0 {
                return Err(InvalidInput(
                    "trace page limit must be greater than 0".into(),
                ));
            }
            Ok(())
        }
        Request::Status | Request::Shutdown => Ok(()),
        Request::Compare { request }
        | Request::Trace { request }
        | Request::ModelEvaluate { request }
        | Request::Semantic { request } => validate_object(request, "request"),
        Request::Replay { kind: _, id } => validate_evidence_id(id),
        Request::ModelSetup { offline: _ } => Ok(()),
        Request::RunStart { request } => validate_work(request),
    }
}

fn validate_work(request: &WorkRequest) -> Result<(), InvalidInput> {
    match request {
        WorkRequest::Check(check) => validate_check(check),
        WorkRequest::Evidence { id } | WorkRequest::Replay { id, .. } => validate_evidence_id(id),
        WorkRequest::Compare { request }
        | WorkRequest::Trace { request }
        | WorkRequest::ModelEvaluate { request }
        | WorkRequest::Semantic { request } => validate_object(request, "request"),
        WorkRequest::ModelSetup { .. } => Ok(()),
    }
}

fn validate_object(value: &Value, name: &str) -> Result<(), InvalidInput> {
    if value.is_object() {
        Ok(())
    } else {
        Err(InvalidInput(format!("{name} must be a JSON object")))
    }
}

fn validate_check(check: &CheckRequest) -> Result<(), InvalidInput> {
    if check.include.is_empty() {
        return Err(InvalidInput(
            "include must contain at least one workspace-relative glob".into(),
        ));
    }
    for glob in &check.include {
        validate_glob(glob)?;
    }
    validate_limits(&check.limits)
}

fn validate_glob(glob: &str) -> Result<(), InvalidInput> {
    if glob.is_empty() || glob.chars().all(char::is_whitespace) {
        return Err(InvalidInput("include glob must be non-empty".into()));
    }
    if glob.chars().any(char::is_control) {
        return Err(InvalidInput(format!(
            "include glob contains a control character: {glob}"
        )));
    }
    let path = Path::new(glob);
    if path.is_absolute() {
        return Err(InvalidInput(format!(
            "include glob must be workspace-relative, not absolute: {glob}"
        )));
    }
    if path
        .components()
        .any(|component| component == Component::ParentDir)
    {
        return Err(InvalidInput(format!(
            "include glob must not contain '..': {glob}"
        )));
    }
    Ok(())
}

fn validate_limits(limits: &Limits) -> Result<(), InvalidInput> {
    if limits.max_files == 0 {
        return Err(InvalidInput("max_files must be greater than 0".into()));
    }
    if limits.max_bytes == 0 {
        return Err(InvalidInput("max_bytes must be greater than 0".into()));
    }
    if limits.max_records == 0 {
        return Err(InvalidInput("max_records must be greater than 0".into()));
    }
    if limits.timeout_ms == 0 {
        return Err(InvalidInput("timeout_ms must be greater than 0".into()));
    }
    Ok(())
}

pub fn validate_evidence_id(id: &str) -> Result<(), InvalidInput> {
    if id.is_empty() || id.chars().all(char::is_whitespace) {
        return Err(InvalidInput(
            "evidence id must be non-empty; use the id from a check result".into(),
        ));
    }
    if id.len() > 512 {
        return Err(InvalidInput("evidence id must be at most 512 bytes".into()));
    }
    if id.chars().any(|ch| ch.is_control() || ch.is_whitespace()) {
        return Err(InvalidInput(
            "evidence id must be a single token without whitespace".into(),
        ));
    }
    if id.contains('/') || id.contains('\\') || id.contains("..") {
        return Err(InvalidInput(
            "evidence id must not be a path; pass the id returned by check".into(),
        ));
    }
    Ok(())
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct EvidenceArgs {
    /// Evidence id returned by checkweave_check. This is not a filesystem path.
    id: String,
}

#[derive(Debug, Default, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct StatusArgs {}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct ReplayArgs {
    kind: ReplayKind,
    /// Evidence id from the original compare or trace.
    id: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct ModelSetupArgs {
    /// When true, succeed only from an existing cache.
    #[serde(default)]
    offline: bool,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct ModelEvaluateArgs {
    states: Vec<crate::models::ModelState>,
    questions: Vec<crate::models::ModelQuestion>,
    /// Deadline for the provider call when omitted. An explicit value is strict.
    #[serde(default = "default_model_timeout")]
    timeout_ms: u64,
}

fn default_model_timeout() -> u64 {
    180_000
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct TracePageArgs {
    /// Trace evidence id. Not a filesystem path.
    id: String,
    /// Stored events to skip. The first response uses offset 0.
    #[serde(default)]
    offset: usize,
    /// Maximum events in this page before the response-size fit.
    #[serde(default = "default_trace_page_limit")]
    limit: usize,
}

fn default_trace_page_limit() -> usize {
    crate::trace::INITIAL_EVENT_PAGE
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct RunStartArgs {
    request: WorkRequest,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct RunIdArgs {
    /// Run id returned by checkweave_run_start.
    id: String,
}

#[derive(Clone)]
struct CheckweaveServer {
    workspace: Workspace,
    tool_router: ToolRouter<Self>,
}

impl CheckweaveServer {
    fn new(workspace: Workspace) -> Self {
        Self {
            workspace,
            tool_router: Self::tool_router(),
        }
    }
}

#[tool_router]
impl CheckweaveServer {
    /// Check fixture or config-export JSONL against a predicate.
    ///
    /// A match is a normal result. Unresolved records stay in the report.
    /// Coverage counts processed records and is not a measure of model accuracy.
    #[tool(
        name = "checkweave_check",
        description = "Use when validating fixture or config-export JSONL: which rows match a field rule, which are unresolved, and whether freshness is validated. Check JSON Lines records in files matching include globs against a predicate. Returns coverage, per-record matches, source pointers, and an evidence id. A match is a normal result, not a failure. Unresolved records stay visible. Limits bound files, bytes, records, results, and time. max_results defaults to 50 and must be at most 5000.",
        annotations(
            title = "Check collection",
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn checkweave_check(
        &self,
        Parameters(request): Parameters<CheckRequest>,
    ) -> CallToolResult {
        // The check writes only the disposable .checkweave cache. It does not
        // mutate project files, so the read-only hint matches the user-visible effect.
        self.finish(dispatch(&self.workspace, Request::Check(request)).await)
    }

    /// Read retained evidence for a previous check id.
    #[tool(
        name = "checkweave_evidence",
        description = "Fetch retained evidence for an id returned by checkweave_check. The id is not a filesystem path. Missing or expired evidence is reported, not invented.",
        annotations(
            title = "Get evidence",
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn checkweave_evidence(
        &self,
        Parameters(EvidenceArgs { id }): Parameters<EvidenceArgs>,
    ) -> CallToolResult {
        self.finish(dispatch(&self.workspace, Request::Evidence { id }).await)
    }

    /// Read a later page of retained trace events.
    #[tool(
        name = "checkweave_trace_page",
        description = "Fetch a page of stored trace events for an id returned by checkweave_trace. offset is the number of stored events to skip. limit is the maximum events in the page. event_total is the number retained. The id is not a filesystem path. This does not rerun the script.",
        annotations(
            title = "Trace event page",
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn checkweave_trace_page(
        &self,
        Parameters(TracePageArgs { id, offset, limit }): Parameters<TracePageArgs>,
    ) -> CallToolResult {
        self.finish(dispatch(&self.workspace, Request::TracePage { id, offset, limit }).await)
    }

    /// Report workspace and worker status.
    #[tool(
        name = "checkweave_status",
        description = "Report workspace and worker status without running a check.",
        annotations(
            title = "Workspace status",
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn checkweave_status(&self, Parameters(_args): Parameters<StatusArgs>) -> CallToolResult {
        self.finish(dispatch(&self.workspace, Request::Status).await)
    }

    /// Compare two JSON-in/JSON-out commands. This runs only when called.
    #[tool(
        name = "checkweave_compare",
        description = "Use when a refactor may change behavior or you are comparing two alternatives. Compare before and after commands that read one JSON value and write one JSON document, with their declared sources. Runs programs only because this tool was called. A difference is an observation, not a regression or an equivalence result. The retained case is the witness input. The response is bounded.",
        annotations(
            title = "Compare behavior",
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = false,
            open_world_hint = false
        )
    )]
    async fn checkweave_compare(
        &self,
        Parameters(request): Parameters<crate::compare::CompareRequest>,
    ) -> CallToolResult {
        match serde_json::to_value(&request) {
            Ok(request) => {
                self.finish(dispatch(&self.workspace, Request::Compare { request }).await)
            }
            Err(err) => tool_error(err.into()),
        }
    }

    /// Replay a retained compare on current targets, or a trace snapshot.
    #[tool(
        name = "checkweave_replay",
        description = "Replay retained evidence by id. Compare replay re-runs the retained case against the current targets or the originally requested Git revisions. Trace replay executes the saved source snapshot, so after edits run a fresh trace instead of treating replay as the new code. This executes the retained request again. It does not claim the environment is a sandbox.",
        annotations(
            title = "Replay evidence",
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = false,
            open_world_hint = false
        )
    )]
    async fn checkweave_replay(
        &self,
        Parameters(ReplayArgs { kind, id }): Parameters<ReplayArgs>,
    ) -> CallToolResult {
        self.finish(dispatch(&self.workspace, Request::Replay { kind, id }).await)
    }

    /// Trace a Python script for a known input. This runs only when called.
    #[tool(
        name = "checkweave_trace",
        description = "Use when debugging a Python script that produced a wrong value or exception for a known input. Capture call, line, return, and exception events and scalar locals for an explicit TraceRequest. The script runs only because this tool was called, not because files changed. After edits, run a fresh trace; replay uses the saved snapshot. Event order is not causation.",
        annotations(
            title = "Trace execution",
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = false,
            open_world_hint = false
        )
    )]
    async fn checkweave_trace(
        &self,
        Parameters(request): Parameters<crate::trace::TraceRequest>,
    ) -> CallToolResult {
        match serde_json::to_value(&request) {
            Ok(request) => self.finish(dispatch(&self.workspace, Request::Trace { request }).await),
            Err(err) => tool_error(err.into()),
        }
    }

    /// Install or reuse the model runtime.
    #[tool(
        name = "checkweave_model_setup",
        description = "Prepare the configured decision model when a semantic or evaluate request needs it. Do not run this on every session. Offline refuses downloads. Local failure does not send data to a hosted provider.",
        annotations(
            title = "Set up model",
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = false,
            open_world_hint = true
        )
    )]
    async fn checkweave_model_setup(
        &self,
        Parameters(ModelSetupArgs { offline }): Parameters<ModelSetupArgs>,
    ) -> CallToolResult {
        self.finish(dispatch(&self.workspace, Request::ModelSetup { offline }).await)
    }

    /// Evaluate typed model decisions. May call a hosted provider when configured.
    #[tool(
        name = "checkweave_model_evaluate",
        description = "Evaluate typed decisions for the supplied states and questions. Reuses the workspace model worker. A hosted provider is used only when configured. Scores are not coverage.",
        annotations(
            title = "Evaluate model",
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = false,
            open_world_hint = true
        )
    )]
    async fn checkweave_model_evaluate(
        &self,
        Parameters(args): Parameters<ModelEvaluateArgs>,
    ) -> CallToolResult {
        match serde_json::to_value(args) {
            Ok(request) => {
                self.finish(dispatch(&self.workspace, Request::ModelEvaluate { request }).await)
            }
            Err(err) => tool_error(err.into()),
        }
    }

    /// Check a JSONL collection with the shared model provider.
    #[tool(
        name = "checkweave_semantic",
        description = "Use only when the question needs interpretation of text. Exact field checks use checkweave_check. Judge JSON Lines records with the configured model, local by default. text_pointer is a JSON Pointer; the empty string is the whole record. Reuses the daemon's model provider and cached row judgments. A hosted provider is used only when configured. Do not set up a model on every session. Unsupported questions stay visible. This does not run because files changed.",
        annotations(
            title = "Semantic collection check",
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = false,
            open_world_hint = true
        )
    )]
    async fn checkweave_semantic(
        &self,
        Parameters(request): Parameters<crate::semantic::SemanticCheckRequest>,
    ) -> CallToolResult {
        match serde_json::to_value(&request) {
            Ok(request) => {
                self.finish(dispatch(&self.workspace, Request::Semantic { request }).await)
            }
            Err(err) => tool_error(err.into()),
        }
    }

    /// Start work and return a handle without waiting.
    #[tool(
        name = "checkweave_run_start",
        description = "Start a check, compare, trace, replay, or model operation and return a run id. Poll checkweave_run_status. Cancel with checkweave_run_cancel. Status stays available while the run is in progress.",
        annotations(
            title = "Start run",
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = false,
            open_world_hint = true
        )
    )]
    async fn checkweave_run_start(
        &self,
        Parameters(RunStartArgs { request }): Parameters<RunStartArgs>,
    ) -> CallToolResult {
        self.finish(dispatch(&self.workspace, Request::RunStart { request }).await)
    }

    /// Read one run snapshot.
    #[tool(
        name = "checkweave_run_status",
        description = "Read progress for a run id without waiting for the run to finish.",
        annotations(
            title = "Run status",
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn checkweave_run_status(
        &self,
        Parameters(RunIdArgs { id }): Parameters<RunIdArgs>,
    ) -> CallToolResult {
        self.finish(dispatch(&self.workspace, Request::RunStatus { id }).await)
    }

    /// Cancel one run handle.
    #[tool(
        name = "checkweave_run_cancel",
        description = "Cancel one run. A shared deterministic check that other clients are still waiting for is not cancelled.",
        annotations(
            title = "Cancel run",
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn checkweave_run_cancel(
        &self,
        Parameters(RunIdArgs { id }): Parameters<RunIdArgs>,
    ) -> CallToolResult {
        self.finish(dispatch(&self.workspace, Request::RunCancel { id }).await)
    }

    fn finish(&self, outcome: anyhow::Result<Value>) -> CallToolResult {
        match outcome {
            Ok(value) => tool_success(value),
            Err(err) => tool_error(err),
        }
    }
}

#[tool_handler(
    router = self.tool_router,
    name = "checkweave",
    instructions = "Checkweave checks JSON Lines collections in this workspace. Compare, trace, and replay run programs only when that tool is called. Each record includes a source pointer with a path, line, and content fingerprint. Coverage counts records processed and is not model accuracy. A match means the record satisfied the predicate you supplied. For fixture or config-export JSONL, use checkweave_check and read coverage, unresolved, and freshness. For a refactor or two alternatives, use checkweave_compare on JSON-in/JSON-out commands with declared sources; a difference is not a regression and no difference is not equivalence. Compare replay re-runs the retained case on current targets or the originally requested Git revisions. For a wrong Python value or exception, use checkweave_trace on the known script and input and read scalar locals; after edits run a fresh trace because trace replay uses the saved snapshot. Use checkweave_semantic only for interpretation; the model is local by default and is not set up every session. Prefer the deterministic tool when it is enough. Do not call every tool. Detailed bounded JSON follows the summary in each result. Reuse that result before fetching again. checkweave_evidence accepts collection-check ids; trace_page accepts trace ids; compare replay accepts compare ids. Freshness validated means the result was checked against the recorded snapshot, not that files stay unchanged. Checkweave is not an execution sandbox."
)]
impl ServerHandler for CheckweaveServer {}

fn tool_success(value: Value) -> CallToolResult {
    let summary = summarize(&value);
    // Some clients expose only content to the assistant, not structuredContent.
    // Keep the bounded report available to both kinds of client.
    let text_report = value.to_string();
    let mut result = CallToolResult::structured(value);
    result.content = vec![ContentBlock::text(summary), ContentBlock::text(text_report)];
    result
}

fn tool_error(err: anyhow::Error) -> CallToolResult {
    let message = match err.downcast_ref::<InvalidInput>() {
        Some(invalid) => invalid.0.clone(),
        None => format!("{err:#}"),
    };
    let mut result = CallToolResult::structured_error(serde_json::json!({ "error": message }));
    result.content = vec![ContentBlock::text(brief(&format!("checkweave: {message}")))];
    result
}

fn summarize(value: &Value) -> String {
    let mut parts = Vec::new();
    for key in ["execution", "freshness", "outcome"] {
        if let Some(text) = value.get(key).and_then(Value::as_str) {
            parts.push(format!("{key}={text}"));
        }
    }
    if let Some(matched) = value.pointer("/coverage/matched").and_then(Value::as_u64) {
        parts.push(format!("matched={matched}"));
    }
    if let Some(unresolved) = value
        .pointer("/coverage/unresolved")
        .and_then(Value::as_u64)
    {
        parts.push(format!("unresolved={unresolved}"));
    }
    if value.get("truncated").and_then(Value::as_bool) == Some(true) {
        parts.push("truncated=true".into());
    }
    if let Some(id) = value.get("id").and_then(Value::as_str) {
        parts.push(format!("id={id}"));
    }
    if let Some(warning) = value
        .get("warnings")
        .and_then(Value::as_array)
        .and_then(|warnings| warnings.first())
        .and_then(Value::as_str)
    {
        parts.push(format!("warning={warning}"));
    }
    if parts.is_empty() {
        brief("checkweave: completed")
    } else {
        brief(&parts.join(" "))
    }
}

fn brief(text: &str) -> String {
    let mut truncated = String::new();
    for (index, ch) in text.chars().enumerate() {
        if index == TEXT_LIMIT {
            truncated.push('…');
            break;
        }
        truncated.push(ch);
    }
    truncated
}

/// Serve Checkweave tools on stdio using the official Rust MCP SDK.
///
/// Logging must stay off stdout. Closing stdin, or Ctrl-C, ends the process
/// without a protocol banner.
pub async fn serve(workspace: Workspace) -> anyhow::Result<()> {
    let (interrupt_tx, mut interrupt_rx) = tokio::sync::watch::channel(false);
    tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            let _ = interrupt_tx.send(true);
        }
    });

    if *interrupt_rx.borrow() {
        return Ok(());
    }

    let server = CheckweaveServer::new(workspace);
    let started = tokio::select! {
        result = server.serve(stdio()) => result,
        _ = interrupt_rx.changed() => return Ok(()),
    };
    let running = match started {
        Ok(running) => running,
        Err(err) => return quiet_initialize_end(err),
    };

    if *interrupt_rx.borrow() {
        return finish(running.cancel().await);
    }

    let cancel = running.cancellation_token();
    let wait = running.waiting();
    tokio::pin!(wait);
    tokio::select! {
        result = &mut wait => finish(result),
        _ = interrupt_rx.changed() => {
            cancel.cancel();
            finish(wait.await)
        }
    }
}

fn quiet_initialize_end(err: ServerInitializeError) -> anyhow::Result<()> {
    match err {
        ServerInitializeError::Cancelled | ServerInitializeError::ConnectionClosed(_) => Ok(()),
        other => Err(other.into()),
    }
}

fn finish(result: Result<QuitReason, tokio::task::JoinError>) -> anyhow::Result<()> {
    match result {
        Ok(QuitReason::Cancelled | QuitReason::Closed) => Ok(()),
        Ok(QuitReason::JoinError(err)) => Err(err.into()),
        Ok(other) => Err(anyhow::anyhow!("MCP server stopped: {other:?}")),
        Err(err) => Err(err.into()),
    }
}
