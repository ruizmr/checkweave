"""Protocol and checkpoint limits for the local inference worker."""

from __future__ import annotations

PROTOCOL_VERSION = 1
MAX_FRAME_BYTES = 8 * 1024 * 1024
DEFAULT_MAX_INPUT_TOKENS = 4096
HARD_MAX_INPUT_TOKENS = 4096
MAX_STATES = 32
MAX_QUESTIONS = 16
MAX_PAIRS = 64
MAX_LABELS = 32
MAX_LABEL_CHARS = 200
MAX_DESCRIPTION_CHARS = 1000
MAX_ID_CHARS = 200
MAX_STATEMENT_CHARS = 4000
MAX_TEXT_CHARS = 1_000_000
UPSTREAM_BATCH_SIZE = 8
DEFAULT_THREADS = 4
MIN_THREADS = 1
MAX_THREADS = 32

MODEL_ID = "fastino/gliner2.5-base-v1"
MODEL_REVISION = "1a8bc24e00dc7300b9017c81d63e3dcdabb26596"
PROVIDER = "local"
PRECISION = "float32"
LANGUAGE = "en"

INPUT_POLICY = (
    "full_serialized_input_including_schema_and_labels; "
    "reject_over_limit; no_truncation; omitted_nothing"
)

# Mirrors gliner2.classification.schema reserved marker tokens.
RESERVED_MARKERS = (
    "[P]",
    "[L]",
    "[C]",
    "[E]",
    "[R]",
    "[DESCRIPTION]",
    "[EXAMPLE]",
    "[OUTPUT]",
    "(",
    ")",
)

PREDICATE_REASON = (
    "local profile does not support predicate judgments; "
    "the pinned checkpoint was not qualified to distinguish missing evidence, "
    "and a low score is not used as a proxy"
)

SCORE_TOLERANCE = 1e-3
ARGMAX_TOLERANCE = 1e-6
