#!/bin/sh
# Install the checkweave binary into a user directory.
# Review this script before running it. Do not pipe it into a shell.
# See docs/usage.md. This does not modify configs, caches, or source files.
set -eu

BASE="${CHECKWEAVE_RELEASE_BASE:-https://github.com/ruizmr/checkweave/releases/download}"
BASE="${BASE%/}"
BIN_DIR="${CHECKWEAVE_BIN_DIR:-}"
VERSION=""
ARCHIVE=""
BINARY=""
CHECKSUM=""

usage() {
  printf '%s\n' \
    "usage: install.sh --version VER | --archive FILE | --binary FILE" \
    "       [--checksum SHA256] [--bin-dir DIR]" \
    "" \
    "Default directory: \$HOME/.local/bin (or CHECKWEAVE_BIN_DIR)." \
    "Remote installs verify SHA256SUMS. Local archive and binary installs" \
    "require --checksum. No sudo and no system directories."
  exit "${1:-2}"
}

while [ "$#" -gt 0 ]; do
  case "$1" in
    -h|--help) usage 0 ;;
    --version)
      [ "$#" -ge 2 ] || usage
      VERSION="$2"
      shift 2
      ;;
    --archive)
      [ "$#" -ge 2 ] || usage
      ARCHIVE="$2"
      shift 2
      ;;
    --binary)
      [ "$#" -ge 2 ] || usage
      BINARY="$2"
      shift 2
      ;;
    --checksum)
      [ "$#" -ge 2 ] || usage
      CHECKSUM="$2"
      shift 2
      ;;
    --bin-dir)
      [ "$#" -ge 2 ] || usage
      BIN_DIR="$2"
      shift 2
      ;;
    *)
      printf 'unknown argument: %s\n' "$1" >&2
      usage
      ;;
  esac
done

modes=0
[ -n "$VERSION" ] && modes=$((modes + 1))
[ -n "$ARCHIVE" ] && modes=$((modes + 1))
[ -n "$BINARY" ] && modes=$((modes + 1))
if [ "$modes" -ne 1 ]; then
  printf '%s\n' "pass exactly one of --version, --archive, or --binary" >&2
  usage
fi
if [ -z "$BIN_DIR" ]; then
  BIN_DIR="${HOME:?HOME is required}/.local/bin"
fi

hash_file() {
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$1" | awk '{print tolower($1)}'
  elif command -v shasum >/dev/null 2>&1; then
    shasum -a 256 "$1" | awk '{print tolower($1)}'
  else
    printf '%s\n' "sha256sum or shasum is required" >&2
    return 1
  fi
}

normalize_hash() {
  printf '%s' "$1" | tr 'A-F' 'a-f' | tr -d '[:space:]'
}

require_hash() {
  normalized=$(normalize_hash "$1")
  printf '%s' "$normalized" | grep -Eq '^[0-9a-f]{64}$' || {
    printf '%s\n' "checksum must be 64 hex digits" >&2
    return 1
  }
  printf '%s\n' "$normalized"
}

verify_hash() {
  actual=$(hash_file "$1") || return 1
  expected=$(require_hash "$2") || return 1
  if [ "$actual" != "$expected" ]; then
    printf 'checksum mismatch for %s\n' "$1" >&2
    return 1
  fi
}

hash_for() {
  awk -v name="$2" '
    {
      hash = tolower($1)
      file = $2
      sub(/^\*/, "", file)
      gsub(/\r/, "", file)
      if (file == name) { print hash; found = 1; exit }
    }
    END { if (!found) exit 1 }
  ' "$1"
}

fetch() {
  url="$1"
  dest="$2"
  if command -v curl >/dev/null 2>&1; then
    curl -fsSL --retry 3 --output "$dest" "$url"
  elif command -v wget >/dev/null 2>&1; then
    wget -O "$dest" "$url"
  else
    printf '%s\n' "curl or wget is required for --version; use --archive or --binary" >&2
    return 1
  fi
}

detect_target() {
  os=$(uname -s)
  mach=$(uname -m)
  case "$os:$mach" in
    Linux:x86_64) printf '%s %s\n' x86_64-unknown-linux-gnu tar.gz ;;
    Linux:aarch64|Linux:arm64) printf '%s %s\n' aarch64-unknown-linux-gnu tar.gz ;;
    Darwin:x86_64) printf '%s %s\n' x86_64-apple-darwin tar.gz ;;
    Darwin:arm64|Darwin:aarch64) printf '%s %s\n' aarch64-apple-darwin tar.gz ;;
    *)
      printf 'unsupported platform %s %s; Windows uses install.ps1\n' "$os" "$mach" >&2
      return 1
      ;;
  esac
}

install_atomic() {
  src="$1"
  dest="$2"
  if [ -L "$src" ] || [ ! -f "$src" ]; then
    printf 'source is not a regular file: %s\n' "$src" >&2
    return 1
  fi
  dir=$(dirname "$dest")
  mkdir -p "$dir"
  if [ -d "$dest" ]; then
    printf 'refusing to replace directory %s\n' "$dest" >&2
    return 1
  fi
  tmp=$(mktemp "$dir/.checkweave.install.XXXXXX")
  tmpbin="$tmp"
  cp "$src" "$tmp"
  chmod 755 "$tmp"
  if [ -f "$dest" ] && cmp -s "$dest" "$tmp"; then
    rm -f "$tmp"
    tmpbin=""
    printf 'already installed: %s\n' "$dest"
    return 0
  fi
  mv -f "$tmp" "$dest"
  tmpbin=""
  printf 'installed: %s\n' "$dest"
}

reject_archive_paths() {
  listing=$(tar -tzf "$1") || return 1
  if printf '%s\n' "$listing" | grep -E '(^/|(^|/)\.\.(/|$)|\\)' >/dev/null; then
    printf '%s\n' "archive contains an unsafe path" >&2
    return 1
  fi
}

locate_binary() {
  root="$1"
  name="$2"
  matches=$(find "$root" -type f -name "$name")
  if [ -z "$matches" ]; then
    printf 'archive has no %s binary\n' "$name" >&2
    return 1
  fi
  count=0
  src=""
  oldifs=$IFS
  IFS='
'
  for line in $matches; do
    [ -n "$line" ] || continue
    count=$((count + 1))
    src=$line
  done
  IFS=$oldifs
  if [ "$count" -ne 1 ]; then
    printf 'archive must contain exactly one %s\n' "$name" >&2
    return 1
  fi
  printf '%s\n' "$src"
}

work=$(mktemp -d "${TMPDIR:-/tmp}/checkweave-install.XXXXXX")
case "$work" in
  *checkweave-install.*) ;;
  *)
    printf 'unexpected temp directory\n' >&2
    exit 1
    ;;
esac
tmpbin=""
cleanup() {
  if [ -n "${tmpbin:-}" ] && [ -f "$tmpbin" ]; then
    rm -f "$tmpbin"
  fi
  if [ -n "${work:-}" ]; then
    case "$work" in
      *checkweave-install.*) rm -rf "$work" ;;
    esac
  fi
}
trap cleanup EXIT

if [ -n "$VERSION" ]; then
  case "$VERSION" in
    v*) VERSION=${VERSION#v} ;;
  esac
  case "$VERSION" in
    *[!0-9A-Za-z._+-]*)
      printf 'invalid version\n' >&2
      exit 1
      ;;
  esac
  case "$VERSION" in
    [0-9]*.[0-9]*.[0-9]*) ;;
    *)
      printf 'invalid version\n' >&2
      exit 1
      ;;
  esac
  spec=$(detect_target) || exit 1
  # shellcheck disable=SC2086
  set -- $spec
  target="$1"
  ext="$2"
  name="checkweave-${VERSION}-${target}.${ext}"
  fetch "$BASE/v${VERSION}/$name" "$work/$name"
  fetch "$BASE/v${VERSION}/SHA256SUMS" "$work/SHA256SUMS"
  expected=$(hash_for "$work/SHA256SUMS" "$name") || {
    printf 'SHA256SUMS has no entry for %s\n' "$name" >&2
    exit 1
  }
  verify_hash "$work/$name" "$expected"
  ARCHIVE="$work/$name"
fi

if [ -n "$ARCHIVE" ]; then
  [ -f "$ARCHIVE" ] || {
    printf 'archive not found: %s\n' "$ARCHIVE" >&2
    exit 1
  }
  if [ -z "$VERSION" ]; then
    [ -n "$CHECKSUM" ] || {
      printf '%s\n' "--checksum is required for --archive" >&2
      exit 1
    }
    verify_hash "$ARCHIVE" "$CHECKSUM"
  fi
  reject_archive_paths "$ARCHIVE"
  tar -xzf "$ARCHIVE" -C "$work"
  src=$(locate_binary "$work" checkweave) || exit 1
  install_atomic "$src" "$BIN_DIR/checkweave"
else
  [ -n "$CHECKSUM" ] || {
    printf '%s\n' "--checksum is required for --binary" >&2
    exit 1
  }
  [ -f "$BINARY" ] || {
    printf 'binary not found: %s\n' "$BINARY" >&2
    exit 1
  }
  verify_hash "$BINARY" "$CHECKSUM"
  install_atomic "$BINARY" "$BIN_DIR/checkweave"
fi
