use std::{
    path::{Path, PathBuf},
    process::Command,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use checkweave::trace::{
    MAX_CAPTURED_EVENT_BYTES, MAX_SOURCE_FILE_BYTES, MAX_SOURCE_FILES, TRACE_OPERATOR_VERSION,
    TRACE_RESPONSE_MAX, TraceLimits, TraceReport, TraceRequest, evidence, evidence_page, replay,
    trace,
};
use serde_json::{Value, json};
use tempfile::TempDir;

fn limits(
    timeout_ms: u64,
    max_events: usize,
    max_value_bytes: usize,
    max_output_bytes: usize,
) -> TraceLimits {
    TraceLimits {
        timeout_ms,
        max_events,
        max_value_bytes,
        max_output_bytes,
    }
}

fn request(script: &str, input: Value) -> TraceRequest {
    let mut request = TraceRequest::new(script, input);
    request.limits = limits(5_000, 2_000, 4_096, 65_536);
    request
}

fn write_script(root: &Path, name: &str, body: &str) {
    std::fs::write(root.join(name), body).unwrap();
}

fn cancel_flag() -> Arc<AtomicBool> {
    Arc::new(AtomicBool::new(false))
}

async fn env_lock() -> tokio::sync::MutexGuard<'static, ()> {
    static LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
    LOCK.lock().await
}

struct EnvVar {
    key: &'static str,
    prev: Option<String>,
}
impl EnvVar {
    fn set(key: &'static str, value: &str) -> Self {
        let prev = std::env::var(key).ok();
        unsafe { std::env::set_var(key, value) };
        Self { key, prev }
    }
    fn unset(key: &'static str) -> Self {
        let prev = std::env::var(key).ok();
        unsafe { std::env::remove_var(key) };
        Self { key, prev }
    }
}
impl Drop for EnvVar {
    fn drop(&mut self) {
        match &self.prev {
            Some(value) => unsafe { std::env::set_var(self.key, value) },
            None => unsafe { std::env::remove_var(self.key) },
        }
    }
}

fn snapshot(root: &Path, id: &str, script: &str) -> PathBuf {
    root.join(".checkweave/traces")
        .join(id)
        .join("snapshot")
        .join(script)
}

fn sha256_hex(bytes: &[u8]) -> String {
    let mut child = Command::new("python3")
        .args([
            "-c",
            "import hashlib,sys; print(hashlib.sha256(sys.stdin.buffer.read()).hexdigest())",
        ])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    use std::io::Write;
    child.stdin.take().unwrap().write_all(bytes).unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(output.status.success());
    String::from_utf8(output.stdout).unwrap().trim().to_string()
}

fn line_hash(source: &str, line: usize) -> String {
    let lines: Vec<&str> = source.split_inclusive('\n').collect();
    sha256_hex(lines[line - 1].as_bytes())
}

fn locals_named<'a>(report: &'a TraceReport, function: &str, name: &str) -> Vec<&'a Value> {
    report
        .events
        .iter()
        .filter(|event| event.function == function)
        .filter_map(|event| event.locals.get(name))
        .collect()
}

const DISCOUNT: &str = concat!(
    "import json, sys\n",
    "data = json.load(sys.stdin)\n",
    "def apply_discount(price, percent):\n",
    "    intermediate = price - percent  # bug-marker\n",
    "    return intermediate\n",
    "price = data[\"price\"]\n",
    "percent = data[\"percent\"]\n",
    "result = apply_discount(price, percent)\n",
);

#[tokio::test]
async fn wrong_intermediate_is_tied_to_the_executed_line() {
    let _env = env_lock().await;
    let dir = TempDir::new().unwrap();
    write_script(dir.path(), "discount.py", DISCOUNT);
    let report = trace(
        dir.path(),
        &request("discount.py", json!({"price": 80, "percent": 20})),
        cancel_flag(),
    )
    .await
    .unwrap();
    assert_eq!(report.execution, "complete");
    assert_eq!(report.basis, "direct_observation");
    assert_eq!(report.freshness, "validated");
    assert!(!report.modified_during_run);
    assert!(report.elapsed_ms > 0);
    assert!(report.overhead_us.is_none());
    assert!(report.unsupported.iter().any(|item| item == "native_calls"));
    assert!(
        report
            .unsupported
            .iter()
            .any(|item| item == "event_order_is_not_causal")
    );
    assert!(report.containment.contains("not a sandbox"));
    let values = locals_named(&report, "apply_discount", "intermediate");
    assert!(
        values.iter().any(|value| *value == &json!(60)),
        "{values:?}"
    );
    let line_no = DISCOUNT
        .lines()
        .position(|line| line.contains("bug-marker"))
        .unwrap()
        + 1;
    let expected = line_hash(DISCOUNT, line_no);
    let tied = report.events.iter().any(|event| {
        event.path == "discount.py"
            && event.function == "apply_discount"
            && event.line == line_no as u32
            && event.line_hash == expected
            && event.code_hash.len() == 64
    });
    assert!(
        tied,
        "missing source-linked line event: {:?}",
        report.events
    );
    assert!(
        report
            .events
            .iter()
            .all(|event| event.path == "discount.py")
    );
    let retained =
        std::fs::read_to_string(snapshot(dir.path(), &report.id, "discount.py")).unwrap();
    assert_eq!(retained, DISCOUNT);
    let again = evidence(dir.path(), &report.id).await.unwrap().unwrap();
    assert_eq!(again.freshness, "validated");
    assert_eq!(again.events.len(), report.events.len());
}

#[tokio::test]
async fn edit_marks_evidence_stale_and_replay_uses_snapshot() {
    let _env = env_lock().await;
    let dir = TempDir::new().unwrap();
    write_script(dir.path(), "discount.py", DISCOUNT);
    let report = trace(
        dir.path(),
        &request("discount.py", json!({"price": 80, "percent": 20})),
        cancel_flag(),
    )
    .await
    .unwrap();
    let edited = DISCOUNT.replace("price - percent", "price + percent");
    write_script(dir.path(), "discount.py", &edited);
    let stale = evidence(dir.path(), &report.id).await.unwrap().unwrap();
    assert_eq!(stale.freshness, "stale");
    assert!(!stale.modified_during_run);
    let replayed = replay(dir.path(), &report.id, cancel_flag()).await.unwrap();
    assert_eq!(replayed.replay_of.as_deref(), Some(report.id.as_str()));
    assert_eq!(replayed.basis, "direct_observation");
    let values = locals_named(&replayed, "apply_discount", "intermediate");
    assert!(
        values.iter().any(|value| *value == &json!(60)),
        "{values:?}"
    );
    assert_eq!(
        std::fs::read_to_string(snapshot(dir.path(), &replayed.id, "discount.py")).unwrap(),
        DISCOUNT
    );
    let current = trace(
        dir.path(),
        &request("discount.py", json!({"price": 80, "percent": 20})),
        cancel_flag(),
    )
    .await
    .unwrap();
    let values = locals_named(&current, "apply_discount", "intermediate");
    assert!(
        values.iter().any(|value| *value == &json!(100)),
        "{values:?}"
    );
}

#[tokio::test]
async fn source_modified_during_run_is_never_validated() {
    let _env = env_lock().await;
    let dir = TempDir::new().unwrap();
    let body = concat!(
        "import json, sys, pathlib\n",
        "json.load(sys.stdin)\n",
        "def mutate():\n",
        "    path = pathlib.Path(__file__)\n",
        "    path.write_bytes(path.read_bytes() + b\"\\n# changed during run\\n\")\n",
        "    return 1\n",
        "mutate()\n",
    );
    write_script(dir.path(), "mutate.py", body);
    let original = std::fs::read(dir.path().join("mutate.py")).unwrap();
    let report = trace(dir.path(), &request("mutate.py", json!({})), cancel_flag())
        .await
        .unwrap();
    assert!(report.modified_during_run, "{:?}", report.warnings);
    assert_eq!(report.freshness, "stale");
    std::fs::write(dir.path().join("mutate.py"), original).unwrap();
    let reread = evidence(dir.path(), &report.id).await.unwrap().unwrap();
    assert!(reread.modified_during_run);
    assert_eq!(reread.freshness, "stale");
}

#[tokio::test]
async fn event_limit_drops_and_marks_partial() {
    let _env = env_lock().await;
    let dir = TempDir::new().unwrap();
    let body = concat!(
        "import json, sys\n",
        "json.load(sys.stdin)\n",
        "def spin():\n",
        "    total = 0\n",
        "    for i in range(40):\n",
        "        total = total + i\n",
        "    return total\n",
        "spin()\n",
    );
    write_script(dir.path(), "spin.py", body);
    let mut request = request("spin.py", json!({}));
    request.limits.max_events = 4;
    let report = trace(dir.path(), &request, cancel_flag()).await.unwrap();
    assert!(report.events.len() <= 4, "{}", report.events.len());
    assert!(report.dropped > 0, "dropped {}", report.dropped);
    assert_eq!(report.execution, "partial");
    let page = evidence_page(dir.path(), &report.id, 1, 2)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(page.event_offset, 1);
    assert_eq!(
        page.events.len(),
        2.min(report.events.len().saturating_sub(1))
    );
    assert_eq!(page.event_total, report.events.len());
}

#[tokio::test]
async fn output_budget_is_separate_from_events() {
    let _env = env_lock().await;
    let dir = TempDir::new().unwrap();
    let body = concat!(
        "import json, sys\n",
        "json.load(sys.stdin)\n",
        "print(\"Y\" * 4000)\n",
        "def touched():\n",
        "    seen = 1\n",
        "    return seen\n",
        "touched()\n",
    );
    write_script(dir.path(), "printy.py", body);
    let mut request = request("printy.py", json!({}));
    request.limits.max_output_bytes = 128;
    let report = trace(dir.path(), &request, cancel_flag()).await.unwrap();
    assert!(report.stdout_truncated);
    assert!(report.stdout.len() <= 128);
    assert!(!report.stdout.contains("checkweave_trace"));
    assert_eq!(report.execution, "partial");
    assert!(
        locals_named(&report, "touched", "seen")
            .iter()
            .any(|value| *value == &json!(1))
    );
}

#[tokio::test]
async fn timeout_cancels_the_process_group() {
    let _env = env_lock().await;
    let dir = TempDir::new().unwrap();
    let body = concat!(
        "import json, sys, time\n",
        "json.load(sys.stdin)\n",
        "time.sleep(30)\n"
    );
    write_script(dir.path(), "sleep.py", body);
    let mut request = request("sleep.py", json!({}));
    request.limits.timeout_ms = 800;
    let started = std::time::Instant::now();
    let report = trace(dir.path(), &request, cancel_flag()).await.unwrap();
    assert_eq!(report.execution, "timeout");
    assert!(
        started.elapsed() < Duration::from_secs(8),
        "timeout did not stop the process"
    );
    assert!(report.elapsed_ms >= 400, "{}", report.elapsed_ms);
}

#[tokio::test]
async fn cancel_before_start_and_during_run() {
    let _env = env_lock().await;
    let dir = TempDir::new().unwrap();
    let body = concat!(
        "import json, sys, time\n",
        "json.load(sys.stdin)\n",
        "time.sleep(30)\n"
    );
    write_script(dir.path(), "sleep.py", body);
    let mut request = request("sleep.py", json!({}));
    request.limits.timeout_ms = 8_000;
    let early = Arc::new(AtomicBool::new(true));
    match trace(dir.path(), &request, early).await {
        Ok(report) => assert_eq!(report.execution, "cancelled"),
        Err(err) => assert!(err.to_string().contains("cancelled"), "{err}"),
    }

    let flag = Arc::new(AtomicBool::new(false));
    let later = flag.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(200)).await;
        later.store(true, Ordering::Release);
    });
    let started = std::time::Instant::now();
    match trace(dir.path(), &request, flag).await {
        Ok(report) => assert_eq!(report.execution, "cancelled"),
        Err(err) => assert!(err.to_string().contains("cancelled"), "{err}"),
    }
    assert!(started.elapsed() < Duration::from_secs(7));
}

#[tokio::test]
async fn unrepresentable_values_do_not_call_repr() {
    let _env = env_lock().await;
    let dir = TempDir::new().unwrap();
    let body = concat!(
        "import json, sys\n",
        "json.load(sys.stdin)\n",
        "class Box:\n",
        "    def __repr__(self):\n",
        "        print(\"REPR_CALLED\")\n",
        "        return \"box\"\n",
        "    def __str__(self):\n",
        "        print(\"STR_CALLED\")\n",
        "        return \"box\"\n",
        "def hold(flag):\n",
        "    item = Box()\n",
        "    return flag\n",
        "hold(True)\n",
    );
    write_script(dir.path(), "box.py", body);
    let report = trace(dir.path(), &request("box.py", json!({})), cancel_flag())
        .await
        .unwrap();
    assert_eq!(
        report.execution, "complete",
        "{:?} {}",
        report.warnings, report.stderr
    );
    assert!(!report.stdout.contains("REPR_CALLED"), "{}", report.stdout);
    assert!(!report.stdout.contains("STR_CALLED"), "{}", report.stdout);
    let items = locals_named(&report, "hold", "item");
    assert!(!items.is_empty(), "{:?}", report.events);
    assert!(
        items
            .iter()
            .all(|value| value.get("unrepresentable") == Some(&json!(true)))
    );
    assert!(
        items
            .iter()
            .any(|value| value.get("type") == Some(&json!("Box")))
    );
}

#[tokio::test]
async fn protocol_stdout_and_events_stay_separate() {
    let _env = env_lock().await;
    let dir = TempDir::new().unwrap();
    let body = concat!(
        "import json, sys\n",
        "data = json.load(sys.stdin)\n",
        "print(\"USER_STDOUT_MARKER\")\n",
        "def step(value):\n",
        "    nxt = value + 1\n",
        "    return nxt\n",
        "print(step(data[\"k\"]))\n",
    );
    write_script(dir.path(), "proto.py", body);
    let report = trace(
        dir.path(),
        &request("proto.py", json!({"k": 4})),
        cancel_flag(),
    )
    .await
    .unwrap();
    assert!(
        report.stdout.contains("USER_STDOUT_MARKER"),
        "{}",
        report.stdout
    );
    assert!(report.stdout.contains('5'), "{}", report.stdout);
    assert!(!report.stdout.contains("checkweave_trace"));
    assert!(!report.stderr.contains("USER_STDOUT_MARKER"));
    let encoded = serde_json::to_string(&report.events).unwrap();
    assert!(!encoded.contains("USER_STDOUT_MARKER"));
    assert!(!encoded.contains("checkweave_trace"));
    let values = locals_named(&report, "step", "nxt");
    assert!(values.iter().any(|value| *value == &json!(5)), "{values:?}");
    let events_file = std::fs::read(
        dir.path()
            .join(".checkweave/traces")
            .join(&report.id)
            .join("events.jsonl"),
    )
    .unwrap();
    let stdout_file = std::fs::read(
        dir.path()
            .join(".checkweave/traces")
            .join(&report.id)
            .join("stdout.txt"),
    )
    .unwrap();
    assert_ne!(events_file, stdout_file);
    assert!(!String::from_utf8_lossy(&events_file).contains("USER_STDOUT_MARKER"));
}

#[tokio::test]
async fn baseline_overhead_is_measured_only_when_requested() {
    let _env = env_lock().await;
    let dir = TempDir::new().unwrap();
    let body = concat!(
        "import json, sys\n",
        "json.load(sys.stdin)\n",
        "def work():\n",
        "    total = 0\n",
        "    for i in range(8000):\n",
        "        total += i\n",
        "    return total\n",
        "work()\n",
    );
    write_script(dir.path(), "work.py", body);
    let mut request = request("work.py", json!({}));
    request.baseline = true;
    request.limits.timeout_ms = 30_000;
    request.limits.max_events = 100_000;
    let report = trace(dir.path(), &request, cancel_flag()).await.unwrap();
    assert_eq!(
        report.execution, "complete",
        "{:?} {}",
        report.warnings, report.stderr
    );
    let baseline = report.baseline_us.expect("baseline");
    let traced = report.traced_us.expect("traced");
    assert!(baseline > 0, "{baseline}");
    assert!(traced > 0, "{traced}");
    assert_eq!(report.overhead_us, Some(traced as i64 - baseline as i64));
    assert_eq!(report.baseline_ms, Some(baseline / 1000));
    assert_eq!(
        report.overhead_ms,
        Some((traced as i64 - baseline as i64) / 1000)
    );
    assert!(
        report
            .warnings
            .iter()
            .any(|warning| warning.contains("baseline executed"))
    );
}

#[tokio::test]
async fn function_filter_and_internal_paths_are_excluded() {
    let _env = env_lock().await;
    let dir = TempDir::new().unwrap();
    std::fs::create_dir_all(dir.path().join(".checkweave")).unwrap();
    write_script(
        dir.path().join(".checkweave").as_path(),
        "secret.py",
        "def secret():\n    hidden = 123\n    return hidden\n",
    );
    let body = concat!(
        "import importlib.util, json, sys\n",
        "json.load(sys.stdin)\n",
        "def keep(value):\n",
        "    kept = value + 1\n",
        "    return kept\n",
        "def drop(value):\n",
        "    dropped = value + 2\n",
        "    return dropped\n",
        "keep(1)\n",
        "drop(1)\n",
        "spec = importlib.util.spec_from_file_location(\"secretmod\", \".checkweave/secret.py\")\n",
        "module = importlib.util.module_from_spec(spec)\n",
        "spec.loader.exec_module(module)\n",
        "module.secret()\n",
    );
    write_script(dir.path(), "filtered.py", body);
    let mut request = request("filtered.py", json!({}));
    request.functions = vec!["keep".into()];
    let report = trace(dir.path(), &request, cancel_flag()).await.unwrap();
    assert!(report.events.iter().any(|event| event.function == "keep"));
    assert!(report.events.iter().all(|event| event.function == "keep"));
    assert!(
        locals_named(&report, "keep", "kept")
            .iter()
            .any(|value| *value == &json!(2))
    );
    let encoded = serde_json::to_string(&report.events).unwrap();
    assert!(!encoded.contains("dropped"));
    assert!(!encoded.contains("hidden"));
    assert!(
        report
            .events
            .iter()
            .all(|event| !event.path.contains(".checkweave") && !event.path.contains(".git"))
    );
}

#[tokio::test]
async fn missing_evidence_is_absent() {
    let _env = env_lock().await;
    let dir = TempDir::new().unwrap();
    let missing = evidence(dir.path(), "missingtraceid").await.unwrap();
    assert!(missing.is_none());
    let error = evidence(dir.path(), "../escape").await.unwrap_err();
    assert!(error.to_string().contains("invalid trace id"));
}

#[tokio::test]
async fn relocated_helper_is_embedded_and_override_is_explicit() {
    let _lock = env_lock().await;
    let cache = TempDir::new().unwrap();
    let _cache_env = EnvVar::set("CHECKWEAVE_CACHE_DIR", cache.path().to_str().unwrap());
    let _helper_env = EnvVar::unset("CHECKWEAVE_TRACE_HELPER");
    let dir = TempDir::new().unwrap();
    write_script(
        dir.path(),
        "tiny.py",
        "import json, sys\njson.load(sys.stdin)\ndef marker():\n    seen = 1\n    return seen\nmarker()\n",
    );
    let report = trace(dir.path(), &request("tiny.py", json!({})), cancel_flag())
        .await
        .unwrap();
    assert_eq!(
        report.execution, "complete",
        "{:?} {}",
        report.warnings, report.stderr
    );
    let helper = cache
        .path()
        .join("trace-helper")
        .join(TRACE_OPERATOR_VERSION)
        .join("checkweave_trace.py");
    let helper = helper.canonicalize().unwrap();
    let source = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("python/checkweave_trace.py")
        .canonicalize()
        .unwrap();
    assert_ne!(helper, source);
    let embedded = std::fs::read(&helper).unwrap();
    assert!(embedded.windows(12).any(|window| window == b"sys.settrace"));
    let relocated = cache.path().join("relocated-helper.py");
    std::fs::write(&relocated, &embedded).unwrap();
    let bogus = cache.path().join("bogus.py");
    std::fs::write(&bogus, "import sys\nsys.exit(2)\n").unwrap();
    let _override_bad = EnvVar::set("CHECKWEAVE_TRACE_HELPER", bogus.to_str().unwrap());
    let failed = trace(dir.path(), &request("tiny.py", json!({})), cancel_flag())
        .await
        .unwrap();
    assert_ne!(
        failed.execution, "complete",
        "explicit helper override was ignored"
    );
    drop(_override_bad);
    let _override_ok = EnvVar::set("CHECKWEAVE_TRACE_HELPER", relocated.to_str().unwrap());
    let again = trace(dir.path(), &request("tiny.py", json!({})), cancel_flag())
        .await
        .unwrap();
    assert_eq!(
        again.execution, "complete",
        "{:?} {}",
        again.warnings, again.stderr
    );
}

#[tokio::test]
async fn imported_sources_stop_at_file_and_count_caps() {
    let _env = env_lock().await;
    let dir = TempDir::new().unwrap();
    let mut big = Vec::new();
    big.extend_from_slice(b"PAD = \"");
    big.extend(std::iter::repeat_n(
        b'a',
        MAX_SOURCE_FILE_BYTES as usize + 64,
    ));
    big.extend_from_slice(b"\"\ndef value():\n    return 1\n");
    std::fs::write(dir.path().join("bigmod.py"), &big).unwrap();
    for index in 0..40 {
        write_script(
            dir.path(),
            &format!("small_{index}.py"),
            "def n():\n    return 1\n",
        );
    }
    let mut body = String::from(
        "import json, sys, importlib\njson.load(sys.stdin)\ntry:\n    import bigmod\nexcept Exception:\n    bigmod = None\n",
    );
    for index in 0..40 {
        body.push_str(&format!("importlib.import_module('small_{index}')\n"));
    }
    body.push_str("marker = 1 if bigmod is None else 0\n");
    write_script(dir.path(), "main.py", &body);
    let mut request = request("main.py", json!({}));
    request.limits.timeout_ms = 60_000;
    let report = trace(dir.path(), &request, cancel_flag()).await.unwrap();
    assert_eq!(
        report.execution, "partial",
        "{:?} {}",
        report.warnings, report.stderr
    );
    assert!(report.sources_dropped > 0, "{:?}", report.warnings);
    assert!(report.sources.len() <= MAX_SOURCE_FILES);
    assert!(
        report
            .sources
            .iter()
            .all(|source| source.bytes <= MAX_SOURCE_FILE_BYTES)
    );
    assert!(
        report
            .sources
            .iter()
            .all(|source| source.path != "bigmod.py")
    );
    let snapshot = dir
        .path()
        .join(".checkweave/traces")
        .join(&report.id)
        .join("snapshot");
    assert!(!snapshot.join("bigmod.py").exists());
    let mut retained = 0u64;
    for source in &report.sources {
        retained += source.bytes;
    }
    assert!(retained <= MAX_SOURCE_FILE_BYTES);
}

#[tokio::test]
async fn event_bytes_cap_keeps_the_report_inside_ipc() {
    let _env = env_lock().await;
    let dir = TempDir::new().unwrap();
    let body = concat!(
        "import json, sys\n",
        "json.load(sys.stdin)\n",
        "def bulk():\n",
        "    blob = 'x' * 12000\n",
        "    total = 0\n",
        "    for i in range(900):\n",
        "        total = total + i\n",
        "    return total\n",
        "bulk()\n",
    );
    write_script(dir.path(), "bulk.py", body);
    let mut request = request("bulk.py", json!({}));
    request.limits.timeout_ms = 60_000;
    request.limits.max_events = 100_000;
    request.limits.max_value_bytes = 16_000;
    let report = trace(dir.path(), &request, cancel_flag()).await.unwrap();
    assert_eq!(report.execution, "partial", "{:?}", report.warnings);
    assert!(report.dropped > 0);
    assert!(report.events.len() <= report.event_total);
    let encoded = serde_json::to_vec(&report).unwrap();
    assert!(encoded.len() <= TRACE_RESPONSE_MAX, "{}", encoded.len());
    let events = std::fs::metadata(
        dir.path()
            .join(".checkweave/traces")
            .join(&report.id)
            .join("events.jsonl"),
    )
    .unwrap();
    assert!(events.len() <= MAX_CAPTURED_EVENT_BYTES as u64);
    let page = evidence_page(dir.path(), &report.id, 0, 2)
        .await
        .unwrap()
        .unwrap();
    assert!(page.events.len() <= 2);
    assert!(page.event_total >= report.events.len());
    assert!(serde_json::to_vec(&page).unwrap().len() <= TRACE_RESPONSE_MAX);
}

#[tokio::test]
async fn invalid_utf8_stdout_is_bounded_without_panic() {
    let _env = env_lock().await;
    let dir = TempDir::new().unwrap();
    let body = concat!(
        "import json, sys, os\n",
        "json.load(sys.stdin)\n",
        "os.write(1, bytes([255]) * 64)\n",
        "def marker():\n",
        "    seen = 1\n",
        "    return seen\n",
        "marker()\n",
    );
    write_script(dir.path(), "raw.py", body);
    let mut request = request("raw.py", json!({}));
    request.limits.max_output_bytes = 10;
    let report = trace(dir.path(), &request, cancel_flag()).await.unwrap();
    assert!(report.stdout.len() <= 10, "{}", report.stdout.len());
    assert!(report.stdout.is_char_boundary(report.stdout.len()));
    assert!(report.stdout_truncated);
    assert!(
        locals_named(&report, "marker", "seen")
            .iter()
            .any(|value| *value == &json!(1))
    );
}

#[tokio::test]
async fn nested_script_imports_its_sibling_and_replay_keeps_that_module() {
    let _env = env_lock().await;
    let dir = TempDir::new().unwrap();
    let incident = dir.path().join("TASK/incident");
    std::fs::create_dir_all(&incident).unwrap();
    write_script(
        dir.path(),
        "service.py",
        "def origin():\n    return 'root'\n",
    );
    write_script(
        &incident,
        "service.py",
        "def origin():\n    return 'sibling'\n",
    );
    let body = concat!(
        "import json, sys\n",
        "json.load(sys.stdin)\n",
        "import service\n",
        "def marker():\n",
        "    seen = service.origin()\n",
        "    return seen\n",
        "marker()\n",
    );
    write_script(&incident, "main.py", body);
    let report = trace(
        dir.path(),
        &request("TASK/incident/main.py", json!({})),
        cancel_flag(),
    )
    .await
    .unwrap();
    assert_eq!(
        report.execution, "complete",
        "{:?} {}",
        report.warnings, report.stderr
    );
    assert!(
        locals_named(&report, "marker", "seen")
            .iter()
            .any(|value| *value == &json!("sibling"))
    );
    assert!(
        report
            .sources
            .iter()
            .any(|source| source.path == "TASK/incident/service.py")
    );
    write_script(
        &incident,
        "service.py",
        "def origin():\n    return 'edited'\n",
    );
    let replayed = replay(dir.path(), &report.id, cancel_flag()).await.unwrap();
    assert_eq!(
        replayed.execution, "complete",
        "{:?} {}",
        replayed.warnings, replayed.stderr
    );
    assert!(
        locals_named(&replayed, "marker", "seen")
            .iter()
            .any(|value| *value == &json!("sibling"))
    );
    assert_eq!(replayed.replay_of.as_deref(), Some(report.id.as_str()));
}
