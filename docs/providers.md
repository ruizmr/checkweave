# Model providers

The local default is SemIf's option-logit readout on pinned Qwen3.5-4B.
That choice is the current open-model decision, not a release qualification
of the quantized CPU path. Hosted Jev is explicit and is never a fallback.
GLiNER2.5 is an optional lightweight profile.

Checkweave keeps exact comparisons, counting, and coverage in the kernel.
This boundary only judges text.

## Configuration

`checkweave.toml` may contain a `[model]` table. A missing file or a missing
table selects the SemIf profile and stores no credential. An invalid table
is an error. Unknown keys such as `api_key` are rejected.

```toml
[model]
provider = "local"
device = "auto"
threads = 4
```

`profile = "default"` is SemIf. The checkpoint and revision are filled from
the pins in `docs/open-model-decision.md`:

- tokenizer `Qwen/Qwen3.5-4B` @ `851bf6e806efd8d0a36b00ddf55e13ccb7b8cd0a`
- code `1f2dea3e25379f9dfc98cb83c324f00ab5deda37`
- CPU GGUF `bartowski/Qwen_Qwen3.5-4B-GGUF` @ `4168f45a16a1290d65a4ec0fa312ae917a4c15d6`, file `Qwen_Qwen3.5-4B-Q4_K_M.gguf`

A capable GPU loads BF16. This machine's CPU path is llama.cpp with
`n_gpu_layers = 0`. Precision `gguf-q4_k_m` is not the BF16 JevBench
measurement. `auto` may record `device_fallback` when Q4 is what actually ran.
Explicit CUDA that cannot hold the BF16 weights fails startup.

An existing config that names `fastino/gliner2.5-base-v1` and omits `profile`
is the lightweight profile, not an error. `profile = "lightweight"` is the
same pin at revision `1a8bc24e00dc7300b9017c81d63e3dcdabb26596`. Predicates
are unsupported on that profile only. SemIf scores predicates as
`supported` / `insufficient` / `contradicted`.

Device strings are `auto`, `cpu`, `mps`, `cuda`, or `cuda:<index>`.

Hosted Jev is selected only by name:

```toml
[model]
provider = "jev"
model = "jev-1.13.0"
api_key_env = "TYPESAFE_API_KEY"
```

`jev-latest` and `jev-preview` are rejected. The bearer token stays in the
named environment variable.

## Setup

`setup(&config, offline)` installs the profile that `config` names.
`setup_cancellable(&config, offline, cancel)` is the same call with a flag
the install lock polls. The lock uses `try_lock` on a blocking thread so the
async runtime is not stuck inside `flock`.

Managed setup requires Python 3.12 (`uv venv --python 3.12`, or
`python3.12 -m venv`). It installs the environment only. It does not download
the GGUF or load weights. The first `readiness` or `evaluate` does that. On
this CPU path the ready frame was about 25 seconds. A caller that passes a
shorter timeout keeps that timeout; this module does not raise it.

The SemIf environment is not the GLiNER environment. The directory identity
is the profile, the selected torch wheel, and that profile's requirements, so
a CPU environment and a CUDA environment cannot share a virtualenv. Before
install, a 3-second `nvidia-smi` probe applies the same BF16 gate as the
worker: compute capability at least 8 and at least 10930335232 free bytes on
the addressed GPU.

| Request | Linux result |
| --- | --- |
| `cpu` | `torch==2.10.0+cpu` from the PyTorch CPU index |
| `auto`, no GPU passes the gate | same CPU wheel. This 4× Tesla M10 host takes this path and does not download CUDA |
| `auto`, one visible GPU passes | `torch==2.10.0+cu128` from the PyTorch cu128 index. Several fitting GPUs are not a multi-GPU install; the worker later exposes one of them |
| `cuda` or `cuda:N`, that visible GPU passes | cu128 wheel |
| `cuda` or `cuda:N`, that GPU does not pass | setup fails. The CPU wheel is not installed instead |
| `mps` | error on Linux |

macOS CPU and MPS use the default host torch wheel (`torch==2.10.0+default-untested`), not the Linux CPU index. That path is not tested here. Other operating systems are refused. `CUDA_VISIBLE_DEVICES` must be a comma-separated index list; UUIDs are an error. An empty list hides every GPU. When the worker isolates one physical GPU, execution uses `cuda:0` and provenance keeps the original request plus `cuda_visible_devices`.

SemIf then installs the pinned requirements and builds `llama-cpp-python` with
`CMAKE_BUILD_PARALLEL_LEVEL=6`. `CHECKWEAVE_PYTHON` skips the managed
environment. `CHECKWEAVE_REQUIREMENTS_FILE` replaces the embedded requirements
for a test install and skips the torch bootstrap.

The worker that gets installed is the canonical `python/checkweave_worker`
package embedded in the binary. It is launched as
`python -m checkweave_worker --backend semif|gliner2 --device … --threads N`.
Stdout is protocol. Stderr is diagnostics.

## Readiness and cache identity

`ModelProvider::readiness(timeout_ms, cancel)` returns the live ready
provenance. It starts or reuses the worker and does not send an evaluate
frame. Use that object before accepting a semantic cache hit. The cache key
includes provider, model, revision, adapter, device, requested device,
fallback, precision, backend, profile, code revision, GGUF hash, runtime
versions, score semantics, input policy, and any extra provenance fields.
It omits usage. A fallback or GGUF change produces a different key.

`ModelResult.value` keeps a JSON number for ordinal and Jev scores and a
JSON boolean for a SemIf predicate.
