use checkweave::compare::{
    ArraySpec, CompareBudgets, CompareOutcome, CompareRequest, GeneratedCases, IntegerShape,
    IntegerSpec, RetentionLimits, compare, replay,
};
use checkweave::execute::{ExecutionLimits, Target, fingerprint_file};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    process::Command,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
};

struct TempWork(PathBuf);
impl TempWork {
    fn new() -> Self {
        static N: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "checkweave-cmp-{}-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&path).unwrap();
        Self(path)
    }
    fn path(&self) -> &Path {
        &self.0
    }
}
impl Drop for TempWork {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn py(script: &str) -> Target {
    Target {
        argv: vec!["python3".into(), script.into()],
        cwd: ".".into(),
        env: BTreeMap::new(),
        sources: vec![script.into()],
    }
}

fn inline(code: &str) -> Target {
    Target {
        argv: vec!["python3".into(), "-c".into(), code.into()],
        cwd: ".".into(),
        env: BTreeMap::new(),
        sources: Vec::new(),
    }
}

fn request(before: Target, after: Target, inputs: Vec<Value>) -> CompareRequest {
    CompareRequest {
        before,
        after,
        inputs,
        generated: None,
        budgets: CompareBudgets {
            max_cases: 32,
            max_executions: 400,
            timeout_ms: 60_000,
        },
        per_execution: ExecutionLimits {
            timeout_ms: 5_000,
            max_output_bytes: 64 * 1024,
        },
        reduce: false,
        repeat: 2,
        before_revision: None,
        after_revision: None,
        policy: Default::default(),
        retention: RetentionLimits {
            max_entries: 8,
            max_total_bytes: 8 * 1024 * 1024,
        },
    }
}

fn cancel() -> Arc<AtomicBool> {
    Arc::new(AtomicBool::new(false))
}

fn copy_corpus(dir: &Path) {
    let src = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("examples/behavior");
    let dest = dir.join("examples/behavior");
    std::fs::create_dir_all(&dest).unwrap();
    for name in ["before.py", "equivalent.py", "different.py", "cases.json"] {
        std::fs::copy(src.join(name), dest.join(name)).unwrap();
    }
}

fn corpus_inputs(dir: &Path) -> Vec<Value> {
    let raw = std::fs::read_to_string(dir.join("examples/behavior/cases.json")).unwrap();
    serde_json::from_str(&raw).unwrap()
}

fn git(dir: &Path, args: &[&str]) {
    let status = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .env("GIT_AUTHOR_NAME", "checkweave")
        .env("GIT_AUTHOR_EMAIL", "checkweave@example.com")
        .env("GIT_COMMITTER_NAME", "checkweave")
        .env("GIT_COMMITTER_EMAIL", "checkweave@example.com")
        .status()
        .unwrap();
    assert!(status.success(), "git {args:?} failed");
}

#[tokio::test]
async fn corpus_preserving_change_is_not_a_regression() {
    let dir = TempWork::new();
    copy_corpus(dir.path());
    let mut req = request(
        py("examples/behavior/before.py"),
        py("examples/behavior/equivalent.py"),
        corpus_inputs(dir.path()),
    );
    req.reduce = true;
    let report = compare(dir.path(), &req, cancel()).await.unwrap();
    assert_eq!(report.outcome, CompareOutcome::BoundedNoDifference);
    assert_eq!(report.metrics.discovered_differences, 0);
    assert_eq!(report.metrics.incorrect_regression_claims, 0);
    assert!(report.metrics.reproducible);
    assert!(report.metrics.executions >= 20);
    assert!(report.elapsed_ms > 0);
    assert!(report.difference.is_none());
    let text = serde_json::to_string(&report).unwrap();
    assert!(!text.contains("checkweave-wt-"));
}

#[tokio::test]
async fn corpus_difference_reduces_and_replays_after_edit() {
    let dir = TempWork::new();
    copy_corpus(dir.path());
    let mut req = request(
        py("examples/behavior/before.py"),
        py("examples/behavior/different.py"),
        corpus_inputs(dir.path()),
    );
    req.reduce = true;
    let report = compare(dir.path(), &req, cancel()).await.unwrap();
    assert_eq!(
        report.outcome,
        CompareOutcome::ObservedDifference,
        "{:?}",
        report.warnings
    );
    assert!(report.metrics.discovered_differences >= 1);
    assert_eq!(report.metrics.incorrect_regression_claims, 0);
    assert!(report.metrics.reproducible);
    let diff = report.difference.as_ref().unwrap();
    assert_eq!(diff.minimality, "smallest_retained_within_budget");
    assert_eq!(diff.before.outcome, "completed");
    assert_eq!(diff.after.outcome, "completed");
    assert_ne!(diff.before.output, diff.after.output);
    let artifact = dir
        .path()
        .join(&report.reproduction.as_ref().unwrap().request_artifact);
    assert!(artifact.is_file());
    let cli: Value = serde_json::from_slice(&std::fs::read(&artifact).unwrap()).unwrap();
    assert_eq!(cli["argv"][0], "checkweave");
    assert_eq!(cli["argv"][1], "--workspace");
    assert_eq!(cli["argv"][3], "replay");
    assert_eq!(cli["argv"][4], "--kind");
    assert_eq!(cli["argv"][5], "compare");
    assert_eq!(cli["argv"][6], "--id");
    assert!(
        !serde_json::to_string(&report)
            .unwrap()
            .contains("checkweave-wt-")
    );

    let again = replay(dir.path(), &report.id, cancel()).await.unwrap();
    assert!(again.metrics.executions >= 2, "replay did not execute");
    assert!(
        again.changed_sources.is_empty(),
        "{:?}",
        again.changed_sources
    );
    assert_eq!(again.outcome, CompareOutcome::ObservedDifference);
    assert_eq!(again.outcome_reproduced, Some(true));

    let retained_input = dir
        .path()
        .join(".checkweave/behavior")
        .join(&report.id)
        .join("input.json");
    std::fs::remove_file(&retained_input).unwrap();
    let missing = replay(dir.path(), &report.id, cancel()).await.unwrap();
    assert!(
        missing
            .missing_evidence
            .iter()
            .any(|item| item.contains("input.json")),
        "{:?}",
        missing.missing_evidence
    );
    assert!(missing.metrics.executions >= 2);

    std::fs::copy(
        dir.path().join("examples/behavior/equivalent.py"),
        dir.path().join("examples/behavior/different.py"),
    )
    .unwrap();
    let edited = replay(dir.path(), &report.id, cancel()).await.unwrap();
    assert!(edited.metrics.executions >= 2);
    assert!(
        edited
            .changed_sources
            .iter()
            .any(|path| path.ends_with("different.py"))
    );
    assert_eq!(edited.outcome, CompareOutcome::BoundedNoDifference);
    assert_eq!(edited.outcome_reproduced, Some(false));
    assert_eq!(edited.metrics.incorrect_regression_claims, 0);
}

#[tokio::test]
async fn reducer_drops_irrelevant_structure_and_keeps_a_parsed_difference() {
    let dir = TempWork::new();
    let before =
        "import json,sys\nv=json.load(sys.stdin)\nprint(json.dumps({'total': sum(v['values'])}))\n";
    let after = "import json,sys\nv=json.load(sys.stdin)\nprint(json.dumps({'total': sum(n for n in v['values'] if n >= 0)}))\n";
    let input = json!({"note": "abcdefghij", "values": [1, -2, 3]});
    let mut req = request(inline(before), inline(after), vec![input.clone()]);
    req.reduce = true;
    let report = compare(dir.path(), &req, cancel()).await.unwrap();
    assert_eq!(report.outcome, CompareOutcome::ObservedDifference);
    let diff = report.difference.unwrap();
    assert!(diff.reduced, "retained {}", diff.input);
    assert!(
        serde_json::to_string(&diff.input).unwrap().len()
            < serde_json::to_string(&input).unwrap().len()
    );
    assert_eq!(diff.before.outcome, "completed");
    assert_eq!(diff.after.outcome, "completed");
    assert!(diff.before.output.is_some());
    assert_ne!(diff.before.output, diff.after.output);
    let values = diff.input.get("values").and_then(|v| v.as_array()).unwrap();
    assert!(values.iter().any(|n| n.as_i64().unwrap() < 0));
    assert!(diff.input.get("note").is_none());
}

#[tokio::test]
async fn crashes_and_parser_failures_are_unsupported() {
    let dir = TempWork::new();
    let good = inline("import json,sys\njson.dump({'n': 1}, sys.stdout)\n");
    let bad = inline("import sys\nsys.stdout.write('nope\\n')\n");
    let crash = inline("import sys\nsys.exit(2)\n");
    let text = compare(
        dir.path(),
        &request(good.clone(), bad, vec![json!({})]),
        cancel(),
    )
    .await
    .unwrap();
    assert_eq!(text.outcome, CompareOutcome::Unsupported);
    assert_eq!(text.metrics.discovered_differences, 0);
    assert_eq!(text.metrics.incorrect_regression_claims, 0);
    assert!(text.difference.is_none());

    let crashed = compare(dir.path(), &request(good, crash, vec![json!({})]), cancel())
        .await
        .unwrap();
    assert_eq!(crashed.outcome, CompareOutcome::Unsupported);
    assert_eq!(crashed.cases_unsupported, 1);
    assert_ne!(crashed.outcome, CompareOutcome::ObservedDifference);
}

#[tokio::test]
async fn unstable_stdout_is_nondeterministic() {
    let dir = TempWork::new();
    let code = "import json,os,pathlib,sys\np=pathlib.Path(os.environ['CW_COUNTER'])\nn=int(p.read_text()) if p.exists() else 0\np.write_text(str(n+1))\njson.dump({'n': n % 2}, sys.stdout)\n";
    let mut before = inline(code);
    before.env.insert(
        "CW_COUNTER".into(),
        dir.path()
            .join("before-counter")
            .to_string_lossy()
            .into_owned(),
    );
    let mut after = inline(code);
    after.env.insert(
        "CW_COUNTER".into(),
        dir.path()
            .join("after-counter")
            .to_string_lossy()
            .into_owned(),
    );
    let report = compare(
        dir.path(),
        &request(before, after, vec![json!({})]),
        cancel(),
    )
    .await
    .unwrap();
    assert_eq!(
        report.outcome,
        CompareOutcome::Nondeterministic,
        "{:?}",
        report.warnings
    );
    assert_eq!(report.metrics.discovered_differences, 0);
    assert!(!report.metrics.reproducible);
    assert!(report.dependency_completeness.contains("untracked"));
}

#[tokio::test]
async fn generated_integer_range_is_deterministic_and_shrinks() {
    let dir = TempWork::new();
    let before =
        inline("import json,sys\nn=json.load(sys.stdin)\njson.dump({'n': n}, sys.stdout)\n");
    let after = inline(
        "import json,sys\nn=json.load(sys.stdin)\njson.dump({'n': n if n < 3 else 0}, sys.stdout)\n",
    );
    let mut req = request(before, after, Vec::new());
    req.reduce = true;
    req.generated = Some(GeneratedCases {
        seed: 7,
        integers: Some(IntegerSpec {
            start: 0,
            end: 5,
            shape: IntegerShape::Number,
        }),
        arrays: None,
    });
    let report = compare(dir.path(), &req, cancel()).await.unwrap();
    assert_eq!(report.outcome, CompareOutcome::ObservedDifference);
    assert!(report.metrics.discovered_differences >= 1);
    let input = report.difference.unwrap().input;
    assert_eq!(input, json!(3));
}

#[tokio::test]
async fn budgets_and_cancellation_are_explicit() {
    let dir = TempWork::new();
    let program = inline("import json,sys\njson.dump(json.load(sys.stdin), sys.stdout)\n");
    let mut limited = request(
        program.clone(),
        program.clone(),
        vec![json!({"n": 1}), json!({"n": 2})],
    );
    limited.budgets.max_executions = 1;
    let exhausted = compare(dir.path(), &limited, cancel()).await.unwrap();
    assert_eq!(exhausted.outcome, CompareOutcome::BudgetExhausted);
    assert!(exhausted.metrics.executions <= 1);
    assert_eq!(exhausted.metrics.discovered_differences, 0);

    let flag = Arc::new(AtomicBool::new(true));
    let cancelled = compare(
        dir.path(),
        &request(program.clone(), program, vec![json!({})]),
        flag,
    )
    .await
    .unwrap();
    assert_eq!(cancelled.outcome, CompareOutcome::Cancelled);
    assert_eq!(cancelled.metrics.executions, 0);
}

#[tokio::test]
async fn generated_arrays_use_the_seed_and_retention_is_bounded() {
    let dir = TempWork::new();
    std::fs::write(dir.path().join("kept.py"), b"import json,sys\nv=json.load(sys.stdin)\njson.dump({'total': sum(v['values'])}, sys.stdout)\n").unwrap();
    let mut req = request(py("kept.py"), py("kept.py"), Vec::new());
    req.generated = Some(GeneratedCases {
        seed: 11,
        integers: None,
        arrays: Some(ArraySpec {
            count: 2,
            length: 3,
            min: -2,
            max: 2,
            key: "values".into(),
        }),
    });
    req.retention.max_entries = 1;
    let first = compare(dir.path(), &req, cancel()).await.unwrap();
    assert_eq!(first.outcome, CompareOutcome::BoundedNoDifference);
    assert_eq!(first.cases_selected, 2);
    let second = compare(dir.path(), &req, cancel()).await.unwrap();
    let entries = std::fs::read_dir(dir.path().join(".checkweave/behavior"))
        .unwrap()
        .count();
    assert_eq!(entries, 1);
    assert_ne!(second.id, first.id);
    assert!(
        dir.path()
            .join(".checkweave/behavior")
            .join(&second.id)
            .join("manifest.json")
            .is_file()
    );
    assert!(
        !dir.path()
            .join(".checkweave/behavior")
            .join(&first.id)
            .exists()
    );
}

#[tokio::test]
async fn historical_revision_does_not_switch_checkout_or_mix_dirty_bytes() {
    let dir = TempWork::new();
    std::fs::write(
        dir.path().join("prog.py"),
        b"import json,sys\njson.dump({'n': 1}, sys.stdout)\n",
    )
    .unwrap();
    git(dir.path(), &["init"]);
    git(dir.path(), &["add", "prog.py"]);
    git(dir.path(), &["commit", "-m", "base"]);
    let head = Command::new("git")
        .arg("-C")
        .arg(dir.path())
        .args(["rev-parse", "HEAD"])
        .output()
        .unwrap();
    let head = String::from_utf8(head.stdout).unwrap();
    std::fs::write(
        dir.path().join("prog.py"),
        b"import json,sys\njson.dump({'n': 2}, sys.stdout)\n",
    )
    .unwrap();
    let after = inline("import json,sys\njson.dump({'n': 1}, sys.stdout)\n");
    let mut before = py("prog.py");
    before.sources = vec!["prog.py".into()];
    let mut req = request(before, after, vec![json!({})]);
    req.before_revision = Some("HEAD".into());
    let report = compare(dir.path(), &req, cancel()).await.unwrap();
    assert_eq!(
        report.outcome,
        CompareOutcome::BoundedNoDifference,
        "{:?}",
        report.warnings
    );
    assert!(
        report
            .warnings
            .iter()
            .any(|w| w.contains("without mixing dirty"))
    );
    assert_eq!(report.before_revision.as_deref(), Some(head.trim()));
    assert!(
        !serde_json::to_string(&report)
            .unwrap()
            .contains("checkweave-wt-")
    );

    let head_after = Command::new("git")
        .arg("-C")
        .arg(dir.path())
        .args(["rev-parse", "HEAD"])
        .output()
        .unwrap();
    assert_eq!(head_after.stdout, head.as_bytes());
    let status_after = String::from_utf8(
        Command::new("git")
            .arg("-C")
            .arg(dir.path())
            .args(["status", "--porcelain"])
            .output()
            .unwrap()
            .stdout,
    )
    .unwrap();
    assert!(status_after.contains("prog.py"), "{status_after}");
    assert!(!status_after.contains("HEAD"), "{status_after}");
    let dirty = std::fs::read_to_string(dir.path().join("prog.py")).unwrap();
    assert!(dirty.contains("'n': 2"), "{dirty}");

    let mut missing = py("untracked.py");
    std::fs::write(
        dir.path().join("untracked.py"),
        b"import json,sys\njson.dump({'n': 9}, sys.stdout)\n",
    )
    .unwrap();
    missing.sources = vec!["untracked.py".into()];
    let mut req = request(
        missing,
        inline("import json,sys\njson.dump({'n': 1}, sys.stdout)\n"),
        vec![json!({})],
    );
    req.before_revision = Some(head.trim().into());
    let unsupported = compare(dir.path(), &req, cancel()).await.unwrap();
    assert_eq!(unsupported.outcome, CompareOutcome::Unsupported);
    assert!(
        unsupported
            .warnings
            .iter()
            .any(|w| w.contains("missing at revision"))
    );
    assert!(
        std::fs::read_to_string(dir.path().join("untracked.py"))
            .unwrap()
            .contains("9")
    );
}

#[tokio::test]
async fn reproduction_argv_replays_with_the_built_binary() {
    let dir = TempWork::new();
    let elsewhere = TempWork::new();
    std::fs::write(
        dir.path().join("same.py"),
        b"import json,sys\njson.dump(json.load(sys.stdin), sys.stdout)\n",
    )
    .unwrap();
    let bin = env!("CARGO_BIN_EXE_checkweave");
    let root = dir.path().canonicalize().unwrap();
    let init = Command::new(bin)
        .args([
            "--workspace",
            root.to_str().unwrap(),
            "init",
            "--agent",
            "none",
        ])
        .current_dir(elsewhere.path())
        .env("RUST_LOG", "off")
        .output()
        .unwrap();
    assert!(
        init.status.success(),
        "{}",
        String::from_utf8_lossy(&init.stderr)
    );
    let req = request(py("same.py"), py("same.py"), vec![json!({"n": 1})]);
    let report = compare(dir.path(), &req, cancel()).await.unwrap();
    let argv = report.reproduction.as_ref().unwrap().argv.clone();
    assert_eq!(
        argv,
        vec![
            "checkweave".to_string(),
            "--workspace".into(),
            root.display().to_string(),
            "replay".into(),
            "--kind".into(),
            "compare".into(),
            "--id".into(),
            report.id.clone(),
        ]
    );
    let replay = Command::new(bin)
        .args(argv.iter().skip(1).map(String::as_str))
        .current_dir(elsewhere.path())
        .env("RUST_LOG", "off")
        .output()
        .unwrap();
    let shutdown = Command::new(bin)
        .args(["--workspace", root.to_str().unwrap(), "shutdown"])
        .current_dir(elsewhere.path())
        .env("RUST_LOG", "off")
        .output()
        .unwrap();
    assert!(
        replay.status.success(),
        "stdout {}\nstderr {}",
        String::from_utf8_lossy(&replay.stdout),
        String::from_utf8_lossy(&replay.stderr)
    );
    let value: Value = serde_json::from_slice(&replay.stdout).unwrap();
    assert_eq!(value["replay_of"], report.id);
    assert_eq!(value["outcome_reproduced"], true);
    let _ = shutdown;
}

#[tokio::test]
async fn global_deadline_kills_a_long_command_and_git_setup() {
    let dir = TempWork::new();
    std::fs::write(dir.path().join("slow.py"), b"import time\ntime.sleep(30)\n").unwrap();
    let mut req = request(py("slow.py"), py("slow.py"), vec![json!({})]);
    req.budgets.timeout_ms = 400;
    req.per_execution.timeout_ms = 30_000;
    let started = std::time::Instant::now();
    let report = compare(dir.path(), &req, cancel()).await.unwrap();
    assert!(
        started.elapsed() < std::time::Duration::from_secs(3),
        "{:?}",
        started.elapsed()
    );
    assert_eq!(
        report.outcome,
        CompareOutcome::BudgetExhausted,
        "{:?}",
        report.warnings
    );
    assert!(!report.metrics.reproducible);

    git(dir.path(), &["init"]);
    git(dir.path(), &["add", "slow.py"]);
    git(dir.path(), &["commit", "-m", "slow"]);
    let mut historical = request(py("slow.py"), py("slow.py"), vec![json!({})]);
    historical.before_revision = Some("HEAD".into());
    historical.budgets.timeout_ms = 200;
    historical.per_execution.timeout_ms = 30_000;
    let started = std::time::Instant::now();
    let git_report = compare(dir.path(), &historical, cancel()).await.unwrap();
    assert!(
        started.elapsed() < std::time::Duration::from_secs(3),
        "{:?}",
        started.elapsed()
    );
    assert_eq!(
        git_report.outcome,
        CompareOutcome::BudgetExhausted,
        "{:?}",
        git_report.warnings
    );
    assert!(!git_report.metrics.reproducible);
    assert!(git_report.elapsed_ms < 3_000);
}

#[tokio::test]
async fn stale_sources_are_not_called_reproducible_and_bytes_must_match() {
    let dir = TempWork::new();
    let script = "import json,sys,pathlib\npathlib.Path('same.py').write_text(pathlib.Path('same.py').read_text()+'\\n')\njson.dump(json.load(sys.stdin), sys.stdout)\n";
    std::fs::write(dir.path().join("same.py"), script).unwrap();
    let req = request(py("same.py"), py("same.py"), vec![json!({"n": 1})]);
    let report = compare(dir.path(), &req, cancel()).await.unwrap();
    assert_eq!(
        report.outcome,
        CompareOutcome::BoundedNoDifference,
        "{:?}",
        report.warnings
    );
    assert_eq!(report.freshness, "stale");
    assert!(!report.metrics.reproducible);
    assert!(report.before_sources.iter().any(|src| !src.bytes_retained));
    assert!(
        report
            .missing_evidence
            .iter()
            .any(|item| item.contains("do not match"))
    );

    let clean = TempWork::new();
    std::fs::write(
        clean.path().join("same.py"),
        b"import json,sys\njson.dump(json.load(sys.stdin), sys.stdout)\n",
    )
    .unwrap();
    let kept = compare(
        clean.path(),
        &request(py("same.py"), py("same.py"), vec![json!({"n": 1})]),
        cancel(),
    )
    .await
    .unwrap();
    assert!(
        kept.before_sources
            .iter()
            .any(|src| src.bytes_retained && src.path == "same.py")
    );
    let mut remaining = 16 * 1024 * 1024u64;
    let evidence = clean.path().join(".checkweave/behavior").join(&kept.id);
    let retained_hash =
        fingerprint_file(&evidence.join("sources"), "same.py", &mut remaining).unwrap();
    assert_eq!(retained_hash, kept.before_sources[0].fingerprint);

    let mut huge = request(py("same.py"), py("same.py"), vec![json!({"n": 1})]);
    huge.retention.max_total_bytes = 64 * 1024 * 1024;
    let clamped = compare(clean.path(), &huge, cancel()).await.unwrap();
    assert!(clamped.warnings.iter().any(|w| w.contains("8 MiB")));
    let mut total = 0u64;
    for entry in walk(&clean.path().join(".checkweave/behavior")) {
        if entry.is_file() {
            total += entry.metadata().unwrap().len();
        }
    }
    assert!(total <= 8 * 1024 * 1024, "{total}");
}

fn walk(path: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(path) else {
        return out;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            out.extend(walk(&path));
        } else {
            out.push(path);
        }
    }
    out
}
