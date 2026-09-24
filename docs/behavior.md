# Behavior comparison

Use a comparison when you want to know whether a refactor changed a program's
output. Checkweave runs both versions with the same input, records differences,
and can shrink a failing input into an easier case to investigate. That case
can help you write a regression test.

Start with the runnable example in [Getting started](getting-started.md).
This page is the detailed request and evidence reference. Each program must
accept JSON on stdin and return JSON on stdout. A difference tells you what
changed; deciding whether the change is a bug still requires the intended behavior.

The behavior adapter runs two explicit targets on the same inputs and reports
what the runs actually did. It does not watch the filesystem, and a filesystem
event never starts a command. [`execute::run`](../src/execute.rs) is the only
process entry point. `compare` and `replay` call it; nothing else in this
adapter does.

Arguments are an argv vector. The adapter does not start a shell and does not
expand `$`, backticks, or `;`. Absolute argv paths are rejected so a historical
run cannot reach back into the caller's checkout by path. Relative script
arguments are resolved by the command against the execution working directory.

## What an execution is

Each target reads one JSON value from stdin and is expected to write one JSON
document to stdout. The document may have trailing whitespace. A second value
or any trailing non-whitespace is `unsupported_output`. That parser failure is
not a semantic difference.

| Observation | Meaning |
| --- | --- |
| `completed` | Exit status success and one JSON document |
| `failed` | Nonzero exit. JSON is kept when the stdout document parsed; a crash with no JSON has `output: null` |
| `unsupported_output` | Success, but stdout was not one JSON document |
| `timeout` | The per-execution limit fired; the process group was killed |
| `cancelled` | The caller set the cancel flag |
| `output_limit` | Stdout or stderr exceeded `max_output_bytes`. Partial bytes are not parsed as a result |
| `containment_failure` | Output pipes stayed open after the group was killed |

Stdout and stderr are each capped at `max_output_bytes`. Declared sources are
fingerprinted with BLAKE3, at most 128 files and 16 MiB together. The inherited
process environment and any other external state are untracked. Declared
environment overrides are applied on top of that inherited environment; they
are not a sealed environment.

A working directory, a Unix process group, and a Git worktree are not a
sandbox. The command can read and write the host.

### Process cleanup

On Unix the command is placed in its own process group. When it exits, times
out, is cancelled, or hits the output cap, the adapter sends `SIGKILL` to that
group so a grandchild holding a pipe cannot stall capture or keep running.
This was exercised on Linux. It is not a claim about other operating systems.

On Windows, `kill_on_drop` terminates the direct child only. Descendant
processes are not contained, and this workspace has not tested Windows. Reports
include that containment string instead of a portable success claim.

## Compare request

`compare(root, request, cancel)` takes a `CompareRequest`:

- `before` and `after` are [`Target`](../src/execute.rs) values.
- `inputs` are caller-supplied JSON values.
- `generated` optionally adds a deterministic integer range and/or arrays from
  `seed` (SplitMix64). Generation order is inputs, then integers, then arrays.
- `budgets.max_cases`, `max_executions`, and `timeout_ms` bound the whole
  comparison, including git revision checks, detached worktree setup,
  stability repeats, and reduction. Each execution's timeout is clamped to
  the time still remaining. A process is not started when that remainder is
  already zero. Worktree removal is separately capped at 2 seconds and killed
  if it overruns; that cleanup is not counted as covered comparison time.
- `per_execution` is the [`ExecutionLimits`](../src/execute.rs) for every run,
  except that its timeout cannot outlive the compare deadline.
- `repeat` is at least 2. Each case runs each target that many times.
- `reduce` shrinks one stable difference inside the remaining budget.
- `before_revision` and `after_revision` are optional Git revisions.
- `policy.exit` is `require_success` (default) or `compare_exit`.
- `retention` bounds how many evidence directories and bytes are kept.

An empty input list is `unsupported`. Vacuous success is not preserved behavior.

## Outcomes

| Outcome | When |
| --- | --- |
| `observed_difference` | At least one input produced a stable, comparable difference |
| `bounded_no_difference` | Every selected case was stable and agreed. This is not equivalence |
| `nondeterministic` | A target disagreed with itself across `repeat` and no stable difference was kept |
| `unsupported` | Crashes, timeouts, output limits, non-JSON stdout, or missing historical files. These do not count as preserved behavior |
| `budget_exhausted` | Time or execution budget ended before the selected cases finished, and no stable difference was confirmed |
| `cancelled` | The cancel flag was set |

`require_success` compares JSON only when both sides `completed`. A nonzero
exit, timeout, or parser failure is `unsupported`, even if the other side
printed JSON. `compare_exit` also treats unequal exit codes as a difference
when both sides printed JSON. A crash that did not print JSON stays unsupported,
so a traceback is not reported as the original semantic difference.

Reduction accepts a candidate only when every repeat still shows that same
stable JSON (and exit, under `compare_exit`) difference. A candidate that
crashes or stops parsing is rejected. The search is deterministic ddmin over
array elements, sorted object keys, and string characters, plus numeric
candidates toward zero. The retained input is the smallest one accepted within
budget. The report says `smallest_retained_within_budget` and does not claim
the input is minimal.

`metrics.discovered_differences` counts stable differing cases.
`metrics.incorrect_regression_claims` is always 0: the adapter does not decide
whether a difference is a regression. `metrics.executions` and `elapsed_ms`
include original cases, repeats, and reduction. `metrics.reproducible` is true
only when source freshness is `validated` and the outcome is a stable
difference or a stable bounded agreement. A matching outcome enum with stale
or missing sources is not reproducible.

## Git

A revision is resolved with `git rev-parse --verify --end-of-options <rev>^{commit}`.
Execution uses `git worktree add --detach` in a temporary directory and
`git worktree remove --force` on the way out. The user's branch and worktree
are not checked out, switched, or reset.

Historical commands use the worktree as their root. Evidence stores the commit
hash and workspace-relative paths, never the temporary worktree path.

Dirty or untracked bytes are not copied onto a historical tree. If a declared
source is missing from that revision, the outcome is `unsupported`. If the
workspace copy differs and the blob exists in the revision, the clean blob is
what runs, and the report says the dirty bytes were not mixed.

## Evidence and replay

A finished compare writes `.checkweave/behavior/<uuid>/` and publishes
`manifest.json` last, via a temporary file and rename. The directory holds the
original request, a `witness-request.json` that reruns only the retained
input, a `cli-request.json` artifact, the retained input, one pair of
observations, and declared source bytes copied from the tree that actually
ran. Those bytes are kept only when their BLAKE3 hash still equals the
fingerprint captured for that execution. A mismatch is missing evidence and
stale freshness, not `bytes_retained`.

The report keeps aggregate counts for every case. It retains one sample pair
and, when there is a difference, the smallest stable counterexample found.
Stdout and stderr in that retained pair are capped, and a parsed document
larger than 64 KiB is dropped while its fingerprint remains. The published
directory and the report together stay inside an 8 MiB wire budget even if
`retention.max_total_bytes` or `max_output_bytes` is larger. A larger retention
request is clamped and the report says so.

`cli-request.json` carries a direct argv, independent of the caller's current
directory:

```json
{
  "argv": ["checkweave", "--workspace", "<absolute-root>", "replay", "--kind", "compare", "--id", "<uuid>"]
}
```

That argv is not a shell command. `replay(root, id, cancel)` loads
`witness-request.json` and runs that minimized input on the current workspace
(and a fresh worktree if the request names a commit). This is a current
workspace rerun, not a snapshot of the original process. `outcome_reproduced`
is true only when the retained before and after outputs still match under the
exit policy and the compared source fingerprints are unchanged.
`source_comparisons` records the original fingerprint, the current
fingerprint, and whether each workspace source changed. Missing evidence is
listed explicitly. Reproduction does not open the previous temporary worktree.

Retention keeps at most `max_entries` published directories and at most 8 MiB
total, even when `max_total_bytes` asks for more. Older published entries are
removed first. A directory without a manifest is unpublished and is removed
on the next prune.

## Corpus

`examples/behavior/` has `before.py`, a behavior-preserving `equivalent.py`,
an intentional `different.py`, and `cases.json`. Tests report discovered
differences, incorrect regression claims, executions, elapsed time, and whether
a replay actually runs after an edit. Unsupported and nondeterministic fixtures
are separate cases with those outcomes.
