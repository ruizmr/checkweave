#!/usr/bin/env python3
"""Workspace Python execution evidence for Checkweave.

The target script runs as ``__main__`` and reads JSON from stdin. This process
records ``call``, ``line``, ``return``, and ``exception`` events for workspace
files only. Observations are tied to the bytecode and source line that ran.

Observed scope is this CPython process. The adapter does not record native
calls, subprocesses, or async task scheduling. Event order is the order events
were observed on a thread; it is not a causal explanation. The process is not
a sandbox.

Protocol data is written to the result file and a one-line acknowledgement on
the original stdout. The script's stdout and stderr go to separate capture
files. Trace events go to a JSONL file.
"""

from __future__ import annotations

import hashlib
import importlib.machinery
import io
import json
import linecache
import os
import sys
import threading
import time
import traceback

SELF = os.path.realpath(__file__)
EVENTS = ("call", "line", "return", "exception")
UNSUPPORTED = (
    "native_calls",
    "subprocess_events",
    "async_scheduling",
    "threads_started_before_the_hook",
)

_tls = threading.local()
_log_lock = threading.Lock()


class TraceState:
    def __init__(self, config):
        self.workspace = os.path.realpath(config["workspace"])
        self.script_path = os.path.realpath(config["script_path"])
        self.script_rel = config["script"]
        self.events_path = config["events_path"]
        self.stdout_path = config["stdout_path"]
        self.stderr_path = config["stderr_path"]
        self.result_path = config["result_path"]
        self.snapshot_dir = os.path.realpath(config["snapshot_dir"])
        self.max_events = int(config["max_events"])
        self.max_value_bytes = int(config["max_value_bytes"])
        self.max_output_bytes = int(config["max_output_bytes"])
        self.max_event_bytes = int(config.get("max_event_bytes") or (8 * 1024 * 1024))
        self.max_source_file_bytes = int(config.get("max_source_file_bytes") or (8 * 1024 * 1024))
        self.max_source_total_bytes = int(config.get("max_source_total_bytes") or (8 * 1024 * 1024))
        self.max_source_files = int(config.get("max_source_files") or 32)
        self.functions = set(config.get("functions") or [])
        self.paths = [p.replace("\\", "/").strip("/") for p in (config.get("paths") or []) if p]
        self.baseline = bool(config.get("baseline"))
        self.replay_snapshot = config.get("replay_snapshot")
        self.replay_overlay = config.get("replay_overlay")
        self.dropped = 0
        self.recorded = 0
        self.event_bytes = 0
        self.source_bytes = 0
        self.source_files = 0
        self.sources_dropped = 0
        self.modified_during_run = False
        self.hashes = {}
        self.disk_start = {}
        self.stat_cache = {}
        self.event_fd = None

    def allow(self, rel, func_name):
        if self.paths:
            ok = False
            for item in self.paths:
                if rel == item or rel.startswith(item + "/"):
                    ok = True
                    break
            if not ok:
                return False
        if self.functions and func_name not in self.functions:
            return False
        return True


STATE: TraceState | None = None


def relate(filename):
    state = STATE
    if not filename or filename.startswith("<"):
        return None
    try:
        path = os.path.realpath(filename)
    except OSError:
        return None
    root = state.workspace
    if path != root and not path.startswith(root + os.sep):
        return None
    rel = os.path.relpath(path, root).replace(os.sep, "/")
    parts = rel.split("/")
    if any(part in ("..", ".git", ".checkweave") for part in parts):
        return None
    if path == SELF:
        return None
    return rel


def prime_linecache(filename, data):
    text = data.decode("utf-8", "replace")
    lines = text.splitlines(keepends=True)
    linecache.cache[filename] = (len(data), None, lines, filename)


def remember_bytes(filename, data):
    state = STATE
    rel = relate(filename)
    if rel is None or not isinstance(data, (bytes, bytearray)):
        return
    size = len(data)
    if (
        size > state.max_source_file_bytes
        or state.source_files >= state.max_source_files
        or state.source_bytes + size > state.max_source_total_bytes
    ):
        state.sources_dropped += 1
        return
    data = bytes(data)
    state.source_files += 1
    state.source_bytes += size
    digest = hashlib.sha256(data).hexdigest()
    state.hashes[filename] = digest
    if state.replay_snapshot and os.path.realpath(filename) == state.script_path:
        try:
            with open(filename, "rb") as handle:
                disk = handle.read(state.max_source_file_bytes + 1)
            if len(disk) > state.max_source_file_bytes:
                state.disk_start[filename] = None
                state.modified_during_run = True
            else:
                state.disk_start[filename] = hashlib.sha256(disk).hexdigest()
        except OSError:
            state.disk_start[filename] = None
    else:
        state.disk_start[filename] = digest
    prime_linecache(filename, data)
    dest = os.path.normpath(os.path.join(state.snapshot_dir, rel))
    root = state.snapshot_dir
    if dest != root and not dest.startswith(root + os.sep):
        return
    parent = os.path.dirname(dest)
    os.makedirs(parent, exist_ok=True)
    with open(dest, "wb") as handle:
        handle.write(data)


def note_disk(filename):
    state = STATE
    if state.modified_during_run or filename not in state.disk_start:
        return
    start = state.disk_start.get(filename)
    try:
        st = os.stat(filename)
        key = (getattr(st, "st_mtime_ns", st.st_mtime), st.st_size)
    except OSError:
        state.modified_during_run = True
        return
    if st.st_size > state.max_source_file_bytes:
        state.modified_during_run = True
        return
    if state.stat_cache.get(filename) == key:
        return
    state.stat_cache[filename] = key
    try:
        with open(filename, "rb") as handle:
            current = hashlib.sha256(handle.read(state.max_source_file_bytes + 1)).hexdigest()
    except OSError:
        state.modified_during_run = True
        return
    if start is None or current != start:
        state.modified_during_run = True


def type_name(value):
    try:
        name = type(value).__name__
    except Exception:
        return "unknown"
    if type(name) is not str:
        return "unknown"
    if len(name) > 128:
        return name[:128]
    return name


def represent(value, budget):
    try:
        return _represent(value, budget)
    except Exception:
        return {"unrepresentable": True, "type": "unknown", "reason": "error"}


def _represent(value, budget):
    if value is None:
        return None
    if type(value) is bool:
        return value
    if type(value) is int:
        # str() on a real int does not call user code. bool is excluded above.
        if value.bit_length() > budget * 8:
            return {"unrepresentable": True, "type": "int", "reason": "size"}
        text = str(value)
        if len(text) > budget:
            return {"unrepresentable": True, "type": "int", "reason": "size"}
        return value
    if type(value) is float:
        if value != value or value == float("inf") or value == float("-inf"):
            return {"unrepresentable": True, "type": "float", "reason": "non_finite"}
        return value
    if type(value) is str:
        size = len(value.encode("utf-8", "replace"))
        if size > budget:
            return {"unrepresentable": True, "type": "str", "reason": "size", "bytes": size}
        return value
    if type(value) is bytes:
        if len(value) > budget:
            return {"unrepresentable": True, "type": "bytes", "reason": "size", "bytes": len(value)}
        return {"type": "bytes", "hex": value.hex()}
    return {"unrepresentable": True, "type": type_name(value)}


def snapshot_locals(frame):
    budget = STATE.max_value_bytes
    try:
        pairs = list(frame.f_locals.items())
    except Exception:
        return {}
    pairs.sort(key=lambda item: item[0] if type(item[0]) is str else "")
    out = {}
    for name, value in pairs:
        if type(name) is not str or name == "__builtins__":
            continue
        if len(out) >= 16:
            break
        out[name] = represent(value, budget)
    return out


def line_hash(filename, lineno):
    entry = linecache.cache.get(filename)
    text = ""
    if entry is not None:
        lines = entry[2]
        if 1 <= lineno <= len(lines):
            text = lines[lineno - 1]
    return hashlib.sha256(text.encode("utf-8")).hexdigest()


def exception_value(arg):
    name = "exception"
    if type(arg) is tuple and arg:
        try:
            et = arg[0]
            if isinstance(et, type):
                label = et.__name__
                if type(label) is str:
                    name = label[:128]
        except Exception:
            name = "exception"
    return {"unrepresentable": True, "type": name, "reason": "exception"}


def write_event(frame, event, arg):
    state = STATE
    if state.recorded >= state.max_events or state.event_bytes >= state.max_event_bytes:
        state.dropped += 1
        return
    rel = relate(frame.f_code.co_filename)
    if rel is None or not state.allow(rel, frame.f_code.co_name):
        return
    if event not in EVENTS:
        return
    note_disk(frame.f_code.co_filename)
    lineno = int(getattr(frame, "f_lineno", 0) or 0)
    if lineno < 0:
        lineno = 0
    payload = {
        "path": rel,
        "line": lineno,
        "function": frame.f_code.co_name,
        "event": event,
        "locals": snapshot_locals(frame),
        "code_hash": hashlib.sha256(frame.f_code.co_code).hexdigest(),
        "line_hash": line_hash(frame.f_code.co_filename, lineno),
    }
    if event == "return":
        payload["value"] = represent(arg, state.max_value_bytes)
    elif event == "exception":
        payload["value"] = exception_value(arg)
    try:
        encoded = json.dumps(payload, separators=(",", ":"), ensure_ascii=False).encode("utf-8")
    except (TypeError, ValueError):
        state.dropped += 1
        return
    if len(encoded) > 65536 or state.event_bytes + len(encoded) + 1 > state.max_event_bytes:
        state.dropped += 1
        return
    line = encoded + b"\n"
    with _log_lock:
        if state.recorded >= state.max_events or state.event_bytes + len(line) > state.max_event_bytes:
            state.dropped += 1
            return
        os.write(state.event_fd, line)
        state.recorded += 1
        state.event_bytes += len(line)


def tracer(frame, event, arg):
    if getattr(_tls, "busy", False):
        return None
    filename = frame.f_code.co_filename
    if filename == SELF:
        return None
    if event not in EVENTS:
        return tracer
    _tls.busy = True
    try:
        write_event(frame, event, arg)
    except Exception:
        if STATE is not None:
            STATE.dropped += 1
    finally:
        _tls.busy = False
    return tracer


class SnapshotLoader(importlib.machinery.SourceFileLoader):
    def get_data(self, path):
        state = STATE
        overlay = state.replay_overlay if state is not None else None
        rel = relate(path) if isinstance(path, str) else None
        if overlay and rel and state is not None:
            retained = os.path.join(overlay, rel)
            if os.path.isfile(retained):
                with open(retained, "rb") as handle:
                    data = handle.read(state.max_source_file_bytes + 1)
                if len(data) > state.max_source_file_bytes:
                    state.sources_dropped += 1
                else:
                    return data
        if state is not None and isinstance(path, str):
            try:
                size = os.stat(path).st_size
            except OSError:
                size = None
            if size is not None and size > state.max_source_file_bytes:
                state.sources_dropped += 1
                raise OSError("source exceeds the retained source byte limit")
        return super().get_data(path)

    def source_to_code(self, data, path, *, _optimize=-1):
        if isinstance(data, (bytes, bytearray)):
            remember_bytes(path if isinstance(path, str) else str(path), bytes(data))
        return super().source_to_code(data, path, _optimize=_optimize)


class WorkspaceFinder:
    def find_spec(self, fullname, path, target=None):
        spec = importlib.machinery.PathFinder.find_spec(fullname, path)
        if spec is None or not spec.origin or not isinstance(spec.origin, str):
            return None
        if spec.origin == SELF or relate(spec.origin) is None:
            return None
        if not isinstance(spec.loader, importlib.machinery.SourceFileLoader):
            return None
        spec.loader = SnapshotLoader(spec.loader.name, spec.origin)
        return spec


class PipeCapture:
    def __init__(self, target_fd, dest_path, limit):
        self.truncated = False
        self.error = None
        read_fd, write_fd = os.pipe()
        os.dup2(write_fd, target_fd)
        os.close(write_fd)
        self._read_fd = read_fd
        self._thread = threading.Thread(target=self._pump, args=(dest_path, limit), daemon=True)
        self._thread.start()

    def _pump(self, dest_path, limit):
        remaining = limit
        try:
            with open(dest_path, "wb") as out:
                while True:
                    try:
                        chunk = os.read(self._read_fd, 8192)
                    except OSError as exc:
                        self.error = str(exc)
                        break
                    if not chunk:
                        break
                    if remaining <= 0:
                        self.truncated = True
                        continue
                    take = chunk[:remaining]
                    out.write(take)
                    remaining -= len(take)
                    if len(chunk) > len(take):
                        self.truncated = True
                out.flush()
        except Exception as exc:
            self.error = str(exc)
        finally:
            try:
                os.close(self._read_fd)
            except OSError:
                pass

    def finish(self):
        self._thread.join(timeout=2)
        if self._thread.is_alive():
            self.truncated = True
            self.error = "capture thread did not finish"


def load_config():
    path = os.environ.get("CHECKWEAVE_TRACE_CONFIG")
    if not path:
        raise RuntimeError("CHECKWEAVE_TRACE_CONFIG is not set")
    with open(path, "r", encoding="utf-8") as handle:
        return json.load(handle)


def load_source(state: TraceState):
    path = state.replay_snapshot or state.script_path
    with open(path, "rb") as handle:
        data = handle.read(state.max_source_file_bytes + 1)
    if len(data) > state.max_source_file_bytes:
        state.sources_dropped += 1
        raise RuntimeError("script exceeds the source byte limit")
    remember_bytes(state.script_path, data)
    prime_linecache(state.script_path, data)
    return compile(data, state.script_path, "exec")


def make_globals(script_path):
    return {
        "__name__": "__main__",
        "__file__": script_path,
        "__package__": None,
        "__cached__": None,
        "__builtins__": __builtins__,
    }


def run_code(code, script_path, stdin_bytes):
    sys.stdin = io.TextIOWrapper(io.BytesIO(stdin_bytes), encoding="utf-8")
    sys.argv = [script_path]
    globs = make_globals(script_path)
    exec(code, globs, globs)


def status_from_exception(exc):
    if isinstance(exc, SystemExit):
        code = exc.code
        if code in (None, 0):
            return "complete", None
        return "failed", "SystemExit"
    return "failed", type_name(exc)


def write_result(state: TraceState, payload):
    raw = json.dumps(payload, separators=(",", ":")).encode("utf-8")
    tmp = state.result_path + ".tmp"
    fd = os.open(tmp, os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o644)
    try:
        os.write(fd, raw)
        os.fsync(fd)
    finally:
        os.close(fd)
    os.replace(tmp, state.result_path)


def flush_stdio():
    try:
        sys.stdout.flush()
    except Exception:
        pass
    try:
        sys.stderr.flush()
    except Exception:
        pass


def bind_stdio():
    sys.stdout = open(1, "w", encoding="utf-8", closefd=False, buffering=1)
    sys.stderr = open(2, "w", encoding="utf-8", closefd=False, buffering=1)


def silence_stdio():
    devnull = os.open(os.devnull, os.O_RDWR)
    try:
        os.dup2(devnull, 1)
        os.dup2(devnull, 2)
    finally:
        os.close(devnull)


def main():
    global STATE
    config = load_config()
    STATE = TraceState(config)
    state = STATE
    stdin_bytes = sys.stdin.buffer.read()
    protocol_fd = os.dup(1)
    os.makedirs(state.snapshot_dir, exist_ok=True)
    state.event_fd = os.open(state.events_path, os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o644)

    status = "complete"
    error = None
    baseline_us = None
    traced_us = None
    stdout_cap = None
    stderr_cap = None
    try:
        # Match `python3 path/to/script.py`: the script directory is sys.path[0].
        # cwd stays the workspace, so '' still resolves workspace-root imports.
        script_dir = os.path.dirname(state.script_path)
        if script_dir and script_dir not in sys.path:
            sys.path.insert(0, script_dir)
        sys.meta_path.insert(0, WorkspaceFinder())
        code = load_source(state)
        if state.baseline:
            silence_stdio()
            bind_stdio()
            started = time.perf_counter_ns()
            try:
                run_code(code, state.script_path, stdin_bytes)
            except (Exception, SystemExit):
                pass
            baseline_us = (time.perf_counter_ns() - started) // 1000
            flush_stdio()
        stdout_cap = PipeCapture(1, state.stdout_path, state.max_output_bytes)
        stderr_cap = PipeCapture(2, state.stderr_path, state.max_output_bytes)
        bind_stdio()
        sys.settrace(tracer)
        threading.settrace(tracer)
        started = time.perf_counter_ns()
        try:
            run_code(code, state.script_path, stdin_bytes)
        except (Exception, SystemExit) as exc:
            status, error = status_from_exception(exc)
        finally:
            sys.settrace(None)
            threading.settrace(None)
            traced_us = (time.perf_counter_ns() - started) // 1000
    except Exception as exc:
        status = "failed"
        error = type_name(exc)
        try:
            sys.stderr.write(traceback.format_exc())
            sys.stderr.flush()
        except Exception:
            pass
    finally:
        flush_stdio()
        devnull = os.open(os.devnull, os.O_RDWR)
        try:
            os.dup2(devnull, 1)
            os.dup2(devnull, 2)
        finally:
            os.close(devnull)
        if stdout_cap is not None:
            stdout_cap.finish()
        if stderr_cap is not None:
            stderr_cap.finish()
        if state.event_fd is not None:
            try:
                os.close(state.event_fd)
            except OSError:
                pass

    cap_error = None
    if stdout_cap is not None and stdout_cap.error:
        cap_error = stdout_cap.error
    elif stderr_cap is not None and stderr_cap.error:
        cap_error = stderr_cap.error
    if cap_error:
        status = "failed" if status == "complete" else status
        error = error or cap_error

    payload = {
        "status": status,
        "error": error,
        "dropped": state.dropped,
        "sources_dropped": state.sources_dropped,
        "stdout_truncated": bool(stdout_cap and stdout_cap.truncated),
        "stderr_truncated": bool(stderr_cap and stderr_cap.truncated),
        "modified_during_run": state.modified_during_run,
        "script_sha256": state.hashes.get(state.script_path),
        "baseline_us": baseline_us,
        "traced_us": traced_us,
        "unsupported": list(UNSUPPORTED),
    }
    try:
        write_result(state, payload)
    except Exception as exc:
        os.write(protocol_fd, json.dumps({"checkweave_trace": 1, "error": type_name(exc)}).encode() + b"\n")
        os.close(protocol_fd)
        return 1
    os.write(protocol_fd, b'{"checkweave_trace":1}\n')
    os.close(protocol_fd)
    return 0


if __name__ == "__main__":
    try:
        sys.exit(main())
    except Exception:
        sys.stderr.write(traceback.format_exc())
        sys.exit(1)
