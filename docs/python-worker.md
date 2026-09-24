# Python inference worker

Default local backend is SemIf direct option-logit scoring of `Qwen/Qwen3.5-4B`.
GLiNER2.5 is an explicit `--backend gliner2` profile. Hosted providers are
never selected by this process.

The worker is a supervised process. Stdout is newline-delimited JSON. Stderr
is diagnostics. Protocol version is 1. Frames are at most 8388608 bytes.
`FrameReader` uses `read1` on a `BufferedReader`, so a short line returns
while the writer stays open.

```sh
PYTHONPATH=python python -m checkweave_worker \
  --backend semif \
  --device auto \
  --threads 4 \
  --offline
```

`--backend` is `semif` (default) or `gliner2`. `--device` is `auto`, `cpu`,
`cuda`, `cuda:<index>`, or `mps`. `--gguf PATH` overrides the pinned GGUF for
the SemIf CPU path. `--offline` reads the local Hugging Face cache.

## SemIf default

Prompt text is upstream `semif_phase1.direct` at code pin
`1f2dea3e25379f9dfc98cb83c324f00ab5deda37` (`direct-options-v1`). This worker
does not substitute its own system string. Each state/question pair is one
`score()` call. Shared-prefix batching is not used.

| Field | Value |
| --- | --- |
| Model | `Qwen/Qwen3.5-4B` |
| Tokenizer revision | `851bf6e806efd8d0a36b00ddf55e13ccb7b8cd0a` |
| CPU checkpoint | `bartowski/Qwen_Qwen3.5-4B-GGUF` @ `4168f45a16a1290d65a4ec0fa312ae917a4c15d6` |
| GGUF file | `Qwen_Qwen3.5-4B-Q4_K_M.gguf`, 3013027808 bytes |
| GGUF sha256 | `13c16f426047e2de38cd075bdade4a7bcbc8c774384876f677740cda65f8a983` |
| Adapter version | `checkweave-semif-1` |
| CPU precision | `gguf-q4_k_m`, llama.cpp `n_gpu_layers=0` |
| Context | 4096 tokens, including the upstream system text and every option |
| Code pin | `1f2dea3e25379f9dfc98cb83c324f00ab5deda37` |

Q4 scores are a softmax over the declared option tokens. Provenance records
them as uncalibrated and not a BF16 quality measurement. The published
JevBench BF16 numbers do not apply to this GGUF.

BF16 (`bfloat16`) is attempted only when exactly one CUDA device is visible,
that device supports BF16, and free memory is at least 10930335232 bytes.
The fit gate runs before the weights load. An explicit CUDA request that
fails the gate, or whose BF16 load or probe fails, exits. It does not load
Q4. `auto` that passes the gate and then fails the BF16 load or probe drops
that model and constructs a new pinned Q4 llama.cpp backend. It does not
call `.place("cpu")` on the BF16 model. Provenance then records
`gguf-q4_k_m`, device `cpu`, and that fallback.

Several GPUs are not a multi-GPU run. Before torch starts, `auto` may set
`CUDA_VISIBLE_DEVICES` to the first visible GPU that passes the gate.
Explicit `cuda:N` is isolated only when that index itself passes; a
different GPU is not substituted. After that isolation the process executes
on `cuda:0`, because torch renumbers the one visible device. Provenance
keeps `device_requested` as the original request and `cuda_visible_devices`
as the physical index. A `CUDA_VISIBLE_DEVICES` value that is not
a comma-separated index list is left unchanged. AMD and ROCm are not a
supported selection.

On this host `nvidia-smi` lists four Tesla M10 GPUs (compute capability 5.0,
8 GB). None pass the gate, so `auto` and `cpu` stay on the Q4 CPU backend.
The managed installer selects the CPU torch wheel and does not download
CUDA. The CPU scoring path is unchanged.

### Predicates

SemIf accepts predicate questions. Options stay in this order, with the WANLI
descriptions from SemIf `benchmarks/build_wanli.py` at the code pin:

| Label | Description | Result |
| --- | --- | --- |
| `supported` | The evidence establishes the claim | resolved, `value` true |
| `insufficient` | The evidence does not establish either | unresolved, `value` null |
| `contradicted` | The evidence establishes the opposite | resolved, `value` false |

The question text is `Assess the claim using only the supplied evidence: `
plus the statement. Selecting `insufficient` is the explicit option. A low
score on another option is not treated as abstention. Scores are not
calibrated truth.

Choice criterion: `Which listed option applies to the evidence?` Ordinal
criterion: `Which listed level applies to the evidence?` Option descriptions
are the request descriptions, otherwise the label names. Declaration order is
pinned. Ordinal `value` is the expected level only when scores sum to 1
within 1e-3, marked `adapter_derived_expected_level`.

Token counts use `encode_prompt` on the full rendered prompt. Over the limit
is `unresolved` with `omitted` null. The library raises if a prompt would be
truncated; the worker counts first and does not send that prompt to the model.

### Install

`python/requirements.txt` includes `python/requirements-semif.txt`. Keep this
environment separate from `python/requirements-gliner2.txt`. The tested CPU
environment is `/tmp/checkweave-semif-venv`. `llama-cpp-python==0.3.35` is an
sdist here; the build used `CMAKE_BUILD_PARALLEL_LEVEL=6`.

```sh
python -m venv /tmp/checkweave-semif-venv
/tmp/checkweave-semif-venv/bin/pip install 'torch==2.10.0' \
  --index-url https://download.pytorch.org/whl/cpu
CMAKE_BUILD_PARALLEL_LEVEL=6 \
  /tmp/checkweave-semif-venv/bin/pip install \
  'transformers==5.17.0' 'accelerate==1.12.0' 'safetensors==0.8.0' \
  'huggingface-hub==1.31.0' 'tokenizers==0.23.2' 'numpy==2.2.6' \
  'sentencepiece==0.2.1' 'protobuf==7.36.1' 'llama-cpp-python==0.3.35'
/tmp/checkweave-semif-venv/bin/pip install --no-deps \
  'semif-phase1 @ git+https://github.com/TheoLeeCJ/SemIf.git@1f2dea3e25379f9dfc98cb83c324f00ab5deda37'
```

Installing the requirements file after the CPU wheel also works when pip
treats `torch==2.10.0` as already satisfied by `2.10.0+cpu`. The `--no-deps`
package install above keeps that wheel from being replaced. Confirmed in this
environment: `torch 2.10.0+cpu`, `transformers 5.17.0`, `llama-cpp-python 0.3.35`,
`semif-phase1 0.1.0`.

Checkweave's managed setup uses the same CPU index on this host. A Linux GPU
that passes the fit gate gets `torch==2.10.0` from
`https://download.pytorch.org/whl/cu128` instead, in a different environment
directory. macOS is not installed from the Linux CPU index. Installing the
environment does not load the model; the first ready frame on this CPU path
took about 25 seconds. The worker does not lengthen a timeout the caller
already set.

Download the tokenizer files and the one GGUF. Do not fetch the BF16
safetensors shards for this CPU profile.

```sh
hf download Qwen/Qwen3.5-4B \
  --revision 851bf6e806efd8d0a36b00ddf55e13ccb7b8cd0a \
  --include tokenizer.json tokenizer_config.json vocab.json merges.txt \
  config.json chat_template.jinja
hf download bartowski/Qwen_Qwen3.5-4B-GGUF Qwen_Qwen3.5-4B-Q4_K_M.gguf \
  --revision 4168f45a16a1290d65a4ec0fa312ae917a4c15d6
```

### CPU smoke

```sh
/tmp/checkweave-semif-venv/bin/python python/tests/smoke_semif.py
```

Offline, `--backend semif --device cpu --threads 4`. The writer stayed open
between requests. Wall clock 47.3 s, ready after 24.5 s (sha256 of the GGUF,
load, and the startup probe). Artifact:
[python/tests/results/semif-cpu-smoke.json](../python/tests/results/semif-cpu-smoke.json).
Stderr: `python/tests/results/semif-cpu-stderr.log`.

One short state ("The deployment completed, health checks passed, and no
rollback was started.") and claim "The deployment succeeded." Predicate label
`supported`, value true, scores about 0.9984 / 0.0011 / 0.0005. Choice label
`behavior`, scores about 0.1245 / 0.8755. The second request, still on the
open pipe, returned the same choice scores. A choice prompt of 113 tokens was
rejected at `max_input_tokens` 8 with `omitted` null. Shutdown with an id
returned `shutdown: true`.

These labels are the model's returned argmax on two hand-written lines. They
are not a calibration result and not a BF16 comparison. `device_fallback` is
null because the process was started with `--device cpu`.

Unit tests, 22 passed:

```sh
PYTHONPATH=python python3 -m unittest discover -s python/tests -p test_worker.py -v
```

That run includes a real `os.pipe` whose writer stays open for two frames and
for oversize recovery. It also checks that a 4-GPU, no-BF16 inventory selects
Q4 and that an explicit CUDA request on that inventory fails instead of
substituting Q4.

## GLiNER2.5 optional

`--backend gliner2` with `python/requirements-gliner2.txt` in its own
virtualenv. Do not install it into `/tmp/checkweave-semif-venv`. The earlier
experiment env `/tmp/checkweave-backends.ER6XYe/venv` was left unchanged.

| Field | Value |
| --- | --- |
| Model | `fastino/gliner2.5-base-v1` |
| Revision | `1a8bc24e00dc7300b9017c81d63e3dcdabb26596` |
| Adapter | `gliner2.classification.Classifier`, softmax |
| Adapter version | `checkweave-gliner2-1` |
| Precision | float32 |
| Predicates | `unsupported` |

A low score is not abstention. Earlier offline smokes with that environment:
CPU about 10.7 s, `cuda:0` about 11.9 s, `auto` selected `cuda:0` about 11.3 s.
Numbers are in [python/tests/results/local-smoke.json](../python/tests/results/local-smoke.json).
CPU batching of one choice with a separate ordinal left the choice scores
unchanged on that pair of short texts (`max_abs_score_delta` 0.0).

## Platform matrix

| Platform | This change |
| --- | --- |
| Linux x86_64, CPU, Python 3.12.9, torch 2.10.0+cpu, llama-cpp-python 0.3.35, 4 threads, Q4_K_M | Ran offline. Live pipe, two evaluates, over-limit, shutdown |
| Linux x86_64, four Tesla M10 GPUs | Visible to `nvidia-smi`. The CPU torch wheel reports zero CUDA devices. BF16 was not loaded |
| macOS / MPS | Not run |
| Windows | Not run |
| AMD ROCm | Not run |

## Limits

Context handling rejects the whole rendered prompt past the requested token
limit (hard cap 4096). It does not chunk. The 113-token count above includes
the upstream system prompt and the option text, not the state sentence alone.

Predicate `insufficient` is only the explicit third option. Scores are not a
calibrated probability of being correct. Q4_K_M quality on a held-out sample
is outside this smoke.
