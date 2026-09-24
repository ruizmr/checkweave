"""Device request parsing and accelerator failure classification."""

from __future__ import annotations

import re
from dataclasses import dataclass

_CUDA = re.compile(r"cuda:(\d+)$")


class DeviceFailure(Exception):
    """A probed accelerator could not run this model."""

    def __init__(self, device: str, reason: str, kind: str) -> None:
        self.device = device
        self.reason = reason
        self.kind = kind
        super().__init__(f"{device}: {kind}: {reason}")


class StartupError(Exception):
    """The worker could not load the pinned checkpoint onto a usable device."""


@dataclass(frozen=True)
class AcceleratorInventory:
    cuda_device_count: int = 0
    rocm: bool = False
    mps: bool = False


def parse_device(value: str) -> str:
    """Return a canonical device token.

    ``cuda`` means ``cuda:0``. ``auto`` is left for the selector.
    """
    if not isinstance(value, str):
        raise ValueError("device must be a string")
    if value in ("auto", "cpu", "mps"):
        return value
    if value == "cuda":
        return "cuda:0"
    match = _CUDA.fullmatch(value)
    if match is None:
        raise ValueError(
            "device must be auto, cpu, mps, cuda, or cuda:<index>"
        )
    return f"cuda:{int(match.group(1))}"


def candidate_devices(request: str, inventory: AcceleratorInventory) -> list[str]:
    """Order devices for a real forward probe.

    Explicit requests stay on that device. ``auto`` tries CUDA/ROCm, then MPS,
    then CPU. PyTorch exposes ROCm through the CUDA device API, so a ROCm
    device is still named ``cuda:0``.
    """
    selected = parse_device(request)
    if selected != "auto":
        return [selected]
    ordered: list[str] = []
    if inventory.cuda_device_count > 0:
        ordered.append("cuda:0")
    if inventory.mps:
        ordered.append("mps")
    ordered.append("cpu")
    return ordered


def require_device_present(device: str, inventory: AcceleratorInventory) -> None:
    if device.startswith("cuda:"):
        index = int(device.split(":", 1)[1])
        if index >= inventory.cuda_device_count:
            raise DeviceFailure(device, "device is not available", "unavailable")
    elif device == "mps" and not inventory.mps:
        raise DeviceFailure(device, "mps is not available", "unavailable")


def accelerator_failure_kind(exc: BaseException) -> str | None:
    """Classify accelerator runtime failures. Other exceptions stay visible."""
    if type(exc).__name__ == "OutOfMemoryError":
        return "oom"
    if not isinstance(exc, RuntimeError):
        return None
    text = str(exc).lower()
    if "out of memory" in text:
        return "oom"
    if any(
        marker in text
        for marker in (
            "not implemented",
            "no kernel image",
            "cuda capability",
            "mps backend",
        )
    ):
        return "unsupported"
    if any(
        marker in text
        for marker in (
            "cuda error",
            "hip error",
            "no cuda gpus",
            "device-side assert",
            "illegal memory access",
        )
    ):
        return "unavailable"
    return None


def device_failure_from(exc: BaseException, device: str) -> DeviceFailure | None:
    """Map a known accelerator error. Unrelated exceptions stay untouched."""
    kind = accelerator_failure_kind(exc)
    if kind is None:
        return None
    return DeviceFailure(device, str(exc), kind)


def accelerator_label(device: str, inventory: AcceleratorInventory) -> str:
    if device.startswith("cuda"):
        return "rocm" if inventory.rocm else "cuda"
    if device == "mps":
        return "mps"
    return "cpu"
