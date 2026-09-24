# Platforms

**Status:** release tooling is configured. No GitHub Release has been published
from this repository state, and the release workflow has not been executed.
Only combinations actually built and tested may be treated as verified.

Checkweave's release artifact is a native Rust binary. Semantic CPU and CUDA
runs are not part of that matrix.

## Release targets

Archives are built on native GitHub-hosted runners. Cross-compilation is not
used: each target below has a standard runner label in the
[GitHub-hosted runners reference](https://docs.github.com/en/actions/reference/runners/github-hosted-runners)
as checked on 2026-09-21. The release workflow pins those labels. It does not
use `ubuntu-latest`, `macos-latest`, or `windows-latest`, because those labels
move to newer images (Ubuntu 26.04 migration is scheduled to begin 2026-10-19).

| Rust target | Runner label | Archive | Verification |
| --- | --- | --- | --- |
| `x86_64-unknown-linux-gnu` | `ubuntu-24.04` | `.tar.gz` | This host built `cargo build --locked --release` and smoked a copied binary. The GitHub runner job has not run. |
| `aarch64-unknown-linux-gnu` | `ubuntu-24.04-arm` | `.tar.gz` | Workflow configured. Not run. |
| `x86_64-apple-darwin` | `macos-15-intel` | `.tar.gz` | Workflow configured. Not run. |
| `aarch64-apple-darwin` | `macos-15` | `.tar.gz` | Workflow configured. Not run. |
| `x86_64-pc-windows-msvc` | `windows-2025` | `.zip` | Workflow configured. Not run. |

Not release targets: 32-bit x86, Windows ARM, Linux musl, and any GPU build.

`.github/workflows/ci.yml` runs `cargo test --locked` on `ubuntu-latest`,
`macos-latest`, and `windows-latest`. That is a separate, floating-label test
matrix. This document does not record a result for it. macOS and Windows remain
unverified until those jobs, or the release jobs, have actually succeeded.

The release workflow runs `cargo test --locked --all-targets` and
`cargo build --locked --release` on each pinned runner. It uploads archives
only when the push is a `v*` tag such as `v0.1.0`. `workflow_dispatch` builds
artifacts for inspection and does not create a GitHub Release. Nothing in the
current tree has pushed such a tag.

## Archive and checksum layout

Version `0.1.0` on the Linux x86_64 target is named:

```text
checkweave-0.1.0-x86_64-unknown-linux-gnu.tar.gz
```

The tag is `v` plus that version. The other names follow the same pattern.
Windows uses `.zip` and the `x86_64-pc-windows-msvc` target. Each archive
contains one top-level directory of the same stem:

```text
checkweave-0.1.0-x86_64-unknown-linux-gnu/
  checkweave          (checkweave.exe on Windows)
  LICENSE
  docs/usage.md
  docs/platforms.md
```

`SHA256SUMS` uses the `sha256sum` text format: 64 hex digits, two spaces, and
the file basename. It lists the archives and the install scripts attached to
the release. Verify before extracting:

```sh
sha256sum -c SHA256SUMS
```

On Windows PowerShell:

```powershell
Get-FileHash -Algorithm SHA256 .\checkweave-0.1.0-x86_64-pc-windows-msvc.zip
```

Download URLs, once a release exists, follow the GitHub convention:

```text
https://github.com/ruizmr/checkweave/releases/download/v0.1.0/checkweave-0.1.0-x86_64-unknown-linux-gnu.tar.gz
https://github.com/ruizmr/checkweave/releases/download/v0.1.0/SHA256SUMS
```

Those URLs are the installer's default. They do not mean a release is present
now. Set `CHECKWEAVE_RELEASE_BASE` to a mirror that serves the same
`/v<version>/` paths.

The archive is the binary, `LICENSE`, and these two docs. The Python tracer
is inside the binary. `trace` writes
`trace-helper/trace-python-v1/checkweave_trace.py` under
`$CHECKWEAVE_CACHE_DIR`, `$XDG_CACHE_HOME/checkweave`, or
`~/.cache/checkweave`. `CHECKWEAVE_TRACE_HELPER` overrides that file. A
copied binary does not need the build checkout. Model setup likewise
materializes the embedded canonical `python/checkweave_worker` package into
the user cache. See [usage](usage.md).

## Models are not a release platform

Local `profile = "default"` is SemIf on `Qwen/Qwen3.5-4B` @
`851bf6e806efd8d0a36b00ddf55e13ccb7b8cd0a`, with the pinned Q4 GGUF for CPU.
The local Q4 sample scored 63/64 supported labels and 11/12 predicate-gold
items. That is not the BF16 JevBench score, and this machine has not run the
534 JevBench items. Automatic GPU is not a release claim. This host stays on
the CPU wheel because the Tesla M10 cannot hold BF16
([Python worker](python-worker.md)). `profile = "lightweight"` is GLiNER2.5
base, 50/64 on the same labels
([semantic validation](semantic-validation.md)). Jev is opt-in and is not a
fallback. djev is a research candidate, not a profile.

[Model backends](model-backends.md) records an older 24-case smoke test on
this class of Linux host. Those runs are not rows in the table above.
Deterministic JSONL checks need no model. `check`, `compare`, `semantic`,
and `trace` are commands; see [usage](usage.md).
