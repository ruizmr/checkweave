"""CPU smoke of the pinned SemIf Q4 worker on a live pipe.

The writer stays open between requests. Scores are recorded as returned.
They are not a calibration check and they are not a BF16 quality claim.
"""

from __future__ import annotations

import json
import os
import subprocess
import sys
import time
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
PYTHON = os.environ.get("CHECKWEAVE_PYTHON", "/tmp/checkweave-semif-venv/bin/python")
RESULTS = Path(__file__).resolve().parent / "results"
CODE_PIN = "1f2dea3e25379f9dfc98cb83c324f00ab5deda37"
TOKENIZER_REVISION = "851bf6e806efd8d0a36b00ddf55e13ccb7b8cd0a"
GGUF_BYTES = 3013027808

CHOICE = {
    "kind": "choice",
    "id": "scope",
    "labels": [
        {"label": "documentation", "description": "Only comments or docs change."},
        {"label": "behavior", "description": "Runtime behavior changes."},
    ],
}
PREDICATE = {
    "kind": "predicate",
    "id": "deployed",
    "statement": "The deployment succeeded.",
}
STATE = {
    "id": "note",
    "text": "The deployment completed, health checks passed, and no rollback was started.",
}


def main() -> int:
    RESULTS.mkdir(parents=True, exist_ok=True)
    stderr_path = RESULTS / "semif-cpu-stderr.log"
    env = os.environ.copy()
    env["PYTHONPATH"] = "python"
    env["HF_HUB_DISABLE_IMPLICIT_TOKEN"] = "1"
    env["CUDA_VISIBLE_DEVICES"] = ""
    started = time.perf_counter()
    with stderr_path.open("w", encoding="utf-8") as stderr:
        process = subprocess.Popen(
            [
                PYTHON,
                "-m",
                "checkweave_worker",
                "--backend",
                "semif",
                "--device",
                "cpu",
                "--threads",
                "4",
                "--offline",
            ],
            cwd=ROOT,
            env=env,
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=stderr,
        )
        assert process.stdin is not None and process.stdout is not None
        try:
            ready = _read_json(process.stdout, 600)
            load_seconds = time.perf_counter() - started
            first = _request(
                process,
                {
                    "version": 1,
                    "id": "smoke-1",
                    "op": "evaluate",
                    "states": [STATE],
                    "questions": [PREDICATE, CHOICE],
                },
            )
            # Writer remains open. A second short frame must return.
            assert not process.stdin.closed
            second = _request(
                process,
                {
                    "version": 1,
                    "id": "smoke-2",
                    "op": "evaluate",
                    "states": [STATE],
                    "questions": [CHOICE],
                },
            )
            limited = _request(
                process,
                {
                    "version": 1,
                    "id": "smoke-limit",
                    "op": "evaluate",
                    "max_input_tokens": 8,
                    "states": [STATE],
                    "questions": [CHOICE],
                },
            )
            shutdown = _request(
                process, {"version": 1, "id": "bye", "op": "shutdown"}
            )
        finally:
            if process.poll() is None:
                process.stdin.close()
                process.wait(timeout=120)
    wall = time.perf_counter() - started
    report = {
        "python": PYTHON,
        "backend": "semif",
        "device_arg": "cpu",
        "threads": 4,
        "offline": True,
        "load_seconds": load_seconds,
        "wall_seconds": wall,
        "writer_stayed_open_for_second_request": True,
        "ready": ready,
        "first": first,
        "second": second,
        "over_limit": limited,
        "shutdown": shutdown,
        "quality_claim": (
            "Q4_K_M smoke only. Scores are uncalibrated. "
            "This is not a BF16 JevBench measurement."
        ),
    }
    destination = RESULTS / "semif-cpu-smoke.json"
    destination.write_text(json.dumps(report, indent=2) + "\n")
    _check(report)
    print(destination)
    print(f"wall_seconds={wall:.1f} load_seconds={load_seconds:.1f}")
    return 0


def _request(process: subprocess.Popen, payload: dict) -> dict:
    assert process.stdin is not None and process.stdout is not None
    process.stdin.write(json.dumps(payload).encode() + b"\n")
    process.stdin.flush()
    return _read_json(process.stdout, 600)


def _read_json(stream, timeout: float) -> dict:
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        line = stream.readline()
        if line:
            return json.loads(line)
        if stream.closed:
            break
        time.sleep(0.05)
    raise TimeoutError("worker produced no protocol line")


def _check(report: dict) -> None:
    ready = report["ready"]
    if not ready.get("ready"):
        raise SystemExit(f"worker not ready: {ready}")
    provenance = ready["provenance"]
    extra = provenance.get("extra") or provenance
    gguf = extra.get("gguf") if isinstance(extra, dict) else None
    if not isinstance(gguf, dict):
        # extras are flattened onto provenance
        gguf = provenance.get("gguf")
    if not isinstance(gguf, dict) or not gguf.get("sha256"):
        raise SystemExit(f"missing GGUF sha256: {provenance}")
    if gguf.get("bytes") != GGUF_BYTES:
        raise SystemExit(f"unexpected GGUF size: {gguf}")
    if provenance.get("precision") != "gguf-q4_k_m":
        raise SystemExit(f"precision {provenance.get('precision')}")
    if provenance.get("device") != "cpu":
        raise SystemExit(f"device {provenance.get('device')}")
    if extra.get("code_pin", provenance.get("code_pin")) != CODE_PIN:
        raise SystemExit("code pin mismatch")
    if extra.get("tokenizer_revision", provenance.get("tokenizer_revision")) != TOKENIZER_REVISION:
        raise SystemExit("tokenizer revision mismatch")
    if provenance.get("n_gpu_layers", extra.get("n_gpu_layers")) not in (0, None):
        n_gpu = provenance.get("n_gpu_layers", extra.get("n_gpu_layers"))
        if n_gpu != 0:
            raise SystemExit(f"n_gpu_layers {n_gpu}")
    status = provenance.get("score_semantics", {}).get("probability_status", "")
    if "uncalibrated" not in status or "BF16" not in status:
        raise SystemExit(f"score semantics do not disclaim BF16 quality: {status}")
    if report["second"].get("id") != "smoke-2":
        raise SystemExit("second live-pipe request did not return")
    for row in report["first"]["results"]:
        if row["question_id"] == "deployed":
            scores = row.get("scores") or {}
            if set(scores) != {"supported", "insufficient", "contradicted"}:
                raise SystemExit(f"predicate scores {scores}")
        if row["question_id"] == "scope":
            scores = row.get("scores") or {}
            if set(scores) != {"documentation", "behavior"}:
                raise SystemExit(f"choice scores {scores}")
    limit = report["over_limit"]["results"][0]
    if limit.get("status") != "unresolved" or (limit.get("answer") or {}).get("omitted") not in (
        None,
        True,
    ):
        # engine uses omitted: null inside the row, not nested answer
        if limit.get("status") != "unresolved":
            raise SystemExit(f"over-limit row {limit}")
    if "omitted" in limit and limit["omitted"] is not None:
        raise SystemExit(f"over-limit omitted a span: {limit}")
    if report["shutdown"].get("shutdown") is not True:
        raise SystemExit(f"shutdown failed: {report['shutdown']}")


if __name__ == "__main__":
    raise SystemExit(main())
