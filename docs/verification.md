# Completion evidence

This checklist preserves the project requirements through implementation. Passing
a kernel test does not qualify model judgments or establish release readiness.
Current implementation results are recorded in [implementation.md](implementation.md).
The evidence below covers specific scenarios; requirements beyond that scope
remain open.

## Current cross-platform baseline

Authoritative for release status as of 2026-09-24.

[GitHub Actions run 36070429830](https://github.com/ruizmr/checkweave/actions/runs/36070429830) was a `workflow_dispatch` of commit `8b05c10ed79fbc4c9ff2c8b2a542eac51ce402eb`. All four native build, test, and package jobs passed: Linux x86_64, Linux ARM64, macOS Intel, and macOS Apple silicon. The Windows WSL installer smoke passed on a Windows runner using Ubuntu 24.04. The publish job was skipped. This was a dry run, not a published release.

`gh release list` was empty, and the tags API was empty. No GitHub Release and no tag exist. The matrix and the WSL install path are in [platforms](platforms.md).

Source and tests are that release tree, plus documentation. On this Linux x86_64 host, `cargo build --locked --release` passed, and `cargo test --locked --test interface --test workspace` passed interface 5 and workspace 12. This page does not record a binary hash. The local binary is not the set of archives from the Actions run, and it is not a release asset.

This baseline covers the combinations those jobs built. It does not cover native Windows, every WSL architecture, or any semantic CPU or GPU device.

## Documentation walkthrough — 2026-09-24

The source installation and [Getting started](getting-started.md) commands
were exercised on Linux x86_64 using the current release build in a temporary
workspace. The first check reported one missing owner; the repeat reused both
records; adding the owner produced zero matches, one hit, and one miss. The
comparison reported totals of 4 and 2, and its retained input replayed with
`outcome_reproduced: true`. The worker was shut down afterward.

`cargo test --locked --test interface --test workspace` passed all 17 tests,
covering the CLI/MCP check-and-evidence round trip and managed Cursor setup.
This verifies protocol and configuration behavior, not a real editor session.
Documentation links, heading anchors, and JSON examples were checked locally.

## Requirements

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

## Recorded evidence for those gates

The test suites below were included in Actions run 36070429830. The mapping
identifies relevant regression coverage; broader validation work is listed in
[Still open](#still-open).

| Requirement | Recorded so far |
| --- | --- |
| Rust runtime and common types | The release workflow built and tested the four native targets. Local `cargo build --locked --release` passed. `cargo test --locked --test interface` passed 5. |
| Initialization | [tests/workspace.rs](../tests/workspace.rs): `preserves_unrelated_files_and_is_idempotent`, `concurrent_initialization_preserves_other_servers`, `linked_worktrees_have_distinct_roots`. |
| Shared worker | [tests/daemon.rs](../tests/daemon.rs): `concurrent_clients_share_one_daemon`, `idle_shutdown_removes_socket`, `protocol_version_mismatch_is_rejected_and_worker_stays_up`, `client_waits_past_ready_timeout_while_lock_is_held`. [tests/adversarial.rs](../tests/adversarial.rs): `concurrent_daemon_startup_shutdown_and_crash_reconnect`. |
| Incremental collection checks | [tests/collection.rs](../tests/collection.rs): `cold_warm_counts_and_one_changed_record`, `limits_partial_cancel_and_predicate_validation`. |
| Freshness | `same_size_same_mtime_is_a_content_change`, `additions_deletions_renames_and_ignore_rules`, `mutation_during_check_cannot_publish_a_false_snapshot`, `evidence_missing_and_stale_after_edit`, `cancelled_check_does_not_publish_complete_and_restart_keeps_prior`. Git checkout of the checked collection, and a missed watch event, have no matching name in this list. |
| Bounded disposable cache | `corrupt_cache_recovers_without_hiding_permission_errors`, `reopen_keeps_cache_and_corrupt_db_is_quarantined`, `cancelled_check_does_not_publish_complete_and_restart_keeps_prior`. |
| Automatic upkeep | Queries in the collection tests read the bytes they check. A missed-watch-event end-to-end case is not in this list. |
| Resource limits | `limits_partial_cancel_and_predicate_validation`, `max_records_is_global_across_files`, [tests/trace.rs](../tests/trace.rs) `event_limit_drops_and_marks_partial`, `timeout_cancels_the_process_group`. |
| Behavior comparison | [examples/behavior](../examples/behavior/) holds the preserving and differing programs. [tests/compare.rs](../tests/compare.rs): `corpus_preserving_change_is_not_a_regression`, `corpus_difference_reduces_and_replays_after_edit`, `crashes_and_parser_failures_are_unsupported`, `unstable_stdout_is_nondeterministic`, `generated_arrays_use_the_seed_and_retention_is_bounded`, `generated_integer_range_is_deterministic_and_shrinks`, `reproduction_argv_replays_with_the_built_binary`. |
| Git history | `historical_revision_does_not_switch_checkout_or_mix_dirty_bytes`. That case keeps the user's checkout stable during a historical compare. It is not a collection recheck after checkout. |
| Semantic provider boundary | [integration](integration.md) already routes semantic through the shared provider. Its targeted `end_to_end` run passed 5, including `compare_trace_and_semantic_predicate_round_trip`. The historical root suite included semantic 9. |
| Local inference | Managed CPU install and the Q4 sample in the historical table below. Those runs are CPU evidence, not a device matrix. |
| Optional hosted inference | The same targeted integration run posts one hosted request to a local HTTP mock. That is a wiring check. |
| Semantic qualification | [Semantic validation](semantic-validation.md): 63/64 supported labels, predicate gold 11/12. `release_qualified` stays false. The 534-item board is not in that note. |
| Execution evidence | [tests/trace.rs](../tests/trace.rs): `wrong_intermediate_is_tied_to_the_executed_line`, `baseline_overhead_is_measured_only_when_requested`, `edit_marks_evidence_stale_and_replay_uses_snapshot`, `source_modified_during_run_is_never_validated`, `nested_script_imports_its_sibling_and_replay_keeps_that_module`. |
| Agent experience | `init` writes the Cursor entry ([usage](usage.md)). The paired study finished three tasks in both arms, and the Checkweave arm ran only `--help` ([performance](performance.md)). |
| Release | The four native package jobs and the WSL installer smoke passed in the baseline above. Publish was skipped. The 2026-09-22 local archive install in the historical table is an earlier Linux artifact, not those published assets. |

## Recorded, not closed

These notes are snapshots. A later edit can invalidate them. This file does
not mark a milestone complete. The local release binary and archive hashes in
this table are earlier artifacts. They are not the dry-run archives, and they
are not current release assets.

| Snapshot | What it recorded |
| --- | --- |
| [Kernel validation](kernel-validation.md) | Historical debug-binary snapshot: acceptance 11 passed; adversarial 11 passed and 3 failed (record budget, predicate cache identity, deep `not`). Those three failures are not the present status. Later suites superseded that snapshot. Median CLI round trip 82.33 ms on that debug corpus, not a release benchmark. |
| [Integration](integration.md) | `cargo test --offline --locked` for `daemon`, `end_to_end`, `interface`, and `workspace` passed daemon 15, end_to_end 10, interface 5, workspace 12. A later targeted `end_to_end` run passed 5, including `compare_trace_and_semantic_predicate_round_trip`. Semantic collection was already wired in that note. |
| Root suite, after the SemIf default | `cargo test --locked --all-targets --no-fail-fast`: 142 passed, 1 ignored. Includes collection 17, compare 12, daemon 15, end_to_end 11, models 17 + 1 ignored, semantic 9, trace 17. The ignored models test is the managed install, not a skipped failure. |
| Managed CPU install | `cargo test --test models --offline -- --ignored installed_canonical_worker_cpu_smoke` passed in 63.58 s. Printed `smoke device=cpu fallback=None precision=gguf-q4_k_m`. Canonical embedded worker. Not a quality eval. |
| [Semantic validation](semantic-validation.md) | Same 64 supported labels: SemIf Q4 CPU 63/64, GLiNER 50/64. Predicate gold 11/12. Artifact `experiments/semantic-validation/results/semif-q4-cpu.json`. Ready 24.78 s, warm p50/p95 8.865/11.482 s, peak RSS 4,901,544 kB. Not the 534-item BF16 board. `release_qualified` stays false. |
| [Python worker](python-worker.md) | SemIf CPU smoke succeeded: ready 24.5 s, wall 47.3 s, `gguf-q4_k_m`, `n_gpu_layers` 0. Artifact `python/tests/results/semif-cpu-smoke.json`. Not automatic GPU, not BF16, not calibration. |
| User-path semantic smoke | Debug CLI, shared daemon, canonical worker via `CHECKWEAVE_PYTHON`. Cold wall 45847 ms (2 misses, 1 fresh model call); warm wall 47 ms (2 hits, 0 model calls); one edited row wall 7216 ms (1 hit, 1 miss). All `complete`. Artifacts under `/tmp/checkweave-development/semantic-cli-smoke/`. Not the 64-label eval. |
| Release build | `cargo build --locked --release` succeeded on this Linux x86_64 host in 45.45 s. Relocated binary: deterministic JSONL reuse, compare/replay, and trace/replay with the embedded helper and no checkout. Real archive install, same-bytes reinstall, bad-checksum refusal, upgrade from a stub, and uninstall of only that binary passed. macOS and Windows were not run. |
| Startup timing | A ~15 s physical-disk cold-start failure occurred under heavy IO: the lock holder had not finished opening the cache, and clients gave up at a fixed 15 s without respawning. Clients now keep waiting (up to 120 s) while the daemon lock is held, respawn when no daemon holds it, and the daemon logs its cache-open time. `client_waits_past_ready_timeout_while_lock_is_held` holds the lock for 17 s and passes. Release binary on ext4 at load ~6: 20 rounds × 4 cold clients, 0 failures, max 137 ms. Not rerun under the original heavy-IO load. |

## Still open

- **Published assets.** A tag and a GitHub Release. Install, upgrade, and removal of those published archives on the release platforms. The 2026-09-22 Linux archive log is an earlier host artifact.
- **Semantic qualification.** The independent labeled set, baseline, and device runs named in the qualification row. The 63/64 local Q4 sample is not the 534-item BF16 board. `release_qualified` stays false. Hardware qualification is only for devices that were actually run.
- **Agent efficacy.** A complete task with and without Checkweave where the tool arm uses the operations, with correctness, missed bugs, time, and tokens. The paired study ran `--help` only ([performance](performance.md)).
- **Release measurements.** Idle CPU and memory, cold startup, repeated-query latency, and disk use of the version to be released; trace usefulness and overhead on representative debugging tasks. The 82.33 ms debug median and the ext4 cold-client note above are historical.
- **Recovery evidence to extend.** A Git checkout that the collection check must follow, a missed watch event through to a byte-validated result, and environment regressions outside the tests listed above. Worker crash reconnect, corrupt-cache recovery, and the compare corpus in [examples/behavior](../examples/behavior/) already have the names in the mapping.
