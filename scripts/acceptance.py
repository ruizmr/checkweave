#!/usr/bin/env python3
"""Black-box lifecycle and freshness gates. Run after cargo build.

Uses only synthetic temporary workspaces and the Python standard library.
"""

import argparse
import concurrent.futures
import json
import os
from pathlib import Path
import shutil
import signal
import statistics
import subprocess
import tempfile
import time


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, default=Path("target/debug/checkweave"))
    parser.add_argument("--output", type=Path)
    args = parser.parse_args()
    binary = str(args.binary.resolve())
    checks = []
    timings = []
    roots = []

    def command(root, *argv, success=True, env=None):
        start = time.perf_counter()
        result = subprocess.run(
            [binary, "--workspace", str(root), *argv],
            capture_output=True, text=True, timeout=45, env=env,
        )
        if argv[0] == "check":
            timings.append(1000 * (time.perf_counter() - start))
        if success:
            assert result.returncode == 0, (argv, result.stdout, result.stderr)
            return json.loads(result.stdout)
        return result

    predicate = json.dumps({"op": "gt", "path": "/n", "value": 1})

    def check(root):
        return command(root, "check", "--include", "**/*.jsonl", "--predicate", predicate)

    def assert_counts(report, matched, total):
        assert report["execution"] == "complete", report
        assert report["freshness"] == "validated", report
        assert report["coverage"]["matched"] == matched, report
        assert report["coverage"]["evaluated"] == total, report

    with tempfile.TemporaryDirectory(prefix="checkweave-acceptance-") as temp:
        root = Path(temp) / "main"
        root.mkdir()
        roots.append(root)
        try:
            command(root, "init", "--agent", "none")
            command(root, "init", "--agent", "none")
            source = root / "items.jsonl"
            source.write_text('{"n":1}\n{"n":2}\n{"n":3}\n')
            cold = check(root)
            assert_counts(cold, 2, 3)
            warm = check(root)
            assert_counts(warm, 2, 3)
            assert warm["coverage"]["cache_hits"] == 3, warm
            checks.append("cold/warm coverage and unchanged reuse")

            metadata = source.stat()
            source.write_text('{"n":1}\n{"n":0}\n{"n":3}\n')
            os.utime(source, ns=(metadata.st_atime_ns, metadata.st_mtime_ns))
            edited = check(root)
            assert_counts(edited, 1, 3)
            assert edited["coverage"]["cache_hits"] == 2, edited
            assert command(root, "evidence", cold["id"])["freshness"] == "stale"
            checks.append("same-size/same-mtime change and stale evidence")

            extra = root / "new.jsonl"
            extra.write_text('{"n":4}\n')
            assert_counts(check(root), 2, 4)
            extra.rename(root / "renamed.jsonl")
            renamed = check(root)
            assert_counts(renamed, 2, 4)
            assert any(s["path"] == "renamed.jsonl" for s in renamed["sources"])
            (root / "renamed.jsonl").unlink()
            assert_counts(check(root), 1, 3)
            checks.append("membership creation/rename/deletion")

            extra.write_text('{"n":5}\n')
            (root / ".ignore").write_text("new.jsonl\n")
            assert_counts(check(root), 1, 3)
            (root / ".ignore").write_text("")
            assert_counts(check(root), 2, 4)
            checks.append("ignore-rule change invalidates membership")

            command(root, "shutdown")
            time.sleep(0.2)
            assert_counts(check(root), 2, 4)
            checks.append("worker restart preserves correct reuse")

            concurrent_root = Path(temp) / "concurrent"
            concurrent_root.mkdir()
            roots.append(concurrent_root)
            command(concurrent_root, "init", "--agent", "none")
            (concurrent_root / "data.jsonl").write_text('{"n":7}\n')
            with concurrent.futures.ThreadPoolExecutor(max_workers=8) as pool:
                results = list(pool.map(lambda _: check(concurrent_root), range(8)))
            for result in results:
                assert_counts(result, 1, 1)
            misses_by_id = {}
            for result in results:
                misses_by_id.setdefault(result["id"], result["coverage"]["cache_misses"])
            assert sum(misses_by_id.values()) == 1, results
            checks.append("eight simultaneous cold clients share work")

            if shutil.which("git"):
                gitroot = Path(temp) / "git"
                gitroot.mkdir()
                roots.append(gitroot)

                def git(*argv):
                    result = subprocess.run(["git", "-C", str(gitroot), *argv],
                                            capture_output=True, text=True, timeout=15)
                    assert result.returncode == 0, result.stderr
                    return result.stdout.strip()

                git("init", "-q")
                (gitroot / "data.jsonl").write_text('{"n":2}\n')
                git("add", "data.jsonl")
                git("-c", "user.name=Checkweave Test", "-c", "user.email=test@example.invalid",
                    "commit", "-qm", "first")
                first = git("rev-parse", "HEAD")
                (gitroot / "data.jsonl").write_text('{"n":0}\n')
                git("add", "data.jsonl")
                git("-c", "user.name=Checkweave Test", "-c", "user.email=test@example.invalid",
                    "commit", "-qm", "second")
                command(gitroot, "init", "--agent", "none")
                assert_counts(check(gitroot), 0, 1)
                git("checkout", "-q", first)
                assert_counts(check(gitroot), 1, 1)
                linked = Path(temp) / "linked"
                git("worktree", "add", "--detach", str(linked), "HEAD")
                roots.append(linked)
                command(linked, "init", "--agent", "none")
                (linked / "data.jsonl").write_text('{"n":0}\n')
                assert_counts(check(linked), 0, 1)
                assert_counts(check(gitroot), 1, 1)
                checks.append("Git checkout and isolated linked worktrees")

            lifecycle = Path(temp) / "lifecycle"
            lifecycle.mkdir()
            roots.append(lifecycle)
            command(lifecycle, "init", "--agent", "none")
            (lifecycle / "items.jsonl").write_text('{"n":2}\n{"n":0}\n')
            with concurrent.futures.ThreadPoolExecutor(max_workers=4) as pool:
                statuses = list(pool.map(lambda _: command(lifecycle, "status"), range(4)))
            pids = {s.get("pid") for s in statuses}
            assert len(pids) == 1 and None not in pids, statuses
            watcher = statuses[0].get("watcher")
            command(lifecycle, "shutdown")
            assert_counts(check(lifecycle), 1, 2)
            checks.append("concurrent startup, immediate shutdown, reconnect")

            status = command(lifecycle, "status")
            pid = int(status["pid"])
            os.kill(pid, signal.SIGKILL)
            deadline = time.time() + 5
            while time.time() < deadline and Path(f"/proc/{pid}").exists():
                time.sleep(0.05)
            assert not Path(f"/proc/{pid}").exists(), pid
            assert_counts(check(lifecycle), 1, 2)
            checks.append("controlled crash of the workspace daemon and reconnect")

            idle_root = Path(temp) / "idle"
            idle_root.mkdir()
            roots.append(idle_root)
            idle_env = os.environ.copy()
            idle_env["CHECKWEAVE_IDLE_SECONDS"] = "1"
            command(idle_root, "init", "--agent", "none", env=idle_env)
            (idle_root / "items.jsonl").write_text('{"n":2}\n')
            command(idle_root, "check", "--include", "*.jsonl", "--predicate", predicate, env=idle_env)
            idle_status = command(idle_root, "status", env=idle_env)
            idle_pid = int(idle_status["pid"])
            idle_deadline = time.time() + 20
            while time.time() < idle_deadline and Path(f"/proc/{idle_pid}").exists():
                time.sleep(0.1)
            assert not Path(f"/proc/{idle_pid}").exists(), idle_pid
            assert_counts(check(idle_root), 1, 1)
            checks.append("idle shutdown observed, then reconnect")
            checks.append(f"watcher mode during lifecycle: {watcher}")
        finally:
            for workspace in roots:
                try:
                    command(workspace, "shutdown", success=False)
                except subprocess.TimeoutExpired:
                    pass
    report = {
        "passed": checks,
        "check_calls": len(timings),
        "cli_roundtrip_median_ms": round(statistics.median(timings), 2) if timings else None,
        "cli_roundtrip_max_ms": round(max(timings), 2) if timings else None,
        "scope_coverage": {
            "harness_checks_passed": len(checks),
            "concurrent_misses_counted_per_unique_report_id": True,
            "query_fingerprint_without_waiting_for_poll": "same-size/same-mtime edit",
            "background_poll_forced": False,
            "background_poll_reason": "No supported switch disables native events; the query path re-reads bytes. The periodic poll is not separately timed.",
            "performance": "cli_roundtrip_* are synthetic debug-binary samples, not a release benchmark",
        },
        "note": "Synthetic small collections; shared host, debug binary unless specified.",
    }
    if args.output:
        args.output.write_text(json.dumps(report, indent=2) + "\n")
    print(json.dumps(report, indent=2))


if __name__ == "__main__":
    main()
