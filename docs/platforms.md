# Supported platforms

Checkweave runs on Linux and macOS. On Windows, run it inside WSL2.

| Your system | How to run Checkweave | Verified on 2026-09-24 |
| --- | --- | --- |
| Linux, Intel/AMD or ARM64 | Native binary | Tests, build, and packaging |
| macOS, Intel or Apple silicon | Native binary | Tests, build, and packaging |
| Windows | Linux binary inside WSL2 | x86_64 install, reinstall, collection check, and uninstall |

The [release dry run](https://github.com/ruizmr/checkweave/actions/runs/36070429830)
passed, but no tag or GitHub Release has been published as of 2026-09-24.
Follow [Getting started](getting-started.md) to install from source.
Native Windows is not yet supported. The rest of this page records exact
build targets and packaging details.

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
| `x86_64-unknown-linux-gnu` | `ubuntu-24.04` | `.tar.gz` | Release workflow dry run (2026-09-24): tests and release build passed on the runner. This host also smoked a copied binary. |
| `aarch64-unknown-linux-gnu` | `ubuntu-24.04-arm` | `.tar.gz` | Release workflow dry run (2026-09-24): tests and release build passed on the runner. |
| `x86_64-apple-darwin` | `macos-15-intel` | `.tar.gz` | Tests and release build passed in [release run 36070429830](https://github.com/ruizmr/checkweave/actions/runs/36070429830), after fixing socket paths over 104 bytes and `/var` → `/private/var`. |
| `aarch64-apple-darwin` | `macos-15` | `.tar.gz` | Tests and release build passed in the same run. |

Not release targets: native Windows, 32-bit x86, Linux musl, and any GPU build.

## Windows through WSL2

Windows runs the Linux archive inside WSL2. `scripts/install.ps1` checks
that WSL has a working distribution, verifies `install.sh` against
`SHA256SUMS`, and runs it in that distribution; `install.sh` then verifies
and installs the Linux archive into `~/.local/bin`. On Windows ARM the same
flow picks the `aarch64-unknown-linux-gnu` archive. The release workflow's
`wsl-install` job runs `install.ps1` on `windows-2025` with Ubuntu 24.04
under WSL2, checks a collection, and uninstalls; publishing waits on it. It
passed in [release run 36070429830](https://github.com/ruizmr/checkweave/actions/runs/36070429830): install, same-bytes reinstall, a complete JSONL check,
uninstall, and a no-op second uninstall.

Open the project from WSL (`wsl`, `cd` into it, `cursor .`) so Cursor starts
the MCP server inside Linux. Keep projects in the WSL filesystem: under
`/mnt/c` file access is slow and native change events are unreliable, so the
daemon falls back to polling.

Native Windows builds compile, and `ci.yml` still runs them with
`continue-on-error`. The first native test run failed about 30 tests
(verbatim `\\?\` paths, process termination, named-pipe shutdown), so native
Windows is not a release target.

`.github/workflows/ci.yml` runs fmt, clippy, and
`cargo test --locked --all-targets --no-fail-fast` on `ubuntu-latest`,
`macos-latest`, and `windows-latest`. That is a separate, floating-label test
matrix. The Windows job may fail without failing the workflow.

The release workflow runs `cargo test --locked --all-targets` and
`cargo build --locked --release` on each pinned runner. It publishes archives
to a GitHub Release only when the push is a `v*` tag such as `v0.1.0`.
`workflow_dispatch` uploads workflow artifacts for inspection and does not
create a GitHub Release.

## Archive and checksum layout

Version `0.1.0` on the Linux x86_64 target is named:

```text
checkweave-0.1.0-x86_64-unknown-linux-gnu.tar.gz
```

The tag is `v` plus that version. The other names follow the same pattern.
Each archive contains one top-level directory of the same stem:

```text
checkweave-0.1.0-x86_64-unknown-linux-gnu/
  checkweave
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

On Windows, `install.ps1` does this check itself.

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
