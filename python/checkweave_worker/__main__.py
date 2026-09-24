"""Managed local inference process.

Stdout is reserved for protocol frames. Diagnostics go to stderr.
"""

from __future__ import annotations

import argparse
import logging
import os
import signal
import sys

from .constants import (
    DEFAULT_THREADS,
    MAX_THREADS,
    MIN_THREADS,
    PROTOCOL_VERSION,
)
from .devices import StartupError, parse_device
from .stdio import WorkerLoop


def parse_args(argv: list[str] | None = None) -> argparse.Namespace:
    parser = argparse.ArgumentParser(description="Checkweave local inference worker")
    parser.add_argument(
        "--device",
        default="auto",
        help="auto, cpu, cuda, cuda:<index>, or mps",
    )
    parser.add_argument("--threads", type=int, default=DEFAULT_THREADS)
    parser.add_argument(
        "--backend",
        choices=("semif", "gliner2"),
        default="semif",
        help="semif is the default local adapter; gliner2 is the lightweight profile",
    )
    parser.add_argument(
        "--gguf",
        default=None,
        help="local Q4_K_M GGUF for the SemIf CPU path; default is the pinned Hub file",
    )
    parser.add_argument(
        "--offline",
        action="store_true",
        help="load the pinned checkpoint from the local cache only",
    )
    args = parser.parse_args(argv)
    try:
        args.device = parse_device(args.device)
    except ValueError as exc:
        parser.error(str(exc))
    if args.threads < MIN_THREADS or args.threads > MAX_THREADS:
        parser.error(f"--threads must be between {MIN_THREADS} and {MAX_THREADS}")
    return args


def configure_environment(args: argparse.Namespace) -> None:
    threads = str(args.threads)
    os.environ["OMP_NUM_THREADS"] = threads
    os.environ["MKL_NUM_THREADS"] = threads
    os.environ["OPENBLAS_NUM_THREADS"] = threads
    os.environ["NUMEXPR_NUM_THREADS"] = threads
    os.environ["TOKENIZERS_PARALLELISM"] = "false"
    os.environ["HF_HUB_DISABLE_IMPLICIT_TOKEN"] = "1"
    os.environ["HF_HUB_DISABLE_TELEMETRY"] = "1"
    if args.offline:
        os.environ["HF_HUB_OFFLINE"] = "1"
        os.environ["TRANSFORMERS_OFFLINE"] = "1"
        os.environ["HF_DATASETS_OFFLINE"] = "1"
    # Explicit CPU must not initialize every CUDA device. Auto and cuda keep
    # the caller's visibility so the forward probe can see the accelerator.
    if args.device == "cpu":
        os.environ["CUDA_VISIBLE_DEVICES"] = ""
    elif args.device.startswith("cuda:"):
        os.environ["CUDA_VISIBLE_DEVICES"] = args.device.split(":", 1)[1]


def main(argv: list[str] | None = None) -> int:
    logging.basicConfig(stream=sys.stderr, level=logging.INFO, format="%(message)s")
    args = parse_args(argv)
    configure_environment(args)
    protocol_out = sys.stdout.buffer
    protocol_in = _detach_stdin()
    sys.stdout = sys.stderr
    try:
        logging.info("loading %s backend", args.backend)
        engine = _build_engine(args)
        logging.info("probing device %s", args.device)
        engine.start()
        logging.info("model ready on %s precision %s", engine.selected_device, engine.model.precision)
    except (Exception, StartupError) as exc:
        _emit(
            protocol_out,
            {"version": PROTOCOL_VERSION, "ready": False, "error": str(exc)},
        )
        logging.exception("inference worker failed to start")
        return 1
    _emit(
        protocol_out,
        {
            "version": PROTOCOL_VERSION,
            "ready": True,
            "provenance": engine.provenance(),
        },
    )
    loop = WorkerLoop(engine, protocol_in, protocol_out)

    def _stop(signum, _frame):
        logging.info("received signal %s; stopping after the current request", signum)
        loop.request_stop()

    signal.signal(signal.SIGTERM, _stop)
    signal.signal(signal.SIGINT, _stop)
    try:
        return loop.run()
    except BrokenPipeError:
        return 0


def _build_engine(args: argparse.Namespace):
    from .devices import AcceleratorInventory
    from .engine import Engine

    if args.backend == "gliner2":
        import torch

        from .gliner_backend import load_gliner_model, torch_inventory

        return Engine(
            device_request=args.device,
            threads=args.threads,
            offline=args.offline,
            inventory=torch_inventory(torch),
            loader=lambda: load_gliner_model(args.offline, args.threads),
        )
    from .semif_backend import (
        GpuFit,
        SemifSelection,
        choose_semif_runtime,
        gpu_fit_from_torch,
        isolate_visible_device,
        load_semif_model,
        nvidia_smi_samples,
    )

    measured = GpuFit()
    visible = None
    if args.device not in ("cpu", "mps"):
        visible = isolate_visible_device(args.device, nvidia_smi_samples())
        if visible is not None:
            # One physical GPU. Torch then numbers that device cuda:0.
            os.environ["CUDA_VISIBLE_DEVICES"] = visible
    if args.device != "cpu":
        import torch

        measured = gpu_fit_from_torch(torch)
    selection = choose_semif_runtime(args.device, measured)
    if visible is not None and selection.device.startswith("cuda:"):
        selection = SemifSelection(selection.precision, "cuda:0", selection.reason)
    cpu_loader = None
    if args.device == "auto" and selection.precision == "bfloat16":
        q4 = SemifSelection("gguf-q4_k_m", "cpu", None)

        def cpu_loader(selection=q4):
            return load_semif_model(
                selection,
                offline=args.offline,
                threads=args.threads,
                gguf=args.gguf,
            )
    if selection.device == "mps":
        fit_inventory = AcceleratorInventory(mps=True)
    elif selection.device.startswith("cuda"):
        fit_inventory = AcceleratorInventory(cuda_device_count=1)
    else:
        fit_inventory = AcceleratorInventory()
    return Engine(
        device_request=args.device,
        threads=args.threads,
        offline=args.offline,
        inventory=fit_inventory,
        forced_device=selection.device,
        fallback_reason=selection.reason,
        cpu_loader=cpu_loader,
        cuda_visible_devices=visible,
        loader=lambda: load_semif_model(
            selection,
            offline=args.offline,
            threads=args.threads,
            gguf=args.gguf,
        ),
    )


def _detach_stdin():
    """Keep protocol bytes off fd 0 so libraries cannot block on them."""
    protocol_fd = os.dup(0)
    protocol_in = os.fdopen(protocol_fd, "rb", buffering=0)
    devnull = os.open(os.devnull, os.O_RDONLY)
    try:
        os.dup2(devnull, 0)
    finally:
        os.close(devnull)
    return protocol_in


def _emit(stream, payload: dict) -> None:
    import json

    stream.write(
        json.dumps(payload, ensure_ascii=False, allow_nan=False, separators=(",", ":")).encode(
            "utf-8"
        )
        + b"\n"
    )
    stream.flush()


if __name__ == "__main__":
    raise SystemExit(main())
