"""Compare GLiNER2.5 and Jeff's GLiFormer on the same short fixtures.

This is a local inference experiment. It is not the Checkweave runtime and it
does not qualify a release. Fixtures are the existing 24-case smoke set.
Expected labels are not modified here.

GLiNER2 uses the Checkweave smoke adapter: one single-label schema whose
labels keep their descriptions. A second, separately reported pass uses the
JevBench v1.2.2 packing (question text in front of the state, full softmax).

GLiFormer uses Jeff's default choice rendering from commit 6f43d3e: the
instruction is the group name, each label is "key: description", and the
selected label is the argmax of the independent sigmoid scores. Temperature
3.2 rescales the reported distribution and does not change that argmax when
every score is positive.
"""

from __future__ import annotations

import argparse
import hashlib
import importlib.metadata
import json
import math
import os
import platform
import threading
import time
from pathlib import Path

os.environ.setdefault("HF_HUB_DISABLE_IMPLICIT_TOKEN", "1")
os.environ.setdefault("TOKENIZERS_PARALLELISM", "false")
os.environ.setdefault("OMP_NUM_THREADS", "4")
os.environ.setdefault("MKL_NUM_THREADS", "4")

import torch

GLINER = (
    "fastino/gliner2.5-base-v1",
    "1a8bc24e00dc7300b9017c81d63e3dcdabb26596",
)
GLIFORMER = (
    "knowledgator/gliformer-large-v1",
    "d0a4e53d09cebe6bc963dd9be319d4279084bb2d",
)
JEFF_TEMPERATURE = 3.2
# Decoder treats 0.0 as unset; a negative threshold returns every label.
ALL_LABELS_THRESHOLD = -1.0

ORDINAL_PROBE = {
    "id": "ordinal-probe",
    "text": "The export button fails for every user and there is no workaround.",
    "instruction": "How many users does the text say are affected?",
    "levels": {
        "none": "The text does not describe harm to users.",
        "some_users": "The text describes harm that affects some users and not others.",
        "all_users": "The text describes harm that affects every user.",
    },
    "expected": "all_users",
}


def percentile(values: list[float], p: float) -> float:
    ordered = sorted(values)
    if len(ordered) == 1:
        return ordered[0]
    rank = (len(ordered) - 1) * p
    low = math.floor(rank)
    high = math.ceil(rank)
    if low == high:
        return ordered[int(rank)]
    weight = rank - low
    return ordered[low] * (1.0 - weight) + ordered[high] * weight


def rss_bytes() -> int:
    for line in Path("/proc/self/status").read_text().splitlines():
        if line.startswith("VmRSS:"):
            return int(line.split()[1]) * 1024
    return 0


class RssSampler:
    def __init__(self) -> None:
        self.peak = 0
        self._stop = threading.Event()
        self._thread = threading.Thread(target=self._run, daemon=True)

    def _run(self) -> None:
        while not self._stop.is_set():
            self.peak = max(self.peak, rss_bytes())
            self._stop.wait(0.05)

    def __enter__(self) -> "RssSampler":
        self.peak = rss_bytes()
        self._thread.start()
        return self

    def __exit__(self, *_) -> None:
        self._stop.set()
        self._thread.join()
        self.peak = max(self.peak, rss_bytes())


def package_versions(names: list[str]) -> dict[str, str | None]:
    found = {}
    for name in names:
        try:
            found[name] = importlib.metadata.version(name)
        except importlib.metadata.PackageNotFoundError:
            found[name] = None
    return found


def family_counts(rows: list[dict]) -> dict[str, dict[str, int]]:
    families: dict[str, dict[str, int]] = {}
    for row in rows:
        bucket = families.setdefault(row["task"], {"correct": 0, "count": 0})
        bucket["count"] += 1
        bucket["correct"] += int(row["correct"])
    return families


def evidence_split(rows: list[dict]) -> dict[str, dict[str, int]]:
    decisive = [row for row in rows if row["expected"] != "unknown"]
    missing = [row for row in rows if row["expected"] == "unknown"]
    return {
        "decisive": {
            "correct": sum(row["correct"] for row in decisive),
            "count": len(decisive),
        },
        "missing_evidence": {
            "correct": sum(row["correct"] for row in missing),
            "count": len(missing),
            "false_decisive": sum(
                row["predicted"] != "unknown" for row in missing
            ),
        },
    }


def normalize(scores: list[float], temperature: float) -> list[float]:
    """Jeff's score renormalization (src/jeff/core/answers.py, commit 6f43d3e)."""
    scaled = [max(float(score), 0.0) for score in scores]
    if temperature != 1.0:
        scaled = [score ** (1.0 / temperature) for score in scaled]
    total = sum(scaled)
    if total <= 1e-9:
        return [1.0 / len(scaled)] * len(scaled)
    return [score / total for score in scaled]


class GlinerPredictor:
    name = "gliner2"

    def __init__(self, device: str) -> None:
        from gliner2 import AutoExtractor
        from gliner2.classification import ClassificationSchema, Classifier
        from huggingface_hub import snapshot_download

        checkpoint = snapshot_download(
            GLINER[0],
            revision=GLINER[1],
            allow_patterns=["*.json", "*.safetensors", "*.model", "*.txt"],
        )
        self.classifier = Classifier.from_pretrained(checkpoint).to(device=device).eval()
        self.extractor = AutoExtractor.from_pretrained(checkpoint)
        if device != "cpu":
            self.extractor = self.extractor.to(device)
        self.schema_cls = ClassificationSchema
        self.device = device

    def predict(self, case: dict, labels: dict[str, str] | None = None) -> tuple[str, dict]:
        labels = labels if labels is not None else case["labels"]
        schema = self.schema_cls().single(case["task"], labels)
        result = self.classifier.classify(case["text"], schema)
        payload = result.to_dict()[case["task"]]
        return payload["value"], payload

    def predict_jevbench(self, case: dict) -> tuple[str, dict]:
        text = f"Question: {case['task']}\n\n{case['text']}"
        schema = self.extractor.create_schema().classification(
            "decision",
            case["labels"],
            multi_label=True,
            cls_threshold=0.0,
            class_act="softmax",
        )
        raw = self.extractor.extract(text, schema, include_confidence=True)
        probs = {item["label"]: float(item["confidence"]) for item in raw["decision"]}
        selected = max(probs, key=probs.get)
        return selected, {"probabilities": probs, "packing": "jevbench-v1.2.2"}

    def predict_pair(self, first: dict, second: dict) -> tuple[str, str]:
        schema = self.schema_cls().single(first["task"], first["labels"]).single(
            "other_" + second["task"], second["labels"]
        )
        result = self.classifier.classify(first["text"], schema)
        payload = result.to_dict()
        return payload[first["task"]]["value"], payload["other_" + second["task"]]["value"]

    def predict_ordinal(self, levels: dict[str, str], reversed_order: bool) -> tuple[str, dict]:
        ordered = dict(reversed(list(levels.items()))) if reversed_order else levels
        schema = self.schema_cls().ordinal("impact", ordered)
        result = self.classifier.classify(ORDINAL_PROBE["text"], schema)
        payload = result.to_dict()["impact"]
        return payload["value"], payload


class GliformerPredictor:
    name = "gliformer"

    def __init__(self, device: str) -> None:
        from gliformer import GLiFormer
        from huggingface_hub import snapshot_download

        checkpoint = snapshot_download(GLIFORMER[0], revision=GLIFORMER[1])
        self.model = GLiFormer.from_pretrained(
            checkpoint,
            map_location=device,
            _attn_implementation="eager",
        )
        self.model.to(device).eval()
        self._force_eager()
        collator_cls = self.model.data_collator_class
        if collator_cls is None:
            from gliformer.gliformer import resolve_gliformer_collator_class

            collator_cls = resolve_gliformer_collator_class(self.model.config)
        self.collator = collator_cls(
            self.model.config,
            data_processor=self.model.data_processor,
            return_tokens=True,
            prepare_labels=False,
        )
        self.device = device

    def _force_eager(self) -> None:
        for module in self.model.model.modules():
            if hasattr(module, "attn_kernel"):
                module.attn_kernel = "eager"

    def _fold(self, labels: dict[str, str]) -> tuple[str, ...]:
        return tuple(f"{key}: {text}" if text else key for key, text in labels.items())

    @torch.inference_mode()
    def _score(self, text: str, groups: list[tuple[str, str, tuple[str, ...]]]) -> list[list[float]]:
        from torch.utils.data import DataLoader

        tokens, _, _ = self.model.prepare_inputs([text])
        item = {
            "tokenized_text": tokens[0],
            "classification": [
                {
                    "name": name,
                    "description": None,
                    "all_labels": list(labels),
                    "true_labels": [],
                }
                for _key, name, labels in groups
            ],
        }
        loader = DataLoader([item], batch_size=1, shuffle=False, collate_fn=self.collator)
        decoded, _ = self.model._process_multitask_batches(
            loader, ALL_LABELS_THRESHOLD, True, True, decoder_kwargs=None
        )
        groups_out = decoded.get("classification", [[]])[0]
        scored = []
        for (_key, _name, labels), preds in zip(groups, groups_out):
            by_name = {pred["class_name"]: float(pred["score"]) for pred in preds}
            scored.append([by_name.get(label, 0.0) for label in labels])
        return scored

    def _choose(self, keys: list[str], scores: list[float]) -> tuple[str, dict]:
        probs = normalize(scores, JEFF_TEMPERATURE)
        selected = keys[max(range(len(keys)), key=lambda index: scores[index])]
        return selected, {
            "raw_scores": {key: score for key, score in zip(keys, scores)},
            "probabilities_t3_2": {key: prob for key, prob in zip(keys, probs)},
            "probability_origin": "independent-sigmoid-renormalized",
        }

    def predict(self, case: dict, labels: dict[str, str] | None = None) -> tuple[str, dict]:
        labels = labels if labels is not None else case["labels"]
        folded = self._fold(labels)
        scores = self._score(case["text"], [(case["task"], case["task"], folded)])[0]
        return self._choose(list(labels), scores)

    def predict_pair(self, first: dict, second: dict) -> tuple[str, str, dict]:
        groups = [
            (first["task"], first["task"], self._fold(first["labels"])),
            ("other", "other_" + second["task"], self._fold(second["labels"])),
        ]
        shared = self._score(first["text"], groups)
        alone = self._score(first["text"], [groups[0]])[0]
        shared_label, _ = self._choose(list(first["labels"]), shared[0])
        alone_label, alone_payload = self._choose(list(first["labels"]), alone)
        return shared_label, alone_label, alone_payload

    def predict_ordinal(self, levels: dict[str, str], reversed_order: bool) -> tuple[str, dict]:
        ordered = dict(reversed(list(levels.items()))) if reversed_order else dict(levels)
        folded = self._fold(ordered)
        scores = self._score(
            ORDINAL_PROBE["text"],
            [("impact", ORDINAL_PROBE["instruction"], folded)],
        )[0]
        return self._choose(list(ordered), scores)


def synchronize(device: str) -> None:
    if device.startswith("cuda"):
        torch.cuda.synchronize()


def timed(predict, case: dict, device: str) -> tuple[str, dict, float]:
    started = time.perf_counter()
    label, raw = predict(case)
    synchronize(device)
    return label, raw, time.perf_counter() - started


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--backend", choices=("gliner2", "gliformer"), required=True)
    parser.add_argument("--device", default="cpu")
    parser.add_argument("--threads", type=int, default=4)
    parser.add_argument(
        "--cases",
        type=Path,
        default=Path(__file__).resolve().parents[1] / "model-backends" / "cases.json",
    )
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()

    torch.set_num_threads(args.threads)
    try:
        torch.set_num_interop_threads(1)
    except RuntimeError:
        pass
    torch.manual_seed(0)
    cases_bytes = args.cases.read_bytes()
    cases = json.loads(cases_bytes)

    started = time.perf_counter()
    if args.backend == "gliner2":
        predictor = GlinerPredictor(args.device)
        model_id, revision = GLINER
    else:
        predictor = GliformerPredictor(args.device)
        model_id, revision = GLIFORMER
    synchronize(args.device)
    load_seconds = time.perf_counter() - started
    rss_after_load = rss_bytes()

    with RssSampler() as sampler:
        predictor.predict(cases[0])
        synchronize(args.device)
        rows = []
        for case in cases:
            label, raw, seconds = timed(predictor.predict, case, args.device)
            rows.append({
                "id": case["id"],
                "task": case["task"],
                "expected": case["expected"],
                "predicted": label,
                "correct": label == case["expected"],
                "seconds": seconds,
                "raw": raw,
            })

        order_rows = []
        for case in cases:
            reversed_labels = dict(reversed(list(case["labels"].items())))
            label, raw, seconds = timed(
                lambda item, labels=reversed_labels: predictor.predict(item, labels),
                case,
                args.device,
            )
            original = next(row for row in rows if row["id"] == case["id"])
            order_rows.append({
                "id": case["id"],
                "original": original["predicted"],
                "reversed": label,
                "same_label": label == original["predicted"],
                "seconds": seconds,
            })

        pair_rows = []
        for index, case in enumerate(cases):
            other = cases[(index + 6) % len(cases)]
            started = time.perf_counter()
            if args.backend == "gliner2":
                together, _other_label = predictor.predict_pair(case, other)
                alone = next(row["predicted"] for row in rows if row["id"] == case["id"])
            else:
                together, alone, _payload = predictor.predict_pair(case, other)
            synchronize(args.device)
            pair_rows.append({
                "id": case["id"],
                "paired_with": other["id"],
                "alone": alone,
                "together": together,
                "same_label": together == alone,
                "seconds": time.perf_counter() - started,
            })

        ordinal = {}
        for reversed_order in (False, True):
            started = time.perf_counter()
            label, raw = predictor.predict_ordinal(ORDINAL_PROBE["levels"], reversed_order)
            synchronize(args.device)
            key = "reversed" if reversed_order else "forward"
            ordinal[key] = {
                "predicted": label,
                "matches_obvious_level": label == ORDINAL_PROBE["expected"],
                "seconds": time.perf_counter() - started,
                "raw": raw,
            }

        jevbench_rows = []
        if args.backend == "gliner2":
            for case in cases:
                started = time.perf_counter()
                label, raw = predictor.predict_jevbench(case)
                synchronize(args.device)
                jevbench_rows.append({
                    "id": case["id"],
                    "task": case["task"],
                    "expected": case["expected"],
                    "predicted": label,
                    "correct": label == case["expected"],
                    "seconds": time.perf_counter() - started,
                    "raw": raw,
                })

    seconds = [row["seconds"] for row in rows]
    report = {
        "model": model_id,
        "revision": revision,
        "backend": args.backend,
        "device": args.device,
        "dtype": "float32",
        "device_name": (
            torch.cuda.get_device_name(args.device)
            if args.device.startswith("cuda")
            else platform.processor() or args.device
        ),
        "threads": args.threads,
        "python": platform.python_version(),
        "platform": platform.platform(),
        "packages": package_versions([
            "torch", "transformers", "gliner2", "gliformer", "numpy",
            "huggingface-hub", "tokenizers", "safetensors",
        ]),
        "cases_sha256": hashlib.sha256(cases_bytes).hexdigest(),
        "runner_sha256": hashlib.sha256(Path(__file__).read_bytes()).hexdigest(),
        "load_seconds": load_seconds,
        "rss_after_load_bytes": rss_after_load,
        "rss_peak_bytes": sampler.peak,
        "cuda_peak_allocated_bytes": (
            torch.cuda.max_memory_allocated(args.device)
            if args.device.startswith("cuda") else None
        ),
        "cuda_peak_reserved_bytes": (
            torch.cuda.max_memory_reserved(args.device)
            if args.device.startswith("cuda") else None
        ),
        "correct": sum(row["correct"] for row in rows),
        "count": len(rows),
        "by_task": family_counts(rows),
        "evidence": evidence_split(rows),
        "median_seconds": percentile(seconds, 0.50),
        "p95_seconds": percentile(seconds, 0.95),
        "label_order_same": sum(row["same_label"] for row in order_rows),
        "label_order_count": len(order_rows),
        "multi_question_same": sum(row["same_label"] for row in pair_rows),
        "multi_question_count": len(pair_rows),
        "ordinal_probe": ordinal,
        "ordinal_probe_note": (
            "One obvious ordered example, excluded from correct/count. "
            "It checks that a level can be selected; it is not an ordinal benchmark."
        ),
        "results": rows,
        "label_order": order_rows,
        "multi_question": pair_rows,
        "jevbench_mapping": {
            "note": (
                "GLiNER2 only. Official v1.2.2 packing on these fixtures. "
                "Not mixed into correct/count."
            ),
            "correct": sum(row["correct"] for row in jevbench_rows),
            "count": len(jevbench_rows),
            "by_task": family_counts(jevbench_rows) if jevbench_rows else {},
            "evidence": evidence_split(jevbench_rows) if jevbench_rows else {},
            "results": jevbench_rows,
        },
    }
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(json.dumps(report, indent=2) + "\n")
    print(json.dumps({
        "backend": args.backend,
        "device": args.device,
        "correct": report["correct"],
        "count": report["count"],
        "evidence": report["evidence"],
        "by_task": report["by_task"],
        "median_seconds": report["median_seconds"],
        "p95_seconds": report["p95_seconds"],
        "rss_peak_bytes": report["rss_peak_bytes"],
        "label_order_same": report["label_order_same"],
        "multi_question_same": report["multi_question_same"],
        "ordinal": {
            key: value["predicted"] for key, value in ordinal.items()
        },
        "jevbench_mapping_correct": report["jevbench_mapping"]["correct"],
    }, indent=2))


if __name__ == "__main__":
    main()
