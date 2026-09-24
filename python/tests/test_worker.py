"""Fake-model tests for protocol, batching, limits, and device fallback."""

from __future__ import annotations

import io
import json
import sys
import unittest
from pathlib import Path

from checkweave_worker.constants import MAX_FRAME_BYTES, MODEL_REVISION
from checkweave_worker.devices import (
    AcceleratorInventory,
    DeviceFailure,
    StartupError,
    candidate_devices,
)
from checkweave_worker.engine import Engine, RawScore
from checkweave_worker.frames import FrameReader, FrameTooLarge
from checkweave_worker.gliner_backend import checkpoint_download_kwargs
from checkweave_worker.questions import ChoiceQuestion, OrdinalQuestion
from checkweave_worker.stdio import WorkerLoop


class FakeModel:
    def __init__(self) -> None:
        self.parameter_device = "cpu"
        self.input_device = "cpu"
        self.precision = "float32"
        self.placed: list[str] = []
        self.probes: list[str] = []
        self.batches: list[list[tuple[str, str, tuple[str, ...]]]] = []
        self.fail_place: set[str] = set()
        self.fail_probe: set[str] = set()
        self.fail_classify = 0
        self.released = 0
        self.scores = None

    def place(self, device: str) -> None:
        self.placed.append(device)
        if device in self.fail_place:
            raise DeviceFailure(device, "out of memory", "oom")
        self.parameter_device = device
        self.input_device = device

    def probe(self) -> None:
        self.probes.append(self.parameter_device)
        if self.parameter_device in self.fail_probe:
            raise DeviceFailure(self.parameter_device, "unsupported kernel", "unsupported")

    def release_accelerator(self) -> None:
        self.released += 1

    def count_tokens(self, text: str, question) -> int:
        del question
        if text.startswith("LONG"):
            return 5000
        return 12

    def classify_batch(self, items):
        if self.fail_classify:
            self.fail_classify -= 1
            raise DeviceFailure(self.parameter_device, "out of memory", "oom")
        recorded = []
        results = []
        for item in items:
            names = _names(item.question)
            recorded.append((item.text, item.question.id, names))
            if self.scores is not None:
                label, scores = self.scores(item)
            elif "RIGHT" in item.text:
                scores = {name: 0.0 for name in names}
                scores[names[-1]] = 1.0
                label = names[-1]
            else:
                scores = {name: 0.0 for name in names}
                scores[names[0]] = 1.0
                label = names[0]
            results.append(RawScore(label=label, scores=dict(scores)))
        self.batches.append(recorded)
        return results

    def runtime_versions(self) -> dict:
        return {"python": "3.12.9", "torch": "2.4.0", "gliner2": "2.0.0"}


def _names(question) -> tuple[str, ...]:
    if isinstance(question, ChoiceQuestion):
        return tuple(label.name for label in question.labels)
    if isinstance(question, OrdinalQuestion):
        return tuple(level.name for level in question.levels)
    if question.__class__.__name__ == "PredicateQuestion":
        return ("supported", "insufficient", "contradicted")
    raise AssertionError(type(question))


def _engine(model: FakeModel, **kwargs) -> Engine:
    defaults = dict(
        device_request="cpu",
        threads=4,
        offline=True,
        inventory=AcceleratorInventory(),
        loader=lambda: model,
    )
    defaults.update(kwargs)
    return Engine(**defaults)


def _choice(question_id="scope", labels=("documentation", "behavior")) -> dict:
    return {
        "kind": "choice",
        "id": question_id,
        "labels": [
            {"label": label, "description": f"means {label}"} for label in labels
        ],
    }


def _ordinal() -> dict:
    return {
        "kind": "ordinal",
        "id": "urgency",
        "levels": [
            {"label": "low", "description": "can wait", "value": 0},
            {"label": "high", "description": "immediate", "value": 10},
        ],
    }


def _evaluate(states, questions, **extra) -> dict:
    payload = {
        "version": 1,
        "id": extra.pop("id", "req-1"),
        "op": "evaluate",
        "states": states,
        "questions": questions,
    }
    payload.update(extra)
    return payload


def _run(engine: Engine, raw: bytes) -> list[dict]:
    stdout = io.BytesIO()
    loop = WorkerLoop(engine, io.BytesIO(raw), stdout)
    loop.run()
    return [json.loads(line) for line in stdout.getvalue().splitlines() if line]


class ProtocolTests(unittest.TestCase):
    def test_malformed_frame_recovers_and_keeps_request_id(self) -> None:
        model = FakeModel()
        engine = _engine(model)
        good = _evaluate(
            [{"id": "row", "text": "A documentation comment."}],
            [_choice()],
            id="kept",
        )
        raw = b"\n{not json\n" + json.dumps(good).encode() + b"\n"
        messages = _run(engine, raw)
        self.assertEqual(messages[0]["error"], "empty frame")
        self.assertNotIn("id", messages[0])
        self.assertEqual(messages[1]["error"], "malformed json")
        self.assertEqual(messages[2]["id"], "kept")
        self.assertEqual(messages[2]["results"][0]["status"], "resolved")

    def test_schema_errors_do_not_call_the_model(self) -> None:
        model = FakeModel()
        engine = _engine(model)
        cases = [
            _evaluate([{"id": "row", "text": "x"}], [{"kind": "choice", "id": "q", "labels": []}]),
            _evaluate(
                [{"id": "row", "text": "x"}],
                [{"kind": "choice", "id": "q(1)", "labels": [{"label": "a"}, {"label": "b"}]}],
            ),
            _evaluate(
                [{"id": "row", "text": "x"}, {"id": "row", "text": "y"}],
                [_choice()],
            ),
            _evaluate(
                [{"id": "row", "text": "x"}],
                [_ordinal() | {"levels": [{"label": "only", "value": 1}]}],
            ),
            {"version": 2, "id": "old", "op": "evaluate"},
        ]
        for payload in cases:
            message = _run(engine, json.dumps(payload).encode() + b"\n")[0]
            self.assertIn("error", message)
        self.assertEqual(model.batches, [])

    def test_predicate_is_unsupported_without_a_score(self) -> None:
        model = FakeModel()
        engine = _engine(model)
        payload = _evaluate(
            [{"id": "row", "text": "Tests passed on Linux."}],
            [
                _choice(),
                {
                    "kind": "predicate",
                    "id": "supported",
                    "statement": "The tests passed on Windows.",
                },
            ],
        )
        message = _run(engine, json.dumps(payload).encode() + b"\n")[0]
        rows = {(row["question_id"]): row for row in message["results"]}
        self.assertEqual(rows["scope"]["status"], "resolved")
        self.assertIsNone(rows["scope"]["value"])
        self.assertEqual(rows["supported"]["status"], "unsupported")
        self.assertIsNone(rows["supported"]["scores"])
        self.assertIn("missing evidence", rows["supported"]["reason"])
        self.assertEqual(len(model.batches), 1)
        self.assertEqual(model.batches[0][0][1], "scope")

    def test_over_limit_rejects_without_omitting_a_span(self) -> None:
        model = FakeModel()
        engine = _engine(model)
        payload = _evaluate(
            [
                {"id": "short", "text": "A documentation comment."},
                {"id": "long", "text": "LONG " + ("token " * 50)},
            ],
            [_choice()],
            max_input_tokens=4096,
        )
        message = _run(engine, json.dumps(payload).encode() + b"\n")[0]
        rows = {row["state_id"]: row for row in message["results"]}
        self.assertEqual(rows["short"]["status"], "resolved")
        self.assertEqual(rows["long"]["status"], "unresolved")
        self.assertIsNone(rows["long"]["omitted"])
        self.assertIsNone(rows["long"]["label"])
        self.assertIsNone(rows["long"]["scores"])
        self.assertIn("omitted nothing", rows["long"]["reason"])
        self.assertEqual(rows["long"]["token_count"], 5000)
        self.assertEqual([item[0] for item in model.batches[0]], ["A documentation comment."])

    def test_batch_keeps_independent_labels(self) -> None:
        model = FakeModel()
        engine = _engine(model)
        payload = _evaluate(
            [
                {"id": "left", "text": "The change is documentation."},
                {"id": "right", "text": "RIGHT behavior change."},
            ],
            [_choice(), _ordinal()],
        )
        message = _run(engine, json.dumps(payload).encode() + b"\n")[0]
        self.assertEqual(len(model.batches), 1)
        self.assertEqual(len(model.batches[0]), 4)
        rows = {(row["state_id"], row["question_id"]): row for row in message["results"]}
        self.assertEqual(set(rows[("left", "scope")]["scores"]), {"documentation", "behavior"})
        self.assertEqual(rows[("left", "scope")]["label"], "documentation")
        self.assertEqual(rows[("right", "scope")]["label"], "behavior")
        self.assertEqual(set(rows[("left", "urgency")]["scores"]), {"low", "high"})
        self.assertEqual(rows[("left", "urgency")]["score_semantics"], "exclusive_softmax")
        self.assertTrue(rows[("left", "urgency")]["value_derived"])
        self.assertEqual(rows[("left", "urgency")]["value"], 0.0)
        self.assertEqual(rows[("right", "urgency")]["label"], "high")
        self.assertEqual(rows[("right", "urgency")]["value"], 10.0)
        self.assertEqual(message["provenance"]["revision"], MODEL_REVISION)
        self.assertEqual(message["provenance"]["precision"], "float32")

    def test_unnormalized_ordinal_does_not_invent_an_expected_value(self) -> None:
        model = FakeModel()

        def scores(item):
            names = _names(item.question)
            return names[0], {name: 2.0 for name in names}

        model.scores = scores
        engine = _engine(model)
        payload = _evaluate([{"id": "row", "text": "Soon."}], [_ordinal()])
        row = _run(engine, json.dumps(payload).encode() + b"\n")[0]["results"][0]
        self.assertEqual(row["status"], "resolved")
        self.assertEqual(row["score_semantics"], "unnormalized")
        self.assertIsNone(row["value"])
        self.assertFalse(row["value_derived"])
        self.assertIn("not derived", row["reason"])

    def test_decoder_disagreement_stays_unresolved(self) -> None:
        model = FakeModel()

        def scores(item):
            return "low", {"low": 0.1, "high": 0.9}

        model.scores = scores
        engine = _engine(model)
        payload = _evaluate([{"id": "row", "text": "Soon."}], [_ordinal()])
        row = _run(engine, json.dumps(payload).encode() + b"\n")[0]["results"][0]
        self.assertEqual(row["status"], "unresolved")
        self.assertIsNone(row["label"])
        self.assertEqual(row["scores"]["high"], 0.9)

    def test_frame_over_8mib_is_rejected_and_the_next_line_runs(self) -> None:
        model = FakeModel()
        engine = _engine(model)
        shutdown = json.dumps({"version": 1, "id": "stop", "op": "shutdown"}).encode()
        raw = b"{" + (b"a" * (MAX_FRAME_BYTES + 4)) + b"\n" + shutdown + b"\n"
        messages = _run(engine, raw)
        self.assertIn("8388608", messages[0]["error"])
        self.assertEqual(messages[1]["shutdown"], True)
        self.assertEqual(model.batches, [])

    def test_loader_runs_once_across_batches(self) -> None:
        model = FakeModel()
        loads = []

        def loader():
            loads.append(1)
            return model

        engine = _engine(model, loader=loader)
        payload = _evaluate([{"id": "row", "text": "A note."}], [_choice()])
        raw = json.dumps(payload).encode() + b"\n" + json.dumps(payload | {"id": "req-2"}).encode() + b"\n"
        messages = _run(engine, raw)
        self.assertEqual(loads, [1])
        self.assertEqual([message["id"] for message in messages], ["req-1", "req-2"])


class DeviceTests(unittest.TestCase):
    def test_auto_order_is_cuda_then_mps_then_cpu(self) -> None:
        self.assertEqual(
            candidate_devices("auto", AcceleratorInventory(cuda_device_count=1, mps=True)),
            ["cuda:0", "mps", "cpu"],
        )
        self.assertEqual(
            candidate_devices("auto", AcceleratorInventory(cuda_device_count=1, rocm=True)),
            ["cuda:0", "cpu"],
        )
        self.assertEqual(candidate_devices("cuda", AcceleratorInventory()), ["cuda:0"])

    def test_explicit_cuda_failure_is_visible(self) -> None:
        model = FakeModel()
        model.fail_place.add("cuda:0")
        engine = _engine(
            model,
            device_request="cuda:0",
            inventory=AcceleratorInventory(cuda_device_count=1),
        )
        with self.assertRaises(StartupError) as caught:
            engine.start()
        self.assertIn("cuda:0", str(caught.exception))
        self.assertFalse(engine.fell_back)
        self.assertNotIn("retained", str(caught.exception))

    def test_auto_oom_falls_back_to_cpu_and_records_provenance(self) -> None:
        model = FakeModel()
        model.fail_probe.add("cuda:0")
        engine = _engine(
            model,
            device_request="auto",
            inventory=AcceleratorInventory(cuda_device_count=1),
        )
        engine.start()
        provenance = engine.provenance()
        self.assertEqual(provenance["device"], "cpu")
        self.assertEqual(provenance["precision"], "float32")
        self.assertEqual(provenance["parameter_device"], "cpu")
        self.assertEqual(provenance["input_device"], "cpu")
        self.assertIn("cuda:0", provenance["device_fallback"])
        self.assertTrue(engine.fell_back)
        self.assertGreaterEqual(model.released, 1)

    def test_later_gpu_error_retries_once_and_retains_cpu(self) -> None:
        model = FakeModel()
        model.fail_classify = 1
        engine = _engine(
            model,
            device_request="auto",
            inventory=AcceleratorInventory(cuda_device_count=1),
        )
        payload = _evaluate([{"id": "row", "text": "A note."}], [_choice()])
        first = _run(engine, json.dumps(payload).encode() + b"\n")[0]
        self.assertEqual(first["results"][0]["status"], "resolved")
        self.assertEqual(first["provenance"]["device"], "cpu")
        self.assertIn("retained cpu", first["provenance"]["device_fallback"])
        second = _run(engine, json.dumps(payload | {"id": "again"}).encode() + b"\n")[0]
        self.assertEqual(second["provenance"]["device"], "cpu")
        self.assertEqual(model.fail_classify, 0)
        self.assertEqual(model.parameter_device, "cpu")
        self.assertGreaterEqual(len(model.batches), 2)

    def test_checkpoint_pin_and_offline_flag(self) -> None:
        online = checkpoint_download_kwargs(False)
        offline = checkpoint_download_kwargs(True)
        self.assertEqual(online["revision"], MODEL_REVISION)
        self.assertEqual(online["repo_id"], "fastino/gliner2.5-base-v1")
        self.assertFalse(online["local_files_only"])
        self.assertTrue(offline["local_files_only"])

    def test_package_does_not_call_a_hosted_inference_api(self) -> None:
        root = Path(__file__).resolve().parents[1] / "checkweave_worker"
        text = "\n".join(path.read_text() for path in root.rglob("*.py"))
        for host in ("typesafe.ai", "api.openai.com", "generativelanguage.googleapis.com"):
            self.assertNotIn(host, text)


class SemifContractTests(unittest.TestCase):
    def test_m10_shaped_auto_selects_q4_without_claiming_bf16(self) -> None:
        from checkweave_worker.semif_backend import (
            BF16_FIT_BYTES,
            GpuFit,
            StartupError,
            choose_semif_runtime,
            decision_row,
        )
        from checkweave_worker.questions import PredicateQuestion

        fit = GpuFit(device_count=4, free_bytes=7 * 1024**3, total_bytes=8 * 1024**3, bf16_supported=False)
        selected = choose_semif_runtime("auto", fit)
        self.assertEqual(selected.precision, "gguf-q4_k_m")
        self.assertEqual(selected.device, "cpu")
        self.assertIn("Q4_K_M", selected.reason)
        self.assertNotIn("bfloat16", selected.precision)
        one_small = GpuFit(device_count=1, free_bytes=BF16_FIT_BYTES - 1, total_bytes=8 * 1024**3, bf16_supported=True)
        with self.assertRaises(StartupError) as caught:
            choose_semif_runtime("cuda:0", one_small)
        self.assertIn("not a silent substitute", str(caught.exception))
        capable = GpuFit(device_count=1, free_bytes=BF16_FIT_BYTES + 10, bf16_supported=True)
        self.assertEqual(choose_semif_runtime("auto", capable).precision, "bfloat16")
        row = decision_row(
            "note",
            "Health checks passed.",
            PredicateQuestion("claim", "The deployment succeeded."),
        )
        self.assertEqual(
            [option["id"] for option in row["options"]],
            ["supported", "insufficient", "contradicted"],
        )
        self.assertEqual(row["options"][1]["description"], "The evidence does not establish either")
        self.assertTrue(row["question"].startswith("Assess the claim using only the supplied evidence: "))

    def test_predicate_insufficient_is_explicit_and_uncalibrated(self) -> None:
        model = FakeModel()
        model.supports_predicates = True
        model.option_names = lambda question: ("supported", "insufficient", "contradicted") if question.__class__.__name__ == "PredicateQuestion" else None

        def scores(item):
            return "insufficient", {
                "supported": 0.05,
                "insufficient": 0.9,
                "contradicted": 0.05,
            }

        model.scores = scores
        engine = _engine(model, device_request="auto", forced_device="cpu", fallback_reason="BF16 not selected")
        payload = _evaluate(
            [{"id": "note", "text": "Health checks passed."}],
            [{"kind": "predicate", "id": "claim", "statement": "The deployment succeeded."}],
        )
        message = _run(engine, json.dumps(payload).encode() + b"\n")[0]
        row = message["results"][0]
        self.assertEqual(row["status"], "unresolved")
        self.assertEqual(row["label"], "insufficient")
        self.assertIsNone(row["value"])
        self.assertIn("not calibrated", row["reason"])
        self.assertEqual(message["provenance"]["device"], "cpu")
        self.assertIn("BF16", message["provenance"]["device_fallback"])

    def test_auto_bf16_failure_constructs_a_new_cpu_model(self) -> None:
        gpu = FakeModel()
        gpu.precision = "bfloat16"
        gpu.fail_probe.add("cuda:0")
        cpu = FakeModel()
        cpu.precision = "gguf-q4_k_m"
        engine = _engine(
            gpu,
            device_request="auto",
            inventory=AcceleratorInventory(cuda_device_count=1),
            cpu_loader=lambda: cpu,
        )
        engine.start()
        self.assertIs(engine.model, cpu)
        self.assertEqual(engine.selected_device, "cpu")
        self.assertEqual(cpu.precision, "gguf-q4_k_m")
        self.assertNotIn("cpu", gpu.placed)
        self.assertIn("unloaded", engine.fallback_reason)

    def test_explicit_cuda_failure_does_not_construct_a_cpu_model(self) -> None:
        gpu = FakeModel()
        gpu.fail_probe.add("cuda:0")

        def unexpected():
            raise AssertionError("cpu backend must not be constructed")

        engine = _engine(
            gpu,
            device_request="cuda:0",
            inventory=AcceleratorInventory(cuda_device_count=1),
            cpu_loader=unexpected,
        )
        with self.assertRaises(StartupError):
            engine.start()
        self.assertNotIn("cpu", gpu.placed)

    def test_bf16_loader_failure_constructs_the_cpu_backend(self) -> None:
        cpu = FakeModel()
        cpu.precision = "gguf-q4_k_m"

        def fail_load():
            raise StartupError("bf16 weights did not load")

        engine = _engine(
            FakeModel(),
            device_request="auto",
            inventory=AcceleratorInventory(cuda_device_count=1),
            loader=fail_load,
            cpu_loader=lambda: cpu,
        )
        engine.start()
        self.assertIs(engine.model, cpu)
        self.assertEqual(engine.provenance()["precision"], "gguf-q4_k_m")
        self.assertIn("unloaded", engine.fallback_reason)

    def test_m10_samples_are_not_isolated_and_explicit_cuda_is_not_swapped(self) -> None:
        from checkweave_worker.semif_backend import BF16_FIT_BYTES, GpuSample, isolate_visible_device

        m10 = [GpuSample(index, 8 * 1024**3, 5) for index in range(4)]
        self.assertIsNone(isolate_visible_device("auto", m10))
        self.assertIsNone(isolate_visible_device("cuda:0", m10))
        mixed = [GpuSample(0, 8 * 1024**3, 5), GpuSample(1, BF16_FIT_BYTES, 8)]
        self.assertEqual(isolate_visible_device("auto", mixed), "1")
        self.assertIsNone(isolate_visible_device("cuda:0", mixed))
        self.assertEqual(isolate_visible_device("cuda:1", mixed), "1")

    def test_build_engine_remaps_isolated_cuda1_and_reorders_visible_devices(self) -> None:
        import os
        import sys
        import types
        from unittest.mock import patch

        from checkweave_worker.__main__ import _build_engine
        from checkweave_worker.semif_backend import BF16_FIT_BYTES

        class FakeCuda:
            @staticmethod
            def is_available() -> bool:
                return True

            @staticmethod
            def device_count() -> int:
                return 1

            @staticmethod
            def mem_get_info(_index: int = 0):
                return (BF16_FIT_BYTES + 1, BF16_FIT_BYTES + 1)

            @staticmethod
            def is_bf16_supported() -> bool:
                return True

        torch_mod = types.ModuleType("torch")
        torch_mod.cuda = FakeCuda()
        loaded = []

        def fake_load(selection, *, offline, threads, gguf):
            del offline, threads, gguf
            loaded.append(selection.device)
            model = FakeModel()
            model.precision = selection.precision
            model.bound = selection.device

            def place(device: str) -> None:
                model.placed.append(device)
                if device != model.bound:
                    raise DeviceFailure(device, "not the isolated device", "unavailable")
                model.parameter_device = device
                model.input_device = device

            model.place = place
            return model

        smi = "0, 8192, 5.0\n1, 24576, 8.9\n"

        def fake_smi(*_args, **_kwargs):
            completed = types.SimpleNamespace(returncode=0, stdout=smi, stderr="")
            return completed

        previous = os.environ.get("CUDA_VISIBLE_DEVICES")
        try:
            os.environ.pop("CUDA_VISIBLE_DEVICES", None)
            with patch.dict(sys.modules, {"torch": torch_mod}), patch(
                "subprocess.run", side_effect=fake_smi
            ), patch(
                "checkweave_worker.semif_backend.load_semif_model", side_effect=fake_load
            ):
                engine = _build_engine(_args(device="cuda:1"))
                engine.start()
                self.assertEqual(os.environ["CUDA_VISIBLE_DEVICES"], "1")
                self.assertEqual(loaded, ["cuda:0"])
                self.assertEqual(engine.selected_device, "cuda:0")
                provenance = engine.provenance()
                self.assertEqual(provenance["device_requested"], "cuda:1")
                self.assertEqual(provenance["device"], "cuda:0")
                self.assertEqual(provenance["cuda_visible_devices"], "1")
                self.assertEqual(provenance["precision"], "bfloat16")
                self.assertIsNone(provenance["device_fallback"])

            os.environ["CUDA_VISIBLE_DEVICES"] = "1,0"
            loaded.clear()
            with patch.dict(sys.modules, {"torch": torch_mod}), patch(
                "subprocess.run", side_effect=fake_smi
            ), patch(
                "checkweave_worker.semif_backend.load_semif_model", side_effect=fake_load
            ):
                engine = _build_engine(_args(device="cuda:0"))
                engine.start()
                self.assertEqual(os.environ["CUDA_VISIBLE_DEVICES"], "1")
                self.assertEqual(loaded, ["cuda:0"])
                provenance = engine.provenance()
                self.assertEqual(provenance["device_requested"], "cuda:0")
                self.assertEqual(provenance["device"], "cuda:0")
                self.assertEqual(provenance["cuda_visible_devices"], "1")
        finally:
            if previous is None:
                os.environ.pop("CUDA_VISIBLE_DEVICES", None)
            else:
                os.environ["CUDA_VISIBLE_DEVICES"] = previous

    def test_real_semif_oom_replaces_with_one_q4_and_explicit_stays_error(self) -> None:
        import sys
        import types
        from unittest.mock import patch

        from checkweave_worker.engine import ScoreItem
        from checkweave_worker.questions import ChoiceQuestion, Label
        from checkweave_worker.semif_backend import SemifModel, SemifSelection, _load_bf16

        def oom(*_args, **_kwargs):
            raise RuntimeError("CUDA out of memory")

        gpu = SemifModel(object(), object(), {}, oom, "bfloat16", "cuda:0")
        q4_calls = []

        def q4():
            q4_calls.append(1)
            cpu = FakeModel()
            cpu.precision = "gguf-q4_k_m"
            return cpu

        engine = _engine(
            gpu,
            device_request="auto",
            inventory=AcceleratorInventory(cuda_device_count=1),
            forced_device="cuda:0",
            loader=lambda: gpu,
            cpu_loader=q4,
        )
        engine.start()
        self.assertEqual(q4_calls, [1])
        self.assertIsNone(gpu.backend)
        self.assertIsNone(gpu.score_fn)
        self.assertEqual(engine.model.precision, "gguf-q4_k_m")
        self.assertEqual(engine.provenance()["device"], "cpu")
        self.assertIn("unloaded", engine.provenance()["device_fallback"])

        explicit = SemifModel(object(), object(), {}, oom, "bfloat16", "cuda:0")

        def forbidden():
            raise AssertionError("explicit GPU failure must not load Q4")

        explicit_engine = _engine(
            explicit,
            device_request="cuda:0",
            inventory=AcceleratorInventory(cuda_device_count=1),
            forced_device="cuda:0",
            loader=lambda: explicit,
            cpu_loader=forbidden,
        )
        with self.assertRaises(StartupError):
            explicit_engine.start()
        self.assertIsNotNone(explicit.backend)

        item = ScoreItem(
            "row",
            "evidence",
            ChoiceQuestion("q", (Label("a", "A"), Label("b", "B"))),
        )
        def mismatch(*_args, **_kwargs):
            raise RuntimeError("tokenizer mismatch")

        unrelated = SemifModel(object(), object(), {}, mismatch, "bfloat16", "cuda:0")
        with self.assertRaises(RuntimeError) as caught:
            unrelated.classify_batch([item])
        self.assertNotIsInstance(caught.exception, DeviceFailure)

        core = types.ModuleType("semif_phase1.core")

        def load_causal_model(*_args, **_kwargs):
            raise RuntimeError("CUDA out of memory")

        core.load_causal_model = load_causal_model
        direct = types.ModuleType("semif_phase1.direct")
        direct.score = lambda *_args, **_kwargs: None
        package = types.ModuleType("semif_phase1")
        with patch.dict(
            sys.modules,
            {
                "semif_phase1": package,
                "semif_phase1.core": core,
                "semif_phase1.direct": direct,
            },
        ):
            with self.assertRaises(DeviceFailure) as loaded:
                _load_bf16(SemifSelection("bfloat16", "cuda:1", None), offline=False)
        self.assertEqual(loaded.exception.kind, "oom")
        self.assertEqual(loaded.exception.device, "cuda:1")

        def plain_failure(*_args, **_kwargs):
            raise RuntimeError("revision pin mismatch")

        core.load_causal_model = plain_failure
        with patch.dict(
            sys.modules,
            {
                "semif_phase1": package,
                "semif_phase1.core": core,
                "semif_phase1.direct": direct,
            },
        ):
            with self.assertRaises(RuntimeError) as plain:
                _load_bf16(SemifSelection("bfloat16", "cuda:0", None), offline=False)
        self.assertNotIsInstance(plain.exception, DeviceFailure)


def _args(device: str):
    import argparse

    return argparse.Namespace(
        backend="semif",
        device=device,
        threads=1,
        offline=True,
        gguf=None,
    )


class PipeTests(unittest.TestCase):
    def test_live_pipe_returns_two_frames_while_writer_stays_open(self) -> None:
        import os
        import threading

        read_fd, write_fd = os.pipe()
        buffered = io.BufferedReader(os.fdopen(read_fd, "rb", buffering=0))
        writer = os.fdopen(write_fd, "wb", buffering=0)
        reader = FrameReader(buffered)
        first: dict = {}
        second: dict = {}

        def pull_first() -> None:
            first["frame"] = reader.read()

        def pull_second() -> None:
            second["frame"] = reader.read()

        writer.write(b'{"id":"one"}\n')
        writer.flush()
        thread = threading.Thread(target=pull_first)
        thread.start()
        thread.join(1.0)
        self.assertFalse(thread.is_alive(), "first frame blocked while the writer stayed open")
        self.assertEqual(first["frame"], b'{"id":"one"}')
        writer.write(b'{"id":"two"}\n')
        writer.flush()
        thread = threading.Thread(target=pull_second)
        thread.start()
        thread.join(1.0)
        self.assertFalse(thread.is_alive(), "second frame blocked while the writer stayed open")
        self.assertEqual(second["frame"], b'{"id":"two"}')
        self.assertFalse(writer.closed)
        writer.close()
        self.assertIsNone(reader.read())
        buffered.close()

    def test_live_pipe_oversize_line_recovers_onto_the_next_frame(self) -> None:
        import os
        import threading

        read_fd, write_fd = os.pipe()
        buffered = io.BufferedReader(os.fdopen(read_fd, "rb", buffering=0))
        writer = os.fdopen(write_fd, "wb", buffering=0)
        reader = FrameReader(buffered, limit=8)
        outcome: dict = {}

        def pull() -> None:
            try:
                outcome["frame"] = reader.read()
            except FrameTooLarge:
                outcome["error"] = True
                outcome["next"] = reader.read()

        writer.write(b"0123456789\nok\n")
        writer.flush()
        thread = threading.Thread(target=pull)
        thread.start()
        thread.join(1.0)
        self.assertFalse(thread.is_alive(), "oversize recovery blocked while the writer stayed open")
        self.assertTrue(outcome.get("error"))
        self.assertEqual(outcome.get("next"), b"ok")
        self.assertFalse(writer.closed)
        writer.close()
        buffered.close()


class FrameTests(unittest.TestCase):
    def test_reader_returns_exact_lines(self) -> None:
        reader = FrameReader(io.BytesIO(b"one\ntwo\n"), limit=8)
        self.assertEqual(reader.read(), b"one")
        self.assertEqual(reader.read(), b"two")
        self.assertIsNone(reader.read())

    def test_reader_rejects_an_overlong_line_and_keeps_the_next(self) -> None:
        reader = FrameReader(io.BytesIO(b"0123456789\nok\n"), limit=4)
        with self.assertRaises(FrameTooLarge):
            reader.read()
        self.assertEqual(reader.read(), b"ok")


class QuietTests(unittest.TestCase):
    def test_library_stdout_is_redirected_to_stderr(self) -> None:
        from checkweave_worker.quiet import library_quiet

        stdout = io.StringIO()
        stderr = io.StringIO()
        previous_out, previous_err = sys.stdout, sys.stderr
        sys.stdout, sys.stderr = stdout, stderr
        try:
            with library_quiet():
                print("library-noise")
            print("protocol")
        finally:
            sys.stdout, sys.stderr = previous_out, previous_err
        self.assertEqual(stdout.getvalue(), "protocol\n")
        self.assertEqual(stderr.getvalue(), "library-noise\n")


if __name__ == "__main__":
    unittest.main()
