# Kernel validation

Independent checks against the documented collection and daemon invariants.
This is not a release qualification and it does not measure end-to-end performance.

## Commands

```sh
cargo test --offline --test adversarial -- --test-threads=8
python3 scripts/acceptance.py --binary target/debug/checkweave \
  --output /tmp/checkweave-development/kernel_adversarial-acceptance.json
```

`cargo test --offline --test adversarial` on the current tree: **16 passed, 0 failed**, 6.90s.

Acceptance was run after that debug binary was built. The first attempt failed one of four cold `status` calls with `checkweave request failed: No such file or directory (os error 2)`. The same command, run again immediately, passed all 11 checks. The harness was not changed to retry that call.

## Harness corrections

`a.jsonl` and `b.jsonl` are the same two rows, so a check sees four paths and two contents. The cache key is content plus predicate. A cold or new predicate therefore records **2 misses and 2 within-request hits**. A later identical predicate records **4 hits**. Requiring `cache_hits == 0` on a new predicate was an invented invariant and has been removed. The test still requires the new predicate's decisions, distinct source paths, and full hits on the warm repeat.

A 4096-deep `Not` chain aborted the test process while the `Box` chain was dropped, including when validation would already have rejected it. The harness now uses depth 48, which is past the kernel predicate bound and inside serde's default JSON recursion limit. `check` runs while the request is still borrowed, and the chain is dropped iteratively. The depth-48 predicate is still required to be rejected before evaluation.

Integers above `u64` that differ by one (`2^64` and `2^64+1`) must match exactly or be unresolved. A float collapse that matches both is a failure. That fixture passed.

## Scope coverage

| Area | Result |
| --- | --- |
| Same-size, same-mtime content change and stale evidence | passed |
| Ignore negation and membership | passed |
| Symlink escapes and internal symlink non-membership | passed |
| Invalid UTF-8, blank lines, malformed JSON, huge line vs `max_bytes` | passed |
| Invalid regex, bare path, non-finite comparisons, shallow `not` | passed |
| Predicate depth 48 rejected without a drop abort | passed |
| Integers above 2^53 and `u64::MAX` | passed |
| `2^64` vs `2^64+1`: exact or unresolved, no false equal | passed |
| Missing vs null, exists/eq/kind/gt, all/any/not | passed |
| Distinct paths, warm all-hits, new-predicate 2 misses + 2 hits | passed |
| In-flight atomic replacement is not a false validated snapshot | passed |
| Cancelled check is not stored as complete; reopen keeps the prior report | passed |
| Corrupt SQLite recovery vs an unreadable sibling file | passed |
| `max_records`, `max_results`, and `max_files` across two files | passed |
| Concurrent daemon start, shutdown, reconnect, kill of that pid only | passed |
| Acceptance: cold/warm, membership, ignore, restart, eight-client dedup, git checkout, linked worktree, crash, idle | passed on the second run (11 checks) |
| Background poll interval forced with native events disabled | not run; no supported switch. Query freshness is the same-mtime check. Status reported `watcher: native` |
| CLI/MCP parity | not re-executed here. `tests/interface.rs` compares a stable CLI check with `checkweave_check` |

Concurrent acceptance clients may share one report id. Cache misses were summed once per id. That check passed.

Debug CLI round trips on the successful acceptance run: median 412.23 ms, max 1678.86 ms, 25 check calls. Those are samples from this host's debug binary, not a release benchmark.

No new Cargo dependencies.
