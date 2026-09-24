"""Fixed baselines for the held-out sample.

Neither baseline reads expected labels and neither is fit on this split.
There is no training split. The constant baseline is the lexicographically
smallest label, not the empirical mode of the held-out answers.
"""

from __future__ import annotations

import re
from typing import Any, Mapping, Optional

# Proxy used only by baselines. The worker applies its own tokenizer.
# 4096 tokens * 4 bytes is a deliberate overestimate for short English text
# and an underestimate only for extremely dense tokenization. Over-limit
# fixtures are far above this ceiling.
BYTE_LIMIT = 4096 * 4

_STOPWORDS = frozenset(
    """
    a an the of to and or for that this with from in on at by is was were be
    been being it its as are do does did has have but if when what which who
    whom how than then into about without within not no nor
    """.split()
)

_TOKEN = re.compile(r"[a-z0-9]+")


def _tokens(text: str) -> set[str]:
    return {
        token
        for token in _TOKEN.findall(text.lower())
        if token not in _STOPWORDS and len(token) > 2
    }


def label_rows(question: Mapping[str, Any]) -> list[Mapping[str, str]]:
    kind = question.get("kind")
    if kind == "choice":
        return list(question.get("labels") or [])
    if kind == "ordinal":
        return list(question.get("levels") or [])
    return []


def fixed_constant_label(question: Mapping[str, Any]) -> Optional[str]:
    names = [row["label"] for row in label_rows(question)]
    if not names:
        return None
    return min(names)


def lexical_overlap_label(text: str, question: Mapping[str, Any]) -> Optional[str]:
    """Pick the label whose name and description share the most text tokens.

    Ties break toward the lexicographically smallest label. No overlap abstains.
    Stopwords include negation words, so this baseline is weak on negation by
    construction. The rule is not adjusted against held-out answers.
    """
    rows = label_rows(question)
    if not rows:
        return None
    text_tokens = _tokens(text)
    scores: dict[str, int] = {}
    for row in rows:
        description = row.get("description") or ""
        scores[row["label"]] = len(text_tokens & _tokens(row["label"].replace("_", " ") + " " + description))
    best = max(scores.values())
    if best <= 0:
        return None
    winners = sorted(label for label, score in scores.items() if score == best)
    return winners[0]


def _over_limit(text: str) -> bool:
    return len(text.encode("utf-8")) > BYTE_LIMIT


def predict_fixed_constant(case: Mapping[str, Any], question: Mapping[str, Any]) -> dict[str, Any]:
    return _predict(case, question, lexical=False)


def predict_lexical(case: Mapping[str, Any], question: Mapping[str, Any]) -> dict[str, Any]:
    return _predict(case, question, lexical=True)


def _predict(case: Mapping[str, Any], question: Mapping[str, Any], lexical: bool) -> dict[str, Any]:
    """Return a protocol-shaped decision. `question["expectation"]` is ignored."""
    if question.get("kind") == "predicate" or case.get("language") != "en":
        return {"covered": True, "status": "unsupported", "label": None, "selected_score": None}
    if _over_limit(case.get("text") or ""):
        return {"covered": True, "status": "unresolved", "label": None, "selected_score": None}
    if lexical:
        label = lexical_overlap_label(case.get("text") or "", question)
        if label is None:
            return {"covered": True, "status": "unresolved", "label": None, "selected_score": None}
    else:
        label = fixed_constant_label(question)
        if label is None:
            return {"covered": True, "status": "unresolved", "label": None, "selected_score": None}
    return {"covered": True, "status": "resolved", "label": label, "selected_score": None}


BASELINES = {
    "fixed_constant": {
        "predict": predict_fixed_constant,
        "rule": (
            "Always the lexicographically smallest choice or ordinal label. "
            "Not the empirical majority of this held-out split. "
            "Predicate questions and non-English text are unsupported. "
            "Text above the documented byte proxy is unresolved."
        ),
    },
    "lexical_overlap": {
        "predict": predict_lexical,
        "rule": (
            "Argmax of token overlap between the text and the label name plus "
            "description, after a fixed stopword list. Ties break lexicographically. "
            "Zero overlap abstains as unresolved. No parameters are fit on labels. "
            "Predicate questions and non-English text are unsupported. "
            "Text above the documented byte proxy is unresolved."
        ),
    },
}
