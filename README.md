# Checkweave

**Composable checks. Current evidence. One local runtime.**

Checkweave is a local Rust runtime for AI agents. It checks JSON Lines collections, compares two programs on shared inputs, records Python execution events, and can ask a configured decision model. CLI and MCP use the same requests. Derived state lives in an ignored `.checkweave/` directory.

> **Status: working prototype.** The binary, worker, and MCP server are in this tree. No GitHub Release is published. Build from source until a tag exists. Release targets are Linux (x86_64, aarch64) and macOS (Intel, Apple silicon); both Linux targets pass on GitHub runners, and macOS fixes await a passing run. Windows runs the Linux build through WSL2 via `install.ps1`; native Windows is not supported. The local default profile is SemIf on pinned Qwen3.5-4B. A Q4 CPU sample scored 63/64 labels; that is not the BF16 JevBench score. `profile = "lightweight"` is GLiNER2.5 base (50/64 on the same labels). Jev is opt-in. djev is not a profile. Details: [implementation](docs/implementation.md) and [verification](docs/verification.md).

## Quickstart

```sh
cargo build --locked --release
cargo run --locked --release -- --workspace . init --agent none
```

```sh
printf '%s\n' '{"n":1}' '{"n":2}' > items.jsonl
cargo run --locked --release -- --workspace . check \
  --include 'items.jsonl' \
  --predicate '{"op":"gt","path":"/n","value":1}'
```

`init --agent cursor` (the default) adds a managed Cursor MCP entry and rule without replacing other servers. Commands, install, and uninstall: [usage](docs/usage.md).

Deterministic checks need no model. Absent `[model]` selects the SemIf default. `model setup` installs the Python environment only; it does not leave a warm model.

```sh
checkweave model setup
checkweave model setup --offline
```

Hosted Jev runs only when `provider = "jev"` is set. A local failure does not switch to it.

## Commands

| Command | Role |
| --- | --- |
| `init --agent cursor\|none` | Workspace state and optional Cursor integration |
| `check` | JSONL predicate check |
| `evidence ID` | Collection store, then trace, then semantic evidence |
| `status` / `shutdown` | Worker status / ask it to exit |
| `deinit` | Remove managed Cursor MCP and rule entries only |
| `compare` / `replay` | Before/after runs, or replay compare or trace evidence |
| `semantic` | JSONL text judged by the configured model |
| `trace` | Python line events for one explicit script |
| `model setup` / `model evaluate` | Cache the configured model, or score typed questions |
| `run start` / `run status` / `run cancel` | Handle for the same operations |
| `mcp` | Stdio MCP server |

`daemon` is hidden. Full flags and JSON examples: [usage](docs/usage.md).

## What it does

| Agent task | Checkweave's job | Useful result |
| --- | --- | --- |
| Check every item in a collection | Enumerate inputs, apply deterministic or semantic predicates, and track coverage | Matching records, unresolved cases, and source references |
| Investigate a behavior change | Run two implementations on shared inputs and reduce a differing case | A small input, before/after outputs, and a runnable reproduction |
| Understand an execution | Capture supported runtime events and retrieve the relevant observations | Values, events, and the source locations available from the adapter |
| Revisit an earlier conclusion | Track its input dependencies and revalidate after changes | A current result or an explicit account of what needs rechecking |

These capabilities compose. A finding can feed another check; a failing input can be replayed after an edit; a collection scan can reuse results for unchanged items. `semantic` is the collection judgment command. `model evaluate` scores states you pass in directly.

## Design commitments

- **Rust runtime.** Distribute a native executable with predictable resource use. Manage a separate Python inference worker when semantic checks need a model.
- **Lightweight state.** Keep fingerprints, dependencies, derived results, and selected reproduction evidence in an ignored `.checkweave/` directory.
- **Use Git's existing machinery.** Read committed history from Git and use ordinary temporary worktrees when historical execution needs them.
- **Automatic maintenance.** Watch files, reconcile after downtime or missed events, and revalidate relevant inputs before presenting a result as current.
- **Composable internals, small agent interface.** Common tasks should fit in a single request with a compact, actionable response.
- **Measured scope.** Preserve the difference between an observed failure, a model judgment, and a bounded search that found no failure.
- **Local inference by default.** Profile `default` is SemIf on pinned Qwen3.5-4B. The local Q4 sample is 63/64 labels, separate from the BF16 board. Profile `lightweight` is GLiNER2.5 base. Jev is an explicitly configured hosted provider, never a silent fallback. djev is not implemented. Deterministic checks need no model.
- **Bounded work.** Limit background activity, execution time, retained evidence, model calls, and response size.

## Project outline

- [Usage](docs/usage.md): commands, install, and the Cursor files `init` writes.
- [Platforms](docs/platforms.md): release runners and what has actually been executed.
- [Architecture](docs/architecture.md): runtime boundaries, operator contract, automatic updates, cache policy, and agent integration.
- [Roadmap](docs/roadmap.md): milestones and the evidence needed to complete them.
- [Verification](docs/verification.md): required evidence and the runs recorded so far.
- [Behavior](docs/behavior.md), [execution evidence](docs/execution-evidence.md), [providers](docs/providers.md), [semantic collections](docs/semantic-collections.md).
- [Open-model decision](docs/open-model-decision.md): why Qwen3.5-4B is the local default, and which scores are BF16 versus this host's Q4 sample.
- [Contributing](CONTRIBUTING.md).

## Inspiration

- [CodeGraph](https://github.com/colbymchenry/codegraph): initialization, automatic updates, and compact tools that agents reach for naturally.
- [Jev](https://typesafe.ai/blog/introducing-system-one-models-and-jev): typed decisions as useful building blocks inside larger systems.
- [Laya-MLX](https://github.com/mizorewww/laya-mlx): a concrete reference for local decision-model inference on Apple silicon.

## License

[MIT](LICENSE).
