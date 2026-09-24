#!/usr/bin/env python3
"""Grade one actor submission against the task fixtures.

Ground truth is derived by reading the fixtures and by running them.
experiments/agent-tasks/private/expected.json is a snapshot of that derivation
for audit. It is not copied into an actor workspace. The grader does not
require a particular tool sequence.
"""

from __future__ import annotations

import argparse
import importlib.util
import json
import shutil
import subprocess
import sys
import tempfile
import traceback
from pathlib import Path
from typing import Any

ROOT = Path(__file__).resolve().parent
FIXTURES = ROOT / "fixtures"
PRIVATE = ROOT / "private" / "expected.json"


def load_jsonl(path: Path) -> list[dict[str, Any]]:
    rows = []
    for line in path.read_text().splitlines():
        if line.strip():
            rows.append(json.loads(line))
    return rows


def policy_truth() -> dict[str, Any]:
    before = {row["id"]: row for row in load_jsonl(FIXTURES / "policy" / "before.jsonl")}
    after = {row["id"]: row for row in load_jsonl(FIXTURES / "policy" / "after.jsonl")}
    ids = sorted(set(before) | set(after))
    changes = []
    for key in ids:
        left = before.get(key)
        right = after.get(key)
        changed = left is None or right is None or left["action"] != right["action"] or left["risk"] != right["risk"]
        if not changed:
            continue
        changes.append(
            {
                "id": key,
                "before": None if left is None else {"action": left["action"], "risk": left["risk"]},
                "after": None if right is None else {"action": right["action"], "risk": right["risk"]},
            }
        )
    violations = []
    for key in sorted(after):
        row = after[key]
        if (row["action"] == "allow" and row["risk"] >= 70) or (row["action"] == "deny" and row["risk"] < 30):
            violations.append(key)
    return {
        "changed_count": len(changes),
        "changed_ids": [item["id"] for item in changes],
        "changes": changes,
        "after_violation_count": len(violations),
        "after_violation_ids": violations,
    }


def exception_truth() -> dict[str, Any]:
    proc = subprocess.run(
        [sys.executable, str(FIXTURES / "incident" / "main.py")],
        capture_output=True,
        text=True,
    )
    if proc.returncode == 0:
        raise RuntimeError("incident fixture exited 0; expected a failure")
    spec = importlib.util.spec_from_file_location("incident_main", FIXTURES / "incident" / "main.py")
    if spec is None or spec.loader is None:
        raise RuntimeError("cannot load incident main")
    module = importlib.util.module_from_spec(spec)
    sys.modules["incident_main"] = module
    incident_dir = str(FIXTURES / "incident")
    if incident_dir not in sys.path:
        sys.path.insert(0, incident_dir)
    try:
        spec.loader.exec_module(module)
        try:
            module.main()
        except Exception as exc:
            root = exc
            while root.__cause__ is not None:
                root = root.__cause__
            frame = traceback.extract_tb(root.__traceback__)[-1]
            line = Path(frame.filename).read_text().splitlines()[frame.lineno - 1]
            return {
                "exception_type": type(root).__name__,
                "origin_file": Path(frame.filename).name,
                "origin_line": frame.lineno,
                "source_line": line.strip(),
                "missing_key": "apac",
            }
    finally:
        sys.path = [item for item in sys.path if item != incident_dir]
        sys.modules.pop("incident_main", None)
        for name in ("loader", "normalize"):
            sys.modules.pop(name, None)
    raise RuntimeError(f"incident main did not raise; stderr={proc.stderr!r}")


def diff_example() -> dict[str, Any]:
    """One known differing input. Actors may submit a different input."""
    sys.path.insert(0, str(FIXTURES / "diff"))
    try:
        import billing_after
        import billing_before

        sample = [{"qty": -1, "cents": 25}]
        before = billing_before.invoice_cents(sample)
        after = billing_after.invoice_cents(sample)
    finally:
        sys.path = [item for item in sys.path if item != str(FIXTURES / "diff")]
        sys.modules.pop("billing_before", None)
        sys.modules.pop("billing_after", None)
    if before == after:
        raise RuntimeError("diff fixture does not differ on the known sample")
    return {"input": sample, "before": before, "after": after}


def derive_expected() -> dict[str, Any]:
    return {
        "policy": policy_truth(),
        "exception": exception_truth(),
        "diff_example": diff_example(),
    }


def grade_policy(submitted: dict[str, Any], truth: dict[str, Any]) -> list[str]:
    errors = []
    if submitted.get("changed_count") != truth["changed_count"]:
        errors.append(f"policy.changed_count {submitted.get('changed_count')} != {truth['changed_count']}")
    if submitted.get("changed_ids") != truth["changed_ids"]:
        errors.append("policy.changed_ids mismatch")
    if submitted.get("changes") != truth["changes"]:
        errors.append("policy.changes mismatch")
    if submitted.get("after_violation_count") != truth["after_violation_count"]:
        errors.append(
            f"policy.after_violation_count {submitted.get('after_violation_count')} != {truth['after_violation_count']}"
        )
    if submitted.get("after_violation_ids") != truth["after_violation_ids"]:
        errors.append("policy.after_violation_ids mismatch")
    return errors


def grade_exception(submitted: dict[str, Any], truth: dict[str, Any]) -> list[str]:
    errors = []
    if submitted.get("exception_type") != truth["exception_type"]:
        errors.append("exception.exception_type mismatch")
    if submitted.get("origin_file") != truth["origin_file"]:
        errors.append("exception.origin_file mismatch")
    if submitted.get("origin_line") != truth["origin_line"]:
        errors.append("exception.origin_line mismatch")
    evidence = submitted.get("source_evidence")
    if not isinstance(evidence, str) or len(evidence) < 10 or evidence not in truth["source_line"]:
        errors.append("exception.source_evidence is not a substring of the origin line")
    cause = submitted.get("cause")
    if not isinstance(cause, str) or truth["missing_key"] not in cause.lower():
        errors.append("exception.cause does not name the missing key")
    elif not any(word in cause.lower() for word in ("table", "map", "missing", "absent", "unknown", "not in")):
        errors.append("exception.cause does not describe the lookup failure")
    return errors


def grade_repro(repro: Path) -> list[str]:
    if not repro.is_file():
        return ["submission/repro.py is missing"]
    with tempfile.TemporaryDirectory(prefix="checkweave-repro-") as temp:
        work = Path(temp)
        # Match the brief: workspace root, TASK/, and submission/repro.py.
        # Canonical fixtures, not the actor tree, so a modified TASK cannot change the modules.
        shutil.copytree(FIXTURES, work / "TASK")
        submission = work / "submission"
        submission.mkdir()
        (submission / "repro.py").write_text(repro.read_text())
        proc = subprocess.run(
            [sys.executable, "submission/repro.py"],
            cwd=work,
            capture_output=True,
            text=True,
            timeout=15,
        )
    if proc.returncode != 0:
        return [f"repro.py exited {proc.returncode}: {proc.stderr.strip()[:400]}"]
    lines = [line for line in proc.stdout.splitlines() if line.strip()]
    if len(lines) != 1 or not lines[0].startswith("DIFF "):
        return ["repro.py must print exactly one DIFF line"]
    parts = lines[0].split()
    if len(parts) != 4:
        return ["DIFF line must be: DIFF <json> <before> <after>"]
    try:
        payload = json.loads(parts[1])
        before_printed = int(parts[2])
        after_printed = int(parts[3])
    except (ValueError, json.JSONDecodeError):
        return ["DIFF line has an invalid JSON array or integer"]
    if not isinstance(payload, list) or len(parts[1]) > 400:
        return ["DIFF json must be an array of at most 400 characters"]
    sys.path.insert(0, str(FIXTURES / "diff"))
    try:
        import billing_after
        import billing_before

        actual_before = billing_before.invoice_cents(payload)
        actual_after = billing_after.invoice_cents(payload)
    except Exception as exc:
        return [f"canonical invoice_cents rejected the repro input: {exc}"]
    finally:
        sys.path = [item for item in sys.path if item != str(FIXTURES / "diff")]
        sys.modules.pop("billing_before", None)
        sys.modules.pop("billing_after", None)
    errors = []
    if actual_before != before_printed or actual_after != after_printed:
        errors.append("printed totals do not match canonical invoice_cents")
    if actual_before == actual_after:
        errors.append("printed input does not show a difference")
    return errors


def grade(submission_dir: Path) -> dict[str, Any]:
    truth = derive_expected()
    snapshot_note = "private snapshot matches derivation"
    if PRIVATE.is_file():
        snapshot = json.loads(PRIVATE.read_text())
        if snapshot != truth:
            snapshot_note = "private snapshot differs from derivation; grader used derivation"
    else:
        snapshot_note = "private snapshot missing; grader used derivation"
    answers_path = submission_dir / "answers.json"
    errors: list[str] = []
    if not answers_path.is_file():
        errors.append("submission/answers.json is missing")
        answers: dict[str, Any] = {}
    else:
        try:
            answers = json.loads(answers_path.read_text())
        except json.JSONDecodeError as exc:
            errors.append(f"answers.json is not JSON: {exc}")
            answers = {}
    if isinstance(answers, dict):
        errors.extend(grade_policy(answers.get("policy") or {}, truth["policy"]))
        errors.extend(grade_exception(answers.get("exception") or {}, truth["exception"]))
    errors.extend(grade_repro(submission_dir / "repro.py"))
    return {
        "passed": not errors,
        "errors": errors,
        "snapshot": snapshot_note,
        "tasks": 3,
    }


def write_passing_submission(directory: Path, truth: dict[str, Any]) -> None:
    directory.mkdir(parents=True, exist_ok=True)
    sample = truth["diff_example"]
    payload = json.dumps(sample["input"], separators=(",", ":"))
    (directory / "repro.py").write_text(
        "import importlib.util\n"
        "import json\n"
        "from pathlib import Path\n"
        "root = Path(__file__).resolve().parents[1]\n"
        "def load(name, path):\n"
        "    spec = importlib.util.spec_from_file_location(name, path)\n"
        "    module = importlib.util.module_from_spec(spec)\n"
        "    spec.loader.exec_module(module)\n"
        "    return module\n"
        "before = load('billing_before', root / 'TASK' / 'diff' / 'billing_before.py')\n"
        "after = load('billing_after', root / 'TASK' / 'diff' / 'billing_after.py')\n"
        f"lines = json.loads({payload!r})\n"
        "print('DIFF ' + json.dumps(lines, separators=(',', ':')) + f' {before.invoice_cents(lines)} {after.invoice_cents(lines)}')\n"
    )
    exc = truth["exception"]
    answers = {
        "policy": truth["policy"],
        "exception": {
            "exception_type": exc["exception_type"],
            "origin_file": exc["origin_file"],
            "origin_line": exc["origin_line"],
            "source_evidence": exc["source_line"].strip(),
            "cause": "The region key apac is missing from the lookup table.",
        },
    }
    (directory / "answers.json").write_text(json.dumps(answers, indent=2) + "\n")


def self_test() -> None:
    truth = derive_expected()
    PRIVATE.parent.mkdir(parents=True, exist_ok=True)
    PRIVATE.write_text(json.dumps(truth, indent=2) + "\n")
    with tempfile.TemporaryDirectory(prefix="checkweave-grade-") as temp:
        good = Path(temp) / "good"
        write_passing_submission(good, truth)
        passed = grade(good)
        if not passed["passed"]:
            raise SystemExit(f"self-test passing submission failed: {passed}")
        bad = Path(temp) / "bad"
        write_passing_submission(bad, truth)
        answers = json.loads((bad / "answers.json").read_text())
        answers["policy"]["changed_count"] = 0
        (bad / "answers.json").write_text(json.dumps(answers))
        failed = grade(bad)
        if failed["passed"]:
            raise SystemExit("self-test expected a failing submission to fail")
    print(json.dumps({"self_test": "ok", "expected": str(PRIVATE)}, indent=2))


def main() -> None:
    parser = argparse.ArgumentParser(description="Validate an agent-task submission")
    parser.add_argument("--submission", type=Path, help="Directory containing answers.json and repro.py")
    parser.add_argument("--out", type=Path, default=None)
    parser.add_argument("--write-expected", action="store_true", help="Refresh private/expected.json")
    parser.add_argument("--self-test", action="store_true")
    args = parser.parse_args()
    if args.write_expected or args.self_test:
        if args.self_test:
            self_test()
            return
        truth = derive_expected()
        PRIVATE.parent.mkdir(parents=True, exist_ok=True)
        PRIVATE.write_text(json.dumps(truth, indent=2) + "\n")
        print(PRIVATE)
        return
    if args.submission is None:
        raise SystemExit("pass --submission or --self-test")
    report = grade(args.submission)
    text = json.dumps(report, indent=2) + "\n"
    if args.out is not None:
        args.out.parent.mkdir(parents=True, exist_ok=True)
        args.out.write_text(text)
    print(text, end="")
    if not report["passed"]:
        raise SystemExit(1)


if __name__ == "__main__":
    main()
