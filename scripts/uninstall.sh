#!/bin/sh
# Remove one selected checkweave binary. Does not delete configs or caches.
# See docs/usage.md for the manual cache and agent-integration procedure.
set -eu

BIN_DIR="${CHECKWEAVE_BIN_DIR:-}"
BIN=""

usage() {
  printf '%s\n' \
    "usage: uninstall.sh [--bin-dir DIR] [--bin PATH]" \
    "" \
    "Removes only the selected checkweave file. Preserves workspace" \
    ".checkweave/ caches, agent configuration, and every other path."
  exit "${1:-2}"
}

while [ "$#" -gt 0 ]; do
  case "$1" in
    -h|--help) usage 0 ;;
    --bin-dir)
      [ "$#" -ge 2 ] || usage
      BIN_DIR="$2"
      shift 2
      ;;
    --bin)
      [ "$#" -ge 2 ] || usage
      BIN="$2"
      shift 2
      ;;
    *)
      printf 'unknown argument: %s\n' "$1" >&2
      usage
      ;;
  esac
done

if [ -z "$BIN" ]; then
  if [ -z "$BIN_DIR" ]; then
    BIN_DIR="${HOME:?HOME is required}/.local/bin"
  fi
  BIN="$BIN_DIR/checkweave"
fi

if [ -z "$BIN" ] || [ "$BIN" = "/" ]; then
  printf '%s\n' "refusing empty or root path" >&2
  exit 1
fi

if [ ! -e "$BIN" ] && [ ! -L "$BIN" ]; then
  printf 'no binary at %s (no other files were modified)\n' "$BIN"
  exit 0
fi

if [ -d "$BIN" ]; then
  printf 'refusing to remove directory %s\n' "$BIN" >&2
  exit 1
fi

if [ ! -f "$BIN" ]; then
  printf 'refusing to remove non-file %s\n' "$BIN" >&2
  exit 1
fi

rm -f -- "$BIN"
printf 'removed %s (workspace cache and agent configuration were not modified)\n' "$BIN"
