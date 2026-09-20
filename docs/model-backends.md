# Model backends

**Status:** backend research and proposed integration, reviewed 2026-09-20.
Checkweave does not yet implement inference or the configuration shown below.

## Decision

Use a managed **Python / PyTorch inference worker** behind the Rust kernel.
Local open weights are the default; use a compatible GPU when available and
fall back to CPU. Training and fine-tuning are outside Checkweave's scope.
Offer **TypeSafe Jev** as an explicitly configured hosted provider.

Select **GLiNER2.5 base** as the starting local backend for implementation:
[`fastino/gliner2.5-base-v1`](https://huggingface.co/fastino/gliner2.5-base-v1),
about 194 million parameters with Apache-2.0 weights. It supports runtime-defined
classification labels and structured extraction without generating text. Use the
GLiNER2 package's `AutoExtractor` / `Classifier` interfaces for this checkpoint;
the legacy `GLiNER2` class loads the older span architecture.

This is an initial engineering choice, pending release qualification. Our small
local comparison favored it over GLiClass, but exposed failures on missing
evidence. The base checkpoint is English; hardware portability does not imply
multilingual accuracy. Release selection must pass the task evaluation below.

Keep the provider interface independent of this checkpoint. A classifier can be
useful for routing and narrowing a collection while remaining unsuitable for a
subtle evidence judgment. A model should earn each supported capability through
evaluation; a matching response schema does not establish equivalent behavior.

## Candidates and tradeoffs

These are different task families, not entries in a comparable accuracy ranking.
Published scores use different prompts, datasets, hardware, and calibration.

| Candidate | Relevant capability | Assessment for Checkweave |
| --- | --- | --- |
| [GLiNER2 / GLiNER2.5](https://github.com/fastino-ai/GLiNER2) | Classification plus entities, relations, and structured extraction; 2.5 base is roughly 194M parameters | Selected starting backend. Source extraction is useful for evidence retrieval, but extracted spans still need validation and cannot automatically justify a classification. |
| [GLiClass v3 base](https://huggingface.co/knowledgator/gliclass-base-v3.0) | Runtime labels, zero-shot classification, roughly 187M parameters | Useful lightweight baseline; weaker on our initial Checkweave-shaped cases. |
| [GLiFormer through Jeff](https://github.com/logan-markewich/jeff) | Local `choice`, `score`, and `noul` behind a Jev-compatible HTTP interface | Useful adapter reference. Its own evaluation reports weaker reasoning results than Jev; the service and larger default model add overhead. |
| [Laya](https://github.com/NandhaKishorM/laya) | Native typed decisions, open weights, multilingual variant, roughly 322–421M parameters | Promising specialist option; the base checkpoints' general typed-decision results do not justify making them the default. |
| [Laya-MLX](https://github.com/mizorewww/laya-mlx) | Apple-silicon implementation of Laya inference | An optional platform optimization, not the portable baseline. It inherits the underlying checkpoint's accuracy limits. |
| [SemIf](https://github.com/TheoLeeCJ/SemIf) | Direct decision scoring using language-model logits; Qwen3.5-4B option | Candidate for a more capable local profile if small encoders fail our tasks; substantially heavier CPU and memory requirements. |
| [Jev](https://docs.typesafe.ai/models) | Hosted typed decisions and larger text context | Optional provider for users who choose hosted inference; never an automatic fallback from local execution. |

### Why not simply default to Laya?

The upstream [evaluation and limitations](https://github.com/NandhaKishorM/laya/blob/42626c348753fbb17572a813127df2278a1ec527/README.md#honest-limits)
report base-model accuracies of 0.362 and 0.342 on typed-decisions, below a
0.461 majority-class baseline. The 0.766 result belongs to a checkpoint
fine-tuned on that benchmark's own training split. That is useful specialist
evidence, but it does not establish general zero-shot decision quality.

The English base configuration also divides a 512-token budget between question
and state. Short latency numbers do not show that a whole source file or trace
was evaluated. Treat context handling and confident errors as first-class
selection criteria.

### Earlier work

The public research predates Jev's September 2026 launch:

- [GLiClass](https://github.com/Knowledgator/GLiClass) has a public repository
  created in June 2024 and an [August 2025 paper](https://arxiv.org/abs/2508.07662)
  describing lightweight generalist classification.
- [GLiNER2's July 2025 paper](https://arxiv.org/abs/2507.18546) describes one
  small encoder for schema-driven extraction and classification, including CPU
  deployment. The newer 2.5 checkpoint should not be backdated to that paper.
- Laya author Nandakishor M published
  [SalesRLAgent](https://arxiv.org/abs/2503.23303) in March 2025 and
  [confidence-aware routing](https://arxiv.org/abs/2510.01237) in September 2025.
  These are related earlier research, not evidence that either implements the
  same model as proprietary Jev. The current Laya repository appeared in
  September 2026 after Jev's launch.

The precise X posts that prompted this investigation were not verified; the
papers and repositories above are the primary-source record.

## Provider contract

Keep Checkweave's request types independent of a vendor's wire protocol:

```text
evaluate(state, questions, limits) -> typed results + provenance
question = predicate | choice | ordinal rubric
```

The adapter declares which types it supports, applicable language/domain limits,
maximum input and label budgets, batching behavior, and score semantics.
Checkweave validates schemas before inference and retains original source handles.

- **Predicate:** distinguish true, false, and insufficient evidence where the
  adapter has been evaluated for that distinction. A low binary score alone does
  not mean the evidence is missing. Otherwise mark this capability unsupported.
- **Choice:** return the selected label and available per-label scores. An
  exclusive softmax distribution and independent sigmoid scores are different
  things; preserve that distinction.
- **Ordinal rubric:** preserve the named levels and their order. Only derive an
  expected numeric level when the adapter returns an appropriate distribution;
  record the mapping and mark it as an adapter-derived score.

Model scores are not automatically calibrated probabilities of correctness.
Thresholds must be evaluated per model, task family, and rubric. Abstention policy
is separate from provider-reported confidence. Even a confident judgment remains
a model judgment, with processing coverage reported independently.

Never silently discard input to satisfy a model's context limit. Count the full
serialized request, including labels and instructions. Reject over-limit requests
or use a declared chunking/reduction policy with evaluated and omitted spans
reported. Chunk-wise classification does not imply document-wide reasoning.

## Hardware and lifecycle

The intended release targets are Linux, macOS, and Windows where the selected
Python/PyTorch runtime has supported wheels. This is a support target, not a
tested release matrix yet.

1. `checkweave init` configures the workspace and agent integration. Ordinary
   deterministic operations require no Python environment or model download.
2. The first semantic request prepares a versioned, user-level inference
   environment and pinned checkpoint with visible download progress. Reuse these
   files across workspaces; offer an offline preinstallation path.
3. Start a supervised worker over framed standard I/O. Keep protocol output
   separate from diagnostic logs and reuse loaded weights across batches.
4. In `auto` mode, probe supported CUDA or ROCm acceleration, Apple MPS, then CPU
   as applicable. A representative forward pass must succeed before selection.
   PyTorch exposes supported ROCm devices through its CUDA device API.
5. On an unavailable accelerator, unsupported operation, or GPU memory failure,
   apply a bounded local retry/fallback policy. Record the device and precision
   actually used. Explicit device requests may fail instead of silently changing.
6. Bound threads, memory, batch size, queue length, and idle lifetime. Surface a
   concrete local failure if CPU execution also cannot satisfy the budget.

Use conservative precision first. Add reduced precision, quantization, ONNX, or
MLX only after checking output changes and end-to-end benefit on the target
hardware. Package acceleration dependencies per platform; an NVIDIA runtime
must not be required on CPU-only machines.

Pin checkpoint and tokenizer revisions, adapter version, serialization, library
environment, precision, and relevant settings. Include them in cache identity.
A changed model or execution policy can invalidate a cached judgment just as a
changed input does. Avoid mutable model aliases as reproducibility identifiers.

## Optional Jev integration

The current [API](https://docs.typesafe.ai/api) accepts typed questions at
`POST https://api.typesafe.ai/v1/systemone` with bearer authentication. Map
Checkweave's contract to its `noul`, `choice`, and `score` types in one adapter.
Keep the returned concrete model ID and provider-specific scores in provenance.

Illustrative configuration, not an implemented parser:

```toml
[model]
provider = "local"
checkpoint = "fastino/gliner2.5-base-v1"
device = "auto"
```

Opting into hosted inference changes the provider explicitly:

```toml
[model]
provider = "jev"
model = "jev-1.13.0"
api_key_env = "TYPESAFE_API_KEY"
```

The [current model documentation](https://docs.typesafe.ai/models) lists that
version; verify availability during implementation. Credentials belong outside
the repository. Request limits, retries, cancellation, and usage accounting belong
in the adapter. Respect `Retry-After` and keep retries within the caller's budget.
Never infer that a provider switch preserves classification thresholds.

## Evaluation required before release

Publish a labeled Checkweave task set covering collection classification,
predicate checks, missing evidence, and ordinal rubrics. Separate straightforward
semantic matching from reasoning-heavy requests. Include a simple lexical or
majority baseline and compare only on identical tasks and held-out examples.

Measure per-family precision/recall, confident errors, unresolved rate, and
calibration where probabilities are exposed. Exercise negation, distracting
instructions inside input data, paraphrases, label order, multiple questions,
long inputs, and unsupported languages. Do not tune on the final evaluation split.

Report cold setup, warm p50/p95 latency, batch throughput, CPU/GPU memory, and
end-to-end collection time. Verify cancellation, accelerator failure, offline
restart, and whether fallback changes outputs. Publish which OS/device/runtime
combinations were actually tested.

If a small local checkpoint fails a capability, narrow that capability or evaluate
a stronger local model. Keep the kernel and provider boundary stable while the
checkpoint improves.

## Initial local measurements

The [reproducible smoke test](../experiments/model-backends/README.md) contains
24 synthetic English cases and pinned results. GLiNER2.5 base matched 20 labels;
GLiClass v3 base matched 15. CPU and CUDA selected the same labels per model.
GLiNER2.5's warm median was approximately 174 ms with four CPU threads and 59 ms
on a Tesla M10, using float32 on this Linux host.

All four GLiNER2.5 errors involved insufficient evidence, including confident
mistakes. These results support the initial implementation choice and expose a
specific release blocker for general evidence judgments. They do not establish
overall accuracy, extraction quality, calibration, or cross-platform support.
Automatic GPU fallback has not been implemented; these were explicit CPU and
CUDA runs. The experiment's library pins are not the proposed production stack.
