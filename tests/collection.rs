use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::time::Duration;

use checkweave::collection::{Engine, IPC_FRAME_BYTES, REPORT_JSON_BUDGET};
use checkweave::types::{
    CheckReport, CheckRequest, JsonKind, Limits, OPERATOR_VERSION, Predicate, RunSnapshot,
    RunState, WireResponse,
};
use filetime::{FileTime, set_file_mtime};
use serde_json::{Value, json};

fn ws() -> tempfile::TempDir {
    tempfile::tempdir().unwrap()
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
        ..Limits::default()
    }
}

fn req(include: &[&str], predicate: Predicate, limits: Limits) -> CheckRequest {
    CheckRequest {
        include: include.iter().map(|s| (*s).to_string()).collect(),
        predicate,
        limits,
    }
}

fn run(eng: &mut Engine, request: &CheckRequest) -> CheckReport {
    let cancel = AtomicBool::new(false);
    eng.check(request, &cancel).unwrap()
}

fn exists(path: &str) -> Predicate {
    Predicate::Exists { path: path.into() }
}

#[test]
fn cold_warm_counts_and_one_changed_record() {
    let dir = ws();
    let root = dir.path();
    write(&root.join("b.jsonl"), "{\"a\":1}\n{\"a\":0}\n{\"b\":1}\n");
    write(&root.join("a.jsonl"), "{\"a\":1}\n");
    let mut eng = Engine::open(root).unwrap();
    let request = req(&["*.jsonl"], exists("/a"), limits());
    let cold = run(&mut eng, &request);
    assert_eq!(cold.execution, "complete");
    assert_eq!(cold.freshness, "validated");
    assert_eq!(cold.basis, "deterministic");
    assert_eq!(cold.operator_version, OPERATOR_VERSION);
    assert_eq!(cold.coverage.files, 2);
    assert_eq!(cold.coverage.records, 4);
    assert_eq!(cold.coverage.evaluated, 4);
    assert_eq!(cold.coverage.matched, 3);
    assert_eq!(cold.coverage.unmatched, 1);
    assert_eq!(cold.coverage.unresolved, 0);
    assert_eq!(cold.coverage.cache_hits, 1);
    assert_eq!(cold.coverage.cache_misses, 3);
    assert!(!cold.truncated);
    assert!(cold.items.iter().all(|item| item.matched != Some(false)));
    assert_eq!(cold.items.len(), 3);
    assert_eq!(cold.items[0].source.path, "a.jsonl");
    assert_eq!(cold.items[0].source.line, 1);
    assert_eq!(cold.items[1].source.path, "b.jsonl");
    assert_eq!(cold.items[1].source.line, 1);
    assert_eq!(cold.items[0].value, Some(json!({"a": 1})));
    assert_eq!(
        cold.items[0].source.fingerprint,
        cold.sources
            .iter()
            .find(|s| s.path == "a.jsonl")
            .unwrap()
            .fingerprint
    );

    let warm = run(&mut eng, &request);
    assert_eq!(warm.coverage.cache_hits, 4);
    assert_eq!(warm.coverage.cache_misses, 0);
    assert_eq!(warm.coverage.matched, cold.coverage.matched);
    assert_eq!(warm.generation, cold.generation);
    assert_ne!(warm.id, cold.id);

    write(&root.join("b.jsonl"), "{\"a\":1}\n{\"a\":2}\n{\"b\":1}\n");
    let edited = run(&mut eng, &request);
    assert_eq!(edited.coverage.cache_hits, 3);
    assert_eq!(edited.coverage.cache_misses, 1);
    assert_eq!(edited.coverage.matched, 3);
    assert_eq!(edited.coverage.unmatched, 1);
    assert_ne!(edited.generation, cold.generation);
    let prior = eng.evidence(&cold.id).unwrap().unwrap();
    assert_eq!(prior.freshness, "stale");
}

#[test]
fn predicate_change_misses_then_old_predicate_hits() {
    let dir = ws();
    let root = dir.path();
    write(
        &root.join("rows.jsonl"),
        "{\"a\":1}\n{\"a\":0}\n{\"b\":1}\n",
    );
    let mut eng = Engine::open(root).unwrap();
    let first = req(&["*.jsonl"], exists("/a"), limits());
    let other = req(
        &["*.jsonl"],
        Predicate::Eq {
            path: "/a".into(),
            value: json!(1),
        },
        limits(),
    );
    let _ = run(&mut eng, &first);
    let changed = run(&mut eng, &other);
    assert_eq!(changed.coverage.cache_misses, 3);
    assert_eq!(changed.coverage.cache_hits, 0);
    assert_eq!(changed.coverage.matched, 1);
    assert_eq!(changed.coverage.unmatched, 1);
    assert_eq!(changed.coverage.unresolved, 1);
    assert_eq!(changed.items.len(), 2);
    let again = run(&mut eng, &first);
    assert_eq!(again.coverage.cache_hits, 3);
    assert_eq!(again.coverage.cache_misses, 0);
}

#[test]
fn malformed_blank_missing_null_and_types() {
    let dir = ws();
    let root = dir.path();
    write(
        &root.join("rows.jsonl"),
        "\n{\"a\":1}\n\n{bad\n{\"a\":null}\n{}\n{\"a\":\"x\"}\n",
    );
    let mut eng = Engine::open(root).unwrap();
    let exists_report = run(&mut eng, &req(&["*.jsonl"], exists("/a"), limits()));
    assert_eq!(exists_report.coverage.skipped, 2);
    assert_eq!(exists_report.coverage.records, 5);
    assert_eq!(exists_report.coverage.matched, 3);
    assert_eq!(exists_report.coverage.unmatched, 1);
    assert_eq!(exists_report.coverage.unresolved, 1);
    assert!(
        exists_report
            .items
            .iter()
            .any(|item| item.matched.is_none() && item.reason.as_deref() == Some("malformed json"))
    );
    assert_eq!(
        exists_report
            .items
            .iter()
            .find(|i| i.source.line == 3)
            .map(|i| i.source.line),
        None
    );
    let lines: Vec<_> = exists_report.items.iter().map(|i| i.source.line).collect();
    assert!(lines.contains(&2));
    assert!(lines.contains(&5));

    let eq_null = run(
        &mut eng,
        &req(
            &["*.jsonl"],
            Predicate::Eq {
                path: "/a".into(),
                value: Value::Null,
            },
            limits(),
        ),
    );
    assert_eq!(eq_null.coverage.matched, 1);
    assert!(eq_null.coverage.unresolved >= 2);
    assert!(
        eq_null
            .items
            .iter()
            .any(|item| item.matched == Some(true) && item.value == Some(json!({"a": null})))
    );

    let gt = run(
        &mut eng,
        &req(
            &["*.jsonl"],
            Predicate::Gt {
                path: "/a".into(),
                value: 0.0,
            },
            limits(),
        ),
    );
    assert!(gt.coverage.unresolved >= 3);
    assert_eq!(gt.coverage.matched, 1);
    assert!(
        gt.items
            .iter()
            .any(|item| item.reason.as_deref() == Some("type mismatch"))
    );

    let kind = run(
        &mut eng,
        &req(
            &["*.jsonl"],
            Predicate::Kind {
                path: "/a".into(),
                kind: JsonKind::Null,
            },
            limits(),
        ),
    );
    assert_eq!(kind.coverage.matched, 1);
    assert!(kind.coverage.unmatched >= 1);

    let warm = run(&mut eng, &req(&["*.jsonl"], exists("/a"), limits()));
    assert_eq!(warm.coverage.cache_hits, exists_report.coverage.records);
    assert_eq!(warm.coverage.cache_misses, 0);
}

#[test]
fn three_valued_all_any_not_and_pointer_escape() {
    let dir = ws();
    let root = dir.path();
    write(&root.join("rows.jsonl"), "{\"a\":1,\"a/b\":true}\n");
    let mut eng = Engine::open(root).unwrap();
    let row = || req(&["*.jsonl"], exists("/a"), limits());
    let _ = row;

    let all_false = run(
        &mut eng,
        &req(
            &["*.jsonl"],
            Predicate::All {
                predicates: vec![
                    Predicate::Eq {
                        path: "/missing".into(),
                        value: json!(1),
                    },
                    exists("/nope"),
                ],
            },
            limits(),
        ),
    );
    assert_eq!(all_false.coverage.matched, 0);
    assert_eq!(all_false.coverage.unmatched, 1);
    assert!(all_false.items.is_empty());

    let all_unknown = run(
        &mut eng,
        &req(
            &["*.jsonl"],
            Predicate::All {
                predicates: vec![
                    Predicate::Eq {
                        path: "/missing".into(),
                        value: json!(1),
                    },
                    exists("/a"),
                ],
            },
            limits(),
        ),
    );
    assert_eq!(all_unknown.coverage.unresolved, 1);
    assert_eq!(all_unknown.items[0].matched, None);

    let any_true = run(
        &mut eng,
        &req(
            &["*.jsonl"],
            Predicate::Any {
                predicates: vec![
                    Predicate::Eq {
                        path: "/missing".into(),
                        value: json!(1),
                    },
                    exists("/a"),
                ],
            },
            limits(),
        ),
    );
    assert_eq!(any_true.coverage.matched, 1);

    let any_unknown = run(
        &mut eng,
        &req(
            &["*.jsonl"],
            Predicate::Any {
                predicates: vec![
                    Predicate::Eq {
                        path: "/missing".into(),
                        value: json!(1),
                    },
                    exists("/nope"),
                ],
            },
            limits(),
        ),
    );
    assert_eq!(any_unknown.coverage.unresolved, 1);

    let not_false = run(
        &mut eng,
        &req(
            &["*.jsonl"],
            Predicate::Not {
                predicate: Box::new(exists("/nope")),
            },
            limits(),
        ),
    );
    assert_eq!(not_false.coverage.matched, 1);

    let not_unknown = run(
        &mut eng,
        &req(
            &["*.jsonl"],
            Predicate::Not {
                predicate: Box::new(Predicate::Eq {
                    path: "/missing".into(),
                    value: json!(1),
                }),
            },
            limits(),
        ),
    );
    assert_eq!(not_unknown.coverage.unresolved, 1);

    let escaped = run(&mut eng, &req(&["*.jsonl"], exists("/a~1b"), limits()));
    assert_eq!(escaped.coverage.matched, 1);

    let regex = run(
        &mut eng,
        &req(
            &["*.jsonl"],
            Predicate::Regex {
                path: "/missing".into(),
                pattern: "a+".into(),
            },
            limits(),
        ),
    );
    assert_eq!(regex.coverage.unresolved, 1);
    write(
        &root.join("text.jsonl"),
        "{\"name\":\"hello-9\"}\n{\"name\":1}\n",
    );
    let matched = run(
        &mut eng,
        &req(
            &["text.jsonl"],
            Predicate::Regex {
                path: "/name".into(),
                pattern: "^hello-\\d+$".into(),
            },
            limits(),
        ),
    );
    assert_eq!(matched.coverage.matched, 1);
    assert_eq!(matched.coverage.unresolved, 1);
}

#[test]
fn exact_number_equality_avoids_lossy_integers() {
    let dir = ws();
    let root = dir.path();
    write(
        &root.join("nums.jsonl"),
        "{\"a\":9007199254740993}\n{\"a\":1.0}\n",
    );
    let mut eng = Engine::open(root).unwrap();
    let exact = run(
        &mut eng,
        &req(
            &["nums.jsonl"],
            Predicate::Eq {
                path: "/a".into(),
                value: json!(9007199254740993i64),
            },
            limits(),
        ),
    );
    assert_eq!(exact.coverage.matched, 1);
    assert_eq!(exact.coverage.unmatched, 1);

    let neighbor = run(
        &mut eng,
        &req(
            &["nums.jsonl"],
            Predicate::Eq {
                path: "/a".into(),
                value: json!(9007199254740992i64),
            },
            limits(),
        ),
    );
    assert_eq!(neighbor.coverage.matched, 0);

    let rounded = serde_json::Number::from_f64(9007199254740993.0).unwrap();
    let lossy = run(
        &mut eng,
        &req(
            &["nums.jsonl"],
            Predicate::Eq {
                path: "/a".into(),
                value: Value::Number(rounded),
            },
            limits(),
        ),
    );
    assert_eq!(lossy.coverage.matched, 0);

    let one = run(
        &mut eng,
        &req(
            &["nums.jsonl"],
            Predicate::Eq {
                path: "/a".into(),
                value: json!(1),
            },
            limits(),
        ),
    );
    assert_eq!(one.coverage.matched, 1);

    let gt = run(
        &mut eng,
        &req(
            &["nums.jsonl"],
            Predicate::Gt {
                path: "/a".into(),
                value: 0.0,
            },
            limits(),
        ),
    );
    assert_eq!(gt.coverage.matched, 2);
}

#[test]
fn same_size_and_mtime_edit_is_rehashed() {
    let dir = ws();
    let root = dir.path();
    let kept = root.join("kept.jsonl");
    let changed = root.join("changed.jsonl");
    write(&kept, "{\"k\":\"aa\"}\n");
    write(&changed, "{\"k\":\"zz\"}\n");
    let mtime = FileTime::from_unix_time(1_700_000_000, 0);
    set_file_mtime(&changed, mtime).unwrap();
    let mut eng = Engine::open(root).unwrap();
    let request = req(&["*.jsonl"], exists("/k"), limits());
    let first = run(&mut eng, &request);
    assert_eq!(first.coverage.cache_misses, 2);
    write(&changed, "{\"k\":\"bb\"}\n");
    set_file_mtime(&changed, mtime).unwrap();
    let summary = eng.reconcile().unwrap();
    assert!(summary["sources_changed"].as_i64().unwrap() >= 1);
    assert!(summary["reports_invalidated"].as_i64().unwrap() >= 1);
    let stale = eng.evidence(&first.id).unwrap().unwrap();
    assert_eq!(stale.freshness, "stale");
    let second = run(&mut eng, &request);
    assert_eq!(second.coverage.cache_hits, 1);
    assert_eq!(second.coverage.cache_misses, 1);
    assert_eq!(second.freshness, "validated");
}

#[test]
fn additions_deletions_renames_and_ignore_rules() {
    let dir = ws();
    let root = dir.path();
    write(&root.join("a.jsonl"), "{\"a\":1}\n");
    write(&root.join("b.jsonl"), "{\"a\":1}\n");
    let mut eng = Engine::open(root).unwrap();
    let request = req(&["*.jsonl", "**/*.jsonl"], exists("/a"), limits());
    let first = run(&mut eng, &request);
    assert_eq!(first.coverage.files, 2);

    write(&root.join("c.jsonl"), "{\"a\":1}\n");
    let added = eng.evidence(&first.id).unwrap().unwrap();
    assert_eq!(added.freshness, "stale");
    let with_c = run(&mut eng, &request);
    assert_eq!(with_c.coverage.files, 3);
    assert_eq!(with_c.freshness, "validated");

    fs::remove_file(root.join("c.jsonl")).unwrap();
    assert_eq!(
        eng.evidence(&with_c.id).unwrap().unwrap().freshness,
        "stale"
    );

    fs::rename(root.join("b.jsonl"), root.join("d.jsonl")).unwrap();
    assert_eq!(eng.evidence(&first.id).unwrap().unwrap().freshness, "stale");
    let renamed = run(&mut eng, &request);
    assert!(renamed.sources.iter().any(|s| s.path == "d.jsonl"));
    assert!(renamed.sources.iter().all(|s| s.path != "b.jsonl"));
    assert_ne!(renamed.generation, first.generation);

    write(&root.join(".gitignore"), "d.jsonl\n");
    assert_eq!(
        eng.evidence(&renamed.id).unwrap().unwrap().freshness,
        "stale"
    );
    let ignored = run(&mut eng, &request);
    assert!(ignored.sources.iter().all(|s| s.path != "d.jsonl"));
    assert_eq!(ignored.coverage.files, 1);

    let bare = ws();
    let bare_root = bare.path();
    write(&bare_root.join(".ignore"), "skip.jsonl\n");
    write(&bare_root.join("skip.jsonl"), "{\"a\":1}\n");
    write(&bare_root.join("keep.jsonl"), "{\"a\":1}\n");
    assert!(!bare_root.join(".git").exists());
    let mut bare_eng = Engine::open(bare_root).unwrap();
    let kept = run(&mut bare_eng, &req(&["*.jsonl"], exists("/a"), limits()));
    assert_eq!(kept.coverage.files, 1);
    assert_eq!(kept.sources[0].path, "keep.jsonl");
}

#[test]
fn glob_escape_and_symlink_are_rejected() {
    let dir = ws();
    let root = dir.path();
    write(&root.join("real.jsonl"), "{\"a\":1}\n");
    write(&root.join("sub/nested.jsonl"), "{\"a\":2}\n");
    let outside = ws();
    write(&outside.path().join("secret.jsonl"), "{\"a\":\"SECRET\"}\n");
    std::os::unix::fs::symlink(root.join("real.jsonl"), root.join("link.jsonl")).unwrap();
    std::os::unix::fs::symlink(outside.path().join("secret.jsonl"), root.join("out.jsonl"))
        .unwrap();
    let mut eng = Engine::open(root).unwrap();
    let cancel = AtomicBool::new(false);
    let escaped = eng.check(&req(&["../secret.jsonl"], exists("/a"), limits()), &cancel);
    assert!(
        escaped
            .unwrap_err()
            .to_string()
            .contains("escapes workspace")
    );
    let absolute = eng.check(&req(&["/etc/passwd"], exists("/a"), limits()), &cancel);
    assert!(
        absolute
            .unwrap_err()
            .to_string()
            .contains("must be relative")
    );
    let dotted = eng.check(
        &req(&["sub/../../secret.jsonl"], exists("/a"), limits()),
        &cancel,
    );
    assert!(
        dotted
            .unwrap_err()
            .to_string()
            .contains("escapes workspace")
    );

    let report = run(
        &mut eng,
        &req(&["*.jsonl", "**/*.jsonl"], exists("/a"), limits()),
    );
    assert_eq!(report.coverage.files, 2);
    assert!(report.sources.iter().any(|s| s.path == "real.jsonl"));
    assert!(report.sources.iter().any(|s| s.path == "sub/nested.jsonl"));
    assert!(report.items.iter().all(|item| {
        item.value
            .as_ref()
            .and_then(|v| v.get("a"))
            .and_then(|v| v.as_str())
            != Some("SECRET")
    }));
    write(
        &root.join(".checkweave/hidden.jsonl"),
        "{\"a\":\"SECRET\"}\n",
    );
    write(&root.join(".git/hidden.jsonl"), "{\"a\":\"SECRET\"}\n");
    let hidden = run(
        &mut eng,
        &req(&["**/*.jsonl", "*.jsonl"], exists("/a"), limits()),
    );
    assert_eq!(hidden.coverage.files, 2);
}

#[test]
fn limits_partial_cancel_and_predicate_validation() {
    let dir = ws();
    let root = dir.path();
    write(&root.join("a.jsonl"), "{\"a\":1}\n");
    write(&root.join("b.jsonl"), "{\"a\":1}\n{\"a\":1}\n{\"a\":1}\n");
    write(
        &root.join("c.jsonl"),
        &format!("{}\n", "{\"a\":1,\"pad\":\"xxxxxxxx\"}"),
    );
    let mut eng = Engine::open(root).unwrap();
    let few_files = run(
        &mut eng,
        &req(
            &["*.jsonl"],
            exists("/a"),
            Limits {
                max_files: 1,
                ..limits()
            },
        ),
    );
    assert_eq!(few_files.execution, "partial");
    assert_eq!(few_files.coverage.files, 1);
    assert_eq!(few_files.sources[0].path, "a.jsonl");
    assert!(few_files.warnings.iter().any(|w| w.contains("max_files")));
    assert!(few_files.truncated);

    let few_records = run(
        &mut eng,
        &req(
            &["b.jsonl"],
            exists("/a"),
            Limits {
                max_records: 2,
                ..limits()
            },
        ),
    );
    assert_eq!(few_records.execution, "partial");
    assert_eq!(few_records.coverage.records, 2);
    assert!(
        few_records
            .warnings
            .iter()
            .any(|w| w.contains("max_records"))
    );

    let small = fs::metadata(root.join("a.jsonl")).unwrap().len();
    let few_bytes = run(
        &mut eng,
        &req(
            &["*.jsonl"],
            exists("/a"),
            Limits {
                max_bytes: small,
                ..limits()
            },
        ),
    );
    assert_eq!(few_bytes.execution, "partial");
    assert!(few_bytes.coverage.files >= 1);
    assert!(few_bytes.warnings.iter().any(|w| w.contains("max_bytes")));
    assert!(few_bytes.coverage.files < 3);

    let capped = run(
        &mut eng,
        &req(
            &["b.jsonl"],
            exists("/a"),
            Limits {
                max_results: 1,
                ..limits()
            },
        ),
    );
    assert_eq!(capped.execution, "complete");
    assert_eq!(capped.coverage.matched, 3);
    assert_eq!(capped.items.len(), 1);
    assert!(capped.truncated);
    assert!(capped.warnings.iter().any(|w| w.contains("max_results")));

    let cancel = AtomicBool::new(true);
    let before = eng.stats().unwrap()["reports"].as_i64().unwrap();
    let cancelled = eng
        .check(&req(&["*.jsonl"], exists("/a"), limits()), &cancel)
        .unwrap();
    assert_eq!(cancelled.execution, "cancelled");
    assert_ne!(cancelled.execution, "complete");
    assert!(cancelled.warnings.iter().any(|w| w.contains("cancelled")));
    assert!(eng.evidence(&cancelled.id).unwrap().is_none());
    assert_eq!(eng.stats().unwrap()["reports"].as_i64().unwrap(), before);

    let immediate = run(
        &mut eng,
        &req(
            &["*.jsonl"],
            exists("/a"),
            Limits {
                timeout_ms: 0,
                ..limits()
            },
        ),
    );
    assert_eq!(immediate.execution, "partial");
    assert_eq!(immediate.coverage.files, 0);
    assert!(immediate.warnings.iter().any(|w| w.contains("timeout")));

    let cancel = AtomicBool::new(false);
    let bad_files = eng.check(
        &req(
            &["*.jsonl"],
            exists("/a"),
            Limits {
                max_files: usize::MAX,
                ..limits()
            },
        ),
        &cancel,
    );
    assert!(bad_files.unwrap_err().to_string().contains("hard cap"));
    let nan = eng.check(
        &req(
            &["*.jsonl"],
            Predicate::Gt {
                path: "/a".into(),
                value: f64::NAN,
            },
            limits(),
        ),
        &cancel,
    );
    assert!(nan.unwrap_err().to_string().contains("non-finite"));
    let inf = eng.check(
        &req(
            &["*.jsonl"],
            Predicate::Le {
                path: "/a".into(),
                value: f64::INFINITY,
            },
            limits(),
        ),
        &cancel,
    );
    assert!(inf.unwrap_err().to_string().contains("non-finite"));
    let pointer = eng.check(
        &req(
            &["*.jsonl"],
            Predicate::Exists { path: "a".into() },
            limits(),
        ),
        &cancel,
    );
    assert!(pointer.unwrap_err().to_string().contains("JSON pointer"));
    let regex = eng.check(
        &req(
            &["*.jsonl"],
            Predicate::Regex {
                path: "/a".into(),
                pattern: "(".into(),
            },
            limits(),
        ),
        &cancel,
    );
    assert!(regex.unwrap_err().to_string().contains("invalid regex"));
    let mut deep = exists("/a");
    for _ in 0..40 {
        deep = Predicate::Not {
            predicate: Box::new(deep),
        };
    }
    let nested = eng.check(&req(&["*.jsonl"], deep, limits()), &cancel);
    assert!(nested.unwrap_err().to_string().contains("maximum depth"));
}

#[test]
fn evidence_missing_and_stale_after_edit() {
    let dir = ws();
    let root = dir.path();
    write(&root.join("rows.jsonl"), "{\"a\":1}\n");
    let mut eng = Engine::open(root).unwrap();
    assert!(eng.evidence("missing").unwrap().is_none());
    let report = run(&mut eng, &req(&["*.jsonl"], exists("/a"), limits()));
    let current = eng.evidence(&report.id).unwrap().unwrap();
    assert_eq!(current.freshness, "validated");
    assert_eq!(current.items[0].value, Some(json!({"a": 1})));
    write(&root.join("rows.jsonl"), "{\"a\":2}\n");
    let stale = eng.evidence(&report.id).unwrap().unwrap();
    assert_eq!(stale.freshness, "stale");
    assert_eq!(stale.coverage.matched, 1);
}

#[test]
fn empty_file_and_no_match_coverage() {
    let dir = ws();
    let root = dir.path();
    write(&root.join("empty.jsonl"), "");
    write(&root.join("blanks.jsonl"), "\n\n  \n");
    let mut eng = Engine::open(root).unwrap();
    let empty = run(&mut eng, &req(&["empty.jsonl"], exists("/a"), limits()));
    assert_eq!(empty.execution, "complete");
    assert_eq!(empty.freshness, "validated");
    assert_eq!(empty.coverage.files, 1);
    assert_eq!(empty.coverage.records, 0);
    assert!(empty.items.is_empty());
    assert_eq!(empty.sources.len(), 1);

    let blanks = run(&mut eng, &req(&["blanks.jsonl"], exists("/a"), limits()));
    assert_eq!(blanks.coverage.files, 1);
    assert_eq!(blanks.coverage.records, 0);
    assert!(blanks.coverage.skipped >= 2);
    assert!(blanks.items.is_empty());

    let none = run(&mut eng, &req(&["nope/*.jsonl"], exists("/a"), limits()));
    assert_eq!(none.execution, "complete");
    assert_eq!(none.coverage.files, 0);
    assert_eq!(none.coverage.records, 0);
    assert!(none.sources.is_empty());

    let vacant = run(&mut eng, &req(&[], exists("/a"), limits()));
    assert_eq!(vacant.coverage.files, 0);
    assert_eq!(vacant.execution, "complete");
}

#[test]
fn reopen_keeps_cache_and_corrupt_db_is_quarantined() {
    let dir = ws();
    let root = dir.path();
    write(&root.join("rows.jsonl"), "{\"a\":1}\n{\"a\":0}\n");
    let request = req(&["*.jsonl"], exists("/a"), limits());
    let id = {
        let mut eng = Engine::open(root).unwrap();
        let cold = run(&mut eng, &request);
        assert_eq!(cold.coverage.cache_misses, 2);
        cold.id
    };
    {
        let mut eng = Engine::open(root).unwrap();
        let warm = run(&mut eng, &request);
        assert_eq!(warm.coverage.cache_hits, 2);
        assert_eq!(warm.coverage.cache_misses, 0);
        let again = eng.evidence(&id).unwrap().unwrap();
        assert_eq!(again.freshness, "validated");
        let stats = eng.stats().unwrap();
        assert!(stats["items"].as_i64().unwrap() >= 2);
        assert!(stats["reports"].as_i64().unwrap() >= 1);
    }
    let db = root.join(".checkweave/cache.sqlite");
    let _ = fs::remove_file(PathBuf::from(format!("{}-wal", db.display())));
    let _ = fs::remove_file(PathBuf::from(format!("{}-shm", db.display())));
    fs::write(&db, b"this is not a sqlite database").unwrap();
    let mut eng = Engine::open(root).unwrap();
    let stats = eng.stats().unwrap();
    assert_eq!(stats["items"], 0);
    assert_eq!(stats["reports"], 0);
    let mut quarantined = false;
    for entry in fs::read_dir(root.join(".checkweave")).unwrap() {
        let name = entry.unwrap().file_name().to_string_lossy().to_string();
        if name.contains("corrupt") {
            quarantined = true;
        }
    }
    assert!(quarantined);
    let rebuilt = run(&mut eng, &request);
    assert_eq!(rebuilt.execution, "complete");
    assert_eq!(rebuilt.coverage.cache_misses, 2);
    assert_eq!(rebuilt.freshness, "validated");
}

#[test]
fn object_equality_is_order_independent_and_contains() {
    let dir = ws();
    let root = dir.path();
    write(
        &root.join("rows.jsonl"),
        "{\"a\":{\"z\":1,\"y\":[true]}}\n{\"a\":\"hello\"}\n",
    );
    let mut eng = Engine::open(root).unwrap();
    let eq = run(
        &mut eng,
        &req(
            &["*.jsonl"],
            Predicate::Eq {
                path: "/a".into(),
                value: json!({"y": [true], "z": 1}),
            },
            limits(),
        ),
    );
    assert_eq!(eq.coverage.matched, 1);
    assert_eq!(eq.coverage.unmatched, 1);
    let contains = run(
        &mut eng,
        &req(
            &["*.jsonl"],
            Predicate::Contains {
                path: "/a".into(),
                value: "ell".into(),
            },
            limits(),
        ),
    );
    assert_eq!(contains.coverage.matched, 1);
    assert_eq!(contains.coverage.unresolved, 1);
}

#[test]
fn global_budgets_span_files_and_reuse_truncated_rows() {
    let dir = ws();
    let root = dir.path();
    write(&root.join("a.jsonl"), "{\"a\":1}\n{\"a\":2}\n{\"a\":3}\n");
    write(&root.join("b.jsonl"), "{\"a\":4}\n{\"a\":5}\n{\"a\":6}\n");
    let mut eng = Engine::open(root).unwrap();
    let capped = req(
        &["*.jsonl"],
        exists("/a"),
        Limits {
            max_records: 4,
            max_results: 1,
            ..limits()
        },
    );
    let partial = run(&mut eng, &capped);
    assert_eq!(partial.execution, "partial");
    assert!(partial.coverage.records <= 4, "{:?}", partial.coverage);
    assert!(partial.coverage.evaluated <= 4, "{:?}", partial.coverage);
    assert_eq!(
        partial.coverage.matched + partial.coverage.unmatched + partial.coverage.unresolved,
        partial.coverage.evaluated
    );
    assert!(partial.items.len() <= 1, "{:?}", partial.items);
    assert!(partial.truncated);
    assert!(partial.coverage.skipped <= 4);
    let misses = partial.coverage.cache_misses;
    assert!(misses <= 4);

    let full = run(&mut eng, &req(&["*.jsonl"], exists("/a"), limits()));
    assert_eq!(full.execution, "complete");
    assert_eq!(full.coverage.records, 6);
    assert_eq!(full.coverage.matched, 6);
    assert!(full.coverage.cache_hits >= misses);
    assert_eq!(full.coverage.cache_hits + full.coverage.cache_misses, 6);
    assert_eq!(full.coverage.cache_misses, 6 - misses);
}

#[test]
fn out_of_range_integers_stay_unresolved_and_invalid_utf8_is_counted() {
    let dir = ws();
    let root = dir.path();
    write(
        &root.join("nums.jsonl"),
        "{\"n\":18446744073709551616}\n{\"n\":18446744073709551617}\n{\"n\":1}\n",
    );
    let mut raw = b"\n{\"a\":1}\n".to_vec();
    raw.extend_from_slice(&[0xff, 0xfe, b'\n']);
    raw.extend_from_slice(b"{nope\n");
    fs::write(root.join("messy.jsonl"), raw).unwrap();
    let mut eng = Engine::open(root).unwrap();
    let rounded = serde_json::from_str::<Value>("{\"n\":18446744073709551616}").unwrap();
    let huge = run(
        &mut eng,
        &req(
            &["nums.jsonl"],
            Predicate::Eq {
                path: "/n".into(),
                value: rounded["n"].clone(),
            },
            limits(),
        ),
    );
    assert_eq!(huge.coverage.matched, 0, "{huge:?}");
    assert!(
        huge.coverage.unresolved >= 2,
        "distinct huge integers must not compare equal through f64 rounding: {huge:?}"
    );
    assert!(
        huge.items
            .iter()
            .any(|item| item.reason.as_deref() == Some("out-of-range integer"))
    );

    let messy = run(&mut eng, &req(&["messy.jsonl"], exists("/a"), limits()));
    assert!(messy.coverage.skipped >= 1, "{messy:?}");
    assert!(messy.coverage.unresolved >= 2, "{messy:?}");
    assert!(
        messy
            .items
            .iter()
            .any(|item| item.reason.as_deref() == Some("invalid utf-8"))
    );
    assert!(
        messy
            .items
            .iter()
            .any(|item| item.reason.as_deref() == Some("malformed json"))
    );
    assert_eq!(
        messy.coverage.matched + messy.coverage.unmatched + messy.coverage.unresolved,
        messy.coverage.evaluated
    );
}

#[test]
fn payload_budget_reclaims_old_evidence() {
    let dir = ws();
    let root = dir.path();
    write(&root.join("small.jsonl"), "{\"a\":1}\n");
    let mut eng = Engine::open(root).unwrap();
    let first = run(&mut eng, &req(&["small.jsonl"], exists("/a"), limits()));
    let pad = "x".repeat(30_000);
    let mut body = String::new();
    for i in 0..2400 {
        body.push_str(&format!("{{\"a\":{i},\"p\":\"{pad}\"}}\n"));
    }
    write(&root.join("wide.jsonl"), &body);
    let huge = run(
        &mut eng,
        &req(
            &["wide.jsonl"],
            exists("/p"),
            Limits {
                max_bytes: 200_000_000,
                max_records: 10_000,
                max_results: 1,
                max_files: 10,
                timeout_ms: 180_000,
            },
        ),
    );
    assert_eq!(huge.execution, "complete");
    assert_eq!(huge.coverage.records, 2400);
    assert_eq!(huge.items.len(), 1);
    let stats = eng.stats().unwrap();
    assert!(stats["payload_bytes"].as_i64().unwrap() <= 64 * 1024 * 1024);
    assert!(stats["items"].as_i64().unwrap() < 2400);
    assert!(eng.evidence(&first.id).unwrap().is_none());
    assert!(eng.evidence(&huge.id).unwrap().is_some());
    let _ = Duration::from_millis(1);
}

fn assert_ipc_fits(report: &CheckReport) {
    let report_len = serde_json::to_vec(report).unwrap().len();
    assert!(
        report_len <= REPORT_JSON_BUDGET,
        "report {report_len} exceeds {REPORT_JSON_BUDGET}"
    );
    let direct = WireResponse::success(report).unwrap();
    let direct_len = serde_json::to_vec(&direct).unwrap().len();
    assert!(
        direct_len < IPC_FRAME_BYTES,
        "wire {direct_len} exceeds {IPC_FRAME_BYTES}"
    );
    let snapshot = RunSnapshot {
        id: report.id.clone(),
        state: RunState::Complete,
        operation: "check".into(),
        detail: None,
        result: Some(serde_json::to_value(report).unwrap()),
        error: None,
    };
    let wrapped = WireResponse::success(&snapshot).unwrap();
    let wrapped_len = serde_json::to_vec(&wrapped).unwrap().len();
    assert!(
        wrapped_len < IPC_FRAME_BYTES,
        "wrapped {wrapped_len} exceeds {IPC_FRAME_BYTES}"
    );
}

#[test]
fn report_budget_bounds_large_values_and_escaped_paths() {
    let dir = ws();
    let root = dir.path();
    let name = "q\"u\\ote.jsonl";
    let escaped = format!(
        "{}{}{}",
        "a".repeat(20_000),
        "\\\"".repeat(1500),
        "\\\\".repeat(1500)
    );
    let mut body = String::new();
    for _ in 0..260 {
        body.push_str(&format!("{{\"k\":\"{escaped}\"}}\n"));
    }
    write(&root.join(name), &body);
    let mut eng = Engine::open(root).unwrap();
    let wide = run(
        &mut eng,
        &req(
            &["*.jsonl"],
            exists("/k"),
            Limits {
                max_results: 5_000,
                max_bytes: 32 * 1024 * 1024,
                max_records: 10_000,
                timeout_ms: 60_000,
                ..Limits::default()
            },
        ),
    );
    assert_eq!(wide.execution, "complete");
    assert_eq!(wide.freshness, "validated");
    assert_eq!(wide.coverage.files, 1);
    assert_eq!(wide.sources.len(), 1);
    assert_eq!(wide.coverage.records, 260);
    assert_eq!(wide.coverage.evaluated, 260);
    assert_eq!(
        wide.coverage.matched + wide.coverage.unmatched + wide.coverage.unresolved,
        wide.coverage.evaluated
    );
    assert_eq!(wide.coverage.matched, 260);
    assert!(wide.truncated);
    assert!(
        wide.warnings
            .iter()
            .any(|warning| warning.contains("response byte budget"))
    );
    assert!(wide.items.len() <= wide.coverage.matched);
    assert!(wide.items.iter().any(|item| item.value.is_some()));
    assert!(wide.items.iter().any(|item| {
        item.value.is_none() && item.reason.as_deref() == Some("value omitted: response budget")
    }));
    assert!(wide.items.iter().all(|item| item.source.path == name));
    assert_ipc_fits(&wide);
    let again = eng.evidence(&wide.id).unwrap().unwrap();
    assert_eq!(again.freshness, "validated");
    assert_eq!(again.coverage.matched, wide.coverage.matched);
    assert_ipc_fits(&again);
    #[cfg(unix)]
    escaped_paths_stop_at_report_metadata_budget();
}

/// Backslashes double when JSON-escaped. Windows cannot put them in names.
#[cfg(unix)]
fn escaped_paths_stop_at_report_metadata_budget() {
    // PATH_MAX is 4096 on Linux and 1024 on macOS.
    let levels = if cfg!(target_os = "linux") { 15 } else { 4 };
    let meta = ws();
    let root = meta.path();
    let mut dir = root.to_path_buf();
    for depth in 0..levels {
        dir.push(format!("d{depth:02}_{}", "\\".repeat(180)));
    }
    fs::create_dir_all(&dir).unwrap();
    let escaped = 2 * dir.strip_prefix(root).unwrap().as_os_str().len();
    let created = (2 * REPORT_JSON_BUDGET / escaped).max(1_300);
    for index in 0..created {
        fs::write(dir.join(format!("{index}.jsonl")), b"{\"a\":1}\n").unwrap();
    }
    let mut eng = Engine::open(root).unwrap();
    let partial = run(
        &mut eng,
        &req(
            &["**/*.jsonl"],
            exists("/a"),
            Limits {
                max_files: 100_000,
                max_bytes: 32 * 1024 * 1024,
                max_records: 100_000,
                max_results: 5_000,
                timeout_ms: 120_000,
            },
        ),
    );
    assert_eq!(partial.execution, "partial");
    assert_eq!(partial.freshness, "validated");
    assert!(partial.truncated);
    assert!(partial.coverage.files > 0);
    assert!(partial.coverage.files < created);
    assert_eq!(partial.sources.len(), partial.coverage.files);
    assert_eq!(partial.coverage.records, partial.coverage.files);
    assert_eq!(partial.coverage.evaluated, partial.coverage.records);
    assert_eq!(partial.coverage.matched, partial.coverage.records);
    assert_eq!(partial.coverage.unmatched, 0);
    assert_eq!(partial.coverage.unresolved, 0);
    assert!(
        partial
            .warnings
            .iter()
            .any(|warning| warning == "stopped: report metadata budget")
    );
    assert!(partial.sources.iter().all(|src| src.path.contains('\\')));
    assert!(partial.items.len() <= partial.coverage.matched);
    assert_ipc_fits(&partial);
    let stored = eng.evidence(&partial.id).unwrap().unwrap();
    assert_eq!(stored.freshness, "validated");
    assert_eq!(stored.sources.len(), partial.sources.len());
    assert_ipc_fits(&stored);
}
