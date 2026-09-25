# Roadmap

## Position after the first cross-platform build

Checkweave runs on your machine today. You can check JSON Lines with an explicit predicate, compare two commands, trace a Python script, and optionally ask a configured model about text in those records. The command-line interface and the editor connection use the same workspace daemon. How the pieces fit is in [architecture](architecture.md).

On 2026-09-24, [GitHub Actions run 36070429830](https://github.com/ruizmr/checkweave/actions/runs/36070429830) built, tested, and packaged Linux x86_64, Linux ARM64, macOS Intel, and macOS Apple silicon. The Windows installer smoke passed with Ubuntu 24.04 on a Windows runner. Publish was skipped. There is no GitHub Release and no tag. Platform limits are in [platforms](platforms.md). What that run establishes is in [verification](verification.md).

Those builds and their tests passed. Model judgments are a separate qualification. An assistant benefit is a separate measurement. The fresh three-arm study in [performance](performance.md) solved three tasks per arm and exercised the operations. It uncovered and verified a fix for missing evidence in text-only MCP clients; it did not show a time or token advantage.

## Usable today

| Capability | Usable today | Evidence |
| --- | --- | --- |
| JSON Lines check | Explicit predicates, row reports, coverage, freshness, and item caching, with no model required. | [tests/collection.rs](../tests/collection.rs), [tests/adversarial.rs](../tests/adversarial.rs); [check reference](usage.md#check) |
| Compare and replay | Two commands, supplied or generated JSON, stable differences, optional reduction, a retained reproduction, and a committed revision in a temporary worktree. Replay uses the retained input. | [tests/compare.rs](../tests/compare.rs), [examples/behavior](../examples/behavior/); [behavior](behavior.md) |
| Python trace and replay | Call, line, return, and exception events with source lines. Replay uses the retained snapshot. | [tests/trace.rs](../tests/trace.rs); [execution-evidence](execution-evidence.md) |
| Shared worker and setup | One daemon per worktree, idle exit, distinct linked worktrees, and Cursor `init` that keeps unrelated servers. | [tests/daemon.rs](../tests/daemon.rs), [tests/workspace.rs](../tests/workspace.rs); [usage](usage.md) |
| Semantic check | Optional model judgments over selected text. Deterministic checks stay available without a model. | [semantic-collections](semantic-collections.md), [semantic-validation](semantic-validation.md) |
| Cross-platform build | The dry-run archives above. A published download is still ahead. | [verification](verification.md), [platforms](platforms.md) |

A test name records the scenario it ran. It does not close every clause of an original gate. The gate list, the mapping, and what is still open are in [verification](verification.md).

## Progress on 2026-09-25

- Added [runnable recipes](recipes.md) for data checks, refactor comparison, and debugging.
- Reworked task guidance and fixed MCP evidence delivery for clients that expose only text.
- Added five recovery tests and documented the boundary around external dependencies and interpreters.
- Added archive install/upgrade/removal smoke to all native jobs and the WSL installer flow.
- Recorded [release-profile measurements and fresh Cursor studies](performance.md). The study supports discovery and compatibility, not a general cost advantage.

## Priorities

### 1. First public release and a straightforward first session

Publish a tagged build through the tested release workflow. The first session should be obvious: install, `init`, check one collection, edit a row, and read the new result. Setup time and limits belong in that path before someone hits them. Install steps live in [getting started](getting-started.md).

Acceptance:

- A tag and a GitHub Release with the four native archives, and the WSL installer path the dry run already smoked.
- Install, upgrade, and removal of those published assets, with unrelated editor configuration left in place.
- A first session a new user can finish from the docs: one check, one edit, and the updated result.

**Done when:** those three are true for the published artifacts, on the platforms the release claims.

### 2. Make the tools useful in everyday assistant work

Make the tool descriptions and editor guidance help an assistant choose the
right check during a real debugging or review task. Then compare the same tasks
with Checkweave and without it. Include tasks described in ordinary language,
so the study tests discovery as well as explicitly requested tool use.

Acceptance:

- Runnable recipes for debugging, checking a refactor, and validating project data.
- Recorded tool use for both guided prompts and ordinary task descriptions.
- Both arms scored for correctness and missed bugs.
- Time and tokens recorded for both arms.

**Done when:** the assistant uses the operations and the results show where
Checkweave improves correctness or reduces effort, and where it does not. Use
those findings to revise the integration before adding more adapters. Recipes and the baseline/discovery/guided sample are now recorded in [performance](performance.md). All tasks passed, but the treatment arms used more time and tokens. Reducing evidence volume and testing larger tasks remain open.

### 3. Harden everyday use and measure the released artifacts

Keep the tests that already pass, including the preserving and differing compare cases in [examples/behavior](../examples/behavior/) and [tests/compare.rs](../tests/compare.rs). Add the recovery cases that still lack a recorded end-to-end result. Measure the published binaries.

Acceptance:

- Existing collection, compare, trace, daemon, adversarial, and workspace tests stay in the suite.
- Recorded runs for a Git checkout of the checked files, a missed watch event, and environment regressions the current test names do not cover.
- Idle CPU and memory, cold startup, repeated-query latency, and disk use for a
  release build, with its exact version recorded.
- Debugging tasks that assess whether traces locate failures, with measured
  instrumentation overhead and explicit unsupported cases.

**Done when:** those runs and figures exist, and the suites above still pass. Earlier debug timings stay historical. Details are in [verification](verification.md).

### 4. Qualify model usefulness and hardware

Deterministic checks stay usable with no model. Quality and device support are their own gate. Pins, privacy, and setup stay in [providers](providers.md), [platforms](platforms.md), and [semantic validation](semantic-validation.md).

Acceptance:

- An independent labeled sample and a simple baseline.
- Precision, recall, unresolved rate, confident errors, negation, absent evidence, label-order changes, long inputs, and multiple questions.
- Latency, memory, and end-to-end cost where a cost exists.
- A real accelerator probe and CPU fallback, recorded only for devices that were run.

**Done when:** those reports exist. The held-out Q4 sample (63/64 supported labels, 11/12 predicate gold) stays a sample, and `release_qualified` stays false, until then.

### Later: broader adapters, from evidence

Another language, test runner, or source index waits on use from the priorities above.

**Done when:** a recorded gap in the current operations shows that the adapter is the next step.

## Where the original gates stand

[Verification](verification.md) keeps the original requirement list and the historical metrics:

| Area | Standing |
| --- | --- |
| Runtime, initialization, shared worker, collection, cache, limits, compare, Git reads, trace | Named tests ran in the release workflow. Mapping is in [verification](verification.md). |
| Checkout and missed watch events | Five recovery tests cover live checkout, watcherless validation, restart after missed changes, and undeclared environment changes. Native event-loss injection in a live worker remains open. |
| Semantic quality and devices | The Q4 sample is recorded. Qualification is priority 4. |
| Assistant benefit | Actual check/compare/trace use is recorded, including ordinary-language discovery. All arms passed; no cost or correctness advantage yet. Priority 2. |
| Published install, upgrade, removal, and release performance | Archive lifecycle smoke is automated; local release-profile idle, latency, disk, and trace figures are recorded. Published assets and representative application measurements remain open. |

## Out of scope

Distributed execution, a plugin ABI, a custom workflow language, a general operator graph, and a repository snapshot store stay out. Built-in operations are the ones in [architecture](architecture.md).
