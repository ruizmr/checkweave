"""SemIf direct option-logit adapter.

Scoring, prompts, and letter slots come from the pinned ``semif_phase1``
package. This module only maps Checkweave questions onto that row schema.
"""

from __future__ import annotations

import os
from dataclasses import dataclass
from pathlib import Path

from .constants import HARD_MAX_INPUT_TOKENS
from .devices import DeviceFailure, StartupError, device_failure_from
from .engine import RawScore, ScoreItem
from .questions import ChoiceQuestion, OrdinalQuestion, PredicateQuestion
from .quiet import library_quiet

SEMIF_CODE_PIN = "1f2dea3e25379f9dfc98cb83c324f00ab5deda37"
SEMIF_MODEL_ID = "Qwen/Qwen3.5-4B"
SEMIF_MODEL_REVISION = "851bf6e806efd8d0a36b00ddf55e13ccb7b8cd0a"
GGUF_REPO = "bartowski/Qwen_Qwen3.5-4B-GGUF"
GGUF_REVISION = "4168f45a16a1290d65a4ec0fa312ae917a4c15d6"
GGUF_FILENAME = "Qwen_Qwen3.5-4B-Q4_K_M.gguf"
GGUF_BYTES = 3_013_027_808
BF16_PARAMETER_COUNT = 4_659_861_248
BF16_WEIGHT_BYTES = BF16_PARAMETER_COUNT * 2
BF16_FIT_MARGIN_BYTES = 1536 * 1024 * 1024
BF16_FIT_BYTES = BF16_WEIGHT_BYTES + BF16_FIT_MARGIN_BYTES
CONTEXT_TOKENS = 4096
ADAPTER_VERSION = "checkweave-semif-1"

# Copied from SemIf benchmarks/build_wanli.py at SEMIF_CODE_PIN.
# Order is Checkweave's pinned order. The benchmark itself permutes order per row.
PREDICATE_OPTIONS = (
    ("supported", "The evidence establishes the claim"),
    ("insufficient", "The evidence does not establish either"),
    ("contradicted", "The evidence establishes the opposite"),
)
PREDICATE_QUESTION_PREFIX = "Assess the claim using only the supplied evidence: "
CHOICE_CRITERION = "Which listed option applies to the evidence?"
ORDINAL_CRITERION = "Which listed level applies to the evidence?"


@dataclass(frozen=True)
class GpuFit:
    device_count: int = 0
    free_bytes: int | None = None
    total_bytes: int | None = None
    bf16_supported: bool = False


@dataclass(frozen=True)
class GpuSample:
    """One GPU from ``nvidia-smi``, before torch is imported."""

    index: int
    free_bytes: int
    compute_major: int


@dataclass(frozen=True)
class SemifSelection:
    precision: str
    device: str
    reason: str | None


def choose_semif_runtime(device_request: str, fit: GpuFit) -> SemifSelection:
    """Pick BF16 or the pinned Q4 GGUF without loading either checkpoint.

    Explicit ``cuda`` fails when BF16 will not fit. ``auto`` records that
    reason and uses llama.cpp instead of trying BF16 and swapping precision
    after an out-of-memory error.
    """
    if device_request == "cpu":
        return SemifSelection("gguf-q4_k_m", "cpu", None)
    if device_request == "mps":
        return SemifSelection("bfloat16", "mps", None)
    fits, why = _bf16_fits(fit)
    if device_request.startswith("cuda"):
        if not fits:
            raise StartupError(
                f"{device_request} cannot run SemIf BF16 ({why}); "
                "Q4 GGUF is not a silent substitute for an explicit GPU request"
            )
        return SemifSelection("bfloat16", device_request, None)
    if device_request != "auto":
        raise StartupError(f"unsupported SemIf device {device_request}")
    if fits:
        return SemifSelection("bfloat16", "cuda:0", None)
    return SemifSelection(
        "gguf-q4_k_m",
        "cpu",
        f"BF16 not selected ({why}); loaded pinned Q4_K_M GGUF on CPU",
    )


def sample_can_hold_bf16(sample: GpuSample) -> bool:
    return sample.compute_major >= 8 and sample.free_bytes >= BF16_FIT_BYTES


def isolate_visible_device(device_request: str, samples: list[GpuSample]) -> str | None:
    """Physical index to expose as the only CUDA device, or None to leave the mapping.

    SemIf BF16 is one visible device, not a multi-GPU load. ``auto`` may narrow
    an existing index list to the first sample that passes the fit gate.
    An explicit ``cuda:N`` is isolated only when that visible index itself
    passes. Another GPU is not substituted. ``None`` on an unfit explicit
    request leaves the mapping so startup can fail instead of loading Q4.
    """
    if not samples or device_request in ("cpu", "mps"):
        return None
    fitting = [sample for sample in samples if sample_can_hold_bf16(sample)]
    if device_request == "auto":
        if len(samples) == 1 or not fitting:
            return None
        return str(fitting[0].index)
    if device_request.startswith("cuda:"):
        ordinal = int(device_request.split(":", 1)[1])
        if ordinal >= len(samples):
            return None
        chosen = samples[ordinal]
        if not sample_can_hold_bf16(chosen) or len(samples) == 1:
            return None
        return str(chosen.index)
    return None


def _bf16_fits(fit: GpuFit) -> tuple[bool, str]:
    if fit.device_count != 1:
        return False, f"{fit.device_count} CUDA devices visible; SemIf BF16 needs exactly one"
    if not fit.bf16_supported:
        return False, "bfloat16 is not supported on the visible GPU"
    if fit.free_bytes is None or fit.free_bytes < BF16_FIT_BYTES:
        free = "unknown" if fit.free_bytes is None else str(fit.free_bytes)
        return False, f"free memory {free} bytes is below the {BF16_FIT_BYTES}-byte BF16 fit gate"
    return True, "one GPU can hold BF16 weights plus the fit margin"


def decision_row(state_id: str, text: str, question) -> dict:
    """Build one SemIf row. ``semif_phase1`` owns the chat template and system string."""
    if isinstance(question, PredicateQuestion):
        options = [
            {"id": option_id, "description": description}
            for option_id, description in PREDICATE_OPTIONS
        ]
        criterion = PREDICATE_QUESTION_PREFIX + question.statement
    elif isinstance(question, ChoiceQuestion):
        options = [_described(label.name, label.description) for label in question.labels]
        criterion = CHOICE_CRITERION
    elif isinstance(question, OrdinalQuestion):
        options = [_described(level.name, level.description) for level in question.levels]
        criterion = ORDINAL_CRITERION
    else:
        raise TypeError(f"unsupported question {type(question).__name__}")
    if len(options) < 2:
        raise ValueError("SemIf direct readout requires at least two options")
    return {
        "id": f"{state_id}/{question.id}",
        "state": text,
        "question": criterion,
        "options": options,
    }


def _described(name: str, description: str | None) -> dict:
    return {"id": name, "description": description if description else name}


class SemifModel:
    """One loaded SemIf backend. Precision does not change after load."""

    supports_predicates = True
    adapter_version = ADAPTER_VERSION
    model_id = SEMIF_MODEL_ID
    revision = SEMIF_MODEL_REVISION

    def __init__(self, backend, tokenizer, metadata: dict, score_fn, precision: str, device: str) -> None:
        self.backend = backend
        self.tokenizer = tokenizer
        self.metadata = metadata
        self.score_fn = score_fn
        self.precision = precision
        self.bound_device = device
        self.parameter_device = device
        self.input_device = device
        self.calls = 0

    def place(self, device: str) -> None:
        if device != self.bound_device:
            raise DeviceFailure(
                device,
                f"loaded precision {self.precision} is bound to {self.bound_device}",
                "unavailable",
            )

    def probe(self) -> None:
        from .questions import Label

        question = ChoiceQuestion(
            "probe",
            (Label("yes", "Yes."), Label("no", "No.")),
        )
        self.classify_batch([ScoreItem("probe", "probe evidence", question)])

    def release_accelerator(self) -> None:
        backend = self.backend
        self.backend = None
        self.tokenizer = None
        self.score_fn = None
        closer = getattr(backend, "close", None)
        if callable(closer):
            try:
                closer()
            except Exception:
                pass
        try:
            import torch
        except ImportError:
            return
        empty = getattr(getattr(torch, "cuda", None), "empty_cache", None)
        if callable(empty):
            try:
                empty()
            except Exception:
                pass

    def option_names(self, question) -> tuple[str, ...] | None:
        if isinstance(question, PredicateQuestion):
            return tuple(option_id for option_id, _description in PREDICATE_OPTIONS)
        return None

    def count_tokens(self, text: str, question) -> int:
        from semif_phase1.direct import encode_prompt

        row = decision_row("count", text, question)
        # The library raises instead of truncating. Count with a ceiling above
        # any frame we accept, then let the engine apply max_input_tokens.
        token_ids, _slots, _digest = encode_prompt(
            self.tokenizer, row, max_tokens=1_000_000_000
        )
        return len(token_ids)

    def classify_batch(self, items: list[ScoreItem]) -> list[RawScore]:
        scored_rows = []
        for item in items:
            row = decision_row(item.state_id, item.text, item.question)
            try:
                with library_quiet():
                    scored = self.score_fn(
                        self.backend,
                        self.tokenizer,
                        row,
                        self.metadata,
                        HARD_MAX_INPUT_TOKENS,
                    )
            except (StartupError, DeviceFailure):
                raise
            except Exception as exc:
                failure = device_failure_from(exc, self.bound_device)
                if failure is None:
                    raise
                raise failure from exc
            self.calls += 1
            option_ids = list(scored["option_ids"])
            probabilities = [float(value) for value in scored["probabilities"]]
            if len(option_ids) != len(probabilities):
                raise RuntimeError("SemIf returned a different number of probabilities than options")
            scores = dict(zip(option_ids, probabilities))
            label = option_ids[0]
            for option_id in option_ids[1:]:
                if scores[option_id] > scores[label]:
                    label = option_id
            scored_rows.append(RawScore(label=label, scores=scores))
        return scored_rows

    def runtime_versions(self) -> dict:
        versions = {
            "semif_code": SEMIF_CODE_PIN,
            "prompt_version": "direct-options-v1",
        }
        for key in ("torch_version", "transformers_version", "llama_cpp_python_version"):
            if key in self.metadata:
                versions[key.replace("_version", "")] = self.metadata[key]
        import importlib.metadata
        import platform

        versions["python"] = platform.python_version()
        for dist in ("torch", "transformers", "llama-cpp-python", "huggingface-hub", "tokenizers"):
            try:
                versions.setdefault(dist, importlib.metadata.version(dist))
            except importlib.metadata.PackageNotFoundError:
                versions.setdefault(dist, None)
        return versions

    def score_semantics(self) -> dict:
        status = "conditional option softmax; uncalibrated as decision confidence"
        if self.precision.startswith("gguf"):
            status = (
                "conditional option score over quantized weights; "
                "uncalibrated as decision confidence; not a BF16 quality measurement"
            )
        return {
            "choice": "exclusive_softmax",
            "ordinal": "exclusive_softmax",
            "ordinal_value": "adapter_derived_expected_level",
            "predicate": "exclusive_softmax_over_supported_insufficient_contradicted",
            "probability_status": status,
        }

    def provenance_extra(self) -> dict:
        gguf = self.metadata.get("gguf")
        return {
            "backend": "semif",
            "code_pin": SEMIF_CODE_PIN,
            "tokenizer_revision": SEMIF_MODEL_REVISION,
            "gguf_revision": GGUF_REVISION if gguf else None,
            "gguf": gguf,
            "prompt_version": "direct-options-v1",
            "readout": self.metadata.get("dtype"),
            "n_gpu_layers": self.metadata.get("n_gpu_layers", 0 if self.precision.startswith("gguf") else None),
            "context_tokens": self.metadata.get("max_prompt_tokens", CONTEXT_TOKENS),
        }


def load_semif_model(selection: SemifSelection, *, offline: bool, threads: int, gguf: str | None) -> SemifModel:
    if selection.precision == "bfloat16":
        return _load_bf16(selection, offline=offline)
    return _load_gguf(selection, offline=offline, threads=threads, gguf=gguf)


def _load_bf16(selection: SemifSelection, *, offline: bool) -> SemifModel:
    from semif_phase1.core import load_causal_model
    from semif_phase1.direct import score

    device = "mps" if selection.device == "mps" else "cuda"
    try:
        with library_quiet():
            backend, tokenizer, metadata = load_causal_model(
                SEMIF_MODEL_ID,
                SEMIF_MODEL_REVISION,
                device=device,
                dtype="bfloat16",
            )
    except (StartupError, DeviceFailure):
        raise
    except Exception as exc:
        failure = device_failure_from(exc, selection.device)
        if failure is None:
            raise
        raise failure from exc
    if offline and metadata.get("revision") != SEMIF_MODEL_REVISION:
        raise StartupError("BF16 load did not keep the pinned tokenizer revision")
    return SemifModel(backend, tokenizer, metadata, score, "bfloat16", selection.device)


def _load_gguf(selection: SemifSelection, *, offline: bool, threads: int, gguf: str | None) -> SemifModel:
    from semif_phase1.llamacpp_backend import load_model, score

    path = Path(gguf) if gguf else resolve_gguf(offline)
    if path.stat().st_size != GGUF_BYTES:
        raise StartupError(
            f"GGUF {path} is {path.stat().st_size} bytes; pinned Q4_K_M is {GGUF_BYTES}"
        )
    try:
        with library_quiet():
            backend, tokenizer, metadata = load_model(
                SEMIF_MODEL_ID,
                SEMIF_MODEL_REVISION,
                path,
                threads=threads,
                context_tokens=CONTEXT_TOKENS,
            )
    except (StartupError, DeviceFailure):
        raise
    except Exception as exc:
        failure = device_failure_from(exc, "cpu")
        if failure is None:
            raise
        raise failure from exc
    recorded = metadata.get("gguf") or {}
    if recorded.get("bytes") != GGUF_BYTES or not recorded.get("sha256"):
        raise StartupError("llama.cpp metadata did not record the GGUF checksum")
    if metadata.get("n_gpu_layers") != 0:
        raise StartupError("CPU GGUF load enabled GPU layers")
    return SemifModel(backend, tokenizer, metadata, score, "gguf-q4_k_m", "cpu")


def resolve_gguf(offline: bool) -> Path:
    from huggingface_hub import hf_hub_download

    path = hf_hub_download(
        repo_id=GGUF_REPO,
        filename=GGUF_FILENAME,
        revision=GGUF_REVISION,
        local_files_only=offline,
    )
    return Path(path)


def nvidia_smi_samples() -> list[GpuSample]:
    """Bounded ``nvidia-smi`` query. Missing or slow output is an empty list."""
    import subprocess

    try:
        completed = subprocess.run(
            [
                "nvidia-smi",
                "--query-gpu=index,memory.free,compute_cap",
                "--format=csv,noheader,nounits",
            ],
            check=False,
            capture_output=True,
            text=True,
            timeout=3,
        )
    except (OSError, subprocess.TimeoutExpired):
        return []
    if completed.returncode != 0:
        return []
    spec = os.environ.get("CUDA_VISIBLE_DEVICES")
    if spec is not None and not cuda_visible_is_index_list(spec):
        return []
    samples = parse_nvidia_smi_csv(completed.stdout)
    return filter_cuda_visible_devices(samples, spec)


def cuda_visible_is_index_list(spec: str) -> bool:
    text = spec.strip()
    if text in ("", "-1"):
        return True
    return all(part.strip().isdigit() for part in text.split(",") if part.strip())


def parse_nvidia_smi_csv(text: str) -> list[GpuSample]:
    samples: list[GpuSample] = []
    for line in text.splitlines():
        parts = [part.strip() for part in line.split(",")]
        if len(parts) != 3 or not parts[0]:
            continue
        try:
            index = int(parts[0])
            free_mib = int(float(parts[1]))
            major = int(parts[2].split(".", 1)[0])
        except ValueError:
            continue
        samples.append(GpuSample(index, free_mib * 1024 * 1024, major))
    return samples


def filter_cuda_visible_devices(samples: list[GpuSample], spec: str | None) -> list[GpuSample]:
    if spec is None:
        return samples
    if spec.strip() in ("", "-1"):
        return []
    ordered: list[GpuSample] = []
    by_index = {sample.index: sample for sample in samples}
    for part in spec.split(","):
        part = part.strip()
        if not part or not part.isdigit():
            continue
        sample = by_index.get(int(part))
        if sample is not None:
            ordered.append(sample)
    return ordered


def gpu_fit_from_torch(torch_mod) -> GpuFit:
    if not bool(torch_mod.cuda.is_available()):
        return GpuFit()
    count = int(torch_mod.cuda.device_count())
    free = total = None
    bf16 = False
    if count:
        try:
            free, total = torch_mod.cuda.mem_get_info(0)
        except Exception:
            free, total = None, None
        checker = getattr(torch_mod.cuda, "is_bf16_supported", None)
        if checker is not None:
            try:
                bf16 = bool(checker())
            except Exception:
                bf16 = False
    return GpuFit(
        device_count=count,
        free_bytes=None if free is None else int(free),
        total_bytes=None if total is None else int(total),
        bf16_supported=bf16,
    )
