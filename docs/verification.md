# Completion evidence

This checklist preserves the project requirements through implementation. Passing
a kernel test does not qualify model judgments or establish release readiness.
Current implementation results are recorded in [implementation.md](implementation.md).

| Requirement | Evidence required |
| --- | --- |
| Rust runtime and common types | Native executable builds; CLI/MCP exercise the same engine and return equivalent typed results |
| Initialization | Idempotence, unrelated configuration preserved, invalid configuration not overwritten, subdirectories and linked worktrees resolved correctly |
| Shared worker | Concurrent cold starts reach one worker, protocol mismatch rejected, idle exit, reconnect after crash, bounded client queue |
| Incremental collection checks | Exact counts, malformed/missing/type cases, changed rows recomputed, unchanged rows reused, predicate identity changes |
| Freshness | Same-metadata content changes, new/deleted/renamed matching files, ignore changes, checkout, restart, edit during evaluation, stale historical evidence |
| Bounded disposable cache | Atomic publication, interrupted transaction recovery, corruption recovery, evidence expiry, storage reclamation |
| Automatic upkeep | Native events, startup reconciliation, missed-event/polling fallback; queries validate actual bytes |
| Resource limits | File/byte/item/time/result limits, explicit partial/cancelled outcomes, bounded protocol frames and background work |
| Behavior comparison | Known preserving/differing corpus, supplied/generated inputs, honest unsupported/nondeterministic outcomes, reduction and runnable reproduction |
| Git history | Ordinary Git reads/worktrees, committed and dirty source identities, no modification of user's checkout |
| Semantic provider boundary | Typed choice/predicate/rubric capabilities, validation, batching, provenance, explicit unresolved/unsupported outcomes |
| Local inference | Pinned automatic cached environment/checkpoint setup, offline restart, supervised lazy worker, real accelerator probe and CPU fallback |
| Optional hosted inference | Explicit configuration, concrete Jev model identity, bounded retries/Retry-After/cancellation, no implicit local-to-hosted fallback |
| Semantic qualification | Independent labeled sample, baseline, precision/recall/unresolved/confident errors, adversarial/negation/order/long/multi-question cases, latency/memory/cost |
| Execution evidence | Source-linked structured observations, bounded retrieval, revalidation/replay, known debugging cases and instrumentation overhead |
| Agent experience | One-time integration, compact responses, bounded expansion, longer-run handles/progress/cancellation, complete task comparison |
| Release | Tested platform matrix, binary artifacts, install/init/upgrade/removal, crash/watcher/cache recovery, idle/startup/repeated latency/disk measurements |

Only tested OS/device/runtime combinations may be described as verified. Release
publishing and broad semantic accuracy remain separate gates from runnable code.

## Recorded, not closed

These notes are snapshots. A later edit can invalidate them. This file does
not mark a milestone complete.

| Snapshot | What it recorded |
| --- | --- |
| [Kernel validation](kernel-validation.md) | Historical debug-binary snapshot: acceptance 11 passed; adversarial 11 passed and 3 failed (record budget, predicate cache identity, deep `not`). Those three failures are not the present status. Later suites superseded that snapshot. Median CLI round trip 82.33 ms on that debug corpus, not a release benchmark. |
| [Integration](integration.md) | Historical snapshot: `cargo test --offline --locked` for `end_to_end` (5), `workspace` (15), `daemon` (12), `interface` (5). Semantic collection was not on CLI or MCP in that note. That absence was fixed later; `semantic` is a command now. |
| Root suite, after the SemIf default | `cargo test --locked --all-targets --no-fail-fast`: 142 passed, 1 ignored. Includes collection 17, compare 12, daemon 15, end_to_end 11, models 17 + 1 ignored, semantic 9, trace 17. The ignored models test is the managed install, not a skipped failure. |
| Managed CPU install | `cargo test --test models --offline -- --ignored installed_canonical_worker_cpu_smoke` passed in 63.58 s. Printed `smoke device=cpu fallback=None precision=gguf-q4_k_m`. Canonical embedded worker. Not a quality eval. |
| [Semantic validation](semantic-validation.md) | Same 64 supported labels: SemIf Q4 CPU 63/64, GLiNER 50/64. Predicate gold 11/12. Artifact `experiments/semantic-validation/results/semif-q4-cpu.json`. Ready 24.78 s, warm p50/p95 8.865/11.482 s, peak RSS 4,901,544 kB. Not the 534-item BF16 board. `release_qualified` stays false. |
| [Python worker](python-worker.md) | SemIf CPU smoke succeeded: ready 24.5 s, wall 47.3 s, `gguf-q4_k_m`, `n_gpu_layers` 0. Artifact `python/tests/results/semif-cpu-smoke.json`. Not automatic GPU, not BF16, not calibration. |
| User-path semantic smoke | Debug CLI, shared daemon, canonical worker via `CHECKWEAVE_PYTHON`. Cold wall 45847 ms (2 misses, 1 fresh model call); warm wall 47 ms (2 hits, 0 model calls); one edited row wall 7216 ms (1 hit, 1 miss). All `complete`. Artifacts under `/tmp/checkweave-development/semantic-cli-smoke/`. Not the 64-label eval. |
| Release build | `cargo build --locked --release` succeeded on this Linux x86_64 host in 45.45 s. Relocated binary: deterministic JSONL reuse, compare/replay, and trace/replay with the embedded helper and no checkout. Real archive install, same-bytes reinstall, bad-checksum refusal, upgrade from a stub, and uninstall of only that binary passed. macOS and Windows were not run. |
| Startup timing | A ~15 s physical-disk cold-start failure occurred under heavy IO: the lock holder had not finished opening the cache, and clients gave up at a fixed 15 s without respawning. Clients now keep waiting (up to 120 s) while the daemon lock is held, respawn when no daemon holds it, and the daemon logs its cache-open time. `client_waits_past_ready_timeout_while_lock_is_held` holds the lock for 17 s and passes. Release binary on ext4 at load ~6: 20 rounds × 4 cold clients, 0 failures, max 137 ms. Not rerun under the original heavy-IO load. |

Still open: the 534-item board on this machine, native CUDA, release-workflow
execution on the five runners, and a demonstrated agent-efficacy benefit.
The paired study finished all three tasks in both arms; the Checkweave arm
only ran `--help` ([performance](performance.md)). Linux and macOS pass the
release dry run; Windows ships through WSL2 and native Windows
is not a target. No GitHub Release or tag has been published. The
recorded release binary predates the integration-polish and provider GPU
bootstrap; the final artifact waits on
`/tmp/checkweave-development/final-source-ready.json`.
