# Checkweave

**Local-first code intelligence for safer code changes.**

Checkweave helps a developer or a coding assistant check how code behaves, using concrete evidence from your own machine. You point it at a before-and-after change, a Python run, or a data file, and it returns a specific result you can read, keep, or turn into a test.

## What you can do

| You want to | What you get | Why it helps |
| --- | --- | --- |
| Compare code before and after a change | An input where the versions produce different outputs | Catch unintended changes and turn the case into a regression test |
| Inspect a Python run | Recorded events, source lines, and supported local values | Follow a failing run to the code that needs attention |
| Check JSONL fixtures or config exports | Records that match a rule, with counts and source locations | Find missing fields or invalid values across a collection |
| Ask questions about text | Optional model judgments tied to individual records | Flag bug reports that may lack reproduction steps, for example |

JSONL means one JSON record per line. The first three checks need no account and no model.

Checkweave adds a concrete case, match, or line to the tests and review you already do.

## From a coding assistant

MCP (Model Context Protocol) lets your coding assistant call Checkweave tools. You ask a question in the chat, the assistant calls Checkweave, Checkweave returns the evidence, and the assistant explains the result or proposes a fix.

> Which records in `notes.jsonl` are missing an owner? Call Checkweave and tell me what it found.

How to connect an editor, and more prompts, are in the [MCP guide](docs/mcp.md).

## On your machine

Checkweave runs checks and stores their evidence on your machine. Its optional
semantic checks use a local model by default; first use may download the runtime
and model weights. Hosted inference requires explicit configuration, and a local
failure never switches to it. Your coding assistant may still send returned
evidence to its own AI provider.

## Install

Build from this checkout. You need Rust 1.88 or newer. On Windows, use WSL and a Linux checkout.

```sh
cargo install --path . --locked
export PATH="$HOME/.cargo/bin:$PATH"
cd /path/to/your/project
checkweave init
```

`init` configures **Cursor** for this project. Other MCP clients can be
[connected manually](docs/mcp.md). The first check needs no account and no model.
The [getting started](docs/getting-started.md) tutorial runs it in a temporary folder.

## Release status

On 2026-09-24, the release dry run passed on Linux, macOS, and Windows through
WSL2. A tagged release has not yet been published. See the
[platform matrix](docs/platforms.md) for exactly what was tested.

Today, collection checks read JSONL, comparisons run programs that accept and
return JSON, and tracing supports Python. Model judgments remain experimental.

Every page is listed in the [documentation index](docs/README.md). What works now, and what comes next, is on the [roadmap](docs/roadmap.md).

## Inspiration

- [CodeGraph](https://github.com/colbymchenry/codegraph): compact local tools an assistant can call.
- [Jev](https://typesafe.ai/blog/introducing-system-one-models-and-jev): typed decisions as building blocks.
- [Laya-MLX](https://github.com/mizorewww/laya-mlx): a reference for local decision-model inference on Apple silicon.

## License

[MIT](LICENSE).
