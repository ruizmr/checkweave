//! Deterministic JSONL collection checks and a disposable SQLite cache.

use anyhow::{Context, Result, bail};
use blake3::Hasher;
use globset::{GlobBuilder, GlobSetBuilder};
use ignore::WalkBuilder;
use regex::RegexBuilder;
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::{Number, Value};
use std::collections::{HashMap, HashSet};
use std::fs::{self, File};
use std::io::{BufRead, BufReader, Read};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::types::{
    CheckReport, CheckRequest, Coverage, ItemResult, JsonKind, Limits, OPERATOR_VERSION, Predicate,
    SourceFingerprint, SourceRef,
};

const HARD_MAX_FILES: usize = 100_000;
const HARD_MAX_BYTES: u64 = 1 << 30;
const HARD_MAX_RECORDS: usize = 1_000_000;
const HARD_MAX_RESULTS: usize = 5_000;
const HARD_MAX_TIMEOUT_MS: u64 = 600_000;
const MAX_DEPTH: usize = 32;
const MAX_NODES: usize = 256;
const MAX_REGEX_PATTERN: usize = 1024;
const MAX_POINTER: usize = 4096;
const MAX_VALUE_BYTES: usize = 32 * 1024;
const LINE_CAPTURE: usize = 1024 * 1024;
const MAX_ENTRIES: i64 = 100_000;
const MAX_PAYLOAD: i64 = 64 * 1024 * 1024;
/// Serialized `CheckReport` hard cap. `WireResponse` and `RunSnapshot` wrappers
/// sit outside this, so the IPC frame stays under 8MiB.
pub const REPORT_JSON_BUDGET: usize = 6 * 1024 * 1024;
/// IPC frame the report budget is sized to fit inside, wrappers included.
pub const IPC_FRAME_BYTES: usize = 8 * 1024 * 1024;
const HASH_READ_CAP: u64 = 1 << 30;
const MAX_WALK_STEPS: usize = 50_000;
const RECONCILE_REPORTS: i64 = 32;
const RECONCILE_SOURCES: usize = 64;
const RECLAIM_BATCH: usize = 256;

pub struct Engine {
    root: PathBuf,
    conn: Connection,
    tick: i64,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Tri {
    True,
    False,
    Unknown,
}

enum Compiled {
    Exists { path: Pointer },
    Eq { path: Pointer, value: Value },
    Ne { path: Pointer, value: Value },
    Contains { path: Pointer, value: String },
    Regex { path: Pointer, re: regex::Regex },
    Gt { path: Pointer, value: f64 },
    Ge { path: Pointer, value: f64 },
    Lt { path: Pointer, value: f64 },
    Le { path: Pointer, value: f64 },
    Kind { path: Pointer, kind: JsonKind },
    All { predicates: Vec<Compiled> },
    Any { predicates: Vec<Compiled> },
    Not { predicate: Box<Compiled> },
}

#[derive(Clone)]
struct Pointer {
    segments: Vec<String>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Halt {
    None,
    Cancel,
    Timeout,
    MaxFiles,
    MaxBytes,
    MaxRecords,
    Traversal,
    ReportBudget,
    Unstable,
}

struct LogicalLine {
    bytes: Vec<u8>,
    hash: String,
    oversized: bool,
}

enum ReadLine {
    Eof,
    Line(LogicalLine),
    Budget,
}

struct NewItem {
    key: String,
    matched: Option<i64>,
    reason: Option<String>,
    value_json: Option<String>,
    payload: i64,
    created_at: i64,
}

struct Cached {
    matched: Option<i64>,
    reason: Option<String>,
    value_json: Option<String>,
}

struct EvalOut {
    tri: Tri,
    reason: Option<String>,
    value: Option<Value>,
    value_json: Option<String>,
    payload: i64,
}

enum SchemaStatus {
    Ready,
    Empty,
    Bad,
}

impl Engine {
    pub fn open(root: &Path) -> Result<Self> {
        if !root.is_dir() {
            bail!("workspace root is not a directory: {}", root.display());
        }
        let state = root.join(".checkweave");
        fs::create_dir_all(&state)
            .with_context(|| format!("create state dir {}", state.display()))?;
        let db_path = state.join("cache.sqlite");
        let conn = open_cache(&db_path)?;
        let tick = load_tick(&conn)?;
        let mut engine = Self {
            root: root.to_path_buf(),
            conn,
            tick,
        };
        engine.reclaim()?;
        Ok(engine)
    }

    pub fn check(&mut self, request: &CheckRequest, cancel: &AtomicBool) -> Result<CheckReport> {
        let started = Instant::now();
        validate_limits(&request.limits)?;
        validate_includes(&request.include)?;
        if predicate_exceeds_depth(&request.predicate) {
            bail!("predicate exceeds maximum depth {MAX_DEPTH}");
        }
        let predicate_json =
            serde_json::to_string(&request.predicate).context("serialize predicate")?;
        let compiled = compile_predicate(&request.predicate)?;
        let deadline = started + Duration::from_millis(request.limits.timeout_ms);

        if cancel.load(Ordering::Relaxed) {
            return Ok(self.early_report(started, "cancelled", "unknown", "cancelled"));
        }
        if Instant::now() >= deadline {
            return self.finish(
                request,
                started,
                "partial",
                "unknown",
                Coverage::default(),
                Vec::new(),
                Vec::new(),
                true,
                vec!["stopped: timeout".into()],
                true,
            );
        }

        let listing = self
            .enumerate(&request.include, Some(deadline), Some(cancel))
            .context("enumerate collection inputs")?;
        if cancel.load(Ordering::Relaxed) {
            return Ok(self.early_report(started, "cancelled", "unknown", "cancelled"));
        }
        let members = listing.paths;
        let mut coverage = Coverage::default();
        let mut items = Vec::new();
        let mut sources = Vec::new();
        let mut warnings = Vec::new();
        let mut truncated = false;
        let mut halt = if listing.complete {
            Halt::None
        } else if Instant::now() >= deadline {
            warnings.push("stopped: timeout".into());
            Halt::Timeout
        } else {
            warnings.push("stopped: traversal budget".into());
            Halt::Traversal
        };
        let mut bytes_left = request.limits.max_bytes;
        let mut results_left = request.limits.max_results;
        let envelope = report_envelope_len(&self.root.display().to_string());
        let mut metadata_inside = 0usize;
        let mut detail_inside = 0usize;
        let mut warned_results = false;
        let mut warned_bytes = false;
        if halt != Halt::None {
            truncated = true;
        }

        for rel in &members {
            if cancel.load(Ordering::Relaxed) {
                halt = Halt::Cancel;
                break;
            }
            if Instant::now() >= deadline {
                halt = Halt::Timeout;
                break;
            }
            if coverage.files >= request.limits.max_files {
                halt = Halt::MaxFiles;
                break;
            }
            if coverage.records >= request.limits.max_records {
                halt = Halt::MaxRecords;
                break;
            }
            let abs = safe_join(&self.root, rel)?;
            let meta = match fs::symlink_metadata(&abs) {
                Ok(meta) => meta,
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                    halt = Halt::Unstable;
                    break;
                }
                Err(err) => {
                    return Err(err).with_context(|| format!("stat {}", abs.display()));
                }
            };
            if meta.file_type().is_symlink() || !meta.file_type().is_file() {
                halt = Halt::Unstable;
                break;
            }
            let len = meta.len();
            if len > bytes_left {
                halt = Halt::MaxBytes;
                break;
            }
            let source_len = source_entry_len(rel, len);
            let source_comma = usize::from(!sources.is_empty());
            if envelope + metadata_inside + source_comma + source_len + detail_inside
                > REPORT_JSON_BUDGET
            {
                halt = Halt::ReportBudget;
                warnings.push("stopped: report metadata budget".into());
                break;
            }
            let records_budget = request.limits.max_records.saturating_sub(coverage.records);
            let detail_room = REPORT_JSON_BUDGET
                .saturating_sub(envelope + metadata_inside + source_comma + source_len);
            let mut scanned = self.scan_file(
                &abs,
                rel,
                len,
                &predicate_json,
                &compiled,
                records_budget,
                &mut results_left,
                &mut detail_inside,
                detail_room,
                &mut warned_results,
                &mut warned_bytes,
                deadline,
                cancel,
            )?;
            match scanned.halt {
                Halt::None | Halt::MaxRecords => {
                    bytes_left = bytes_left.saturating_sub(scanned.bytes);
                    coverage.files += 1;
                    coverage.records += scanned.coverage.records;
                    coverage.evaluated += scanned.coverage.evaluated;
                    coverage.matched += scanned.coverage.matched;
                    coverage.unmatched += scanned.coverage.unmatched;
                    coverage.unresolved += scanned.coverage.unresolved;
                    coverage.skipped += scanned.coverage.skipped;
                    coverage.cache_hits += scanned.coverage.cache_hits;
                    coverage.cache_misses += scanned.coverage.cache_misses;
                    if scanned.truncated {
                        truncated = true;
                    }
                    warnings.extend(std::mem::take(&mut scanned.warnings));
                    items.extend(std::mem::take(&mut scanned.items));
                    sources.push(SourceFingerprint {
                        path: rel.clone(),
                        fingerprint: scanned.fingerprint,
                        bytes: scanned.bytes,
                    });
                    self.insert_items(&mut scanned.inserts)?;
                    metadata_inside += source_comma + source_len;
                    if scanned.halt == Halt::MaxRecords {
                        halt = Halt::MaxRecords;
                        break;
                    }
                }
                other => {
                    halt = other;
                    break;
                }
            }
        }

        let (execution, freshness_hint, publish) = match halt {
            Halt::Cancel => {
                warnings.push("cancelled".into());
                ("cancelled", "unknown", false)
            }
            Halt::None => ("complete", "validated", true),
            Halt::Timeout => {
                warnings.push("stopped: timeout".into());
                ("partial", "validated", true)
            }
            Halt::MaxFiles => {
                warnings.push("stopped: max_files".into());
                ("partial", "validated", true)
            }
            Halt::MaxBytes => {
                warnings.push("stopped: max_bytes".into());
                ("partial", "validated", true)
            }
            Halt::MaxRecords => {
                warnings.push("stopped: max_records".into());
                ("partial", "validated", true)
            }
            Halt::ReportBudget => ("partial", "validated", true),
            Halt::Traversal => ("partial", "unknown", true),
            Halt::Unstable => {
                warnings.push("stale: inputs changed before publication".into());
                ("partial", "stale", true)
            }
        };
        if execution != "complete" {
            truncated = true;
        }

        let mut freshness = if listing.complete {
            freshness_hint.to_string()
        } else {
            "unknown".into()
        };
        let mut execution = if listing.complete {
            execution.to_string()
        } else {
            "partial".into()
        };
        if publish && freshness != "stale" && execution != "cancelled" {
            if cancel.load(Ordering::Relaxed) || Instant::now() >= deadline {
                freshness = "unknown".into();
                if execution == "complete" {
                    execution = "partial".into();
                }
                truncated = true;
                warnings.push("stopped: validation budget".into());
            } else {
                let again = self.enumerate(&request.include, Some(deadline), Some(cancel))?;
                if !again.complete || cancel.load(Ordering::Relaxed) || Instant::now() >= deadline {
                    freshness = "unknown".into();
                    if execution == "complete" {
                        execution = "partial".into();
                    }
                    truncated = true;
                    warnings.push("stopped: validation budget".into());
                } else if !snapshot_holds(&execution, &sources, &again.paths) {
                    freshness = "stale".into();
                    warnings.push("stale: inputs changed before publication".into());
                } else {
                    for src in &sources {
                        if cancel.load(Ordering::Relaxed) || Instant::now() >= deadline {
                            freshness = "unknown".into();
                            if execution == "complete" {
                                execution = "partial".into();
                            }
                            truncated = true;
                            warnings.push("stopped: validation budget".into());
                            break;
                        }
                        let abs = safe_join(&self.root, &src.path)?;
                        match hash_file(&abs) {
                            Ok((fp, n)) if fp == src.fingerprint && n == src.bytes => {}
                            Ok(_) => {
                                freshness = "stale".into();
                                warnings.push("stale: inputs changed before publication".into());
                                break;
                            }
                            Err(err) if io_not_found(&err) => {
                                freshness = "stale".into();
                                warnings.push("stale: inputs changed before publication".into());
                                break;
                            }
                            Err(err) => return Err(err),
                        }
                    }
                }
            }
        }

        self.finish(
            request, started, &execution, &freshness, coverage, items, sources, truncated,
            warnings, publish,
        )
    }

    pub fn evidence(&mut self, id: &str) -> Result<Option<CheckReport>> {
        let row = self
            .conn
            .query_row(
                "SELECT request_json, report_json FROM reports WHERE id = ?1",
                params![id],
                |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)),
            )
            .optional()
            .context("sqlite read evidence")?;
        let Some((request_json, report_json)) = row else {
            return Ok(None);
        };
        let request: CheckRequest =
            serde_json::from_str(&request_json).context("decode stored check request")?;
        let mut report: CheckReport =
            serde_json::from_str(&report_json).context("decode stored check report")?;
        let fresh = self.assess(&request, &report)?;
        if fresh != report.freshness {
            report.freshness = fresh;
            self.update_freshness(&report)?;
        }
        Ok(Some(report))
    }

    pub fn reconcile(&mut self) -> Result<Value> {
        let deadline = Instant::now() + Duration::from_millis(2_000);
        let cursor: i64 = self
            .conn
            .query_row(
                "SELECT CAST(value AS INTEGER) FROM meta WHERE key = 'reconcile_cursor'",
                (),
                |r| r.get(0),
            )
            .optional()
            .context("sqlite reconcile cursor")?
            .unwrap_or(0);
        let ids = {
            let mut stmt = self
                .conn
                .prepare("SELECT id FROM reports ORDER BY created_at ASC LIMIT ?1 OFFSET ?2")
                .context("sqlite list reports")?;
            let rows = stmt
                .query_map(params![RECONCILE_REPORTS, cursor], |r| {
                    r.get::<_, String>(0)
                })
                .context("sqlite list reports")?;
            let mut ids = Vec::new();
            for row in rows {
                ids.push(row.context("sqlite read report id")?);
            }
            ids
        };
        let next_cursor = if ids.len() < RECONCILE_REPORTS as usize {
            0
        } else {
            cursor + RECONCILE_REPORTS
        };
        self.conn
            .execute(
                "INSERT INTO meta (key, value) VALUES ('reconcile_cursor', ?1)
                 ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                params![next_cursor.to_string()],
            )
            .context("sqlite store reconcile cursor")?;
        let mut sources_checked = 0i64;
        let mut sources_changed = 0i64;
        let mut sources_missing = 0i64;
        let mut reports_invalidated = 0i64;
        let mut reports_unchanged = 0i64;
        let mut incomplete = ids.len() == RECONCILE_REPORTS as usize;
        let mut observed: HashMap<String, SourceObs> = HashMap::new();
        for id in ids {
            if Instant::now() >= deadline {
                incomplete = true;
                break;
            }
            let Some((_request, mut report)) = self.load_report(&id)? else {
                continue;
            };
            let mut fresh = "validated".to_string();
            for src in &report.sources {
                if observed.len() >= RECONCILE_SOURCES && !observed.contains_key(&src.path) {
                    fresh = "unknown".into();
                    incomplete = true;
                    break;
                }
                let obs = if let Some(obs) = observed.get(&src.path) {
                    *obs
                } else {
                    if Instant::now() >= deadline {
                        fresh = "unknown".into();
                        incomplete = true;
                        break;
                    }
                    sources_checked += 1;
                    let obs = observe_source(&self.root, src)?;
                    match &obs {
                        SourceObs::Missing => sources_missing += 1,
                        SourceObs::Changed => sources_changed += 1,
                        SourceObs::Same => {}
                    }
                    observed.insert(src.path.clone(), obs);
                    *observed.get(&src.path).unwrap()
                };
                match obs {
                    SourceObs::Missing | SourceObs::Changed => {
                        fresh = "stale".into();
                    }
                    SourceObs::Same => {}
                }
            }
            if fresh == "validated" {
                reports_unchanged += 1;
            } else {
                reports_invalidated += 1;
                if report.freshness != fresh {
                    report.freshness = fresh;
                    self.update_freshness(&report)?;
                }
            }
        }
        Ok(serde_json::json!({
            "sources_checked": sources_checked,
            "sources_changed": sources_changed,
            "sources_missing": sources_missing,
            "reports_invalidated": reports_invalidated,
            "reports_unchanged": reports_unchanged,
            "incomplete": incomplete,
        }))
    }

    pub fn stats(&self) -> Result<Value> {
        let (items, reports, payload): (i64, i64, i64) = self
            .conn
            .query_row(
                "SELECT
                    (SELECT COUNT(*) FROM items),
                    (SELECT COUNT(*) FROM reports),
                    COALESCE((SELECT SUM(payload_bytes) FROM items), 0)
                        + COALESCE((SELECT SUM(payload_bytes) FROM reports), 0)",
                (),
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .context("sqlite stats")?;
        let sources: i64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM report_sources", (), |r| r.get(0))
            .context("sqlite stats sources")?;
        Ok(serde_json::json!({
            "items": items,
            "reports": reports,
            "sources": sources,
            "payload_bytes": payload,
            "max_payload_bytes": MAX_PAYLOAD,
            "max_entries": MAX_ENTRIES,
        }))
    }

    fn early_report(
        &self,
        started: Instant,
        execution: &str,
        freshness: &str,
        warning: &str,
    ) -> CheckReport {
        CheckReport {
            id: uuid::Uuid::new_v4().to_string(),
            execution: execution.into(),
            basis: "deterministic".into(),
            freshness: freshness.into(),
            operator_version: OPERATOR_VERSION.into(),
            workspace: self.root.display().to_string(),
            generation: generation_of(&[]),
            coverage: Coverage::default(),
            items: Vec::new(),
            sources: Vec::new(),
            truncated: true,
            warnings: vec![warning.into()],
            elapsed_ms: elapsed_ms(started),
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn finish(
        &mut self,
        request: &CheckRequest,
        started: Instant,
        execution: &str,
        freshness: &str,
        coverage: Coverage,
        items: Vec<ItemResult>,
        sources: Vec<SourceFingerprint>,
        truncated: bool,
        warnings: Vec<String>,
        publish: bool,
    ) -> Result<CheckReport> {
        let report = CheckReport {
            id: uuid::Uuid::new_v4().to_string(),
            execution: execution.into(),
            basis: "deterministic".into(),
            freshness: freshness.into(),
            operator_version: OPERATOR_VERSION.into(),
            workspace: self.root.display().to_string(),
            generation: generation_of(&sources),
            coverage,
            items,
            sources,
            truncated,
            warnings,
            elapsed_ms: elapsed_ms(started),
        };
        let mut report = report;
        fit_report(&mut report);
        if publish {
            self.publish(request, &report)?;
        } else {
            self.reclaim()?;
        }
        Ok(report)
    }

    fn publish(&mut self, request: &CheckRequest, report: &CheckReport) -> Result<()> {
        let request_json = serde_json::to_string(request).context("serialize stored request")?;
        let report_json = serde_json::to_string(report).context("serialize stored report")?;
        let payload = (request_json.len() + report_json.len()) as i64;
        self.tick += 1;
        let tick = self.tick;
        let tx = self
            .conn
            .transaction()
            .context("sqlite begin report transaction")?;
        tx.execute(
            "INSERT INTO reports (id, request_json, report_json, execution, freshness, payload_bytes, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                report.id,
                request_json,
                report_json,
                report.execution,
                report.freshness,
                payload,
                tick
            ],
        )
        .context("sqlite insert report")?;
        for src in &report.sources {
            tx.execute(
                "INSERT INTO report_sources (report_id, path, fingerprint, bytes) VALUES (?1, ?2, ?3, ?4)",
                params![report.id, src.path, src.fingerprint, src.bytes as i64],
            )
            .context("sqlite insert report source")?;
        }
        tx.commit().context("sqlite commit report")?;
        self.reclaim()?;
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn scan_file(
        &mut self,
        abs: &Path,
        rel: &str,
        meta_len: u64,
        predicate_json: &str,
        compiled: &Compiled,
        records_budget: usize,
        results_left: &mut usize,
        detail_inside: &mut usize,
        detail_room: usize,
        warned_results: &mut bool,
        warned_bytes: &mut bool,
        deadline: Instant,
        cancel: &AtomicBool,
    ) -> Result<ScannedFile> {
        let file = File::open(abs).with_context(|| format!("open {}", abs.display()))?;
        let mut reader = BufReader::new(file);
        let mut file_hasher = Hasher::new();
        let mut remaining = meta_len;
        let mut coverage = Coverage::default();
        let mut items = Vec::new();
        let mut warnings = Vec::new();
        let mut inserts = Vec::new();
        let mut pending: HashMap<String, Cached> = HashMap::new();
        let mut truncated = false;
        let mut line_no = 0usize;
        let mut halt = Halt::None;
        let mut stop_eval = false;

        loop {
            if !stop_eval && cancel.load(Ordering::Relaxed) {
                return Ok(ScannedFile::discard(Halt::Cancel));
            }
            if !stop_eval && Instant::now() >= deadline {
                return Ok(ScannedFile::discard(Halt::Timeout));
            }
            if stop_eval && cancel.load(Ordering::Relaxed) {
                return Ok(ScannedFile::discard(Halt::Cancel));
            }
            if stop_eval && Instant::now() >= deadline {
                return Ok(ScannedFile::discard(Halt::Timeout));
            }
            match read_logical_line(&mut reader, &mut file_hasher, &mut remaining)? {
                ReadLine::Eof => break,
                ReadLine::Budget => return Ok(ScannedFile::discard(Halt::Unstable)),
                ReadLine::Line(line) => {
                    line_no += 1;
                    if line.oversized || !line.bytes.iter().all(|b| b.is_ascii_whitespace()) {
                        if coverage.records >= records_budget || stop_eval {
                            stop_eval = true;
                            halt = Halt::MaxRecords;
                            continue;
                        }
                        let key = item_cache_key(predicate_json, &line.hash);
                        let (tri, reason, value, value_json, payload, hit) =
                            if let Some(cached) = pending.get(&key) {
                                (
                                    tri_from_sql(cached.matched),
                                    cached.reason.clone(),
                                    value_from_sql(&cached.value_json),
                                    cached.value_json.clone(),
                                    0i64,
                                    true,
                                )
                            } else if let Some(cached) = self.lookup_item(&key)? {
                                let out = (
                                    tri_from_sql(cached.matched),
                                    cached.reason.clone(),
                                    value_from_sql(&cached.value_json),
                                    cached.value_json.clone(),
                                    0i64,
                                    true,
                                );
                                pending.insert(key, cached);
                                out
                            } else {
                                let eval = evaluate_line(compiled, &line);
                                let cached = Cached {
                                    matched: tri_to_sql(eval.tri),
                                    reason: eval.reason.clone(),
                                    value_json: eval.value_json.clone(),
                                };
                                inserts.push(NewItem {
                                    key: key.clone(),
                                    matched: cached.matched,
                                    reason: cached.reason.clone(),
                                    value_json: cached.value_json.clone(),
                                    payload: eval.payload,
                                    created_at: 0,
                                });
                                pending.insert(key, cached);
                                (
                                    eval.tri,
                                    eval.reason,
                                    eval.value,
                                    eval.value_json,
                                    eval.payload,
                                    false,
                                )
                            };
                        let _ = (value_json, payload);
                        coverage.records += 1;
                        coverage.evaluated += 1;
                        if hit {
                            coverage.cache_hits += 1;
                        } else {
                            coverage.cache_misses += 1;
                        }
                        match tri {
                            Tri::True => coverage.matched += 1,
                            Tri::False => coverage.unmatched += 1,
                            Tri::Unknown => coverage.unresolved += 1,
                        }
                        push_detail(
                            &mut items,
                            results_left,
                            detail_inside,
                            detail_room,
                            &mut truncated,
                            &mut warnings,
                            warned_results,
                            warned_bytes,
                            tri,
                            reason,
                            value,
                            SourceRef {
                                path: rel.to_string(),
                                line: line_no,
                                fingerprint: String::new(),
                            },
                        );
                    } else {
                        if stop_eval {
                            continue;
                        }
                        coverage.skipped += 1;
                    }
                }
            }
        }

        let bytes = meta_len - remaining;
        let fingerprint = file_hasher.finalize().to_hex().to_string();
        for item in &mut items {
            item.source.fingerprint = fingerprint.clone();
        }
        Ok(ScannedFile {
            fingerprint,
            bytes,
            coverage,
            items,
            warnings,
            inserts,
            truncated,
            halt,
        })
    }

    fn lookup_item(&mut self, key: &str) -> Result<Option<Cached>> {
        let row = self
            .conn
            .query_row(
                "SELECT matched, reason, value_json FROM items WHERE cache_key = ?1",
                params![key],
                |r| {
                    Ok(Cached {
                        matched: r.get(0)?,
                        reason: r.get(1)?,
                        value_json: r.get(2)?,
                    })
                },
            )
            .optional()
            .context("sqlite lookup item")?;
        if row.is_some() {
            self.tick += 1;
            let tick = self.tick;
            self.conn
                .execute(
                    "UPDATE items SET created_at = ?1 WHERE cache_key = ?2",
                    params![tick, key],
                )
                .context("sqlite touch item")?;
        }
        Ok(row)
    }

    fn insert_items(&mut self, items: &mut [NewItem]) -> Result<()> {
        if items.is_empty() {
            return Ok(());
        }
        for item in items.iter_mut() {
            self.tick += 1;
            item.created_at = self.tick;
        }
        let tx = self
            .conn
            .transaction()
            .context("sqlite begin item cache transaction")?;
        for item in items.iter() {
            tx.execute(
                "INSERT INTO items (cache_key, matched, reason, value_json, payload_bytes, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![
                    item.key,
                    item.matched,
                    item.reason,
                    item.value_json,
                    item.payload,
                    item.created_at
                ],
            )
            .context("sqlite insert item")?;
        }
        tx.commit().context("sqlite commit item cache")?;
        Ok(())
    }

    fn assess(&self, request: &CheckRequest, report: &CheckReport) -> Result<String> {
        let deadline = Instant::now() + Duration::from_millis(request.limits.timeout_ms);
        let listing = self.enumerate(&request.include, Some(deadline), None)?;
        if !listing.complete || Instant::now() >= deadline {
            return Ok("unknown".into());
        }
        let members = listing.paths;
        if !snapshot_holds(&report.execution, &report.sources, &members) {
            return Ok("stale".into());
        }
        for src in &report.sources {
            let abs = safe_join(&self.root, &src.path)?;
            match fs::symlink_metadata(&abs) {
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                    return Ok("stale".into());
                }
                Err(err) => {
                    return Err(err).with_context(|| format!("stat {}", abs.display()));
                }
                Ok(meta) if meta.file_type().is_symlink() || !meta.file_type().is_file() => {
                    return Ok("stale".into());
                }
                Ok(_) => {}
            }
            if Instant::now() >= deadline {
                return Ok("unknown".into());
            }
            let (fp, n) = hash_file(&abs).with_context(|| format!("hash {}", abs.display()))?;
            if fp != src.fingerprint || n != src.bytes {
                return Ok("stale".into());
            }
        }
        Ok("validated".into())
    }

    fn update_freshness(&mut self, report: &CheckReport) -> Result<()> {
        let report_json = serde_json::to_string(report).context("serialize updated report")?;
        self.conn
            .execute(
                "UPDATE reports SET freshness = ?1, report_json = ?2, payload_bytes = ?3 WHERE id = ?4",
                params![
                    report.freshness,
                    report_json,
                    report_json.len() as i64,
                    report.id
                ],
            )
            .context("sqlite update freshness")?;
        Ok(())
    }

    fn load_report(&self, id: &str) -> Result<Option<(CheckRequest, CheckReport)>> {
        let row = self
            .conn
            .query_row(
                "SELECT request_json, report_json FROM reports WHERE id = ?1",
                params![id],
                |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)),
            )
            .optional()
            .context("sqlite load report")?;
        let Some((request_json, report_json)) = row else {
            return Ok(None);
        };
        let request = serde_json::from_str(&request_json).context("decode stored check request")?;
        let report = serde_json::from_str(&report_json).context("decode stored check report")?;
        Ok(Some((request, report)))
    }

    fn enumerate(
        &self,
        includes: &[String],
        deadline: Option<Instant>,
        cancel: Option<&AtomicBool>,
    ) -> Result<Listing> {
        if includes.is_empty() {
            return Ok(Listing {
                paths: Vec::new(),
                complete: true,
            });
        }
        let mut builder = GlobSetBuilder::new();
        for pattern in includes {
            let glob = GlobBuilder::new(pattern)
                .literal_separator(true)
                .build()
                .with_context(|| format!("invalid include glob: {pattern}"))?;
            builder.add(glob);
        }
        let set = builder.build().context("build include globs")?;
        let walker = WalkBuilder::new(&self.root)
            .hidden(true)
            .ignore(true)
            .git_ignore(true)
            .git_global(false)
            .git_exclude(true)
            .require_git(false)
            .parents(false)
            .follow_links(false)
            .filter_entry(|ent| {
                if ent.depth() == 0 {
                    return true;
                }
                let name = ent.file_name();
                if name == ".git" || name == ".checkweave" {
                    return false;
                }
                !matches!(ent.file_type(), Some(ft) if ft.is_symlink())
            })
            .build();
        let mut out = Vec::new();
        let mut steps = 0usize;
        let mut complete = true;
        for ent in walker {
            steps += 1;
            if steps > MAX_WALK_STEPS
                || cancel.is_some_and(|flag| flag.load(Ordering::Relaxed))
                || deadline.is_some_and(|limit| Instant::now() >= limit)
            {
                complete = false;
                break;
            }
            let ent = ent.with_context(|| format!("walk {}", self.root.display()))?;
            if ent.depth() == 0 {
                continue;
            }
            if ent.file_type().map(|ft| ft.is_dir()).unwrap_or(false) {
                continue;
            }
            let path = ent.path();
            let meta = match fs::symlink_metadata(path) {
                Ok(meta) => meta,
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => continue,
                Err(err) => {
                    return Err(err).with_context(|| format!("stat {}", path.display()));
                }
            };
            if meta.file_type().is_symlink() || !meta.file_type().is_file() {
                continue;
            }
            let Some(rel) = rel_under(&self.root, path) else {
                continue;
            };
            if set.is_match(rel.as_str()) {
                out.push(rel);
            }
        }
        out.sort();
        out.dedup();
        Ok(Listing {
            paths: out,
            complete,
        })
    }

    fn reclaim(&mut self) -> Result<()> {
        let mut deleted = 0usize;
        for _ in 0..1_000 {
            let (items, reports, payload) = cache_totals(&self.conn)?;
            if items + reports <= MAX_ENTRIES && payload <= MAX_PAYLOAD {
                break;
            }
            let n = self.delete_oldest_batch()?;
            if n == 0 {
                break;
            }
            deleted += n;
        }
        if deleted > 0 {
            checkpoint_cache(&self.conn)?;
        }
        Ok(())
    }

    fn delete_oldest_batch(&mut self) -> Result<usize> {
        let rows = {
            let mut stmt = self
                .conn
                .prepare(
                    "SELECT kind, rid FROM (
                        SELECT 'item' AS kind, rowid AS rid, created_at AS ts FROM items
                        UNION ALL
                        SELECT 'report', rowid, created_at FROM reports
                     ) ORDER BY ts ASC LIMIT ?1",
                )
                .context("sqlite oldest cache rows")?;
            let mapped = stmt
                .query_map(params![RECLAIM_BATCH as i64], |r| {
                    Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?))
                })
                .context("sqlite oldest cache rows")?;
            let mut rows = Vec::new();
            for row in mapped {
                rows.push(row.context("sqlite oldest cache row")?);
            }
            rows
        };
        if rows.is_empty() {
            return Ok(0);
        }
        let items: Vec<i64> = rows
            .iter()
            .filter(|(kind, _)| kind == "item")
            .map(|(_, id)| *id)
            .collect();
        let reports: Vec<i64> = rows
            .iter()
            .filter(|(kind, _)| kind == "report")
            .map(|(_, id)| *id)
            .collect();
        let tx = self
            .conn
            .transaction()
            .context("sqlite begin reclaim transaction")?;
        delete_rowids(&tx, "items", &items)?;
        delete_rowids(&tx, "reports", &reports)?;
        tx.commit().context("sqlite commit reclaim")?;
        Ok(rows.len())
    }
}

struct ScannedFile {
    fingerprint: String,
    bytes: u64,
    coverage: Coverage,
    items: Vec<ItemResult>,
    warnings: Vec<String>,
    inserts: Vec<NewItem>,
    truncated: bool,
    halt: Halt,
}

impl ScannedFile {
    fn discard(halt: Halt) -> Self {
        Self {
            fingerprint: String::new(),
            bytes: 0,
            coverage: Coverage::default(),
            items: Vec::new(),
            warnings: Vec::new(),
            inserts: Vec::new(),
            truncated: false,
            halt,
        }
    }
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
    if limits.timeout_ms > HARD_MAX_TIMEOUT_MS {
        bail!(
            "timeout_ms {} exceeds hard cap {HARD_MAX_TIMEOUT_MS}",
            limits.timeout_ms
        );
    }
    Ok(())
}

fn validate_includes(includes: &[String]) -> Result<()> {
    if includes.len() > 256 {
        bail!("include pattern count exceeds hard cap 256");
    }
    for pattern in includes {
        if pattern.is_empty() {
            bail!("empty include pattern");
        }
        if pattern.len() > 4096 {
            bail!("include pattern exceeds hard cap");
        }
        if pattern.contains('\0') {
            bail!("invalid include pattern");
        }
        if Path::new(pattern).is_absolute() {
            bail!("include pattern must be relative: {pattern}");
        }
        for part in pattern.split(['/', '\\']) {
            if part == ".." {
                bail!("include pattern escapes workspace: {pattern}");
            }
        }
        for comp in Path::new(pattern).components() {
            match comp {
                Component::Normal(_) | Component::CurDir => {}
                _ => bail!("include pattern escapes workspace: {pattern}"),
            }
        }
        GlobBuilder::new(pattern)
            .literal_separator(true)
            .build()
            .with_context(|| format!("invalid include glob: {pattern}"))?;
    }
    Ok(())
}

fn predicate_exceeds_depth(pred: &Predicate) -> bool {
    let mut stack = vec![(pred, 1usize)];
    while let Some((node, depth)) = stack.pop() {
        if depth > MAX_DEPTH {
            return true;
        }
        match node {
            Predicate::All { predicates } | Predicate::Any { predicates } => {
                for child in predicates {
                    stack.push((child, depth + 1));
                }
            }
            Predicate::Not { predicate } => stack.push((predicate, depth + 1)),
            _ => {}
        }
    }
    false
}

fn compile_predicate(pred: &Predicate) -> Result<Compiled> {
    let mut nodes = 0usize;
    compile_at(pred, 1, &mut nodes)
}

fn compile_at(pred: &Predicate, depth: usize, nodes: &mut usize) -> Result<Compiled> {
    if depth > MAX_DEPTH {
        bail!("predicate exceeds maximum depth {MAX_DEPTH}");
    }
    *nodes += 1;
    if *nodes > MAX_NODES {
        bail!("predicate exceeds maximum node count {MAX_NODES}");
    }
    match pred {
        Predicate::Exists { path } => Ok(Compiled::Exists {
            path: parse_pointer(path)?,
        }),
        Predicate::Eq { path, value } => Ok(Compiled::Eq {
            path: parse_pointer(path)?,
            value: value.clone(),
        }),
        Predicate::Ne { path, value } => Ok(Compiled::Ne {
            path: parse_pointer(path)?,
            value: value.clone(),
        }),
        Predicate::Contains { path, value } => Ok(Compiled::Contains {
            path: parse_pointer(path)?,
            value: value.clone(),
        }),
        Predicate::Regex { path, pattern } => {
            if pattern.len() > MAX_REGEX_PATTERN {
                bail!("regex pattern exceeds size limit {MAX_REGEX_PATTERN}");
            }
            let re = RegexBuilder::new(pattern)
                .size_limit(1 << 20)
                .dfa_size_limit(1 << 20)
                .build()
                .with_context(|| format!("invalid regex: {pattern}"))?;
            Ok(Compiled::Regex {
                path: parse_pointer(path)?,
                re,
            })
        }
        Predicate::Gt { path, value } => Ok(Compiled::Gt {
            path: parse_pointer(path)?,
            value: finite_param(*value)?,
        }),
        Predicate::Ge { path, value } => Ok(Compiled::Ge {
            path: parse_pointer(path)?,
            value: finite_param(*value)?,
        }),
        Predicate::Lt { path, value } => Ok(Compiled::Lt {
            path: parse_pointer(path)?,
            value: finite_param(*value)?,
        }),
        Predicate::Le { path, value } => Ok(Compiled::Le {
            path: parse_pointer(path)?,
            value: finite_param(*value)?,
        }),
        Predicate::Kind { path, kind } => Ok(Compiled::Kind {
            path: parse_pointer(path)?,
            kind: kind.clone(),
        }),
        Predicate::All { predicates } => {
            let mut out = Vec::with_capacity(predicates.len());
            for child in predicates {
                out.push(compile_at(child, depth + 1, nodes)?);
            }
            Ok(Compiled::All { predicates: out })
        }
        Predicate::Any { predicates } => {
            let mut out = Vec::with_capacity(predicates.len());
            for child in predicates {
                out.push(compile_at(child, depth + 1, nodes)?);
            }
            Ok(Compiled::Any { predicates: out })
        }
        Predicate::Not { predicate } => Ok(Compiled::Not {
            predicate: Box::new(compile_at(predicate, depth + 1, nodes)?),
        }),
    }
}

fn finite_param(value: f64) -> Result<f64> {
    if !value.is_finite() {
        bail!("non-finite numeric parameter");
    }
    Ok(value)
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
    while let Some(c) = chars.next() {
        if c == '~' {
            match chars.next() {
                Some('0') => out.push('~'),
                Some('1') => out.push('/'),
                _ => bail!("invalid JSON pointer escape"),
            }
        } else {
            out.push(c);
        }
    }
    Ok(out)
}

fn lookup<'a>(value: &'a Value, pointer: &Pointer) -> Option<&'a Value> {
    let mut cur = value;
    for seg in &pointer.segments {
        match cur {
            Value::Object(map) => cur = map.get(seg)?,
            Value::Array(arr) => {
                let idx = array_index(seg)?;
                cur = arr.get(idx)?;
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

fn eval_pred(pred: &Compiled, value: &Value, lossy: &HashSet<String>) -> (Tri, Option<String>) {
    match pred {
        Compiled::Exists { path } => {
            if lookup(value, path).is_some() {
                (Tri::True, None)
            } else {
                (Tri::False, None)
            }
        }
        Compiled::Eq {
            path,
            value: expected,
        } => blocked_or(lossy, path, || match lookup(value, path) {
            None => (Tri::Unknown, Some("missing value".into())),
            Some(got) => {
                if json_eq(got, expected) {
                    (Tri::True, None)
                } else {
                    (Tri::False, None)
                }
            }
        }),
        Compiled::Ne {
            path,
            value: expected,
        } => blocked_or(lossy, path, || match lookup(value, path) {
            None => (Tri::Unknown, Some("missing value".into())),
            Some(got) => {
                if json_eq(got, expected) {
                    (Tri::False, None)
                } else {
                    (Tri::True, None)
                }
            }
        }),
        Compiled::Contains {
            path,
            value: needle,
        } => match lookup(value, path) {
            None => (Tri::Unknown, Some("missing value".into())),
            Some(Value::String(hay)) => {
                if hay.contains(needle) {
                    (Tri::True, None)
                } else {
                    (Tri::False, None)
                }
            }
            Some(_) => (Tri::Unknown, Some("type mismatch".into())),
        },
        Compiled::Regex { path, re } => match lookup(value, path) {
            None => (Tri::Unknown, Some("missing value".into())),
            Some(Value::String(hay)) => {
                if re.is_match(hay) {
                    (Tri::True, None)
                } else {
                    (Tri::False, None)
                }
            }
            Some(_) => (Tri::Unknown, Some("type mismatch".into())),
        },
        Compiled::Gt { path, value: rhs } => {
            blocked_or(lossy, path, || cmp_num(path, value, *rhs, Cmp::Gt))
        }
        Compiled::Ge { path, value: rhs } => {
            blocked_or(lossy, path, || cmp_num(path, value, *rhs, Cmp::Ge))
        }
        Compiled::Lt { path, value: rhs } => {
            blocked_or(lossy, path, || cmp_num(path, value, *rhs, Cmp::Lt))
        }
        Compiled::Le { path, value: rhs } => {
            blocked_or(lossy, path, || cmp_num(path, value, *rhs, Cmp::Le))
        }
        Compiled::Kind { path, kind } => match lookup(value, path) {
            None => (Tri::Unknown, Some("missing value".into())),
            Some(got) => {
                if kind_matches(kind, got) {
                    (Tri::True, None)
                } else {
                    (Tri::False, None)
                }
            }
        },
        Compiled::All { predicates } => {
            let mut unknown = None;
            for child in predicates {
                match eval_pred(child, value, lossy) {
                    (Tri::False, _) => return (Tri::False, None),
                    (Tri::Unknown, reason) => {
                        if unknown.is_none() {
                            unknown = Some(reason.unwrap_or_else(|| "unresolved".into()));
                        }
                    }
                    (Tri::True, _) => {}
                }
            }
            if let Some(reason) = unknown {
                (Tri::Unknown, Some(reason))
            } else {
                (Tri::True, None)
            }
        }
        Compiled::Any { predicates } => {
            let mut unknown = None;
            for child in predicates {
                match eval_pred(child, value, lossy) {
                    (Tri::True, _) => return (Tri::True, None),
                    (Tri::Unknown, reason) => {
                        if unknown.is_none() {
                            unknown = Some(reason.unwrap_or_else(|| "unresolved".into()));
                        }
                    }
                    (Tri::False, _) => {}
                }
            }
            if let Some(reason) = unknown {
                (Tri::Unknown, Some(reason))
            } else {
                (Tri::False, None)
            }
        }
        Compiled::Not { predicate } => match eval_pred(predicate, value, lossy) {
            (Tri::True, _) => (Tri::False, None),
            (Tri::False, _) => (Tri::True, None),
            (Tri::Unknown, reason) => (
                Tri::Unknown,
                Some(reason.unwrap_or_else(|| "unresolved".into())),
            ),
        },
    }
}

#[derive(Clone, Copy)]
enum Cmp {
    Gt,
    Ge,
    Lt,
    Le,
}

fn blocked_or(
    lossy: &HashSet<String>,
    path: &Pointer,
    eval: impl FnOnce() -> (Tri, Option<String>),
) -> (Tri, Option<String>) {
    if lossy.contains(&pointer_string(&path.segments)) {
        (Tri::Unknown, Some("out-of-range integer".into()))
    } else {
        eval()
    }
}

fn cmp_num(path: &Pointer, value: &Value, rhs: f64, op: Cmp) -> (Tri, Option<String>) {
    match lookup(value, path) {
        None => (Tri::Unknown, Some("missing value".into())),
        Some(Value::Number(n)) => match number_ord(n, rhs) {
            Some(ord) => {
                let ok = match op {
                    Cmp::Gt => ord == std::cmp::Ordering::Greater,
                    Cmp::Ge => ord != std::cmp::Ordering::Less,
                    Cmp::Lt => ord == std::cmp::Ordering::Less,
                    Cmp::Le => ord != std::cmp::Ordering::Greater,
                };
                if ok {
                    (Tri::True, None)
                } else {
                    (Tri::False, None)
                }
            }
            None => (Tri::Unknown, Some("nonfinite number".into())),
        },
        Some(_) => (Tri::Unknown, Some("type mismatch".into())),
    }
}

fn kind_matches(kind: &JsonKind, value: &Value) -> bool {
    matches!(
        (kind, value),
        (JsonKind::Null, Value::Null)
            | (JsonKind::Boolean, Value::Bool(_))
            | (JsonKind::Number, Value::Number(_))
            | (JsonKind::String, Value::String(_))
            | (JsonKind::Array, Value::Array(_))
            | (JsonKind::Object, Value::Object(_))
    )
}

fn evaluate_line(compiled: &Compiled, line: &LogicalLine) -> EvalOut {
    if line.oversized {
        return EvalOut::unknown("record exceeds capture limit");
    }
    if std::str::from_utf8(&line.bytes).is_err() {
        return EvalOut::unknown("invalid utf-8");
    }
    match serde_json::from_slice::<Value>(&line.bytes) {
        Err(_) => EvalOut::unknown("malformed json"),
        Ok(value) => {
            let lossy = mark_lossy(std::str::from_utf8(&line.bytes).unwrap_or(""));
            let (tri, reason) = eval_pred(compiled, &value, &lossy);
            let packed = pack_value(&value);
            let reason = if packed.omitted && !matches!(tri, Tri::Unknown) {
                Some("value omitted: detail budget".into())
            } else {
                reason
            };
            EvalOut {
                tri,
                reason,
                value: packed.value,
                value_json: packed.value_json,
                payload: packed.payload,
            }
        }
    }
}

struct Packed {
    value: Option<Value>,
    value_json: Option<String>,
    payload: i64,
    omitted: bool,
}

fn pack_value(value: &Value) -> Packed {
    match serde_json::to_string(value) {
        Ok(text) if text.len() <= MAX_VALUE_BYTES => {
            let payload = (text.len() + 64) as i64;
            Packed {
                value: Some(value.clone()),
                value_json: Some(text),
                payload,
                omitted: false,
            }
        }
        _ => Packed {
            value: None,
            value_json: None,
            payload: 64,
            omitted: true,
        },
    }
}

impl EvalOut {
    fn unknown(reason: &str) -> Self {
        Self {
            tri: Tri::Unknown,
            reason: Some(reason.into()),
            value: None,
            value_json: None,
            payload: 64,
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn push_detail(
    items: &mut Vec<ItemResult>,
    results_left: &mut usize,
    detail_inside: &mut usize,
    detail_room: usize,
    truncated: &mut bool,
    warnings: &mut Vec<String>,
    warned_results: &mut bool,
    warned_bytes: &mut bool,
    tri: Tri,
    reason: Option<String>,
    value: Option<Value>,
    mut source: SourceRef,
) {
    if tri == Tri::False {
        return;
    }
    if *results_left == 0 {
        *truncated = true;
        if !*warned_results {
            *warned_results = true;
            warnings.push("results truncated: max_results".into());
        }
        return;
    }
    if source.fingerprint.len() != 64 {
        source.fingerprint = "0".repeat(64);
    }
    let matched = match tri {
        Tri::True => Some(true),
        Tri::Unknown => None,
        Tri::False => return,
    };
    let mut item = ItemResult {
        source,
        matched,
        reason,
        value,
    };
    let comma = usize::from(*detail_inside > 0);
    let full = json_len(&item);
    if detail_inside.saturating_add(comma).saturating_add(full) > detail_room {
        item.value = None;
        if item.reason.is_none() {
            item.reason = Some("value omitted: response budget".into());
        }
        let bare = json_len(&item);
        if detail_inside.saturating_add(comma).saturating_add(bare) > detail_room {
            *truncated = true;
            if !*warned_bytes {
                *warned_bytes = true;
                warnings.push("omitted detail: response byte budget".into());
            }
            return;
        }
        *truncated = true;
        if !*warned_bytes {
            *warned_bytes = true;
            warnings.push("omitted detail: response byte budget".into());
        }
        *detail_inside = detail_inside.saturating_add(comma + bare);
    } else {
        *detail_inside = detail_inside.saturating_add(comma + full);
    }
    *results_left = results_left.saturating_sub(1);
    items.push(item);
}

fn json_len(value: &impl serde::Serialize) -> usize {
    serde_json::to_vec(value)
        .map(|bytes| bytes.len())
        .unwrap_or(usize::MAX)
}

fn array_inside(lengths: &[usize]) -> usize {
    if lengths.is_empty() {
        0
    } else {
        lengths.iter().sum::<usize>() + lengths.len() - 1
    }
}

fn source_entry_len(path: &str, bytes: u64) -> usize {
    json_len(&SourceFingerprint {
        path: path.to_string(),
        fingerprint: "0".repeat(64),
        bytes,
    })
}

/// Upper bound on the report document with empty item and source arrays.
/// Real coverage numbers and a subset of these warnings are smaller.
fn report_envelope_len(workspace: &str) -> usize {
    let warnings = [
        "cancelled",
        "stopped: timeout",
        "stopped: traversal budget",
        "stopped: report metadata budget",
        "stopped: max_files",
        "stopped: max_bytes",
        "stopped: max_records",
        "stopped: validation budget",
        "stale: inputs changed before publication",
        "results truncated: max_results",
        "omitted detail: response byte budget",
    ];
    let mut listed = Vec::new();
    for warning in warnings {
        listed.push(warning.to_string());
        listed.push(warning.to_string());
    }
    let report = CheckReport {
        id: "00000000-0000-0000-0000-000000000000".into(),
        execution: "cancelled".into(),
        basis: "deterministic".into(),
        freshness: "validated".into(),
        operator_version: OPERATOR_VERSION.into(),
        workspace: workspace.to_string(),
        generation: "0".repeat(64),
        coverage: Coverage {
            files: usize::MAX,
            records: usize::MAX,
            evaluated: usize::MAX,
            matched: usize::MAX,
            unmatched: usize::MAX,
            unresolved: usize::MAX,
            skipped: usize::MAX,
            cache_hits: usize::MAX,
            cache_misses: usize::MAX,
        },
        items: Vec::new(),
        sources: Vec::new(),
        truncated: false,
        warnings: listed,
        elapsed_ms: u64::MAX,
    };
    json_len(&report)
}

/// Drop item values, then trailing items, until the exact report JSON fits.
/// Source rows stay so freshness still describes the scanned snapshot.
fn fit_report(report: &mut CheckReport) {
    if json_len(report) <= REPORT_JSON_BUDGET {
        return;
    }
    report.truncated = true;
    if !report
        .warnings
        .iter()
        .any(|warning| warning.contains("response byte budget"))
    {
        report
            .warnings
            .push("omitted detail: response byte budget".into());
    }
    let full_lens: Vec<usize> = report.items.iter().map(json_len).collect();
    let size = json_len(report);
    let inside = array_inside(&full_lens);
    if inside > size {
        binary_fit_items(report);
        return;
    }
    let shell = size - inside;
    let mut lens = full_lens;
    for index in (0..report.items.len()).rev() {
        if shell + array_inside(&lens) <= REPORT_JSON_BUDGET {
            break;
        }
        if report.items[index].value.is_some() {
            report.items[index].value = None;
            if report.items[index].reason.is_none() {
                report.items[index].reason = Some("value omitted: response budget".into());
            }
            lens[index] = json_len(&report.items[index]);
        }
    }
    while shell + array_inside(&lens) > REPORT_JSON_BUDGET && !lens.is_empty() {
        lens.pop();
        report.items.pop();
    }
    if json_len(report) > REPORT_JSON_BUDGET {
        binary_fit_items(report);
    }
}

fn binary_fit_items(report: &mut CheckReport) {
    let saved = std::mem::take(&mut report.items);
    let mut lo = 0usize;
    let mut hi = saved.len();
    while lo < hi {
        let mid = (lo + hi).div_ceil(2);
        report.items = saved[..mid].to_vec();
        if json_len(report) <= REPORT_JSON_BUDGET {
            lo = mid;
        } else {
            hi = mid - 1;
        }
    }
    report.items = saved[..lo].to_vec();
    if json_len(report) > REPORT_JSON_BUDGET {
        report.items.clear();
    }
}

fn tri_to_sql(tri: Tri) -> Option<i64> {
    match tri {
        Tri::True => Some(1),
        Tri::False => Some(0),
        Tri::Unknown => None,
    }
}

fn tri_from_sql(matched: Option<i64>) -> Tri {
    match matched {
        Some(1) => Tri::True,
        Some(0) => Tri::False,
        _ => Tri::Unknown,
    }
}

fn value_from_sql(text: &Option<String>) -> Option<Value> {
    text.as_ref().and_then(|raw| serde_json::from_str(raw).ok())
}

fn item_cache_key(predicate_json: &str, record_hash: &str) -> String {
    let mut hasher = Hasher::new();
    hasher.update(OPERATOR_VERSION.as_bytes());
    hasher.update(&[0]);
    hasher.update(predicate_json.as_bytes());
    hasher.update(&[0]);
    hasher.update(record_hash.as_bytes());
    hasher.finalize().to_hex().to_string()
}

fn generation_of(sources: &[SourceFingerprint]) -> String {
    let mut hasher = Hasher::new();
    for src in sources {
        hasher.update(src.path.as_bytes());
        hasher.update(&[0]);
        hasher.update(src.fingerprint.as_bytes());
        hasher.update(&[0]);
        hasher.update(&src.bytes.to_le_bytes());
        hasher.update(&[0]);
    }
    hasher.finalize().to_hex().to_string()
}

fn snapshot_holds(execution: &str, sources: &[SourceFingerprint], members: &[String]) -> bool {
    let recorded: Vec<&str> = sources.iter().map(|s| s.path.as_str()).collect();
    let member_refs: Vec<&str> = members.iter().map(|s| s.as_str()).collect();
    if execution == "complete" {
        recorded == member_refs
    } else {
        recorded.len() <= member_refs.len()
            && recorded
                .iter()
                .copied()
                .eq(member_refs.iter().copied().take(recorded.len()))
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum SourceObs {
    Same,
    Changed,
    Missing,
}

struct Listing {
    paths: Vec<String>,
    complete: bool,
}

fn observe_source(root: &Path, src: &SourceFingerprint) -> Result<SourceObs> {
    let abs = safe_join(root, &src.path)?;
    match fs::symlink_metadata(&abs) {
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(SourceObs::Missing),
        Err(err) => Err(err).with_context(|| format!("stat {}", abs.display())),
        Ok(meta) if meta.file_type().is_symlink() || !meta.file_type().is_file() => {
            Ok(SourceObs::Changed)
        }
        Ok(_) => {
            let (fp, n) = hash_file(&abs)?;
            if fp == src.fingerprint && n == src.bytes {
                Ok(SourceObs::Same)
            } else {
                Ok(SourceObs::Changed)
            }
        }
    }
}

fn cache_totals(conn: &Connection) -> Result<(i64, i64, i64)> {
    conn.query_row(
        "SELECT
            (SELECT COUNT(*) FROM items),
            (SELECT COUNT(*) FROM reports),
            COALESCE((SELECT SUM(payload_bytes) FROM items), 0)
                + COALESCE((SELECT SUM(payload_bytes) FROM reports), 0)",
        (),
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
    )
    .context("sqlite cache size")
}

fn delete_rowids(conn: &Connection, table: &str, ids: &[i64]) -> Result<()> {
    if ids.is_empty() {
        return Ok(());
    }
    let placeholders = vec!["?"; ids.len()].join(",");
    let sql = format!("DELETE FROM {table} WHERE rowid IN ({placeholders})");
    conn.execute(&sql, rusqlite::params_from_iter(ids.iter()))
        .with_context(|| format!("sqlite delete {table} batch"))?;
    Ok(())
}

fn checkpoint_cache(conn: &Connection) -> Result<()> {
    conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);")
        .context("sqlite wal checkpoint")?;
    let _ = conn.execute_batch("PRAGMA incremental_vacuum(32);");
    Ok(())
}

fn sqlite_corruption(err: &anyhow::Error) -> bool {
    for cause in err.chain() {
        let text = cause.to_string().to_ascii_lowercase();
        if text.contains("integrity check failed")
            || text.contains("file is not a database")
            || text.contains("database disk image is malformed")
            || text.contains("malformed database schema")
        {
            return true;
        }
        if let Some(sql) = cause.downcast_ref::<rusqlite::Error>()
            && let Some(code) = sql.sqlite_error_code()
            && matches!(
                code,
                rusqlite::ffi::ErrorCode::DatabaseCorrupt | rusqlite::ffi::ErrorCode::NotADatabase
            )
        {
            return true;
        }
    }
    false
}

fn pointer_string(segments: &[String]) -> String {
    let mut out = String::new();
    for seg in segments {
        out.push('/');
        for ch in seg.chars() {
            match ch {
                '~' => out.push_str("~0"),
                '/' => out.push_str("~1"),
                other => out.push(other),
            }
        }
    }
    out
}

fn mark_ancestors(stack: &[String], set: &mut HashSet<String>) {
    for len in 0..=stack.len() {
        set.insert(pointer_string(&stack[..len]));
    }
}

fn mark_lossy(text: &str) -> HashSet<String> {
    let bytes = text.as_bytes();
    let mut index = 0usize;
    let mut stack = Vec::new();
    let mut set = HashSet::new();
    let _ = scan_json_value(bytes, &mut index, &mut stack, &mut set);
    set
}

fn scan_json_value(
    bytes: &[u8],
    index: &mut usize,
    stack: &mut Vec<String>,
    set: &mut HashSet<String>,
) -> bool {
    skip_json_ws(bytes, index);
    if *index >= bytes.len() {
        return false;
    }
    match bytes[*index] {
        b'{' => scan_json_object(bytes, index, stack, set),
        b'[' => scan_json_array(bytes, index, stack, set),
        b'"' => skip_json_string(bytes, index).is_some(),
        b't' | b'f' | b'n' => skip_json_literal(bytes, index),
        b'-' | b'0'..=b'9' => scan_json_number(bytes, index, stack, set),
        _ => false,
    }
}

fn scan_json_object(
    bytes: &[u8],
    index: &mut usize,
    stack: &mut Vec<String>,
    set: &mut HashSet<String>,
) -> bool {
    *index += 1;
    loop {
        skip_json_ws(bytes, index);
        if *index < bytes.len() && bytes[*index] == b'}' {
            *index += 1;
            return true;
        }
        let Some(key) = skip_json_string(bytes, index) else {
            return false;
        };
        skip_json_ws(bytes, index);
        if *index >= bytes.len() || bytes[*index] != b':' {
            return false;
        }
        *index += 1;
        stack.push(key);
        if !scan_json_value(bytes, index, stack, set) {
            return false;
        }
        stack.pop();
        skip_json_ws(bytes, index);
        if *index < bytes.len() && bytes[*index] == b',' {
            *index += 1;
            continue;
        }
        if *index < bytes.len() && bytes[*index] == b'}' {
            *index += 1;
            return true;
        }
        return false;
    }
}

fn scan_json_array(
    bytes: &[u8],
    index: &mut usize,
    stack: &mut Vec<String>,
    set: &mut HashSet<String>,
) -> bool {
    *index += 1;
    let mut slot = 0usize;
    loop {
        skip_json_ws(bytes, index);
        if *index < bytes.len() && bytes[*index] == b']' {
            *index += 1;
            return true;
        }
        stack.push(slot.to_string());
        slot += 1;
        if !scan_json_value(bytes, index, stack, set) {
            return false;
        }
        stack.pop();
        skip_json_ws(bytes, index);
        if *index < bytes.len() && bytes[*index] == b',' {
            *index += 1;
            continue;
        }
        if *index < bytes.len() && bytes[*index] == b']' {
            *index += 1;
            return true;
        }
        return false;
    }
}

fn scan_json_number(
    bytes: &[u8],
    index: &mut usize,
    stack: &[String],
    set: &mut HashSet<String>,
) -> bool {
    let start = *index;
    if bytes[*index] == b'-' {
        *index += 1;
    }
    if *index >= bytes.len() || !bytes[*index].is_ascii_digit() {
        return false;
    }
    while *index < bytes.len() && bytes[*index].is_ascii_digit() {
        *index += 1;
    }
    let int_end = *index;
    let mut fractional = false;
    if *index < bytes.len()
        && (bytes[*index] == b'.' || bytes[*index] == b'e' || bytes[*index] == b'E')
    {
        fractional = true;
        *index += 1;
        while *index < bytes.len()
            && (bytes[*index].is_ascii_digit()
                || bytes[*index] == b'+'
                || bytes[*index] == b'-'
                || bytes[*index] == b'e'
                || bytes[*index] == b'E')
        {
            *index += 1;
        }
    }
    if !fractional
        && let Ok(lexeme) = std::str::from_utf8(&bytes[start..int_end])
        && integer_out_of_range(lexeme)
    {
        mark_ancestors(stack, set);
    }
    true
}

fn integer_out_of_range(lexeme: &str) -> bool {
    let (negative, digits) = if let Some(rest) = lexeme.strip_prefix('-') {
        (true, rest)
    } else {
        (false, lexeme.strip_prefix('+').unwrap_or(lexeme))
    };
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return false;
    }
    let digits = digits.trim_start_matches('0');
    if digits.is_empty() {
        return false;
    }
    let limit = if negative {
        "9223372036854775808"
    } else {
        "18446744073709551615"
    };
    match digits.len().cmp(&limit.len()) {
        std::cmp::Ordering::Greater => true,
        std::cmp::Ordering::Less => false,
        std::cmp::Ordering::Equal => digits > limit,
    }
}

fn skip_json_ws(bytes: &[u8], index: &mut usize) {
    while *index < bytes.len() && bytes[*index].is_ascii_whitespace() {
        *index += 1;
    }
}

fn skip_json_literal(bytes: &[u8], index: &mut usize) -> bool {
    for literal in [b"true".as_slice(), b"false".as_slice(), b"null".as_slice()] {
        if bytes[*index..].starts_with(literal) {
            *index += literal.len();
            return true;
        }
    }
    false
}

fn skip_json_string(bytes: &[u8], index: &mut usize) -> Option<String> {
    if bytes.get(*index) != Some(&b'"') {
        return None;
    }
    *index += 1;
    let mut out = String::new();
    while *index < bytes.len() {
        let ch = std::str::from_utf8(&bytes[*index..]).ok()?.chars().next()?;
        *index += ch.len_utf8();
        match ch {
            '"' => return Some(out),
            '\\' => {
                let esc = std::str::from_utf8(&bytes[*index..]).ok()?.chars().next()?;
                *index += esc.len_utf8();
                match esc {
                    '"' => out.push('"'),
                    '\\' => out.push('\\'),
                    '/' => out.push('/'),
                    'b' => out.push('\u{0008}'),
                    'f' => out.push('\u{000c}'),
                    'n' => out.push('\n'),
                    'r' => out.push('\r'),
                    't' => out.push('\t'),
                    'u' => {
                        if *index + 4 > bytes.len() {
                            return None;
                        }
                        let hex = std::str::from_utf8(&bytes[*index..*index + 4]).ok()?;
                        let code = u32::from_str_radix(hex, 16).ok()?;
                        *index += 4;
                        out.push(char::from_u32(code)?);
                    }
                    _ => return None,
                }
            }
            other => out.push(other),
        }
    }
    None
}

fn io_not_found(err: &anyhow::Error) -> bool {
    err.chain().any(|cause| {
        cause
            .downcast_ref::<std::io::Error>()
            .is_some_and(|io| io.kind() == std::io::ErrorKind::NotFound)
    })
}

fn elapsed_ms(started: Instant) -> u64 {
    started.elapsed().as_millis() as u64
}

fn json_eq(left: &Value, right: &Value) -> bool {
    match (left, right) {
        (Value::Null, Value::Null) => true,
        (Value::Bool(a), Value::Bool(b)) => a == b,
        (Value::String(a), Value::String(b)) => a == b,
        (Value::Number(a), Value::Number(b)) => numbers_eq(a, b),
        (Value::Array(a), Value::Array(b)) => {
            a.len() == b.len() && a.iter().zip(b).all(|(x, y)| json_eq(x, y))
        }
        (Value::Object(a), Value::Object(b)) => {
            a.len() == b.len()
                && a.iter()
                    .all(|(k, v)| b.get(k).is_some_and(|w| json_eq(v, w)))
        }
        _ => false,
    }
}

enum Exact {
    Int(i128),
    Float(f64),
}

fn exact_num(n: &Number) -> Option<Exact> {
    if let Some(i) = n.as_i64() {
        Some(Exact::Int(i128::from(i)))
    } else if let Some(u) = n.as_u64() {
        Some(Exact::Int(i128::from(u)))
    } else {
        n.as_f64().filter(|f| f.is_finite()).map(Exact::Float)
    }
}

fn numbers_eq(left: &Number, right: &Number) -> bool {
    match (exact_num(left), exact_num(right)) {
        (Some(Exact::Int(a)), Some(Exact::Int(b))) => a == b,
        (Some(Exact::Float(a)), Some(Exact::Float(b))) => a == b,
        (Some(Exact::Int(i)), Some(Exact::Float(f)))
        | (Some(Exact::Float(f)), Some(Exact::Int(i))) => {
            cmp_i128_f64(i, f) == Some(std::cmp::Ordering::Equal)
        }
        _ => false,
    }
}

fn number_ord(n: &Number, rhs: f64) -> Option<std::cmp::Ordering> {
    if !rhs.is_finite() {
        return None;
    }
    match exact_num(n)? {
        Exact::Float(f) => f.partial_cmp(&rhs),
        Exact::Int(i) => cmp_i128_f64(i, rhs),
    }
}

fn cmp_i128_f64(i: i128, f: f64) -> Option<std::cmp::Ordering> {
    if !f.is_finite() {
        return None;
    }
    const LIM: i128 = 1 << 53;
    if (-LIM..=LIM).contains(&i) {
        return (i as f64).partial_cmp(&f);
    }
    let neg_i = i < 0;
    let neg_f = f.is_sign_negative();
    if neg_i != neg_f {
        return Some(if neg_i {
            std::cmp::Ordering::Less
        } else {
            std::cmp::Ordering::Greater
        });
    }
    let mag_ord = cmp_u128_pos(i.unsigned_abs(), f.abs())?;
    Some(if neg_i { mag_ord.reverse() } else { mag_ord })
}

fn cmp_u128_pos(mag: u128, f: f64) -> Option<std::cmp::Ordering> {
    if !f.is_finite() || f < 0.0 {
        return None;
    }
    if f == 0.0 {
        return Some(if mag == 0 {
            std::cmp::Ordering::Equal
        } else {
            std::cmp::Ordering::Greater
        });
    }
    let bits = f.to_bits();
    let exp = ((bits >> 52) & 0x7ff) as i32;
    let frac = bits & ((1u64 << 52) - 1);
    if exp == 0 {
        return Some(std::cmp::Ordering::Greater);
    }
    if exp == 0x7ff {
        return None;
    }
    let mant = u128::from(frac | (1u64 << 52));
    let shift = exp - 1023 - 52;
    if shift >= 0 {
        let sh = shift as u32;
        if sh >= 128 {
            return Some(std::cmp::Ordering::Less);
        }
        Some(mag.cmp(&(mant << sh)))
    } else {
        let sh = (-shift) as u32;
        if sh >= 128 {
            return Some(std::cmp::Ordering::Greater);
        }
        let int_part = mant >> sh;
        let rem = mant & ((1u128 << sh) - 1);
        if mag > int_part {
            Some(std::cmp::Ordering::Greater)
        } else if mag < int_part {
            Some(std::cmp::Ordering::Less)
        } else if rem == 0 {
            Some(std::cmp::Ordering::Equal)
        } else {
            Some(std::cmp::Ordering::Less)
        }
    }
}

fn read_logical_line<R: BufRead>(
    reader: &mut R,
    file_hasher: &mut Hasher,
    remaining: &mut u64,
) -> std::io::Result<ReadLine> {
    let mut logical = Vec::new();
    let mut hasher = Hasher::new();
    let mut oversized = false;
    let mut pending_cr = false;
    let mut saw = false;
    loop {
        if *remaining == 0 {
            if !saw && !pending_cr {
                let extra = reader.fill_buf()?;
                if extra.is_empty() {
                    return Ok(ReadLine::Eof);
                }
                return Ok(ReadLine::Budget);
            }
            return Ok(ReadLine::Budget);
        }
        let avail = reader.fill_buf()?;
        if avail.is_empty() {
            if pending_cr {
                push_byte(&mut logical, &mut hasher, &mut oversized, b'\r');
            }
            if !saw && logical.is_empty() && !oversized {
                return Ok(ReadLine::Eof);
            }
            return Ok(ReadLine::Line(LogicalLine {
                bytes: if oversized { Vec::new() } else { logical },
                hash: hasher.finalize().to_hex().to_string(),
                oversized,
            }));
        }
        let allow = (*remaining).min(avail.len() as u64) as usize;
        let chunk = &avail[..allow];
        let mut consumed = 0usize;
        let mut finished = false;
        for &b in chunk {
            consumed += 1;
            saw = true;
            if pending_cr {
                if b == b'\n' {
                    pending_cr = false;
                    finished = true;
                    break;
                }
                push_byte(&mut logical, &mut hasher, &mut oversized, b'\r');
                pending_cr = false;
            }
            if b == b'\r' {
                pending_cr = true;
                continue;
            }
            if b == b'\n' {
                finished = true;
                break;
            }
            push_byte(&mut logical, &mut hasher, &mut oversized, b);
        }
        file_hasher.update(&chunk[..consumed]);
        *remaining -= consumed as u64;
        reader.consume(consumed);
        if finished {
            return Ok(ReadLine::Line(LogicalLine {
                bytes: if oversized { Vec::new() } else { logical },
                hash: hasher.finalize().to_hex().to_string(),
                oversized,
            }));
        }
    }
}

fn push_byte(logical: &mut Vec<u8>, hasher: &mut Hasher, oversized: &mut bool, b: u8) {
    hasher.update(&[b]);
    if *oversized {
        return;
    }
    if logical.len() >= LINE_CAPTURE {
        *oversized = true;
        logical.clear();
        return;
    }
    logical.push(b);
}

fn hash_file(path: &Path) -> Result<(String, u64)> {
    let file = File::open(path).with_context(|| format!("open {}", path.display()))?;
    let mut reader = BufReader::new(file);
    let mut hasher = Hasher::new();
    let mut buf = [0u8; 64 * 1024];
    let mut total = 0u64;
    loop {
        let n = reader.read(&mut buf)?;
        if n == 0 {
            break;
        }
        if total.saturating_add(n as u64) > HASH_READ_CAP {
            return Ok(("overflow".into(), total));
        }
        hasher.update(&buf[..n]);
        total += n as u64;
    }
    Ok((hasher.finalize().to_hex().to_string(), total))
}

fn safe_join(root: &Path, rel: &str) -> Result<PathBuf> {
    if rel.is_empty() || rel.starts_with('/') || rel.contains('\0') {
        bail!("unsafe source path");
    }
    let mut out = root.to_path_buf();
    for comp in Path::new(rel).components() {
        match comp {
            Component::Normal(part) => {
                if part == ".git" || part == ".checkweave" || part == ".." {
                    bail!("unsafe source path");
                }
                out.push(part);
            }
            _ => bail!("unsafe source path"),
        }
    }
    Ok(out)
}

fn rel_under(root: &Path, path: &Path) -> Option<String> {
    let rel = path.strip_prefix(root).ok()?;
    let mut parts = Vec::new();
    for comp in rel.components() {
        match comp {
            Component::Normal(part) => {
                let part = part.to_str()?;
                if part == ".git" || part == ".checkweave" || part == ".." {
                    return None;
                }
                parts.push(part);
            }
            _ => return None,
        }
    }
    if parts.is_empty() {
        None
    } else {
        Some(parts.join("/"))
    }
}

fn open_cache(path: &Path) -> Result<Connection> {
    if path.exists() {
        match open_configured(path) {
            Ok(conn) => match schema_status(&conn)? {
                SchemaStatus::Ready => return Ok(conn),
                SchemaStatus::Empty => {
                    init_schema(&conn)?;
                    return Ok(conn);
                }
                SchemaStatus::Bad => {
                    drop(conn);
                    quarantine(path)?;
                }
            },
            Err(err) => {
                if sqlite_corruption(&err) {
                    quarantine(path)
                        .with_context(|| format!("quarantine corrupt sqlite ({err:#})"))?;
                } else {
                    return Err(err);
                }
            }
        }
    }
    let conn = open_configured(path)?;
    init_schema(&conn)?;
    Ok(conn)
}

fn open_configured(path: &Path) -> Result<Connection> {
    let conn = Connection::open(path).with_context(|| format!("open sqlite {}", path.display()))?;
    let _ = conn.pragma_update(None, "auto_vacuum", "INCREMENTAL");
    conn.busy_timeout(Duration::from_secs(5))
        .context("sqlite busy_timeout")?;
    conn.pragma_update(None, "foreign_keys", "ON")
        .context("sqlite foreign_keys")?;
    conn.pragma_update(None, "journal_mode", "WAL")
        .with_context(|| format!("sqlite journal_mode=WAL {}", path.display()))?;
    conn.pragma_update(None, "synchronous", "NORMAL")
        .context("sqlite synchronous")?;
    let status: String = conn
        .query_row("PRAGMA integrity_check", (), |row| row.get(0))
        .with_context(|| format!("sqlite integrity_check {}", path.display()))?;
    if status != "ok" {
        bail!(
            "sqlite integrity check failed for {}: {status}",
            path.display()
        );
    }
    Ok(conn)
}

fn schema_status(conn: &Connection) -> Result<SchemaStatus> {
    let has_meta: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'meta'",
            (),
            |r| r.get(0),
        )
        .context("sqlite schema")?;
    if has_meta == 0 {
        return Ok(SchemaStatus::Empty);
    }
    let version: Option<String> = conn
        .query_row(
            "SELECT value FROM meta WHERE key = 'schema_version'",
            (),
            |r| r.get(0),
        )
        .optional()
        .context("sqlite schema version")?;
    let has_items: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'items'",
            (),
            |r| r.get(0),
        )
        .context("sqlite schema")?;
    let has_reports: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'reports'",
            (),
            |r| r.get(0),
        )
        .context("sqlite schema")?;
    if version.as_deref() == Some("1") && has_items == 1 && has_reports == 1 {
        Ok(SchemaStatus::Ready)
    } else {
        Ok(SchemaStatus::Bad)
    }
}

fn init_schema(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS meta (
            key TEXT PRIMARY KEY NOT NULL,
            value TEXT NOT NULL
        );
        CREATE TABLE IF NOT EXISTS items (
            cache_key TEXT PRIMARY KEY NOT NULL,
            matched INTEGER,
            reason TEXT,
            value_json TEXT,
            payload_bytes INTEGER NOT NULL,
            created_at INTEGER NOT NULL
        );
        CREATE TABLE IF NOT EXISTS reports (
            id TEXT PRIMARY KEY NOT NULL,
            request_json TEXT NOT NULL,
            report_json TEXT NOT NULL,
            execution TEXT NOT NULL,
            freshness TEXT NOT NULL,
            payload_bytes INTEGER NOT NULL,
            created_at INTEGER NOT NULL
        );
        CREATE TABLE IF NOT EXISTS report_sources (
            report_id TEXT NOT NULL,
            path TEXT NOT NULL,
            fingerprint TEXT NOT NULL,
            bytes INTEGER NOT NULL,
            PRIMARY KEY (report_id, path),
            FOREIGN KEY (report_id) REFERENCES reports(id) ON DELETE CASCADE
        );
        CREATE INDEX IF NOT EXISTS idx_items_created ON items(created_at);
        CREATE INDEX IF NOT EXISTS idx_reports_created ON reports(created_at);",
    )
    .context("sqlite init schema")?;
    conn.execute(
        "INSERT INTO meta (key, value) VALUES ('schema_version', '1')
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        (),
    )
    .context("sqlite schema version")?;
    Ok(())
}

fn load_tick(conn: &Connection) -> Result<i64> {
    let items: i64 = conn
        .query_row("SELECT COALESCE(MAX(created_at), 0) FROM items", (), |r| {
            r.get(0)
        })
        .context("sqlite item tick")?;
    let reports: i64 = conn
        .query_row(
            "SELECT COALESCE(MAX(created_at), 0) FROM reports",
            (),
            |r| r.get(0),
        )
        .context("sqlite report tick")?;
    Ok(items.max(reports))
}

fn quarantine(path: &Path) -> Result<()> {
    if !path.exists() {
        return Ok(());
    }
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let dest = path.with_file_name(format!("cache.sqlite.corrupt-{stamp}"));
    fs::rename(path, &dest).with_context(|| format!("quarantine {}", path.display()))?;
    for suffix in ["-wal", "-shm"] {
        let side = PathBuf::from(format!("{}{suffix}", path.display()));
        if side.exists() {
            let moved = PathBuf::from(format!("{}{suffix}", dest.display()));
            fs::rename(&side, &moved).with_context(|| format!("quarantine {}", side.display()))?;
        }
    }
    Ok(())
}
