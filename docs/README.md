# Checkweave documentation

Start with the overview, then the tutorial. Command details stay in the reference pages. Model research and contributor notes are separate from the first-use guide.

## Start here

| Page | Read it for |
| --- | --- |
| [README](../README.md) | What Checkweave does, how an assistant uses it, and the current release status |
| [Getting started](getting-started.md) | Install from a checkout, check a small JSONL file, and optionally compare two programs |
| [MCP](mcp.md) | Connect a coding assistant and see an example tool call |
| [Platforms](platforms.md) | The operating systems and CPUs covered by the 2026-09-24 build |
| [Roadmap](roadmap.md) | Current capability and the next priorities |

## Reference

| Page | Read it for |
| --- | --- |
| [Usage](usage.md) | Commands, limits, and report fields |
| [Behavior comparison](behavior.md) | Comparing two programs, retained inputs, and compare replay |
| [Execution evidence](execution-evidence.md) | Python traces, snapshots, and trace replay |
| [Semantic collections](semantic-collections.md) | Optional model judgments over text in JSONL |
| [Architecture](architecture.md) | The implemented architecture: how a request runs on your machine |

## Contributor and research references

| Page | Read it for |
| --- | --- |
| [Contributing](../CONTRIBUTING.md) | How to propose a change |
| [Verification](verification.md) | Evidence still required for open acceptance checks |
| [Implementation](implementation.md) | Implementation log |
| [Integration](integration.md) | Historical integration and test notes |
| [Kernel validation](kernel-validation.md) | Kernel checks and their results |
| [Performance](performance.md) | A latency sample and an early paired study |
| [Providers](providers.md) | Local SemIf and the explicit hosted Jev choice |
| [Python worker](python-worker.md) | The managed inference process and measured startup |
| [Model backends](model-backends.md) | Backend choices for local inference |
| [Open-model decision](open-model-decision.md) | Why Qwen3.5-4B is the local default, and which score is which |
| [Semantic validation](semantic-validation.md) | Label samples; not a release qualification |
