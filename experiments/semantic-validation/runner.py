"""Benchmark client for the Checkweave semantic worker.

This process does not load a model. It sends the stdio evaluation protocol and
scores the replies. Accuracy uses one question per request. A joint request is
only a coupling check. Batch throughput is not an accuracy source.

Scores copied from the worker are not treated as calibrated probabilities.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import queue
import subprocess
import sys
import threading
import time
from pathlib import Path
from typing import Any, Mapping, Optional

import baselines
import metrics
import sample_data

ROOT = Path(__file__).resolve().parents[2]
DEFAULT_PYTHON = Path("/tmp/checkweave-backends.ER6XYe/venv/bin/python")
PID_PATH = Path("/tmp/checkweave-development/python_worker.pid")
INBOX_PATH = Path("/tmp/checkweave-development/semantic_eval-inbox.md")
CONTRACT_PATH = Path("/tmp/checkweave-development/python-contract-notes.md")
PREDICATE_GOLD_PATH = Path(__file__).with_name("predicate_gold.json")
FRAME_LIMIT = 8 * 1024 * 1024
BATCH_CAP = 4


def sha256_bytes(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


def sha256_file(path: Path) -> str:
    return sha256_bytes(path.read_bytes())


def worker_module_present(root: Path = ROOT) -> bool:
    package = root / "python" / "checkweave_worker"
    return (package / "__main__.py").is_file() or (root / "python" / "checkweave_worker.py").is_file()


def living_pid(path: Path = PID_PATH) -> Optional[int]:
    try:
        pid = int(path.read_text(encoding="utf-8").strip())
    except (OSError, ValueError):
        return None
    try:
        os.kill(pid, 0)
    except OSError:
        return None
    return pid


def build_request(
    request_id: str,
    states: list[Mapping[str, str]],
    questions: list[Mapping[str, Any]],
    max_input_tokens: int = 4096,
) -> dict[str, Any]:
    payload = {
        "version": 1,
        "id": request_id,
        "op": "evaluate",
        "states": [{"id": state["id"], "text": state["text"]} for state in states],
        "questions": [sample_data.wire_question(question) for question in questions],
        "max_input_tokens": max_input_tokens,
    }
    encoded = json.dumps(payload, ensure_ascii=False).encode("utf-8")
    if len(encoded) + 1 > FRAME_LIMIT:
        raise ValueError(f"{request_id} exceeds the 8MiB frame limit")
    return payload


def attach(
    case: Mapping[str, Any],
    question: Mapping[str, Any],
    prediction: Mapping[str, Any],
    *,
    grade_unsupported_capability: bool = True,
) -> dict[str, Any]:
    expectation = question["expectation"]
    capability_graded = True
    if (
        expectation.get("type") == "capability"
        and expectation.get("status") == "unsupported"
        and not grade_unsupported_capability
    ):
        capability_graded = False
    return {
        "id": case["id"],
        "family": case["family"],
        "question_id": question["id"],
        "language": case.get("language"),
        "text_sha256": sha256_bytes(case["text"].encode("utf-8")),
        "text_chars": len(case["text"]),
        "expected_type": expectation["type"],
        "expected_status": expectation["status"],
        "expected_label": expectation.get("label"),
        "covered": bool(prediction.get("covered")),
        "status": prediction.get("status"),
        "label": prediction.get("label"),
        "selected_score": prediction.get("selected_score"),
        "scores": prediction.get("scores"),
        "request_error": prediction.get("request_error"),
        "reason": prediction.get("reason"),
        "value": prediction.get("value"),
        "value_derived": prediction.get("value_derived"),
        "score_semantics": prediction.get("score_semantics"),
        "token_count": prediction.get("token_count"),
        "latency_seconds": prediction.get("latency_seconds"),
        "question_kind": question.get("kind"),
        "capability_graded": capability_graded,
    }


def baseline_items(
    sample: Mapping[str, Any],
    predict,
    *,
    grade_unsupported_capability: bool = True,
) -> list[dict[str, Any]]:
    return [
        attach(
            case,
            question,
            predict(case, question),
            grade_unsupported_capability=grade_unsupported_capability,
        )
        for case, question in sample_data.iter_questions(sample)
    ]


def _selected_score(label: Optional[str], scores: Any) -> Optional[float]:
    if not label or not isinstance(scores, dict):
        return None
    score = scores.get(label)
    if isinstance(score, bool) or not isinstance(score, (int, float)):
        return None
    return float(score)


def predictions_from_response(
    cases: list[Mapping[str, Any]],
    questions_for_case: dict[str, list[Mapping[str, Any]]],
    response: Optional[Mapping[str, Any]],
    latency_seconds: Optional[float],
    *,
    grade_unsupported_capability: bool = True,
) -> list[dict[str, Any]]:
    rows = []
    error = None if response is None else response.get("error")
    by_key = {}
    if response and isinstance(response.get("results"), list):
        for result in response["results"]:
            if isinstance(result, dict):
                by_key[(result.get("state_id"), result.get("question_id"))] = result
    for case in cases:
        for question in questions_for_case[case["id"]]:
            result = by_key.get((case["id"], question["id"]))
            if result is None:
                prediction = {
                    "covered": False,
                    "status": None,
                    "label": None,
                    "request_error": error,
                    "latency_seconds": latency_seconds,
                }
            else:
                label = result.get("label")
                scores = result.get("scores")
                prediction = {
                    "covered": True,
                    "status": result.get("status"),
                    "label": label,
                    "scores": scores,
                    "selected_score": _selected_score(label, scores),
                    "score_semantics": result.get("score_semantics"),
                    "reason": result.get("reason"),
                    "value": result.get("value"),
                    "value_derived": result.get("value_derived"),
                    "token_count": result.get("token_count"),
                    "request_error": error,
                    "latency_seconds": latency_seconds,
                }
            rows.append(
                attach(
                    case,
                    question,
                    prediction,
                    grade_unsupported_capability=grade_unsupported_capability,
                )
            )
    return rows


class LineQueue:
    def __init__(self, stream):
        self.queue: queue.Queue = queue.Queue()
        self.thread = threading.Thread(target=self._read, args=(stream,), daemon=True)
        self.thread.start()

    def _read(self, stream) -> None:
        try:
            for line in stream:
                self.queue.put(line)
        finally:
            self.queue.put(None)

    def read_json(self, timeout: float) -> tuple[dict[str, Any], list[str]]:
        deadline = time.monotonic() + timeout
        noise: list[str] = []
        while True:
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                raise TimeoutError("timed out waiting for a protocol line")
            try:
                line = self.queue.get(timeout=remaining)
            except queue.Empty as exc:
                raise TimeoutError("timed out waiting for a protocol line") from exc
            if line is None:
                raise EOFError("worker closed stdout")
            if isinstance(line, bytes):
                line = line.decode("utf-8", errors="replace")
            text = line.strip()
            if not text:
                continue
            try:
                return json.loads(text), noise
            except json.JSONDecodeError:
                noise.append(text[:500])


def read_rss(pid: int) -> dict[str, Optional[int]]:
    rss = hwm = None
    try:
        status = Path(f"/proc/{pid}/status").read_text(encoding="utf-8")
    except OSError:
        return {"rss_kb": None, "hwm_kb": None}
    for line in status.splitlines():
        if line.startswith("VmRSS:"):
            rss = int(line.split()[1])
        elif line.startswith("VmHWM:"):
            hwm = int(line.split()[1])
    return {"rss_kb": rss, "hwm_kb": hwm}


class WorkerSession:
    def __init__(
        self,
        python: Path,
        device: str,
        threads: int,
        root: Path,
        backend: str = "gliner2",
        gguf: Optional[str] = None,
    ):
        env = os.environ.copy()
        env["PYTHONPATH"] = str(root / "python")
        env["HF_HUB_OFFLINE"] = "1"
        env["HF_HUB_DISABLE_IMPLICIT_TOKEN"] = "1"
        env["TOKENIZERS_PARALLELISM"] = "false"
        # An explicit CPU run should not initialize the host GPUs. CUDA init on
        # the Tesla M10s can stall the process before the ready frame.
        if device == "cpu":
            env["CUDA_VISIBLE_DEVICES"] = ""
        command = [
            str(python), "-m", "checkweave_worker",
            "--device", device,
            "--threads", str(threads),
            "--backend", backend,
            "--offline",
        ]
        if gguf:
            command.extend(["--gguf", gguf])
        self.command = command
        self.started = time.perf_counter()
        self.proc = subprocess.Popen(
            command,
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            cwd=root,
            env=env,
            bufsize=0,
        )
        assert self.proc.stdout is not None and self.proc.stdin is not None and self.proc.stderr is not None
        self.stdout = LineQueue(self.proc.stdout)
        self.stderr = LineQueue(self.proc.stderr)
        self.memory: list[dict[str, Any]] = []

    def sample_memory(self, when: str) -> None:
        row = read_rss(self.proc.pid)
        row["when"] = when
        self.memory.append(row)

    def request(self, payload: Mapping[str, Any], timeout: float) -> tuple[dict[str, Any], float, list[str]]:
        assert self.proc.stdin is not None
        started = time.perf_counter()
        self.proc.stdin.write(json.dumps(payload, ensure_ascii=False).encode("utf-8") + b"\n")
        self.proc.stdin.flush()
        response, noise = self.stdout.read_json(timeout)
        return response, time.perf_counter() - started, noise

    def close(self) -> str:
        if self.proc.stdin is not None:
            self.proc.stdin.close()
        try:
            self.proc.wait(timeout=30)
        except subprocess.TimeoutExpired:
            self.proc.terminate()
            try:
                self.proc.wait(timeout=10)
            except subprocess.TimeoutExpired:
                self.proc.kill()
                self.proc.wait(timeout=10)
        chunks = []
        while True:
            try:
                line = self.stderr.queue.get_nowait()
            except queue.Empty:
                break
            if line is None:
                break
            if isinstance(line, bytes):
                line = line.decode("utf-8", errors="replace")
            chunks.append(line)
        return "".join(chunks)[-4000:]


def _index_questions(cases: list[Mapping[str, Any]]) -> dict[str, list[Mapping[str, Any]]]:
    return {case["id"]: list(case["questions"]) for case in cases}


def _short_case(sample: Mapping[str, Any]) -> Mapping[str, Any]:
    for case in sample["cases"]:
        if case["family"] == "collection_routing":
            return case
    raise RuntimeError("sample has no collection_routing case for warmup")


def run_model(
    sample: Mapping[str, Any],
    python: Path,
    device: str,
    threads: int,
    request_timeout: float,
    ready_timeout: float,
    *,
    backend: str = "gliner2",
    gguf: Optional[str] = None,
    extra_cases: Optional[list[Mapping[str, Any]]] = None,
) -> dict[str, Any]:
    grade_unsupported = backend == "gliner2"
    cases = list(sample["cases"]) + list(extra_cases or [])
    questions_for_case = _index_questions(cases)
    session = WorkerSession(python, device, threads, ROOT, backend=backend, gguf=gguf)
    noise: list[str] = []
    try:
        ready, ready_noise = session.stdout.read_json(ready_timeout)
        noise.extend(ready_noise)
        ready_seconds = time.perf_counter() - session.started
        session.sample_memory("after_ready")
        if not ready.get("ready"):
            return {
                "benchmark_run": False,
                "fabricated": False,
                "device_requested": device,
                "ready_seconds": ready_seconds,
                "error": ready.get("error") or "worker ready was false",
                "provenance": ready.get("provenance"),
                "results": None,
            }
        warmup_case = _short_case(sample)
        warmup_question = warmup_case["questions"][0]
        warmup_payload = build_request(
            "warmup",
            [{"id": warmup_case["id"], "text": warmup_case["text"]}],
            [warmup_question],
            sample["max_input_tokens"],
        )
        _, warmup_seconds, warmup_noise = session.request(warmup_payload, request_timeout)
        noise.extend(warmup_noise)
        session.sample_memory("after_warmup")

        scored: list[dict[str, Any]] = []
        latencies: list[float] = []
        gold_latencies: list[float] = []
        completed_requests = 1  # warmup already returned on this open pipe
        for case in cases:
            for question in case["questions"]:
                payload = build_request(
                    f"q-{case['id']}-{question['id']}",
                    [{"id": case["id"], "text": case["text"]}],
                    [question],
                    sample["max_input_tokens"],
                )
                try:
                    response, seconds, extra_noise = session.request(payload, request_timeout)
                    noise.extend(extra_noise)
                    if not response.get("error"):
                        completed_requests += 1
                except (TimeoutError, EOFError) as exc:
                    response = {"version": 1, "id": payload["id"], "error": str(exc)}
                    seconds = None
                observed = seconds if seconds is not None else request_timeout
                if case["family"] == "predicate_gold":
                    gold_latencies.append(observed)
                else:
                    latencies.append(observed)
                scored.extend(
                    predictions_from_response(
                        [case],
                        {case["id"]: [question]},
                        response,
                        seconds,
                        grade_unsupported_capability=grade_unsupported,
                    )
                )
                session.sample_memory("during_items")
        session.sample_memory("after_items")

        independence = _coupling_check(session, sample, scored, request_timeout, noise)
        throughput = _batch_throughput(session, sample, request_timeout, noise)
        session.sample_memory("after_batch")
        stderr_text = ""
    finally:
        stderr_text = session.close()

    rss_values = [row["rss_kb"] for row in session.memory if row.get("rss_kb") is not None]
    hwm_values = [row["hwm_kb"] for row in session.memory if row.get("hwm_kb") is not None]
    main_rows = [row for row in scored if row["family"] != "predicate_gold"]
    gold_rows = [row for row in scored if row["family"] == "predicate_gold"]
    report = metrics.assemble_report(main_rows, latencies=latencies, independence=independence["pairs"])
    gold_report = metrics.assemble_report(gold_rows, latencies=gold_latencies)
    gold_report["note"] = (
        "Preregistered predicate gold, scored apart from the 64 supported labels. "
        "The insufficient option counts when label is insufficient, including the "
        "protocol remap to status unresolved. GLiNER's unsupported predicate rows "
        "are not this gold."
    )
    timeouts = [
        {"id": row["id"], "question_id": row["question_id"], "request_error": row.get("request_error")}
        for row in scored
        if row.get("request_error") and "timed out" in str(row.get("request_error"))
    ]
    report.update(
        {
            "benchmark_run": True,
            "fabricated": False,
            "backend": backend,
            "device_requested": device,
            "threads": threads,
            "gguf_argument": gguf,
            "predicate_profile": (
                "gliner2 rows that expect unsupported stay a capability check"
                if grade_unsupported
                else "GLiNER unsupported-predicate rows are not graded as SemIf ground truth"
            ),
            "roundtrip_confirmed": completed_requests >= 2,
            "completed_evaluate_responses": completed_requests,
            "timeouts": timeouts,
            "ready_seconds": ready_seconds,
            "warmup_seconds": warmup_seconds,
            "loading_time_seconds": ready_seconds,
            "loading_time_note": (
                "Seconds from process start until the ready frame. This worker loads "
                "weights before ready. warmup_seconds is an extra unmeasured forward "
                "pass and is not part of loading_time_seconds or the warm latency sample."
            ),
            "provenance": ready.get("provenance"),
            "memory": {
                "unit": "kB",
                "source": "Linux /proc/PID/status VmRSS and VmHWM",
                "peak_rss_kb": max(rss_values) if rss_values else None,
                "hwm_kb": max(hwm_values) if hwm_values else None,
                "samples": _compact_memory(session.memory),
            },
            "batch_throughput": throughput,
            "question_independence": independence["summary"],
            "per_question_warm_seconds": metrics.latency_summary(latencies),
            "predicate_gold": gold_report,
            "results": scored,
            "protocol_noise": noise[:20],
            "stderr_tail": stderr_text,
        }
    )
    return report


def _compact_memory(samples: list[dict[str, Any]]) -> list[dict[str, Any]]:
    kept = [row for row in samples if row["when"] != "during_items"]
    during = [row for row in samples if row["when"] == "during_items" and row.get("rss_kb") is not None]
    if during:
        peak = max(during, key=lambda row: row["rss_kb"])
        kept.append({"when": "peak_during_items", "rss_kb": peak["rss_kb"], "hwm_kb": peak["hwm_kb"]})
    return kept


def _coupling_check(session: WorkerSession, sample, scored, timeout: float, noise: list[str]) -> dict[str, Any]:
    by_key = {(row["id"], row["question_id"]): row for row in scored}
    pairs = []
    for case in sample["cases"]:
        if len(case["questions"]) < 2:
            continue
        payload = build_request(
            f"joint-{case['id']}",
            [{"id": case["id"], "text": case["text"]}],
            case["questions"],
            sample["max_input_tokens"],
        )
        try:
            response, _seconds, extra = session.request(payload, timeout)
            noise.extend(extra)
        except (TimeoutError, EOFError) as exc:
            response = {"error": str(exc)}
        joint_rows = predictions_from_response(
            [case], {case["id"]: case["questions"]}, response, None
        )
        for joint in joint_rows:
            separate = by_key[(joint["id"], joint["question_id"])]
            pairs.append(
                {
                    "id": joint["id"],
                    "question_id": joint["question_id"],
                    "joint_label": joint["label"] if joint["status"] == "resolved" else None,
                    "separate_label": separate["label"] if separate["status"] == "resolved" else None,
                    "joint_status": joint["status"],
                    "separate_status": separate["status"],
                }
            )
    summary = metrics.independence_rate(pairs)
    summary["used_for_accuracy"] = False
    summary["pairs"] = pairs
    return {"summary": summary, "pairs": pairs}


def _batch_throughput(session: WorkerSession, sample, timeout: float, noise: list[str]) -> dict[str, Any]:
    group = [case for case in sample["cases"] if case["family"] == "collection_routing"][:BATCH_CAP]
    if len(group) < 2:
        return {"ran": False, "reason": "not enough same-schema cases"}
    questions = group[0]["questions"]
    payload = build_request(
        "batch-throughput",
        [{"id": case["id"], "text": case["text"]} for case in group],
        questions,
        sample["max_input_tokens"],
    )
    try:
        response, seconds, extra = session.request(payload, timeout)
        noise.extend(extra)
    except (TimeoutError, EOFError) as exc:
        return {"ran": False, "error": str(exc), "states": len(group)}
    if response.get("error") and not response.get("results"):
        return {"ran": False, "error": response["error"], "states": len(group), "seconds": seconds}
    result_count = len(response.get("results") or [])
    return {
        "ran": True,
        "used_for_accuracy": False,
        "states": len(group),
        "seconds": seconds,
        "result_rows": result_count,
        "states_per_second": len(group) / seconds if seconds else None,
        "rows_per_second": result_count / seconds if seconds else None,
        "note": "Throughput only. Labels from this batch are not the accuracy source.",
    }


def _note_file(path: Path) -> dict[str, Any]:
    if not path.is_file():
        return {"path": str(path), "present": False}
    return {"path": str(path), "present": True, "sha256": sha256_file(path)}


def load_predicate_gold(path: Path) -> dict[str, Any]:
    payload = json.loads(path.read_text(encoding="utf-8"))
    if not payload.get("frozen_before_model_run"):
        raise ValueError(f"{path} is not marked frozen before the model run")
    labels = []
    for case in payload["cases"]:
        if case.get("family") != "predicate_gold":
            raise ValueError(f"{case.get('id')} is not predicate_gold")
        for question in case["questions"]:
            if question.get("kind") != "predicate":
                raise ValueError(f"{case['id']} must be a predicate question")
            label = question["expectation"].get("label")
            if label not in {"supported", "insufficient", "contradicted"}:
                raise ValueError(f"{case['id']} has gold outside the pinned predicate schema")
            labels.append(label)
            statement = question["statement"]
            if "(" in statement or ")" in statement:
                raise ValueError(f"{case['id']} statement contains reserved parentheses")
    if len(labels) < 9 or any(labels.count(name) < 3 for name in ("supported", "insufficient", "contradicted")):
        raise ValueError("predicate gold needs at least three cases of each pinned option")
    return payload


def compare_gliner(path: Path, semif_rows: list[Mapping[str, Any]]) -> dict[str, Any]:
    """Same supported-label denominator as the stored GLiNER run. Not BF16 JevBench."""
    if not path.is_file():
        return {"present": False, "path": str(path)}
    payload = json.loads(path.read_text(encoding="utf-8"))
    reference = {
        (row["id"], row["question_id"]): row
        for row in (payload.get("model") or {}).get("results") or []
        if row.get("expected_type") == "label" and row.get("family") != "predicate_gold"
    }
    both = gliner_only = semif_only = neither = same = 0
    for row in semif_rows:
        if row.get("expected_type") != "label" or row.get("family") == "predicate_gold":
            continue
        other = reference.get((row["id"], row["question_id"]))
        if other is None:
            continue
        gliner_ok = other.get("status") == "resolved" and other.get("label") == other.get("expected_label")
        semif_ok = metrics.is_correct(row)
        if gliner_ok and semif_ok:
            both += 1
        elif gliner_ok:
            gliner_only += 1
        elif semif_ok:
            semif_only += 1
        else:
            neither += 1
        if other.get("status") == row.get("status") and other.get("label") == row.get("label"):
            same += 1
    denominator = both + gliner_only + semif_only + neither
    return {
        "present": True,
        "path": str(path),
        "denominator": denominator,
        "gliner_correct": both + gliner_only,
        "semif_correct": both + semif_only,
        "both_correct": both,
        "gliner_only": gliner_only,
        "semif_only": semif_only,
        "both_wrong": neither,
        "same_status_and_label": same,
        "bf16_jevbench_not_used": True,
        "note": (
            "Comparison uses the stored GLiNER CPU labels on the same supported-label "
            "questions. It is not a transfer of the BF16 JevBench score."
        ),
    }


def build_report(
    sample: Mapping[str, Any],
    sample_path: Path,
    model: Optional[dict[str, Any]],
    backend: str = "gliner2",
) -> dict[str, Any]:
    grade_unsupported = backend == "gliner2"
    baseline_reports = {}
    for name, spec in baselines.BASELINES.items():
        items = baseline_items(
            sample, spec["predict"], grade_unsupported_capability=grade_unsupported
        )
        report = metrics.assemble_report(items)
        report["rule"] = spec["rule"]
        report["byte_limit"] = baselines.BYTE_LIMIT
        report["results"] = items
        baseline_reports[name] = report
    model_block = model or {
        "benchmark_run": False,
        "fabricated": False,
        "results": None,
        "reason": "Model benchmark was not requested.",
    }
    return {
        "sample_version": sample["sample_version"],
        "limitation": sample["limitation"],
        "fabricated": False,
        "release_gate": None,
        "release_qualified": False,
        "release_note": (
            "No numeric release gate is defined. This run does not qualify a "
            "checkpoint, including the initial GLiNER2.5 candidate, for release."
        ),
        "scoring": {
            "accuracy_requests": "one question per evaluate call",
            "joint_requests": "coupling check only",
            "batch_requests": "throughput only",
            "scores_are_calibrated": False,
            "absent_evidence_labels": "author judgments; not rewritten from model output",
        },
        "hashes": {
            "sample_sha256": sha256_file(sample_path),
            "runner_sha256": sha256_file(Path(__file__).resolve()),
            "metrics_sha256": sha256_file(Path(__file__).with_name("metrics.py")),
            "baselines_sha256": sha256_file(Path(__file__).with_name("baselines.py")),
            "sample_data_sha256": sha256_file(Path(__file__).with_name("sample_data.py")),
            "predicate_gold_sha256": (
                sha256_file(PREDICATE_GOLD_PATH) if PREDICATE_GOLD_PATH.is_file() else None
            ),
        },
        "inbox": _note_file(INBOX_PATH),
        "contract_notes": _note_file(CONTRACT_PATH),
        "baselines": baseline_reports,
        "model": model_block,
    }


def wait_for_worker(seconds: float) -> bool:
    deadline = time.monotonic() + seconds
    while True:
        if worker_module_present():
            return True
        if time.monotonic() >= deadline:
            return False
        if living_pid() is None:
            return False
        time.sleep(5)


def main(argv: Optional[list[str]] = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--sample", type=Path, default=sample_data.SAMPLE_PATH)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--python", type=Path, default=DEFAULT_PYTHON)
    parser.add_argument("--device", default="cpu")
    parser.add_argument("--threads", type=int, default=4)
    parser.add_argument("--backend", choices=("gliner2", "semif"), default="gliner2")
    parser.add_argument("--gguf", default=None, help="local Q4_K_M GGUF path for the SemIf CPU profile")
    parser.add_argument(
        "--predicate-gold",
        type=Path,
        default=None,
        help="preregistered predicate gold; scored apart from the 64 label questions",
    )
    parser.add_argument(
        "--gliner-results",
        type=Path,
        default=None,
        help="existing GLiNER result JSON for a same-denominator comparison",
    )
    parser.add_argument("--baselines-only", action="store_true")
    parser.add_argument("--wait-seconds", type=float, default=0)
    parser.add_argument("--request-timeout", type=float, default=180)
    parser.add_argument("--ready-timeout", type=float, default=600)
    args = parser.parse_args(argv)

    sample = sample_data.load_sample(args.sample)
    extra_cases = []
    if args.predicate_gold is not None:
        extra_cases = load_predicate_gold(args.predicate_gold)["cases"]
    model = None
    exit_code = 0
    if not args.baselines_only:
        if args.wait_seconds:
            wait_for_worker(args.wait_seconds)
        if not worker_module_present():
            model = {
                "benchmark_run": False,
                "fabricated": False,
                "results": None,
                "reason": "python/checkweave_worker is not importable yet.",
                "worker_pid": living_pid(),
                "device_requested": args.device,
            }
            exit_code = 2
        elif not args.python.is_file():
            model = {
                "benchmark_run": False,
                "fabricated": False,
                "results": None,
                "reason": f"Python interpreter not found at {args.python}.",
                "device_requested": args.device,
            }
            exit_code = 2
        else:
            try:
                model = run_model(
                    sample,
                    args.python,
                    args.device,
                    args.threads,
                    args.request_timeout,
                    args.ready_timeout,
                    backend=args.backend,
                    gguf=args.gguf,
                    extra_cases=extra_cases,
                )
                if model.get("benchmark_run") and args.gliner_results is not None:
                    model["gliner_comparison"] = compare_gliner(args.gliner_results, model.get("results") or [])
            except Exception as exc:  # noqa: BLE001 — record the failure, do not invent metrics.
                model = {
                    "benchmark_run": False,
                    "fabricated": False,
                    "results": None,
                    "reason": f"{type(exc).__name__}: {exc}",
                    "device_requested": args.device,
                }
                exit_code = 1
            if not model.get("benchmark_run"):
                exit_code = 1
    report = build_report(sample, args.sample, model, backend=args.backend)
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(json.dumps(report, indent=2, ensure_ascii=False) + "\n", encoding="utf-8")
    summary = {
        "output": str(args.output),
        "benchmark_run": bool(model and model.get("benchmark_run")),
        "release_qualified": False,
        "fabricated": False,
    }
    print(json.dumps(summary))
    return exit_code


if __name__ == "__main__":
    sys.exit(main())
