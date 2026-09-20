# Roadmap

**Status:** the repository is at the design stage. Milestones below are planned;
only the initial project outline is complete.

The first goal is to establish the experience: initialize once, make a useful
request, edit an input, and receive an updated result with unchanged work reused.

## 0. Project outline

- [x] Define the product and its boundaries.
- [x] Record the Rust runtime, lightweight cache, and Git integration decisions.
- [x] Outline composable operators and automatic maintenance.
- [x] Publish the initial documentation under an open-source license.

## 1. A useful kernel

- [ ] Create the Rust binary and shared request/response types.
- [ ] Implement idempotent workspace initialization and ignored local state.
- [ ] Implement worker startup, locking, reconnection, and idle shutdown.
- [ ] Add fingerprints, dependency tracking, and bounded SQLite result storage.
- [ ] Add native watching, startup reconciliation, and a polling fallback.
- [ ] Expose one deterministic collection check through CLI and MCP.
- [ ] Add one supported agent integration with concise usage instructions.

Start with a structured collection such as JSONL and explicit predicates such as
required fields and value constraints. The first adapter should exercise
enumeration, item-level results, coverage, caching, and useful error reporting.

**Completion evidence:** initialize a fixture workspace, check a collection, edit
a few inputs, and repeat. Results must match a clean evaluation while unchanged
evaluations are reused. Repeat across a stopped worker, an interrupted update,
file creation/deletion, and a Git checkout. Concurrent clients must share work;
linked worktrees must remain distinct.

## 2. Behavior comparison

- [ ] Add a narrowly scoped execution adapter with explicit inputs and outputs.
- [ ] Run before/after targets against shared generated or supplied inputs.
- [ ] Emit concrete differences and reduce a supported failing input.
- [ ] Preserve enough evidence to replay a finding after an edit.
- [ ] Use standard Git reads and temporary worktrees where execution requires them.
- [ ] Reuse the kernel's lifecycle, dependency, budget, and result machinery.

**Completion evidence:** a small public corpus of behavior-preserving changes and
intentional differences. Report discovered differences, incorrect regression
claims, time and executions used, and whether emitted reproductions run. Include
unsupported and nondeterministic cases with explicit outcomes.

## 3. Semantic collection checks

- [ ] Evaluate a native, Python-free decision-model backend.
- [ ] Add typed predicates, batching, and explicit unresolved outcomes.
- [ ] Record model/settings identity and input handling in evidence.
- [ ] Recompute changed items without reprocessing the entire collection.
- [ ] Keep deterministic checks usable without a model or account.

**Completion evidence:** publish a labeled task sample and measure precision,
recall, unresolved rate, latency, memory, and end-to-end cost where applicable.
Include an existing simple baseline. Processing coverage and judgment quality
must be reported separately.

## 4. Execution evidence

- [ ] Select one runtime or test adapter for structured event capture.
- [ ] Tie observations to the source and execution that produced them.
- [ ] Return bounded relevant events and values through the common result surface.
- [ ] Revalidate or replay supported observations when inputs change.

**Completion evidence:** debugging tasks where recorded observations help locate
a failure. Record instrumentation overhead and unsupported scope. Distinguish
observed relationships from inferred explanations.

## 5. Release ergonomics

- [ ] Publish native binaries for the tested platforms.
- [ ] Verify install/init, agent configuration preservation, upgrade, and removal.
- [ ] Test recovery from worker crashes, watcher failures, and cache corruption.
- [ ] Measure idle resource use, cold startup, repeated-query latency, and disk use.
- [ ] Demonstrate a complete agent task with and without Checkweave.

**Completion evidence:** an agent can discover and use the right operation after
initialization, without manual cache management or repeated setup instructions.
Publish the supported-platform and adapter limits alongside the release.

## Scope discipline

Each milestone must produce useful behavior before broadening its adapters.
Keep a general plugin framework, custom workflow language, distributed execution,
and repository snapshot system outside the initial scope. Add finer dependency
tracking and more automation when measured workloads justify them.
