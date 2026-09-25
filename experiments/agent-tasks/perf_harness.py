#!/usr/bin/env python3
"""Bounded Checkweave performance harness (stdlib only).

Measures a real checkweave binary in a temporary workspace. It does not run
against the user's project, and it does not repeat crash or cache-corruption
recovery checks.

Sample counts are fixed and small. Median uses the average of the two central
values when the count is even. p95 uses the nearest-rank method
(ceil(0.95 * n), 1-based). With these counts, p95 is a sample order statistic,
not a stable population percentile.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import math
import os
import platform
import sqlite3
import statistics
import subprocess
import tempfile
import time
from pathlib import Path
from typing import Any

PREDICATE = {"op": "gt", "path": "/n", "value": 1}
SMALL_RECORDS = 40
CHANGED_SAMPLES = 8
COLD_SAMPLES = 8
WARM_SAMPLES = 20
HELP_SAMPLES = 8
IDLE_SAMPLES = 4
IDLE_SECONDS = 2
LARGE_FILES = 4
LARGE_RECORDS_PER_FILE = 2000


def percentile_nearest(values: list[float], fraction: float) -> float:
    ordered = sorted(values)
    rank = max(1, math.ceil(fraction * len(ordered)))
    return ordered[rank - 1]


def summarize(values: list[float]) -> dict[str, Any]:
    return {
        "n": len(values),
        "min": min(values),
        "median": statistics.median(values),
        "p95_nearest_rank": percentile_nearest(values, 0.95),
        "max": max(values),
        "samples_ms": [round(v, 3) for v in values],
    }


def sha256_file(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        for chunk in iter(lambda: handle.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def sha256_text(text: str) -> str:
    return hashlib.sha256(text.encode()).hexdigest()


def binary_fingerprint(path: Path) -> dict[str, Any]:
    file_out = subprocess.run(
        ["file", "-b", str(path)],
        check=True,
        capture_output=True,
        text=True,
    ).stdout.strip()
    build_id = ""
    for part in file_out.split(","):
        part = part.strip()
        if part.lower().startswith("buildid"):
            build_id = part.split("=", 1)[-1].strip()
    lowered = str(path)
    if "with debug_info" in file_out or "/debug/" in lowered:
        profile = "debug"
    elif "/release/" in lowered:
        profile = "release"
    else:
        profile = "unlabeled"
    return {
        "path": str(path.resolve()),
        "size_bytes": path.stat().st_size,
        "version": subprocess.check_output([str(path.resolve()), "--version"], text=True, timeout=10).strip(),
        "sha256": sha256_file(path),
        "file": file_out,
        "build_id": build_id,
        "profile": profile,
        "profile_signals": {
            "path_has_debug": "/debug/" in lowered,
            "path_has_release": "/release/" in lowered,
            "file_mentions_debug_info": "with debug_info" in file_out,
            "file_mentions_stripped": "stripped" in file_out and "not stripped" not in file_out,
        },
    }


def write_jsonl(path: Path, records: list[dict[str, Any]]) -> str:
    body = "".join(json.dumps(row, separators=(",", ":")) + "\n" for row in records)
    path.write_text(body)
    return hashlib.sha256(body.encode()).hexdigest()


def dir_bytes(path: Path) -> int:
    total = 0
    if not path.exists():
        return 0
    for item in path.rglob("*"):
        if item.is_file():
            total += item.stat().st_size
    return total


def sqlite_snapshot(db_path: Path) -> dict[str, Any]:
    out: dict[str, Any] = {
        "path": str(db_path),
        "file_bytes": db_path.stat().st_size if db_path.is_file() else 0,
    }
    if not db_path.is_file():
        out["error"] = "missing"
        return out
    uri = f"file:{db_path}?mode=ro"
    conn = sqlite3.connect(uri, uri=True)
    try:
        page_size = conn.execute("PRAGMA page_size").fetchone()[0]
        page_count = conn.execute("PRAGMA page_count").fetchone()[0]
        freelist = conn.execute("PRAGMA freelist_count").fetchone()[0]
        tables = [
            row[0]
            for row in conn.execute(
                "SELECT name FROM sqlite_master WHERE type='table' ORDER BY name"
            )
        ]
        counts = {}
        for name in tables:
            counts[name] = conn.execute(f"SELECT COUNT(*) FROM {name}").fetchone()[0]
        payload = None
        columns = [
            row[1]
            for row in conn.execute("PRAGMA table_info(reports)").fetchall()
        ] if "reports" in tables else []
        if "payload_bytes" in columns:
            payload = conn.execute(
                "SELECT COUNT(*), COALESCE(SUM(payload_bytes), 0) FROM reports"
            ).fetchone()
        out.update(
            {
                "page_size": page_size,
                "page_count": page_count,
                "allocated_bytes": page_size * page_count,
                "freelist_pages": freelist,
                "tables": counts,
                "reports_rows": None if payload is None else payload[0],
                "reports_payload_bytes_sum": None if payload is None else payload[1],
            }
        )
    finally:
        conn.close()
    return out


class Cli:
    def __init__(self, binary: Path, root: Path, env: dict[str, str]) -> None:
        self.binary = binary
        self.root = root
        self.env = env

    def run(self, *args: str, timeout: float = 60) -> tuple[float, dict[str, Any]]:
        start = time.perf_counter()
        proc = subprocess.run(
            [str(self.binary), "--workspace", str(self.root), *args],
            capture_output=True,
            text=True,
            timeout=timeout,
            env=self.env,
        )
        elapsed_ms = (time.perf_counter() - start) * 1000
        if proc.returncode != 0:
            raise RuntimeError(
                f"command failed {args}: rc={proc.returncode} stdout={proc.stdout!r} stderr={proc.stderr!r}"
            )
        try:
            payload = json.loads(proc.stdout)
        except json.JSONDecodeError as exc:
            raise RuntimeError(f"non-json stdout for {args}: {proc.stdout!r}") from exc
        return elapsed_ms, payload

    def shutdown(self) -> None:
        try:
            self.run("shutdown", timeout=20)
        except (RuntimeError, subprocess.TimeoutExpired):
            pass

    def daemon_pid(self) -> int | None:
        path = self.root / ".checkweave" / "daemon.json"
        if not path.is_file():
            return None
        try:
            return int(json.loads(path.read_text()).get("pid"))
        except (OSError, ValueError, json.JSONDecodeError):
            return None


def pid_alive(pid: int) -> bool:
    try:
        os.kill(pid, 0)
    except ProcessLookupError:
        return False
    except PermissionError:
        return True
    return True


def wait_pid_exit(pid: int, timeout: float) -> float | None:
    start = time.perf_counter()
    deadline = start + timeout
    while time.perf_counter() < deadline:
        if not pid_alive(pid):
            return (time.perf_counter() - start) * 1000
        time.sleep(0.05)
    return None


def check_args() -> list[str]:
    return [
        "check",
        "--include",
        "**/*.jsonl",
        "--predicate",
        json.dumps(PREDICATE, separators=(",", ":")),
        "--max-results",
        "1",
    ]


def assert_complete(report: dict[str, Any], records: int) -> None:
    if report.get("execution") != "complete" or report.get("freshness") != "validated":
        raise RuntimeError(f"unexpected report status: {report}")
    coverage = report["coverage"]
    if coverage["records"] != records or coverage["evaluated"] != records:
        raise RuntimeError(f"unexpected coverage: {coverage}")


def measure_help(binary: Path, samples: int) -> list[float]:
    values = []
    for _ in range(samples):
        start = time.perf_counter()
        proc = subprocess.run(
            [str(binary), "--help"],
            capture_output=True,
            text=True,
            timeout=30,
        )
        elapsed = (time.perf_counter() - start) * 1000
        if proc.returncode != 0:
            raise RuntimeError(proc.stderr)
        values.append(elapsed)
    return values


def disk_report(root: Path) -> dict[str, Any]:
    state = root / ".checkweave"
    db = state / "cache.sqlite"
    files = []
    if state.is_dir():
        for item in sorted(p for p in state.rglob("*") if p.is_file()):
            files.append({"name": item.relative_to(state).as_posix(), "bytes": item.stat().st_size})
    family = 0
    for suffix in ("", "-wal", "-shm"):
        sibling = Path(str(db) + suffix)
        if sibling.is_file():
            family += sibling.stat().st_size
    snap = sqlite_snapshot(db)
    snap["family_bytes"] = family
    return {
        "state_dir_bytes": dir_bytes(state),
        "workspace_bytes": dir_bytes(root),
        "state_files": files,
        "sqlite": snap,
    }


def filesystem_of(path: Path) -> dict[str, Any]:
    proc = subprocess.run(
        ["findmnt", "-T", str(path), "-J"],
        capture_output=True,
        text=True,
    )
    if proc.returncode != 0:
        return {"path": str(path), "findmnt_error": proc.stderr.strip()}
    try:
        payload = json.loads(proc.stdout)
    except json.JSONDecodeError:
        return {"path": str(path), "findmnt_raw": proc.stdout.strip()}
    filesystems = payload.get("filesystems") or []
    current = filesystems[0] if filesystems else {}
    fstype = current.get("fstype")
    role = "diagnostic_tmpfs" if fstype == "tmpfs" else "physical_filesystem_sample"
    return {
        "path": str(path),
        "target": current.get("target"),
        "source": current.get("source"),
        "fstype": fstype,
        "role": role,
    }


def loadavg() -> str:
    return Path("/proc/loadavg").read_text().strip()


def idle_resources(cli: Cli, seconds: float = 5.0) -> dict[str, Any]:
    """Linux process CPU and resident pages, with no client calls during the window."""
    if platform.system() != "Linux":
        return {"status": "unsupported", "reason": "requires Linux /proc"}
    pid = cli.daemon_pid()
    if pid is None:
        raise RuntimeError("no daemon to measure")

    def read_process() -> tuple[int, int, int]:
        # comm may contain spaces or parentheses; fields after its last ')' start at 3.
        fields = Path(f"/proc/{pid}/stat").read_text().rsplit(")", 1)[1].split()
        return int(fields[11]) + int(fields[12]), int(fields[19]), int(fields[21])

    ticks0, birth0, rss0 = read_process()
    start = time.perf_counter()
    time.sleep(seconds)
    ticks1, birth1, rss1 = read_process()
    elapsed = time.perf_counter() - start
    if birth0 != birth1:
        raise RuntimeError("daemon pid was reused during idle measurement")
    hz = os.sysconf("SC_CLK_TCK")
    cpu_seconds = (ticks1 - ticks0) / hz
    return {"status": "measured", "pid": pid, "window_seconds": elapsed,
            "cpu_ticks": ticks1 - ticks0, "ticks_per_second": hz,
            "cpu_seconds": cpu_seconds, "cpu_percent_one_core": 100 * cpu_seconds / elapsed,
            "rss_start_bytes": rss0 * os.sysconf("SC_PAGE_SIZE"),
            "rss_end_bytes": rss1 * os.sysconf("SC_PAGE_SIZE"),
            "meaning": "One five-second window; RSS endpoints, not peak memory. CPU is tick-quantized."}


def trace_overhead(cli: Cli) -> dict[str, Any]:
    script = Path(__file__).resolve().parents[2] / "examples/recipes/debug/discount.py"
    source = script.read_text()
    (cli.root / "discount.py").write_text(source)
    request = {"script": "discount.py", "input": {"price": 80, "percent": 20}, "baseline": True}
    request_path = cli.root / "trace-perf.json"
    request_path.write_text(json.dumps(request))
    samples = []
    for _ in range(5):
        wall, report = cli.run("trace", "--request-file", str(request_path))
        if report["execution"] != "complete" or report["freshness"] != "validated":
            raise RuntimeError(f"trace measurement failed: {report}")
        samples.append({"wall_ms": wall, **{k: report[k] for k in
                        ("baseline_us", "traced_us", "overhead_us", "event_total", "dropped", "unsupported")}})
    return {"fixture_sha256": sha256_text(source), "request": request, "samples": samples,
            "baseline_ms": summarize([v["baseline_us"] / 1000 for v in samples]),
            "traced_ms": summarize([v["traced_us"] / 1000 for v in samples]),
            "overhead_ms": summarize([v["overhead_us"] / 1000 for v in samples]),
            "meaning": "Each sample executes the script twice (baseline first); in-process run_code timings; excludes interpreter startup. Baseline runs first in the same interpreter and may warm imports. Tiny fixture, five samples."}


def run_suite(binary: Path, parent: Path) -> dict[str, Any]:
    base_env = os.environ.copy()
    base_env.pop("CHECKWEAVE_IDLE_SECONDS", None)
    limits = {
        "predicate": PREDICATE,
        "include": "**/*.jsonl",
        "max_results": 1,
        "other_limits": "CLI defaults (max_files 1000, max_bytes 33554432, max_records 100000, timeout_ms 30000) except the large corpus timeout_ms",
        "large_timeout_ms": 120000,
        "small_records": SMALL_RECORDS,
        "large_files": LARGE_FILES,
        "large_records_per_file": LARGE_RECORDS_PER_FILE,
        "cold_samples": COLD_SAMPLES,
        "warm_samples": WARM_SAMPLES,
        "changed_samples": CHANGED_SAMPLES,
        "help_samples": HELP_SAMPLES,
        "idle_samples": IDLE_SAMPLES,
        "idle_seconds": IDLE_SECONDS,
    }
    spec = json.dumps(limits, sort_keys=True, separators=(",", ":"))
    parent.mkdir(parents=True, exist_ok=True)
    result: dict[str, Any] = {
        "harness": "experiments/agent-tasks/perf_harness.py",
        "measured_at_unix": time.time(),
        "loadavg_start": loadavg(),
        "filesystem": filesystem_of(parent),
        "conditions": (
            "Samples were taken on this host while other work was running. "
            "They describe this run. They are not an intrinsic latency of the CLI. "
            "A tmpfs result is a diagnostic and does not replace a physical-filesystem sample."
        ),
        "platform": {
            "system": platform.system(),
            "release": platform.release(),
            "machine": platform.machine(),
            "python": platform.python_version(),
            "uname": " ".join(os.uname()),
        },
        "binary": binary_fingerprint(binary),
        "limits": limits,
        "fixture_spec_sha256": sha256_text(spec),
        "notes": [
            "Wall-clock samples include process spawn and JSON printing.",
            "elapsed_ms is the value the check report itself records.",
            "Warm samples reuse one daemon process. Cold check samples shut that process down first.",
            "A changed row uses a record body that has not been seen before, so the record cache misses once.",
            "Idle exit is the daemon process leaving after CHECKWEAVE_IDLE_SECONDS with no further client calls.",
            "Recovery, crash, and cache-corruption cases are left to scripts/acceptance.py.",
        ],
    }

    with tempfile.TemporaryDirectory(prefix="checkweave-perf-", dir=str(parent)) as temp:
        root = Path(temp) / "latency"
        root.mkdir()
        cli = Cli(binary, root, base_env)
        try:
            cli.run("init", "--agent", "none")
            source = root / "rows.jsonl"
            records = [{"n": i, "id": f"r{i:04d}"} for i in range(SMALL_RECORDS)]
            fixture_hash = write_jsonl(source, records)
            result["small_fixture_sha256"] = fixture_hash

            empty_wall, empty_report = cli.run(*check_args(), timeout=60)
            assert_complete(empty_report, SMALL_RECORDS)
            if empty_report["coverage"]["cache_misses"] != SMALL_RECORDS:
                raise RuntimeError(empty_report["coverage"])
            result["empty_cache_first_check"] = {
                "wall_ms": round(empty_wall, 3),
                "elapsed_ms": empty_report.get("elapsed_ms"),
                "coverage": empty_report["coverage"],
            }
            daemon_pid = cli.daemon_pid()

            warm_wall: list[float] = []
            warm_elapsed: list[float] = []
            pids: list[int | None] = []
            for _ in range(WARM_SAMPLES):
                wall, report = cli.run(*check_args(), timeout=60)
                assert_complete(report, SMALL_RECORDS)
                coverage = report["coverage"]
                if coverage["cache_hits"] != SMALL_RECORDS or coverage["cache_misses"] != 0:
                    raise RuntimeError(f"warm check recomputed records: {coverage}")
                warm_wall.append(wall)
                warm_elapsed.append(float(report.get("elapsed_ms") or 0))
                pids.append(cli.daemon_pid())
            result["warm_unchanged"] = {
                "wall_ms": summarize(warm_wall),
                "report_elapsed_ms": summarize(warm_elapsed),
                "daemon_pid": daemon_pid,
                "daemon_pid_stable": len(set(pids)) == 1 and pids[0] == daemon_pid and daemon_pid is not None,
                "cache_hits": SMALL_RECORDS,
                "cache_misses": 0,
            }

            changed_wall: list[float] = []
            changed_elapsed: list[float] = []
            next_n = 10_000
            for sample in range(CHANGED_SAMPLES):
                records[sample] = {"n": next_n + sample, "id": f"changed-{sample}"}
                write_jsonl(source, records)
                wall, report = cli.run(*check_args(), timeout=60)
                assert_complete(report, SMALL_RECORDS)
                coverage = report["coverage"]
                if coverage["cache_misses"] != 1 or coverage["cache_hits"] != SMALL_RECORDS - 1:
                    raise RuntimeError(f"changed-row cache mismatch: {coverage}")
                changed_wall.append(wall)
                changed_elapsed.append(float(report.get("elapsed_ms") or 0))
            result["changed_one_record"] = {
                "wall_ms": summarize(changed_wall),
                "report_elapsed_ms": summarize(changed_elapsed),
                "cache_hits": SMALL_RECORDS - 1,
                "cache_misses": 1,
                "daemon_pid_unchanged": cli.daemon_pid() == daemon_pid,
            }
            result["idle_resources"] = idle_resources(cli)
            result["disk_small"] = disk_report(root)
            result["trace_overhead"] = trace_overhead(cli)
            cli.shutdown()

            cold_wall: list[float] = []
            cold_elapsed: list[float] = []
            for _ in range(COLD_SAMPLES):
                pid = cli.daemon_pid()
                if pid is not None and pid_alive(pid):
                    cli.shutdown()
                    gone = wait_pid_exit(pid, 15)
                    if gone is None and pid_alive(pid):
                        raise RuntimeError(f"daemon {pid} stayed up after shutdown")
                wall, report = cli.run(*check_args(), timeout=60)
                assert_complete(report, SMALL_RECORDS)
                coverage = report["coverage"]
                if coverage["cache_misses"] != 0:
                    raise RuntimeError(f"cold cached check missed: {coverage}")
                cold_wall.append(wall)
                cold_elapsed.append(float(report.get("elapsed_ms") or 0))
                cli.shutdown()
            result["cold_check_daemon_respawn"] = {
                "wall_ms": summarize(cold_wall),
                "report_elapsed_ms": summarize(cold_elapsed),
                "meaning": "CLI process plus daemon spawn plus a fully cached check",
            }
        finally:
            cli.shutdown()

        idle_env = base_env.copy()
        idle_env["CHECKWEAVE_IDLE_SECONDS"] = str(IDLE_SECONDS)
        idle_observed: list[float] = []
        idle_root = Path(temp) / "idle"
        idle_root.mkdir()
        idle = Cli(binary, idle_root, idle_env)
        try:
            idle.run("init", "--agent", "none")
            write_jsonl(idle_root / "rows.jsonl", [{"n": 2, "id": "only"}])
            for _ in range(IDLE_SAMPLES):
                existing = idle.daemon_pid()
                if existing is not None and pid_alive(existing):
                    idle.shutdown()
                    wait_pid_exit(existing, 15)
                _wall, report = idle.run(*check_args(), timeout=60)
                assert_complete(report, 1)
                pid = idle.daemon_pid()
                if pid is None:
                    raise RuntimeError("idle sample did not publish daemon.json")
                mark = time.perf_counter()
                gone_ms = wait_pid_exit(pid, IDLE_SECONDS + 20)
                if gone_ms is None:
                    raise RuntimeError(f"daemon {pid} did not exit after idle")
                # wait_pid_exit starts its own timer; recompute from mark for the full gap.
                idle_observed.append((time.perf_counter() - mark) * 1000)
                _ = gone_ms
            result["idle_exit"] = {
                "configured_idle_seconds": IDLE_SECONDS,
                "observed_from_check_return_ms": summarize(idle_observed),
                "meaning": "time from check process return until daemon pid is no longer alive",
            }
        finally:
            idle.shutdown()

        large_root = Path(temp) / "large"
        large_root.mkdir()
        large = Cli(binary, large_root, base_env)
        try:
            large.run("init", "--agent", "none")
            total_records = 0
            bodies = []
            for file_index in range(LARGE_FILES):
                rows = [
                    {"n": (file_index + record) % 7, "id": f"f{file_index}-{record:05d}"}
                    for record in range(LARGE_RECORDS_PER_FILE)
                ]
                path = large_root / f"part-{file_index}.jsonl"
                bodies.append(write_jsonl(path, rows))
                total_records += len(rows)
            large_hash = hashlib.sha256("".join(bodies).encode()).hexdigest()
            wall, report = large.run(
                "check",
                "--include",
                "**/*.jsonl",
                "--predicate",
                json.dumps(PREDICATE, separators=(",", ":")),
                "--max-results",
                "1",
                "--timeout-ms",
                "120000",
                timeout=180,
            )
            assert_complete(report, total_records)
            if report["coverage"]["cache_misses"] != total_records:
                raise RuntimeError(report["coverage"])
            result["disk_large"] = {
                "files": LARGE_FILES,
                "records": total_records,
                "input_sha256": large_hash,
                "first_check_wall_ms": round(wall, 3),
                "first_check_elapsed_ms": report.get("elapsed_ms"),
                "first_coverage": report["coverage"],
                "after_first_check": disk_report(large_root),
            }
            wall2, report2 = large.run(
                "check",
                "--include",
                "**/*.jsonl",
                "--predicate",
                json.dumps(PREDICATE, separators=(",", ":")),
                "--max-results",
                "1",
                "--timeout-ms",
                "120000",
                timeout=180,
            )
            assert_complete(report2, total_records)
            if report2["coverage"]["cache_misses"] != 0:
                raise RuntimeError(report2["coverage"])
            result["disk_large"]["second_check_wall_ms"] = round(wall2, 3)
            result["disk_large"]["second_check_elapsed_ms"] = report2.get("elapsed_ms")
            result["disk_large"]["second_coverage"] = report2["coverage"]
            result["disk_large"]["after_second_check"] = disk_report(large_root)
        finally:
            large.shutdown()

    result["cli_help"] = {"wall_ms": summarize(measure_help(binary, HELP_SAMPLES))}
    result["loadavg_end"] = loadavg()
    result["agent_comparison"] = {
        "status": "pending",
        "reason": "Paired Cursor actor runs are not part of this harness. No token or outcome comparison is claimed here.",
    }
    return result


def default_binary(repo: Path) -> Path:
    debug = repo / "target" / "debug" / "checkweave"
    release = repo / "target" / "release" / "checkweave"
    if debug.is_file():
        return debug
    if release.is_file():
        return release
    raise SystemExit("no checkweave binary at target/debug/checkweave or target/release/checkweave")


def main() -> None:
    repo = Path(__file__).resolve().parents[2]
    parser = argparse.ArgumentParser(description="Measure checkweave startup, reuse, idle, and disk")
    parser.add_argument("--binary", type=Path, default=None)
    parser.add_argument(
        "--parent",
        type=Path,
        default=Path(tempfile.gettempdir()),
        help="Directory that holds the temporary workspace. Use a physical filesystem for the qualification sample.",
    )
    parser.add_argument(
        "--out",
        type=Path,
        default=repo / "experiments" / "agent-tasks" / "results" / "debug-baseline.json",
    )
    args = parser.parse_args()
    binary = args.binary if args.binary is not None else default_binary(repo)
    if not binary.is_file():
        raise SystemExit(f"binary not found: {binary}")
    payload = run_suite(binary, args.parent)
    args.out.parent.mkdir(parents=True, exist_ok=True)
    args.out.write_text(json.dumps(payload, indent=2) + "\n")
    print(args.out)


if __name__ == "__main__":
    main()
