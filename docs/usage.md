# Command reference

For a guided first check, start with [Getting started](getting-started.md).
For use through a coding assistant, see [Using Checkweave through MCP](mcp.md).
This page lists command syntax and result fields.

As of 2026-09-24, the cross-platform build and Windows WSL installer have
passed the [release dry run](platforms.md). A tagged release has not yet been
published; install from source for now.

## Choose a command

| You want to… | Command |
| --- | --- |
| Connect a project to Cursor | `init --agent cursor` |
| Find records that match a rule | `check` |
| Find an input where two programs behave differently | `compare` |
| Inspect what happened inside a Python script | `trace` |
| Classify text or ask a model a question about each record | `semantic` |
| Read the evidence behind an earlier result | `evidence ID` |
| Rerun a saved comparison or trace | `replay` |
| Prepare or directly query the optional model | `model setup` / `model evaluate` |
| Start, inspect, or cancel longer work | `run start` / `run status` / `run cancel` |
| Check or stop the local background worker | `status` / `shutdown` |
| Remove Checkweave's Cursor integration | `deinit` |

## Command conventions

Every successful command prints one JSON document. Diagnostics go to stderr.
Exit codes are `0` for a returned report, `1` for a runtime error, `2` for a
usage error, and `130` for interruption. A returned report can contain findings
or incomplete work: read its outcome and coverage before treating it as a pass.
`--workspace` defaults to the current directory. Linked Git worktrees are
separate workspaces.

```sh
checkweave --workspace ROOT <command>
```

`check`, `compare`, `replay`, `trace`, and `model` start a run handle and wait. Ctrl-C cancels that handle. A shared collection check keeps running while another client still waits.

## Build and install

With Rust 1.88 or later installed, run this from the Checkweave source checkout:

```sh
cargo install --path . --locked
checkweave --help
```

Ensure Cargo's binary directory (normally `~/.cargo/bin`) is on your `PATH`.
Use a WSL terminal on Windows. The commands below cover custom installation
paths and the future release-download route.

To install a locally built binary with checksum verification:

```sh
cargo build --locked --release
# Linux / WSL:
sha256sum target/release/checkweave
# macOS:
shasum -a 256 target/release/checkweave
sh scripts/install.sh \
  --binary target/release/checkweave \
  --checksum "<64 hex digits>" \
  --bin-dir "$HOME/.local/bin"
```

Do not pipe a script into a shell. The installers do not use `sudo`, do not write `/usr/local`, and do not edit shell rc, agent config, or workspace data. Default binary path is `$HOME/.local/bin/checkweave` (inside WSL on Windows), or `CHECKWEAVE_BIN_DIR` / `--bin-dir`. Add that directory to `PATH` yourself. A second install with the same bytes leaves the file in place. A new checksum replaces only that binary, via a temporary name in the same directory. A bad checksum leaves the old file.

```sh
sh scripts/install.sh --version 0.1.0 --bin-dir "$HOME/.local/bin"
```

`--version 0.1.0` and `v0.1.0` are tag `v0.1.0` on `ruizmr/checkweave`. This fails until that tag exists. `CHECKWEAVE_RELEASE_BASE` is the download prefix (default `https://github.com/ruizmr/checkweave/releases/download`). Local `--archive` and `--binary` require `--checksum`. Pass exactly one of `--version`, `--archive`, or `--binary`. Archive layout is in [platforms](platforms.md).

After a tagged release is published, the Windows installer can install the
Linux build into WSL2. Download both install scripts from that release and
run the following from their directory. Run `wsl --install` first if WSL is missing:

```powershell
powershell -NoProfile -ExecutionPolicy Bypass -File .\install.ps1 -Version 0.1.0
```

`-Distro NAME` picks a WSL distribution other than the default, and `-BinDir` is a path inside Linux. `-Archive FILE -Checksum SHA256` installs a downloaded Linux archive; `install.sh` must sit next to `install.ps1`. Then open the project from WSL (`wsl`, `cd` into it, `cursor .`) and run `checkweave init` there. See [platforms](platforms.md#windows-through-wsl2).

## Initialize

```sh
checkweave init --agent none
checkweave init --agent cursor
```

`--agent` defaults to `cursor`. `none` only creates `.checkweave/` (marker, ignore). `init` is idempotent. The JSON result has `root`, `state_dir`, and `integration`.

`init --agent cursor` writes only:

- `.cursor/mcp.json` key `mcpServers.checkweave` (`command` plus `args`: `--workspace`, the root, `mcp`)
- `.cursor/rules/checkweave.mdc` when that file is absent or already contains `checkweave:managed`

Other MCP servers are kept. An existing `checkweave.mdc` without `checkweave:managed` is left unchanged and `init` reports that on `rule`. A `checkweave` MCP entry whose command is not `checkweave` / `checkweave.exe` is a conflict and is not overwritten. `.checkweave/` is disposable cache; delete that directory yourself if you want it gone.

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

Only `semantic` and `model evaluate` need a decision model. Use a model when
checking text requires interpretation, such as deciding whether a bug report
contains reproduction steps. Exact field and value checks use `check`.

The default runs locally using SemIf with Qwen3.5-4B. First use needs a Python
runtime and several gigabytes of model downloads; cached installations can
work offline. Quality and device support are still being evaluated. See
[model providers](providers.md) for pinned versions, supported devices, and
configuration details, and [semantic validation](semantic-validation.md) for
measured accuracy.

Create `checkweave.toml` in the workspace to change the defaults:

```toml
[model]
provider = "local"
profile = "default"
device = "auto"
threads = 4
```

| Profile | Use |
| --- | --- |
| `default` | SemIf with Qwen3.5-4B; supports choices, ordered ratings, and true/false questions |
| `lightweight` | GLiNER2.5 base; supports choices and ordered ratings, but not true/false questions |

`device` accepts `auto`, `cpu`, `mps`, `cuda`, or `cuda:<index>`. Availability
of a setting does not mean it has been tested on every platform. The
[worker matrix](python-worker.md#platform-matrix) records actual model runs.
An explicit device does not silently move to another device.

```sh
checkweave model setup
checkweave model setup --offline
checkweave model evaluate --request-file eval.json
```

`setup` prepares the runtime environment; it does not keep a model warm.
Weights load on the first model request. `--offline` requires the necessary
setup cache to exist. Model requests have a default three-minute budget;
first installation or a cold load can take longer than a warm request.

`eval.json` can ask a choice question about text supplied directly:

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

Questions can be `choice`, `ordinal` (ordered `levels` with `label`,
`description`, and `value`), or `predicate` (a `statement`). Model scores are
judgments to review; processing every record does not make every answer correct.

Hosted Jev requires an explicit choice in `checkweave.toml`:

```toml
[model]
provider = "jev"
model = "jev-1.13.0"
api_key_env = "TYPESAFE_API_KEY"
```

Set the token in that environment variable. `jev-latest` is rejected so results
can be tied to a specific model. With this provider, semantic inputs are sent
to its API. A local model failure never switches to hosted inference.

For advanced setup, the cache directory is `$CHECKWEAVE_CACHE_DIR`, then
`$XDG_CACHE_HOME/checkweave`, then `~/.cache/checkweave`. `CHECKWEAVE_PYTHON`
selects an existing interpreter instead of a managed environment. The worker
package is embedded in the binary, so setup needs no source checkout.
Default `startup_timeout_ms` is 600000 (allowed 1..=1800000).
Omitted `timeout_ms` for `model evaluate` or `semantic` is 180000; explicit
values are strict, `0` is rejected, and the maximum is 600000. Deterministic
`check` defaults to 30000. Exact cache identity and setup behavior are in
[providers](providers.md).

## Runs and MCP

```sh
checkweave run start --request-file request.json
checkweave run status ID
checkweave run cancel ID
checkweave mcp
```

`request.json` is one `WorkRequest`: `operation` of `check`, `evidence`, `compare`, `replay`, `semantic`, `trace`, `model_setup`, or `model_evaluate`, plus that operation's fields. Not `run_start`, `run_status`, `run_cancel`, or `shutdown`. The snapshot has `id`, `state` (`queued`, `running`, `complete`, `cancelled`, `failed`), `operation`, and optional `detail`, `result`, `error`.

`mcp` is the stdio server. Tools: `checkweave_check`, `checkweave_evidence`, `checkweave_status`, `checkweave_compare`, `checkweave_replay`, `checkweave_semantic`, `checkweave_trace`, `checkweave_trace_page`, `checkweave_model_setup`, `checkweave_model_evaluate`, `checkweave_run_start`, `checkweave_run_status`, `checkweave_run_cancel`. Compare, trace, replay, and semantic run work and are not all read-only. `init --agent cursor` points Cursor at this process.

## Uninstall

To remove the Cursor integration, run `checkweave deinit` in the project
before removing the binary. This keeps the local evidence cache.

If you installed with `cargo install`, remove the binary with
`cargo uninstall checkweave`. For the shell installer, use the commands below.
The uninstall script deletes one selected binary; it does not stop a worker
or delete source, Git data, `.checkweave/`, model caches, or Cursor config.

```sh
checkweave --workspace ROOT shutdown
sh scripts/uninstall.sh --bin-dir "$HOME/.local/bin"
```

On Windows, `powershell -NoProfile -File .\uninstall.ps1` removes the binary from the default WSL distribution (`-Distro`, `-BinDir` as above).

`--bin PATH` removes that file instead. A missing file is success. A directory is left in place and the script fails. A second run is success.

`checkweave deinit` (alias `uninit`) removes only managed Cursor entries and
keeps `.checkweave/`. JSON reports `removed`, `preserved`, and `"state": "kept"`.
It deletes `mcpServers.checkweave` when the command file name is `checkweave`
or `checkweave.exe`, and deletes `.cursor/rules/checkweave.mdc` only when the
file contains `checkweave:managed`. Anything else is listed under `preserved`
and left on disk. Other MCP servers stay. Uninstall does not call `deinit`.
