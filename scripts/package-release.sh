#!/bin/bash
# Build the release tar.gz the installers extract.
# Source docs are not modified. Links to files that are not shipped are
# rewritten in the archive copy to this commit on GitHub.
set -euo pipefail

usage() {
  printf '%s\n' \
    "usage: package-release.sh --version VER --target TRIPLE --binary FILE --commit SHA [--out DIR] [--ext tar.gz]" \
    "" \
    "VERSION, TARGET, ARCHIVE_EXT, and COMMIT (or GITHUB_SHA) are accepted" \
    "from the environment when the matching flag is omitted." \
    "Writes dist/checkweave-VER-TRIPLE.tar.gz and fails if a shipped" \
    "Markdown link still points outside the archive."
  exit "${1:-2}"
}

fail() {
  printf 'package-release: %s\n' "$1" >&2
  exit 1
}

VERSION="${VERSION:-}"
TARGET="${TARGET:-}"
BINARY=""
COMMIT="${COMMIT:-${GITHUB_SHA:-}}"
EXT="${ARCHIVE_EXT:-tar.gz}"
OUT=""

while [ "$#" -gt 0 ]; do
  case "$1" in
    -h|--help) usage 0 ;;
    --version)
      [ "$#" -ge 2 ] || usage
      VERSION="$2"
      shift 2
      ;;
    --target)
      [ "$#" -ge 2 ] || usage
      TARGET="$2"
      shift 2
      ;;
    --binary)
      [ "$#" -ge 2 ] || usage
      BINARY="$2"
      shift 2
      ;;
    --commit)
      [ "$#" -ge 2 ] || usage
      COMMIT="$2"
      shift 2
      ;;
    --out)
      [ "$#" -ge 2 ] || usage
      OUT="$2"
      shift 2
      ;;
    --ext)
      [ "$#" -ge 2 ] || usage
      EXT="$2"
      shift 2
      ;;
    *)
      printf 'unknown argument: %s\n' "$1" >&2
      usage
      ;;
  esac
done

root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
if [ -z "$OUT" ]; then
  OUT="$root/dist"
fi
if [ -z "$BINARY" ]; then
  BINARY="$root/target/${TARGET}/release/checkweave"
fi

[ -n "$VERSION" ] || fail "missing --version"
[ -n "$TARGET" ] || fail "missing --target"
[ -n "$COMMIT" ] || fail "missing --commit"
case "$VERSION" in
  *[!0-9A-Za-z._+-]*|"") fail "invalid version: $VERSION" ;;
esac
case "$TARGET" in
  *[!0-9A-Za-z_-]*|"") fail "invalid target: $TARGET" ;;
esac
[ "${#COMMIT}" -eq 40 ] || fail "commit must be 40 hex digits"
case "$COMMIT" in
  *[!0-9a-fA-F]*) fail "commit must be 40 hex digits" ;;
esac
case "$EXT" in
  tar.gz) ;;
  *) fail "unsupported archive extension: $EXT" ;;
esac
[ -f "$BINARY" ] || fail "binary not found: $BINARY"
[ -f "$root/LICENSE" ] || fail "missing LICENSE"
[ -f "$root/README.md" ] || fail "missing README.md"
[ -f "$root/CONTRIBUTING.md" ] || fail "missing CONTRIBUTING.md"
command -v python3 >/dev/null 2>&1 || fail "python3 is required"
command -v tar >/dev/null 2>&1 || fail "tar is required"

member="checkweave-${VERSION}-${TARGET}"
stage=$(mktemp -d "${TMPDIR:-/tmp}/checkweave-package.XXXXXX")
cleanup() {
  case "${stage:-}" in
    *checkweave-package.*) rm -rf "$stage" ;;
  esac
}
trap cleanup EXIT

dest="$stage/$member"
mkdir -p "$dest/docs"
cp "$BINARY" "$dest/checkweave"
chmod 755 "$dest/checkweave"
cp "$root/LICENSE" "$dest/LICENSE"
cp "$root/README.md" "$dest/README.md"
cp "$root/CONTRIBUTING.md" "$dest/CONTRIBUTING.md"
mkdir -p "$dest/examples"
cp -R "$root/examples/recipes" "$dest/examples/recipes"
found_docs=0
for doc in "$root"/docs/*.md; do
  [ -f "$doc" ] || fail "no docs/*.md guides to package"
  cp "$doc" "$dest/docs/$(basename "$doc")"
  found_docs=1
done
[ "$found_docs" -eq 1 ] || fail "no docs were copied"
for required in \
  docs/README.md \
  docs/getting-started.md \
  docs/mcp.md \
  docs/usage.md \
  docs/platforms.md \
  docs/roadmap.md
do
  [ -f "$dest/$required" ] || fail "missing packaged guide: $required"
done

export CW_REPO="$root"
export CW_STAGE="$dest"
export CW_COMMIT="$COMMIT"
python3 - <<'PY'
import os
import re
import sys
from pathlib import Path
from urllib.parse import quote

stage = Path(os.environ["CW_STAGE"])
commit = os.environ["CW_COMMIT"].lower()
checkout = Path(os.environ["CW_REPO"])
origin = "https://github.com/ruizmr/checkweave"

packaged = set()
for path in stage.rglob("*"):
    packaged.add(path.relative_to(stage).as_posix())

inline_re = re.compile(r"(!?\[[^\]\n]*\])\(([^)\n]+)\)")
ref_re = re.compile(r"^(\[[^\]\n]+\]:[ \t]+)(\S+)([ \t]+.*)?$")
fence_re = re.compile(r"^(```+|~~~+)")

def split_target(dest: str) -> tuple[str, str]:
    dest = dest.strip()
    title = ""
    if dest.startswith("<") and ">" in dest:
        end = dest.find(">")
        url = dest[1:end]
        title = dest[end + 1 :]
        return url.strip(), title
    match = re.match(r"(\S+)(\s+.*)?$", dest)
    if not match:
        return dest, ""
    return match.group(1), match.group(2) or ""

def join_target(url: str, title: str, wrapped: bool) -> str:
    body = f"<{url}>" if wrapped else url
    if title.strip():
        return f"{body}{title if title.startswith((' ', '\t')) else ' ' + title}"
    return body

def normalize(base: str, url: str) -> str:
    parts: list[str] = []
    start = base
    if url.startswith("/"):
        start = ""
    for part in [*start.split("/"), *url.split("/")]:
        if part in ("", "."):
            continue
        if part == "..":
            if not parts:
                raise ValueError(f"link escapes archive root: {url}")
            parts.pop()
            continue
        if "\\" in part or part == "":
            raise ValueError(f"unsafe link path: {url}")
        parts.append(part)
    return "/".join(parts)

def rewrite_url(source: str, raw: str) -> str:
    url, title = split_target(raw)
    wrapped = raw.strip().startswith("<")
    if not url or url.startswith("#"):
        return raw
    if re.match(r"^[a-zA-Z][a-zA-Z0-9+.-]*:", url):
        return raw
    path, sep, fragment = url.partition("#")
    if path == "":
        return raw
    resolved = normalize(str(Path(source).parent.as_posix()), path)
    if resolved in packaged:
        relative = os.path.relpath(resolved, start=str(Path(source).parent.as_posix()))
        relative = Path(relative).as_posix()
        if fragment:
            relative = f"{relative}#{fragment}"
        return join_target(relative, title, wrapped)
    kind = "tree" if (checkout / resolved).is_dir() else "blob"
    quoted = "/".join(quote(part) for part in resolved.split("/"))
    absolute = f"{origin}/{kind}/{commit}/{quoted}"
    if fragment:
        absolute = f"{absolute}#{fragment}"
    return join_target(absolute, title, wrapped)

def rewrite_text(source: str, text: str) -> str:
    fenced = False
    lines = []
    for line in text.splitlines(keepends=True):
        probe = line.lstrip("\n")
        if fence_re.match(probe.strip()):
            fenced = not fenced
            lines.append(line)
            continue
        if fenced:
            lines.append(line)
            continue
        def replace_inline(match: re.Match[str]) -> str:
            return match.group(1) + "(" + rewrite_url(source, match.group(2)) + ")"
        updated = inline_re.sub(replace_inline, line)
        ref = ref_re.match(updated.rstrip("\n"))
        if ref and not updated.lstrip().startswith("    "):
            ending = "\n" if updated.endswith("\n") else ""
            body = updated.rstrip("\n")
            matched = ref_re.match(body)
            if matched:
                updated = (
                    matched.group(1)
                    + rewrite_url(source, matched.group(2) + (matched.group(3) or ""))
                    + ending
                )
        lines.append(updated)
    return "".join(lines)

for path in sorted(stage.rglob("*.md")):
    source = path.relative_to(stage).as_posix()
    original = path.read_text(encoding="utf-8")
    rewritten = rewrite_text(source, original)
    if rewritten != original:
        path.write_text(rewritten, encoding="utf-8")

broken = []
for path in sorted(stage.rglob("*.md")):
    source = path.relative_to(stage).as_posix()
    fenced = False
    for line in path.read_text(encoding="utf-8").splitlines():
        if fence_re.match(line.strip()):
            fenced = not fenced
            continue
        if fenced:
            continue
        for match in inline_re.finditer(line):
            url, _title = split_target(match.group(2))
            if re.match(r"^[a-zA-Z][a-zA-Z0-9+.-]*:", url) or url.startswith("#"):
                continue
            path_part = url.split("#", 1)[0]
            if path_part == "":
                continue
            resolved = normalize(str(Path(source).parent.as_posix()), path_part)
            if resolved not in packaged and not (stage / resolved).exists():
                broken.append(f"{source}: {url}")
if broken:
    sys.stderr.write("packaged markdown still has links outside the archive:\n")
    sys.stderr.write("\n".join(broken) + "\n")
    sys.exit(1)
PY

mkdir -p "$OUT"
archive="$OUT/${member}.tar.gz"
tar -C "$stage" -czf "$archive" "$member"
list=$(tar -tzf "$archive")
needles=(
  "${member}/checkweave"
  "${member}/LICENSE"
  "${member}/README.md"
  "${member}/CONTRIBUTING.md"
  "${member}/docs/README.md"
  "${member}/docs/getting-started.md"
  "${member}/docs/mcp.md"
  "${member}/docs/usage.md"
  "${member}/docs/platforms.md"
)
for needle in "${needles[@]}"; do
  if ! printf '%s\n' "$list" | grep -Fxq "$needle"; then
    printf 'archive is missing %s\n' "$needle" >&2
    printf '%s\n' "$list" >&2
    exit 1
  fi
done
count=$(printf '%s\n' "$list" | grep -c '/checkweave$' || true)
if [ "$count" -ne 1 ]; then
  fail "archive must contain exactly one checkweave binary, found $count"
fi
digest=$(python3 - "$archive" <<'PY'
import hashlib
import sys
from pathlib import Path
path = Path(sys.argv[1])
print(hashlib.sha256(path.read_bytes()).hexdigest())
PY
)
printf 'archive: %s\nsha256: %s\n' "$archive" "$digest" >&2
