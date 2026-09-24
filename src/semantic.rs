//! Incremental semantic collection checks.
//!
//! Source membership and file hashes come from [`crate::sources`]. Judgment calls
//! go through [`crate::models::ModelProvider`]. Rows are cached by extracted text,
//! question schema, and the concrete provider identity. A warm cache does not
//! claim that a mutable model alias was reprobed.

use anyhow::{Context, Result, bail};
use blake3::Hasher;
use rusqlite::{Connection, OptionalExtension, params};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use crate::models::{
    ModelProvenance, ModelProvider, ModelQuestion, ModelResponse, ModelSettingsIdentity,
    ModelState, ModelStatus,
};
use crate::sources::{self, SourceHalt};
use crate::types::{Limits, SourceFingerprint, SourceRef};

pub const OPERATOR_VERSION: &str = "semantic-collection-v1";
const SERIALIZATION: &str = "json-pointer-utf8-v1";
const MAX_BATCH: usize = 32;
const EVIDENCE_BUDGET: usize = 32 * 1024;
/// Resource cap on one extracted string. This is not a token estimate.
pub const MAX_SERIALIZED_TEXT_BYTES: usize = 1_048_576;
/// Canonical worker protocol: 16 questions and 64 state-question pairs per call.
const WORKER_MAX_QUESTIONS: usize = 16;
const WORKER_MAX_PAIRS: usize = 64;
const WORKER_MAX_STATES: usize = 32;
/// Detail rows, leaving room for the protocol and run wrapper under 8 MiB.
const MAX_DETAIL_BYTES: usize = 1_048_576;
const MAX_REPORT_BYTES: usize = 7 * 1024 * 1024;
const MAX_PAYLOAD: i64 = 64 * 1024 * 1024;
const RECLAIM_BATCH: usize = 256;
const MAX_IDENTITY_ROWS: i64 = 64;
const MAX_ENTRIES: i64 = 100_000;
const HARD_MAX_FILES: usize = 100_000;
const HARD_MAX_BYTES: u64 = 1 << 30;
const HARD_MAX_RECORDS: usize = 1_000_000;
const HARD_MAX_RESULTS: usize = 5_000;
const HARD_MAX_TIMEOUT_MS: u64 = 600_000;
const MAX_POINTER: usize = 4096;
const TRACKED_ENV: &[&str] = &[
    "CHECKWEAVE_PYTHON",
    "CHECKWEAVE_MODEL_ID",
    "CHECKWEAVE_MODEL_REVISION",
    "HF_HUB_OFFLINE",
    "CUDA_VISIBLE_DEVICES",
];

fn default_batch_size() -> usize {
    8
}

/// Cold SemIf CPU readiness was about 25s and a two-row check about 46s.
pub const SEMANTIC_DEFAULT_TIMEOUT_MS: u64 = 180_000;

fn default_semantic_limits() -> Limits {
    Limits {
        timeout_ms: SEMANTIC_DEFAULT_TIMEOUT_MS,
        ..Limits::default()
    }
}

fn deserialize_semantic_limits<'de, D>(deserializer: D) -> Result<Limits, D::Error>
where
    D: serde::Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct RawLimits {
        #[serde(default)]
        max_files: Option<usize>,
        #[serde(default)]
        max_bytes: Option<u64>,
        #[serde(default)]
        max_records: Option<usize>,
        #[serde(default)]
        max_results: Option<usize>,
        #[serde(default)]
        timeout_ms: Option<u64>,
    }
    let raw = RawLimits::deserialize(deserializer)?;
    let base = Limits::default();
    Ok(Limits {
        max_files: raw.max_files.unwrap_or(base.max_files),
        max_bytes: raw.max_bytes.unwrap_or(base.max_bytes),
        max_records: raw.max_records.unwrap_or(base.max_records),
        max_results: raw.max_results.unwrap_or(base.max_results),
        timeout_ms: raw.timeout_ms.unwrap_or(SEMANTIC_DEFAULT_TIMEOUT_MS),
    })
}

fn semantic_limits_schema(generator: &mut schemars::SchemaGenerator) -> schemars::Schema {
    let mut schema = Limits::json_schema(generator);
    let defaults = default_semantic_limits();
    schema.insert(
        "default".to_owned(),
        serde_json::json!({
            "max_files": defaults.max_files,
            "max_bytes": defaults.max_bytes,
            "max_records": defaults.max_records,
            "max_results": defaults.max_results,
            "timeout_ms": defaults.timeout_ms,
        }),
    );
    schema
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SemanticCheckRequest {
    /// Workspace-relative globs. Membership uses the collection source rules.
    pub globs: Vec<String>,
    /// JSON Pointer to the string sent to the model. `""` is the whole value.
    pub text_pointer: String,
    pub questions: Vec<ModelQuestion>,
    /// Other fields follow collection `Limits`. Omitted `timeout_ms` is 180000.
    #[serde(
        default = "default_semantic_limits",
        deserialize_with = "deserialize_semantic_limits"
    )]
    #[schemars(schema_with = "semantic_limits_schema")]
    pub limits: Limits,
    /// States per model call. Values above 32 are rejected.
    #[serde(default = "default_batch_size")]
    pub batch_size: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, Default)]
pub struct SemanticCoverage {
    pub files: usize,
    pub records: usize,
    /// Record-question pairs with an explicit outcome.
    pub evaluated: usize,
    pub resolved: usize,
    pub unresolved: usize,
    pub unsupported: usize,
    /// Blank lines only. Invalid JSON, missing paths, and non-text values are counted elsewhere.
    pub skipped: usize,
    pub cache_hits: usize,
    pub cache_misses: usize,
    /// Calls to the model provider. Reused rows do not increment this.
    pub fresh_model_calls: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct SemanticDecision {
    pub source: SourceRef,
    pub state_id: String,
    pub question_id: String,
    /// `resolved`, `unresolved`, or `unsupported`.
    pub status: String,
    /// `fresh`, `reused`, or `not_sent`.
    pub observation: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    /// Number, boolean, or other worker score. Preserved as returned.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// Parsed record, or the original line when JSON parsing failed, within the evidence budget.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub record: Option<Value>,
    /// Extracted text within the evidence budget.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub omissions: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct SemanticReport {
    pub id: String,
    /// Always `model_judgment`.
    pub basis: String,
    /// `complete`, `partial`, or `cancelled`. A cancelled run is never `complete`.
    pub execution: String,
    /// `validated`, `stale`, or `unknown`.
    pub freshness: String,
    pub sources: Vec<SourceFingerprint>,
    pub generation: String,
    pub coverage: SemanticCoverage,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<ModelProvenance>,
    pub settings: ModelSettingsIdentity,
    pub decisions: Vec<SemanticDecision>,
    pub truncated: bool,
    pub warnings: Vec<String>,
    pub limitations: Vec<String>,
    pub elapsed_ms: u64,
    pub workspace: String,
    pub operator_version: String,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Halt {
    None,
    Cancel,
    Timeout,
    MaxFiles,
    MaxBytes,
    MaxRecords,
    Unstable,
}

struct Pointer {
    segments: Vec<String>,
}

enum Extract {
    Missing,
    NonText,
    Text(String),
}

struct Eligible {
    source: SourceRef,
    state_id: String,
    text: String,
    text_hash: String,
    record: Option<Value>,
    omissions: Vec<String>,
}

struct Accounted {
    source: SourceRef,
    state_id: String,
    status: &'static str,
    reason: String,
    record: Option<Value>,
    text: Option<String>,
    omissions: Vec<String>,
}

enum Order {
    Accounted(usize),
    Eligible(usize),
}

struct ScanOut {
    eligible: Vec<Eligible>,
    accounted: Vec<Accounted>,
    order: Vec<Order>,
    sources: Vec<SourceFingerprint>,
    coverage: SemanticCoverage,
    halt: Halt,
    warnings: Vec<String>,
    truncated: bool,
}

#[derive(Clone, Serialize, Deserialize)]
struct CachedJudgment {
    status: String,
    label: Option<String>,
    value: Option<Value>,
    reason: Option<String>,
}

#[derive(Clone)]
struct Pair {
    row: usize,
    question: usize,
    key: String,
    hit: Option<CachedJudgment>,
}

struct EnvIdentity {
    hash: String,
    limitations: Vec<String>,
}

pub async fn check(
    root: &Path,
    request: &SemanticCheckRequest,
    provider: &mut ModelProvider,
    cancel: Arc<AtomicBool>,
) -> Result<SemanticReport> {
    let started = Instant::now();
    validate_request(request)?;
    if !root.is_dir() {
        bail!("workspace root is not a directory: {}", root.display());
    }
    let settings = provider.settings_identity();
    let settings_fp = provider.settings_fingerprint().to_string();
    let source_fp = provider.source_fingerprint().to_string();
    let pointer = parse_pointer(&request.text_pointer)?;
    let question_json = question_canonical(&request.questions)?;
    let env = environment_identity();
    let root_buf = root.to_path_buf();
    let request_scan = request.clone();
    let cancel_scan = Arc::clone(&cancel);
    let scan = tokio::task::spawn_blocking(move || {
        scan_workspace(&root_buf, &request_scan, &pointer, started, &cancel_scan)
    })
    .await
    .context("semantic scan task")??;

    let mut limitations = env.limitations.clone();
    let db_path = semantic_db(root);
    let latch_id = latch_key(&settings_fp, &source_fp, &env.hash);
    let mut warnings = scan.warnings.clone();
    let mut halt = scan.halt;
    let mut fresh_calls = 0usize;
    let mut live: Option<ModelProvenance> = None;
    let mut fresh: HashMap<(usize, usize), CachedJudgment> = HashMap::new();
    let mut cacheable = false;
    let mut pairs = empty_pairs(&scan.eligible, &question_json);

    if matches!(
        halt,
        Halt::None | Halt::MaxRecords | Halt::MaxFiles | Halt::MaxBytes
    ) && cancel.load(Ordering::Relaxed)
    {
        halt = Halt::Cancel;
    }

    if !matches!(halt, Halt::Cancel | Halt::Timeout | Halt::Unstable) && !scan.eligible.is_empty() {
        let readiness =
            live_readiness(provider, &cancel, started, request.limits.timeout_ms).await?;
        if let Some(ready) = readiness {
            if let Err(reason) = provenance_reusable(&ready) {
                push_unique(&mut limitations, reason);
                push_unique(&mut limitations, "readiness_identity_not_reusable".into());
            } else {
                pairs = load_pairs(
                    &db_path,
                    &scan.eligible,
                    &question_json,
                    &env.hash,
                    &settings_fp,
                    &source_fp,
                    Some(&ready),
                )?;
                live = Some(ready);
            }
            let outcome = judge_misses(
                provider,
                request,
                &scan.eligible,
                &pairs,
                &cancel,
                started,
                request.limits.timeout_ms,
                live.clone(),
            )
            .await?;
            fresh_calls = outcome.calls;
            warnings.extend(outcome.warnings);
            fresh = outcome.fresh;
            live = outcome.provenance.or(live);
            cacheable = outcome.cacheable;
            if outcome.timed_out {
                halt = Halt::Timeout;
            }
            if outcome.cancelled {
                halt = Halt::Cancel;
            }
            if let Some(extra) = outcome.limitation {
                push_unique(&mut limitations, extra);
            }
            if outcome.invalidated_hits {
                for pair in &mut pairs {
                    pair.hit = None;
                }
            }
        } else {
            push_unique(
                &mut limitations,
                "readiness_api_unavailable_probed_one_row".into(),
            );
            let probe: Vec<Pair> = pairs.iter().filter(|pair| pair.row == 0).cloned().collect();
            let outcome = judge_misses(
                provider,
                request,
                &scan.eligible,
                &probe,
                &cancel,
                started,
                request.limits.timeout_ms,
                None,
            )
            .await?;
            fresh_calls = outcome.calls;
            warnings.extend(outcome.warnings);
            fresh = outcome.fresh;
            live = outcome.provenance;
            if outcome.timed_out {
                halt = Halt::Timeout;
            }
            if outcome.cancelled {
                halt = Halt::Cancel;
            }
            if !matches!(halt, Halt::Cancel | Halt::Timeout | Halt::Unstable)
                && let Some(ready) = live.clone()
                && provenance_reusable(&ready).is_ok()
            {
                let mut rest = load_pairs(
                    &db_path,
                    &scan.eligible,
                    &question_json,
                    &env.hash,
                    &settings_fp,
                    &source_fp,
                    Some(&ready),
                )?;
                rest.retain(|pair| pair.row > 0);
                let more = judge_misses(
                    provider,
                    request,
                    &scan.eligible,
                    &rest,
                    &cancel,
                    started,
                    request.limits.timeout_ms,
                    Some(ready),
                )
                .await?;
                fresh_calls += more.calls;
                warnings.extend(more.warnings);
                fresh.extend(more.fresh);
                live = more.provenance.or(live);
                cacheable = more.cacheable;
                if more.timed_out {
                    halt = Halt::Timeout;
                }
                if more.cancelled {
                    halt = Halt::Cancel;
                }
                if more.invalidated_hits {
                    for pair in &mut rest {
                        pair.hit = None;
                    }
                }
                pairs.retain(|pair| pair.row == 0);
                pairs.append(&mut rest);
            }
        }
    } else if scan.eligible.is_empty() && halt == Halt::None {
        push_unique(&mut limitations, "no_model_observation".into());
    }
    if live.is_none() {
        for pair in &mut pairs {
            pair.hit = None;
        }
    }
    if let Some(provenance) = live.as_ref()
        && let Err(reason) = provenance_reusable(provenance)
    {
        push_unique(&mut limitations, reason);
        cacheable = false;
    }

    let mut coverage = scan.coverage.clone();
    coverage.fresh_model_calls = fresh_calls;
    let mut decisions = Vec::new();
    let mut store = Vec::new();
    let mut truncated = scan.truncated;
    let identity = match live.as_ref() {
        Some(provenance) if cacheable || pairs.iter().any(|pair| pair.hit.is_some()) => Some(
            identity_json(&settings_fp, &source_fp, &env.hash, provenance)?,
        ),
        _ => None,
    };
    emit_decisions(
        request,
        &scan,
        &question_json,
        &pairs,
        &fresh,
        identity.as_deref(),
        cacheable,
        halt,
        &mut coverage,
        &mut decisions,
        &mut store,
        &mut truncated,
        &mut warnings,
    );

    if !matches!(halt, Halt::None | Halt::Cancel) {
        warnings.push(halt_warning(halt).into());
    }
    if halt == Halt::Cancel {
        warnings.push("cancelled".into());
    }

    let (execution, mut freshness) = match halt {
        Halt::Cancel => ("cancelled", "unknown"),
        Halt::None => ("complete", "validated"),
        Halt::Unstable => ("partial", "stale"),
        _ => ("partial", "validated"),
    };
    if execution == "cancelled" {
        freshness = "unknown";
    }
    cap_published_details(&mut decisions, &scan.sources, &mut truncated, &mut warnings);
    let sources = scan.sources;
    let recheck_execution = if execution == "cancelled" {
        "partial"
    } else {
        execution
    };
    let deadline = started + Duration::from_millis(request.limits.timeout_ms);
    let verdict = {
        let root_buf = root.to_path_buf();
        let globs = request.globs.clone();
        let sources_buf = sources.clone();
        let cancel_buf = Arc::clone(&cancel);
        let execution_buf = recheck_execution.to_string();
        tokio::task::spawn_blocking(move || {
            recheck_sources(
                &root_buf,
                &globs,
                &execution_buf,
                &sources_buf,
                deadline,
                &cancel_buf,
            )
        })
        .await
        .context("semantic recheck task")??
    };
    match verdict {
        SourceVerdict::Current => {}
        SourceVerdict::Stale => {
            freshness = "stale";
            warnings.push("stale: inputs changed before publication".into());
        }
        SourceVerdict::Unknown => {
            freshness = "unknown";
            warnings.push("freshness recheck did not finish within the deadline".into());
        }
    }
    if serde_json::to_string(&sources).unwrap_or_default().len()
        + serde_json::to_string(&decisions).unwrap_or_default().len()
        > MAX_REPORT_BYTES
    {
        freshness = "unknown";
        warnings.push(
            "report metadata exceeds the serialized budget; full source list is retained but the scan is not claimed validated"
                .into(),
        );
    }

    let report = SemanticReport {
        id: uuid::Uuid::new_v4().to_string(),
        basis: "model_judgment".into(),
        execution: execution.into(),
        freshness: freshness.into(),
        generation: sources::generation_of(&sources),
        coverage,
        model: live,
        settings,
        decisions,
        truncated,
        warnings,
        limitations,
        elapsed_ms: started.elapsed().as_millis() as u64,
        workspace: root.display().to_string(),
        operator_version: OPERATOR_VERSION.into(),
        sources,
    };
    let request_json = serde_json::to_string(request).context("serialize semantic request")?;
    let persist_report = report.clone();
    let persist_path = db_path.clone();
    let persist_store = store;
    let persist_latch = latch_id;
    let persist_provenance = report
        .model
        .clone()
        .filter(|item| cacheable && provenance_reusable(item).is_ok());
    tokio::task::spawn_blocking(move || {
        persist(
            &persist_path,
            &request_json,
            &persist_report,
            &persist_store,
            &persist_latch,
            persist_provenance.as_ref(),
        )
    })
    .await
    .context("semantic persist task")??;
    Ok(report)
}

/// Reload a report and re-read source bytes. A missing id is `Ok(None)`.
/// A stored row that cannot be decoded is an error.
pub fn evidence(root: &Path, id: &str) -> Result<Option<SemanticReport>> {
    if id.is_empty() || id.len() > 128 || id.chars().any(|ch| ch.is_control()) {
        bail!("invalid semantic evidence id");
    }
    let path = semantic_db(root);
    if !path.exists() {
        return Ok(None);
    }
    let conn = open_db(&path)?;
    let row = conn
        .query_row(
            "SELECT request_json, report_json FROM reports WHERE id = ?1",
            params![id],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
        )
        .optional()
        .context("sqlite read semantic evidence")?;
    let Some((request_json, report_json)) = row else {
        return Ok(None);
    };
    let request: SemanticCheckRequest = serde_json::from_str(&request_json)
        .context("semantic evidence request expired or corrupt")?;
    let mut report: SemanticReport = serde_json::from_str(&report_json)
        .context("semantic evidence report expired or corrupt")?;
    let execution = if report.execution == "cancelled" {
        "partial"
    } else {
        report.execution.as_str()
    };
    if let Some(stored) = load_stored_sources(&conn, &report.id)?
        && !stored.is_empty()
    {
        report.sources = stored;
    }
    let deadline = Instant::now() + Duration::from_millis(2_000);
    let cancel = AtomicBool::new(false);
    let current = match recheck_sources(
        root,
        &request.globs,
        execution,
        &report.sources,
        deadline,
        &cancel,
    ) {
        Ok(SourceVerdict::Current) if report.execution == "cancelled" => "unknown".to_string(),
        Ok(SourceVerdict::Current) => "validated".to_string(),
        Ok(SourceVerdict::Stale) => "stale".to_string(),
        Ok(SourceVerdict::Unknown) => "unknown".to_string(),
        Err(err) => return Err(err),
    };
    if current != report.freshness {
        report.freshness = current;
        let encoded =
            serde_json::to_string(&report).context("serialize refreshed semantic report")?;
        conn.execute(
            "UPDATE reports SET freshness = ?1, report_json = ?2, payload_bytes = ?3 WHERE id = ?4",
            params![
                report.freshness,
                encoded,
                (request_json.len() + encoded.len()) as i64,
                report.id
            ],
        )
        .context("sqlite update semantic freshness")?;
    }
    Ok(Some(report))
}

struct JudgeOutcome {
    calls: usize,
    provenance: Option<ModelProvenance>,
    fresh: HashMap<(usize, usize), CachedJudgment>,
    warnings: Vec<String>,
    timed_out: bool,
    cancelled: bool,
    invalidated_hits: bool,
    cacheable: bool,
    limitation: Option<String>,
}

async fn live_readiness(
    provider: &mut ModelProvider,
    cancel: &Arc<AtomicBool>,
    started: Instant,
    timeout_ms: u64,
) -> Result<Option<ModelProvenance>> {
    if cancel.load(Ordering::Relaxed) {
        return Ok(None);
    }
    let remain = timeout_ms
        .saturating_sub(started.elapsed().as_millis() as u64)
        .max(1);
    match provider.readiness(remain, Arc::clone(cancel)).await {
        Ok(provenance) => Ok(Some(provenance)),
        Err(err) if err.to_string().contains("readiness is the local") => Ok(None),
        Err(err) => Err(err),
    }
}

#[allow(clippy::too_many_arguments)]
async fn judge_misses(
    provider: &mut ModelProvider,
    request: &SemanticCheckRequest,
    rows: &[Eligible],
    pairs: &[Pair],
    cancel: &Arc<AtomicBool>,
    started: Instant,
    timeout_ms: u64,
    seed: Option<ModelProvenance>,
) -> Result<JudgeOutcome> {
    let mut outcome = JudgeOutcome {
        calls: 0,
        provenance: seed,
        fresh: HashMap::new(),
        warnings: Vec::new(),
        timed_out: false,
        cancelled: false,
        invalidated_hits: false,
        cacheable: false,
        limitation: None,
    };
    let pending = miss_groups(pairs);
    if pending.is_empty() {
        outcome.cacheable = outcome
            .provenance
            .as_ref()
            .is_some_and(|item| provenance_reusable(item).is_ok());
        return Ok(outcome);
    }
    drive_groups(
        provider,
        request,
        rows,
        &pending,
        cancel,
        started,
        timeout_ms,
        &mut outcome,
    )
    .await?;
    let Some(provenance) = outcome.provenance.clone() else {
        return Ok(outcome);
    };
    if let Err(reason) = provenance_reusable(&provenance) {
        outcome.limitation = Some(reason);
        outcome.cacheable = false;
        return Ok(outcome);
    }
    // A live call that reports a different device, revision, or runtime rejudges
    // rows that were only hits under the previous latch.
    if pairs.iter().any(|pair| pair.hit.is_some())
        && !outcome.cancelled
        && !outcome.timed_out
        && let Some(live) = outcome.provenance.clone()
    {
        let hit_rows = hit_row_indexes(pairs);
        if !hit_rows.is_empty() && latched_identity_differs(pairs, &live) {
            outcome.invalidated_hits = true;
            outcome.limitation = Some("model_identity_changed_rechecked_hits".into());
            let group = hit_groups(&hit_rows, request.questions.len());
            drive_groups(
                provider,
                request,
                rows,
                &group,
                cancel,
                started,
                timeout_ms,
                &mut outcome,
            )
            .await?;
        }
    }
    outcome.cacheable = outcome
        .provenance
        .as_ref()
        .is_some_and(|item| provenance_reusable(item).is_ok())
        && !outcome
            .warnings
            .iter()
            .any(|warning| warning.contains("changed again"));
    Ok(outcome)
}

fn latched_identity_differs(pairs: &[Pair], live: &ModelProvenance) -> bool {
    let marker = provenance_marker(live);
    pairs
        .iter()
        .filter(|pair| pair.hit.is_some())
        .any(|pair| !pair.key.ends_with(&marker))
}

fn provenance_marker(live: &ModelProvenance) -> String {
    let mut value = serde_json::to_value(live).unwrap_or(Value::Null);
    if let Some(obj) = value.as_object_mut() {
        obj.remove("usage");
    }
    blake3::hash(serde_json::to_string(&value).unwrap_or_default().as_bytes())
        .to_hex()
        .to_string()
}

#[allow(clippy::too_many_arguments)]
async fn drive_groups(
    provider: &mut ModelProvider,
    request: &SemanticCheckRequest,
    rows: &[Eligible],
    groups: &BTreeMap<Vec<usize>, Vec<usize>>,
    cancel: &Arc<AtomicBool>,
    started: Instant,
    timeout_ms: u64,
    outcome: &mut JudgeOutcome,
) -> Result<()> {
    for (questions, row_indexes) in groups {
        for (qchunk, rows_chunk) in call_slices(questions, row_indexes, request.batch_size) {
            if cancel.load(Ordering::Relaxed) {
                outcome.cancelled = true;
                return Ok(());
            }
            let elapsed = started.elapsed().as_millis() as u64;
            if elapsed >= timeout_ms {
                outcome.timed_out = true;
                return Ok(());
            }
            let remain = (timeout_ms - elapsed).clamp(1, 3_600_000);
            let states: Vec<ModelState> = rows_chunk
                .iter()
                .map(|index| ModelState {
                    id: rows[*index].state_id.clone(),
                    text: rows[*index].text.clone(),
                })
                .collect();
            let asked: Vec<ModelQuestion> = qchunk
                .iter()
                .map(|index| request.questions[*index].clone())
                .collect();
            let response =
                match evaluate_stable(provider, states, asked.clone(), remain, cancel, outcome)
                    .await?
                {
                    Some(response) => response,
                    None => return Ok(()),
                };
            absorb_response(rows, &rows_chunk, &qchunk, &asked, response, outcome);
        }
    }
    Ok(())
}

/// Split one miss group into worker-sized calls. Every pair is scheduled once.
fn call_slices(
    questions: &[usize],
    rows: &[usize],
    batch_size: usize,
) -> Vec<(Vec<usize>, Vec<usize>)> {
    let mut plans = Vec::new();
    let batch = batch_size.clamp(1, WORKER_MAX_STATES);
    for qchunk in questions.chunks(WORKER_MAX_QUESTIONS) {
        let qlen = qchunk.len().max(1);
        let by_pairs = (WORKER_MAX_PAIRS / qlen).max(1);
        let width = by_pairs.min(WORKER_MAX_STATES).min(batch).max(1);
        for row_chunk in rows.chunks(width) {
            plans.push((qchunk.to_vec(), row_chunk.to_vec()));
        }
    }
    plans
}

async fn evaluate_stable(
    provider: &mut ModelProvider,
    states: Vec<ModelState>,
    asked: Vec<ModelQuestion>,
    remain: u64,
    cancel: &Arc<AtomicBool>,
    outcome: &mut JudgeOutcome,
) -> Result<Option<ModelResponse>> {
    let first = match provider
        .evaluate(states.clone(), asked.clone(), remain, Arc::clone(cancel))
        .await
    {
        Ok(response) => response,
        Err(err) => return eval_stop(err, cancel, outcome),
    };
    let Some(previous) = outcome.provenance.clone() else {
        return Ok(Some(first));
    };
    if provenance_marker(&previous) == provenance_marker(&first.provenance) {
        return Ok(Some(first));
    }
    outcome
        .warnings
        .push("model identity changed between batches; recomputing the batch".into());
    let second = match provider
        .evaluate(states, asked, remain, Arc::clone(cancel))
        .await
    {
        Ok(response) => response,
        Err(err) => return eval_stop(err, cancel, outcome),
    };
    if provenance_marker(&first.provenance) != provenance_marker(&second.provenance) {
        outcome
            .warnings
            .push("model identity changed again; batch left unresolved".into());
        outcome.cacheable = false;
        outcome.provenance = Some(second.provenance);
        return Ok(None);
    }
    outcome.provenance = Some(second.provenance.clone());
    Ok(Some(second))
}

fn eval_stop(
    err: anyhow::Error,
    cancel: &AtomicBool,
    outcome: &mut JudgeOutcome,
) -> Result<Option<ModelResponse>> {
    let message = format!("{err:#}");
    if cancel.load(Ordering::Relaxed) || message.contains("cancelled") {
        outcome.cancelled = true;
        Ok(None)
    } else if message.contains("timed out") {
        outcome.timed_out = true;
        Ok(None)
    } else {
        Err(err)
    }
}

fn absorb_response(
    rows: &[Eligible],
    chunk: &[usize],
    question_indexes: &[usize],
    asked: &[ModelQuestion],
    response: ModelResponse,
    outcome: &mut JudgeOutcome,
) {
    outcome.calls += 1;
    if let Some(previous) = &outcome.provenance
        && provenance_marker(previous) != provenance_marker(&response.provenance)
    {
        outcome
            .warnings
            .push("model identity changed between batches".into());
    }
    outcome.provenance = Some(response.provenance);
    let mut expected = BTreeSet::new();
    for index in chunk {
        for question in asked {
            expected.insert((rows[*index].state_id.clone(), question.id().to_string()));
        }
    }
    let mut seen = BTreeSet::new();
    for result in response.results {
        let key = (result.state_id.clone(), result.question_id.clone());
        if !seen.insert(key.clone()) {
            outcome
                .warnings
                .push(format!("duplicate model result for {}/{}", key.0, key.1));
            continue;
        }
        if !expected.contains(&key) {
            outcome
                .warnings
                .push(format!("model result for unknown pair {}/{}", key.0, key.1));
            continue;
        }
        let Some(row) = chunk
            .iter()
            .copied()
            .find(|index| rows[*index].state_id == result.state_id)
        else {
            continue;
        };
        let Some(question) = question_indexes.iter().copied().find(|index| {
            asked.iter().any(|item| item.id() == result.question_id)
                && asked_id(asked, question_indexes, *index) == result.question_id
        }) else {
            outcome.warnings.push(format!(
                "model result for unknown question {}",
                result.question_id
            ));
            continue;
        };
        outcome.fresh.insert(
            (row, question),
            CachedJudgment {
                status: status_name(result.status).to_string(),
                label: result.label,
                value: result.value,
                reason: result.reason,
            },
        );
    }
}

fn asked_id(asked: &[ModelQuestion], question_indexes: &[usize], index: usize) -> String {
    question_indexes
        .iter()
        .position(|item| *item == index)
        .and_then(|pos| asked.get(pos))
        .map(|question| question.id().to_string())
        .unwrap_or_default()
}

fn miss_groups(pairs: &[Pair]) -> BTreeMap<Vec<usize>, Vec<usize>> {
    let mut by_row: BTreeMap<usize, Vec<usize>> = BTreeMap::new();
    for pair in pairs {
        if pair.hit.is_none() {
            by_row.entry(pair.row).or_default().push(pair.question);
        }
    }
    let mut groups: BTreeMap<Vec<usize>, Vec<usize>> = BTreeMap::new();
    for (row, mut questions) in by_row {
        questions.sort_unstable();
        groups.entry(questions).or_default().push(row);
    }
    groups
}

fn hit_row_indexes(pairs: &[Pair]) -> Vec<usize> {
    let mut rows = Vec::new();
    for pair in pairs {
        if pair.hit.is_some() && !rows.contains(&pair.row) {
            rows.push(pair.row);
        }
    }
    rows
}

fn hit_groups(rows: &[usize], question_count: usize) -> BTreeMap<Vec<usize>, Vec<usize>> {
    let mut groups = BTreeMap::new();
    if !rows.is_empty() && question_count > 0 {
        groups.insert((0..question_count).collect(), rows.to_vec());
    }
    groups
}

#[allow(clippy::too_many_arguments)]
fn emit_decisions(
    request: &SemanticCheckRequest,
    scan: &ScanOut,
    question_json: &[String],
    pairs: &[Pair],
    fresh: &HashMap<(usize, usize), CachedJudgment>,
    identity: Option<&str>,
    cacheable: bool,
    halt: Halt,
    coverage: &mut SemanticCoverage,
    decisions: &mut Vec<SemanticDecision>,
    store: &mut Vec<(String, CachedJudgment)>,
    truncated: &mut bool,
    warnings: &mut Vec<String>,
) {
    let limit = request.limits.max_results;
    for item in &scan.order {
        match item {
            Order::Accounted(index) => {
                let row = &scan.accounted[*index];
                for question in &request.questions {
                    bump(coverage, row.status);
                    push_decision(
                        decisions,
                        limit,
                        truncated,
                        SemanticDecision {
                            source: row.source.clone(),
                            state_id: row.state_id.clone(),
                            question_id: question.id().to_string(),
                            status: row.status.into(),
                            observation: "not_sent".into(),
                            label: None,
                            value: None,
                            reason: Some(row.reason.clone()),
                            record: row.record.clone(),
                            text: row.text.clone(),
                            omissions: row.omissions.clone(),
                        },
                    );
                }
            }
            Order::Eligible(index) => {
                let row = &scan.eligible[*index];
                for (qindex, question) in request.questions.iter().enumerate() {
                    let pair = pairs
                        .iter()
                        .find(|pair| pair.row == *index && pair.question == qindex);
                    let (judgment, observation, cache_hit) =
                        if let Some(hit) = pair.and_then(|pair| pair.hit.clone()) {
                            (hit, "reused", true)
                        } else if let Some(found) = fresh.get(&(*index, qindex)) {
                            (found.clone(), "fresh", false)
                        } else {
                            let reason = if halt == Halt::Cancel {
                                "cancelled"
                            } else if halt == Halt::Timeout {
                                "evaluation timed out"
                            } else {
                                "model result missing"
                            };
                            if reason == "model result missing" {
                                warnings.push(format!(
                                    "missing model result for state {} question {}",
                                    row.state_id,
                                    question.id()
                                ));
                            }
                            (
                                CachedJudgment {
                                    status: "unresolved".into(),
                                    label: None,
                                    value: None,
                                    reason: Some(reason.into()),
                                },
                                "not_sent",
                                false,
                            )
                        };
                    if cache_hit {
                        coverage.cache_hits += 1;
                    } else if observation == "fresh" {
                        coverage.cache_misses += 1;
                    }
                    bump(coverage, &judgment.status);
                    if observation == "fresh"
                        && cacheable
                        && let Some(identity) = identity
                    {
                        let key = cache_key(&row.text_hash, &question_json[qindex], identity);
                        store.push((key, judgment.clone()));
                    }
                    let mut omissions = row.omissions.clone();
                    let text = if row.text.len() <= EVIDENCE_BUDGET {
                        Some(row.text.clone())
                    } else {
                        omissions.push("text_over_evidence_budget".into());
                        None
                    };
                    push_decision(
                        decisions,
                        limit,
                        truncated,
                        SemanticDecision {
                            source: row.source.clone(),
                            state_id: row.state_id.clone(),
                            question_id: question.id().to_string(),
                            status: judgment.status,
                            observation: observation.into(),
                            label: judgment.label,
                            value: judgment.value,
                            reason: judgment.reason,
                            record: row.record.clone(),
                            text,
                            omissions,
                        },
                    );
                }
            }
        }
    }
}

fn push_decision(
    decisions: &mut Vec<SemanticDecision>,
    limit: usize,
    truncated: &mut bool,
    decision: SemanticDecision,
) {
    if decisions.len() < limit {
        decisions.push(decision);
    } else {
        *truncated = true;
    }
}

fn bump(coverage: &mut SemanticCoverage, status: &str) {
    coverage.evaluated += 1;
    match status {
        "resolved" => coverage.resolved += 1,
        "unsupported" => coverage.unsupported += 1,
        _ => coverage.unresolved += 1,
    }
}

fn scan_workspace(
    root: &Path,
    request: &SemanticCheckRequest,
    pointer: &Pointer,
    started: Instant,
    cancel: &AtomicBool,
) -> Result<ScanOut> {
    let deadline = started + Duration::from_millis(request.limits.timeout_ms);
    let mut out = ScanOut {
        eligible: Vec::new(),
        accounted: Vec::new(),
        order: Vec::new(),
        sources: Vec::new(),
        coverage: SemanticCoverage::default(),
        halt: Halt::None,
        warnings: Vec::new(),
        truncated: false,
    };
    if cancel.load(Ordering::Relaxed) {
        out.halt = Halt::Cancel;
        return Ok(out);
    }
    if Instant::now() >= deadline {
        out.halt = Halt::Timeout;
        out.truncated = true;
        return Ok(out);
    }
    let listed = sources::enumerate_bounded(root, &request.globs, Some(deadline), Some(cancel))
        .context("enumerate semantic inputs")?;
    if !listed.complete {
        out.halt = if cancel.load(Ordering::Relaxed) {
            Halt::Cancel
        } else {
            Halt::Timeout
        };
        out.warnings
            .push("source walk stopped before completion".into());
        out.truncated = true;
        return Ok(out);
    }
    let members = listed.paths;
    let mut bytes_left = request.limits.max_bytes;
    let mut records_left = request.limits.max_records;
    for rel in &members {
        if cancel.load(Ordering::Relaxed) {
            out.halt = Halt::Cancel;
            break;
        }
        if Instant::now() >= deadline {
            out.halt = Halt::Timeout;
            break;
        }
        if out.coverage.files >= request.limits.max_files {
            out.halt = Halt::MaxFiles;
            break;
        }
        if records_left == 0 {
            out.halt = Halt::MaxRecords;
            break;
        }
        let abs = sources::safe_join(root, rel)?;
        let meta = match fs::symlink_metadata(&abs) {
            Ok(meta) => meta,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                out.halt = Halt::Unstable;
                break;
            }
            Err(err) => return Err(err).with_context(|| format!("stat {}", abs.display())),
        };
        if meta.file_type().is_symlink() || !meta.file_type().is_file() {
            out.halt = Halt::Unstable;
            break;
        }
        let len = meta.len();
        if len > bytes_left {
            out.halt = Halt::MaxBytes;
            break;
        }
        let scanned = sources::scan_file(&abs, len, records_left, deadline, cancel)?;
        match scanned.halt {
            SourceHalt::Cancel => {
                out.halt = Halt::Cancel;
                break;
            }
            SourceHalt::Timeout => {
                out.halt = Halt::Timeout;
                break;
            }
            SourceHalt::Unstable => {
                out.halt = Halt::Unstable;
                break;
            }
            SourceHalt::None | SourceHalt::RecordBudget => {
                bytes_left = bytes_left.saturating_sub(scanned.bytes);
                out.coverage.files += 1;
                out.coverage.skipped += scanned.skipped_blank;
                out.sources.push(SourceFingerprint {
                    path: rel.clone(),
                    fingerprint: scanned.fingerprint.clone(),
                    bytes: scanned.bytes,
                });
                for record in scanned.records {
                    records_left = records_left.saturating_sub(1);
                    out.coverage.records += 1;
                    classify_record(rel, &scanned.fingerprint, record, pointer, &mut out);
                }
                if scanned.halt == SourceHalt::RecordBudget {
                    out.halt = Halt::MaxRecords;
                    break;
                }
            }
        }
    }
    if out.halt != Halt::None {
        out.truncated = true;
    }
    Ok(out)
}

fn classify_record(
    rel: &str,
    fingerprint: &str,
    record: sources::SourceRecord,
    pointer: &Pointer,
    out: &mut ScanOut,
) {
    let source = SourceRef {
        path: rel.to_string(),
        line: record.line,
        fingerprint: fingerprint.to_string(),
    };
    if record.oversized {
        let index = out.accounted.len();
        out.accounted.push(Accounted {
            source,
            state_id: format!("a{index}"),
            status: "unsupported",
            reason: "oversized_record".into(),
            record: None,
            text: None,
            omissions: vec!["record_over_line_capture".into()],
        });
        out.order.push(Order::Accounted(index));
        return;
    }
    let Ok(text) = std::str::from_utf8(&record.bytes) else {
        let index = out.accounted.len();
        out.accounted.push(Accounted {
            source,
            state_id: format!("a{index}"),
            status: "unresolved",
            reason: "invalid_json".into(),
            record: None,
            text: None,
            omissions: vec!["record_not_utf8".into()],
        });
        out.order.push(Order::Accounted(index));
        return;
    };
    let parsed = serde_json::from_str::<Value>(text);
    let Ok(value) = parsed else {
        let (record_value, mut omissions) = bounded_raw_line(text);
        omissions.insert(0, "invalid_json".into());
        let index = out.accounted.len();
        out.accounted.push(Accounted {
            source,
            state_id: format!("a{index}"),
            status: "unresolved",
            reason: "invalid_json".into(),
            record: record_value,
            text: None,
            omissions,
        });
        out.order.push(Order::Accounted(index));
        return;
    };
    let (record_value, mut omissions) = bounded_json(&value);
    match extract_text(&value, pointer) {
        Extract::Missing => {
            omissions.push("missing_path".into());
            let index = out.accounted.len();
            out.accounted.push(Accounted {
                source,
                state_id: format!("a{index}"),
                status: "unresolved",
                reason: "missing_path".into(),
                record: record_value,
                text: None,
                omissions,
            });
            out.order.push(Order::Accounted(index));
        }
        Extract::NonText => {
            omissions.push("non_text".into());
            let index = out.accounted.len();
            out.accounted.push(Accounted {
                source,
                state_id: format!("a{index}"),
                status: "unsupported",
                reason: "non_text".into(),
                record: record_value,
                text: None,
                omissions,
            });
            out.order.push(Order::Accounted(index));
        }
        Extract::Text(extracted) => {
            if extracted.len() > MAX_SERIALIZED_TEXT_BYTES {
                omissions.push("over_serialized_byte_bound".into());
                let shown = if extracted.len() <= EVIDENCE_BUDGET {
                    Some(extracted)
                } else {
                    omissions.push("text_over_evidence_budget".into());
                    None
                };
                let index = out.accounted.len();
                out.accounted.push(Accounted {
                    source,
                    state_id: format!("a{index}"),
                    status: "unsupported",
                    reason: format!(
                        "over_serialized_byte_bound: utf-8 length exceeds {MAX_SERIALIZED_TEXT_BYTES} bytes; token limits are enforced by the model worker"
                    ),
                    record: record_value,
                    text: shown,
                    omissions,
                });
                out.order.push(Order::Accounted(index));
                return;
            }
            if extracted.len() > EVIDENCE_BUDGET {
                omissions.push("text_over_evidence_budget".into());
            }
            let index = out.eligible.len();
            out.eligible.push(Eligible {
                source,
                state_id: format!("s{index}"),
                text_hash: blake3::hash(extracted.as_bytes()).to_hex().to_string(),
                text: extracted,
                record: record_value,
                omissions,
            });
            out.order.push(Order::Eligible(index));
        }
    }
}

fn bounded_json(value: &Value) -> (Option<Value>, Vec<String>) {
    match serde_json::to_string(value) {
        Ok(raw) if raw.len() <= EVIDENCE_BUDGET => (Some(value.clone()), Vec::new()),
        _ => (None, vec!["record_over_evidence_budget".into()]),
    }
}

fn bounded_raw_line(text: &str) -> (Option<Value>, Vec<String>) {
    if text.len() <= EVIDENCE_BUDGET {
        (Some(Value::String(text.to_string())), Vec::new())
    } else {
        (None, vec!["record_over_evidence_budget".into()])
    }
}

fn extract_text(value: &Value, pointer: &Pointer) -> Extract {
    match lookup(value, pointer) {
        None => Extract::Missing,
        Some(Value::String(text)) => Extract::Text(text.clone()),
        Some(_) => Extract::NonText,
    }
}

fn load_pairs(
    path: &Path,
    rows: &[Eligible],
    question_json: &[String],
    env_hash: &str,
    settings_fp: &str,
    source_fp: &str,
    known: Option<&ModelProvenance>,
) -> Result<Vec<Pair>> {
    let Some(provenance) = known else {
        return Ok(empty_pairs(rows, question_json));
    };
    if provenance_reusable(provenance).is_err() || !path.exists() {
        return Ok(empty_pairs(rows, question_json));
    }
    let conn = open_db(path)?;
    let identity = Some(identity_json(settings_fp, source_fp, env_hash, provenance)?);
    let mut pairs = Vec::new();
    for (row_index, row) in rows.iter().enumerate() {
        for (qindex, qjson) in question_json.iter().enumerate() {
            let (key, hit) = if let Some(identity) = &identity {
                let key = cache_key(&row.text_hash, qjson, identity);
                let marker = provenance_marker(provenance);
                let stored = lookup_judgment(&conn, &key)?;
                (format!("{key}{marker}"), stored)
            } else {
                (String::new(), None)
            };
            pairs.push(Pair {
                row: row_index,
                question: qindex,
                key,
                hit,
            });
        }
    }
    Ok(pairs)
}

fn empty_pairs(rows: &[Eligible], question_json: &[String]) -> Vec<Pair> {
    let mut pairs = Vec::new();
    for row in 0..rows.len() {
        for question in 0..question_json.len() {
            pairs.push(Pair {
                row,
                question,
                key: String::new(),
                hit: None,
            });
        }
    }
    pairs
}

fn validate_request(request: &SemanticCheckRequest) -> Result<()> {
    validate_limits(&request.limits)?;
    sources::validate_globs(&request.globs)?;
    if request.questions.is_empty() {
        bail!("semantic check requires at least one question");
    }
    if request.questions.len() > 64 {
        bail!(
            "at most 64 questions on one semantic request; calls are split to {WORKER_MAX_QUESTIONS} questions and {WORKER_MAX_PAIRS} pairs"
        );
    }
    if request.batch_size == 0 || request.batch_size > MAX_BATCH {
        bail!(
            "batch_size {} is outside 1..={MAX_BATCH}",
            request.batch_size
        );
    }
    let mut seen = BTreeSet::new();
    for question in &request.questions {
        if question.id().is_empty() || question.id().len() > 256 {
            bail!("question id must be 1..=256 characters");
        }
        if !seen.insert(question.id().to_string()) {
            bail!("duplicate question id {}", question.id());
        }
    }
    parse_pointer(&request.text_pointer)?;
    Ok(())
}

fn validate_limits(limits: &Limits) -> Result<()> {
    if limits.max_files == 0 || limits.max_files > HARD_MAX_FILES {
        bail!(
            "max_files {} is outside the hard cap 1..={HARD_MAX_FILES}",
            limits.max_files
        );
    }
    if limits.max_bytes == 0 || limits.max_bytes > HARD_MAX_BYTES {
        bail!(
            "max_bytes {} is outside the hard cap 1..={HARD_MAX_BYTES}",
            limits.max_bytes
        );
    }
    if limits.max_records == 0 || limits.max_records > HARD_MAX_RECORDS {
        bail!(
            "max_records {} is outside the hard cap 1..={HARD_MAX_RECORDS}",
            limits.max_records
        );
    }
    if limits.max_results > HARD_MAX_RESULTS {
        bail!(
            "max_results {} exceeds hard cap {HARD_MAX_RESULTS}",
            limits.max_results
        );
    }
    if limits.timeout_ms == 0 || limits.timeout_ms > HARD_MAX_TIMEOUT_MS {
        bail!(
            "timeout_ms {} is outside 1..={HARD_MAX_TIMEOUT_MS}",
            limits.timeout_ms
        );
    }
    Ok(())
}

fn parse_pointer(raw: &str) -> Result<Pointer> {
    if raw.len() > MAX_POINTER {
        bail!("invalid JSON pointer: exceeds size limit");
    }
    if raw.is_empty() {
        return Ok(Pointer {
            segments: Vec::new(),
        });
    }
    if !raw.starts_with('/') {
        bail!("invalid JSON pointer: {raw}");
    }
    let mut segments = Vec::new();
    for seg in raw[1..].split('/') {
        segments
            .push(unescape_segment(seg).with_context(|| format!("invalid JSON pointer: {raw}"))?);
    }
    Ok(Pointer { segments })
}

fn unescape_segment(seg: &str) -> Result<String> {
    let mut out = String::with_capacity(seg.len());
    let mut chars = seg.chars();
    while let Some(ch) = chars.next() {
        if ch == '~' {
            match chars.next() {
                Some('0') => out.push('~'),
                Some('1') => out.push('/'),
                _ => bail!("invalid JSON pointer escape"),
            }
        } else {
            out.push(ch);
        }
    }
    Ok(out)
}

fn lookup<'a>(value: &'a Value, pointer: &Pointer) -> Option<&'a Value> {
    let mut cur = value;
    for seg in &pointer.segments {
        match cur {
            Value::Object(map) => cur = map.get(seg)?,
            Value::Array(items) => {
                let index = array_index(seg)?;
                cur = items.get(index)?;
            }
            _ => return None,
        }
    }
    Some(cur)
}

fn array_index(seg: &str) -> Option<usize> {
    if seg.is_empty() || (seg.len() > 1 && seg.starts_with('0')) {
        return None;
    }
    if !seg.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    seg.parse().ok()
}

fn question_canonical(questions: &[ModelQuestion]) -> Result<Vec<String>> {
    questions
        .iter()
        .map(|question| serde_json::to_string(question).context("serialize question"))
        .collect()
}

fn environment_identity() -> EnvIdentity {
    let mut values = BTreeMap::new();
    let mut limitations = Vec::new();
    for key in TRACKED_ENV {
        match std::env::var(key) {
            Ok(value) => {
                values.insert((*key).to_string(), value);
            }
            Err(std::env::VarError::NotPresent) => {
                values.insert((*key).to_string(), "<absent>".into());
            }
            Err(std::env::VarError::NotUnicode(_)) => {
                values.insert((*key).to_string(), "<non-utf8>".into());
                limitations.push(format!("untracked_environment: {key} is not utf-8"));
            }
        }
    }
    let mut unknown = Vec::new();
    for (key, value) in std::env::vars_os() {
        let Some(key) = key.to_str() else {
            limitations.push("untracked_environment: non-utf8 name".into());
            continue;
        };
        if values.contains_key(key) || !tracked_prefix(key) {
            continue;
        }
        unknown.push(key.to_string());
        values.insert(key.to_string(), value.to_string_lossy().into_owned());
    }
    unknown.sort();
    unknown.dedup();
    if !unknown.is_empty() {
        limitations.push(format!("untracked_environment: {}", unknown.join(",")));
    }
    let raw = serde_json::to_string(&values).unwrap_or_else(|_| "{}".into());
    EnvIdentity {
        hash: blake3::hash(raw.as_bytes()).to_hex().to_string(),
        limitations,
    }
}

fn tracked_prefix(key: &str) -> bool {
    key.starts_with("CHECKWEAVE_")
        || key.starts_with("HF_")
        || key.starts_with("CUDA_")
        || key.starts_with("TORCH_")
}

fn provenance_reusable(provenance: &ModelProvenance) -> Result<(), String> {
    if !revision_pinned(&provenance.revision) {
        return Err("unpinned_model_revision".into());
    }
    if provenance.model.is_empty()
        || matches!(provenance.model.as_str(), "latest" | "main" | "auto")
    {
        return Err("unpinned_model_alias".into());
    }
    if provenance.device.is_empty() || matches!(provenance.device.as_str(), "auto" | "not_invoked")
    {
        return Err("actual_device_unknown".into());
    }
    if provenance.precision.is_empty() || provenance.runtime_versions.is_empty() {
        return Err("runtime_identity_incomplete".into());
    }
    if !semantics_present(&provenance.score_semantics) {
        return Err("score_semantics_incomplete".into());
    }
    if provenance.provider.is_empty() || provenance.adapter_version.is_empty() {
        return Err("provider_identity_incomplete".into());
    }
    Ok(())
}

fn revision_pinned(revision: &str) -> bool {
    if revision.len() == 40 && revision.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return true;
    }
    let Some(rest) = revision.strip_prefix("jev-") else {
        return false;
    };
    let mut parts = rest.split('.');
    let numeric = |part: Option<&str>| {
        part.is_some_and(|part| !part.is_empty() && part.bytes().all(|byte| byte.is_ascii_digit()))
    };
    numeric(parts.next())
        && numeric(parts.next())
        && numeric(parts.next())
        && parts.next().is_none()
}

fn semantics_present(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::String(text) => !text.is_empty(),
        Value::Object(map) => !map.is_empty(),
        Value::Array(items) => !items.is_empty(),
        Value::Bool(_) | Value::Number(_) => true,
    }
}

fn identity_json(
    settings_fp: &str,
    source_fp: &str,
    env_hash: &str,
    provenance: &ModelProvenance,
) -> Result<String> {
    let mut value = serde_json::to_value(provenance).context("serialize model provenance")?;
    let Some(map) = value.as_object_mut() else {
        bail!("model provenance was not an object");
    };
    map.remove("usage");
    map.insert(
        "environment_hash".into(),
        Value::String(env_hash.to_string()),
    );
    map.insert(
        "settings_fingerprint".into(),
        Value::String(settings_fp.to_string()),
    );
    map.insert(
        "source_fingerprint".into(),
        Value::String(source_fp.to_string()),
    );
    map.insert("serialization".into(), Value::String(SERIALIZATION.into()));
    map.insert("operator".into(), Value::String(OPERATOR_VERSION.into()));
    Ok(serde_json::to_string(&value)?)
}

fn cache_key(text_hash: &str, question_json: &str, identity: &str) -> String {
    let mut hasher = Hasher::new();
    hasher.update(OPERATOR_VERSION.as_bytes());
    hasher.update(&[0]);
    hasher.update(text_hash.as_bytes());
    hasher.update(&[0]);
    hasher.update(question_json.as_bytes());
    hasher.update(&[0]);
    hasher.update(identity.as_bytes());
    hasher.finalize().to_hex().to_string()
}

fn latch_key(settings_fp: &str, source_fp: &str, env_hash: &str) -> String {
    let mut hasher = Hasher::new();
    hasher.update(settings_fp.as_bytes());
    hasher.update(&[0]);
    hasher.update(source_fp.as_bytes());
    hasher.update(&[0]);
    hasher.update(env_hash.as_bytes());
    hasher.finalize().to_hex().to_string()
}

fn status_name(status: ModelStatus) -> &'static str {
    match status {
        ModelStatus::Resolved => "resolved",
        ModelStatus::Unresolved => "unresolved",
        ModelStatus::Unsupported => "unsupported",
    }
}

fn halt_warning(halt: Halt) -> &'static str {
    match halt {
        Halt::Timeout => "stopped: timeout",
        Halt::MaxFiles => "stopped: max_files",
        Halt::MaxBytes => "stopped: max_bytes",
        Halt::MaxRecords => "stopped: max_records",
        Halt::Unstable => "stale: inputs changed before publication",
        Halt::Cancel => "cancelled",
        Halt::None => "complete",
    }
}

fn push_unique(items: &mut Vec<String>, item: String) {
    if !items.iter().any(|existing| existing == &item) {
        items.push(item);
    }
}

fn semantic_db(root: &Path) -> PathBuf {
    root.join(".checkweave").join("semantic.sqlite")
}

enum SourceVerdict {
    Current,
    Stale,
    Unknown,
}

fn cap_published_details(
    decisions: &mut Vec<SemanticDecision>,
    sources: &[SourceFingerprint],
    truncated: &mut bool,
    warnings: &mut Vec<String>,
) {
    let sources_len = serde_json::to_string(sources).unwrap_or_default().len();
    while !decisions.is_empty() {
        let detail = serde_json::to_string(&*decisions).unwrap_or_default().len();
        if detail <= MAX_DETAIL_BYTES && detail.saturating_add(sources_len) <= MAX_REPORT_BYTES {
            break;
        }
        decisions.pop();
        *truncated = true;
    }
    if *truncated {
        warnings.push("stopped: serialized detail budget".into());
    }
}

fn recheck_sources(
    root: &Path,
    globs: &[String],
    execution: &str,
    sources: &[SourceFingerprint],
    deadline: Instant,
    cancel: &AtomicBool,
) -> Result<SourceVerdict> {
    if cancel.load(Ordering::Relaxed) || Instant::now() >= deadline {
        return Ok(SourceVerdict::Unknown);
    }
    let listed = sources::enumerate_bounded(root, globs, Some(deadline), Some(cancel))
        .context("re-enumerate semantic inputs")?;
    if !listed.complete || cancel.load(Ordering::Relaxed) || Instant::now() >= deadline {
        return Ok(SourceVerdict::Unknown);
    }
    if !sources::snapshot_holds(execution, sources, &listed.paths) {
        return Ok(SourceVerdict::Stale);
    }
    for src in sources {
        if cancel.load(Ordering::Relaxed) || Instant::now() >= deadline {
            return Ok(SourceVerdict::Unknown);
        }
        let abs = sources::safe_join(root, &src.path)?;
        match fs::symlink_metadata(&abs) {
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                return Ok(SourceVerdict::Stale);
            }
            Err(err) => return Err(err).with_context(|| format!("stat {}", abs.display())),
            Ok(meta) if meta.file_type().is_symlink() || !meta.file_type().is_file() => {
                return Ok(SourceVerdict::Stale);
            }
            Ok(_) => {}
        }
        match sources::hash_file_bounded(&abs, Some(deadline), Some(cancel))
            .with_context(|| format!("hash {}", abs.display()))?
        {
            sources::HashedFile::Incomplete => return Ok(SourceVerdict::Unknown),
            sources::HashedFile::Ready { fingerprint, bytes }
                if fingerprint == src.fingerprint && bytes == src.bytes => {}
            sources::HashedFile::Ready { .. } => return Ok(SourceVerdict::Stale),
        }
    }
    Ok(SourceVerdict::Current)
}

fn open_db(path: &Path) -> Result<Connection> {
    if path.exists() && sqlite_file_corrupt(path)? {
        quarantine_db(path)?;
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    }
    let conn = Connection::open(path).with_context(|| format!("open sqlite {}", path.display()))?;
    conn.busy_timeout(Duration::from_secs(5))
        .context("sqlite busy_timeout")?;
    conn.pragma_update(None, "journal_mode", "WAL")
        .with_context(|| format!("sqlite journal_mode {}", path.display()))?;
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS items (
            cache_key TEXT PRIMARY KEY,
            judgment_json TEXT NOT NULL,
            payload_bytes INTEGER NOT NULL,
            created_at INTEGER NOT NULL
        );
        CREATE TABLE IF NOT EXISTS reports (
            id TEXT PRIMARY KEY,
            request_json TEXT NOT NULL,
            report_json TEXT NOT NULL,
            execution TEXT NOT NULL,
            freshness TEXT NOT NULL,
            payload_bytes INTEGER NOT NULL,
            created_at INTEGER NOT NULL
        );
        CREATE TABLE IF NOT EXISTS identity (
            latch_key TEXT PRIMARY KEY,
            provenance_json TEXT NOT NULL,
            payload_bytes INTEGER NOT NULL DEFAULT 0,
            created_at INTEGER NOT NULL
        );
        CREATE TABLE IF NOT EXISTS report_sources (
            report_id TEXT NOT NULL,
            path TEXT NOT NULL,
            fingerprint TEXT NOT NULL,
            bytes INTEGER NOT NULL
        );
        CREATE TABLE IF NOT EXISTS meta (
            key TEXT PRIMARY KEY,
            value TEXT NOT NULL
        );",
    )
    .context("sqlite semantic schema")?;
    let _ = conn.execute(
        "ALTER TABLE identity ADD COLUMN payload_bytes INTEGER NOT NULL DEFAULT 0",
        [],
    );
    Ok(conn)
}

fn sqlite_file_corrupt(path: &Path) -> Result<bool> {
    let conn = match Connection::open(path) {
        Ok(conn) => conn,
        Err(err) if corrupt_message(&err) => return Ok(true),
        Err(err) => return Err(err).with_context(|| format!("open sqlite {}", path.display())),
    };
    match conn.query_row("PRAGMA integrity_check", [], |row| row.get::<_, String>(0)) {
        Ok(status) if status == "ok" => Ok(false),
        Ok(_) => Ok(true),
        Err(err) if corrupt_message(&err) => Ok(true),
        Err(err) => Err(err).context("sqlite integrity_check"),
    }
}

fn corrupt_message(err: &impl ToString) -> bool {
    let text = err.to_string().to_ascii_lowercase();
    text.contains("malformed")
        || text.contains("not a database")
        || text.contains("database disk image is corrupt")
}

fn quarantine_db(path: &Path) -> Result<()> {
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|dur| dur.as_secs())
        .unwrap_or(0);
    for suffix in ["", "-wal", "-shm"] {
        let src = if suffix.is_empty() {
            path.to_path_buf()
        } else {
            PathBuf::from(format!("{}{suffix}", path.display()))
        };
        if src.exists() {
            let dest = PathBuf::from(format!("{}.corrupt-{stamp}{suffix}", path.display()));
            fs::rename(&src, &dest).with_context(|| format!("quarantine {}", src.display()))?;
        }
    }
    Ok(())
}

fn lookup_judgment(conn: &Connection, key: &str) -> Result<Option<CachedJudgment>> {
    let row = conn
        .query_row(
            "SELECT judgment_json FROM items WHERE cache_key = ?1",
            params![key],
            |row| row.get::<_, String>(0),
        )
        .optional()
        .context("sqlite lookup semantic item")?;
    match row {
        Some(raw) => Ok(Some(
            serde_json::from_str(&raw).context("decode semantic cache item")?,
        )),
        None => Ok(None),
    }
}

fn persist(
    path: &Path,
    request_json: &str,
    report: &SemanticReport,
    store: &[(String, CachedJudgment)],
    latch_id: &str,
    provenance: Option<&ModelProvenance>,
) -> Result<()> {
    let conn = open_db(path)?;
    let mut tick = conn
        .query_row(
            "SELECT COALESCE(MAX(created_at), 0) FROM items",
            [],
            |row| row.get::<_, i64>(0),
        )
        .unwrap_or(0);
    let tx = conn
        .unchecked_transaction()
        .context("sqlite begin semantic transaction")?;
    for (key, judgment) in store {
        tick += 1;
        let judgment_json =
            serde_json::to_string(judgment).context("serialize semantic judgment")?;
        tx.execute(
            "INSERT INTO items (cache_key, judgment_json, payload_bytes, created_at)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(cache_key) DO UPDATE SET
                judgment_json = excluded.judgment_json,
                payload_bytes = excluded.payload_bytes,
                created_at = excluded.created_at",
            params![key, judgment_json, judgment_json.len() as i64, tick],
        )
        .context("sqlite upsert semantic item")?;
    }
    if let Some(provenance) = provenance {
        tick += 1;
        let provenance_json =
            serde_json::to_string(provenance).context("serialize semantic provenance")?;
        tx.execute(
            "INSERT INTO identity (latch_key, provenance_json, payload_bytes, created_at)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(latch_key) DO UPDATE SET
                provenance_json = excluded.provenance_json,
                payload_bytes = excluded.payload_bytes,
                created_at = excluded.created_at",
            params![
                latch_id,
                provenance_json,
                provenance_json.len() as i64,
                tick
            ],
        )
        .context("sqlite upsert semantic identity")?;
    }
    for src in &report.sources {
        tx.execute(
            "INSERT INTO report_sources (report_id, path, fingerprint, bytes) VALUES (?1, ?2, ?3, ?4)",
            params![report.id, src.path, src.fingerprint, src.bytes as i64],
        )
        .context("sqlite insert semantic source")?;
    }
    tick += 1;
    let report_json = serde_json::to_string(report).context("serialize semantic report")?;
    let payload = (request_json.len() + report_json.len()) as i64;
    tx.execute(
        "INSERT INTO reports (id, request_json, report_json, execution, freshness, payload_bytes, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![report.id, request_json, report_json, report.execution, report.freshness, payload, tick],
    )
    .context("sqlite insert semantic report")?;
    tx.commit().context("sqlite commit semantic report")?;
    reclaim(&conn)?;
    Ok(())
}

fn load_stored_sources(conn: &Connection, id: &str) -> Result<Option<Vec<SourceFingerprint>>> {
    let mut stmt = conn
        .prepare(
            "SELECT path, fingerprint, bytes FROM report_sources WHERE report_id = ?1 ORDER BY rowid",
        )
        .context("sqlite list semantic sources")?;
    let rows = stmt
        .query_map(params![id], |row| {
            Ok(SourceFingerprint {
                path: row.get(0)?,
                fingerprint: row.get(1)?,
                bytes: row.get::<_, i64>(2)? as u64,
            })
        })
        .context("sqlite list semantic sources")?;
    let mut sources = Vec::new();
    for row in rows {
        sources.push(row.context("sqlite read semantic source")?);
    }
    if sources.is_empty() {
        Ok(None)
    } else {
        Ok(Some(sources))
    }
}

fn reclaim(conn: &Connection) -> Result<()> {
    let mut deleted = 0usize;
    for _ in 0..1_000 {
        let (items, reports, identities, payload): (i64, i64, i64, i64) = conn
            .query_row(
                "SELECT
                    (SELECT COUNT(*) FROM items),
                    (SELECT COUNT(*) FROM reports),
                    (SELECT COUNT(*) FROM identity),
                    COALESCE((SELECT SUM(payload_bytes) FROM items), 0)
                        + COALESCE((SELECT SUM(payload_bytes) FROM reports), 0)
                        + COALESCE((SELECT SUM(payload_bytes) FROM identity), 0)",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .context("sqlite semantic cache size")?;
        if items + reports <= MAX_ENTRIES
            && identities <= MAX_IDENTITY_ROWS
            && payload <= MAX_PAYLOAD
        {
            break;
        }
        let n = delete_oldest_batch(conn)?;
        if n == 0 {
            break;
        }
        deleted += n;
    }
    if deleted > 0 {
        let _ = conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);");
        let _ = conn.execute_batch("PRAGMA incremental_vacuum(32);");
    }
    Ok(())
}

fn delete_oldest_batch(conn: &Connection) -> Result<usize> {
    let mut stmt = conn
        .prepare(
            "SELECT kind, rid FROM (
                SELECT 'item' AS kind, rowid AS rid, created_at AS ts FROM items
                UNION ALL
                SELECT 'report', rowid, created_at FROM reports
                UNION ALL
                SELECT 'identity', rowid, created_at FROM identity
             ) ORDER BY ts ASC LIMIT ?1",
        )
        .context("sqlite oldest semantic rows")?;
    let mapped = stmt
        .query_map(params![RECLAIM_BATCH as i64], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
        })
        .context("sqlite oldest semantic rows")?;
    let mut items = Vec::new();
    let mut reports = Vec::new();
    let mut identities = Vec::new();
    for row in mapped {
        let (kind, rid) = row.context("sqlite oldest semantic row")?;
        match kind.as_str() {
            "item" => items.push(rid),
            "identity" => identities.push(rid),
            _ => reports.push(rid),
        }
    }
    let count = items.len() + reports.len() + identities.len();
    if count == 0 {
        return Ok(0);
    }
    let tx = conn
        .unchecked_transaction()
        .context("sqlite begin semantic reclaim")?;
    delete_rowids(&tx, "items", &items)?;
    delete_rowids(&tx, "identity", &identities)?;
    if !reports.is_empty() {
        let ids = report_ids(&tx, &reports)?;
        delete_rowids(&tx, "reports", &reports)?;
        if !ids.is_empty() {
            let marks = vec!["?"; ids.len()].join(",");
            tx.execute(
                &format!("DELETE FROM report_sources WHERE report_id IN ({marks})"),
                rusqlite::params_from_iter(ids.iter()),
            )
            .context("sqlite delete semantic sources")?;
        }
    }
    tx.commit().context("sqlite commit semantic reclaim")?;
    Ok(count)
}

fn report_ids(conn: &Connection, rowids: &[i64]) -> Result<Vec<String>> {
    if rowids.is_empty() {
        return Ok(Vec::new());
    }
    let marks = vec!["?"; rowids.len()].join(",");
    let mut stmt = conn
        .prepare(&format!("SELECT id FROM reports WHERE rowid IN ({marks})"))
        .context("sqlite semantic report ids")?;
    let rows = stmt
        .query_map(rusqlite::params_from_iter(rowids.iter()), |row| {
            row.get::<_, String>(0)
        })
        .context("sqlite semantic report ids")?;
    let mut ids = Vec::new();
    for row in rows {
        ids.push(row.context("sqlite semantic report id")?);
    }
    Ok(ids)
}

fn delete_rowids(conn: &Connection, table: &str, ids: &[i64]) -> Result<()> {
    if ids.is_empty() {
        return Ok(());
    }
    let marks = vec!["?"; ids.len()].join(",");
    conn.execute(
        &format!("DELETE FROM {table} WHERE rowid IN ({marks})"),
        rusqlite::params_from_iter(ids.iter()),
    )
    .with_context(|| format!("sqlite delete {table} batch"))?;
    Ok(())
}
