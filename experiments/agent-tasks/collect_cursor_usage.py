#!/usr/bin/env python3
"""Read one Cursor --output-format stream-json log per arm.

A present pair is one paired sample of duration and token counts. It is not
a statistical estimate and it is not evidence of a causal benefit.
Missing logs produce status pending and do not invent counts.
"""

from __future__ import annotations

import argparse
import json
from pathlib import Path
from typing import Any

TOKEN_FIELDS = {
    "input_tokens": ("input_tokens", "inputTokens", "prompt_tokens", "promptTokens"),
    "output_tokens": ("output_tokens", "outputTokens", "completion_tokens", "completionTokens"),
    "cache_read_tokens": ("cache_read_tokens", "cacheReadTokens", "cache_read_input_tokens"),
    "cache_write_tokens": ("cache_write_tokens", "cacheWriteTokens", "cache_write_input_tokens"),
}


def first_present(mapping: dict[str, Any], names: tuple[str, ...]) -> int | None:
    for name in names:
        if name in mapping and isinstance(mapping[name], (int, float)):
            return int(mapping[name])
    return None


def usage_from(event: dict[str, Any]) -> dict[str, int]:
    found: dict[str, int] = {}
    candidates = []
    if isinstance(event.get("usage"), dict):
        candidates.append(event["usage"])
    message = event.get("message")
    if isinstance(message, dict) and isinstance(message.get("usage"), dict):
        candidates.append(message["usage"])
    for candidate in candidates:
        for key, names in TOKEN_FIELDS.items():
            value = first_present(candidate, names)
            if value is not None:
                found[key] = value
    return found


def parse_log(path: Path) -> dict[str, Any]:
    if not path.is_file():
        return {"status": "missing", "path": str(path)}
    events = 0
    tool_calls = 0
    tool_started = 0
    tool_completed = 0
    result_event: dict[str, Any] | None = None
    raw_usage: dict[str, Any] | None = None
    last_usage: dict[str, int] = {}
    timestamps: list[float] = []
    for line in path.read_text(errors="replace").splitlines():
        line = line.strip()
        if not line or not line.startswith("{"):
            continue
        try:
            event = json.loads(line)
        except json.JSONDecodeError:
            continue
        if not isinstance(event, dict):
            continue
        events += 1
        kind = str(event.get("type") or event.get("event") or "")
        if kind in {"tool_call", "tool_use"} or "tool_call" in event:
            tool_calls += 1
            subtype = str(event.get("subtype") or "")
            if subtype == "started":
                tool_started += 1
            elif subtype == "completed":
                tool_completed += 1
        usage = usage_from(event)
        if usage:
            last_usage = usage
        if kind == "result":
            result_event = event
            if isinstance(event.get("usage"), dict):
                raw_usage = event["usage"]
            if usage:
                last_usage = usage
        for stamp_key in ("timestamp", "time", "created_at"):
            stamp = event.get(stamp_key)
            if isinstance(stamp, (int, float)):
                timestamps.append(float(stamp))
    duration_ms = None
    if result_event is not None:
        for key in ("duration_ms", "durationMs", "elapsed_ms"):
            if isinstance(result_event.get(key), (int, float)):
                duration_ms = float(result_event[key])
                break
    if duration_ms is None and len(timestamps) >= 2:
        span = max(timestamps) - min(timestamps)
        duration_ms = span if span > 10_000 else span * 1000
    return {
        "status": "parsed",
        "path": str(path),
        "json_events": events,
        "tool_call_events": tool_calls,
        "tool_call_started": tool_started,
        "tool_call_completed": tool_completed,
        "duration_ms": duration_ms,
        "usage": last_usage or None,
        "usage_raw": raw_usage,
        "result_type": None if result_event is None else result_event.get("type"),
        "is_error": None if result_event is None else result_event.get("is_error"),
    }


def _command_args(command: dict[str, Any]) -> list[str]:
    values = []
    for arg in command.get("args") or []:
        if isinstance(arg, dict):
            values.append(str(arg.get("value") or ""))
        else:
            values.append(str(arg))
    return values


def checkweave_shell_use(path: Path) -> dict[str, Any]:
    """Count shell invocations of the checkweave executable.

    A path that merely contains the name, and a bare --help/-h invocation,
    are not counted as solving the tasks with Checkweave.
    """
    help_calls = []
    functional_calls = []
    if not path.is_file():
        return {"status": "missing", "functional_calls": 0, "help_calls": 0}
    for line in path.read_text(errors="replace").splitlines():
        line = line.strip()
        if not line.startswith("{"):
            continue
        try:
            event = json.loads(line)
        except json.JSONDecodeError:
            continue
        if not isinstance(event, dict) or event.get("type") != "tool_call":
            continue
        if event.get("subtype") not in (None, "completed"):
            continue
        tool = event.get("tool_call")
        if not isinstance(tool, dict):
            continue
        shell = tool.get("shellToolCall")
        if not isinstance(shell, dict):
            continue
        args = shell.get("args") if isinstance(shell.get("args"), dict) else {}
        parsed = args.get("parsingResult") if isinstance(args.get("parsingResult"), dict) else {}
        commands = parsed.get("executableCommands") or []
        for command in commands:
            if not isinstance(command, dict):
                continue
            name = str(command.get("name") or "")
            if Path(name).name != "checkweave":
                continue
            argv = _command_args(command)
            record = {"argv": [name, *argv]}
            if argv and all(item in {"--help", "-h", "help"} for item in argv):
                help_calls.append(record)
            else:
                functional_calls.append(record)
    return {
        "functional_calls": len(functional_calls),
        "help_calls": len(help_calls),
        "functional_argv": functional_calls,
        "help_argv": help_calls,
        "adoption": (
            "used_for_task"
            if functional_calls
            else "tool_discovery_only"
            if help_calls
            else "not_invoked"
        ),
    }


def paired(baseline: dict[str, Any], checkweave: dict[str, Any]) -> dict[str, Any]:
    both = baseline.get("status") == "parsed" and checkweave.get("status") == "parsed"
    return {
        "status": "one_paired_sample" if both else "pending",
        "interpretation": (
            "One log from each arm is a single paired sample. "
            "Token counts are not a cost or benefit estimate. "
            "A smaller input-token count is not evidence that Checkweave helped."
        ),
        "baseline": baseline,
        "checkweave": checkweave,
    }


def self_test() -> None:
    sample = "\n".join(
        [
            json.dumps({"type": "assistant", "message": {"usage": {"inputTokens": 3, "outputTokens": 4}}}),
            json.dumps({"type": "tool_call", "name": "shell"}),
            json.dumps(
                {
                    "type": "result",
                    "duration_ms": 1500,
                    "is_error": False,
                    "usage": {"input_tokens": 10, "output_tokens": 6, "cache_read_tokens": 2},
                }
            ),
        ]
    )
    path = Path("/tmp/checkweave-agent-study-usage-selftest.jsonl")
    path.write_text(sample + "\n")
    parsed = parse_log(path)
    if parsed["duration_ms"] != 1500 or parsed["usage"]["input_tokens"] != 10:
        raise SystemExit(parsed)
    if parsed["tool_call_events"] < 1:
        raise SystemExit(parsed)
    missing = parse_log(Path("/tmp/checkweave-agent-study-usage-absent.jsonl"))
    report = paired(parsed, missing)
    if report["status"] != "pending":
        raise SystemExit(report)
    print(json.dumps({"self_test": "ok"}, indent=2))


def main() -> None:
    parser = argparse.ArgumentParser(description="Collect one Cursor stream-json sample per arm")
    parser.add_argument("--baseline-log", type=Path)
    parser.add_argument("--checkweave-log", type=Path)
    parser.add_argument("--out", type=Path)
    parser.add_argument("--self-test", action="store_true")
    args = parser.parse_args()
    if args.self_test:
        self_test()
        return
    if args.baseline_log is None or args.checkweave_log is None:
        raise SystemExit("pass both logs, or --self-test")
    report = paired(parse_log(args.baseline_log), parse_log(args.checkweave_log))
    report["checkweave_shell_use"] = checkweave_shell_use(args.checkweave_log)
    report["baseline_shell_use"] = checkweave_shell_use(args.baseline_log)
    text = json.dumps(report, indent=2) + "\n"
    if args.out is not None:
        args.out.parent.mkdir(parents=True, exist_ok=True)
        args.out.write_text(text)
    print(text, end="")


if __name__ == "__main__":
    main()
