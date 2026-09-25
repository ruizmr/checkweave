# Performance

This page records release-profile and historical debug samples, plus fresh Cursor CLI task studies. The host was busy. The numbers describe that run. They are not an intrinsic latency of the CLI, and they are not a release qualification by themselves.

A same-sha256 debug binary had already failed a four-client cold start on this ext4 volume in `/tmp/checkweave-development/startup-audit/result.json` (about 15s, `No such file or directory` while a daemon was already running). A separate `/dev/shm` run of that audit passed. The client-side cause is fixed; see [verification](verification.md). This harness does not replace that audit. Its ext4 sample is the physical-filesystem result. `experiments/agent-tasks/results/debug-tmpfs-diagnostic.json` is a second run of the same harness on tmpfs, kept as a diagnostic.

## Release-profile sample, 2026-09-25

The current local `checkweave 0.1.0` release-profile binary is 17,321,496 bytes,
sha256 `fafa7780aac9116a239999c1c828b17a7e6e95ac985942b70c1f563da77ad67a`.
It includes the MCP text-evidence fix and is not a published asset. Full samples
and fixture hashes: [release measurement](../experiments/agent-tasks/results/release-with-mcp-text-20260925.json).
Linux x86_64, ext4 under `/tmp`, other work running; one-minute load rose from
8.33 to 9.72. The earlier same-day sample remains in
[release-20260925.json](../experiments/agent-tasks/results/release-20260925.json).

| Measurement | Result | Scope |
| --- | --- | --- |
| Warm check, 40 records | 21.85 ms median, 53.82 ms p95 | 20 samples, 40 hits, same daemon |
| Cold check after shutdown | 75.83 ms median, 88.89 ms p95 | 8 samples, process and worker startup, cached rows |
| Idle CPU | 0.04 CPU seconds / 5.000 seconds (0.80% of one core) | One window, 100 Hz process accounting |
| Idle RSS | 14,155,776 bytes at both endpoints | Resident memory, not peak memory |
| SQLite family, 40 records / 29 checks | 4,247,032 bytes | Main database, WAL, and SHM |
| SQLite family, 8,000 records / 2 checks | 5,807,632 bytes | 8,000 cached items |
| Discount-script trace overhead | 4.382 ms median, 8.372 ms maximum | 5 samples, 12 events each, no dropped events |

Trace timing compares the code body without hooks to the traced code body,
inside the same interpreter; the baseline runs first and may warm imports.
It excludes interpreter startup. The CLI wall times remain in the raw artifact.
This tiny script does not qualify instrumentation overhead on a large application.
Native calls, subprocess events, async scheduling, pre-existing threads, and
causal event ordering remain unsupported, as recorded in each trace.

## Cursor discovery and evidence delivery, 2026-09-25

Three fresh sessions used `grok-4.7-medium-fast`: baseline, ordinary-language
discovery with the managed Cursor rule/MCP server, and explicitly guided use.
Each solved the same policy-count, differing-invoice, and originating-exception
tasks. The fixture hashes match; no external MCP calls were recorded. The grader
checks the counts/evidence, runs the submitted reproduction, and checks the
originating exception location. It found no missed seeded bugs in any arm.
This is one run per arm, with the baseline shared by the two comparisons.

The [initial study](../experiments/agent-tasks/results/agent-study-20260925.json)
passed all three arms. Discovery invoked check, compare, trace, and replay,
but Cursor's recorded tool responses exposed only our summary text. Evidence
was present in `structuredContent`, which that client did not expose. The study
also recorded a rejected `max_results=100000` and a compare id mistakenly passed
to collection evidence. The server now includes the bounded report as JSON text,
mentions the result cap, and clarifies which ids the supporting tools accept.

The [follow-up study](../experiments/agent-tasks/results/agent-study-text-20260925.json)
used the fixed binary above. Both treatment sessions received two text blocks,
including detailed check, comparison, and trace evidence. No MCP tool errors
were recorded. The exception fixture correctly produced a trace of a failed
program; that is evidence, not a tool failure.

| Arm | Tasks passed | Process wall time | Input tokens | Output tokens | Cache-read tokens |
| --- | --- | --- | --- | --- | --- |
| baseline | 3/3 | 38.73 s | 35,677 | 3,737 | 99,840 |
| discovery | 3/3 | 53.02 s | 81,731 | 4,441 | 162,816 |
| guided | 3/3 | 58.93 s | 51,111 | 5,678 | 256,000 |

Discovery used check twice, compare once, and trace once. Guided use called
check, evidence, compare, and trace once each. Both remained slower and used more
tokens than baseline. The earlier treatment wall times were 78.53 and 90.84 seconds;
the follow-up was shorter, but single runs do not establish a causal speedup.
No dollar-cost or general quality advantage is claimed. All raw usage fields,
including cache writes (zero), grades, tool summaries, and binary hashes are in
the artifacts. Source logs and submissions remain in the temporary study folders.

The next integration work is reducing evidence volume and unnecessary calls on
larger tasks. The trace response in this sample was about 30 KB. A result limit
is not by itself a useful token budget.

## Historical debug sample

### Environment

| Item | Value |
| --- | --- |
| Host | Linux guardian 6.8.0-138-generic, x86_64, Python 3.12.9 |
| Load at start | 16.64, 48.59, 45.71 |
| Load at end | 14.26, 45.47, 44.75 |
| Workspace filesystem | ext4 `/dev/mapper/ubuntu--vg-ubuntu--lv`, temporary directory under `/tmp` |
| Binary | `target/debug/checkweave` |
| Profile | debug (`with debug_info`, not stripped) |
| Size | 178818120 bytes |
| sha256 | `109f1f7169fdef9ea5b467fc1c8de4e1fd6f11c4c0267c2cfe83952c6c8cf498` |
| Build ID | `8d61f6dcd2270f5429ed44a31c378856044dcf11` |
| Fixture spec sha256 | `6005b0cea5f7314040353c0a260cce5ef0c23fa3d3b8f68c7396c4ce717e9c0d` |
| Small input sha256 | `ba265fcd68dcdaa27be460bff6cdf793fa80f4cb1f660d901488104724e3c93b` |
| Large input sha256 | `860b8911a8454e7be0d187b7ed0e813fc8801b988013d37d17ccfa6afa0f6158` |

The process never uses the user's project as a workspace. Recovery, crash, and cache-corruption checks stay in `scripts/acceptance.py`.

## Method

CLI flags match acceptance: `--workspace`, `init --agent none`, `check --include '**/*.jsonl' --predicate {"op":"gt","path":"/n","value":1}`. `--max-results 1` keeps the printed JSON small. Other check limits stay at the CLI defaults, except the 8000-record corpus uses `--timeout-ms 120000`.

Median is the statistical median. p95 is nearest-rank, `ceil(0.95 * n)`. For n=8 that rank is the maximum sample. For n=4 it is also the maximum. Those p95 figures are order statistics of a short sample.

Wall-clock time wraps the client process. `elapsed_ms` is the field the check report records. Each warm or changed-row sample is one client invocation against a daemon that stays up. Cold samples call `shutdown` and wait until that pid is gone before the next check.

## Ext4 debug sample

Full samples: `experiments/agent-tasks/results/debug-baseline.json`.

| Measurement | n | Median | p95 nearest-rank | What it includes |
| --- | --- | --- | --- | --- |
| `checkweave --help` | 8 | 15.3 ms | 21.2 ms | Process startup and help text |
| First check, empty record cache, 40 records | 1 | 301 ms wall, report 8 ms | — | Daemon spawn plus 40 misses |
| Warm check, same 40 records | 20 | 241 ms wall, report 8 ms | 256 ms wall | Same daemon, 40 hits, 0 misses |
| One new record body | 8 | 245 ms wall, report 13 ms | 258 ms wall | Same daemon, 39 hits, 1 miss |
| Cold check after shutdown | 8 | 303 ms wall, report 11 ms | 554 ms wall | New process, daemon respawn, cached records |
| Idle exit, `CHECKWEAVE_IDLE_SECONDS=2` | 4 | 2056 ms | 2357 ms | From check return until the daemon pid is gone |

The warm daemon pid stayed constant, including across the one-record edits. Unchanged records were cache hits. The edited record was one cache miss. On this small file the client wall time stays near 240 ms while the report's own elapsed time stays near 8–13 ms, so most of the wall time is outside the recorded check body.

The 8000-record corpus is 4 files × 2000 records, one pair of checks:

| Check | Wall | Report elapsed | Cache |
| --- | --- | --- | --- |
| First | 1172 ms | 1036 ms | 0 hits, 8000 misses |
| Second | 2153 ms | 2030 ms | 8000 hits, 0 misses |

The second check did not recompute record predicates (`cache_misses` is 0). It was not faster in this sample. Zero misses are the reuse observation. A latency drop is not.

### Disk

Report payload is the `payload_bytes` sum stored on `reports`. The on-disk footprint is the SQLite main file plus `-wal` and `-shm`.

| Corpus | Reports payload sum | `cache.sqlite` | `-wal` | Family bytes | Item rows |
| --- | --- | --- | --- | --- | --- |
| 40 records after 29 checks | 23681 | 90112 | 4124152 | 4247032 | 48 |
| 8000 records after the first check | 1376 | 4096 | 2838712 | 2875576 | 8000 |
| 8000 records after the second check | 2752 | 1646592 | 4128272 | 5807632 | 8000 |

After the first large check the main database file was still 4096 bytes while SQLite reported 400 pages (1638400 bytes allocated) and the WAL held 2838712 bytes. The stored report stayed small because `--max-results 1` limits returned items. The item cache is the larger table.

## Tmpfs diagnostic

`experiments/agent-tasks/results/debug-tmpfs-diagnostic.json` repeats the harness with `--parent /dev/shm`. Role is `diagnostic_tmpfs`. Warm wall median was 239 ms. The 8000-record checks were 1180 ms then 1546 ms wall, again with 8000 misses and then 0 misses. It does not replace the ext4 sample and it is not the four-client startup audit.

## Agent tasks

Three tasks sit in `experiments/agent-tasks/fixtures/`. The brief asks for counts with copied field evidence, a one-line behavior difference that a script prints, and the originating exception with a source line. Ordinary Python can solve each one. Checkweave was on `PATH` only for the checkweave arm. The grader runs `submission/repro.py` from a temporary workspace that has `TASK/` and `submission/`, matching the brief. It checks the structured answer against the fixtures. It does not require a particular tool sequence.

Private expected values stay in `experiments/agent-tasks/private/expected.json`, which is not committed; `validate_submission.py --write-expected` regenerates it from the fixtures. Both actor `TASK/` trees still hash to `defd5ee4e739c8f73825f4d99479a16919c0d5abc3f4263782e7a8bf16447516`, the hash from launch. The brief sha256 is still `366057f74bbe9064c8ae841e9142c2b4983023060c8b8302ec509615d6670a58`.

### Paired actor sample

Two fresh actors, model `grok-4.7-medium-fast`, immutable debug binary sha256 `eed0e864724ea8a110add1d2dec6a09076d34128196401f7f117268336c1cf09` (`/tmp/checkweave-agent-study/launch-metadata.json`). This binary is not the earlier harness binary `109f1f71…`. `target/release/checkweave` was not present during that historical study; the later release-profile sample is above.

Both submissions passed `validate_submission.py` after the grader was fixed to keep `TASK/` beside `submission/repro.py`. Reports: `experiments/agent-tasks/results/baseline-validation.json` and `experiments/agent-tasks/results/checkweave-validation.json`.

| Arm | Duration | Input tokens | Output tokens | Cache read | Cache write | Tool events started | Tool events completed |
| --- | --- | --- | --- | --- | --- | --- | --- |
| baseline | 36903 ms | 56390 | 3261 | 95104 | 0 | 14 | 14 |
| checkweave | 45245 ms | 40567 | 4483 | 102400 | 0 | 16 | 16 |

Raw usage objects are in `experiments/agent-tasks/results/agent-paired-sample.json` under `usage_raw` (`inputTokens`, `outputTokens`, `cacheReadTokens`, `cacheWriteTokens`). Started and completed tool events are counted separately from shell invocations of Checkweave. This is one paired sample. It is not a statistical comparison.

The checkweave actor’s completed shell calls include one `checkweave --help` and no later `checkweave` subcommand. Paths that contain the workspace name were not counted. That arm is a tool-discovery sample: the binary was available, the actor printed help, and the tasks were solved with Python and file reads. The lower input-token count is not evidence of a cost or quality benefit.

## Reproduce

```sh
python3 experiments/agent-tasks/perf_harness.py \
  --binary target/debug/checkweave \
  --parent /tmp \
  --out experiments/agent-tasks/results/debug-baseline.json
```

For the current harness, use `target/release/checkweave` and a new output path.
Do not overwrite the historical results. To reproduce the fresh Cursor study:

```sh
python3 experiments/agent-tasks/run_study.py --study /tmp/checkweave-new-study prepare --binary target/release/checkweave
python3 experiments/agent-tasks/run_study.py --study /tmp/checkweave-new-study run baseline
python3 experiments/agent-tasks/run_study.py --study /tmp/checkweave-new-study run discovery
python3 experiments/agent-tasks/run_study.py --study /tmp/checkweave-new-study run guided
python3 experiments/agent-tasks/run_study.py --study /tmp/checkweave-new-study summarize --out /tmp/checkweave-new-study/results.json
```

The runner refuses to overwrite sessions. It uses the current Cursor account,
model access, and global MCP configuration; it does not change HOME. The actor
is instructed to stay in its fixture workspace, and the summary flags external
MCP calls. That instruction is not a security sandbox.
