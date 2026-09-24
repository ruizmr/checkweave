"""Real local checkpoint smoke. Uses the existing experiment interpreter.

This does not modify that environment. It writes a JSON report under
python/tests/results/.
"""

from __future__ import annotations

import json
import os
import subprocess
import sys
import time
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
PYTHON = os.environ.get(
    "CHECKWEAVE_PYTHON",
    "/tmp/checkweave-backends.ER6XYe/venv/bin/python",
)
RESULTS = Path(__file__).resolve().parent / "results"
CHOICE = {
    "kind": "choice",
    "id": "scope",
    "labels": [
        {"label": "documentation", "description": "Only comments or docs change."},
        {"label": "behavior", "description": "Runtime behavior changes."},
    ],
}
ORDINAL = {
    "kind": "ordinal",
    "id": "urgency",
    "levels": [
        {"label": "low", "description": "Can wait.", "value": 0},
        {"label": "high", "description": "Needs attention now.", "value": 10},
    ],
}
PREDICATE = {
    "kind": "predicate",
    "id": "windows",
    "statement": "The tests passed on Windows.",
}


class Worker:
    def __init__(self, device: str) -> None:
        env = os.environ.copy()
        env["PYTHONPATH"] = "python"
        env["HF_HUB_DISABLE_IMPLICIT_TOKEN"] = "1"
        self.stderr_path = RESULTS / f"stderr-{device.replace(':', '_')}.log"
        RESULTS.mkdir(parents=True, exist_ok=True)
        self.stderr_file = self.stderr_path.open("w", encoding="utf-8")
        self.process = subprocess.Popen(
            [
                PYTHON,
                "-m",
                "checkweave_worker",
                "--device",
                device,
                "--threads",
                "4",
                "--offline",
            ],
            cwd=ROOT,
            env=env,
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=self.stderr_file,
        )
        assert self.process.stdin is not None
        assert self.process.stdout is not None

    def request(self, payload: dict, timeout: float = 180) -> dict:
        assert self.process.stdin is not None
        assert self.process.stdout is not None
        line = json.dumps(payload).encode() + b"\n"
        self.process.stdin.write(line)
        self.process.stdin.flush()
        return _read_json(self.process.stdout, timeout)

    def ready(self, timeout: float = 180) -> dict:
        assert self.process.stdout is not None
        return _read_json(self.process.stdout, timeout)

    def close(self) -> str:
        assert self.process.stdin is not None
        self.process.stdin.close()
        code = self.process.wait(timeout=60)
        self.stderr_file.close()
        stderr = self.stderr_path.read_text(encoding="utf-8", errors="replace")
        return f"exit={code}\n{stderr}"


def _read_json(stream, timeout: float) -> dict:
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        line = stream.readline()
        if line:
            return json.loads(line)
        time.sleep(0.05)
    raise TimeoutError("worker produced no protocol line")


def _run_device(device: str) -> dict:
    started = time.perf_counter()
    worker = Worker(device)
    try:
        ready = worker.ready()
        load_seconds = time.perf_counter() - started
        if not ready.get("ready"):
            return {"device_arg": device, "ready": ready, "load_seconds": load_seconds}
        short = "The documentation only changes a comment in the public guide."
        evaluated = worker.request(
            {
                "version": 1,
                "id": "smoke-1",
                "op": "evaluate",
                "max_input_tokens": 4096,
                "states": [{"id": "note", "text": short}],
                "questions": [CHOICE, ORDINAL, PREDICATE],
            }
        )
        limited = worker.request(
            {
                "version": 1,
                "id": "smoke-limit",
                "op": "evaluate",
                "max_input_tokens": 8,
                "states": [{"id": "note", "text": short}],
                "questions": [CHOICE],
            }
        )
        again = worker.request(
            {
                "version": 1,
                "id": "smoke-2",
                "op": "evaluate",
                "states": [
                    {"id": "note", "text": short},
                    {"id": "code", "text": "The function now returns the previous value."},
                ],
                "questions": [CHOICE],
            }
        )
        shutdown = worker.request({"version": 1, "id": "bye", "op": "shutdown"})
        return {
            "device_arg": device,
            "load_seconds": load_seconds,
            "ready": ready,
            "evaluate": evaluated,
            "over_limit": limited,
            "second": again,
            "shutdown": shutdown,
        }
    finally:
        report = worker.close()
        print(f"--- {device} stderr ---", file=sys.stderr)
        print(report[-4000:], file=sys.stderr)


def main() -> int:
    RESULTS.mkdir(parents=True, exist_ok=True)
    report = {"python": PYTHON, "runs": []}
    for device in ("cpu", "cuda:0", "auto"):
        print(f"smoke {device}", flush=True)
        try:
            report["runs"].append(_run_device(device))
        except Exception as exc:
            report["runs"].append({"device_arg": device, "error": f"{type(exc).__name__}: {exc}"})
    destination = RESULTS / "local-smoke.json"
    destination.write_text(json.dumps(report, indent=2) + "\n")
    print(destination)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
