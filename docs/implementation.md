# Implementation work log

The full project scope remains the five milestones in [roadmap.md](roadmap.md).
This file records development and evidence; unchecked release gates stay open.

## Current work

2026-09-21: starting the Rust implementation. Cursor CLI workers use
`grok-4.7-high-fast` with explicit contracts and disjoint file ownership. The
coordinator reviews and integrates their code and verifies completion gates.

- Collection kernel: JSONL selection, typed predicates, content fingerprints,
  item reuse, SQLite storage, bounded evidence, publication freshness.
- Runtime: idempotent initialization, Cursor integration, shared daemon,
  locking, IPC, crash recovery, watching and reconciliation.
- Interface: CLI and official Rust MCP SDK using the same requests.

No milestone is marked complete until its documented evidence is collected.

## 2026-09-22 prototype surface

CLI and MCP share the daemon for `check`, `evidence`, `status`, `shutdown`,
`compare`, `replay`, `semantic`, `trace`, `model setup`, `model evaluate`,
and run handles. `init` and `deinit` call the workspace directly. `daemon`
stays hidden. `deinit` removes only a managed `mcpServers.checkweave` entry
and a `checkweave.mdc` that contains `checkweave:managed`.

Local profile `default` is SemIf + `Qwen/Qwen3.5-4B`. Profile `lightweight`
is GLiNER2.5 base. Jev stays opt-in. djev is a research candidate, not a
profile. The 534-item JevBench board was not run here.

Held-out sample ([semantic validation](semantic-validation.md)): SemIf Q4 CPU
63/64 supported labels, GLiNER 50/64, predicate gold 11/12. Artifact
`experiments/semantic-validation/results/semif-q4-cpu.json`. That is not the
BF16 board and `release_qualified` stays false.

The trace helper is embedded (`include_bytes!` of `python/checkweave_trace.py`)
and materialized at `trace-helper/trace-python-v1/checkweave_trace.py` under
the user cache. Nested script imports and the total source byte cap are in
that adapter. `CHECKWEAVE_TRACE_HELPER` still overrides the path. The model
worker materializes the embedded canonical `python/checkweave_worker` package.

Root `cargo test --locked --all-targets --no-fail-fast` succeeded: 142 passed,
1 ignored. The ignored test is the real managed install
(`installed_canonical_worker_cpu_smoke`). A later offline run of that ignored
test passed in 63.58 s (`device=cpu`, `precision=gguf-q4_k_m`, no hosted
fallback). Counts in that full suite: adversarial 16, collection 17, compare
12, daemon 15, end_to_end 11, execute 9, interface 5, models 17 plus 1
ignored, semantic 9, trace 17, workspace 12, lib 2. The default semantic
smoke returns early and is not CPU evidence. SemIf CPU smoke is
[python/tests/results/semif-cpu-smoke.json](../python/tests/results/semif-cpu-smoke.json)
(ready 24.5 s, wall 47.3 s, Q4, `n_gpu_layers` 0). The later 64-label Q4
sample is the 63/64 result above.

`cargo build --locked --release` on this Linux x86_64 host finished in 45.45 s
(log `/tmp/checkweave-development/release-build.log`, exit 0). The stripped
binary is 17238904 bytes, sha256
`1262ecc5f75b7c2535add67961071169a6434c841592822fc4d4d0112c8340bd`. A copy
outside this checkout, with `CHECKWEAVE_TRACE_HELPER` unset and
`CHECKWEAVE_CACHE_DIR` isolated, ran `init --agent none`, two JSONL checks
(same items and generation; the second run had 2 cache hits), `compare`
(`observed_difference`, 1 differing case) and `replay --kind compare`
(`outcome_reproduced`), then `trace` of a script that imports a nested module
(14 events, stdout `{"n": 2}`) and `replay --kind trace`. The helper file in
the isolated cache matched the embedded source (20798 bytes). Log:
`/tmp/checkweave-development/release-relocate.log`.

Default `startup_timeout_ms` remains 600000. `model setup` installs the
environment and does not return a warm model; the first request may load
weights. The 24.5 s first-ready time is the worker smoke. A user-path semantic
cold check was 45.8 s wall, the warm repeat 47 ms, and one edited row 7.2 s
([verification](verification.md)). About 180 s is the budget to document for
that first request, not a shorter default. A later change to the provider
binary, including a device-aware torch bootstrap, needs a new locked release
build before this artifact is treated as current. A physical-disk
cold-start failure around 15 s happened under heavy IO. The same binary on
tmpfs passed 20 cold clients, maximum 88 ms. That does not prove a startup
race is fixed; integration is still improving the diagnostic. See
[performance](performance.md).

The same binary was packed as
`checkweave-0.1.0-x86_64-unknown-linux-gnu.tar.gz` with
`LICENSE`, `docs/usage.md`, and `docs/platforms.md`. The archive of the same
binary plus the docs in this tree is sha256
`10a9d9b5e025887dea2942c5ceb14b9e2ce41782627c8030248076dca1c333b4`.
`scripts/install.sh`
installed a stub, replaced it from that archive, left the file in place on a
second install of the same bytes, and refused a bad checksum without
replacing the binary. The installed copy completed a JSONL check. Uninstall
removed only that binary; a neighbor file, `.checkweave/keep-state`, and an
unrelated `.cursor/mcp.json` stayed. A second uninstall was a no-op success.
Log: `/tmp/checkweave-development/release-installer.log`. PowerShell was not
executed on this host.

macOS and Windows release jobs are configured and have not been executed. No
tag or GitHub Release was published.

## 2026-09-24 release preparation

The cold-start failure was a client timeout, not a lost daemon. The lock
holder was still inside `Engine::open` (full `integrity_check` plus reclaim)
when every client stopped at 15 s, and a client never respawned after its first
spawn exited on lock contention. `daemon::request` now extends its deadline
while the lock is held (cap 120 s), respawns at most every 500 ms when nothing
holds it, and names `daemon.log` in the error. The daemon logs
`opening cache` and `cache opened in N ms`. `cargo fmt --check` and
`cargo clippy --all-targets -D warnings` are clean. Root suite: 151 passed,
2 ignored (managed model installs). The ignored CPU smoke no longer hardcodes a
home directory; it reads `HF_HUB_CACHE` or `$HOME/.cache/huggingface/hub`.
