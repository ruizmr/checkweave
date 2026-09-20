# Checkweave

**Composable checks. Current evidence. One local binary.**

Checkweave is a planned Rust utility that gives AI agents a compact way to check
collections, compare behavior, inspect execution evidence, and keep findings
current as a workspace changes. Its composable operators share one local runtime,
dependency tracker, and cache, exposed through MCP and a matching CLI.

The intended experience is simple: initialize a project once, let the agent use
Checkweave, and let Checkweave handle its own upkeep.

> **Status: design stage.** This repository contains the project outline. There
> is no executable, installable release, or implemented MCP server yet. Commands
> and interfaces below describe the intended experience.

## What it should do

| Agent task | Checkweave's job | Useful result |
| --- | --- | --- |
| Check every item in a collection | Enumerate inputs, apply deterministic or semantic predicates, and track coverage | Matching records, unresolved cases, and source references |
| Investigate a behavior change | Run two implementations on shared inputs and reduce a differing case | A small input, before/after outputs, and a runnable reproduction |
| Understand an execution | Capture supported runtime events and retrieve the relevant observations | Values, events, and the source locations available from the adapter |
| Revisit an earlier conclusion | Track its input dependencies and revalidate after changes | A current result or an explicit account of what needs rechecking |

These capabilities compose. A finding can feed another check; a failing input can
be replayed after an edit; a collection scan can reuse results for unchanged items.

## The intended workflow

```sh
checkweave init
```

Initialization should discover the workspace, prepare a small local cache, and
configure the selected supported agent integration. A short instruction block
teaches the agent when to use Checkweave. Existing configuration and instructions
are preserved.

The agent then requests useful operations without constructing an execution graph
by hand. A shared process starts when needed, watches the workspace, refreshes
cheap indexes, and invalidates affected results. Expensive work runs on demand
within a budget. The CLI and MCP should expose equivalent capabilities.

## Design commitments

- **Rust runtime.** Distribute a native executable with predictable resource use.
- **Lightweight state.** Keep fingerprints, dependencies, derived results, and
  selected reproduction evidence in an ignored `.checkweave/` directory.
- **Use Git's existing machinery.** Read committed history from Git and use
  ordinary temporary worktrees when historical execution needs them.
- **Automatic maintenance.** Watch files, reconcile after downtime or missed
  events, and revalidate relevant inputs before presenting a result as current.
- **Composable internals, small agent interface.** Common tasks should fit in a
  single request with a compact, actionable response.
- **Measured scope.** Preserve the difference between an observed failure, a
  model judgment, and a bounded search that found no failure.
- **Optional model backends.** Deterministic operations should work locally
  without an account. Semantic operations add a separately validated backend.
- **Bounded work.** Limit background activity, execution time, retained evidence,
  model calls, and response size.

## Project outline

- [Architecture](docs/architecture.md): runtime boundaries, operator contract,
  automatic updates, cache policy, and agent integration.
- [Roadmap](docs/roadmap.md): milestones and the evidence needed to complete them.
- [Contributing](CONTRIBUTING.md): how to help while the design takes shape.

The first milestone is a small Rust runtime that can perform a useful collection
check, reuse unchanged work, and recover correctly across edits and restarts.
Behavior comparison and richer execution adapters build on that same kernel.

## Inspiration

- [CodeGraph](https://github.com/colbymchenry/codegraph): initialization,
  automatic updates, and compact tools that agents reach for naturally.
- [Jev](https://typesafe.ai/blog/introducing-system-one-models-and-jev): typed
  decisions as useful building blocks inside larger systems.
- [Laya-MLX](https://github.com/mizorewww/laya-mlx): a concrete reference for local
  decision-model inference. A native Checkweave integration remains to be evaluated.

## License

[MIT](LICENSE).
