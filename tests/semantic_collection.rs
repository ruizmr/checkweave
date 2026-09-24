//! Semantic collection checks against a fake local worker protocol.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use checkweave::models::{ChoiceLabel, ModelProvider, ModelQuestion};
use checkweave::semantic::{SEMANTIC_DEFAULT_TIMEOUT_MS, SemanticCheckRequest, check, evidence};
use checkweave::sources;
use checkweave::types::Limits;

fn env_lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|err| err.into_inner())
}

struct EnvSet {
    saved: Vec<(&'static str, Option<String>)>,
}

impl EnvSet {
    fn new() -> Self {
        Self { saved: Vec::new() }
    }
    fn set(mut self, key: &'static str, value: impl AsRef<str>) -> Self {
        self.saved.push((key, std::env::var(key).ok()));
        // Safety: tests hold env_lock, so no other test mutates these variables.
        unsafe { std::env::set_var(key, value.as_ref()) };
        self
    }
}

impl Drop for EnvSet {
    fn drop(&mut self) {
        for (key, value) in self.saved.drain(..).rev() {
            unsafe {
                match value {
                    Some(value) => std::env::set_var(key, value),
                    None => std::env::remove_var(key),
                }
            }
        }
    }
}

fn write(path: &Path, body: &str) {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(path, body).unwrap();
}

fn limits() -> Limits {
    Limits {
        max_results: 100,
        timeout_ms: 30_000,
        ..Limits::default()
    }
}

fn choice(id: &str, labels: &[&str]) -> ModelQuestion {
    ModelQuestion::Choice {
        id: id.into(),
        labels: labels
            .iter()
            .map(|label| ChoiceLabel {
                label: (*label).into(),
                description: "d".into(),
            })
            .collect(),
    }
}

fn semantic_request(
    globs: &[&str],
    questions: Vec<ModelQuestion>,
    limits: Limits,
    batch_size: usize,
) -> SemanticCheckRequest {
    SemanticCheckRequest {
        globs: globs.iter().map(|item| (*item).to_string()).collect(),
        text_pointer: "/text".into(),
        questions,
        limits,
        batch_size,
    }
}

fn worker_script(dir: &Path) -> PathBuf {
    let path = dir.join("fake-python.py");
    write(
        &path,
        r#"#!/usr/bin/env python3
import json, os, sys, time
PROTO = 1

def read_file(name):
    path = os.environ.get(name, "")
    if not path or not os.path.exists(path):
        return ""
    return open(path, encoding="utf-8").read().strip()

def provenance():
    device = read_file("CHECKWEAVE_FAKE_DEVICE_FILE") or "cpu"
    runtime = read_file("CHECKWEAVE_FAKE_RUNTIME_FILE") or "fake-1"
    return {
        "provider": "local",
        "model": "fastino/gliner2.5-base-v1",
        "revision": "1a8bc24e00dc7300b9017c81d63e3dcdabb26596",
        "adapter_version": "checkweave-gliner2-1",
        "backend": "fake-gliner",
        "profile": "lightweight",
        "code_revision": "fake-code",
        "device_requested": "cpu",
        "device_fallback": None,
        "device": device,
        "precision": "float32",
        "score_semantics": "gliner2_classification_schema_single",
        "input_policy": "reject_over_max_input_tokens",
        "runtime_versions": {"fake": runtime},
    }

def emit(obj):
    sys.stdout.write(json.dumps(obj) + "\n")
    sys.stdout.flush()

emit({"version": PROTO, "ready": True, "provenance": provenance()})
log_path = os.environ.get("CHECKWEAVE_FAKE_LOG", "")
for raw in sys.stdin:
    raw = raw.strip()
    if not raw:
        continue
    delay = read_file("CHECKWEAVE_FAKE_SLEEP_FILE")
    if delay:
        time.sleep(int(delay) / 1000)
    req = json.loads(raw)
    if log_path:
        with open(log_path, "a", encoding="utf-8") as handle:
            handle.write(json.dumps({
                "states": [{"id": state["id"], "text": state["text"]} for state in req["states"]],
                "questions": [question["id"] for question in req["questions"]],
            }) + "\n")
    results = []
    for state in req["states"]:
        for question in req["questions"]:
            labels = [item["label"] for item in question.get("labels", [])] or ["no"]
            label = labels[0] if "yes" in state["text"] else labels[-1]
            results.append({
                "state_id": state["id"],
                "question_id": question["id"],
                "status": "resolved",
                "label": label,
            })
    emit({"version": PROTO, "id": req["id"], "results": results, "provenance": provenance()})
"#,
    );
    let _ = Command::new("chmod").arg("+x").arg(&path).status();
    path
}

#[allow(dead_code)]
struct Harness {
    root: PathBuf,
    cache: PathBuf,
    log: PathBuf,
    device: PathBuf,
    sleep: PathBuf,
    runtime: PathBuf,
    _guard: EnvSet,
    _dir: tempfile::TempDir,
}

impl Harness {
    fn open() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("ws");
        let cache = dir.path().join("cache");
        fs::create_dir_all(&root).unwrap();
        fs::create_dir_all(&cache).unwrap();
        let log = dir.path().join("calls.jsonl");
        let device = dir.path().join("device");
        let sleep = dir.path().join("sleep");
        let runtime = dir.path().join("runtime");
        write(&device, "cpu");
        write(&runtime, "fake-1");
        write(
            &root.join("checkweave.toml"),
            "[model]\nprofile = \"lightweight\"\ndevice = \"cpu\"\nthreads = 1\n",
        );
        let script = worker_script(dir.path());
        let guard = EnvSet::new()
            .set("CHECKWEAVE_PYTHON", script.to_str().unwrap())
            .set("CHECKWEAVE_CACHE_DIR", cache.to_str().unwrap())
            .set("CHECKWEAVE_FAKE_LOG", log.to_str().unwrap())
            .set("CHECKWEAVE_FAKE_DEVICE_FILE", device.to_str().unwrap())
            .set("CHECKWEAVE_FAKE_SLEEP_FILE", sleep.to_str().unwrap())
            .set("CHECKWEAVE_FAKE_RUNTIME_FILE", runtime.to_str().unwrap());
        Self {
            root,
            cache,
            log,
            device,
            sleep,
            runtime,
            _guard: guard,
            _dir: dir,
        }
    }

    async fn provider(&self) -> ModelProvider {
        ModelProvider::new(&self.root).await.unwrap()
    }

    fn calls(&self) -> Vec<serde_json::Value> {
        if !self.log.exists() {
            return Vec::new();
        }
        fs::read_to_string(&self.log)
            .unwrap()
            .lines()
            .filter(|line| !line.is_empty())
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }

    fn clear_log(&self) {
        if self.log.exists() {
            fs::write(&self.log, "").unwrap();
        }
    }
}

#[test]
fn source_membership_skips_state_and_non_matches() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(&root.join("keep.jsonl"), "{}\n");
    write(&root.join("skip.txt"), "{}\n");
    write(&root.join(".checkweave").join("secret.jsonl"), "{}\n");
    write(&root.join(".hidden.jsonl"), "{}\n");
    let found = sources::enumerate(root, &["*.jsonl".into()]).unwrap();
    assert_eq!(found, vec!["keep.jsonl".to_string()]);
}

#[allow(clippy::await_holding_lock)] // holds the process env lock for the whole async test
#[tokio::test]
async fn changed_row_reuses_unchanged_text() {
    let _lock = env_lock();
    let harness = Harness::open();
    write(
        &harness.root.join("rows.jsonl"),
        "{\"text\":\"yes stable\"}\n{\"text\":\"yes stable\"}\n{\"text\":\"no two\"}\n",
    );
    let mut provider = harness.provider().await;
    let request = semantic_request(
        &["*.jsonl"],
        vec![choice("mood", &["yes", "no"])],
        limits(),
        8,
    );
    let cancel = Arc::new(AtomicBool::new(false));
    let cold = check(&harness.root, &request, &mut provider, Arc::clone(&cancel))
        .await
        .unwrap();
    assert_eq!(cold.basis, "model_judgment");
    assert_eq!(cold.execution, "complete");
    assert_eq!(cold.freshness, "validated");
    assert_eq!(cold.coverage.records, 3);
    assert_eq!(cold.coverage.cache_misses, 3);
    assert_eq!(cold.coverage.cache_hits, 0);
    assert!(cold.coverage.fresh_model_calls >= 1);
    assert_eq!(cold.decisions.len(), 3);
    assert!(
        cold.decisions
            .iter()
            .all(|item| item.observation == "fresh")
    );
    assert_eq!(
        cold.decisions[0].record.as_ref().unwrap()["text"],
        "yes stable"
    );
    assert_eq!(cold.decisions[0].text.as_deref(), Some("yes stable"));

    harness.clear_log();
    write(
        &harness.root.join("rows.jsonl"),
        "{\"text\":\"yes stable\"}\n{\"text\":\"yes stable\"}\n{\"text\":\"yes changed\"}\n",
    );
    let warm = check(&harness.root, &request, &mut provider, cancel)
        .await
        .unwrap();
    assert_eq!(warm.coverage.cache_hits, 2);
    assert_eq!(warm.coverage.cache_misses, 1);
    assert!(warm.coverage.fresh_model_calls >= 1);
    assert_eq!(warm.coverage.resolved, 3);
    let calls = harness.calls();
    let sent: Vec<_> = calls
        .iter()
        .flat_map(|call| call["states"].as_array().unwrap().clone())
        .collect();
    assert!(sent.iter().any(|state| state["text"] == "yes changed"));
    let reused: Vec<_> = warm
        .decisions
        .iter()
        .filter(|item| item.observation == "reused")
        .collect();
    assert_eq!(reused.len(), 2);
    assert!(
        reused
            .iter()
            .all(|item| item.text.as_deref() == Some("yes stable"))
    );
    assert!(
        warm.limitations
            .iter()
            .any(|item| item.contains("readiness_api_unavailable"))
            || warm.model.is_some()
    );
}

#[allow(clippy::await_holding_lock)]
#[tokio::test]
async fn question_and_config_changes_invalidate_only_their_cache() {
    let _lock = env_lock();
    let harness = Harness::open();
    write(
        &harness.root.join("rows.jsonl"),
        "{\"text\":\"yes one\"}\n{\"text\":\"yes one\"}\n",
    );
    write(
        &harness.root.join("checkweave.toml"),
        "[model]\nprofile = \"lightweight\"\nthreads = 1\ndevice = \"cpu\"\n",
    );
    let mut provider = harness.provider().await;
    let first = semantic_request(
        &["*.jsonl"],
        vec![
            choice("keep", &["yes", "no"]),
            choice("edit", &["yes", "no"]),
        ],
        limits(),
        8,
    );
    let cancel = Arc::new(AtomicBool::new(false));
    let _ = check(&harness.root, &first, &mut provider, Arc::clone(&cancel))
        .await
        .unwrap();
    harness.clear_log();
    let second = semantic_request(
        &["*.jsonl"],
        vec![
            choice("keep", &["yes", "no"]),
            choice("edit", &["yes", "maybe", "no"]),
        ],
        limits(),
        8,
    );
    let changed = check(&harness.root, &second, &mut provider, Arc::clone(&cancel))
        .await
        .unwrap();
    assert!(changed.coverage.cache_hits >= 1);
    assert!(changed.coverage.cache_misses >= 1);
    let calls = harness.calls();
    assert!(calls.iter().any(|call| {
        call["questions"]
            .as_array()
            .unwrap()
            .iter()
            .any(|id| id == "edit")
            && call["questions"].as_array().unwrap().len() <= 16
    }));

    drop(provider);
    write(
        &harness.root.join("checkweave.toml"),
        "[model]\nprofile = \"lightweight\"\nthreads = 2\ndevice = \"cpu\"\n",
    );
    let mut provider = harness.provider().await;
    harness.clear_log();
    let configured = check(&harness.root, &second, &mut provider, cancel)
        .await
        .unwrap();
    assert_eq!(configured.coverage.cache_hits, 0);
    assert_eq!(configured.coverage.cache_misses, 4);
    assert!(configured.coverage.fresh_model_calls >= 1);
}

#[allow(clippy::await_holding_lock)]
#[tokio::test]
async fn live_device_change_rechecks_unchanged_rows() {
    let _lock = env_lock();
    let harness = Harness::open();
    write(
        &harness.root.join("rows.jsonl"),
        "{\"text\":\"yes one\"}\n{\"text\":\"no two\"}\n",
    );
    let mut provider = harness.provider().await;
    let request = semantic_request(
        &["*.jsonl"],
        vec![choice("mood", &["yes", "no"])],
        limits(),
        8,
    );
    let cancel = Arc::new(AtomicBool::new(false));
    let _ = check(&harness.root, &request, &mut provider, Arc::clone(&cancel))
        .await
        .unwrap();
    write(&harness.device, "cuda:0");
    write(
        &harness.root.join("rows.jsonl"),
        "{\"text\":\"yes one\"}\n{\"text\":\"yes changed\"}\n",
    );
    harness.clear_log();
    let report = check(&harness.root, &request, &mut provider, cancel)
        .await
        .unwrap();
    assert_eq!(report.coverage.cache_hits, 0);
    assert_eq!(report.coverage.cache_misses, 2);
    assert!(report.coverage.fresh_model_calls >= 1);
    assert_eq!(report.model.as_ref().unwrap().device, "cuda:0");
    let states: Vec<_> = harness
        .calls()
        .iter()
        .flat_map(|call| call["states"].as_array().unwrap().clone())
        .collect();
    assert!(states.iter().any(|state| state["text"] == "yes one"));
    assert!(states.iter().any(|state| state["text"] == "yes changed"));
}

#[allow(clippy::await_holding_lock)]
#[tokio::test]
async fn batch_maps_duplicate_text_and_isolated_questions() {
    let _lock = env_lock();
    let harness = Harness::open();
    write(&harness.root.join("a.jsonl"), "{\"text\":\"yes same\"}\n");
    write(&harness.root.join("b.jsonl"), "{\"text\":\"yes same\"}\n");
    let mut provider = harness.provider().await;
    let request = semantic_request(
        &["*.jsonl"],
        vec![
            choice("left", &["yes", "no"]),
            choice("right", &["yes", "no"]),
        ],
        limits(),
        8,
    );
    let report = check(
        &harness.root,
        &request,
        &mut provider,
        Arc::new(AtomicBool::new(false)),
    )
    .await
    .unwrap();
    assert_eq!(report.decisions.len(), 4);
    let mut state_ids: Vec<_> = report
        .decisions
        .iter()
        .map(|item| item.state_id.clone())
        .collect();
    state_ids.sort();
    state_ids.dedup();
    assert_eq!(state_ids.len(), 2);
    assert!(
        report
            .decisions
            .iter()
            .any(|item| item.source.path == "a.jsonl" && item.question_id == "left")
    );
    assert!(
        report
            .decisions
            .iter()
            .any(|item| item.source.path == "b.jsonl" && item.question_id == "right")
    );
    let calls = harness.calls();
    let mut state_ids_sent = Vec::new();
    for call in &calls {
        let states = call["states"].as_array().unwrap();
        let questions = call["questions"].as_array().unwrap();
        assert!(questions.len() <= 16);
        assert!(states.len() * questions.len() <= 64);
        state_ids_sent.extend(
            states
                .iter()
                .map(|state| state["id"].as_str().unwrap().to_string()),
        );
    }
    state_ids_sent.sort();
    state_ids_sent.dedup();
    assert_eq!(state_ids_sent.len(), 2);
}

#[allow(clippy::await_holding_lock)]
#[tokio::test]
async fn timeout_cancel_evidence_and_explicit_rows() {
    let _lock = env_lock();
    let harness = Harness::open();
    write(
        &harness.root.join("rows.jsonl"),
        "\nnot-json\n{\"other\":1}\n{\"text\":1}\n{\"text\":\"yes\"}\n",
    );
    let mut provider = harness.provider().await;
    let request = semantic_request(
        &["*.jsonl"],
        vec![choice("mood", &["yes", "no"])],
        limits(),
        8,
    );
    let cancel = Arc::new(AtomicBool::new(false));
    let report = check(&harness.root, &request, &mut provider, Arc::clone(&cancel))
        .await
        .unwrap();
    assert_eq!(report.coverage.records, 4);
    assert_eq!(report.coverage.skipped, 1);
    assert_eq!(report.coverage.unresolved, 2);
    assert_eq!(report.coverage.unsupported, 1);
    assert_eq!(report.coverage.resolved, 1);
    assert!(
        report
            .decisions
            .iter()
            .any(|item| item.reason.as_deref() == Some("invalid_json") && item.record.is_some())
    );
    assert!(
        report
            .decisions
            .iter()
            .any(|item| item.reason.as_deref() == Some("missing_path"))
    );
    assert!(
        report
            .decisions
            .iter()
            .any(|item| item.reason.as_deref() == Some("non_text"))
    );
    assert!(evidence(&harness.root, "missing").unwrap().is_none());
    let stored = evidence(&harness.root, &report.id).unwrap().unwrap();
    assert_eq!(stored.freshness, "validated");
    write(&harness.root.join("rows.jsonl"), "{\"text\":\"yes\"}\n");
    let stale = evidence(&harness.root, &report.id).unwrap().unwrap();
    assert_eq!(stale.freshness, "stale");
    assert_eq!(stale.execution, "complete");

    write(&harness.sleep, "5000");
    write(
        &harness.root.join("rows.jsonl"),
        "{\"text\":\"yes later\"}\n",
    );
    let mut tight = limits();
    tight.timeout_ms = 1_500;
    let timed_request =
        semantic_request(&["*.jsonl"], vec![choice("mood", &["yes", "no"])], tight, 8);
    let timed = check(
        &harness.root,
        &timed_request,
        &mut provider,
        Arc::clone(&cancel),
    )
    .await
    .unwrap();
    assert_eq!(timed.execution, "partial");
    assert_ne!(timed.execution, "complete");
    write(&harness.sleep, "");

    cancel.store(true, Ordering::Relaxed);
    let cancelled = check(&harness.root, &request, &mut provider, Arc::clone(&cancel))
        .await
        .unwrap();
    assert_eq!(cancelled.execution, "cancelled");
    assert_ne!(cancelled.execution, "complete");
}

#[allow(clippy::await_holding_lock)]
#[tokio::test]
async fn over_limit_is_not_sent_and_budgets_are_global() {
    let _lock = env_lock();
    let harness = Harness::open();
    write(
        &harness.root.join("checkweave.toml"),
        "[model]\nprofile = \"lightweight\"\ndevice = \"cpu\"\nthreads = 1\n",
    );
    let huge = "x".repeat(checkweave::semantic::MAX_SERIALIZED_TEXT_BYTES + 8);
    write(
        &harness.root.join("rows.jsonl"),
        &format!("{{\"text\":{huge:?}}}\n{{\"text\":\"zz\"}}\n"),
    );
    let mut provider = harness.provider().await;
    let mut narrow = limits();
    narrow.max_results = 1;
    let request = semantic_request(
        &["*.jsonl"],
        vec![choice("mood", &["yes", "no"])],
        narrow,
        8,
    );
    let report = check(
        &harness.root,
        &request,
        &mut provider,
        Arc::new(AtomicBool::new(false)),
    )
    .await
    .unwrap();
    assert_eq!(report.coverage.records, 2);
    assert_eq!(report.coverage.unsupported, 1);
    assert_eq!(report.decisions.len(), 1);
    assert!(report.truncated);
    assert!(
        report.decisions.iter().any(|item| {
            item.reason
                .as_deref()
                .unwrap_or("")
                .contains("over_serialized_byte_bound")
        }) || report.coverage.unsupported == 1
    );
    let sent = harness
        .calls()
        .iter()
        .flat_map(|call| call["states"].as_array().unwrap().clone())
        .count();
    assert_eq!(sent, 1);
    assert!(
        harness.calls()[0]["states"][0]["text"]
            .as_str()
            .unwrap()
            .len()
            <= 8
    );

    write(
        &harness.root.join("a.jsonl"),
        "{\"text\":\"qa\"}\n{\"text\":\"qb\"}\n",
    );
    write(
        &harness.root.join("b.jsonl"),
        "{\"text\":\"qc\"}\n{\"text\":\"qd\"}\n",
    );
    write(
        &harness.root.join("c.jsonl"),
        "{\"text\":\"qe\"}\n{\"text\":\"qf\"}\n",
    );
    fs::remove_file(harness.root.join("rows.jsonl")).unwrap();
    harness.clear_log();
    let mut capped = limits();
    capped.max_records = 3;
    capped.max_results = 100;
    let capped_request = semantic_request(
        &["*.jsonl"],
        vec![choice("mood", &["yes", "no"])],
        capped,
        1,
    );
    let partial = check(
        &harness.root,
        &capped_request,
        &mut provider,
        Arc::new(AtomicBool::new(false)),
    )
    .await
    .unwrap();
    assert_eq!(partial.coverage.records, 3);
    assert_eq!(partial.execution, "partial");
    assert!(partial.coverage.files < 3);
    let sent = harness
        .calls()
        .iter()
        .flat_map(|call| call["states"].as_array().unwrap().clone())
        .count();
    assert_eq!(sent, 3);
}

#[allow(clippy::await_holding_lock)]
#[tokio::test]
async fn worker_caps_split_questions_and_pairs() {
    let _lock = env_lock();
    let harness = Harness::open();
    let mut body = String::new();
    for index in 0..5 {
        body.push_str(&format!("{{\"text\":\"yes row{index}\"}}\n"));
    }
    write(&harness.root.join("rows.jsonl"), &body);
    let questions: Vec<_> = (0..17)
        .map(|index| choice(&format!("q{index}"), &["yes", "no"]))
        .collect();
    let mut provider = harness.provider().await;
    let request = semantic_request(&["*.jsonl"], questions, limits(), 32);
    let report = check(
        &harness.root,
        &request,
        &mut provider,
        Arc::new(AtomicBool::new(false)),
    )
    .await
    .unwrap();
    assert_eq!(report.coverage.records, 5);
    assert_eq!(report.coverage.resolved, 85);
    let mut pairs = 0usize;
    for call in harness.calls() {
        let states = call["states"].as_array().unwrap().len();
        let asked = call["questions"].as_array().unwrap().len();
        assert!(asked <= 16);
        assert!(states <= 32);
        assert!(states * asked <= 64);
        pairs += states * asked;
    }
    assert_eq!(pairs, 85);
}

#[tokio::test]
#[ignore = "opt-in real worker: CHECKWEAVE_SEMANTIC_SMOKE=1 cargo test --test semantic_collection -- --ignored real_worker_smoke_when_requested"]
async fn real_worker_smoke_when_requested() {
    if std::env::var("CHECKWEAVE_SEMANTIC_SMOKE").ok().as_deref() != Some("1") {
        panic!("set CHECKWEAVE_SEMANTIC_SMOKE=1 to run the real worker smoke");
    }
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(&root.join("rows.jsonl"), "{\"text\":\"yes\"}\n");
    let mut provider = ModelProvider::new(root).await.expect("provider");
    let request = semantic_request(
        &["*.jsonl"],
        vec![choice("mood", &["yes", "no"])],
        limits(),
        4,
    );
    let report = tokio::time::timeout(
        Duration::from_secs(600),
        check(
            root,
            &request,
            &mut provider,
            Arc::new(AtomicBool::new(false)),
        ),
    )
    .await
    .expect("smoke timeout")
    .expect("smoke check");
    assert_eq!(report.basis, "model_judgment");
    assert!(report.coverage.records >= 1);
}

fn question_body() -> serde_json::Value {
    serde_json::json!([{
        "kind": "predicate",
        "id": "p",
        "statement": "the text is present"
    }])
}

#[tokio::test]
async fn omitted_semantic_timeout_defaults_to_180s() {
    let absent = serde_json::json!({
        "globs": ["rows.jsonl"],
        "text_pointer": "/text",
        "questions": question_body(),
    });
    let absent: SemanticCheckRequest = serde_json::from_value(absent).unwrap();
    assert_eq!(absent.limits.timeout_ms, SEMANTIC_DEFAULT_TIMEOUT_MS);
    assert_eq!(absent.limits.max_files, Limits::default().max_files);
    assert_eq!(absent.limits.timeout_ms, 180_000);
    assert_ne!(Limits::default().timeout_ms, 180_000);

    let partial = serde_json::json!({
        "globs": ["rows.jsonl"],
        "text_pointer": "/text",
        "questions": question_body(),
        "limits": { "max_results": 0 }
    });
    let partial: SemanticCheckRequest = serde_json::from_value(partial).unwrap();
    assert_eq!(partial.limits.timeout_ms, 180_000);
    assert_eq!(partial.limits.max_results, 0);
    assert_eq!(partial.limits.max_files, Limits::default().max_files);

    let explicit = serde_json::json!({
        "globs": ["rows.jsonl"],
        "text_pointer": "/text",
        "questions": question_body(),
        "limits": { "timeout_ms": 30_000, "max_files": 4 }
    });
    let explicit: SemanticCheckRequest = serde_json::from_value(explicit).unwrap();
    assert_eq!(explicit.limits.timeout_ms, 30_000);
    assert_eq!(explicit.limits.max_files, 4);

    let schema = serde_json::to_value(schemars::schema_for!(SemanticCheckRequest)).unwrap();
    let rendered = schema.to_string();
    assert!(
        rendered.contains("180000"),
        "schema did not publish the semantic timeout default: {schema}"
    );

    let zero = serde_json::json!({
        "globs": ["rows.jsonl"],
        "text_pointer": "/text",
        "questions": question_body(),
        "limits": { "timeout_ms": 0 }
    });
    let zero: SemanticCheckRequest = serde_json::from_value(zero).unwrap();
    assert_eq!(zero.limits.timeout_ms, 0);
    let dir = tempfile::tempdir().unwrap();
    let mut provider = ModelProvider::new(dir.path()).await.expect("provider");
    let err = check(
        dir.path(),
        &zero,
        &mut provider,
        Arc::new(AtomicBool::new(false)),
    )
    .await
    .expect_err("timeout 0 must be rejected");
    assert!(err.to_string().contains("timeout_ms"), "{err}");
}
