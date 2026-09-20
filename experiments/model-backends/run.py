"""Small exploratory inference comparison; this is not the Checkweave runtime."""

import argparse
import hashlib
import importlib.metadata
import json
import os
from pathlib import Path
import platform
import statistics
import time

# Both checkpoints are public; do not implicitly attach cached credentials.
os.environ.setdefault("HF_HUB_DISABLE_IMPLICIT_TOKEN", "1")
os.environ.setdefault("TOKENIZERS_PARALLELISM", "false")

import torch


MODELS = {
    "gliner2": (
        "fastino/gliner2.5-base-v1",
        "1a8bc24e00dc7300b9017c81d63e3dcdabb26596",
    ),
    "gliclass": (
        "knowledgator/gliclass-base-v3.0",
        "77a70e6cd52e602ed18184ef37d18bdd3741e3d5",
    ),
}


def load_predictor(backend, device):
    model_id, revision = MODELS[backend]
    if backend == "gliner2":
        from gliner2.classification import Classifier, ClassificationSchema
        from huggingface_hub import snapshot_download

        checkpoint = snapshot_download(
            model_id,
            revision=revision,
            allow_patterns=["*.json", "*.safetensors", "*.model", "*.txt"],
        )
        # `device=` alone configures input placement in this library version.
        # `.to()` also moves the model's parameters.
        classifier = Classifier.from_pretrained(checkpoint).to(device=device).eval()

        def predict(case):
            schema = ClassificationSchema().single(case["task"], case["labels"])
            result = classifier.classify(case["text"], schema)
            return result.value(case["task"]), result.to_dict()

    else:
        from gliclass import GLiClassModel, ZeroShotClassificationPipeline
        from transformers import AutoTokenizer

        model = GLiClassModel.from_pretrained(model_id, revision=revision)
        tokenizer = AutoTokenizer.from_pretrained(model_id, revision=revision)
        pipeline = ZeroShotClassificationPipeline(
            model, tokenizer, classification_type="multi-label",
            device=torch.device(device), max_length=512,
        )

        def predict(case):
            descriptions = {f"{label}: {text}": label
                            for label, text in case["labels"].items()}
            raw = pipeline(case["text"], list(descriptions), threshold=0.0)[0]
            scores = {descriptions[row["label"]]: row["score"] for row in raw}
            return max(scores, key=scores.get), scores

    return predict


def synchronize(device):
    if device.startswith("cuda"):
        torch.cuda.synchronize()
    elif device == "mps":
        torch.mps.synchronize()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--backend", choices=MODELS, required=True)
    parser.add_argument("--device", default="cpu")
    parser.add_argument("--threads", type=int, default=4)
    parser.add_argument("--cases", type=Path,
                        default=Path(__file__).with_name("cases.json"))
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    torch.set_num_threads(args.threads)
    torch.manual_seed(0)
    cases_bytes = args.cases.read_bytes()
    cases = json.loads(cases_bytes)
    started = time.perf_counter()
    predict = load_predictor(args.backend, args.device)
    synchronize(args.device)
    load_seconds = time.perf_counter() - started

    rows = []
    with torch.inference_mode():
        predict(cases[0])  # One unmeasured warmup; no fitting or prompt tuning.
        synchronize(args.device)
        for case in cases:
            started = time.perf_counter()
            label, raw = predict(case)
            synchronize(args.device)
            rows.append({
                "id": case["id"], "expected": case["expected"],
                "predicted": label, "correct": label == case["expected"],
                "seconds": time.perf_counter() - started, "raw": raw,
            })

    packages = ["torch", "transformers", "gliclass", "gliner2", "numpy",
                "peft", "huggingface-hub", "tokenizers", "safetensors"]
    report = {
        "model": MODELS[args.backend][0], "revision": MODELS[args.backend][1],
        "backend": args.backend, "device": args.device, "dtype": "float32",
        "device_name": (torch.cuda.get_device_name(args.device)
                        if args.device.startswith("cuda") else args.device),
        "threads": args.threads, "python": platform.python_version(),
        "platform": platform.platform(),
        "packages": {name: importlib.metadata.version(name) for name in packages},
        "cases_sha256": hashlib.sha256(cases_bytes).hexdigest(),
        "runner_sha256": hashlib.sha256(Path(__file__).read_bytes()).hexdigest(),
        "load_seconds": load_seconds,
        "correct": sum(row["correct"] for row in rows), "count": len(rows),
        "median_seconds": statistics.median(row["seconds"] for row in rows),
        "results": rows,
    }
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(json.dumps(report, indent=2) + "\n")
    print(json.dumps({key: value for key, value in report.items() if key != "results"}))


if __name__ == "__main__":
    main()
