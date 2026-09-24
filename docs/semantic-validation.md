# Semantic validation sample

**Status:** held-out measurement harness. Not a release qualification.

This sample is synthetic, small, and authored for evaluation. A score on it does
not establish broad accuracy, calibration, extraction quality, or a default
checkpoint. No numeric release gate is defined. `release_qualified` stays false. The measured local default below is SemIf
Qwen3.5-4B Q4_K_M on CPU. The GLiNER CPU and CUDA runs stay in this file as
the lightweight profile measurement.

The runner does not load a model. It speaks the worker's line-delimited JSON
protocol. Accuracy uses one question per `evaluate` call. Joint multi-question
calls are a coupling check only. A same-schema batch is throughput only. Model
scores are copied as returned and are not treated as calibrated probabilities.
Absent-evidence labels are author judgments. They are not rewritten when a model
disagrees.

## What this is not

The exploratory comparison in `experiments/model-backends/` has 24 short English
cases and was used while choosing a starting backend. This sample does not reuse
those texts. It was not used to pick the checkpoint. Do not merge the two scores.

## Sample

`experiments/semantic-validation/sample.json` is the pinned artifact. Every case
has an id, family, `source: synthetic`, `split: heldout`, text, and a question
schema. Each question has either an expected label or an expected capability
status.

| Family | Role |
| --- | --- |
| `collection_routing` | Supported choice. Which collection a note belongs to. |
| `negation` | Supported choice. A negative wording changes the label. |
| `absent_evidence` | Supported choice with an `unestablished` label. The text does not settle the question. |
| `prompt_injection` | Supported choice. Instructions inside the text are data. The label follows the content. |
| `paraphrase` | Supported choice pairs. Different wording, same expected label. |
| `label_order` | Supported choice pairs. Same text and label, reversed label list. |
| `multi_question` | One state, a choice and an ordinal, scored as separate requests. |
| `ordinal_rubric` | Supported ordinal levels for rollback detail. |
| `capability_split` | A supported choice plus a predicate on the same text. |
| `over_limit` | Padded text over the 4096-token budget. Expected status `unresolved`, with no label. |
| `unsupported_language` | Non-English sentences. Outside the English-only local profile. Not an accuracy item. |
| `unsupported_predicate` | Predicate questions whose fixture status is `unsupported`. |

Label questions are the supported classification set (64). For the GLiNER
profile, expected `unsupported` and `unresolved` rows are a capability check.
For SemIf, expected `unsupported` is that same profile check and is not graded:
SemIf can return a real predicate option. Over-limit rows stay graded for both.
Language rows are reported and then left out of accuracy. A label match on them
is not multilingual support.

`experiments/semantic-validation/predicate_gold.json` is a separate
preregistered set (`sv-pgold-01` through `sv-pgold-12`): four `supported`,
four `contradicted`, four `insufficient`. It was frozen before the SemIf run.
It is not part of the 64. The option wording is the worker schema:
`supported` is "The evidence establishes the claim", `insufficient` is
"The evidence does not establish either", `contradicted` is "The evidence
establishes the opposite". The worker keeps the `insufficient` label and
reports protocol status `unresolved`. Scoring counts that pair as the
insufficient option. It is not an unsupported question kind.

Schema strings avoid the classifier's reserved markers, including parentheses,
so a request is not rejected for an unbuildable schema.

## Baselines

There is no training split. Neither baseline reads expected labels or fits
parameters on this sample.

- **fixed_constant.** The lexicographically smallest label in the question
  schema. This is not the empirical mode of the held-out answers. Computing that
  mode from the evaluation labels would leak.
- **lexical_overlap.** Most shared tokens between the text and the label name
  plus description, after a fixed stopword list. Ties break lexicographically.
  No overlap abstains as `unresolved`. Negation words are stopwords, so this
  baseline is weak on negation by construction.

Both baselines return `unsupported` for predicate questions and for
`language != en`. Text whose UTF-8 size exceeds `4096 * 4` bytes is
`unresolved`. That byte ceiling is a baseline proxy, not the worker tokenizer.

## Metrics

Reported separately:

- Per-family precision, recall, and confusion for expected-label items. Overall
  accuracy is micro only, because the same label string can mean different
  things in different families.
- Protocol coverage, and accuracy only on covered supported-label items.
  Uncovered items stay in the coverage rate.
- Unsupported rate, unresolved rate, and missing rate.
- High-score errors: a wrong resolved label whose reported score clears 0.8 or
  0.9. Those cutoffs are not confidence.
- Warm per-question p50 and p95 after one discarded warmup request.
- Batch throughput for four `collection_routing` states in one request.
- Linux worker RSS and HWM from `/proc/PID/status`.
- Loading time: process start until the ready frame. This worker loads weights
  before it emits ready.

The output JSON pins SHA-256 hashes of the sample, runner, metrics, baselines,
and sample authoring module, plus worker provenance when a run exists.

## Run

Unit tests use the system Python and do not load weights:

```sh
python3 experiments/semantic-validation/test_metrics.py
python3 experiments/semantic-validation/test_sample.py
```

A live run needs `python/checkweave_worker` and the existing experiment
interpreter. From the repository root:

```sh
python3 experiments/semantic-validation/runner.py \
  --python /tmp/checkweave-backends.ER6XYe/venv/bin/python \
  --device cpu --threads 4 \
  --output experiments/semantic-validation/results/cpu.json

python3 experiments/semantic-validation/runner.py \
  --python /tmp/checkweave-backends.ER6XYe/venv/bin/python \
  --device cuda:0 --threads 4 \
  --output experiments/semantic-validation/results/cuda.json

python3 experiments/semantic-validation/runner.py \
  --python /tmp/checkweave-semif-venv/bin/python \
  --backend semif --device cpu --threads 4 \
  --gguf /home/overseer1/.cache/huggingface/hub/models--bartowski--Qwen_Qwen3.5-4B-GGUF/snapshots/4168f45a16a1290d65a4ec0fa312ae917a4c15d6/Qwen_Qwen3.5-4B-Q4_K_M.gguf \
  --predicate-gold experiments/semantic-validation/predicate_gold.json \
  --gliner-results experiments/semantic-validation/results/cpu.json \
  --request-timeout 180 --ready-timeout 600 \
  --output experiments/semantic-validation/results/semif-q4-cpu.json
```

`--baselines-only` writes baseline metrics and does not start a worker. If the
worker module is absent, the runner records `benchmark_run: false` and does not
invent model numbers.

An explicit CPU run sets `CUDA_VISIBLE_DEVICES` empty so PyTorch does not
initialize the host GPUs before the ready frame. The CUDA run uses `--device cuda:0`
and leaves the devices visible.

## Results

Measured 2026-09-21 on this host: Linux 6.8 x86-64, Intel Xeon E5-2698 v3,
four PyTorch threads, float32, offline cache. CUDA is one Tesla M10. These
figures are from `experiments/semantic-validation/results/cpu.json` and
`cuda.json`. They are not a release decision. `release_qualified` is false
because no numeric gate is defined.

Sample SHA-256 `9e9b2a25120e712af9236f8c2a582ea7aa83f05632058783adc6568febd739b9`.
The runner hash is recorded in each JSON file. Provenance is
`fastino/gliner2.5-base-v1` revision `1a8bc24e00dc7300b9017c81d63e3dcdabb26596`,
adapter `checkweave-gliner2-1`, Python 3.12.9, PyTorch 2.4.0, transformers
4.46.3, gliner2 2.0.0. Score semantics reported by the worker are
`exclusive_softmax`. Those scores are not calibrated.

### Unit tests

`test_metrics.py` (12 tests) and `test_sample.py` (10 tests) passed. The metric
tests use a known cat/dog/bird confusion pattern, including an unresolved row
and a missing row. The fixture tests check the 60-label minimum, disjointness
from the 24 exploratory texts, pair structure, baseline isolation from expected
labels, and that one evaluate response records only the question it asked.

The builder reports 75 cases, 64 supported label questions, and 81 questions
including capability rows.

### Labels

Both devices returned a result for every question (coverage 1.0) and selected
the same status and label on all 81 rows. Supported-label accuracy is 50/64
(0.781). That denominator is the covered supported items. It is a separate
number from coverage.

| Family | Correct / items | Macro precision | Macro recall |
| --- | --- | --- | --- |
| collection_routing | 9/10 | 0.917 | 0.938 |
| negation | 7/8 | 0.900 | 0.875 |
| absent_evidence | 5/8 | 0.722 | 0.750 |
| prompt_injection | 4/6 | 0.722 | 0.625 |
| paraphrase | 7/8 | 0.917 | 0.875 |
| label_order | 4/6 | 0.750 | 0.667 |
| multi_question | 7/8 | 0.917 | 0.917 |
| ordinal_rubric | 5/8 | 0.750 | 0.667 |
| capability_split (choice only) | 2/2 | 1.000 | 1.000 |

Capability checks that are graded: 11/11. All 8 predicate questions, including
the two paired with a supported choice, came back `unsupported`. All 3
over-limit texts came back `unresolved` with no label (about 6100 serialized
tokens against the 4096 limit). Unsupported rate on the full 81 questions is
8/81. Unresolved rate is 3/81.

The six non-English rows were resolved with a label. They stay outside
accuracy. `language: en` on the worker is a profile limit, and this worker does
not refuse other languages. A label on those rows is not multilingual support.

Joint requests agreed with the separate-question labels on 10/10 comparable
rows. Agreement is a coupling check. The accuracy table uses the separate calls.
The four-state routing batch is throughput only.

### Baselines

On the same 64 label questions, `fixed_constant` scored 19/64 (0.297) and
`lexical_overlap` scored 22/64 (0.344). Neither was fit on these labels.

### High-score mistakes

Eight wrong labels had a selected softmax score of at least 0.8, and six of
those were at least 0.9. The cutoff is not a probability of correctness.
Expected labels were left as authored.

| Id | Expected | Selected | Score |
| --- | --- | --- | --- |
| sv-route-09 | incident_record | how_to | 0.997 |
| sv-neg-08 | completed | not_completed | 0.964 |
| sv-abs-05 | unestablished | not_notified | 0.821 |
| sv-abs-07 | unestablished | not_notified | 0.838 |
| sv-para-01b | incident_record | how_to | 0.937 |
| sv-multi-03 | incident_record | meeting_note | 0.991 |
| sv-ord-06 | actionable | mentioned | 0.946 |
| sv-ord-07 | actionable | mentioned | 0.998 |

`sv-abs-04` was also wrong (`unestablished` selected as `notified`) at about
0.39, so it is not in the high-score list. Two prompt-injection misses and the
label-order pair `sv-order-03` were wrong with scores under 0.8. The same
label-order text was wrong in both label orders, so the pair agreed with
itself and disagreed with the author.

Absent evidence remains a release blocker on this checkpoint: three of eight
`unestablished` items were given a definite label, two of them above 0.8.
Those mistakes were not relabeled.

### Time and memory

Loading time is process start until the ready frame, which is after weights
load and the worker's own probe.

| | CPU | CUDA (`cuda:0`) |
| --- | --- | --- |
| Loading | 8.72 s | 9.75 s |
| Warm per-question p50 | 0.263 s | 0.074 s |
| Warm per-question p95 | 0.332 s | 0.079 s |
| Batch of 4 routing states | 5.44 states/s | 21.0 states/s |
| Peak process RSS | 1,549,512 kB | 1,206,588 kB |
| Peak HWM | 2,183,044 kB | 2,198,588 kB |

RSS is the worker process on Linux. It is not GPU framebuffer accounting.
The batch is four short states, so the throughput number does not describe
long documents. Timing was on a shared machine.

### Limits of this measurement

The sample is synthetic and small. Fifty correct labels out of sixty-four do
not qualify `gliner2.5-base-v1` as a default. No release gate was applied.
Mac, Windows, ROCm, and MPS were not run. Device memory for the M10 was not
read from the driver. Cancellation, accelerator failure, and offline restart
were not part of this client run.

## SemIf Q4_K_M CPU

Measured 2026-09-22 on the same host, artifact
`experiments/semantic-validation/results/semif-q4-cpu.json`. One live worker
pipe, stdin kept open. Ready, then 94 evaluate responses (one warmup, the 81
sample questions, and the 12 predicate-gold questions) before coupling and the
batch. `roundtrip_confirmed` is true. No request hit the 180-second deadline.
`release_qualified` is false. These Q4 scores are not the BF16 JevBench
numbers.

Worker provenance: backend `semif`, model `Qwen/Qwen3.5-4B` revision
`851bf6e806efd8d0a36b00ddf55e13ccb7b8cd0a`, code pin
`1f2dea3e25379f9dfc98cb83c324f00ab5deda37` (`semif-phase1` 0.1.0), prompt
`direct-options-v1`, adapter `checkweave-semif-1`, precision `gguf-q4_k_m`,
`n_gpu_layers` 0, context 4096, threads 4, offline. GGUF file
`Qwen_Qwen3.5-4B-Q4_K_M.gguf`, 3,013,027,808 bytes, sha256
`13c16f426047e2de38cd075bdade4a7bcbc8c774384876f677740cda65f8a983`,
revision `4168f45a16a1290d65a4ec0fa312ae917a4c15d6`. Packages from the worker:
Python 3.12.9, torch `2.10.0+cpu`, transformers 5.17.0, llama-cpp-python
0.3.35, tokenizers 0.23.2, huggingface-hub 1.31.0. The interpreter was
`/tmp/checkweave-semif-venv/bin/python`. Hardware: Intel Xeon E5-2698 v3,
64 hardware threads, four Tesla M10s present and unused (`n_gpu_layers` 0).

Sample hash is unchanged:
`9e9b2a25120e712af9236f8c2a582ea7aa83f05632058783adc6568febd739b9`.
Predicate-gold hash
`9f66182d56febf6a5e9c73b2211964064def84573d13a4903f83fc47fffdcc06`.
GLiNER `cpu.json` and `cuda.json` were not rewritten. Adapter mapping
failures: 0.

### Same 64 labels

Coverage on the 81 sample questions is 1.0. Supported-label accuracy is
63/64 (0.984). Every family was  complete except `ordinal_rubric` at 7/8.
The one miss is `sv-ord-04`: text "Operators can revert this change.",
expected `mentioned`, selected `unspecified` at 0.804
(`mentioned` 0.192, `actionable` 0.005). That is the only selected score
at or above 0.8 on a wrong label, and none reached 0.9. The cutoff is not
calibrated confidence. The gold label was not changed.

On this denominator, stored GLiNER CPU is 50/64. Both correct: 49. SemIf
only: 14, which are the GLiNER misses listed above (`sv-ord-03` as well as
the eight high-score GLiNER mistakes and the lower-score GLiNER misses).
GLiNER only: `sv-ord-04`. Both wrong: 0. Fixed-constant baseline 19/64.
Lexical-overlap baseline 22/64. Same comparison is stored on the SemIf JSON
as `gliner_comparison`.

The eight fixture rows whose expected status is `unsupported` all returned a
predicate option. None came back `unsupported`. They stay ungraded.
`sv-pred-03` selected `insufficient` at 0.527; the other seven selected
`supported`, `contradicted`, or `insufficient` above 0.9. The six language
rows resolved and stay outside accuracy. The three over-limit texts were
`unresolved` with no label, at 7276, 7230, and 7240 serialized tokens
against 4096, each in about 0.07 seconds. Joint requests agreed with the
separate-question labels on 11/11 comparable rows. That agreement is not
accuracy.

### Predicate gold

11/12. All four `supported` and all four `insufficient` matched. Three of
four `contradicted` matched. `sv-pgold-06` expected `contradicted` for
"The note records the full request body" given "The note records only the
hostname." The worker selected `insufficient` at 0.527 and status
`unresolved`, with the explicit-insufficient reason. That score is under
0.8. The insufficient option is a real prompt option; the protocol remap
does not mean the prompt lacks an insufficient choice. Prompts and
thresholds were not tuned after these labels.

### Time and memory

| | SemIf Q4 CPU |
| --- | --- |
| Loading (start until ready) | 24.78 s |
| Discarded warmup | 13.10 s |
| Warm per-question p50 (n=81) | 8.865 s |
| Warm per-question p95 | 11.482 s |
| Slowest of those 81 | 14.755 s |
| Batch of 4 routing states | 42.82 s (0.093 states/s) |
| Peak process RSS | 4,901,544 kB |
| Peak HWM | 4,912,048 kB |
| Per-request deadline | 180 s |
| Timeouts | 0 |

RSS is the worker process. The batch figure is four short states. Timing
was on a shared machine. This sample does not show a cluster of
near-certain wrong labels that would justify a Q8 download. Q8 was not
fetched. No hosted provider was called. BF16 was not run on the M10s.
