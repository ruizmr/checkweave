# Integration work log

This page preserves implementation and test snapshots from development. For
current setup instructions, read [Using Checkweave through MCP](mcp.md).
[Verification](verification.md) records the later cross-platform release run;
the limitations and pending checks below describe the time of each note.

## What is wired

CLI and MCP go through one daemon per workspace, except `init` and `deinit` (`uninit`). `deinit` asks a live daemon to shut down and does not start one, then removes only a managed `mcpServers.checkweave` command and a `checkweave.mdc` that contains `checkweave:managed`.

`compare`, `trace`, `model evaluate`, and `semantic` use the typed request structs. Semantic calls `semantic::check` with the shared `ModelProvider`. Evidence looks in the collection engine, then trace, then semantic. Model setup uses `models::setup_cancellable` and drops the cached provider afterward. Evaluate and semantic reload that provider when the config source or settings fingerprint changes. Waiting for the provider lock observes cancel and the request deadline.

At most 8 runs may be queued or running. Finished run records stay within 32 entries and 8 MiB of stored results. Shutdown abandons runs, stops in-flight checks, waits up to 5 seconds, aborts the rest, and drops the provider. A queued or running run counts as daemon work, so a 1-second idle timeout does not exit during a detached run. Synchronous CLI polling checks the handle immediately, then waits 5, 10, 20, 50, and 200 milliseconds. If startup holds the init lock past the ready deadline, the client error says that explicitly.

Hosted Jev is opt-in. This note does not qualify the SemIf/Qwen checkpoint.

## Commands and results

`rustfmt --edition 2024` on `src/daemon.rs`, `src/main.rs`, and `tests/end_to_end.rs`.

```text
cargo test --offline --locked --test end_to_end --test interface --test workspace --test daemon -- --test-threads=1
```

That run, before the idle-timeout and adaptive-poll edits, passed daemon 15, end_to_end 10, interface 5, workspace 12.

After those edits:

```text
cargo test --offline --locked --test end_to_end -- --test-threads=1 \
  detached_run_survives_idle_timeout \
  compare_trace_and_semantic_predicate_round_trip \
  provider_reloads_when_config_changes_from_local_to_hosted_mock \
  run_queue_rejects_when_eight_jobs_are_inflight \
  cancelling_one_shared_check_lets_the_other_finish
```

5 passed. The semantic case uses `CHECKWEAVE_PYTHON` as a ready-frame mock and `profile = "lightweight"` so it does not download a checkpoint. The hosted case posts one request to a local HTTP mock.

Global `cargo fmt` and clippy were not run. Other workers were still editing when this note was written. No publish.

## Polish

`checkweave evidence ID` is unchanged. `checkweave evidence ID --offset N --limit M` and MCP `checkweave_trace_page` call `trace::evidence_page`. A fresh trace response keeps the first 64 events and reports `event_total`. Returned trace JSON is capped at 6 MiB (`TRACE_RESPONSE_MAX`); stored `events.jsonl` still uses the 8 MiB capture cap. `stdout_omitted_bytes` and `stderr_omitted_bytes` record display truncation. The captured files stay.

`check --max-results 0` is accepted by CLI and MCP and returns coverage counts with an empty `items` list.

Omitted `model evaluate` `timeout_ms` is 180000. Explicit values stay strict. An omitted semantic `timeout_ms` is also 180000; an explicit semantic timeout stays as written. `model setup` does not warm the model.

`real_worker_smoke_when_requested` is `#[ignore]` unless `CHECKWEAVE_SEMANTIC_SMOKE=1` and `--ignored`.

`cargo clippy --offline --locked --all-targets -- -D warnings` is clean except `tests/models.rs` (provider-owned): useless `format!` at lines 175 and 180, and a field assignment outside a `Default` initializer at line 963. `src/models.rs` produced no clippy error in that run. Lock files use `.truncate(false)` with `.create(true)`. `fs2::FileExt::unlock` is used where `File::unlock` would require a newer compiler. MSRV stays 1.88.

Release notes for the new flags are in `/tmp/checkweave-development/release_docs-inbox.md`.
