"""GLiNER2.5 classifier backend. Imports torch only when the model is loaded."""

from __future__ import annotations

import importlib.metadata
import platform

from .constants import (
    MODEL_ID,
    MODEL_REVISION,
    PRECISION,
    UPSTREAM_BATCH_SIZE,
)
from .devices import DeviceFailure, accelerator_failure_kind
from .engine import RawScore, ScoreItem
from .questions import ChoiceQuestion, OrdinalQuestion
from .quiet import library_quiet

_CHECKPOINT_PATTERNS = ["*.json", "*.safetensors", "*.model", "*.txt"]
_PROBE_TEXT = "The documentation describes the public interface."
_RUNTIME_PACKAGES = (
    "torch",
    "transformers",
    "gliner2",
    "huggingface-hub",
    "tokenizers",
    "safetensors",
    "numpy",
    "pydantic",
)


def checkpoint_download_kwargs(offline: bool) -> dict:
    """Pinned local snapshot. Offline never reaches the network."""
    return {
        "repo_id": MODEL_ID,
        "revision": MODEL_REVISION,
        "allow_patterns": list(_CHECKPOINT_PATTERNS),
        "local_files_only": bool(offline),
    }


def torch_inventory(torch_mod):
    from .devices import AcceleratorInventory

    cuda_count = 0
    rocm = False
    if bool(torch_mod.cuda.is_available()):
        cuda_count = int(torch_mod.cuda.device_count())
        rocm = bool(getattr(torch_mod.version, "hip", None))
    mps = False
    mps_backend = getattr(getattr(torch_mod, "backends", None), "mps", None)
    if mps_backend is not None:
        try:
            mps = bool(mps_backend.is_available())
        except Exception:
            mps = False
    return AcceleratorInventory(
        cuda_device_count=cuda_count,
        rocm=rocm,
        mps=mps,
    )


def configure_torch(torch_mod, threads: int) -> None:
    torch_mod.set_num_threads(threads)
    try:
        torch_mod.set_num_interop_threads(1)
    except RuntimeError:
        pass
    set_precision = getattr(torch_mod, "set_float32_matmul_precision", None)
    if set_precision is not None:
        set_precision("highest")
    cuda_backend = getattr(getattr(torch_mod, "backends", None), "cuda", None)
    matmul = getattr(cuda_backend, "matmul", None)
    if matmul is not None and hasattr(matmul, "allow_tf32"):
        matmul.allow_tf32 = False
    cudnn = getattr(getattr(torch_mod, "backends", None), "cudnn", None)
    if cudnn is not None and hasattr(cudnn, "allow_tf32"):
        cudnn.allow_tf32 = False


def runtime_versions() -> dict:
    versions = {"python": platform.python_version()}
    for name in _RUNTIME_PACKAGES:
        try:
            versions[name] = importlib.metadata.version(name)
        except importlib.metadata.PackageNotFoundError:
            versions[name] = None
    return versions


class GlinerModel:
    """One Classifier. Parameters and inputs share the selected device."""

    def __init__(self, classifier, torch_mod) -> None:
        self.classifier = classifier
        self.torch = torch_mod
        self.parameter_device = "cpu"
        self.input_device = "cpu"
        self.precision = PRECISION

    def place(self, device: str) -> None:
        def move():
            self.classifier.to(device=device, dtype=self.torch.float32).eval()

        self._guard(move, device)
        parameter = next(self.classifier.model.parameters())
        actual = self.torch.device(parameter.device)
        wanted = self.torch.device(device)
        if actual.type != wanted.type or (
            wanted.index is not None and actual.index != wanted.index
        ):
            raise RuntimeError(
                f"parameters remained on {actual}, expected {wanted}"
            )
        if parameter.dtype != self.torch.float32:
            raise RuntimeError(f"parameters are {parameter.dtype}, expected float32")
        placed_inputs = self.torch.device(self.classifier.device)
        if placed_inputs.type != wanted.type or (
            wanted.index is not None and placed_inputs.index not in (None, wanted.index)
        ):
            raise RuntimeError(
                f"input placement is {placed_inputs}, expected {wanted}"
            )
        self.parameter_device = str(actual)
        self.input_device = str(placed_inputs)
        self.precision = "float32"

    def probe(self) -> None:
        from .questions import Label

        question = ChoiceQuestion(
            id="probe",
            labels=(
                Label("documentation", "The text is about documentation."),
                Label("behavior", "The text is about runtime behavior."),
            ),
        )
        self.classify_batch(
            [ScoreItem("probe", _PROBE_TEXT, question)]
        )

    def release_accelerator(self) -> None:
        empty_cache = getattr(getattr(self.torch, "cuda", None), "empty_cache", None)
        if empty_cache is not None:
            try:
                empty_cache()
            except Exception:
                pass
        mps = getattr(self.torch, "mps", None)
        mps_empty = getattr(mps, "empty_cache", None)
        if mps_empty is not None:
            try:
                mps_empty()
            except Exception:
                pass

    def count_tokens(self, text: str, question) -> int:
        schema = self._schema(question)
        compiled = self.classifier.compile_schema(schema)

        def count():
            batch = self.classifier.model.processor.collate_fn_inference(
                [(text, compiled.build())],
                max_len=None,
                error_policy="raise",
            )
            if not batch.original_lengths:
                raise RuntimeError("tokenizer produced an empty batch")
            return int(batch.original_lengths[0])

        return self._guard(count, self.parameter_device)

    def classify_batch(self, items: list[ScoreItem]) -> list[RawScore]:
        if not items:
            return []
        from gliner2.classification import ClassificationConfig

        compiled = [
            self.classifier.compile_schema(self._schema(item.question)) for item in items
        ]
        texts = [item.text for item in items]
        config = ClassificationConfig(batch_size=UPSTREAM_BATCH_SIZE, max_len=None)

        def forward():
            scores = self.classifier.scorer.batch_score(
                texts,
                compiled,
                batch_size=UPSTREAM_BATCH_SIZE,
                max_len=None,
            )
            return [
                self.classifier.decode(score, schema, config=config)
                for score, schema in zip(scores, compiled)
            ]

        decoded = self._guard(forward, self.parameter_device)
        results = []
        for item, result in zip(items, decoded):
            task = item.question.id
            probabilities = result.probabilities(task)
            label = result.value(task)
            results.append(
                RawScore(
                    label=label if isinstance(label, str) else None,
                    scores={name: float(value) for name, value in probabilities.items()},
                )
            )
        return results

    def runtime_versions(self) -> dict:
        return runtime_versions()

    def _schema(self, question):
        from gliner2.classification import ClassificationSchema

        labels = _label_argument(question)
        if isinstance(question, ChoiceQuestion):
            return ClassificationSchema().single(
                question.id, labels, activation="softmax"
            )
        if isinstance(question, OrdinalQuestion):
            return ClassificationSchema().ordinal(
                question.id, labels, activation="softmax"
            )
        raise TypeError(f"unsupported model question {type(question).__name__}")

    def _guard(self, operation, device: str):
        try:
            with library_quiet():
                return operation()
        except DeviceFailure:
            raise
        except Exception as exc:
            kind = accelerator_failure_kind(exc)
            if kind is None:
                raise
            raise DeviceFailure(device, str(exc), kind) from exc


def load_gliner_model(offline: bool, threads: int) -> GlinerModel:
    import torch
    from gliner2.classification import Classifier
    from huggingface_hub import snapshot_download

    configure_torch(torch, threads)
    with library_quiet():
        checkpoint = snapshot_download(**checkpoint_download_kwargs(offline))
        classifier = Classifier.from_pretrained(checkpoint)
        classifier.to(device="cpu", dtype=torch.float32).eval()
    return GlinerModel(classifier, torch)


def _label_argument(question) -> list[str] | dict[str, str | None]:
    rows = question.labels if isinstance(question, ChoiceQuestion) else question.levels
    if all(row.description is None for row in rows):
        return [row.name for row in rows]
    return {row.name: row.description for row in rows}
