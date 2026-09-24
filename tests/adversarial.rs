//! Adversarial kernel checks against documented invariants.
//! Failures are defects: expectations are not relaxed to match an implementation.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicUsize, Ordering};
use std::sync::{Arc, Barrier};
use std::thread;
use std::time::{Duration, Instant};

use checkweave::collection::Engine;
use checkweave::types::{CheckReport, CheckRequest, JsonKind, Limits, OPERATOR_VERSION, Predicate};
use serde_json::{Value, json};

fn canonical_temp() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path().canonicalize().expect("canonicalize root");
    (dir, root)
}

fn write(path: &Path, bytes: impl AsRef<[u8]>) {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).expect("mkdir");
    }
    fs::write(path, bytes).expect("write");
}

fn atomic_replace(path: &Path, bytes: &[u8]) {
    let tmp = path.with_extension("jsonl.swap");
    fs::write(&tmp, bytes).expect("swap write");
    fs::rename(&tmp, path).expect("atomic rename");
}

fn limits(max_results: usize) -> Limits {
    Limits {
        max_results,
        ..Limits::default()
    }
}

fn request(include: &[&str], predicate: Predicate, limits: Limits) -> CheckRequest {
    CheckRequest {
        include: include.iter().map(|s| (*s).to_string()).collect(),
        predicate,
        limits,
    }
}

fn open(root: &Path) -> Engine {
    Engine::open(root).expect("engine open")
}

fn run(engine: &mut Engine, req: &CheckRequest) -> CheckReport {
    let cancel = AtomicBool::new(false);
    engine.check(req, &cancel).expect("check")
}

fn eq(path: &str, value: Value) -> Predicate {
    Predicate::Eq {
        path: path.into(),
        value,
    }
}

fn gt(path: &str, value: f64) -> Predicate {
    Predicate::Gt {
        path: path.into(),
        value,
    }
}

fn exists(path: &str) -> Predicate {
    Predicate::Exists { path: path.into() }
}

fn not(predicate: Predicate) -> Predicate {
    Predicate::Not {
        predicate: Box::new(predicate),
    }
}

fn all(predicates: Vec<Predicate>) -> Predicate {
    Predicate::All { predicates }
}

fn any(predicates: Vec<Predicate>) -> Predicate {
    Predicate::Any { predicates }
}

fn assert_accounting(report: &CheckReport) {
    let c = &report.coverage;
    assert_eq!(
        c.matched + c.unmatched + c.unresolved,
        c.evaluated,
        "three-valued counts must partition evaluated rows: {report:?}"
    );
    assert!(
        report.items.len() <= c.matched + c.unresolved,
        "items are matches and unresolved rows only: {report:?}"
    );
    assert_eq!(
        report.sources.len(),
        c.files,
        "source fingerprints are the file snapshot: {report:?}"
    );
}

fn assert_validated_snapshot(report: &CheckReport) {
    assert_eq!(report.execution, "complete", "{report:?}");
    assert_eq!(report.freshness, "validated", "{report:?}");
    assert_eq!(report.operator_version, OPERATOR_VERSION, "{report:?}");
    assert!(!report.generation.is_empty(), "{report:?}");
    assert!(!report.basis.is_empty(), "{report:?}");
    let fps: std::collections::BTreeMap<_, _> = report
        .sources
        .iter()
        .map(|s| (s.path.clone(), s.fingerprint.clone()))
        .collect();
    assert_eq!(
        fps.len(),
        report.sources.len(),
        "duplicate source paths: {report:?}"
    );
    for item in &report.items {
        let fp = fps
            .get(&item.source.path)
            .unwrap_or_else(|| panic!("item path missing from sources: {item:?}"));
        assert_eq!(
            &item.source.fingerprint, fp,
            "item fingerprint disagrees with source snapshot: {report:?}"
        );
        assert!(item.source.line >= 1, "line numbers are 1-based: {item:?}");
    }
    assert_accounting(report);
}

fn source_fp<'a>(report: &'a CheckReport, path: &str) -> &'a str {
    report
        .sources
        .iter()
        .find(|s| s.path == path)
        .unwrap_or_else(|| panic!("missing source {path}: {report:?}"))
        .fingerprint
        .as_str()
}

fn decisions(report: &CheckReport) -> Vec<(String, usize, Option<bool>)> {
    let mut rows: Vec<_> = report
        .items
        .iter()
        .map(|i| (i.source.path.clone(), i.source.line, i.matched))
        .collect();
    rows.sort();
    rows
}

fn restore_mtime(path: &Path, mtime: filetime::FileTime) {
    filetime::set_file_mtime(path, mtime).expect("set mtime");
}

#[test]
fn same_size_same_mtime_is_a_content_change() {
    let (_tmp, root) = canonical_temp();
    let path = root.join("items.jsonl");
    let before = b"{\"n\":1}\n{\"n\":2}\n{\"n\":3}\n";
    let after = b"{\"n\":1}\n{\"n\":0}\n{\"n\":3}\n";
    assert_eq!(before.len(), after.len());
    write(&path, before);
    let mut engine = open(&root);
    let req = request(&["*.jsonl"], gt("/n", 1.0), limits(50));
    let cold = run(&mut engine, &req);
    assert_validated_snapshot(&cold);
    assert_eq!(cold.coverage.matched, 2, "{cold:?}");
    assert_eq!(cold.coverage.evaluated, 3, "{cold:?}");
    let previous_generation = cold.generation.clone();
    let fp = source_fp(&cold, "items.jsonl").to_string();

    let meta = fs::metadata(&path).unwrap();
    let mtime = filetime::FileTime::from_last_modification_time(&meta);
    write(&path, after);
    restore_mtime(&path, mtime);
    let after_meta = fs::metadata(&path).unwrap();
    assert_eq!(meta.len(), after_meta.len());
    assert_eq!(
        filetime::FileTime::from_last_modification_time(&after_meta),
        mtime
    );

    let reconciled = engine.reconcile().expect("reconcile");
    assert!(
        reconciled.is_object()
            || reconciled.is_array()
            || reconciled.is_number()
            || reconciled.is_string(),
        "reconcile should report a value, got {reconciled}"
    );

    let historical = engine
        .evidence(&cold.id)
        .expect("evidence")
        .expect("stored report");
    assert_ne!(
        historical.freshness, "validated",
        "same-size same-mtime edit must not stay validated: {historical:?}"
    );

    let edited = run(&mut engine, &req);
    assert_validated_snapshot(&edited);
    assert_eq!(edited.coverage.matched, 1, "{edited:?}");
    assert_eq!(edited.coverage.evaluated, 3, "{edited:?}");
    assert_eq!(
        edited.coverage.cache_hits, 2,
        "unchanged rows stay reusable: {edited:?}"
    );
    assert_eq!(edited.coverage.cache_misses, 1, "{edited:?}");
    assert_ne!(
        edited.generation, previous_generation,
        "generation tracks content, not metadata"
    );
    assert_ne!(source_fp(&edited, "items.jsonl"), fp.as_str());
    let _ = engine.stats().expect("stats");
}

#[test]
fn ignore_negation_changes_membership() {
    let (_tmp, root) = canonical_temp();
    write(&root.join("visible.jsonl"), "{\"n\":2}\n");
    write(&root.join("hidden.jsonl"), "{\"n\":3}\n");
    write(&root.join(".checkweave").join("nope.jsonl"), "{\"n\":9}\n");
    write(&root.join(".ignore"), "*.jsonl\n!visible.jsonl\n");
    let mut engine = open(&root);
    let req = request(&["**/*.jsonl"], gt("/n", 1.0), limits(50));
    let ignored = run(&mut engine, &req);
    assert_validated_snapshot(&ignored);
    let paths: Vec<_> = ignored.sources.iter().map(|s| s.path.as_str()).collect();
    assert_eq!(paths, ["visible.jsonl"], "{ignored:?}");
    assert_eq!(ignored.coverage.matched, 1, "{ignored:?}");
    assert!(
        !paths
            .iter()
            .any(|p| p.contains(".checkweave") || *p == "hidden.jsonl"),
        "{ignored:?}"
    );

    write(
        &root.join(".ignore"),
        "*.jsonl\n!visible.jsonl\n!hidden.jsonl\n",
    );
    let negated = run(&mut engine, &req);
    assert_validated_snapshot(&negated);
    let mut paths: Vec<_> = negated.sources.iter().map(|s| s.path.clone()).collect();
    paths.sort();
    assert_eq!(paths, ["hidden.jsonl", "visible.jsonl"], "{negated:?}");
    assert_eq!(negated.coverage.matched, 2, "{negated:?}");
    assert!(
        negated
            .sources
            .iter()
            .all(|s| !s.path.contains(".checkweave"))
    );

    write(&root.join(".ignore"), "hidden.jsonl\n");
    let dropped = run(&mut engine, &req);
    assert_validated_snapshot(&dropped);
    assert!(
        dropped.sources.iter().all(|s| s.path != "hidden.jsonl"),
        "{dropped:?}"
    );
    assert_eq!(dropped.coverage.files, 1, "{dropped:?}");
}

#[test]
fn symlink_and_include_cannot_escape_workspace() {
    let (_tmp, root) = canonical_temp();
    let outside = tempfile::tempdir().expect("outside");
    let outside = outside.path().canonicalize().expect("outside canon");
    write(&outside.join("secret.jsonl"), "{\"n\":1}\n");
    write(&root.join("inside.jsonl"), "{\"n\":2}\n");
    std::os::unix::fs::symlink(&outside, root.join("linked-dir")).expect("symlink dir");
    std::os::unix::fs::symlink(outside.join("secret.jsonl"), root.join("linked.jsonl"))
        .expect("symlink file");
    std::os::unix::fs::symlink(root.join("inside.jsonl"), root.join("alias.jsonl"))
        .expect("internal symlink");

    let mut engine = open(&root);
    let req = request(&["**/*.jsonl"], gt("/n", 0.0), limits(50));
    let report = run(&mut engine, &req);
    assert_validated_snapshot(&report);
    let paths: Vec<_> = report.sources.iter().map(|s| s.path.as_str()).collect();
    assert!(paths.contains(&"inside.jsonl"), "{report:?}");
    assert!(
        paths
            .iter()
            .all(|p| *p != "linked.jsonl" && !p.contains("secret") && !p.contains("linked-dir")),
        "symlink escapes or symlink entries were scanned: {paths:?}"
    );
    assert!(
        !paths.contains(&"alias.jsonl"),
        "symlinks are not collection members: {paths:?}"
    );

    for include in [
        "../secret.jsonl",
        "/tmp/secret.jsonl",
        "linked-dir/../secret.jsonl",
    ] {
        let bad = request(&[include], exists("/n"), Limits::default());
        let err = engine.check(&bad, &AtomicBool::new(false));
        assert!(
            err.is_err(),
            "include {include} must be rejected, got {err:?}"
        );
    }
}

#[test]
fn invalid_unicode_blank_lines_and_byte_ceiling() {
    let (_tmp, root) = canonical_temp();
    let mut bytes = Vec::new();
    bytes.extend(b"\n");
    bytes.extend(b"{\"n\":1}\n");
    bytes.extend([0xff, 0xfe]);
    bytes.extend(b"\n");
    bytes.extend(b"{not json\n");
    write(&root.join("mixed.jsonl"), &bytes);
    let mut engine = open(&root);
    let req = request(&["*.jsonl"], gt("/n", 0.0), limits(50));
    let report = run(&mut engine, &req);
    assert_eq!(report.freshness, "validated", "{report:?}");
    assert_eq!(report.execution, "complete", "{report:?}");
    assert_accounting(&report);
    assert_eq!(
        report.coverage.matched, 1,
        "valid row still matches: {report:?}"
    );
    assert!(
        report.coverage.skipped >= 1,
        "blank line counts as skipped: {report:?}"
    );
    assert!(
        report.coverage.unresolved >= 1,
        "malformed rows are unresolved: {report:?}"
    );
    assert!(
        report
            .items
            .iter()
            .any(|i| i.source.line == 2 && i.matched == Some(true)),
        "physical line of the valid record is 2: {report:?}"
    );
    assert!(
        report
            .items
            .iter()
            .all(|i| i.matched != Some(true) || i.source.line == 2),
        "invalid bytes must not become a match: {report:?}"
    );
    let good = report.items.iter().find(|i| i.source.line == 2).unwrap();
    assert_eq!(good.value, Some(json!({"n": 1})));

    let huge = root.join("huge.jsonl");
    let mut line = vec![b' '; 256 * 1024];
    line[0] = b'{';
    write(&huge, &line);
    let bounded = CheckRequest {
        include: vec!["huge.jsonl".into()],
        predicate: exists("/n"),
        limits: Limits {
            max_bytes: 1024,
            ..Limits::default()
        },
    };
    let partial = engine
        .check(&bounded, &AtomicBool::new(false))
        .expect("bounded check");
    assert_ne!(
        partial.execution, "complete",
        "over-budget input is not a complete success: {partial:?}"
    );
    assert!(
        partial.execution == "partial" || partial.execution == "failed",
        "{partial:?}"
    );
    assert!(
        partial.items.iter().all(|i| i.matched != Some(true)),
        "a truncated huge line must not be published as a match: {partial:?}"
    );
    assert!(partial.coverage.evaluated <= partial.coverage.records.max(1));
}

#[test]
fn schema_regex_and_recursion_rejected_before_success() {
    let (_tmp, root) = canonical_temp();
    write(&root.join("ok.jsonl"), "{\"s\":\"aaa\"}\n");
    let mut engine = open(&root);
    let good = request(
        &["ok.jsonl"],
        Predicate::Regex {
            path: "/s".into(),
            pattern: "^a+$".into(),
        },
        limits(10),
    );
    let report = run(&mut engine, &good);
    assert_validated_snapshot(&report);
    assert_eq!(report.coverage.matched, 1, "{report:?}");

    let invalid = request(
        &["ok.jsonl"],
        Predicate::Regex {
            path: "/s".into(),
            pattern: "(".into(),
        },
        Limits::default(),
    );
    assert!(
        engine.check(&invalid, &AtomicBool::new(false)).is_err(),
        "invalid regex"
    );

    let bare = request(&["ok.jsonl"], exists("s"), Limits::default());
    assert!(
        engine.check(&bare, &AtomicBool::new(false)).is_err(),
        "paths are JSON Pointers"
    );
    assert!(
        engine
            .check(
                &request(&["/etc/passwd"], exists("/s"), Limits::default()),
                &AtomicBool::new(false)
            )
            .is_err()
    );
    assert!(
        engine
            .check(
                &request(&["../ok.jsonl"], exists("/s"), Limits::default()),
                &AtomicBool::new(false)
            )
            .is_err()
    );

    for value in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
        let bad = request(&["ok.jsonl"], gt("/s", value), Limits::default());
        assert!(
            engine.check(&bad, &AtomicBool::new(false)).is_err(),
            "non-finite {value}"
        );
    }

    let shallow = request(&["ok.jsonl"], not(not(exists("/s"))), limits(10));
    let shallow = run(&mut engine, &shallow);
    assert_validated_snapshot(&shallow);
    assert_eq!(shallow.coverage.matched, 1, "{shallow:?}");
}

/// Depth past the kernel predicate bound, still inside serde's default JSON
/// recursion limit and small enough that a recursive `Drop` would also be safe.
const BOUNDED_PREDICATE_DEPTH: usize = 48;

fn drop_predicate_iterative(mut current: Predicate) {
    loop {
        current = match current {
            Predicate::Not { predicate } => *predicate,
            other => {
                drop(other);
                return;
            }
        };
    }
}

#[test]
fn recursive_predicate_is_rejected_without_aborting() {
    let (_tmp, root) = canonical_temp();
    write(&root.join("ok.jsonl"), "{\"s\":\"aaa\"}\n");
    let mut engine = open(&root);
    let mut predicate = exists("/s");
    for _ in 0..BOUNDED_PREDICATE_DEPTH {
        predicate = not(predicate);
    }
    let mut req = request(&["ok.jsonl"], predicate, Limits::default());
    let outcome = engine.check(&req, &AtomicBool::new(false));
    let predicate = std::mem::replace(&mut req.predicate, exists("/s"));
    drop_predicate_iterative(predicate);
    assert!(
        outcome.is_err(),
        "a predicate deeper than the kernel bound must be rejected before evaluation, got {outcome:?}"
    );
}

#[test]
fn large_integers_do_not_collapse_under_f64() {
    let (_tmp, root) = canonical_temp();
    write(
        &root.join("nums.jsonl"),
        "\
{\"n\":9007199254740993}\n\
{\"n\":9007199254740992}\n\
{\"n\":18446744073709551615}\n",
    );
    let mut engine = open(&root);
    let hi = json!(9_007_199_254_740_993u64);
    let lo = json!(9_007_199_254_740_992u64);
    let max_u = json!(u64::MAX);
    let req = request(&["nums.jsonl"], eq("/n", hi.clone()), limits(10));
    let report = run(&mut engine, &req);
    assert_validated_snapshot(&report);
    assert_eq!(
        report.coverage.matched, 1,
        "2^53+1 must not equal 2^53: {report:?}"
    );
    assert_eq!(report.coverage.unmatched, 2, "{report:?}");
    let item = report
        .items
        .iter()
        .find(|i| i.matched == Some(true))
        .unwrap();
    assert_eq!(item.source.line, 1, "{report:?}");
    assert_eq!(item.value.as_ref().and_then(|v| v.get("n")), Some(&hi));

    let low = run(
        &mut engine,
        &request(&["nums.jsonl"], eq("/n", lo.clone()), limits(10)),
    );
    assert_eq!(low.coverage.matched, 1, "{low:?}");
    assert_eq!(low.items[0].source.line, 2, "{low:?}");
    assert_eq!(
        low.items[0].value.as_ref().and_then(|v| v.get("n")),
        Some(&lo)
    );

    let big = run(
        &mut engine,
        &request(&["nums.jsonl"], eq("/n", max_u.clone()), limits(10)),
    );
    assert_eq!(big.coverage.matched, 1, "{big:?}");
    assert_eq!(big.items[0].source.line, 3, "{big:?}");
    assert_ne!(source_fp(&report, "nums.jsonl"), "");

    write(
        &root.join("wide.jsonl"),
        "\
{\"n\":18446744073709551616}\n\
{\"n\":18446744073709551617}\n",
    );
    let probe: Value = serde_json::from_str("18446744073709551616").expect("probe number");
    let wide = run(
        &mut engine,
        &request(&["wide.jsonl"], eq("/n", probe), limits(10)),
    );
    let mut matched_lines: Vec<_> = wide
        .items
        .iter()
        .filter(|item| item.matched == Some(true))
        .map(|item| item.source.line)
        .collect();
    matched_lines.sort_unstable();
    assert!(
        matched_lines.len() < 2,
        "integers above u64 that differ by one must not compare equal: {wide:?}"
    );
    if wide.coverage.matched == 1 {
        assert_eq!(
            matched_lines,
            vec![1],
            "only the probed literal may match: {wide:?}"
        );
        assert_eq!(
            wide.coverage.unmatched + wide.coverage.unresolved,
            1,
            "{wide:?}"
        );
    } else {
        assert_eq!(wide.coverage.matched, 0, "{wide:?}");
        assert!(
            wide.coverage.unresolved >= 1,
            "inexact integers above u64 must be unresolved rather than falsely equal: {wide:?}"
        );
    }
}

#[test]
fn missing_null_and_three_valued_logic() {
    let (_tmp, root) = canonical_temp();
    write(
        &root.join("rows.jsonl"),
        "\
{\"n\":null}\n\
{}\n\
{\"n\":1}\n\
{\"n\":\"1\"}\n\
{\"n\":false}\n",
    );
    let mut engine = open(&root);
    let base = Limits {
        max_results: 50,
        ..Limits::default()
    };

    let exists_report = run(
        &mut engine,
        &request(&["rows.jsonl"], exists("/n"), base.clone()),
    );
    assert_validated_snapshot(&exists_report);
    assert_eq!(
        exists_report.coverage.matched, 4,
        "null is present: {exists_report:?}"
    );
    assert_eq!(
        exists_report.coverage.unmatched, 1,
        "missing exists is false: {exists_report:?}"
    );
    assert_eq!(exists_report.coverage.unresolved, 0, "{exists_report:?}");
    assert!(exists_report.items.iter().all(|i| i.source.line != 2));

    let eq_null = run(
        &mut engine,
        &request(&["rows.jsonl"], eq("/n", Value::Null), base.clone()),
    );
    assert_validated_snapshot(&eq_null);
    assert_eq!(eq_null.coverage.matched, 1, "{eq_null:?}");
    assert_eq!(
        eq_null.coverage.unresolved, 1,
        "missing eq is unresolved, not false: {eq_null:?}"
    );
    assert_eq!(eq_null.coverage.unmatched, 3, "{eq_null:?}");
    let unresolved = eq_null.items.iter().find(|i| i.matched.is_none()).unwrap();
    assert_eq!(unresolved.source.line, 2);

    let kind_null = run(
        &mut engine,
        &request(
            &["rows.jsonl"],
            Predicate::Kind {
                path: "/n".into(),
                kind: JsonKind::Null,
            },
            base.clone(),
        ),
    );
    assert_eq!(kind_null.coverage.matched, 1, "{kind_null:?}");
    assert_eq!(kind_null.coverage.unresolved, 1, "{kind_null:?}");

    let gt_report = run(
        &mut engine,
        &request(&["rows.jsonl"], gt("/n", 0.0), base.clone()),
    );
    assert_eq!(gt_report.coverage.matched, 1, "{gt_report:?}");
    assert_eq!(
        gt_report.coverage.unresolved, 4,
        "null, missing, string, bool are unresolved: {gt_report:?}"
    );

    let not_exists = run(
        &mut engine,
        &request(&["rows.jsonl"], not(exists("/n")), base.clone()),
    );
    assert_eq!(
        not_exists.coverage.matched, 1,
        "not(false) on the missing row: {not_exists:?}"
    );
    assert_eq!(not_exists.coverage.unmatched, 4, "{not_exists:?}");

    let unknown = gt("/n", 0.0);
    let not_unknown = run(
        &mut engine,
        &request(&["rows.jsonl"], not(unknown.clone()), base.clone()),
    );
    assert_eq!(
        not_unknown.coverage.unresolved, 4,
        "not(unknown) stays unknown: {not_unknown:?}"
    );
    assert_eq!(not_unknown.coverage.matched, 0, "{not_unknown:?}");

    let all_false = run(
        &mut engine,
        &request(
            &["rows.jsonl"],
            all(vec![eq("/n", json!(2)), unknown.clone()]),
            base.clone(),
        ),
    );
    assert_eq!(all_false.coverage.matched, 0, "{all_false:?}");
    assert_eq!(
        all_false.coverage.unresolved, 1,
        "missing stays unknown when no conjunct is false: {all_false:?}"
    );
    assert_eq!(
        all_false.coverage.unmatched, 4,
        "a false conjunct decides all: {all_false:?}"
    );

    let any_true = run(
        &mut engine,
        &request(
            &["rows.jsonl"],
            any(vec![eq("/n", json!(1)), unknown]),
            base,
        ),
    );
    assert_eq!(any_true.coverage.matched, 1, "{any_true:?}");
    assert!(
        any_true.coverage.unresolved >= 1,
        "unknown any without a true stays unresolved: {any_true:?}"
    );
    assert_validated_snapshot(&any_true);
}

#[test]
fn ordered_predicate_identity_and_distinct_paths() {
    let (_tmp, root) = canonical_temp();
    let body = "{\"n\":1,\"m\":2}\n{\"n\":1,\"m\":0}\n";
    write(&root.join("a.jsonl"), body);
    write(&root.join("b.jsonl"), body);
    let mut engine = open(&root);
    let left = all(vec![eq("/n", json!(1)), eq("/m", json!(2))]);
    let right = all(vec![eq("/m", json!(2)), eq("/n", json!(1))]);
    let either = any(vec![eq("/n", json!(1)), eq("/m", json!(2))]);
    let req_left = request(&["*.jsonl"], left, limits(20));
    let first = run(&mut engine, &req_left);
    assert_validated_snapshot(&first);
    assert_eq!(first.coverage.files, 2, "{first:?}");
    assert_eq!(first.coverage.matched, 2, "{first:?}");
    assert_eq!(first.coverage.evaluated, 4, "{first:?}");
    assert_content_reuse(&first, "cold");
    let paths: Vec<_> = first.sources.iter().map(|s| s.path.as_str()).collect();
    assert_eq!(
        paths,
        ["a.jsonl", "b.jsonl"],
        "equal bytes stay distinct paths: {first:?}"
    );
    assert_eq!(source_fp(&first, "a.jsonl"), source_fp(&first, "b.jsonl"));
    let lines: Vec<_> = first
        .items
        .iter()
        .map(|i| (i.source.path.as_str(), i.source.line))
        .collect();
    assert!(
        lines.contains(&("a.jsonl", 1)) && lines.contains(&("b.jsonl", 1)),
        "{first:?}"
    );
    assert!(
        !lines.contains(&("a.jsonl", 2)) && !lines.contains(&("b.jsonl", 2)),
        "{first:?}"
    );

    let warm = run(&mut engine, &req_left);
    assert_ne!(warm.id, first.id, "sequential checks are distinct reports");
    assert_eq!(
        warm.generation, first.generation,
        "same snapshot, same generation"
    );
    assert_eq!(
        warm.coverage.cache_hits, warm.coverage.evaluated,
        "{warm:?}"
    );
    assert_eq!(warm.coverage.cache_misses, 0, "{warm:?}");
    assert_eq!(decisions(&warm), decisions(&first));

    let reordered = run(&mut engine, &request(&["*.jsonl"], right, limits(20)));
    assert_validated_snapshot(&reordered);
    assert_eq!(
        reordered.coverage.matched, first.coverage.matched,
        "{reordered:?}"
    );
    assert_eq!(decisions(&reordered), decisions(&first));
    assert_content_reuse(&reordered, "reordered all");

    let union = run(&mut engine, &request(&["*.jsonl"], either, limits(20)));
    assert_eq!(union.coverage.matched, 4, "any is not all: {union:?}");
    assert_content_reuse(&union, "any");

    let changed = run(
        &mut engine,
        &request(&["*.jsonl"], eq("/n", json!(0)), limits(20)),
    );
    assert_eq!(changed.coverage.matched, 0, "{changed:?}");
    assert_eq!(changed.coverage.unmatched, 4, "{changed:?}");
    assert_content_reuse(&changed, "eq /n 0");
    let flipped = run(
        &mut engine,
        &request(&["*.jsonl"], eq("/m", json!(0)), limits(20)),
    );
    assert_eq!(
        flipped.coverage.matched, 2,
        "rows false under all must be recomputed when a new predicate matches them: {flipped:?}"
    );
    assert!(
        flipped.items.iter().all(|item| item.source.line == 2),
        "{flipped:?}"
    );
    assert_content_reuse(&flipped, "eq /m 0");

    write(
        &root.join("b.jsonl"),
        "{\"n\":0,\"m\":2}\n{\"n\":1,\"m\":0}\n",
    );
    let edited = run(&mut engine, &req_left);
    assert_validated_snapshot(&edited);
    assert_eq!(source_fp(&edited, "a.jsonl"), source_fp(&first, "a.jsonl"));
    assert_ne!(source_fp(&edited, "b.jsonl"), source_fp(&first, "b.jsonl"));
    assert_eq!(
        edited.coverage.matched, 1,
        "only a.jsonl still matches: {edited:?}"
    );
    assert!(
        edited.items.iter().all(|i| i.source.path == "a.jsonl"),
        "{edited:?}"
    );
    assert_eq!(edited.generation, {
        let stable = run(&mut engine, &req_left);
        assert_eq!(
            stable.coverage.cache_hits, stable.coverage.evaluated,
            "{stable:?}"
        );
        stable.generation
    });
}

fn assert_content_reuse(report: &CheckReport, label: &str) {
    assert_eq!(
        report.coverage.cache_misses, 2,
        "{label}: two identical rows are two content misses, not one miss per path: {report:?}"
    );
    assert_eq!(
        report.coverage.cache_hits, 2,
        "{label}: the repeated content hits inside the request: {report:?}"
    );
}

#[test]
fn mutation_during_check_cannot_publish_a_false_snapshot() {
    let (_tmp, root) = canonical_temp();
    let path = root.join("data.jsonl");
    let oracle_a = fixed_corpus(3000, 0);
    let oracle_b = fixed_corpus(3000, 1);
    assert_eq!(oracle_a.len(), oracle_b.len());
    write(&path, &oracle_a);
    let mut engine = open(&root);
    let req = request(&["data.jsonl"], eq("/v", json!(0)), limits(4000));
    let stable_a = run(&mut engine, &req);
    assert_validated_snapshot(&stable_a);
    assert_eq!(stable_a.coverage.matched, 3000, "{stable_a:?}");
    atomic_replace(&path, &oracle_b);
    let stable_b = run(&mut engine, &req);
    assert_validated_snapshot(&stable_b);
    assert_eq!(stable_b.coverage.matched, 0, "{stable_b:?}");
    assert_ne!(stable_a.generation, stable_b.generation);
    atomic_replace(&path, &oracle_a);

    let phase = Arc::new(AtomicU8::new(0));
    let writes = Arc::new(AtomicUsize::new(0));
    let worker = {
        let phase = Arc::clone(&phase);
        let writes = Arc::clone(&writes);
        let path = path.clone();
        let a = oracle_a.clone();
        let b = oracle_b.clone();
        thread::spawn(move || {
            while phase.load(Ordering::SeqCst) == 0 {
                thread::yield_now();
            }
            let mut flip = false;
            while phase.load(Ordering::SeqCst) == 1 {
                flip = !flip;
                atomic_replace(&path, if flip { &b } else { &a });
                writes.fetch_add(1, Ordering::Relaxed);
            }
        })
    };
    phase.store(1, Ordering::SeqCst);
    let raced = engine
        .check(&req, &AtomicBool::new(false))
        .expect("raced check");
    phase.store(2, Ordering::SeqCst);
    worker.join().expect("mutator");
    let observed = writes.load(Ordering::Relaxed);
    eprintln!("adversarial_scope mutation_writes_during_check={observed}");

    if raced.freshness == "validated" {
        assert_validated_snapshot(&raced);
        let matched_a = raced.coverage.matched == stable_a.coverage.matched
            && raced.generation == stable_a.generation
            && source_fp(&raced, "data.jsonl") == source_fp(&stable_a, "data.jsonl");
        let matched_b = raced.coverage.matched == stable_b.coverage.matched
            && raced.generation == stable_b.generation
            && source_fp(&raced, "data.jsonl") == source_fp(&stable_b, "data.jsonl");
        assert!(
            matched_a || matched_b,
            "validated snapshot must be entirely corpus A or B (writes={observed}): {raced:?}"
        );
        assert!(!(matched_a && matched_b));
    } else if observed > 0 {
        assert!(
            raced.freshness == "stale" || raced.freshness == "unknown",
            "in-flight edit must not be published as validated (writes={observed}): {raced:?}"
        );
    } else {
        panic!("no in-flight write and result was not validated: {raced:?}");
    }

    atomic_replace(&path, &oracle_a);
    let again = run(&mut engine, &req);
    assert_validated_snapshot(&again);
    assert_eq!(again.generation, stable_a.generation, "{again:?}");
    assert_eq!(again.coverage.matched, 3000, "{again:?}");
    let evidence = engine
        .evidence(&again.id)
        .expect("evidence")
        .expect("stored");
    assert_eq!(evidence.freshness, "validated");
    assert_eq!(evidence.generation, again.generation);
    for source in &evidence.sources {
        assert_eq!(source.fingerprint, source_fp(&again, &source.path));
    }
}

fn fixed_corpus(rows: usize, version: u8) -> Vec<u8> {
    let mut out = String::new();
    for i in 0..rows {
        // Four-digit values stay in 1000.. so every line is valid JSON and the same width.
        out.push_str(&format!("{{\"i\":{},\"v\":{version}}}\n", i + 1000));
    }
    out.into_bytes()
}

#[test]
fn cancelled_check_does_not_publish_complete_and_restart_keeps_prior() {
    let (_tmp, root) = canonical_temp();
    write(&root.join("items.jsonl"), "{\"n\":2}\n{\"n\":0}\n");
    let mut engine = open(&root);
    let req = request(&["*.jsonl"], gt("/n", 1.0), limits(20));
    let published = run(&mut engine, &req);
    assert_validated_snapshot(&published);
    let cancel = AtomicBool::new(true);
    if let Ok(report) = engine.check(&req, &cancel) {
        assert_ne!(report.execution, "complete", "{report:?}");
        assert_eq!(report.execution, "cancelled", "{report:?}");
        if let Some(stored) = engine.evidence(&report.id).expect("evidence lookup") {
            assert_ne!(stored.execution, "complete", "{stored:?}");
        }
    }
    drop(engine);
    let mut engine = open(&root);
    let restored = engine
        .evidence(&published.id)
        .expect("reopen evidence")
        .expect("prior report");
    assert_eq!(restored.execution, "complete", "{restored:?}");
    assert_eq!(restored.freshness, "validated", "{restored:?}");
    assert_eq!(restored.generation, published.generation);
    assert_eq!(restored.coverage.matched, 1);
    let _ = engine.stats().expect("stats after reopen");
}

#[test]
fn corrupt_cache_recovers_without_hiding_permission_errors() {
    let (_tmp, root) = canonical_temp();
    let marker = root.join("keep.txt");
    write(&marker, "do-not-touch");
    write(&root.join("good.jsonl"), "{\"n\":2}\n");
    write(&root.join("other.jsonl"), "{\"n\":0}\n");
    let mut engine = open(&root);
    let req = request(&["good.jsonl"], gt("/n", 1.0), limits(10));
    let before = run(&mut engine, &req);
    assert_validated_snapshot(&before);
    drop(engine);

    let cache = root.join(".checkweave").join("cache.sqlite");
    assert!(cache.is_file(), "cache created");
    for suffix in ["", "-wal", "-shm"] {
        let path = root
            .join(".checkweave")
            .join(format!("cache.sqlite{suffix}"));
        if path.exists() {
            write(&path, b"this is not a sqlite database");
        }
    }
    let mut engine = open(&root);
    let recovered = run(&mut engine, &req);
    assert_validated_snapshot(&recovered);
    assert_eq!(recovered.coverage.matched, 1, "{recovered:?}");
    assert_eq!(fs::read(&marker).unwrap(), b"do-not-touch");
    assert!(cache.is_file(), "recovered cache still exists");
    let header = fs::read(&cache).unwrap_or_default();
    assert!(
        header.starts_with(b"SQLite format 3"),
        "corruption recovery should quarantine garbage and open a real database"
    );

    let locked = root.join("other.jsonl");
    let mut perms = fs::metadata(&locked).unwrap().permissions();
    use std::os::unix::fs::PermissionsExt;
    let original = perms.mode();
    perms.set_mode(0o000);
    fs::set_permissions(&locked, perms).unwrap();
    struct Restore(PathBuf, u32);
    impl Drop for Restore {
        fn drop(&mut self) {
            let mut perms = fs::metadata(&self.0).unwrap().permissions();
            perms.set_mode(self.1);
            let _ = fs::set_permissions(&self.0, perms);
        }
    }
    let _restore = Restore(locked.clone(), original);
    let unreadable = fs::File::open(&locked).is_err();
    if unreadable {
        let both = request(&["*.jsonl"], gt("/n", 1.0), limits(10));
        let outcome = engine.check(&both, &AtomicBool::new(false));
        match outcome {
            Ok(report) => {
                assert!(
                    report
                        .items
                        .iter()
                        .all(|i| i.source.path != "other.jsonl" || i.matched != Some(true)),
                    "unreadable input must not become a match: {report:?}"
                );
                assert_eq!(
                    report
                        .sources
                        .iter()
                        .filter(|s| s.path == "good.jsonl")
                        .count(),
                    1,
                    "readable sibling remains: {report:?}"
                );
                if report.sources.iter().any(|s| s.path == "good.jsonl")
                    && report.freshness == "validated"
                {
                    let good_items = report.coverage.matched;
                    assert!(good_items >= 1, "{report:?}");
                }
            }
            Err(err) => {
                let text = format!("{err:#}").to_ascii_lowercase();
                assert!(
                    !text.contains("corrupt") && !text.contains("quarantine"),
                    "an unreadable data file is not cache corruption: {err:#}"
                );
            }
        }
        assert_eq!(fs::read(root.join("good.jsonl")).unwrap(), b"{\"n\":2}\n");
        let still = run(&mut engine, &req);
        assert_validated_snapshot(&still);
        assert_eq!(
            still.coverage.matched, 1,
            "permission error must not destroy the cache: {still:?}"
        );
        let header = fs::read(&cache).unwrap_or_default();
        assert!(
            header.starts_with(b"SQLite format 3"),
            "permission error is not cache corruption"
        );
    }
}

fn two_matching_files() -> (tempfile::TempDir, PathBuf) {
    let (_tmp, root) = canonical_temp();
    write(&root.join("a.jsonl"), "{\"n\":2}\n{\"n\":3}\n{\"n\":4}\n");
    write(&root.join("b.jsonl"), "{\"n\":5}\n{\"n\":6}\n{\"n\":7}\n");
    (_tmp, root)
}

#[test]
fn max_records_is_global_across_files() {
    let (_tmp, root) = two_matching_files();
    let mut engine = open(&root);
    let records = CheckRequest {
        include: vec!["*.jsonl".into()],
        predicate: gt("/n", 1.0),
        limits: Limits {
            max_records: 4,
            max_results: 50,
            ..Limits::default()
        },
    };
    let report = run(&mut engine, &records);
    assert_eq!(report.execution, "partial", "{report:?}");
    assert!(
        report.coverage.evaluated <= 4,
        "max_records is global across files, not restarted per file: {report:?}"
    );
    assert!(report.coverage.matched <= 4, "{report:?}");
    assert_accounting(&report);
}

#[test]
fn max_results_is_global_across_files() {
    let (_tmp, root) = two_matching_files();
    let mut engine = open(&root);
    let details = CheckRequest {
        include: vec!["*.jsonl".into()],
        predicate: gt("/n", 1.0),
        limits: Limits {
            max_results: 1,
            ..Limits::default()
        },
    };
    let bounded = run(&mut engine, &details);
    assert!(
        bounded.items.len() <= 1,
        "max_results is global across files: {bounded:?}"
    );
    assert!(
        bounded.truncated
            || bounded.execution == "partial"
            || bounded.coverage.matched > bounded.items.len(),
        "withheld matches stay visible as truncation or a partial scan: {bounded:?}"
    );
}

#[test]
fn max_files_does_not_evaluate_unselected_files() {
    let (_tmp, root) = two_matching_files();
    let mut engine = open(&root);
    let files = CheckRequest {
        include: vec!["*.jsonl".into()],
        predicate: gt("/n", 1.0),
        limits: Limits {
            max_files: 1,
            ..Limits::default()
        },
    };
    let one = run(&mut engine, &files);
    assert!(
        one.coverage.files <= 1,
        "max_files bounds the scan: {one:?}"
    );
    assert!(
        one.coverage.evaluated <= 3,
        "the unselected file is not evaluated: {one:?}"
    );
    assert_eq!(one.execution, "partial", "{one:?}");
}

#[test]
fn concurrent_daemon_startup_shutdown_and_crash_reconnect() {
    let (_tmp, root) = canonical_temp();
    let bin = env!("CARGO_BIN_EXE_checkweave");
    cli(bin, &root, &["init", "--agent", "none"]);
    write(&root.join("items.jsonl"), "{\"n\":2}\n{\"n\":0}\n");

    let barrier = Arc::new(Barrier::new(4));
    let mut joins = Vec::new();
    for _ in 0..4 {
        let barrier = Arc::clone(&barrier);
        let root = root.clone();
        joins.push(thread::spawn(move || {
            barrier.wait();
            cli_value(bin, &root, &["status"])
        }));
    }
    let statuses: Vec<Value> = joins
        .into_iter()
        .map(|j| j.join().expect("status"))
        .collect();
    let mut pids: Vec<_> = statuses.iter().map(|s| s["pid"].as_u64()).collect();
    pids.sort();
    pids.dedup();
    assert_eq!(
        pids.len(),
        1,
        "concurrent startup must share one daemon: {statuses:?}"
    );
    assert!(pids[0].is_some(), "{statuses:?}");

    cli(bin, &root, &["shutdown"]);
    let reconnected = cli_value(
        bin,
        &root,
        &[
            "check",
            "--include",
            "*.jsonl",
            "--predicate",
            "{\"op\":\"gt\",\"path\":\"/n\",\"value\":1}",
        ],
    );
    assert_eq!(reconnected["execution"], "complete", "{reconnected}");
    assert_eq!(reconnected["freshness"], "validated", "{reconnected}");
    assert_eq!(reconnected["coverage"]["matched"], 1, "{reconnected}");
    assert_eq!(reconnected["coverage"]["evaluated"], 2, "{reconnected}");

    let status = cli_value(bin, &root, &["status"]);
    let pid = status["pid"].as_u64().expect("pid");
    let kill = Command::new("kill")
        .args(["-KILL", &pid.to_string()])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .expect("kill");
    assert!(kill.success(), "kill targeted daemon pid {pid}");
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        let alive = Command::new("kill")
            .args(["-0", &pid.to_string()])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
        if alive.map(|s| !s.success()).unwrap_or(true) {
            break;
        }
        thread::sleep(Duration::from_millis(30));
    }
    let alive = Command::new("kill")
        .args(["-0", &pid.to_string()])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
    assert!(
        alive.map(|s| !s.success()).unwrap_or(true),
        "daemon pid {pid} still alive"
    );
    let after = cli_value(
        bin,
        &root,
        &[
            "check",
            "--include",
            "*.jsonl",
            "--predicate",
            "{\"op\":\"gt\",\"path\":\"/n\",\"value\":1}",
        ],
    );
    assert_eq!(after["coverage"]["matched"], 1, "{after}");
    assert_eq!(after["freshness"], "validated", "{after}");
    let _ = cli_allow_fail(bin, &root, &["shutdown"]);
}

fn cli(bin: &str, root: &Path, args: &[&str]) -> std::process::Output {
    let output = Command::new(bin)
        .arg("--workspace")
        .arg(root)
        .args(args)
        .env("CHECKWEAVE_IDLE_SECONDS", "120")
        .output()
        .expect("spawn checkweave");
    assert!(
        output.status.success(),
        "args {args:?}\nstdout {}\nstderr {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    output
}

fn cli_value(bin: &str, root: &Path, args: &[&str]) -> Value {
    let output = cli(bin, root, args);
    serde_json::from_slice(&output.stdout).expect("cli json")
}

fn cli_allow_fail(bin: &str, root: &Path, args: &[&str]) -> std::process::Output {
    Command::new(bin)
        .arg("--workspace")
        .arg(root)
        .args(args)
        .env("CHECKWEAVE_IDLE_SECONDS", "120")
        .output()
        .expect("spawn")
}
