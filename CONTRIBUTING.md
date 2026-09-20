# Contributing

Checkweave is at the design stage. Start with the [README](README.md),
[architecture](docs/architecture.md), and [roadmap](docs/roadmap.md).

Useful early contributions include:

- A concrete agent task that currently requires repeated manual work.
- A small fixture showing the input, expected result, and supporting evidence.
- A proposal that simplifies setup, automatic maintenance, or an operator contract.
- Measurements of an existing engine or model that could supply a bounded capability.

For implementation proposals, describe the agent-visible behavior, the smallest
adapter that supports it, and how to verify that it works. Preserve the distinction
between planned and implemented capabilities in documentation.

The runtime direction is Rust. Keep workspace state lightweight, use Git's existing
history and worktree mechanisms, and make resource use explicit. Common tasks
should have concise CLI and MCP surfaces backed by the same implementation.
Python is appropriate for the managed local inference worker. Keep model-specific
dependencies outside the Rust kernel and preserve CPU inference as a fallback.

There are no runtime build or test commands yet. The
[backend experiment](experiments/model-backends/README.md) documents a small
inference comparison. Add runtime commands and meaningful checks with the first
implementation. For the core, prioritize correctness across
edits, interrupted runs, missed watcher events, restarts, and concurrent clients.

Do not commit local evidence caches, model weights, credentials, or reproduction
inputs taken from private projects. Use small synthetic fixtures when possible.
