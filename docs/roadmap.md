# Roadmap

**Status:** working prototype. The milestone lists below are still the
completion gates. A checked box means that gate's evidence is recorded, not
merely that source exists. See [verification](verification.md) and
[implementation](implementation.md). Linux and macOS targets pass on GitHub runners; Windows is supported through WSL2 only.
A local Q4 sample exists and is not the 534-item board. A paired agent study
ran; both arms were correct, and the tool arm only printed `--help`, so an
efficacy benefit is still open.

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

- [ ] Evaluate the selected local checkpoint on Checkweave tasks and simple baselines.
- [ ] Add a managed Python inference worker behind the Rust provider boundary.
- [ ] Probe GPU compatibility and verify CPU fallback; publish a tested platform matrix.
- [ ] Add an optional Jev provider with explicit configuration and version identity.
- [ ] Add typed predicates, batching, and explicit unresolved outcomes.
- [ ] Record model/settings identity and input handling in evidence.
- [ ] Recompute changed items without reprocessing the entire collection.
- [ ] Keep deterministic checks usable without a model or account.
- [ ] Make model/runtime installation automatic, cached, and reproducible.

**Completion evidence:** publish a labeled task sample and measure precision,
recall, unresolved rate, latency, memory, and end-to-end cost where applicable.
Include an existing simple baseline. Processing coverage and judgment quality
must be reported separately. Cover negation, absent evidence, label-order changes,
long inputs, multiple questions, and confident mistakes. A small exploratory
smoke test is not sufficient to qualify a default for release.

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
