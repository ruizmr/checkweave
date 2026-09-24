"""Keep library prints off the protocol stdout stream."""

from __future__ import annotations

import sys
from contextlib import contextmanager


@contextmanager
def library_quiet():
    """Send incidental library stdout to stderr for the duration of the block."""
    original = sys.stdout
    sys.stdout = sys.stderr
    try:
        yield
    finally:
        sys.stdout = original
