# Usage

**Status:** working prototype. No GitHub Release is published. Platform limits are in [platforms](platforms.md).

One JSON document on stdout for every successful command. Diagnostics on stderr. Exit `0` success, `1` runtime, `2` usage, `130` interrupt. `--workspace` defaults to the current directory. Linked Git worktrees are separate workspaces.

```sh
checkweave --workspace ROOT <command>
```

`check`, `compare`, `replay`, `trace`, and `model` start a run handle and wait. Ctrl-C cancels that handle. A shared collection check keeps running while another client still waits.

## Build and install

No release asset exists yet. Build from this tree:

```sh
cargo build --locked --release
sha256sum target/release/checkweave
sh scripts/install.sh \
  --binary target/release/checkweave \
  --checksum "<64 hex digits>" \
  --bin-dir "$HOME/.local/bin"
```

Do not pipe a script into a shell. The installers do not use `sudo`, do not write `/usr/local`, and do not edit shell rc, agent config, or workspace data. Default binary path is `$HOME/.local/bin/checkweave` (`%USERPROFILE%\.local\bin\checkweave.exe` on Windows), or `CHECKWEAVE_BIN_DIR` / `--bin-dir`. Add that directory to `PATH` yourself. A second install with the same bytes leaves the file in place. A new checksum replaces only that binary, via a temporary name in the same directory. A bad checksum leaves the old file.

```sh
sh scripts/install.sh --version 0.1.0 --bin-dir "$HOME/.local/bin"
```

`--version 0.1.0` and `v0.1.0` are tag `v0.1.0` on `ruizmr/checkweave`. This fails until that tag exists. `CHECKWEAVE_RELEASE_BASE` is the download prefix (default `https://github.com/ruizmr/checkweave/releases/download`). Local `--archive` and `--binary` require `--checksum`. Pass exactly one of `--version`, `--archive`, or `--binary`. Archive layout is in [platforms](platforms.md).

Windows:

```powershell
powershell -NoProfile -File .\scripts\install.ps1 -Version 0.1.0 -BinDir "$env:USERPROFILE\.local\bin"
```

## Uninstall

Deletes one selected file. It does not stop a worker and does not delete source, Git data, `.checkweave/`, model caches, or Cursor config.

```sh
checkweave --workspace ROOT shutdown
sh scripts/uninstall.sh --bin-dir "$HOME/.local/bin"
```

`--bin PATH` removes that file instead. A missing file is success. A directory is left in place and the script fails. A second run is success.

`checkweave deinit` (alias `uninit`) removes only managed Cursor entries and
keeps `.checkweave/`. JSON reports `removed`, `preserved`, and `"state": "kept"`.
It deletes `mcpServers.checkweave` when the command file name is `checkweave`
or `checkweave.exe`, and deletes `.cursor/rules/checkweave.mdc` only when the
file contains `checkweave:managed`. Anything else is listed under `preserved`
and left on disk. Other MCP servers stay. Uninstall does not call `deinit`.

`init --agent cursor` writes only:

- `.cursor/mcp.json` key `mcpServers.checkweave` (`command` plus `args`: `--workspace`, the root, `mcp`)
- `.cursor/rules/checkweave.mdc` when that file is absent or already contains `checkweave:managed`

Other MCP servers are kept. An existing `checkweave.mdc` without `checkweave:managed` is left unchanged and `init` reports that on `rule`. A `checkweave` MCP entry whose command is not `checkweave` / `checkweave.exe` is a conflict and is not overwritten. `.checkweave/` is disposable cache; delete that directory yourself if you want it gone.

## Initialize

```sh
checkweave init --agent none
checkweave init --agent cursor
```

`--agent` defaults to `cursor`. `none` only creates `.checkweave/` (marker, ignore). `init` is idempotent. The JSON result has `root`, `state_dir`, and `integration`.

## Check

```sh
checkweave check \
  --include '**/*.jsonl' \
  --predicate '{"op":"gt","path":"/n","value":1}'
```

Repeat `--include`. `--predicate` and `--predicate-file` are mutually exclusive. Paths are JSON Pointers: `""` is the record, `/status` is a member, `/` is the empty-name member. Unknown predicate fields are rejected.

| `op` | Fields |
| --- | --- |
| `exists` | `path` |
| `eq` `ne` | `path`, `value` |
| `contains` | `path`, `value` (string) |
| `regex` | `path`, `pattern` (Rust `regex`) |
| `gt` `ge` `lt` `le` | `path`, `value` (number) |
| `kind` | `path`, `kind`: `null` `boolean` `number` `string` `array` `object` |
| `all` `any` | `predicates` |
| `not` | `predicate` |

Limits, with these defaults: `--max-files 1000`, `--max-bytes 33554432`, `--max-records 100000`, `--max-results 50`, `--timeout-ms 30000`. `--max-results 0` is counts-only: coverage still counts every scanned row, and `items` is empty.

The report includes `id`, `execution` (`complete`, `partial`, `cancelled`, `failed`), `basis`, `freshness` (`validated`, `stale`, `unknown`), `operator_version` (`jsonl-check-v1`), `coverage` (`files`, `records`, `evaluated`, `matched`, `unmatched`, `unresolved`, `skipped`, `cache_hits`, `cache_misses`), `items`, `sources`, `truncated`, `warnings`, `elapsed_ms`. `matched: null` is unresolved. Coverage is not accuracy.

## Evidence, status, shutdown

```sh
checkweave evidence ID
checkweave status
checkweave shutdown
```

`evidence` reads the collection store, then trace evidence, then semantic evidence. A compare id is not opened this way; use `replay`. Missing evidence is an error, not an empty success. Omit `--offset` and `--limit` to keep that lookup. A trace page is:

```sh
checkweave evidence TRACE_ID --offset 64 --limit 64
```

`--offset` defaults to 0. `--limit` defaults to 64 when it is passed or when `--offset` is not 0. A fresh `trace` response includes the first 64 stored events, plus `event_offset` 0 and `event_total`. Retained events stay in `.checkweave/traces/<id>/events.jsonl`. Displayed stdout and stderr may set `stdout_omitted_bytes` and `stderr_omitted_bytes`; the captured files stay. The trace JSON is capped at 6 MiB. MCP tool `checkweave_trace_page` takes `id`, `offset`, and `limit`.

`status` includes `pid`, `instance`, `workspace`, `protocol` (`1`), `implementation`, `watcher`, `stats`, and `runs`. `shutdown` asks the worker to exit. Idle shutdown is also internal (hidden `daemon --idle-seconds`, default 300). Do not start `daemon` yourself.

## Compare and replay

`compare` runs programs only for this request. It does not start a shell. A difference is an observation, not a regression proof. A worktree is not a sandbox.

```sh
checkweave compare --request-file compare.json
checkweave replay --kind compare --id ID
checkweave replay --kind trace --id ID
```

```json
{
  "before": {"argv": ["python3", "before.py"], "sources": ["before.py"]},
  "after": {"argv": ["python3", "after.py"], "sources": ["after.py"]},
  "inputs": [{"n": 1}]
}
```

Each target reads one JSON value on stdin and writes one JSON document on stdout. `cwd` defaults to `"."` and must stay inside the workspace. `repeat` defaults to 2. Optional: `generated`, `reduce`, `before_revision`, `after_revision`, `budgets`, `per_execution`, `policy`, `retention`. Details: [behavior](behavior.md).

## Trace

```sh
checkweave trace --request-file trace.json
```

```json
{"script": "app.py", "input": {"n": 1}, "baseline": false}
```

`script` is a workspace-relative Python file run as `__main__`. Nested imports of other workspace modules are included in the captured sources, subject to the total byte cap. Optional `functions` and `paths` filter events. Default limits: `timeout_ms` 5000, `max_events` 5000, `max_value_bytes` 4096, `max_output_bytes` 65536. The helper is embedded in the binary and written to `$CHECKWEAVE_CACHE_DIR/trace-helper/trace-python-v1/checkweave_trace.py` (otherwise `$XDG_CACHE_HOME/checkweave` or `~/.cache/checkweave`). `CHECKWEAVE_TRACE_HELPER` overrides that path. A copied binary does not need this source checkout. `CHECKWEAVE_TRACE_PYTHON` selects the interpreter (`python3`, then `python`). Event order is not causation. Details: [execution evidence](execution-evidence.md).

## Semantic collection

```sh
checkweave semantic --request-file semantic.json
```

```json
{
  "globs": ["notes.jsonl"],
  "text_pointer": "/text",
  "questions": [{"kind": "predicate", "id": "q", "statement": "The note states a deadline."}],
  "batch_size": 8
}
```

`text_pointer` `""` is the whole JSON value. `batch_size` is 1..=32. Omitting `limits.timeout_ms` uses 180000. Set `"limits": {"timeout_ms": 30000}` for a shorter job. Counting and freshness stay in Rust. This is not model accuracy. Details: [semantic collections](semantic-collections.md). MCP tool: `checkweave_semantic`.

## Model

Deterministic `check` does not download a model. Local failure never calls a hosted API.

Absent `[model]` selects profile `default`: SemIf option-logit on
`Qwen/Qwen3.5-4B` revision `851bf6e806efd8d0a36b00ddf55e13ccb7b8cd0a`, SemIf
code `1f2dea3e25379f9dfc98cb83c324f00ab5deda37`. CPU uses GGUF
`bartowski/Qwen_Qwen3.5-4B-GGUF` revision
`4168f45a16a1290d65a4ec0fa312ae917a4c15d6`, file
`Qwen_Qwen3.5-4B-Q4_K_M.gguf` (3013027808 bytes). On this host that Q4 file
scored 63/64 supported labels and 11/12 predicate-gold items. GLiNER2.5 base
scored 50/64 on the same labels. Neither sample is the 534-item JevBench
board, and the Q4 score is not the published BF16 score. Details:
[semantic validation](semantic-validation.md). djev is a research candidate,
not a profile. Automatic GPU selection is not a release claim. A Tesla M10
cannot hold the BF16 checkpoint; this host stays on the CPU wheel and Q4
with `n_gpu_layers` 0. Explicit CUDA that fails the fit gate is an error.
macOS, Windows, and ROCm were not run. Measurements:
[Python worker](python-worker.md) and [providers](providers.md).

```toml
[model]
provider = "local"
profile = "default"
device = "auto"
threads = 4
```

`profile = "lightweight"` is `fastino/gliner2.5-base-v1` revision
`1a8bc24e00dc7300b9017c81d63e3dcdabb26596`. Predicates are unsupported on that
profile. A 64-label sample scored 50/64 on both CPU and CUDA, with eight wrong
labels at softmax ≥ 0.8 ([semantic validation](semantic-validation.md)). That
sample is not a release qualification.

`device` is `auto`, `cpu`, `mps`, `cuda`, or `cuda:<index>`. `auto` probes; an explicit device does not silently move. Jev is opt-in:

```toml
[model]
provider = "jev"
model = "jev-1.13.0"
api_key_env = "TYPESAFE_API_KEY"
```

`jev-latest` is rejected. The token stays in the environment.

```sh
checkweave model setup
checkweave model setup --offline
checkweave model evaluate --request-file eval.json
```

`--offline` succeeds only when the cache is already present. Cache directory: `$CHECKWEAVE_CACHE_DIR`, else `$XDG_CACHE_HOME/checkweave`, else `~/.cache/checkweave`. `CHECKWEAVE_PYTHON` uses an existing interpreter and skips the managed env. The managed install materializes the embedded canonical `python/checkweave_worker` package; setup does not need this checkout. `model setup` installs that environment. It does not return a warm model. Weights load on the first readiness or evaluate. Default `startup_timeout_ms` is `600000` (10 minutes), allowed `1..=1800000`. `model evaluate` with no `timeout_ms` uses 180000 so a cold CPU load can finish. An explicit `timeout_ms` is strict. An omitted semantic `timeout_ms` is also 180000, whether `limits` is absent or present without that field. Other semantic limit fields keep their defaults. An explicit value, including 30000, is strict. `0` is rejected. The hard cap is 600000. Deterministic `check` still defaults to 30000. On this host the Q4 worker was ready in about 25 s ([Python worker](python-worker.md)); a user-path semantic cold check was 45.8 s. A 30 s budget is not enough for that cold start.

```json
{
  "states": [{"id": "s1", "text": "rotate the token, then revoke the old one"}],
  "questions": [{
    "kind": "choice",
    "id": "q1",
    "labels": [
      {"label": "how_to", "description": "Instructions for a task."},
      {"label": "incident", "description": "A report of a failure that already happened."}
    ]
  }],
  "timeout_ms": 180000
}
```

`kind` is `choice`, `ordinal` (`levels` with `label`, `description`, `value`), or `predicate` (`statement`). Scores are not coverage. Provider behavior: [providers](providers.md). Lightweight GLiNER still returns predicate as `unsupported`.

## Runs and MCP

```sh
checkweave run start --request-file request.json
checkweave run status ID
checkweave run cancel ID
checkweave mcp
```

`request.json` is one `WorkRequest`: `operation` of `check`, `evidence`, `compare`, `replay`, `semantic`, `trace`, `model_setup`, or `model_evaluate`, plus that operation's fields. Not `run_start`, `run_status`, `run_cancel`, or `shutdown`. The snapshot has `id`, `state` (`queued`, `running`, `complete`, `cancelled`, `failed`), `operation`, and optional `detail`, `result`, `error`.

`mcp` is the stdio server. Tools: `checkweave_check`, `checkweave_evidence`, `checkweave_status`, `checkweave_compare`, `checkweave_replay`, `checkweave_semantic`, `checkweave_trace`, `checkweave_trace_page`, `checkweave_model_setup`, `checkweave_model_evaluate`, `checkweave_run_start`, `checkweave_run_status`, `checkweave_run_cancel`. Compare, trace, replay, and semantic run work and are not all read-only. `init --agent cursor` points Cursor at this process.
