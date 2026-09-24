#!/usr/bin/env python3
"""Copy public task fixtures into the two actor workspaces.

Private expected answers stay in the repository. The two TASK trees are
byte-identical. Directory mode is 02775 (group writable, setgid). File mode
is 0664, or 0775 for Python modules. No arm label is written inside TASK.
"""

from __future__ import annotations

import argparse
import hashlib
import os
import shutil
import stat
from pathlib import Path

ROOT = Path(__file__).resolve().parent
FIXTURES = ROOT / "fixtures"
DEFAULT_STUDY = Path("/tmp/checkweave-agent-study")
ARMS = ("baseline", "checkweave")


def tree_hash(path: Path) -> str:
    digest = hashlib.sha256()
    for item in sorted(p for p in path.rglob("*") if p.is_file()):
        rel = item.relative_to(path).as_posix()
        digest.update(rel.encode())
        digest.update(b"\0")
        digest.update(item.read_bytes())
        digest.update(b"\0")
    return digest.hexdigest()


def apply_modes(path: Path) -> None:
    if path.is_dir():
        os.chmod(path, 0o2775)
        for child in path.iterdir():
            apply_modes(child)
        return
    mode = 0o775 if path.suffix == ".py" else 0o664
    os.chmod(path, mode)


def copy_task(destination: Path) -> None:
    if destination.exists():
        shutil.rmtree(destination)
    shutil.copytree(FIXTURES, destination)
    apply_modes(destination)


def prepare(study: Path, binary: Path | None) -> dict[str, str]:
    study.mkdir(parents=True, exist_ok=True)
    os.chmod(study, 0o2775)
    hashes = {}
    for arm in ARMS:
        arm_root = study / arm
        arm_root.mkdir(parents=True, exist_ok=True)
        os.chmod(arm_root, 0o2775)
        task = arm_root / "TASK"
        copy_task(task)
        submission = arm_root / "submission"
        submission.mkdir(parents=True, exist_ok=True)
        os.chmod(submission, 0o2775)
        hashes[arm] = tree_hash(task)
    if len(set(hashes.values())) != 1:
        raise SystemExit(f"TASK trees differ: {hashes}")
    bin_dir = study / "bin"
    bin_dir.mkdir(parents=True, exist_ok=True)
    os.chmod(bin_dir, 0o2775)
    link = bin_dir / "checkweave"
    if link.is_symlink() or link.exists():
        link.unlink()
    if binary is not None:
        if not binary.is_file():
            raise SystemExit(f"binary not found: {binary}")
        link.symlink_to(binary.resolve())
    for arm in ARMS:
        mode = (study / arm).stat().st_mode
        if not (mode & stat.S_IWGRP):
            raise SystemExit(f"group write missing on {study / arm}")
    return hashes


def main() -> None:
    repo = Path(__file__).resolve().parents[2]
    parser = argparse.ArgumentParser(description="Stage identical actor task trees")
    parser.add_argument("--study", type=Path, default=DEFAULT_STUDY)
    parser.add_argument(
        "--binary",
        type=Path,
        default=repo / "target" / "debug" / "checkweave",
        help="Symlink target for /tmp/.../bin/checkweave. The checkweave actor PATH should include that bin directory.",
    )
    args = parser.parse_args()
    hashes = prepare(args.study, args.binary if args.binary.is_file() else None)
    print(f"task_sha256 {hashes['baseline']}")
    for arm in ARMS:
        print(args.study / arm / "TASK" / "BRIEF.md")


if __name__ == "__main__":
    main()
