# Execution evidence

Use a trace when reading a Python script leaves you unsure which branch ran
or where a value changed. Checkweave runs the script and returns recorded
events, source lines, and supported local values for your assistant to inspect.
See the [trace command](usage.md#trace) for a minimal request; this page
explains what is captured and how to interpret it.

To check a fix, run a **new trace**. Replaying a trace runs its saved source
snapshot, which is useful for revisiting the original behavior.

**Status:** Python line-level adapter implemented in `src/trace.rs` and
`python/checkweave_trace.py`. This is a direct-observation capture, not a
sandbox and not a causal debugger.

## What a trace is

`trace` runs one workspace-relative Python script as `__main__`. The request's
`input` JSON is the script's stdin. The adapter records `call`, `line`,
`return`, and `exception` events for files inside the workspace.

Each event stores:

| Field | Meaning |
| --- | --- |
| `path`, `line`, `function` | Workspace-relative file, executed line, and code-object name |
| `event` | `call`, `line`, `return`, or `exception` |
| `locals` | Bounded scalars only: `None`, `bool`, `int`, `float`, `str`, and `bytes` |
| `value` | Return value, or an exception type summary |
| `code_hash` | SHA-256 of the code object's bytecode (`co_code`) at the event |
| `line_hash` | SHA-256 of the source line that was compiled for that line number |

Line text comes from the bytes passed to `compile`, kept in `linecache` for the
run. It is not a later read of whatever is on disk. Other objects are
`unrepresentable` and are not formatted with `repr` or `str`. Values that
exceed `max_value_bytes` are unrepresentable size markers, not silent prefixes.

The script's stdout and stderr are captured in separate files under the trace
directory, with `max_output_bytes`. Events are appended as JSONL to
`.checkweave/traces/<id>/events.jsonl`. The tracer's own acknowledgement is a
one-line JSON object on the process stdout pipe and is not part of the
script's stdout. A stored `report.json` is the retrieval record.

## Request and report

`TraceRequest` carries `script`, `input`, optional `functions` and `paths`
filters, `limits`, and `baseline`.

Limits are `timeout_ms` (1..=300000), `max_events` (1..=100000),
`max_value_bytes` (1..=1048576), and `max_output_bytes` (1..=4194304).
`functions` and `paths` are combined with AND. An empty list does not filter.
A path matches that workspace-relative file or a file below that prefix.

`TraceReport.basis` is `direct_observation`. `execution` is `complete`,
`partial`, `failed`, `cancelled`, or `timeout`. `dropped` counts events that
were not retained because the event budget or a single event's size was
exceeded. `elapsed_ms` is the process wall time, including interpreter startup.
`generation` is a fingerprint of the retained source snapshots. Per-file
fingerprints are BLAKE3 of the bytes that were compiled.

When `baseline` is true, the helper runs the script once with tracing off and
once with tracing on, in the same process. `baseline_us`, `traced_us`, and
`overhead_us` (`traced - baseline`, which can be negative) are that
measurement. `baseline_ms` and `overhead_ms` are those values in whole
milliseconds. Imported modules stay loaded across the two runs. Replay does
not repeat the baseline run.

## Freshness, snapshots, and replay

The snapshot covers retained workspace source files. It does not freeze installed
packages, the Python interpreter, environment variables, or files outside the
workspace. Those can change replay behavior while source freshness remains
`validated`. The dependency-change case in [tests/recovery.rs](../tests/recovery.rs)
records this explicitly: old trace events stay old, while a new trace and replay
observe the changed external dependency.

The main script, and workspace modules loaded through the adapter's import
hook, are copied under `.checkweave/traces/<id>/snapshot/`. Those bytes are
what replay executes.

`evidence` reads the stored report and recomputes freshness from the current
workspace bytes:

| Condition | Freshness |
| --- | --- |
| A source fingerprint matches the current file | contributes to `validated` |
| A source is missing or its bytes differ | `stale` |
| The run set `modified_during_run` | `stale`, and it stays stale |

`modified_during_run` means the on-disk bytes of a traced file changed after
the bytes that were compiled, while the process was still running. Restoring
the file afterward does not make that report `validated`.

`replay` executes the retained snapshot with the stored input, filters, and
limits. Event paths still name the original workspace files. The new report's
`replay_of` is the original id. Its freshness compares the snapshot
fingerprints with the current workspace, so an edited file makes the replay
report stale even though the executed bytes are the snapshot.

`evidence_page(root, id, offset, limit)` returns the same revalidated report
with `events` sliced. `event_total` is the stored event count. `dropped` stays
the capture-time drop count.

Retention is bounded to 32 published reports and 64 MiB under
`.checkweave/traces`. Publication of `report.json` is an atomic rename.
Cleanup does not delete the report just published.

## Runtime and containment

The interpreter is `CHECKWEAVE_TRACE_PYTHON` when set, otherwise the first of
`python3` and `python` that runs. The helper source is compiled into the binary
with `include_bytes!`. Unless `CHECKWEAVE_TRACE_HELPER` points at an explicit
file, Checkweave writes that copy atomically under
`$CHECKWEAVE_CACHE_DIR/trace-helper/trace-python-v1/` (or the XDG/home cache).
A released binary does not read `python/checkweave_trace.py` from a source
checkout. The working directory stays the workspace. The script's own directory
is placed at the front of `sys.path`, the same as `python3 path/to/script.py`,
so a nested script imports a sibling module from that directory. Replay keeps
that path and serves retained snapshot bytes for those modules.

Timeout, cancel, and process-group kill use `execute`. The user stdout budget
is enforced on the capture files; the execute pipe only carries the short
tracer acknowledgement, so its byte budget is at least 8 KiB.

Captured event JSONL stops at 8 MiB and 64 KiB per event. Retained source
snapshots stop at 8 MiB per file, 8 MiB total, and 32 files. Reads check size
before allocating the file body. `trace` and `evidence_page` shorten the
returned event page until the report JSON is at most 8 MiB, which is the IPC
frame. `event_total` stays the stored count. `dropped` and `sources_dropped`
stay visible, and the execution is `partial` when a budget omits events or
source bytes. Stdout is decoded lossily and cut on a UTF-8 character boundary,
so a truncated invalid byte sequence does not panic.

This is not a sandbox. The script inherits the environment, can write the
workspace, and can start other processes. Those processes are not traced.
`containment` on the report says so.

## Observed Python scope

Recorded:

- `call`, `line`, `return`, and `exception` on this process for workspace files
- threads started after the hook is installed (`sys.settrace` and `threading.settrace`)
- workspace imports loaded by the snapshot import hook

Not recorded, and not inferred:

- native and builtin calls
- subprocesses and any other process
- async task scheduling; a line inside a coroutine can appear when that
  coroutine runs on a traced thread, which is not a trace of the event loop
- threads that were already running when the hook was installed
- lines outside the workspace; the hook stays active across those frames so a
  later workspace frame is still visible, but those outside lines are not events
- files under `.git` or `.checkweave`, and this helper

Event order is the order events were observed. It does not establish why a
later value occurred. Unsupported scope is listed on every report.
