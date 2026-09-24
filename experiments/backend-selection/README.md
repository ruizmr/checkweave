# Backend selection smoke

Local inference on the existing 24 synthetic English cases in
`experiments/model-backends/cases.json`. This is not the Checkweave runtime,
not the JevBench set, and not a reason to pick the local default.

The adopted default is SemIf on `Qwen/Qwen3.5-4B`, documented in
[docs/model-backends.md](../../docs/model-backends.md) and
[docs/open-model-decision.md](../../docs/open-model-decision.md). SemIf was
**not measured** here. GLiFormer CUDA was **not measured**.

## What ran

2026-09-21, Linux x86-64, Intel Xeon E5-2698 v3, four threads, float32.
GLiNER2.5 CUDA used one Tesla M10 (compute capability 5.0). One unmeasured
warmup, then one case at a time. Downloads and load time are outside the
per-case latency. The machine was shared.

| Run | Matched | Decisive | Missing evidence | Median | p95 | Peak RSS |
| --- | ---: | ---: | --- | ---: | ---: | ---: |
| GLiNER2.5 base CPU | 20/24 | 19/19 | 1/5 | 0.201 s | 0.254 s | 2.40 GB |
| GLiNER2.5 base CUDA | 20/24 | 19/19 | 1/5 | 0.061 s | 0.063 s | 1.32 GB host |
| GLiFormer-large CPU | 16/24 | 13/19 | 3/5 | 0.571 s | 0.720 s | 3.60 GB |

CUDA allocated about 1.59 GB for the GLiNER2.5 run. CPU and CUDA selected the
same GLiNER2.5 labels. GLiNER2.5 label order agreed 24/24; a second question
in the same call changed 3/24. GLiFormer label order agreed 21/24; a shared
pass changed 4/24. Full rows are in [results](results).

GLiNER2.5 uses `fastino/gliner2.5-base-v1` revision
`1a8bc24e00dc7300b9017c81d63e3dcdabb26596` and the existing environment
`/tmp/checkweave-backends.ER6XYe` (`gliner2==2.0.0`, torch 2.4.0). That
process loads the classifier and the extractor, so RSS is not a single-worker
figure. The 75-case worker measurement is in
[docs/semantic-validation.md](../../docs/semantic-validation.md).

GLiFormer uses `knowledgator/gliformer-large-v1` revision
`d0a4e53d09cebe6bc963dd9be319d4279084bb2d` (`pytorch_model.bin`,
2,302,735,855 bytes), package `gliformer==0.1.2`, torch `2.5.1+cpu`, eager
attention. Scoring follows Jeff commit `6f43d3e`: labels folded as
`key: description`, argmax of independent sigmoids, temperature 3.2 only on
the reported distribution. The isolated environment is
`/tmp/checkweave-model-selection/venv`. Do not install into the GLiNER
environment.

Pins for the JevBench v1.2.2 adapters and the v1.3.0 rescore pointer are in
[pins.json](pins.json).

## Reproduce

From the repository root, with the environments above already installed:

```sh
/tmp/checkweave-backends.ER6XYe/venv/bin/python \
  experiments/backend-selection/run_compare.py \
  --backend gliner2 --device cpu --threads 4 \
  --output /tmp/gliner2-cpu.json

/tmp/checkweave-model-selection/venv/bin/python \
  experiments/backend-selection/run_compare.py \
  --backend gliformer --device cpu --threads 4 \
  --output /tmp/gliformer-cpu.json
```

The GLiNER CUDA command is the same runner with `--device cuda:0`.
The GLiFormer environment has no CUDA torch, so a GPU run is not available
there.
