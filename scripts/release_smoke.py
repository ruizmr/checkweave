#!/usr/bin/env python3
"""Install a local release archive and exercise the packaged binary.

The harness uses only the Python 3 standard library. It keeps its caches,
configuration fixtures, and workspaces under a temporary directory it creates
and deletes. Installer destinations are explicit; HOME is not changed.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import shutil
import stat
import subprocess
import sys
import tarfile
import tempfile
import time
from pathlib import Path

COMMAND_TIMEOUT = 45
INSTALL_TIMEOUT = 60
DEADLINE_SECONDS = 240
PREDICATE = json.dumps({"op": "gt", "path": "/n", "value": 1})
REQUIRED_MEMBERS = (
    "checkweave",
    "LICENSE",
    "README.md",
    "CONTRIBUTING.md",
    "docs/README.md",
    "docs/getting-started.md",
    "docs/mcp.md",
    "docs/usage.md",
    "docs/platforms.md",
    "docs/roadmap.md",
)


class SmokeFailure(Exception):
    pass


def fail(message: str) -> None:
    raise SmokeFailure(message)


def emit(summary: dict) -> None:
    print(json.dumps(summary, sort_keys=True, separators=(",", ":")))


def sha256_file(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        for chunk in iter(lambda: handle.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def normalize_checksum(value: str) -> str:
    text = "".join(value.split()).lower()
    if len(text) != 64 or any(ch not in "0123456789abcdef" for ch in text):
        fail(f"checksum must be 64 hex digits, got {value!r}")
    return text


def archive_members(archive: Path) -> tuple[str, dict[str, bytes]]:
    try:
        tar = tarfile.open(archive, "r:gz")
    except (tarfile.TarError, OSError) as exc:
        fail(f"cannot read archive {archive}: {exc}")
    files: dict[str, bytes] = {}
    top: set[str] = set()
    with tar:
        for member in tar.getmembers():
            name = member.name
            if name.startswith("/") or "\\" in name or ".." in Path(name).parts:
                fail(f"archive contains an unsafe path: {name}")
            parts = Path(name).parts
            if not parts:
                continue
            top.add(parts[0])
            if not member.isfile():
                continue
            extracted = tar.extractfile(member)
            if extracted is None:
                fail(f"archive member has no content: {name}")
            files[name] = extracted.read()
    if len(top) != 1:
        fail(f"archive must contain one top-level directory, found {sorted(top)}")
    root = next(iter(top))
    binaries = [name for name in files if Path(name).name == "checkweave"]
    if len(binaries) != 1:
        fail(f"archive must contain exactly one checkweave binary, found {binaries}")
    missing = [f"{root}/{rel}" for rel in REQUIRED_MEMBERS if f"{root}/{rel}" not in files]
    if missing:
        fail("archive is missing " + ", ".join(missing))
    return binaries[0], files


def child_env(tmp: Path) -> dict[str, str]:
    (tmp / "config").mkdir(parents=True, exist_ok=True)
    (tmp / "cache").mkdir(parents=True, exist_ok=True)
    (tmp / "cwcache").mkdir(parents=True, exist_ok=True)
    path = os.environ.get("PATH")
    if not path:
        fail("PATH is required to locate python3 and sh")
    return {
        "PATH": path,
        "LOGNAME": os.environ.get("LOGNAME") or os.environ.get("USER") or "checkweave",
        "USER": os.environ.get("USER") or "checkweave",
        "TMPDIR": str(tmp),
        "XDG_CACHE_HOME": str(tmp / "cache"),
        "XDG_CONFIG_HOME": str(tmp / "config"),
        "CHECKWEAVE_CACHE_DIR": str(tmp / "cwcache"),
        "LANG": os.environ.get("LANG") or "C.UTF-8",
        "PYTHONDONTWRITEBYTECODE": "1",
    }


def run_cmd(
    argv: list[str],
    *,
    cwd: Path,
    env: dict[str, str],
    timeout: float,
    deadline: float,
) -> subprocess.CompletedProcess[str]:
    remaining = deadline - time.monotonic()
    if remaining <= 0:
        fail(f"deadline exceeded before {' '.join(argv)}")
    try:
        return subprocess.run(
            argv,
            cwd=cwd,
            env=env,
            timeout=min(timeout, remaining),
            text=True,
            capture_output=True,
            check=False,
        )
    except subprocess.TimeoutExpired as exc:
        fail(f"timed out after {timeout}s: {' '.join(argv)}")
        raise AssertionError(exc) from exc


def expect_ok(result: subprocess.CompletedProcess[str], what: str) -> str:
    if result.returncode != 0:
        fail(
            f"{what} failed ({result.returncode})\n"
            f"stdout: {result.stdout[-800:]}\nstderr: {result.stderr[-800:]}"
        )
    return result.stdout


def expect_json(result: subprocess.CompletedProcess[str], what: str) -> dict:
    text = expect_ok(result, what)
    try:
        payload = json.loads(text)
    except json.JSONDecodeError as exc:
        fail(f"{what} did not return JSON: {exc}; stdout={text[-800:]!r}")
    if not isinstance(payload, dict):
        fail(f"{what} JSON was not an object")
    return payload


def check_collection(binary: Path, workspace: Path, env: dict[str, str], deadline: float) -> dict:
    return expect_json(
        run_cmd(
            [
                str(binary),
                "--workspace",
                str(workspace),
                "check",
                "--include",
                "items.jsonl",
                "--predicate",
                PREDICATE,
            ],
            cwd=workspace.parent,
            env=env,
            timeout=COMMAND_TIMEOUT,
            deadline=deadline,
        ),
        "check",
    )


def assert_check(report: dict, *, matched: int, hits: int, misses: int) -> None:
    coverage = report.get("coverage")
    if not isinstance(coverage, dict):
        fail(f"check report has no coverage: {report}")
    sources = report.get("sources")
    if not isinstance(sources, list) or len(sources) != 1:
        fail(f"expected one source, got {sources!r}")
    if report.get("execution") != "complete" or report.get("freshness") != "validated":
        fail(f"check was not complete and validated: {report}")
    if coverage.get("matched") != matched or coverage.get("evaluated") != 3:
        fail(f"unexpected match counts: {coverage}")
    if coverage.get("cache_hits") != hits or coverage.get("cache_misses") != misses:
        fail(f"unexpected cache counts: {coverage}")
    if coverage.get("files") != 1 or coverage.get("records") != 3:
        fail(f"unexpected file or record counts: {coverage}")


def install_archive(
    script: Path,
    archive: Path,
    checksum: str,
    bin_dir: Path,
    env: dict[str, str],
    deadline: float,
) -> subprocess.CompletedProcess[str]:
    return run_cmd(
        [
            "sh",
            str(script),
            "--archive",
            str(archive),
            "--checksum",
            checksum,
            "--bin-dir",
            str(bin_dir),
        ],
        cwd=bin_dir.parent,
        env=env,
        timeout=INSTALL_TIMEOUT,
        deadline=deadline,
    )


def functional(
    binary: Path,
    workspace: Path,
    env: dict[str, str],
    deadline: float,
    checks: list[str],
    summary: dict,
) -> None:
    (workspace / "keep.txt").write_text("keep-workspace\n", encoding="utf-8")
    rules = workspace / ".cursor" / "rules"
    rules.mkdir(parents=True)
    (rules / "unrelated.mdc").write_text("keep-unrelated-rule\n", encoding="utf-8")
    mcp_path = workspace / ".cursor" / "mcp.json"
    mcp_path.write_text(
        json.dumps(
            {"mcpServers": {"other": {"command": "unrelated-server", "args": ["--stay"]}}}
        )
        + "\n",
        encoding="utf-8",
    )
    config_marker = Path(env["XDG_CONFIG_HOME"]) / "unrelated.txt"
    config_marker.write_text("keep-config\n", encoding="utf-8")

    empty = workspace.parent / "empty"
    empty.mkdir()
    expect_json(
        run_cmd(
            [str(binary), "--workspace", str(workspace), "init", "--agent", "cursor"],
            cwd=empty,
            env=env,
            timeout=COMMAND_TIMEOUT,
            deadline=deadline,
        ),
        "init",
    )
    mcp = json.loads(mcp_path.read_text(encoding="utf-8"))
    servers = mcp.get("mcpServers", {})
    if "other" not in servers or "checkweave" not in servers:
        fail(f"init did not keep the unrelated server and add checkweave: {mcp}")
    if (rules / "unrelated.mdc").read_text(encoding="utf-8") != "keep-unrelated-rule\n":
        fail("init changed the unrelated rule")
    if not (rules / "checkweave.mdc").is_file():
        fail("init did not write the managed rule")
    (workspace / ".checkweave" / "user-state.txt").write_text("keep-state\n", encoding="utf-8")
    checks.append("init-preserves-unrelated")

    source = workspace / "items.jsonl"
    source.write_text('{"n":1}\n{"n":2}\n{"n":3}\n', encoding="utf-8")
    cold = check_collection(binary, workspace, env, deadline)
    assert_check(cold, matched=2, hits=0, misses=3)
    checks.append("cold-check")
    warm = check_collection(binary, workspace, env, deadline)
    assert_check(warm, matched=2, hits=3, misses=0)
    checks.append("warm-check")
    source.write_text('{"n":1}\n{"n":0}\n{"n":3}\n', encoding="utf-8")
    edited = check_collection(binary, workspace, env, deadline)
    assert_check(edited, matched=1, hits=2, misses=1)
    checks.append("one-row-edit")

    previous = expect_json(
        run_cmd(
            [str(binary), "--workspace", str(workspace), "evidence", cold["id"]],
            cwd=empty,
            env=env,
            timeout=COMMAND_TIMEOUT,
            deadline=deadline,
        ),
        "evidence previous",
    )
    current = expect_json(
        run_cmd(
            [str(binary), "--workspace", str(workspace), "evidence", edited["id"]],
            cwd=empty,
            env=env,
            timeout=COMMAND_TIMEOUT,
            deadline=deadline,
        ),
        "evidence current",
    )
    if previous.get("freshness") != "stale" or len(previous.get("sources", [])) != 1:
        fail(f"previous evidence was not stale with one source: {previous}")
    if current.get("freshness") != "validated" or len(current.get("sources", [])) != 1:
        fail(f"current evidence was not validated with one source: {current}")
    checks.append("evidence-freshness")
    summary["coverage"] = {
        "cold_hits": cold["coverage"]["cache_hits"],
        "warm_hits": warm["coverage"]["cache_hits"],
        "edited_hits": edited["coverage"]["cache_hits"],
        "edited_misses": edited["coverage"]["cache_misses"],
    }
    summary["sources"] = 1
    summary["freshness"] = {
        "cold": cold["freshness"],
        "previous_evidence": previous["freshness"],
        "edited_evidence": current["freshness"],
    }

    if shutil.which("python3", path=env["PATH"]) is None:
        checks.append("compare-skipped")
        checks.append("trace-skipped")
    else:
        (workspace / "before.py").write_text(
            "import json,sys\n"
            "value=json.load(sys.stdin)\n"
            "print(json.dumps({'total': sum(n for n in value['values'] if n >= 0)}))\n",
            encoding="utf-8",
        )
        (workspace / "after.py").write_text(
            "import json,sys\n"
            "value=json.load(sys.stdin)\n"
            "print(json.dumps({'total': sum(value['values'])}))\n",
            encoding="utf-8",
        )
        request = {
            "before": {"argv": ["python3", "before.py"], "sources": ["before.py"]},
            "after": {"argv": ["python3", "after.py"], "sources": ["after.py"]},
            "inputs": [{"values": [1, -2, 3]}],
        }
        (workspace / "compare.json").write_text(json.dumps(request), encoding="utf-8")
        compared = expect_json(
            run_cmd(
                [
                    str(binary),
                    "--workspace",
                    str(workspace),
                    "compare",
                    "--request-file",
                    str(workspace / "compare.json"),
                ],
                cwd=empty,
                env=env,
                timeout=COMMAND_TIMEOUT,
                deadline=deadline,
            ),
            "compare",
        )
        if compared.get("outcome") != "observed_difference":
            fail(f"compare outcome was {compared.get('outcome')!r}")
        checks.append("compare")

        (workspace / "app.py").write_text(
            "import json,sys\n"
            "value=json.load(sys.stdin)\n"
            "print(json.dumps({'n': value['n'] + 1}))\n",
            encoding="utf-8",
        )
        (workspace / "trace.json").write_text(
            json.dumps({"script": "app.py", "input": {"n": 1}, "baseline": False}),
            encoding="utf-8",
        )
        traced = expect_json(
            run_cmd(
                [
                    str(binary),
                    "--workspace",
                    str(workspace),
                    "trace",
                    "--request-file",
                    str(workspace / "trace.json"),
                ],
                cwd=empty,
                env=env,
                timeout=COMMAND_TIMEOUT,
                deadline=deadline,
            ),
            "trace",
        )
        if traced.get("execution") != "complete":
            fail(f"trace execution was {traced.get('execution')!r}")
        helper = Path(env["CHECKWEAVE_CACHE_DIR"]) / "trace-helper" / "trace-python-v1" / "checkweave_trace.py"
        if not helper.is_file() or helper.stat().st_size < 64:
            fail(f"embedded trace helper was not materialized at {helper}")
        checks.append("trace-helper")
        summary["trace_events"] = traced.get("event_total")


def shutdown(binary: Path, workspace: Path, env: dict[str, str]) -> None:
    try:
        subprocess.run(
            [str(binary), "--workspace", str(workspace), "shutdown"],
            cwd=workspace.parent,
            env=env,
            timeout=20,
            text=True,
            capture_output=True,
            check=False,
        )
    except (OSError, subprocess.TimeoutExpired):
        return


def parse_args(argv: list[str]) -> argparse.Namespace:
    parser = argparse.ArgumentParser(description="Smoke-test a local checkweave release archive.")
    parser.add_argument("--archive", required=True, type=Path)
    parser.add_argument("--checksum", required=True)
    parser.add_argument("--install-script", type=Path)
    parser.add_argument("--uninstall-script", type=Path)
    parser.add_argument("--bin", type=Path, help="Existing checkweave from an earlier installer run")
    parser.add_argument("--skip-installer", action="store_true")
    parser.add_argument("--skip-uninstall", action="store_true")
    args = parser.parse_args(argv)
    if args.skip_installer and (args.bin is None or not args.skip_uninstall):
        parser.error("--skip-installer requires --bin and --skip-uninstall")
    if args.bin is not None and not args.skip_installer:
        parser.error("--bin is only valid with --skip-installer")
    if args.skip_uninstall and not args.skip_installer:
        parser.error("--skip-uninstall requires --skip-installer")
    return args


def main(argv: list[str]) -> int:
    args = parse_args(argv)
    checks: list[str] = []
    summary: dict = {"ok": False, "checks": checks, "archive": str(args.archive)}
    parent = Path("/tmp") if Path("/tmp").is_dir() and os.access("/tmp", os.W_OK) else None
    tmp = Path(tempfile.mkdtemp(prefix="cws", dir=parent))
    env = child_env(tmp)
    workspace: Path | None = None
    binary: Path | None = None
    deadline = time.monotonic() + DEADLINE_SECONDS
    try:
        archive = args.archive.resolve()
        if not archive.is_file():
            fail(f"archive not found: {archive}")
        expected = normalize_checksum(args.checksum)
        actual = sha256_file(archive)
        if actual != expected:
            fail(f"checksum mismatch for {archive}: {actual} != {expected}")
        summary["checksum"] = expected
        member, files = archive_members(archive)
        packaged = files[member]
        checks.append("archive-members")

        here = Path(__file__).resolve().parent
        install_script = (args.install_script or here / "install.sh").resolve()
        uninstall_script = (args.uninstall_script or here / "uninstall.sh").resolve()
        bin_dir = tmp / "bin"
        bin_dir.mkdir()
        installed = bin_dir / "checkweave"

        if args.skip_installer:
            given = args.bin.resolve()
            if not given.is_file():
                fail(f"installed binary not found: {given}")
            if sha256_file(given) != hashlib.sha256(packaged).hexdigest():
                fail("installed binary does not match the archive member")
            installed = given
            checks.append("installer-skipped")
        else:
            if not install_script.is_file() or not uninstall_script.is_file():
                fail(f"installer scripts not found next to {here}")
            first = install_archive(install_script, archive, expected, bin_dir, env, deadline)
            expect_ok(first, "install")
            if not installed.is_file():
                fail(f"installer did not write {installed}")
            if sha256_file(installed) != hashlib.sha256(packaged).hexdigest():
                fail("installed binary does not match the archive member")
            before = installed.stat()
            checks.append("install")

            second = install_archive(install_script, archive, expected, bin_dir, env, deadline)
            text = expect_ok(second, "reinstall")
            if "already installed" not in text:
                fail(f"reinstall did not report already installed: {text!r}")
            after = installed.stat()
            if sha256_file(installed) != hashlib.sha256(packaged).hexdigest():
                fail("reinstall changed the binary bytes")
            if before.st_ino != after.st_ino or before.st_mtime_ns != after.st_mtime_ns:
                fail("reinstall replaced a binary that already matched")
            checks.append("reinstall-same-bytes")

            intact = sha256_file(installed)
            bad_archive = "f" * 64 if expected != "f" * 64 else "e" * 64
            rejected = install_archive(install_script, archive, bad_archive, bin_dir, env, deadline)
            if rejected.returncode == 0:
                fail("wrong checksum was accepted")
            if sha256_file(installed) != intact or installed.stat().st_mtime_ns != after.st_mtime_ns:
                fail("wrong checksum modified the installed binary")
            checks.append("wrong-checksum-keeps-binary")

            installed.write_text("#!/bin/sh\necho stub\n", encoding="utf-8")
            installed.chmod(installed.stat().st_mode | stat.S_IEXEC)
            upgraded = install_archive(install_script, archive, expected, bin_dir, env, deadline)
            expect_ok(upgraded, "upgrade")
            if sha256_file(installed) != hashlib.sha256(packaged).hexdigest():
                fail("upgrade did not replace the stub with the archive binary")
            checks.append("upgrade-replaces-stub")

        reloc_dir = tmp / "reloc"
        reloc_dir.mkdir()
        binary = reloc_dir / "checkweave"
        shutil.copy2(installed, binary)
        binary.chmod(0o755)
        if sha256_file(binary) != hashlib.sha256(packaged).hexdigest():
            fail("relocated binary does not match the archive")
        # Run outside the checkout and outside the directory that holds the archive.
        summary["binary"] = str(binary)
        checks.append("relocated-binary")
        workspace = tmp / "ws"
        workspace.mkdir()
        functional(binary, workspace, env, deadline, checks, summary)

        shutdown(binary, workspace, env)
        checks.append("shutdown")
        deinit = expect_json(
            run_cmd(
                [str(binary), "--workspace", str(workspace), "deinit"],
                cwd=tmp / "empty",
                env=env,
                timeout=COMMAND_TIMEOUT,
                deadline=deadline,
            ),
            "deinit",
        )
        if deinit.get("state") != "kept":
            fail(f"deinit did not keep state: {deinit}")
        mcp = json.loads((workspace / ".cursor" / "mcp.json").read_text(encoding="utf-8"))
        servers = mcp.get("mcpServers", {})
        if "other" not in servers or "checkweave" in servers:
            fail(f"deinit did not preserve only the unrelated server: {mcp}")
        rules = workspace / ".cursor" / "rules"
        if (rules / "unrelated.mdc").read_text(encoding="utf-8") != "keep-unrelated-rule\n":
            fail("deinit changed the unrelated rule")
        if (rules / "checkweave.mdc").exists():
            fail("deinit left the managed rule in place")
        if (workspace / ".checkweave" / "user-state.txt").read_text(encoding="utf-8") != "keep-state\n":
            fail("deinit removed unrelated workspace state")
        if not (workspace / ".checkweave" / "workspace.json").is_file():
            fail("deinit removed workspace.json")
        if (workspace / "keep.txt").read_text(encoding="utf-8") != "keep-workspace\n":
            fail("deinit changed an unrelated workspace file")
        if (Path(env["XDG_CONFIG_HOME"]) / "unrelated.txt").read_text(encoding="utf-8") != "keep-config\n":
            fail("deinit changed unrelated config")
        checks.append("deinit-preserves-unrelated")
        workspace = None

        if not args.skip_uninstall:
            neighbor = bin_dir / "neighbor-tool"
            neighbor.write_text("neighbor\n", encoding="utf-8")
            marker = Path(env["XDG_CONFIG_HOME"]) / "unrelated.txt"
            removed = run_cmd(
                ["sh", str(uninstall_script), "--bin-dir", str(bin_dir)],
                cwd=tmp / "empty",
                env=env,
                timeout=COMMAND_TIMEOUT,
                deadline=deadline,
            )
            expect_ok(removed, "uninstall")
            if installed.exists():
                fail("uninstall left the selected binary")
            if neighbor.read_text(encoding="utf-8") != "neighbor\n":
                fail("uninstall changed the neighboring file")
            if marker.read_text(encoding="utf-8") != "keep-config\n":
                fail("uninstall changed unrelated config")
            checks.append("uninstall")
            again = run_cmd(
                ["sh", str(uninstall_script), "--bin-dir", str(bin_dir)],
                cwd=tmp / "empty",
                env=env,
                timeout=COMMAND_TIMEOUT,
                deadline=deadline,
            )
            text = expect_ok(again, "uninstall again")
            if "no binary" not in text:
                fail(f"second uninstall did not report a missing binary: {text!r}")
            if neighbor.read_text(encoding="utf-8") != "neighbor\n":
                fail("second uninstall changed the neighboring file")
            checks.append("uninstall-again")
        else:
            checks.append("uninstall-skipped")

        summary["ok"] = True
        emit(summary)
        return 0
    except SmokeFailure as exc:
        summary["error"] = str(exc)
        emit(summary)
        return 1
    finally:
        if workspace is not None and binary is not None and binary.is_file():
            shutdown(binary, workspace, env)
        if tmp.name.startswith("cws") and tmp.is_dir():
            shutil.rmtree(tmp, ignore_errors=True)


if __name__ == "__main__":
    try:
        sys.exit(main(sys.argv[1:]))
    except SmokeFailure as exc:
        emit({"ok": False, "error": str(exc), "checks": []})
        sys.exit(1)
