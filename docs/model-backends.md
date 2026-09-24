# Model backends

**Status:** backend selection aligned 2026-09-22 with
[open-model default](open-model-decision.md). Inference integration is in
progress. Nothing below is a release qualification.

## Decision

Use a managed **Python inference worker** behind the Rust kernel. The local
open default is **SemIf's direct option-logit readout** on frozen
[`Qwen/Qwen3.5-4B`](https://huggingface.co/Qwen/Qwen3.5-4B) revision
`851bf6e806efd8d0a36b00ddf55e13ccb7b8cd0a` (Apache-2.0). Code pin:
[`TheoLeeCJ/SemIf`](https://github.com/TheoLeeCJ/SemIf) `1f2dea3e25379f9dfc98cb83c324f00ab5deda37`
(MIT). The older `TheoLeeCJ/openjev` URL resolves to that repository.

On a CUDA GPU that can hold the BF16 checkpoint, load it through SemIf's
transformers path. On CPU, use SemIf's llama.cpp backend with
`n_gpu_layers = 0` and the pinned GGUF
`bartowski/Qwen_Qwen3.5-4B-GGUF` revision
`4168f45a16a1290d65a4ec0fa312ae917a4c15d6`, file
`Qwen_Qwen3.5-4B-Q4_K_M.gguf` (3,013,027,808 bytes, Apache-2.0). That CPU
path does not use vLLM. JevBench's published SemIf quality is the BF16 GPU
run, not this Q4 file. CPU Q4 quality and latency on this host are **not
measured** here; the worker measurement is underway separately.

Training and fine-tuning stay outside Checkweave's scope. Offer **TypeSafe
Jev** only as an explicitly configured hosted provider. Never fall back from
local execution to a hosted API.

**GLiNER2.5 base** and **Jeff / GLiFormer-large** are optional research
profiles, not the default. GLiNER2.5 base remains the lightweight profile
when a small encoder is acceptable and a missed subtle judgment is tolerable.
Jeff stays an adapter reference for a Jev-shaped `choice` / `score` / `noul`
server. The 24-case smoke test does not choose the default.

Keep the provider interface independent of this checkpoint. A model should
earn each supported capability through evaluation; a matching response schema
does not establish equivalent behavior.

Keep the provider interface independent of this checkpoint. A classifier can be
useful for routing and narrowing a collection while remaining unsuitable for a
subtle evidence judgment. A model should earn each supported capability through
evaluation; a matching response schema does not establish equivalent behavior.

## Candidates and tradeoffs

These are different task families, not entries in a comparable accuracy ranking.
Published scores use different prompts, datasets, hardware, and calibration.

| Candidate | Relevant capability | Assessment for Checkweave |
| --- | --- | --- |
| [GLiNER2 / GLiNER2.5](https://github.com/fastino-ai/GLiNER2) | Classification plus entities, relations, and structured extraction; 2.5 base is roughly 194M parameters | Optional lightweight profile. Useful for schema extraction and short routing. Weaker than SemIf on JevBench hard and judge tiers. Source spans still need validation. |
| [GLiClass v3 base](https://huggingface.co/knowledgator/gliclass-base-v3.0) | Runtime labels, zero-shot classification, roughly 187M parameters | Useful lightweight baseline; weaker on our initial Checkweave-shaped cases. |
| [GLiFormer through Jeff](https://github.com/logan-markewich/jeff) | Local `choice`, `score`, and `noul` behind a Jev-compatible HTTP interface | Optional adapter reference. v1.3.0 chance-corrected intelligence is 46.9, under the score's halfway line. Hard-tier accuracy is 0.377, close to GLiNER2.5 base and well below SemIf. |
| [Laya](https://github.com/NandhaKishorM/laya) | Native typed decisions, open weights, multilingual variant, roughly 322–421M parameters | Promising specialist option; the base checkpoints' general typed-decision results do not justify making them the default. |
| [Laya-MLX](https://github.com/mizorewww/laya-mlx) | Apple-silicon implementation of Laya inference | An optional platform optimization, not the portable baseline. It inherits the underlying checkpoint's accuracy limits. |
| [SemIf](https://github.com/TheoLeeCJ/SemIf) | Direct option-logit scoring, no generation; frozen Qwen3.5-4B | **Local default.** Strongest practical open row with a documented CPU GGUF path. BF16 does not fit one Tesla M10 (8 GB). See [open-model default](open-model-decision.md). |
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

The chart that prompted the later comparison is JevBench **v1.2.2**
(`/tmp/checkweave-development/jevbench-chart.jpg`). Current scoring is v1.3.0.
Pins and the distinction are in the next section.

## JevBench identity

Primary repository: [fstandhartinger/jevbench](https://github.com/fstandhartinger/jevbench).

The photo is tag **v1.2.2**, commit `e105a48f8cdb7f3babb3594424f73e5d7bdc97b9`
(2026-09-19), artifact `results/v1.2/jevbench-v1.2-results.json`, generated
`2026-09-19T22:49:39+00:00`. It scores 534 decisions with a geometric mean of
intelligence, calibration, speed, and cost at 25% each. That composite is not
accuracy. Rank 18 for the row labeled GLiNER2 is that composite (score 52.9,
intelligence 56.0, calibration 23.7). Official hard-tier accuracy on all 220
items is 0.364 for that row, 0.377 for Jeff, 0.595 for SemIf, and 0.741 for
Jev 1.13.0.

That GLiNER2 row **is** `fastino/gliner2.5-base-v1` with package `gliner2`
2.0.0, recorded in `docs/v1.2-additions.md` and
`results/v1.2/additions/gliner2.json` at the v1.2.2 tag. The chart label does
not say 2.5. The adapter (`jevbench/adapters/gliner2_local.py`) puts
`Question: <instructions>` in front of the state and reads the model's own
single-label softmax (`class_act="softmax"`, `multi_label=True`,
`cls_threshold=0`). It has no instruction field and no native yes/no or
ordinal primitive. Jeff is commit `6f43d3ee32a150889125a62f3c2933bc421e9863`
plus `knowledgator/gliformer-large-v1`, server defaults (torch, fp32,
temperature 3.2), through `jevbench/adapters/typesafe.py` and model alias
`jev-latest`. SemIf is `Qwen/Qwen3.5-4B` in BF16 via
`jevbench/adapters/semif_direct.py` (`semif_phase1.direct.score`). The v1.2.2
system row does not record a Hugging Face commit for that SemIf run; the
implementation pin is the revision in the open-model decision.

Full-set tier accuracy at v1.2.2 (easy 72 / standard 96 / judge 146 / hard 220):

| System | Easy | Standard | Judge | Hard |
| --- | ---: | ---: | ---: | ---: |
| Jev 1.13.0 | 1.000 | 0.990 | 0.945 | 0.741 |
| SemIf Qwen3.5-4B | 1.000 | 0.979 | 0.952 | 0.595 |
| Jeff / GLiFormer-large | 1.000 | 0.760 | 0.616 | 0.377 |
| GLiNER2.5 base | 0.972 | 0.667 | 0.459 | 0.364 |

Hard-family counts for those same frozen runs are in
`results/v1.2/additions/*-per-task.json` and summarized in the open-model
decision. Ordinal items and choice items are different capabilities: Jeff's
own public-tier note reports ordinal 10/12 on the public standard tier and
much weaker adequacy judging. A classification schema match does not make an
encoder an ordinal judge.

**v1.3.0** (`75e6224ed8103bbc3485ca74820a2eaf7ce8abe0`, artifact generated
`2026-09-21T22:55:40+00:00`) keeps the same frozen runs and rescores
intelligence as chance-corrected. Below 50 intelligence the composite is
multiplied by `(intelligence / 50)²`. Headline scores move to SemIf **73.1**,
GLiNER2.5 base **24.0** (intelligence 35.6), and Jeff **54.4** (intelligence
46.9). Use v1.3.0 when citing the current board. Do not repeat the v1.2.2
ranks as current. `classifier.dev` fast was later removed from the open
ranking because it wraps Jev. Smaller and multilingual GLiNER2.5 rows added
after v1.2.2 score below the English base on this English set; they are not
additional defaults. Pins for the v1.2.2 adapters live in
[experiments/backend-selection/pins.json](../experiments/backend-selection/pins.json).

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
profile = "semif"
checkpoint = "Qwen/Qwen3.5-4B"
revision = "851bf6e806efd8d0a36b00ddf55e13ccb7b8cd0a"
cpu_gguf = "bartowski/Qwen_Qwen3.5-4B-GGUF"
cpu_gguf_revision = "4168f45a16a1290d65a4ec0fa312ae917a4c15d6"
cpu_gguf_file = "Qwen_Qwen3.5-4B-Q4_K_M.gguf"
device = "auto"
```

An explicit lightweight profile selects GLiNER2.5 base. It is not the default:

```toml
[model]
provider = "local"
profile = "gliner2"
checkpoint = "fastino/gliner2.5-base-v1"
revision = "1a8bc24e00dc7300b9017c81d63e3dcdabb26596"
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

## Local measurements already on disk

These figures are host measurements. They do not set the default and they are
not a production accuracy claim.

### 24-case smoke

[experiments/backend-selection](../experiments/backend-selection/README.md)
reran the existing 24 synthetic English cases. Expected labels were not edited.
SemIf was **not measured** on this set. GLiFormer was **not measured** on the
Tesla M10 (the isolated environment is CPU-only torch 2.5.1). Same selected
labels for GLiNER2.5 on CPU and CUDA.

| Run | Labels matched | Decisive (expected not `unknown`) | Missing evidence | Warm median / p95 | Peak host RSS |
| --- | --- | --- | --- | --- | --- |
| GLiNER2.5 base, CPU, 4 threads, float32 | 20/24 | 19/19 | 1/5 correct, 4 false decisive | 0.201 s / 0.254 s | 2.40 GB |
| GLiNER2.5 base, Tesla M10 `cuda:0`, float32 | 20/24 | 19/19 | 1/5 correct, 4 false decisive | 0.061 s / 0.063 s | 1.32 GB host; 1.59 GB CUDA allocated |
| Jeff-style GLiFormer-large, CPU, 4 threads, float32, eager attention | 16/24 | 13/19 | 3/5 correct, 2 false decisive | 0.571 s / 0.720 s | 3.60 GB |

GLiNER2.5 label order agreed on 24/24; a second question in the same schema
changed 3/24 labels. GLiFormer label order agreed on 21/24, and a shared
encoder pass changed 4/24 labels. Jeff's default isolates only `noul`
questions, so choice questions can interfere. One obvious ordinal probe
(`all_users`) was selected in both label orders by both models and is excluded
from the 24. The official JevBench text packing for GLiNER2.5 also matched
20/24 on this smoke set; that is the same four missing-evidence misses, not a
second benchmark. The GLiNER2.5 process loaded both the classifier and the
extractor, so its RSS is higher than the single-worker figure below.

The earlier GLiClass comparison (15/24 on the same cases) remains in
[experiments/model-backends](../experiments/model-backends/README.md).

### Larger GLiNER2.5 sample

The independent 75-case sample is recorded in
[semantic validation](semantic-validation.md): 64 supported label questions,
50/64 (0.781) on both CPU and CUDA, with the same labels on all 81 rows.
Absent evidence is still a blocker there (three of eight `unestablished`
items received a definite label). That sample is the GLiNER lightweight
profile only. It was not used to pick SemIf.

### Not measured in this note

- SemIf BF16 or Q4_K_M on these fixtures, on this Xeon, or on the Tesla M10.
- GLiFormer on CUDA. Official Jeff GPU numbers are a Modal L4, not this host.
- A portable GPU/CPU matrix. The only accelerator timed here is a Tesla M10
  (Maxwell, compute capability 5.0, 8,515,158,016 bytes reported by PyTorch).
  BF16 Qwen3.5-4B does not fit that GPU. See the open-model decision.
