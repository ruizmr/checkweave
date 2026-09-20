# Exploratory backend comparison

This is a small inference experiment, not the Checkweave runtime or a release
benchmark. It compares two checkpoint/adapter combinations on 24 short, synthetic
English examples written for this investigation. No training, fine-tuning, or
calibration was performed.

The fixtures cover change descriptions, reports of credential exposure, evidence
support, and urgency. They include five cases whose expected label is `unknown`.
These are prose classification tasks, not tests of code execution or secret
scanning. The fixture, runner, and checkpoint hashes are recorded in each result.

## Results

Measured on 2026-09-20, Linux x86-64, Intel Xeon E5-2698 v3, four PyTorch CPU
threads, and one NVIDIA Tesla M10 GPU. Both paths use float32. Each run loads once,
performs one unmeasured warmup, and then evaluates the cases individually.
Downloads and model loading are excluded from per-case latency. This was a shared
machine without isolated CPU cores; timing is indicative.

| Checkpoint | Correct | Change scope | Exposure | Evidence support | Urgency |
| --- | --- | --- | --- | --- | --- |
| GLiNER2.5 base | 20/24 | 5/6 | 4/6 | 5/6 | 6/6 |
| GLiClass v3 base | 15/24 | 4/6 | 5/6 | 2/6 | 4/6 |

A constant majority label per task would score 9/24 on these fixtures. That is a
descriptive baseline computed from the fixture labels, not a held-out estimate.
The CPU and CUDA runs produced the same selected labels for each model.

GLiNER2.5's warm median was approximately **174 ms on CPU** and **59 ms on the
Tesla M10**. Full outputs and timings are in [results](results). These numbers
do not establish batching throughput, modern-GPU performance, laptop performance,
or long-context latency. Memory consumption was not measured.

### The important failure

All four GLiNER2.5 errors were on missing evidence. Only one of five `unknown`
cases was recognized. For example, the input says the tests passed only on Linux
and asks whether they passed on Windows. The expected result is insufficient
evidence; the model selects `contradicted` with an approximately 0.99984 softmax
score. A high score threshold would not resolve this failure.

The result favors GLiNER2.5 as a starting implementation backend for semantic
classification. It does **not** qualify either model as a general evidence judge.
The set is small, authored, and used to select a candidate; it is not an
independent final evaluation. No Jev request or Laya inference was performed.

## Reproduce

Use an isolated Python environment and install PyTorch for the device being
tested. The recorded run used Python 3.12.9 and PyTorch 2.4.0 with CUDA 12.1.
These are experimental compatibility versions, not a production dependency
recommendation. Qualify a maintained runtime separately before release.

Install the experiment's adapter versions in that environment:

```sh
python -m pip install 'gliner2[local]==2.0.0' 'gliclass==0.1.16' \
  'transformers==4.46.3' 'peft==0.13.2' 'numpy==2.2.6' \
  'huggingface-hub==0.36.2' 'tokenizers==0.20.3' 'safetensors==0.8.0'
```

From the repository root:

```sh
python experiments/model-backends/run.py --backend gliner2 --device cpu \
  --output /tmp/gliner2-cpu.json
python experiments/model-backends/run.py --backend gliner2 --device cuda:0 \
  --output /tmp/gliner2-cuda.json
python experiments/model-backends/run.py --backend gliclass --device cpu \
  --output /tmp/gliclass-cpu.json
python experiments/model-backends/run.py --backend gliclass --device cuda:0 \
  --output /tmp/gliclass-cuda.json
```

Choose an output path appropriate to your OS. The runner downloads public,
revision-pinned checkpoints into the Hugging Face cache. Weights are not stored
in this repository. Mac/MPS, Windows, and AMD/ROCm were not tested here; automatic
device selection and fallback are future runtime work.

## Adapter details and limitations

- GLiNER2.5 receives a single classification schema with label descriptions. The
  runner explicitly calls `.to(device)` to move model parameters as well as
  configure input placement. In the recorded Transformers version, its encoder
  falls back from the requested SDPA attention implementation to eager attention.
- GLiClass receives the same descriptions formatted as `label: description`,
  scores every label with its multi-label pipeline, and selects the maximum.
  Those raw sigmoid scores are independent; they are not a normalized choice
  distribution. This is a transparent starting adapter, not a tuned best prompt.
- Both paths make one request per case. The comparison does not test independent
  questions sharing a batch, extraction quality, ordinal expected-value scores,
  calibration, adversarial input, cancellation, or context overflow.
- The requests are short. The GLiClass adapter uses a 512-token maximum and its
  upstream pipeline can truncate. This research runner must not be reused as a
  production adapter without explicit input-budget checks.
- Reported `confidence`, `exact`, and `feasible` fields are upstream outputs.
  Decoder feasibility means the label assignment satisfies schema constraints;
  it does not establish semantic correctness.

See the [backend decision](../../docs/model-backends.md) for the release gates.
