#!/usr/bin/env python3
"""Run a bounded discovery/guided-use sample with fresh Cursor CLI actors.

Prepare once, run each arm once, then summarize. Logs and private submissions
stay outside the repository; the summary records hashes, grades, and usage.
This is a small usability experiment, not a statistical efficacy benchmark.
"""
from __future__ import annotations

import argparse
import hashlib
import json
import os
from pathlib import Path
import shutil
import signal
import subprocess
import time

from collect_cursor_usage import parse_log
from validate_submission import grade

HERE = Path(__file__).resolve().parent
ARMS = ("baseline", "discovery", "guided")
TOOLS = ("check", "compare", "trace", "replay", "semantic", "model_evaluate")


def digest(path: Path) -> str:
    return hashlib.sha256(path.read_bytes()).hexdigest()


def tree_digest(root: Path) -> str:
    result = hashlib.sha256()
    for path in sorted(root.rglob("*")):
        if path.is_file() and "__pycache__" not in path.parts:
            result.update(path.relative_to(root).as_posix().encode() + b"\0")
            result.update(path.read_bytes() + b"\0")
    return result.hexdigest()


def save(path: Path, value: object) -> None:
    path.write_text(json.dumps(value, indent=2) + "\n")


def prepare(root: Path, binary: Path, model: str) -> None:
    binary = binary.resolve(strict=True)
    # Refuse reuse: an interrupted or completed actor must never be overwritten.
    root.mkdir(parents=True, exist_ok=False)
    (root / "bin").mkdir()
    copied = root / "bin/checkweave"
    shutil.copy2(binary, copied)
    task_hashes = {}
    for arm in ARMS:
        workspace = root / arm
        workspace.mkdir()
        shutil.copytree(HERE / "fixtures", workspace / "TASK",
                        ignore=shutil.ignore_patterns("__pycache__", "*.pyc"))
        (workspace / "submission").mkdir()
        task_hashes[arm] = tree_digest(workspace / "TASK")
        if arm != "baseline":
            subprocess.run([str(copied), "--workspace", str(workspace), "init"],
                           check=True, capture_output=True, timeout=30)
    assert len(set(task_hashes.values())) == 1
    save(root / "manifest.json", {
        "model": model, "binary_sha256": digest(copied),
        "source_commit": subprocess.check_output(
            ["git", "rev-parse", "HEAD"], cwd=HERE, text=True).strip(),
        "working_tree_diff_sha256": hashlib.sha256(subprocess.check_output(
            ["git", "diff", "HEAD"], cwd=HERE)).hexdigest(),
        "task_hashes": task_hashes,
        "rule_sha256": digest(root / "discovery/.cursor/rules/checkweave.mdc"),
        "design": "One baseline, one discovery arm, and one guided arm; fresh sessions, same tasks/model. No statistical inference.",
    })


def run_arm(root: Path, arm: str, agent: str, timeout: int) -> None:
    manifest = json.loads((root / "manifest.json").read_text())
    workspace = root / arm
    log_path = workspace / "agent-stream.jsonl"
    prompt = (
        "Read TASK/BRIEF.md and complete all three tasks. Write the specified "
        "submission files. Work only in this workspace; do not modify TASK/. "
        "Do not access other workspaces, external sources, or graders. "
        "No source index exists for these fixtures; do not use source-index or network tools. "
        "Use the available tools as appropriate."
    )
    if arm == "guided":
        prompt += (
            " Use Checkweave for policy-violation checking, for the behavior "
            "comparison (create small JSON stdin/stdout wrappers if needed), "
            "and for tracing the originating Python exception. You may combine "
            "its results with Python/file reads for parts the tools do not cover. "
            "Do not set up a decision model; these tasks need no inference model."
        )
    env = os.environ.copy()
    # Keep the ordinary executable environment equal, with Checkweave added only
    # for treatment arms. Do not change HOME or account/credential configuration.
    base_path = "/usr/local/bin:/usr/bin:/bin"
    if shutil.which("checkweave", path=base_path):
        raise RuntimeError("baseline PATH unexpectedly includes Checkweave")
    node = shutil.which("node")
    if node:
        node_dir = str(Path(node).parent)
        if shutil.which("checkweave", path=node_dir):
            raise RuntimeError("node PATH also exposes Checkweave to baseline")
        base_path = node_dir + os.pathsep + base_path
    env["PATH"] = ((str(root / "bin") + os.pathsep) if arm != "baseline" else "") + base_path
    env["PYTHONDONTWRITEBYTECODE"] = "1"
    env["CHECKWEAVE_CACHE_DIR"] = str(workspace / "user-cache")
    args = [agent, "--print", "--output-format", "stream-json", "--trust", "--force",
            "--sandbox", "disabled", "--approve-mcps", "--model", manifest["model"],
            "--workspace", str(workspace), prompt]
    started = time.time()
    with log_path.open("x") as log:
        save(workspace / "invocation.json", {"args": args, "started_at": started})
        process = subprocess.Popen(args, cwd=workspace, env=env, stdout=log,
                                   stderr=subprocess.STDOUT, start_new_session=True)
        try:
            result = process.wait(timeout=timeout)
            outcome = {"returncode": result, "timed_out": False}
        except subprocess.TimeoutExpired:
            os.killpg(process.pid, signal.SIGKILL)
            process.wait()
            outcome = {"returncode": None, "timed_out": True}
        finally:
            wall_ms = round((time.time() - started) * 1000)
            if arm != "baseline":
                subprocess.run([str(root / "bin/checkweave"), "--workspace",
                                str(workspace), "shutdown"], capture_output=True, timeout=30)
    outcome["wall_ms"] = wall_ms
    save(workspace / "process-result.json", outcome)
    print(json.dumps({"arm": arm, **outcome}))


def summarize(root: Path, out: Path) -> None:
    manifest = json.loads((root / "manifest.json").read_text())
    results = {}
    for arm in ARMS:
        workspace = root / arm
        log = workspace / "agent-stream.jsonl"
        final = workspace / "process-result.json"
        if not final.exists():
            raise RuntimeError(f"{arm} has no terminal process result; do not summarize a live run")
        parsed = parse_log(log)
        mcp_calls = []
        external_mcp_calls = []
        mcp_results = []
        for line in log.read_text().splitlines():
            try:
                event = json.loads(line)
            except ValueError:
                continue
            if event.get("type") != "tool_call" or event.get("subtype") != "completed":
                continue
            call = event.get("tool_call", {}).get("mcpToolCall", {})
            name = call.get("args", {}).get("name", "")
            if "checkweave" in name:
                success = call.get("result", {}).get("success", {})
                blocks = success.get("content", [])
                texts = [block.get("text", {}).get("text", "") for block in blocks]
                mcp_results.append({"name": name, "is_error": success.get("isError"),
                                    "text_blocks": len(texts), "summary": texts[0] if texts else "",
                                    "text_bytes": sum(len(t.encode()) for t in texts)})
            if any(name.endswith("checkweave_" + tool) for tool in TOOLS):
                mcp_calls.append(name)
            elif name and "checkweave" not in name:
                external_mcp_calls.append(name)
        from collect_cursor_usage import checkweave_shell_use
        shell = checkweave_shell_use(log)
        unchanged = tree_digest(workspace / "TASK") == manifest["task_hashes"][arm]
        grading = grade(workspace / "submission")
        results[arm] = {
            "process": json.loads(final.read_text()), "usage": parsed,
            "completed_mcp_operation_calls": mcp_calls, "mcp_results": mcp_results, "cli_use": shell,
            "external_mcp_calls": external_mcp_calls,
            "fixture_unchanged": unchanged, "grading": grading,
            "accepted_submission": unchanged and grading["passed"] and not external_mcp_calls,
        }
    save(out, {"manifest": manifest, "arms": results,
               "limits": ["One run per arm; baseline shared by the two comparisons.",
                          "Same host/account; workspace restriction is instructed, not a security sandbox.",
                          "Token fields are reported raw; no dollar-cost or statistical benefit claim.",
                          "A completed tool call may report an error; inspect tool results and grade separately."]})
    print(out)


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--study", required=True, type=Path)
    commands = parser.add_subparsers(dest="command", required=True)
    setup = commands.add_parser("prepare")
    setup.add_argument("--binary", required=True, type=Path)
    setup.add_argument("--model", default="grok-4.7-medium-fast")
    run = commands.add_parser("run")
    run.add_argument("arm", choices=ARMS)
    run.add_argument("--agent", default=shutil.which("agent"))
    run.add_argument("--timeout", type=int, default=600)
    summary = commands.add_parser("summarize")
    summary.add_argument("--out", required=True, type=Path)
    args = parser.parse_args()
    root = args.study.resolve()
    if args.command == "prepare":
        prepare(root, args.binary, args.model)
    elif args.command == "run":
        if not args.agent:
            parser.error("Cursor agent executable not found")
        run_arm(root, args.arm, args.agent, args.timeout)
    else:
        summarize(root, args.out)


if __name__ == "__main__":
    main()
