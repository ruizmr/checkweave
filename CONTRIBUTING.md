# Contributing

Checkweave is a working prototype. Start with the [README](README.md),
[usage](docs/usage.md), [architecture](docs/architecture.md), and
[roadmap](docs/roadmap.md).

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

Build and test from a lockfile:

```sh
cargo test --locked
cargo build --locked --release
```

The [backend experiment](experiments/model-backends/README.md) is a small
inference comparison, not the release matrix. For the core, prioritize
correctness across edits, interrupted runs, missed watcher events, restarts,
and concurrent clients. Do not describe macOS, Windows, or native CUDA as
verified unless you ran them and recorded the result. The local Q4 sample is
not the BF16 JevBench score. djev is not an implemented profile.

Do not commit local evidence caches, model weights, credentials, or reproduction
inputs taken from private projects. Use small synthetic fixtures when possible.
