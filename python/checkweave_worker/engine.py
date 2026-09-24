"""Load the pinned checkpoint once and evaluate independent questions."""

from __future__ import annotations

import math
from dataclasses import dataclass

from checkweave_worker import ADAPTER_VERSION

from .constants import (
    ARGMAX_TOLERANCE,
    INPUT_POLICY,
    LANGUAGE,
    MODEL_ID,
    MODEL_REVISION,
    PREDICATE_REASON,
    PROTOCOL_VERSION,
    PROVIDER,
    SCORE_TOLERANCE,
)
from .devices import (
    AcceleratorInventory,
    DeviceFailure,
    StartupError,
    accelerator_label,
    candidate_devices,
    require_device_present,
)
from .questions import (
    ChoiceQuestion,
    EvaluateRequest,
    OrdinalQuestion,
    PredicateQuestion,
    State,
    label_names,
)


@dataclass(frozen=True)
class ScoreItem:
    state_id: str
    text: str
    question: ChoiceQuestion | OrdinalQuestion | PredicateQuestion


@dataclass(frozen=True)
class RawScore:
    label: str | None
    scores: dict[str, float] | None


class Engine:
    """Reuse one loaded model across batches. Auto GPU failure stays on CPU."""

    def __init__(
        self,
        *,
        device_request: str,
        threads: int,
        offline: bool,
        inventory: AcceleratorInventory,
        loader,
        forced_device: str | None = None,
        fallback_reason: str | None = None,
        cpu_loader=None,
        cuda_visible_devices: str | None = None,
    ) -> None:
        self.device_request = device_request
        self.threads = threads
        self.offline = offline
        self.inventory = inventory
        self.loader = loader
        self.cpu_loader = cpu_loader
        self.cuda_visible_devices = cuda_visible_devices
        self.model = None
        self.selected_device: str | None = None
        self.fallback_reason: str | None = fallback_reason
        self.fell_back = bool(fallback_reason)
        self.forced_device = forced_device
        self._started = False

    def start(self) -> None:
        if self._started:
            return
        try:
            self.model = self.loader()
        except (StartupError, DeviceFailure) as exc:
            if self._cpu_replacement_allowed("accelerator"):
                self._replace_with_cpu(str(exc))
                return
            raise
        errors: list[str] = []
        if self.forced_device:
            candidates = [self.forced_device]
        else:
            candidates = candidate_devices(self.device_request, self.inventory)
        for index, device in enumerate(candidates):
            try:
                require_device_present(device, self.inventory)
                self.model.place(device)
                self.model.probe()
            except DeviceFailure as exc:
                errors.append(str(exc))
                if self._cpu_replacement_allowed(device):
                    self._replace_with_cpu("; ".join(errors))
                    return
                if len(candidates) == 1 or index == len(candidates) - 1:
                    raise StartupError("; ".join(errors)) from exc
                try:
                    self.model.place("cpu")
                    self.model.release_accelerator()
                except DeviceFailure as recovery:
                    errors.append(str(recovery))
                    raise StartupError("; ".join(errors)) from recovery
                continue
            self.selected_device = device
            self._started = True
            if errors:
                self.fell_back = True
                self.fallback_reason = (
                    "; ".join(errors) + f"; retained {device} after a bounded fallback"
                )
            return
        raise StartupError("no device candidates")

    def provenance(self) -> dict:
        if not self._started or self.model is None or self.selected_device is None:
            raise StartupError("model is not loaded")
        semantics = {
            "choice": "exclusive_softmax",
            "ordinal": "exclusive_softmax",
            "ordinal_value": "adapter_derived_expected_level",
        }
        reporter = getattr(self.model, "score_semantics", None)
        if reporter is not None:
            semantics = reporter() if callable(reporter) else reporter
        payload = {
            "provider": PROVIDER,
            "model": getattr(self.model, "model_id", MODEL_ID),
            "revision": getattr(self.model, "revision", MODEL_REVISION),
            "adapter_version": getattr(self.model, "adapter_version", ADAPTER_VERSION),
            "device": self.selected_device,
            "precision": self.model.precision,
            "score_semantics": semantics,
            "input_policy": INPUT_POLICY,
            "runtime_versions": self.model.runtime_versions(),
            "device_requested": self.device_request,
            "device_fallback": self.fallback_reason,
            "accelerator": accelerator_label(self.selected_device, self.inventory),
            "parameter_device": self.model.parameter_device,
            "input_device": self.model.input_device,
            "threads": self.threads,
            "offline": self.offline,
            "language": LANGUAGE,
        }
        if self.cuda_visible_devices is not None:
            payload["cuda_visible_devices"] = self.cuda_visible_devices
        extra = getattr(self.model, "provenance_extra", None)
        if extra is not None:
            payload.update(extra() if callable(extra) else extra)
        return payload

    def evaluate(self, request: EvaluateRequest) -> dict:
        self.start()
        results: list[dict | None] = []
        pending: list[tuple[int, ScoreItem]] = []
        for state in request.states:
            for question in request.questions:
                ready = self._without_model(state, question, request.max_input_tokens)
                if ready is not None:
                    results.append(ready)
                    continue
                results.append(None)
                pending.append(
                    (
                        len(results) - 1,
                        ScoreItem(state.id, state.text, question),
                    )
                )
        if pending:
            scored = self._run_model(
                lambda: self.model.classify_batch([item for _, item in pending])
            )
            if len(scored) != len(pending):
                raise RuntimeError("model batch returned a different number of rows")
            for (index, item), raw in zip(pending, scored):
                results[index] = self._interpret(item, raw)
        return {
            "version": PROTOCOL_VERSION,
            "id": request.id,
            "results": results,
            "provenance": self.provenance(),
        }

    def _without_model(
        self,
        state: State,
        question,
        max_input_tokens: int,
    ) -> dict | None:
        if isinstance(question, PredicateQuestion) and not getattr(
            self.model, "supports_predicates", False
        ):
            return _row(
                state.id,
                question.id,
                status="unsupported",
                reason=PREDICATE_REASON,
            )
        if not state.text.strip():
            return _row(
                state.id,
                question.id,
                status="unresolved",
                reason="empty text; rejected with no truncation; omitted nothing",
                omitted=True,
            )
        count = self._run_model(lambda: self.model.count_tokens(state.text, question))
        if count > max_input_tokens:
            return _row(
                state.id,
                question.id,
                status="unresolved",
                reason=(
                    f"serialized input is {count} tokens, over the limit of "
                    f"{max_input_tokens}; rejected with no truncation; omitted nothing"
                ),
                omitted=True,
                token_count=count,
            )
        return None

    def _interpret(self, item: ScoreItem, raw: RawScore) -> dict:
        names = self._option_names(item.question)
        if raw.label is None or raw.scores is None:
            return _row(
                item.state_id,
                item.question.id,
                status="unresolved",
                reason="model returned no label distribution",
            )
        if set(raw.scores) != set(names):
            return _row(
                item.state_id,
                item.question.id,
                status="unresolved",
                reason="score labels do not match the question",
            )
        scores: dict[str, float] = {}
        for name in names:
            value = raw.scores[name]
            if isinstance(value, bool) or not isinstance(value, (int, float)):
                return _row(
                    item.state_id,
                    item.question.id,
                    status="unresolved",
                    reason="score is not a finite number",
                )
            number = float(value)
            if not math.isfinite(number) or number < 0:
                return _row(
                    item.state_id,
                    item.question.id,
                    status="unresolved",
                    reason="score is not a finite non-negative number",
                )
            scores[name] = number
        total = math.fsum(scores.values())
        normalized = abs(total - 1.0) <= SCORE_TOLERANCE
        semantics = "exclusive_softmax" if normalized else "unnormalized"
        maximum = max(scores.values())
        winners = [
            name for name in names if scores[name] >= maximum - ARGMAX_TOLERANCE
        ]
        if raw.label not in winners:
            return {
                "state_id": item.state_id,
                "question_id": item.question.id,
                "status": "unresolved",
                "label": None,
                "value": None,
                "scores": scores,
                "score_semantics": semantics,
                "reason": "decoder label disagrees with the score distribution",
            }
        result = {
            "state_id": item.state_id,
            "question_id": item.question.id,
            "status": "resolved",
            "label": raw.label,
            "value": None,
            "scores": scores,
            "score_semantics": semantics,
            "reason": None,
        }
        if isinstance(item.question, PredicateQuestion):
            return _predicate_result(item, result)
        if isinstance(item.question, OrdinalQuestion):
            if normalized:
                result["value"] = math.fsum(
                    scores[level.name] * level.value for level in item.question.levels
                )
                result["value_derived"] = True
                result["value_semantics"] = "adapter_derived_expected_level"
            else:
                result["value_derived"] = False
                result["reason"] = (
                    "ordinal expected value was not derived because scores are "
                    "not a normalized distribution"
                )
        return result

    def _option_names(self, question) -> tuple[str, ...]:
        namer = getattr(self.model, "option_names", None)
        if namer is not None:
            named = namer(question)
            if named is not None:
                return tuple(named)
        return label_names(question)

    def _run_model(self, operation):
        try:
            return operation()
        except DeviceFailure as exc:
            if not self._allow_fallback():
                raise
            self._move_cpu(str(exc))
            return operation()

    def _allow_fallback(self) -> bool:
        return (
            self.device_request == "auto"
            and self.selected_device not in (None, "cpu")
            and not self.fell_back
        )

    def _cpu_replacement_allowed(self, device: str) -> bool:
        return (
            self.cpu_loader is not None
            and self.device_request == "auto"
            and device != "cpu"
        )

    def _replace_with_cpu(self, reason: str) -> None:
        """Drop the accelerator model and construct a new CPU backend.

        SemIf BF16 weights are bound to the device they were loaded on.
        ``place("cpu")`` would keep that precision and is not a Q4 load.
        """
        loader = self.cpu_loader
        self.cpu_loader = None
        failed = self.model
        self.model = None
        if failed is not None:
            release = getattr(failed, "release_accelerator", None)
            if release is not None:
                try:
                    release()
                except Exception:
                    pass
        if loader is None:
            raise StartupError(reason)
        try:
            replacement = loader()
            replacement.place("cpu")
            replacement.probe()
        except (StartupError, DeviceFailure) as exc:
            raise StartupError(f"{reason}; CPU backend failed: {exc}") from exc
        self.model = replacement
        self.selected_device = "cpu"
        self.fell_back = True
        self.fallback_reason = (
            f"{reason}; unloaded the accelerator model and loaded a new CPU backend"
        )
        self._started = True

    def _move_cpu(self, reason: str) -> None:
        if self._cpu_replacement_allowed(self.selected_device or "accelerator"):
            self._replace_with_cpu(reason)
            return
        self.model.place("cpu")
        self.model.release_accelerator()
        self.selected_device = "cpu"
        self.fell_back = True
        self.fallback_reason = f"{reason}; bounded CPU retry retained cpu"


def _predicate_result(item: ScoreItem, result: dict) -> dict:
    label = result["label"]
    if label == "insufficient":
        result["status"] = "unresolved"
        result["value"] = None
        result["reason"] = (
            "explicit insufficient option selected; a low score on another "
            "option is not used as a proxy; scores are not calibrated"
        )
    elif label == "supported":
        result["value"] = True
    elif label == "contradicted":
        result["value"] = False
    else:
        result["status"] = "unresolved"
        result["value"] = None
        result["reason"] = f"predicate label {label!r} is outside the pinned option set"
    del item
    return result


def _row(
    state_id: str,
    question_id: str,
    *,
    status: str,
    reason: str,
    omitted: bool = False,
    token_count: int | None = None,
) -> dict:
    row = {
        "state_id": state_id,
        "question_id": question_id,
        "status": status,
        "label": None,
        "value": None,
        "scores": None,
        "reason": reason,
    }
    if omitted:
        row["omitted"] = None
    if token_count is not None:
        row["token_count"] = token_count
    return row
