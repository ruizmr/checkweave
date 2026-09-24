//! Before/after behavior comparison.
//!
//! Commands run only when [`compare`] or [`replay`] asks [`crate::execute::run`]
//! to run them. Git revisions are read with `git rev-parse --verify` and executed
//! in a temporary detached worktree that is removed afterward. The user's
//! checkout is never switched. A worktree is not a sandbox.

use crate::execute::{
    self, ExecutionLimits, Observation, Target, containment_model, fingerprint_file,
    validate_workspace_relative,
};
use anyhow::{Context, Result, bail, ensure};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Number, Value};
use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use tokio::{io::AsyncReadExt, process::Command as TokioCommand};

const WIRE_BUDGET: u64 = 8 * 1024 * 1024;
const TEXT_CAP: usize = 8 * 1024;
const OUTPUT_KEEP: usize = 64 * 1024;
const GIT_CLEANUP_BOUND: Duration = Duration::from_secs(2);

pub const OPERATOR_VERSION: &str = "behavior-compare-v1";

fn default_repeat() -> u32 {
    2
}
fn default_values_key() -> String {
    "values".into()
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CompareRequest {
    pub before: Target,
    pub after: Target,
    #[serde(default)]
    pub inputs: Vec<Value>,
    #[serde(default)]
    pub generated: Option<GeneratedCases>,
    #[serde(default)]
    pub budgets: CompareBudgets,
    #[serde(default)]
    pub per_execution: ExecutionLimits,
    /// When true, shrink a stable JSON difference within the same budgets.
    #[serde(default)]
    pub reduce: bool,
    /// Re-run each case this many times. Values below 2 are rejected.
    #[serde(default = "default_repeat")]
    pub repeat: u32,
    #[serde(default)]
    pub before_revision: Option<String>,
    #[serde(default)]
    pub after_revision: Option<String>,
    #[serde(default)]
    pub policy: ComparePolicy,
    #[serde(default)]
    pub retention: RetentionLimits,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct GeneratedCases {
    pub seed: u64,
    #[serde(default)]
    pub integers: Option<IntegerSpec>,
    #[serde(default)]
    pub arrays: Option<ArraySpec>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, JsonSchema, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum IntegerShape {
    #[default]
    Number,
    Object,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct IntegerSpec {
    pub start: i64,
    pub end: i64,
    #[serde(default)]
    pub shape: IntegerShape,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ArraySpec {
    pub count: usize,
    pub length: usize,
    pub min: i64,
    pub max: i64,
    #[serde(default = "default_values_key")]
    pub key: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct CompareBudgets {
    pub max_cases: usize,
    pub max_executions: usize,
    pub timeout_ms: u64,
}

impl Default for CompareBudgets {
    fn default() -> Self {
        Self {
            max_cases: 64,
            max_executions: 256,
            timeout_ms: 30_000,
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, JsonSchema, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum ExitPolicy {
    /// Only a successful process that printed one JSON document is comparable.
    #[default]
    RequireSuccess,
    /// Exit codes are part of the comparison when both sides printed JSON.
    CompareExit,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct ComparePolicy {
    pub exit: ExitPolicy,
}

impl Default for ComparePolicy {
    fn default() -> Self {
        Self {
            exit: ExitPolicy::RequireSuccess,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct RetentionLimits {
    pub max_entries: usize,
    pub max_total_bytes: u64,
}

impl Default for RetentionLimits {
    fn default() -> Self {
        Self {
            max_entries: 16,
            max_total_bytes: 16 * 1024 * 1024,
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CompareOutcome {
    ObservedDifference,
    BoundedNoDifference,
    Nondeterministic,
    Unsupported,
    BudgetExhausted,
    Cancelled,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct SourceIdentity {
    pub path: String,
    pub fingerprint: String,
    pub revision: Option<String>,
    pub dirty: bool,
    pub bytes_retained: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct DifferenceEvidence {
    pub original_input: Value,
    pub input: Value,
    pub before: Observation,
    pub after: Observation,
    pub reduced: bool,
    /// Smallest input retained within budget. Not a proof of minimality.
    pub minimality: String,
    pub reduction_budget_exhausted: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ReproductionRef {
    pub id: String,
    /// Workspace-relative directory. Never a temporary worktree path.
    pub evidence_dir: String,
    /// Direct argv. A shell is not involved.
    pub argv: Vec<String>,
    pub request_artifact: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, Default)]
pub struct AggregateMetrics {
    pub discovered_differences: u64,
    /// Always zero: an observed difference is not classified as a regression.
    pub incorrect_regression_claims: u64,
    pub executions: u64,
    pub elapsed_ms: u64,
    pub reproducible: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct CompareReport {
    pub id: String,
    pub operator_version: String,
    pub outcome: CompareOutcome,
    pub basis: String,
    pub executions: u64,
    pub elapsed_ms: u64,
    pub budgets: CompareBudgets,
    pub per_execution: ExecutionLimits,
    pub repeat: u32,
    pub policy: ComparePolicy,
    pub requested_before_revision: Option<String>,
    pub requested_after_revision: Option<String>,
    pub before_revision: Option<String>,
    pub after_revision: Option<String>,
    pub before_sources: Vec<SourceIdentity>,
    pub after_sources: Vec<SourceIdentity>,
    pub cases_selected: usize,
    pub cases_completed: usize,
    pub cases_same: usize,
    pub cases_different: usize,
    pub cases_nondeterministic: usize,
    pub cases_unsupported: usize,
    pub difference: Option<DifferenceEvidence>,
    pub reproduction: Option<ReproductionRef>,
    pub metrics: AggregateMetrics,
    pub warnings: Vec<String>,
    pub containment: String,
    pub dependency_completeness: String,
    pub freshness: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub replay_of: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub changed_sources: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub missing_evidence: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub original_outcome: Option<CompareOutcome>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outcome_reproduced: Option<bool>,
    /// Original versus current fingerprints for this rerun. Empty on the first compare.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub source_comparisons: Vec<SourceComparison>,
    /// Pair produced for the retained witness input. Not the full case list.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub witness_before: Option<Observation>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub witness_after: Option<Observation>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct SourceComparison {
    pub path: String,
    pub original_fingerprint: String,
    pub current_fingerprint: Option<String>,
    pub changed: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Relation {
    Same,
    Difference,
    Nondeterministic,
    Unsupported,
    Cancelled,
    Budget,
}

#[derive(Clone)]
struct CaseResult {
    input: Value,
    relation: Relation,
    before: Vec<Observation>,
    after: Vec<Observation>,
}

struct Worktree {
    repo: PathBuf,
    path: PathBuf,
    removed: bool,
}

impl Worktree {
    fn finish_sync(&mut self) {
        if self.removed {
            return;
        }
        self.removed = true;
        bounded_git_remove(&self.repo, &self.path, GIT_CLEANUP_BOUND);
        let _ = fs::remove_dir_all(&self.path);
    }
}

impl Drop for Worktree {
    fn drop(&mut self) {
        self.finish_sync();
    }
}

struct Deadline {
    started: Instant,
    timeout: Duration,
    cancel: Arc<AtomicBool>,
}

impl Deadline {
    fn remaining(&self) -> Duration {
        self.timeout.saturating_sub(self.started.elapsed())
    }

    fn cancelled(&self) -> bool {
        self.cancel.load(Ordering::Acquire)
    }

    fn expired(&self) -> bool {
        self.cancelled() || self.remaining().is_zero()
    }
}

#[derive(Debug)]
struct BoundHit(CompareOutcome);

impl std::fmt::Display for BoundHit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.0 {
            CompareOutcome::Cancelled => write!(f, "compare cancelled"),
            _ => write!(f, "compare deadline exceeded"),
        }
    }
}

impl std::error::Error for BoundHit {}

struct Side {
    root: PathBuf,
    target: Target,
    revision: Option<String>,
    sources: Vec<SourceIdentity>,
    _worktree: Option<Worktree>,
    warning: Option<String>,
    unavailable: Option<String>,
}

struct RunCtx<'a> {
    before: &'a Side,
    after: &'a Side,
    limits: ExecutionLimits,
    policy: ComparePolicy,
    repeat: u32,
    cancel: Arc<AtomicBool>,
    started: Instant,
    timeout: Duration,
    max_executions: usize,
    executions: u64,
    reduction_stopped: bool,
    last_before: Option<Observation>,
    last_after: Option<Observation>,
}

#[derive(Clone)]
enum Seg {
    Key(String),
    Index(usize),
}

#[derive(Serialize, Deserialize)]
struct EvidenceManifest {
    id: String,
    operator_version: String,
    outcome: CompareOutcome,
    files: Vec<String>,
    source_fingerprints: Vec<SourceIdentity>,
    before_revision: Option<String>,
    after_revision: Option<String>,
    limits: ExecutionLimits,
    budgets: CompareBudgets,
    repeat: u32,
    elapsed_ms: u64,
    executions: u64,
    input: Option<Value>,
    original_input: Option<Value>,
    created_unix_ms: u64,
    containment: String,
    dependency_completeness: String,
}

#[derive(Serialize)]
struct CliArtifact<'a> {
    description: &'a str,
    argv: &'a [String],
    request: &'a CompareRequest,
}

pub async fn compare(
    root: &Path,
    request: &CompareRequest,
    cancel: Arc<AtomicBool>,
) -> Result<CompareReport> {
    validate_request(request)?;
    let root = root.canonicalize().context("canonicalize workspace")?;
    let started = Instant::now();
    let id = uuid::Uuid::new_v4().to_string();
    let mut warnings = vec![
        containment_model().to_string(),
        "commands run only for this compare request; filesystem events do not start them".into(),
        "inherited environment and external state are untracked; a worktree is not a sandbox".into(),
        "JSON parser failures, crashes, timeouts, and truncated output are unsupported and are not semantic differences".into(),
    ];
    if cancel.load(Ordering::Acquire) {
        let mut report = skeleton(&id, request, &root, started, 0, &warnings);
        report.outcome = CompareOutcome::Cancelled;
        report.freshness = "unknown".into();
        return Ok(report);
    }

    let deadline = Deadline {
        started,
        timeout: Duration::from_millis(request.budgets.timeout_ms),
        cancel: cancel.clone(),
    };
    let before_hash =
        match resolve_revision(&root, request.before_revision.as_deref(), &deadline).await {
            Ok(hash) => hash,
            Err(err) => return stop_for_bound(&id, request, &root, started, &warnings, err),
        };
    let after_hash =
        match resolve_revision(&root, request.after_revision.as_deref(), &deadline).await {
            Ok(hash) => hash,
            Err(err) => return stop_for_bound(&id, request, &root, started, &warnings, err),
        };
    let before = match prepare_side(&root, "before", &request.before, before_hash, &deadline).await
    {
        Ok(side) => side,
        Err(err) => return stop_for_bound(&id, request, &root, started, &warnings, err),
    };
    let after = match prepare_side(&root, "after", &request.after, after_hash, &deadline).await {
        Ok(side) => side,
        Err(err) => return stop_for_bound(&id, request, &root, started, &warnings, err),
    };
    if let Some(warning) = before.warning.clone() {
        warnings.push(warning);
    }
    if let Some(warning) = after.warning.clone() {
        warnings.push(warning);
    }

    let mut report = skeleton(&id, request, &root, started, 0, &warnings);
    report.before_revision = before.revision.clone();
    report.after_revision = after.revision.clone();
    report.before_sources = before.sources.clone();
    report.after_sources = after.sources.clone();

    if let Some(reason) = before.unavailable.clone().or(after.unavailable.clone()) {
        report.outcome = CompareOutcome::Unsupported;
        report.warnings.push(reason);
        report
            .warnings
            .push("dirty workspace bytes are not mixed into a historical tree".into());
        report.dependency_completeness = execute::DEPENDENCY_COMPLETENESS.into();
        let _ = publish(&root, request, &before, &after, &mut report, None, None);
        return Ok(report);
    }

    let cases = select_cases(request)?;
    report.cases_selected = cases.len();
    if cases.is_empty() {
        report.outcome = CompareOutcome::Unsupported;
        report
            .warnings
            .push("no inputs to compare; an empty set is not preserved behavior".into());
        let _ = publish(&root, request, &before, &after, &mut report, None, None);
        return Ok(report);
    }

    let mut ctx = RunCtx {
        before: &before,
        after: &after,
        limits: request.per_execution.clone(),
        policy: request.policy.clone(),
        repeat: request.repeat,
        cancel: cancel.clone(),
        started,
        timeout: Duration::from_millis(request.budgets.timeout_ms),
        max_executions: request.budgets.max_executions,
        executions: 0,
        reduction_stopped: false,
        last_before: None,
        last_after: None,
    };

    let mut stopped: Option<Relation> = None;
    let mut same = 0usize;
    let mut different = 0usize;
    let mut nondeterministic = 0usize;
    let mut unsupported = 0usize;
    let mut completed = 0usize;
    let mut best: Option<CaseResult> = None;
    let mut sample_before = None;
    let mut sample_after = None;
    for case in &cases {
        if ctx.cancelled() {
            stopped = Some(Relation::Cancelled);
            break;
        }
        if ctx.over_budget() {
            stopped = Some(Relation::Budget);
            break;
        }
        let result = eval_case(&mut ctx, case).await?;
        let relation = result.relation;
        if matches!(relation, Relation::Cancelled | Relation::Budget) {
            stopped = Some(relation);
            break;
        }
        completed += 1;
        match relation {
            Relation::Same => {
                same += 1;
                if sample_before.is_none() {
                    sample_before = result.before.first().cloned().map(compact_observation);
                    sample_after = result.after.first().cloned().map(compact_observation);
                }
            }
            Relation::Difference => {
                different += 1;
                let replace = best
                    .as_ref()
                    .map(|kept| cost(&result.input) < cost(&kept.input))
                    .unwrap_or(true);
                if replace {
                    sample_before = result.before.first().cloned().map(compact_observation);
                    sample_after = result.after.first().cloned().map(compact_observation);
                    best = Some(CaseResult {
                        input: result.input.clone(),
                        relation,
                        before: sample_before.clone().into_iter().collect(),
                        after: sample_after.clone().into_iter().collect(),
                    });
                }
            }
            Relation::Nondeterministic => nondeterministic += 1,
            Relation::Unsupported => unsupported += 1,
            Relation::Cancelled | Relation::Budget => {}
        }
    }
    report.cases_completed = completed;
    report.cases_same = same;
    report.cases_different = different;
    report.cases_nondeterministic = nondeterministic;
    report.cases_unsupported = unsupported;
    report.executions = ctx.executions;

    let mut outcome = if stopped == Some(Relation::Cancelled) || ctx.cancelled() {
        CompareOutcome::Cancelled
    } else if different > 0 {
        CompareOutcome::ObservedDifference
    } else if stopped == Some(Relation::Budget) {
        CompareOutcome::BudgetExhausted
    } else if report.cases_nondeterministic > 0 {
        CompareOutcome::Nondeterministic
    } else if report.cases_unsupported > 0 || report.cases_same != completed || completed == 0 {
        CompareOutcome::Unsupported
    } else {
        CompareOutcome::BoundedNoDifference
    };

    if outcome == CompareOutcome::ObservedDifference {
        let chosen = best.as_ref().expect("difference");
        ctx.last_before = chosen.before.first().cloned();
        ctx.last_after = chosen.after.first().cloned();
        let original = chosen.input.clone();
        let retained = if request.reduce {
            minimize(original.clone(), &mut ctx).await?
        } else {
            original.clone()
        };
        if ctx.cancelled() {
            outcome = CompareOutcome::Cancelled;
        }
        let reduced = request.reduce && retained != original;
        report.difference = Some(DifferenceEvidence {
            original_input: original,
            input: retained,
            before: ctx
                .last_before
                .clone()
                .context("missing before observation")?,
            after: ctx
                .last_after
                .clone()
                .context("missing after observation")?,
            reduced,
            minimality: if request.reduce {
                "smallest_retained_within_budget"
            } else {
                "not_reduced"
            }
            .into(),
            reduction_budget_exhausted: request.reduce && ctx.reduction_stopped && !ctx.cancelled(),
        });
        report
            .warnings
            .push("an observed difference is not a regression claim".into());
        if request.reduce {
            report.warnings.push("reduction keeps the smallest stable difference found within budget and does not prove minimality".into());
        }
    } else if outcome == CompareOutcome::BoundedNoDifference {
        report.warnings.push(
            "no difference inside the evaluated budget; this does not establish equivalence".into(),
        );
    } else if outcome == CompareOutcome::BudgetExhausted {
        report
            .warnings
            .push("execution or time budget ended before every selected case finished".into());
    }

    report.outcome = outcome;
    report.executions = ctx.executions;
    report.elapsed_ms = started.elapsed().as_millis() as u64;
    report.freshness = freshness(&root, &before, &after);
    report.metrics = AggregateMetrics {
        discovered_differences: report.cases_different as u64,
        incorrect_regression_claims: 0,
        executions: report.executions,
        elapsed_ms: report.elapsed_ms,
        reproducible: report.freshness == "validated"
            && matches!(
                report.outcome,
                CompareOutcome::ObservedDifference | CompareOutcome::BoundedNoDifference
            ),
    };
    report.dependency_completeness = execute::DEPENDENCY_COMPLETENESS.into();
    if let Some(diff) = &mut report.difference {
        diff.before = compact_observation(diff.before.clone());
        diff.after = compact_observation(diff.after.clone());
        sample_before = Some(diff.before.clone());
        sample_after = Some(diff.after.clone());
    }
    report.witness_before = sample_before.clone();
    report.witness_after = sample_after.clone();
    publish(
        &root,
        request,
        &before,
        &after,
        &mut report,
        sample_before.as_ref(),
        sample_after.as_ref(),
    )?;
    if report.freshness != "validated" {
        report.metrics.reproducible = false;
    }
    if deadline.expired() && (request.before_revision.is_some() || request.after_revision.is_some())
    {
        report.warnings.push("git worktree cleanup is bounded to 2s and killed on expiry; that cleanup is not covered comparison time".into());
    }
    Ok(report)
}

pub async fn replay(root: &Path, id: &str, cancel: Arc<AtomicBool>) -> Result<CompareReport> {
    ensure!(
        !id.is_empty()
            && id.len() <= 80
            && id.chars().all(|c| c.is_ascii_hexdigit() || c == '-')
            && !id.contains(".."),
        "invalid evidence id"
    );
    let root = root.canonicalize().context("canonicalize workspace")?;
    let dir = root.join(".checkweave").join("behavior").join(id);
    let manifest_path = dir.join("manifest.json");
    ensure!(manifest_path.is_file(), "missing evidence: manifest.json");
    let manifest: EvidenceManifest = serde_json::from_slice(&fs::read(&manifest_path)?)?;
    let mut missing = Vec::new();
    for file in &manifest.files {
        if file
            .split(['/', '\\'])
            .any(|part| part == ".." || part.is_empty())
        {
            missing.push(format!("invalid evidence path: {file}"));
            continue;
        }
        if !dir.join(file).is_file() {
            missing.push(file.clone());
        }
    }
    let request_bytes =
        fs::read(dir.join("request.json")).context("missing evidence: request.json")?;
    let original: CompareRequest = serde_json::from_slice(&request_bytes)?;
    let witness_path = dir.join("witness-request.json");
    let request: CompareRequest = if witness_path.is_file() {
        serde_json::from_slice(&fs::read(&witness_path)?)?
    } else {
        missing.push("witness-request.json".into());
        original
    };
    let prep = Deadline {
        started: Instant::now(),
        timeout: Duration::from_millis(request.budgets.timeout_ms),
        cancel: cancel.clone(),
    };
    for label in [
        ("before", &manifest.before_revision),
        ("after", &manifest.after_revision),
    ] {
        if let Some(hash) = label.1 {
            match resolve_revision(&root, Some(hash), &prep).await {
                Ok(Some(current)) if &current == hash => {}
                Err(err) if err.downcast_ref::<BoundHit>().is_some() => {
                    missing.push(format!(
                        "{} revision check exceeded the replay deadline",
                        label.0
                    ));
                }
                _ => missing.push(format!("{} revision unavailable: {hash}", label.0)),
            }
        }
    }
    let mut changed = Vec::new();
    let mut comparisons = Vec::new();
    for src in &manifest.source_fingerprints {
        if src.revision.is_some() {
            continue;
        }
        let mut remaining = 16 * 1024 * 1024;
        let current = fingerprint_file(&root, &src.path, &mut remaining).ok();
        let differs = current.as_ref() != Some(&src.fingerprint);
        if differs {
            changed.push(src.path.clone());
            if current.is_none() {
                missing.push(format!("source unavailable: {}", src.path));
            }
        }
        comparisons.push(SourceComparison {
            path: src.path.clone(),
            original_fingerprint: src.fingerprint.clone(),
            current_fingerprint: current,
            changed: differs,
        });
    }
    let stored_before = read_observation(&dir.join("before-observation.json"));
    let stored_after = read_observation(&dir.join("after-observation.json"));
    let mut report = compare(&root, &request, cancel).await?;
    report.replay_of = Some(id.to_string());
    report.changed_sources = changed;
    report.source_comparisons = comparisons;
    report.missing_evidence.extend(missing);
    report.original_outcome = Some(manifest.outcome);
    let outputs_match = match (
        &stored_before,
        &stored_after,
        &report.witness_before,
        &report.witness_after,
    ) {
        (Some(stored_b), Some(stored_a), Some(fresh_b), Some(fresh_a)) => {
            observations_match(stored_b, fresh_b, &request.policy)
                && observations_match(stored_a, fresh_a, &request.policy)
        }
        _ => false,
    };
    let sources_match = report.source_comparisons.iter().all(|item| !item.changed);
    report.outcome_reproduced =
        Some(outputs_match && sources_match && report.missing_evidence.is_empty());
    report.metrics.reproducible = report.outcome_reproduced.unwrap_or(false);
    report.warnings.push("replay is a current-workspace rerun of the retained witness input, not a snapshot reproduction of the original process".into());
    Ok(report)
}

fn read_observation(path: &Path) -> Option<Observation> {
    let bytes = fs::read(path).ok()?;
    serde_json::from_slice(&bytes).ok()
}

fn skeleton(
    id: &str,
    request: &CompareRequest,
    _root: &Path,
    started: Instant,
    executions: u64,
    warnings: &[String],
) -> CompareReport {
    CompareReport {
        id: id.to_string(),
        operator_version: OPERATOR_VERSION.into(),
        outcome: CompareOutcome::Unsupported,
        basis: "direct_observation".into(),
        executions,
        elapsed_ms: started.elapsed().as_millis() as u64,
        budgets: request.budgets.clone(),
        per_execution: request.per_execution.clone(),
        repeat: request.repeat,
        policy: request.policy.clone(),
        requested_before_revision: request.before_revision.clone(),
        requested_after_revision: request.after_revision.clone(),
        before_revision: None,
        after_revision: None,
        before_sources: Vec::new(),
        after_sources: Vec::new(),
        cases_selected: 0,
        cases_completed: 0,
        cases_same: 0,
        cases_different: 0,
        cases_nondeterministic: 0,
        cases_unsupported: 0,
        difference: None,
        reproduction: None,
        metrics: AggregateMetrics::default(),
        warnings: warnings.to_vec(),
        containment: containment_model().into(),
        dependency_completeness: execute::DEPENDENCY_COMPLETENESS.into(),
        freshness: "unknown".into(),
        replay_of: None,
        changed_sources: Vec::new(),
        missing_evidence: Vec::new(),
        original_outcome: None,
        outcome_reproduced: None,
        source_comparisons: Vec::new(),
        witness_before: None,
        witness_after: None,
    }
}

fn validate_request(request: &CompareRequest) -> Result<()> {
    ensure!((2..=32).contains(&request.repeat), "repeat must be 2..=32");
    ensure!(
        (1..=10_000).contains(&request.budgets.max_cases),
        "max_cases must be 1..=10000"
    );
    ensure!(
        (1..=100_000).contains(&request.budgets.max_executions),
        "max_executions must be 1..=100000"
    );
    ensure!(
        (1..=3_600_000).contains(&request.budgets.timeout_ms),
        "compare timeout_ms must be 1..=3600000"
    );
    ensure!(
        (1..=10_000).contains(&request.retention.max_entries),
        "retention max_entries must be 1..=10000"
    );
    ensure!(
        request.retention.max_total_bytes >= 1,
        "retention max_total_bytes must be positive"
    );
    validate_target(&request.before)?;
    validate_target(&request.after)?;
    if let Some(generated) = &request.generated {
        if let Some(spec) = &generated.integers {
            ensure!(spec.end >= spec.start, "integer range end is before start");
        }
        if let Some(spec) = &generated.arrays {
            ensure!(
                spec.count <= 10_000 && spec.length <= 1024,
                "array generation exceeds limit"
            );
            ensure!(spec.min <= spec.max, "array min is greater than max");
            ensure!(
                !spec.key.is_empty() && spec.key.len() <= 128,
                "invalid generated array key"
            );
        }
    }
    Ok(())
}

fn validate_target(target: &Target) -> Result<()> {
    ensure!(
        !target.argv.is_empty() && target.argv.len() <= 128,
        "target argv requires 1..128 entries"
    );
    ensure!(!target.argv[0].is_empty(), "target executable is empty");
    for arg in &target.argv {
        let path = Path::new(arg);
        ensure!(
            !path.is_absolute(),
            "target argv must be a command name or a workspace-relative path, not an absolute path"
        );
    }
    validate_workspace_relative(&target.cwd)?;
    for source in &target.sources {
        validate_workspace_relative(source)?;
    }
    Ok(())
}

fn select_cases(request: &CompareRequest) -> Result<Vec<Value>> {
    let mut cases = Vec::new();
    for input in &request.inputs {
        if cases.len() >= request.budgets.max_cases {
            break;
        }
        ensure!(
            serde_json::to_vec(input)?.len() <= 1024 * 1024,
            "case exceeds 1 MiB"
        );
        cases.push(input.clone());
    }
    if let Some(generated) = &request.generated {
        if let Some(spec) = &generated.integers {
            let mut n = spec.start;
            while cases.len() < request.budgets.max_cases {
                let value = match spec.shape {
                    IntegerShape::Number => Value::from(n),
                    IntegerShape::Object => serde_json::json!({ "n": n }),
                };
                cases.push(value);
                if n == spec.end {
                    break;
                }
                n += 1;
            }
        }
        if let Some(spec) = &generated.arrays {
            let mut state = generated.seed;
            for _ in 0..spec.count {
                if cases.len() >= request.budgets.max_cases {
                    break;
                }
                let mut values = Vec::with_capacity(spec.length);
                for _ in 0..spec.length {
                    values.push(Value::from(next_in_range(&mut state, spec.min, spec.max)));
                }
                let mut object = Map::new();
                object.insert(spec.key.clone(), Value::Array(values));
                cases.push(Value::Object(object));
            }
        }
    }
    Ok(cases)
}

fn next_in_range(state: &mut u64, min: i64, max: i64) -> i64 {
    *state = state.wrapping_add(0x9E3779B97F4A7C15);
    let mut z = *state;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58476D1CE4E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D049BB133111EB);
    let draw = (z ^ (z >> 31)) as u128;
    let span = (max as i128 - min as i128) as u128 + 1;
    (min as i128 + (draw % span) as i128) as i64
}

async fn resolve_revision(
    root: &Path,
    rev: Option<&str>,
    deadline: &Deadline,
) -> Result<Option<String>> {
    let Some(rev) = rev else { return Ok(None) };
    ensure!(
        !rev.is_empty() && !rev.contains(['\0', '\n', '\r']),
        "invalid revision"
    );
    ensure!(
        git_available(root, deadline).await?,
        "revision requested but workspace is not a git work tree"
    );
    let spec = format!("{rev}^{{commit}}");
    let output = git_run(
        root,
        &[
            "rev-parse".into(),
            "--verify".into(),
            "--end-of-options".into(),
            spec,
        ],
        deadline,
    )
    .await?;
    ensure!(
        output.status.success(),
        "git rev-parse --verify failed for {rev}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let hash = String::from_utf8(output.stdout)?.trim().to_string();
    ensure!(
        hash.len() == 40 && hash.chars().all(|c| c.is_ascii_hexdigit()),
        "unexpected rev-parse output"
    );
    Ok(Some(hash))
}

async fn git_available(root: &Path, deadline: &Deadline) -> Result<bool> {
    match git_run(
        root,
        &["rev-parse".into(), "--is-inside-work-tree".into()],
        deadline,
    )
    .await
    {
        Ok(output) => Ok(output.status.success() && output.stdout.starts_with(b"true")),
        Err(err) if err.downcast_ref::<BoundHit>().is_some() => Err(err),
        Err(_) => Ok(false),
    }
}

async fn object_kind(root: &Path, spec: &str, deadline: &Deadline) -> Result<Option<String>> {
    let output = git_run(
        root,
        &["cat-file".into(), "-t".into(), spec.to_string()],
        deadline,
    )
    .await?;
    if !output.status.success() {
        return Ok(None);
    }
    Ok(Some(String::from_utf8(output.stdout)?.trim().to_string()))
}

async fn blob_id(root: &Path, spec: &str, deadline: &Deadline) -> Result<Option<String>> {
    let output = git_run(
        root,
        &[
            "rev-parse".into(),
            "--verify".into(),
            "--end-of-options".into(),
            spec.to_string(),
        ],
        deadline,
    )
    .await?;
    if !output.status.success() {
        return Ok(None);
    }
    Ok(Some(String::from_utf8(output.stdout)?.trim().to_string()))
}

async fn file_blob_id(path: &Path, deadline: &Deadline) -> Result<String> {
    let output = git_run(
        Path::new("."),
        &[
            "hash-object".into(),
            "--".into(),
            path.display().to_string(),
        ],
        deadline,
    )
    .await?;
    ensure!(
        output.status.success(),
        "git hash-object failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(String::from_utf8(output.stdout)?.trim().to_string())
}

async fn git_run(cwd: &Path, args: &[String], deadline: &Deadline) -> Result<std::process::Output> {
    if deadline.cancelled() {
        return Err(BoundHit(CompareOutcome::Cancelled).into());
    }
    let remaining = deadline.remaining();
    if remaining.is_zero() {
        return Err(BoundHit(CompareOutcome::BudgetExhausted).into());
    }
    let mut command = TokioCommand::new("git");
    command
        .arg("-C")
        .arg(cwd)
        .args(args)
        .kill_on_drop(true)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(unix)]
    command.process_group(0);
    let mut child = command
        .spawn()
        .with_context(|| format!("git {}", args.join(" ")))?;
    let pid = child.id();
    let mut stdout = child.stdout.take().context("git stdout")?;
    let mut stderr = child.stderr.take().context("git stderr")?;
    let cancel = deadline.cancel.clone();
    let collect = async move {
        let mut out = Vec::new();
        let mut err = Vec::new();
        let mut out_done = false;
        let mut err_done = false;
        let mut out_buf = [0u8; 8192];
        let mut err_buf = [0u8; 8192];
        while !out_done || !err_done {
            tokio::select! {
                read = stdout.read(&mut out_buf), if !out_done => {
                    let n = read?;
                    if n == 0 {
                        out_done = true;
                    } else {
                        let room = (256 * 1024usize).saturating_sub(out.len());
                        if room > 0 {
                            out.extend_from_slice(&out_buf[..n.min(room)]);
                        }
                    }
                }
                read = stderr.read(&mut err_buf), if !err_done => {
                    let n = read?;
                    if n == 0 {
                        err_done = true;
                    } else {
                        let room = (256 * 1024usize).saturating_sub(err.len());
                        if room > 0 {
                            err.extend_from_slice(&err_buf[..n.min(room)]);
                        }
                    }
                }
            }
        }
        let status = child.wait().await?;
        Ok::<_, anyhow::Error>((status, out, err))
    };
    tokio::pin!(collect);
    tokio::select! {
        result = &mut collect => {
            let (status, stdout, stderr) = result?;
            Ok(std::process::Output { status, stdout, stderr })
        }
        _ = tokio::time::sleep(remaining) => {
            kill_pid(pid);
            Err(BoundHit(CompareOutcome::BudgetExhausted).into())
        }
        _ = wait_cancel(cancel) => {
            kill_pid(pid);
            Err(BoundHit(CompareOutcome::Cancelled).into())
        }
    }
}

async fn wait_cancel(cancel: Arc<AtomicBool>) {
    loop {
        if cancel.load(Ordering::Acquire) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

fn kill_pid(pid: Option<u32>) {
    #[cfg(unix)]
    if let Some(pid) = pid {
        unsafe {
            libc::kill(-(pid as i32), libc::SIGKILL);
        }
    }
    #[cfg(not(unix))]
    let _ = pid;
}

fn bounded_git_remove(repo: &Path, path: &Path, timeout: Duration) {
    let mut command = Command::new("git");
    command
        .arg("-C")
        .arg(repo)
        .args(["worktree", "remove", "--force", "--"])
        .arg(path)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let Ok(mut child) = command.spawn() else {
        return;
    };
    let started = Instant::now();
    loop {
        if child.try_wait().ok().flatten().is_some() {
            return;
        }
        if started.elapsed() >= timeout {
            let _ = child.kill();
            let _ = child.wait();
            return;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

async fn prepare_side(
    root: &Path,
    label: &str,
    target: &Target,
    revision: Option<String>,
    deadline: &Deadline,
) -> Result<Side> {
    let mut unavailable = None;
    let mut warning = None;
    if let Some(hash) = &revision {
        if target.cwd != "." {
            let spec = format!("{hash}:{}", target.cwd);
            if object_kind(root, &spec, deadline).await?.as_deref() != Some("tree") {
                unavailable = Some(format!("{label} cwd is missing at revision {hash}"));
            }
        }
        for source in &target.sources {
            let spec = format!("{hash}:{source}");
            if object_kind(root, &spec, deadline).await?.as_deref() != Some("blob") {
                unavailable = Some(format!(
                    "{label} source is missing at revision {hash} or is not a file: {source}"
                ));
                break;
            }
        }
        let mut dirty = Vec::new();
        for source in &target.sources {
            let abs = root.join(source);
            if !abs.is_file() {
                continue;
            }
            let spec = format!("{hash}:{source}");
            if let Some(committed) = blob_id(root, &spec, deadline).await?
                && file_blob_id(&abs, deadline).await? != committed
            {
                dirty.push(source.clone());
            }
        }
        if !dirty.is_empty() {
            warning = Some(format!(
                "historical target {label} executed {hash} without mixing dirty workspace bytes for: {}",
                dirty.join(", ")
            ));
        }
    }

    let (exec_root, worktree) = if unavailable.is_none() {
        if let Some(hash) = &revision {
            let path = std::env::temp_dir().join(format!("checkweave-wt-{}", uuid::Uuid::new_v4()));
            let output = git_run(
                root,
                &[
                    "worktree".into(),
                    "add".into(),
                    "--detach".into(),
                    "--quiet".into(),
                    "--".into(),
                    path.display().to_string(),
                    hash.clone(),
                ],
                deadline,
            )
            .await;
            let output = match output {
                Ok(output) => output,
                Err(err) => {
                    let _ = fs::remove_dir_all(&path);
                    return Err(err);
                }
            };
            if !output.status.success() {
                let _ = fs::remove_dir_all(&path);
                bail!(
                    "git worktree add --detach failed: {}",
                    String::from_utf8_lossy(&output.stderr)
                );
            }
            let exec_root = path.canonicalize().unwrap_or(path.clone());
            (
                exec_root,
                Some(Worktree {
                    repo: root.to_path_buf(),
                    path,
                    removed: false,
                }),
            )
        } else {
            (root.to_path_buf(), None)
        }
    } else {
        (root.to_path_buf(), None)
    };

    let git = git_available(root, deadline).await?;
    let mut sources = Vec::new();
    if unavailable.is_none() {
        let mut remaining = 16 * 1024 * 1024u64;
        for source in &target.sources {
            let fingerprint = fingerprint_file(&exec_root, source, &mut remaining)?;
            let dirty = if revision.is_some() {
                workspace_differs_from_revision(
                    root,
                    revision.as_deref().unwrap(),
                    source,
                    deadline,
                )
                .await?
            } else if git {
                workspace_dirty(root, source, deadline).await?
            } else {
                true
            };
            sources.push(SourceIdentity {
                path: source.clone(),
                fingerprint,
                revision: revision.clone(),
                dirty,
                bytes_retained: false,
            });
        }
        if !git && revision.is_none() && !target.sources.is_empty() {
            warning = Some(
                "workspace is not a git work tree; declared source bytes are retained as untracked"
                    .into(),
            );
        }
    }

    Ok(Side {
        root: exec_root,
        target: target.clone(),
        revision,
        sources,
        _worktree: worktree,
        warning,
        unavailable,
    })
}

async fn workspace_differs_from_revision(
    root: &Path,
    revision: &str,
    source: &str,
    deadline: &Deadline,
) -> Result<bool> {
    let abs = root.join(source);
    if !abs.is_file() {
        return Ok(false);
    }
    let Some(committed) = blob_id(root, &format!("{revision}:{source}"), deadline).await? else {
        return Ok(true);
    };
    Ok(file_blob_id(&abs, deadline).await? != committed)
}

async fn workspace_dirty(root: &Path, source: &str, deadline: &Deadline) -> Result<bool> {
    let abs = root.join(source);
    if !abs.is_file() {
        return Ok(true);
    }
    let spec = format!("HEAD:{source}");
    let Some(head) = blob_id(root, &spec, deadline).await? else {
        return Ok(true);
    };
    Ok(file_blob_id(&abs, deadline).await? != head)
}

impl RunCtx<'_> {
    fn cancelled(&self) -> bool {
        self.cancel.load(Ordering::Acquire)
    }

    fn over_budget(&self) -> bool {
        self.executions as usize >= self.max_executions || self.started.elapsed() >= self.timeout
    }

    fn remaining_ms(&self) -> u64 {
        self.timeout
            .saturating_sub(self.started.elapsed())
            .as_millis() as u64
    }

    async fn accept(&mut self, candidate: &Value) -> Result<bool> {
        if self.reduction_stopped || self.cancelled() || self.over_budget() {
            self.reduction_stopped = true;
            return Ok(false);
        }
        let result = eval_case(self, candidate).await?;
        match result.relation {
            Relation::Difference => {
                self.last_before = result.before.first().cloned();
                self.last_after = result.after.first().cloned();
                Ok(true)
            }
            Relation::Cancelled | Relation::Budget => {
                self.reduction_stopped = true;
                Ok(false)
            }
            _ => Ok(false),
        }
    }
}

async fn eval_case(ctx: &mut RunCtx<'_>, input: &Value) -> Result<CaseResult> {
    let mut before = Vec::new();
    let mut after = Vec::new();
    for _ in 0..ctx.repeat {
        if ctx.cancelled() {
            return Ok(CaseResult {
                input: input.clone(),
                relation: Relation::Cancelled,
                before,
                after,
            });
        }
        if ctx.over_budget() {
            return Ok(CaseResult {
                input: input.clone(),
                relation: Relation::Budget,
                before,
                after,
            });
        }
        before.push(execute_side(ctx, true, input).await?);
        if ctx.cancelled() {
            return Ok(CaseResult {
                input: input.clone(),
                relation: Relation::Cancelled,
                before,
                after,
            });
        }
        if ctx.over_budget() {
            return Ok(CaseResult {
                input: input.clone(),
                relation: Relation::Budget,
                before,
                after,
            });
        }
        after.push(execute_side(ctx, false, input).await?);
    }
    let relation = classify(&before, &after, &ctx.policy);
    Ok(CaseResult {
        input: input.clone(),
        relation,
        before,
        after,
    })
}

async fn execute_side(ctx: &mut RunCtx<'_>, before: bool, input: &Value) -> Result<Observation> {
    let remaining = ctx.remaining_ms();
    if remaining == 0 || ctx.executions as usize >= ctx.max_executions {
        ctx.reduction_stopped = true;
        return Ok(blank_observation("timeout"));
    }
    let mut limits = ctx.limits.clone();
    limits.timeout_ms = limits.timeout_ms.min(remaining).max(1);
    ctx.executions += 1;
    let side = if before { ctx.before } else { ctx.after };
    match execute::run(&side.root, &side.target, input, &limits, ctx.cancel.clone()).await {
        Ok(obs) => Ok(obs),
        Err(err) => Ok(Observation {
            outcome: "failed".into(),
            exit_code: None,
            output: None,
            stdout: String::new(),
            stderr: err.to_string(),
            elapsed_ms: 0,
            source_fingerprints: BTreeMap::new(),
            source_freshness: "unknown".into(),
            dependency_completeness: execute::DEPENDENCY_COMPLETENESS.into(),
            containment: containment_model().into(),
            output_fingerprint: None,
        }),
    }
}

fn blank_observation(outcome: &str) -> Observation {
    Observation {
        outcome: outcome.into(),
        exit_code: None,
        output: None,
        stdout: String::new(),
        stderr: String::new(),
        elapsed_ms: 0,
        source_fingerprints: BTreeMap::new(),
        source_freshness: "unknown".into(),
        dependency_completeness: execute::DEPENDENCY_COMPLETENESS.into(),
        containment: containment_model().into(),
        output_fingerprint: None,
    }
}

fn compact_observation(mut obs: Observation) -> Observation {
    obs.stdout = truncate_text(&obs.stdout, TEXT_CAP);
    obs.stderr = truncate_text(&obs.stderr, TEXT_CAP);
    if let Some(value) = &obs.output {
        let size = serde_json::to_vec(value)
            .map(|bytes| bytes.len())
            .unwrap_or(usize::MAX);
        if size > OUTPUT_KEEP {
            obs.output = None;
            obs.stderr = truncate_text(
                &format!(
                    "{} [parsed output omitted; fingerprint retained]",
                    obs.stderr
                ),
                TEXT_CAP,
            );
        }
    }
    obs
}

fn truncate_text(text: &str, cap: usize) -> String {
    if text.len() <= cap {
        return text.to_string();
    }
    let mut end = cap;
    while !text.is_char_boundary(end) && end > 0 {
        end -= 1;
    }
    format!("{}…", &text[..end])
}

fn stop_for_bound(
    id: &str,
    request: &CompareRequest,
    root: &Path,
    started: Instant,
    warnings: &[String],
    err: anyhow::Error,
) -> Result<CompareReport> {
    if let Some(hit) = err.downcast_ref::<BoundHit>() {
        let mut report = skeleton(id, request, root, started, 0, warnings);
        report.outcome = hit.0;
        report.elapsed_ms = started.elapsed().as_millis() as u64;
        report.warnings.push(match hit.0 {
            CompareOutcome::Cancelled => "cancelled during git preparation or worktree setup".into(),
            _ => "compare deadline expired during git preparation or worktree setup; the git process was killed and its time is not a completed comparison".into(),
        });
        report.freshness = "unknown".into();
        report.metrics.reproducible = false;
        return Ok(report);
    }
    Err(err)
}

fn observations_match(stored: &Observation, fresh: &Observation, policy: &ComparePolicy) -> bool {
    let output_same = match (&stored.output, &fresh.output) {
        (Some(left), Some(right)) => left == right,
        _ => {
            stored.output_fingerprint.is_some()
                && stored.output_fingerprint == fresh.output_fingerprint
        }
    };
    match policy.exit {
        ExitPolicy::RequireSuccess => {
            stored.outcome == "completed" && fresh.outcome == "completed" && output_same
        }
        ExitPolicy::CompareExit => {
            stored.outcome == fresh.outcome && stored.exit_code == fresh.exit_code && output_same
        }
    }
}

fn classify(before: &[Observation], after: &[Observation], policy: &ComparePolicy) -> Relation {
    if before
        .iter()
        .chain(after)
        .any(|obs| obs.outcome == "cancelled")
    {
        return Relation::Cancelled;
    }
    if before.is_empty() || after.is_empty() {
        return Relation::Budget;
    }
    let bsig: Vec<_> = before.iter().map(signature).collect();
    let asig: Vec<_> = after.iter().map(signature).collect();
    if bsig.iter().any(|sig| sig != &bsig[0]) || asig.iter().any(|sig| sig != &asig[0]) {
        return Relation::Nondeterministic;
    }
    match pair_relation(&before[0], &after[0], policy) {
        Pair::Same => Relation::Same,
        Pair::Difference => Relation::Difference,
        Pair::Unsupported => Relation::Unsupported,
    }
}

fn signature(obs: &Observation) -> (String, Option<i32>, Option<String>) {
    (
        obs.outcome.clone(),
        obs.exit_code,
        obs.output
            .as_ref()
            .map(|v| serde_json::to_string(v).unwrap_or_default()),
    )
}

enum Pair {
    Same,
    Difference,
    Unsupported,
}

fn comparable<'a>(obs: &'a Observation, policy: &ComparePolicy) -> Option<&'a Value> {
    match policy.exit {
        ExitPolicy::RequireSuccess if obs.outcome == "completed" => obs.output.as_ref(),
        ExitPolicy::CompareExit if matches!(obs.outcome.as_str(), "completed" | "failed") => {
            obs.output.as_ref()
        }
        _ => None,
    }
}

fn pair_relation(before: &Observation, after: &Observation, policy: &ComparePolicy) -> Pair {
    let (Some(left), Some(right)) = (comparable(before, policy), comparable(after, policy)) else {
        return Pair::Unsupported;
    };
    let exits_match = match policy.exit {
        ExitPolicy::RequireSuccess => true,
        ExitPolicy::CompareExit => before.exit_code == after.exit_code,
    };
    if left == right && exits_match {
        Pair::Same
    } else {
        Pair::Difference
    }
}

fn cost(value: &Value) -> (usize, usize, String) {
    fn nodes(value: &Value) -> usize {
        match value {
            Value::Array(items) => 1 + items.iter().map(nodes).sum::<usize>(),
            Value::Object(map) => 1 + map.values().map(nodes).sum::<usize>(),
            _ => 1,
        }
    }
    let text = serde_json::to_string(value).unwrap_or_default();
    (nodes(value), text.len(), text)
}

async fn minimize(initial: Value, ctx: &mut RunCtx<'_>) -> Result<Value> {
    let mut best = initial;
    Box::pin(shrink_at(&mut best, Vec::new(), ctx)).await?;
    Ok(best)
}

async fn shrink_at(best: &mut Value, path: Vec<Seg>, ctx: &mut RunCtx<'_>) -> Result<()> {
    if ctx.reduction_stopped {
        return Ok(());
    }
    let Some(node) = get_path(best, &path).cloned() else {
        return Ok(());
    };
    match node {
        Value::Array(items) => {
            let base = best.clone();
            let at = path.clone();
            let reduced = ddmin(items, ctx, |parts| {
                let mut trial = base.clone();
                set_path(&mut trial, &at, Value::Array(parts.to_vec()));
                trial
            })
            .await?;
            set_path(best, &path, Value::Array(reduced.clone()));
            for index in 0..reduced.len() {
                if ctx.reduction_stopped {
                    break;
                }
                let mut child = path.clone();
                child.push(Seg::Index(index));
                Box::pin(shrink_at(best, child, ctx)).await?;
            }
        }
        Value::Object(map) => {
            let mut fields: Vec<(String, Value)> = map.into_iter().collect();
            fields.sort_by(|a, b| a.0.cmp(&b.0));
            let base = best.clone();
            let at = path.clone();
            let reduced = ddmin(fields, ctx, |parts| {
                let mut obj = Map::new();
                for (key, value) in parts {
                    obj.insert(key.clone(), value.clone());
                }
                let mut trial = base.clone();
                set_path(&mut trial, &at, Value::Object(obj));
                trial
            })
            .await?;
            let mut obj = Map::new();
            for (key, value) in &reduced {
                obj.insert(key.clone(), value.clone());
            }
            set_path(best, &path, Value::Object(obj));
            for (key, _) in &reduced {
                if ctx.reduction_stopped {
                    break;
                }
                let mut child = path.clone();
                child.push(Seg::Key(key.clone()));
                Box::pin(shrink_at(best, child, ctx)).await?;
            }
        }
        Value::String(text) => {
            if text.chars().count() > 4096 || ctx.reduction_stopped {
                return Ok(());
            }
            let chars: Vec<char> = text.chars().collect();
            let base = best.clone();
            let at = path.clone();
            let reduced = ddmin(chars, ctx, |parts| {
                let mut trial = base.clone();
                set_path(&mut trial, &at, Value::String(parts.iter().collect()));
                trial
            })
            .await?;
            set_path(best, &path, Value::String(reduced.into_iter().collect()));
        }
        Value::Number(number) => shrink_number(best, &path, &number, ctx).await?,
        _ => {}
    }
    Ok(())
}

async fn shrink_number(
    best: &mut Value,
    path: &[Seg],
    number: &Number,
    ctx: &mut RunCtx<'_>,
) -> Result<()> {
    let candidates = number_candidates(number);
    let base = best.clone();
    for candidate in candidates {
        if ctx.reduction_stopped {
            break;
        }
        let mut trial = base.clone();
        set_path(&mut trial, path, candidate.clone());
        if ctx.accept(&trial).await? {
            set_path(best, path, candidate);
            break;
        }
    }
    Ok(())
}

fn number_candidates(number: &Number) -> Vec<Value> {
    let mut values = Vec::new();
    if let Some(n) = number.as_i64() {
        values.push(0);
        if n != 0 {
            values.push(n.signum());
        }
        let mut h = n;
        for _ in 0..63 {
            if h == 0 || h / 2 == h {
                break;
            }
            h /= 2;
            values.push(h);
        }
        if n > 0 {
            values.push(n - 1);
        } else if n < 0 {
            values.push(n + 1);
        }
        values.sort_by_key(|c| (c.unsigned_abs(), c.is_negative()));
        values.dedup();
        values
            .into_iter()
            .filter(|c| *c != n)
            .map(Value::from)
            .collect()
    } else if let Some(n) = number.as_u64() {
        let mut values = vec![0u64];
        let mut h = n;
        for _ in 0..63 {
            if h == 0 {
                break;
            }
            h /= 2;
            values.push(h);
        }
        values.sort_unstable();
        values.dedup();
        values
            .into_iter()
            .filter(|c| *c != n)
            .map(Value::from)
            .collect()
    } else if let Some(n) = number.as_f64() {
        let mut values = vec![0.0];
        let mut h = n;
        for _ in 0..16 {
            h /= 2.0;
            if !h.is_finite() {
                break;
            }
            values.push(h);
            if h == 0.0 {
                break;
            }
        }
        values
            .into_iter()
            .filter(|c| *c != n)
            .filter_map(Number::from_f64)
            .map(Value::Number)
            .collect()
    } else {
        Vec::new()
    }
}

async fn ddmin<T: Clone>(
    mut parts: Vec<T>,
    ctx: &mut RunCtx<'_>,
    rebuild: impl Fn(&[T]) -> Value,
) -> Result<Vec<T>> {
    if parts.len() < 2 {
        return Ok(parts);
    }
    let mut n = 2usize;
    while parts.len() >= 2 && !ctx.reduction_stopped {
        let subset_len = (parts.len() / n).max(1);
        let mut progressed = false;
        let mut start = 0;
        while start < parts.len() && !ctx.reduction_stopped {
            let end = (start + subset_len).min(parts.len());
            let complement: Vec<T> = parts[..start]
                .iter()
                .chain(&parts[end..])
                .cloned()
                .collect();
            if !complement.is_empty()
                && complement.len() < parts.len()
                && ctx.accept(&rebuild(&complement)).await?
            {
                parts = complement;
                n = n.saturating_sub(1).max(2);
                progressed = true;
                break;
            }
            start = end;
        }
        if progressed || ctx.reduction_stopped {
            continue;
        }
        start = 0;
        while start < parts.len() && !ctx.reduction_stopped {
            let end = (start + subset_len).min(parts.len());
            let subset = parts[start..end].to_vec();
            if !subset.is_empty()
                && subset.len() < parts.len()
                && ctx.accept(&rebuild(&subset)).await?
            {
                parts = subset;
                n = 2;
                progressed = true;
                break;
            }
            start = end;
        }
        if progressed {
            continue;
        }
        if n >= parts.len() {
            break;
        }
        n = (n * 2).min(parts.len());
    }
    Ok(parts)
}

fn get_path<'a>(value: &'a Value, path: &[Seg]) -> Option<&'a Value> {
    let mut current = value;
    for seg in path {
        current = match (seg, current) {
            (Seg::Index(index), Value::Array(items)) => items.get(*index)?,
            (Seg::Key(key), Value::Object(map)) => map.get(key)?,
            _ => return None,
        };
    }
    Some(current)
}

fn set_path(value: &mut Value, path: &[Seg], new: Value) {
    if path.is_empty() {
        *value = new;
        return;
    }
    match (&path[0], value) {
        (Seg::Index(index), Value::Array(items)) => {
            if let Some(child) = items.get_mut(*index) {
                set_path(child, &path[1..], new);
            }
        }
        (Seg::Key(key), Value::Object(map)) => {
            if let Some(child) = map.get_mut(key) {
                set_path(child, &path[1..], new);
            }
        }
        _ => {}
    }
}

fn freshness(root: &Path, before: &Side, after: &Side) -> String {
    for side in [before, after] {
        if side.revision.is_some() {
            continue;
        }
        let mut remaining = 16 * 1024 * 1024;
        for src in &side.sources {
            match fingerprint_file(root, &src.path, &mut remaining) {
                Ok(now) if now == src.fingerprint => {}
                _ => return "stale".into(),
            }
        }
    }
    "validated".into()
}

fn publish(
    root: &Path,
    request: &CompareRequest,
    before: &Side,
    after: &Side,
    report: &mut CompareReport,
    sample_before: Option<&Observation>,
    sample_after: Option<&Observation>,
) -> Result<()> {
    let dir = root.join(".checkweave").join("behavior").join(&report.id);
    fs::create_dir_all(dir.join("sources"))?;
    let mut stored = request.clone();
    stored.before_revision = report.before_revision.clone();
    stored.after_revision = report.after_revision.clone();
    let mut retention = request.retention.clone();
    if retention.max_total_bytes > WIRE_BUDGET {
        retention.max_total_bytes = WIRE_BUDGET;
        report
            .warnings
            .push("retention max_total_bytes clamped to the 8 MiB wire budget".into());
    }
    let argv = reproduction_argv(root, &report.id);
    let artifact_rel = format!(".checkweave/behavior/{}/cli-request.json", report.id);
    let evidence_rel = format!(".checkweave/behavior/{}", report.id);
    write_json(&dir.join("request.json"), &stored)?;
    let retained_input_for_witness = report
        .difference
        .as_ref()
        .map(|d| d.input.clone())
        .or_else(|| stored.inputs.first().cloned());
    let mut witness = stored.clone();
    if let Some(input) = &retained_input_for_witness {
        witness.inputs = vec![input.clone()];
        witness.generated = None;
        witness.reduce = false;
        witness.budgets.max_cases = 1;
    }
    write_json(&dir.join("witness-request.json"), &witness)?;
    write_json(
        &dir.join("cli-request.json"),
        &CliArtifact {
            description: "Replay the retained witness on the current workspace. argv is not a shell command. This reruns current sources; it is not a snapshot of the original process.",
            argv: &argv,
            request: &witness,
        },
    )?;

    let retained_input = report
        .difference
        .as_ref()
        .map(|d| d.input.clone())
        .or_else(|| stored.inputs.first().cloned());
    let original_input = report.difference.as_ref().map(|d| d.original_input.clone());
    if let Some(input) = &retained_input {
        write_json(&dir.join("input.json"), input)?;
    }
    if let Some(input) = &original_input
        && Some(input) != retained_input.as_ref()
    {
        write_json(&dir.join("original-input.json"), input)?;
    }
    if let Some(diff) = &report.difference {
        write_json(&dir.join("before-observation.json"), &diff.before)?;
        write_json(&dir.join("after-observation.json"), &diff.after)?;
    } else {
        if let Some(obs) = sample_before {
            write_json(&dir.join("before-observation.json"), obs)?;
        }
        if let Some(obs) = sample_after {
            write_json(&dir.join("after-observation.json"), obs)?;
        }
    }

    let mut files = vec![
        "request.json".to_string(),
        "witness-request.json".to_string(),
        "cli-request.json".to_string(),
    ];
    if retained_input.is_some() {
        files.push("input.json".into());
    }
    if original_input
        .as_ref()
        .is_some_and(|input| Some(input) != retained_input.as_ref())
    {
        files.push("original-input.json".into());
    }
    if report.difference.is_some() || sample_before.is_some() {
        files.push("before-observation.json".into());
    }
    if report.difference.is_some() || sample_after.is_some() {
        files.push("after-observation.json".into());
    }

    let mut identities = Vec::new();
    let mut seen = BTreeMap::<String, ()>::new();
    let mut copied = BTreeMap::<String, ()>::new();
    for src in before.sources.iter().chain(after.sources.iter()) {
        let identity_key = format!(
            "{}@{}",
            src.path,
            src.revision.as_deref().unwrap_or("workspace")
        );
        if seen.insert(identity_key, ()).is_some() {
            continue;
        }
        let mut copy = src.clone();
        if copied.insert(src.path.clone(), ()).is_none()
            && let Some(executed) = executed_source(before, after, src)
        {
            match retain_source(&dir, &executed, src, &mut files, retention.max_total_bytes) {
                Retain::Kept => copy.bytes_retained = true,
                Retain::Mismatch => {
                    report.missing_evidence.push(format!(
                        "source bytes do not match the executed fingerprint: {}",
                        src.path
                    ));
                    report.freshness = "stale".into();
                }
                Retain::TooLarge => {
                    report
                        .missing_evidence
                        .push(format!("source exceeds the retention budget: {}", src.path));
                }
                Retain::Missing => {
                    report.missing_evidence.push(format!(
                        "executed source missing before retention: {}",
                        src.path
                    ));
                }
            }
        }
        identities.push(copy);
    }
    for identity in &mut report.before_sources {
        if let Some(found) = identities
            .iter()
            .find(|item| item.path == identity.path && item.revision == identity.revision)
        {
            identity.bytes_retained = found.bytes_retained;
        }
    }
    for identity in &mut report.after_sources {
        if let Some(found) = identities
            .iter()
            .find(|item| item.path == identity.path && item.revision == identity.revision)
        {
            identity.bytes_retained = found.bytes_retained;
        }
    }

    let manifest = EvidenceManifest {
        id: report.id.clone(),
        operator_version: OPERATOR_VERSION.into(),
        outcome: report.outcome,
        files: files.clone(),
        source_fingerprints: identities,
        before_revision: report.before_revision.clone(),
        after_revision: report.after_revision.clone(),
        limits: report.per_execution.clone(),
        budgets: report.budgets.clone(),
        repeat: report.repeat,
        elapsed_ms: report.elapsed_ms,
        executions: report.executions,
        input: retained_input,
        original_input,
        created_unix_ms: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0),
        containment: report.containment.clone(),
        dependency_completeness: report.dependency_completeness.clone(),
    };
    let bytes = serde_json::to_vec_pretty(&manifest)?;
    let tmp = dir.join("manifest.json.tmp");
    fs::write(&tmp, &bytes)?;
    fs::File::open(&tmp)?.sync_all()?;
    fs::rename(&tmp, dir.join("manifest.json"))?;
    #[cfg(unix)]
    {
        let _ = fs::File::open(&dir).and_then(|file| file.sync_all());
    }
    report.reproduction = Some(ReproductionRef {
        id: report.id.clone(),
        evidence_dir: evidence_rel,
        argv,
        request_artifact: artifact_rel,
    });
    if let Some(extra) = prune_evidence(root, &report.id, &retention)? {
        report.warnings.extend(extra);
    }
    Ok(())
}

fn reproduction_argv(root: &Path, id: &str) -> Vec<String> {
    vec![
        "checkweave".into(),
        "--workspace".into(),
        root.canonicalize()
            .unwrap_or_else(|_| root.to_path_buf())
            .display()
            .to_string(),
        "replay".into(),
        "--kind".into(),
        "compare".into(),
        "--id".into(),
        id.to_string(),
    ]
}

enum Retain {
    Kept,
    Mismatch,
    TooLarge,
    Missing,
}

fn executed_source(before: &Side, after: &Side, src: &SourceIdentity) -> Option<PathBuf> {
    for side in [before, after] {
        if side.sources.iter().any(|item| {
            item.path == src.path
                && item.fingerprint == src.fingerprint
                && item.revision == src.revision
        }) {
            let path = side.root.join(&src.path);
            if path.is_file() {
                return Some(path);
            }
        }
    }
    None
}

fn retain_source(
    dir: &Path,
    executed: &Path,
    src: &SourceIdentity,
    files: &mut Vec<String>,
    budget: u64,
) -> Retain {
    let Ok(bytes) = fs::read(executed) else {
        return Retain::Missing;
    };
    let hash = blake3::hash(&bytes).to_hex().to_string();
    if hash != src.fingerprint {
        return Retain::Mismatch;
    }
    if bytes.len() as u64 > budget.min(WIRE_BUDGET) || bytes.len() as u64 > 1024 * 1024 {
        return Retain::TooLarge;
    }
    let dest = dir.join("sources").join(&src.path);
    if !dest.starts_with(dir.join("sources")) {
        return Retain::Missing;
    }
    if let Some(parent) = dest.parent()
        && fs::create_dir_all(parent).is_err()
    {
        return Retain::Missing;
    }
    let used = dir_size(dir).unwrap_or(0);
    if used.saturating_add(bytes.len() as u64) > budget.min(WIRE_BUDGET) {
        return Retain::TooLarge;
    }
    if fs::write(&dest, &bytes).is_err() {
        return Retain::Missing;
    }
    files.push(format!("sources/{}", src.path.replace('\\', "/")));
    Retain::Kept
}

fn write_json(path: &Path, value: &impl Serialize) -> Result<()> {
    let bytes = serde_json::to_vec_pretty(value)?;
    let tmp = path.with_extension("tmp");
    fs::write(&tmp, bytes)?;
    fs::File::open(&tmp)?.sync_all()?;
    fs::rename(&tmp, path)?;
    Ok(())
}

fn prune_evidence(
    root: &Path,
    keep: &str,
    limits: &RetentionLimits,
) -> Result<Option<Vec<String>>> {
    let dir = root.join(".checkweave").join("behavior");
    let mut entries = Vec::new();
    let mut warnings = Vec::new();
    for entry in fs::read_dir(&dir)? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().to_string();
        if name == keep {
            continue;
        }
        let path = entry.path();
        if !path.join("manifest.json").is_file() {
            let _ = fs::remove_dir_all(&path);
            warnings.push(format!("removed unpublished evidence {name}"));
            continue;
        }
        let modified = entry
            .metadata()?
            .modified()
            .unwrap_or(SystemTime::UNIX_EPOCH);
        entries.push((modified, dir_size(&path)?, path, name));
    }
    entries.sort_by_key(|entry| entry.0);
    let mut total =
        entries.iter().map(|entry| entry.1).sum::<u64>() + dir_size(&dir.join(keep)).unwrap_or(0);
    let mut count = entries.len() + 1;
    while (count > limits.max_entries || total > limits.max_total_bytes) && !entries.is_empty() {
        let (_, bytes, path, name) = entries.remove(0);
        fs::remove_dir_all(&path)?;
        total = total.saturating_sub(bytes);
        count -= 1;
        warnings.push(format!("expired evidence {name}"));
    }
    if total > limits.max_total_bytes {
        warnings.push("retention budget exceeded by the current evidence entry".into());
    }
    Ok(if warnings.is_empty() {
        None
    } else {
        Some(warnings)
    })
}

fn dir_size(path: &Path) -> Result<u64> {
    let mut total = 0u64;
    if path.is_file() {
        return Ok(path.metadata()?.len());
    }
    for entry in fs::read_dir(path)? {
        let entry = entry?;
        let kind = entry.file_type()?;
        if kind.is_file() {
            total += entry.metadata()?.len();
        } else if kind.is_dir() {
            total += dir_size(&entry.path())?;
        }
    }
    Ok(total)
}
