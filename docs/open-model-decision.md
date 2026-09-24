# Open-model default

**Status:** source decision, 2026-09-22. SemIf on pinned Qwen3.5-4B is the
local default. GLiNER2.5 base is the optional lightweight profile. Jev stays
an explicit hosted provider. djev is a future research candidate, not an
implemented profile and not a config key. A local Q4 CPU sample scored 63/64
supported labels and 11/12 predicate-gold items
([semantic validation](semantic-validation.md)); that is not the BF16
JevBench score. This note selects the local default.

## Default

Use **SemIf's direct option-logit readout** on frozen **`Qwen/Qwen3.5-4B`**
revision **`851bf6e806efd8d0a36b00ddf55e13ccb7b8cd0a`** (Apache-2.0 weights).
Code pin: [`TheoLeeCJ/SemIf`](https://github.com/TheoLeeCJ/SemIf) `master`
`1f2dea3e25379f9dfc98cb83c324f00ab5deda37` (MIT). The old `TheoLeeCJ/openjev`
URL resolves to this repository. No Checkweave training or fine-tune.

On a CUDA GPU that can hold the BF16 checkpoint, load the text config through
SemIf's transformers path (`Qwen3_5ForCausalLM`, one visible GPU). On CPU, use
SemIf's llama.cpp backend with `n_gpu_layers = 0` and the pinned GGUF
`bartowski/Qwen_Qwen3.5-4B-GGUF` revision
`4168f45a16a1290d65a4ec0fa312ae917a4c15d6`, file
`Qwen_Qwen3.5-4B-Q4_K_M.gguf` (`3,013,027,808` bytes, Apache-2.0). That CPU
path does not use vLLM.

This is the strongest **practical** open choice on the current JevBench
measurements: chance-corrected intelligence 79.0 on all 534 frozen decisions,
with a documented CPU fallback. It is not a claim that the four-axis JevBench
Score crowns a universal winner.

### Optional profiles

| Profile | When it is the right process | Checkpoint |
| --- | --- | --- |
| **Default** | GPU when the BF16 weights fit; otherwise CPU llama.cpp | `Qwen/Qwen3.5-4B` @ `851bf6e8…` |
| **Accurate** | Not implemented. A future research candidate for a modern CUDA 13 GPU with room for ~52 GB BF16 weights plus KV cache. No CPU fallback and no Checkweave profile key. | djev one-step read on `google/diffusiongemma-26B-A4B-it` @ `f7f5b7f5fa82ffc52addd066915886d497f5517b` |
| **Lightweight** | CPU routing and schema extraction where a missed subtle judgment is acceptable | `fastino/gliner2.5-base-v1` (GLiNER2.5 base, 194M, Apache-2.0) |

Jeff (GLiFormer-large, ~400M) stays off the default. Its chance-corrected
intelligence is 46.9, under the score's halfway line, and hard-tier accuracy
is 0.377.

## What was scored

Primary board: [JevBench](https://github.com/fstandhartinger/jevbench) `main`
`75e6224ed8103bbc3485ca74820a2eaf7ce8abe0`, artifact
`results/v1.2/jevbench-v1.2-results.json`, `revision` **v1.3.0**, generated
`2026-09-21T22:55:40+00:00`.

Intelligence is chance-corrected per tier:
`100 * (accuracy - chance) / (1 - chance)`, clipped at 0, then weighted
hard 30% / easy 14% / standard 28% / judge 28%. Below 50 intelligence the
composite is multiplied by `(intelligence / 50)²`. Calibration, speed, and
cost stay one quarter each. The task set did not change from v1.2.

The supplied chart (`/tmp/checkweave-development/jevbench-chart.jpg`) is
**v1.2.2**, before that chance correction. Its headline scores (SemIf 74.6,
djev 74.3, GLiNER2 52.9, Jeff 66.9) are the older composite. v1.3.0 rescores
the same frozen runs: SemIf **73.1**, djev **73.0**, GLiNER2.5 base **24.0**,
Jeff **54.4**. The accuracy columns below are the measurements; the v1.2.2
bars are not.

`classifier.dev` fast is an honorable mention, not an open model. The API
identifies `jev-1.13.0`.

Closed Jev 1.13.0 remains the hosted reference (intelligence 85.7, hard
accuracy 0.741). It is an explicit provider, not the local default.

## Measured open comparison

All rows are the full 534 decisions, including 220 hard items. Intelligence,
calibration, and tier accuracy are the quality evidence. Speed and cost move
the composite and are a poor reason to pick a local default: several "cheap"
rows are hosted-price estimates, and self-hosted latency is adjusted by
×2 + 0.15 s, which JevBench marks as an assumption.

| System | Intel. | Cal. | Hard acc. | Easy / std / judge | ECE (hard) | Where measured |
| --- | ---: | ---: | ---: | --- | ---: | --- |
| SemIf Qwen3.5-4B BF16 | 79.0 | 72.6 | 0.595 | 1.000 / 0.979 / 0.952 | 0.121 | RunPod RTX PRO 4500, BF16, network from Germany |
| djev one-step DiffusionGemma | 82.7 | 65.4 | 0.695 | 1.000 / 0.979 / 0.932 | 0.175 | Hosted `api.djev.dev`, free preview |
| reflex 4B (LoRA on Qwen3.5-4B) | 80.1 | 75.2 | 0.632 | 1.000 / 0.948 / 0.973 | 0.105 | RunPod H100; public items used as a dev gate |
| Winnow-12B Q8 | 82.0 | 72.0 | 0.709 | 1.000 / 0.969 / 0.911 | 0.120 | RTX 4090, full GPU offload; private train corpus |
| Qwen3.8-27B direct logit (SimpleJev demo) | 84.7 | 81.1 | 0.750 | 1.000 / 0.969 / 0.932 | 0.061 | Public demo, not a local pin |
| decider-2B | 61.2 | 46.6 | 0.473 | 1.000 / 0.854 / 0.774 | 0.322 | RunPod H100 |
| Open-Jev 2B (Zefan Cai) | 61.0 | 55.1 | 0.427 | 1.000 / 0.792 / 0.884 | 0.257 | RunPod H100 |
| Jeff / GLiFormer-large | 46.9 | 64.6 | 0.377 | 1.000 / 0.760 / 0.616 | 0.185 | CPU, Ryzen 5 3600, 4 threads |
| GLiNER2.5 base (`gliner2` row) | 35.6 | 23.7 | 0.364 | 0.972 / 0.667 / 0.459 | 0.472 | CPU, same Ryzen, 4 threads |
| GLiNER2.5 multi (287M) | 27.7 | 56.1 | 0.377 | 0.903 / 0.510 / 0.438 | 0.265 | CPU; English items do not use the multilingual train |
| GLiNER2.5 small (74M) | 25.6 | 47.2 | 0.332 | 0.833 / 0.479 / 0.500 | 0.326 | CPU |
| GLiNER2 large (older family) | 40.1 | 24.3 | 0.364 | 0.986 / 0.625 / 0.610 | 0.466 | CPU |

The benchmark's GLiNER2 row is **gliner2.5-base** (DeBERTa-v3-base, 194M,
GLiNER2.5 boundary architecture), not the July 2025 paper's older span model.
GLiNER2 large is a separate earlier checkpoint.

Hard-tier families that matter for evidence checks, accuracy (n):

| Family (n) | SemIf 4B | djev | GLiNER2.5 base | Jeff |
| --- | ---: | ---: | ---: | ---: |
| multi_hop (35) | 0.629 | 0.829 | 0.371 | 0.486 |
| long_policy (38) | 0.421 | 0.474 | 0.263 | 0.132 |
| judge_hard (33) | 0.788 | 0.879 | 0.455 | 0.515 |
| ambiguous (14) | 0.571 | 0.714 | 0.357 | 0.000 |
| temporal_numeric (30) | 0.200 | 0.333 | 0.200 | 0.333 |
| probability (20) | 0.400 | 0.600 | 0.300 | 0.350 |
| trap (16) | 1.000 | 0.938 | 0.438 | 0.688 |
| routing_hard (10) | 1.000 | 1.000 | 0.700 | 0.400 |

On aggregate hard accuracy, SemIf (0.595) is ahead of the CPU encoders in
this table (Jeff 0.377, GLiNER2.5 base 0.364). It is not ahead on every
family: Jeff's temporal/numeric accuracy is 0.333 and SemIf's is 0.200.
djev's published hard accuracy is higher (0.695 vs 0.595), including
multi-hop, policy, and probability. That is why djev was considered as a
research candidate. It is not an implemented profile. The published runtime
cannot run on this host.

reflex 4B is close to SemIf on intelligence and a bit better calibrated, on
the same base family. The author reports the 231 public items were used four
times as a development gate, and the run has no CPU backend in the evidence
used here. Winnow-12B Q8 and the Qwen3.8-27B direct-logit rows score higher
on hard items and need a large GPU (or an unmeasured GGUF port) plus, for
Winnow, Gemma licence terms and an unreproducible private corpus. They are
not the portable default.

2B-class decision heads (decider-2B, Zefan Open-Jev 2B) stay well below the
4B frozen readout on hard items.

## Why this default fits the machine

Target host: NVIDIA Tesla M10 (Maxwell, four 8 GB GPUs). A probe on this
machine reported compute capability 5.0. The Xeon has ample CPU memory.

**BF16 transformers path.** SemIf `load_causal_model` accepts only `auto`,
`cuda`, and `mps`. `auto` selects CUDA when present, otherwise MPS. Passing
`cpu` raises `Device must be auto, cuda, or mps`. CUDA mode also requires
exactly one visible GPU. The Hub card lists 4,659,861,248 BF16 parameters,
which is 8.68 GiB of BF16 weight bytes before activations or KV cache. One
M10 GPU is 8 GB, and the board exposes four devices. This BF16 path does not
fit that GPU.

**CPU path.** SemIf's llama.cpp loader forces `n_gpu_layers = 0`, scores from
a local GGUF, and keeps the pinned Hugging Face tokenizer so prompt hashes
match the torch path. It does not import vLLM. Quantized scores are
explicitly conditional on the GGUF: JevBench's 79.0 intelligence is the
**BF16 GPU** run, not this Q4 file. Checkweave later measured this Q4 file
on a 64-label sample: 63/64, with predicate gold 11/12
([semantic validation](semantic-validation.md), artifact
`experiments/semantic-validation/results/semif-q4-cpu.json`). That sample is
not a second JevBench score. Numerical differences versus BF16 are expected.

**Latency, measured versus inferred.**

| Bound | Status |
| --- | --- |
| JevBench SemIf p50 / p95 **0.198 s / 0.315 s** raw | Measured. Warm model on an RTX PRO 4500 32 GB, serial HTTP from Germany. Adjusted p50 used for the score is 0.546 s (×2 + 0.15 s), an assumption. |
| SemIf's own 21-criterion direct readout, median **1.023 s** | Measured on one RTX 3090 for an owned fixture, not the JevBench items. |
| CPU llama.cpp latency on this host | Measured for the Q4 sample, not the 534-item board. Ready 24.78 s. Warm p50 / p95 8.865 s / 11.482 s. Artifact `experiments/semantic-validation/results/semif-q4-cpu.json`. Do not reuse the GPU p50 as a timeout. |
| Process RSS for Q4 | Measured on that same sample: peak RSS 4,901,544 kB. The GGUF itself is 3.01 GB. This is not a memory-cap decision. |

djev's self-host guide asks for one NVIDIA B200, a CUDA 13 driver, torch
`2.13.0+cu130`, a patched vLLM (`0.29.1rc1.dev347`, base commit
`dee37d89115db4c94a820a79a78a7828e141c910`), BF16 weights and BF16 KV, and
about 52 GB for the 26B weights alone, with at least 100 GiB of disk. There
is no transformers or llama.cpp CPU inference path. CPU tests in that repo
use doubles and do not run the model. The Tesla M10 cannot host that stack.
JevBench's djev numbers are the hosted preview API (p50 raw 0.237 s, no
latency adjustment). The author states those probabilities are experimental
and uncalibrated, and the extracted BF16 release has not been freshly
GPU-benchmarked as a release. Internal B200 figures (12-request model-call
p50 59 ms, HTTP p50 376 ms; an older quantized run at 77 / 86 ms) are not
this package and not this host.

Jeff's CPU numbers are measured, on a different CPU: Ryzen 5 3600, 4 threads,
p50 raw 0.938 s, p95 raw 11.0 s. GLiNER2.5 base on that same CPU is p50 raw
0.313 s, p95 raw 4.15 s. Those latencies describe small encoders, not the 4B
default.

## Licensing

| Piece | Licence |
| --- | --- |
| SemIf code | MIT |
| `Qwen/Qwen3.5-4B` and the bartowski Q4_K_M GGUF | Apache-2.0 |
| djev-dev code | Apache-2.0; no djev-specific weights |
| `google/diffusiongemma-26B-A4B-it` | Apache-2.0 (Google's terms on the model card still apply) |
| GLiNER2.5 base / small / multi | Apache-2.0 |
| Jeff code | MIT; GLiFormer weights follow their model card |
| JevBench harness | MIT; weights keep their own licences |

## Limitations to keep in the implementation

- JevBench is English-only. A high easy-tier score (1.000 for SemIf) does not
  transfer to other languages. GLiNER2.5 multi was worse on this English set
  (intelligence 27.7) than the English base.
- SemIf hard-tier temporal/numeric accuracy is 0.200 and long-policy accuracy
  is 0.421. The default still needs Checkweave's evidence and abstention
  rules. A high label probability is not proof.
- open-alternative-jev, a different readout of the same 4B weights, fell from
  72% to 21% on answer-judging items when option order was reversed. SemIf's
  JevBench row does not include that reversal test. Pin option order in the
  adapter and do not assume the 4B model is order-invariant.
- The SemIf JevBench row predates the llama.cpp commit. That quality
  evidence is BF16 on a Blackwell GPU. The local Q4 sample (63/64 labels,
  11/12 predicate gold) is a separate measurement, not a second published
  JevBench score.
- Latency from Germany to a rented GPU is not local CPU latency.
- Held-out items were sent to the systems under test. That is not a
  contamination proof. reflex's public-item dev gate and Winnow's private
  corpus are separate, stronger caveats.
- djev-thinking (hard accuracy 0.777, calibration 92.7) is an experimental
  generation path. Current djev-dev hard-codes thinking off, one denoising
  step, and read-only inference. Neither path is a Checkweave profile.

Evidence copies and the metric extract live under `/tmp/checkweave-open-model`.
