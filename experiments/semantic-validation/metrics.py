"""Metrics for the held-out semantic sample.

Model scores are recorded as returned. Nothing here treats them as calibrated
probabilities of correctness.
"""

from __future__ import annotations

import math
from collections import Counter, defaultdict
from typing import Any, Mapping, Optional, Sequence

UNRESOLVED = "<unresolved>"
UNSUPPORTED = "<unsupported>"
MISSING = "<missing>"

# Fixed cutoffs on the selected label's reported score. Not probabilities.
HIGH_SCORE_THRESHOLDS = (0.8, 0.9)


def percentile(values: Sequence[float], pct: float) -> Optional[float]:
    """Linear interpolation between nearest ranks. pct is in [0, 100]."""
    if pct < 0 or pct > 100:
        raise ValueError("pct must be between 0 and 100")
    if not values:
        return None
    ordered = sorted(float(value) for value in values)
    if len(ordered) == 1:
        return ordered[0]
    rank = (len(ordered) - 1) * (pct / 100.0)
    low = math.floor(rank)
    high = math.ceil(rank)
    if low == high:
        return ordered[low]
    weight = rank - low
    return ordered[low] * (1.0 - weight) + ordered[high] * weight


def _div(num: float, den: float) -> Optional[float]:
    if den == 0:
        return None
    return num / den


def is_correct(item: Mapping[str, Any]) -> bool:
    """Whether the returned option matches the authored label.

    SemIf's predicate schema includes ``insufficient``. The worker keeps that
    option as ``label`` and reports protocol status ``unresolved``. That remap
    is still the insufficient option, not a missing result.
    """
    if not item.get("covered") or not item.get("label"):
        return False
    if item.get("label") != item.get("expected_label"):
        return False
    if item.get("status") == "resolved":
        return True
    return (
        item.get("question_kind") == "predicate"
        and item.get("label") == "insufficient"
        and item.get("status") == "unresolved"
    )


def has_prediction(item: Mapping[str, Any]) -> bool:
    if not item.get("covered") or not item.get("label"):
        return False
    if item.get("status") == "resolved":
        return True
    return (
        item.get("question_kind") == "predicate"
        and item.get("label") == "insufficient"
        and item.get("status") == "unresolved"
    )


def _bucket(item: Mapping[str, Any]) -> str:
    if not item.get("covered"):
        return MISSING
    if has_prediction(item):
        return str(item["label"])
    status = item.get("status")
    if status == "unsupported":
        return UNSUPPORTED
    if status == "unresolved":
        return UNRESOLVED
    return MISSING


def _family_scores(rows: Sequence[Mapping[str, Any]]) -> dict[str, Any]:
    gold = [row["expected_label"] for row in rows]
    predicted = [row["label"] for row in rows if has_prediction(row)]
    matrix_labels = sorted(set(gold) | set(predicted))
    extra = (UNRESOLVED, UNSUPPORTED, MISSING)
    confusion = {
        expected: {col: 0 for col in list(matrix_labels) + list(extra)}
        for expected in matrix_labels
    }
    per = {label: {"tp": 0, "fp": 0, "fn": 0} for label in matrix_labels}
    for row in rows:
        expected = row["expected_label"]
        chosen = _bucket(row)
        if expected not in confusion:
            confusion[expected] = {col: 0 for col in list(matrix_labels) + list(extra)}
        confusion[expected][chosen] = confusion[expected].get(chosen, 0) + 1
        if chosen == expected:
            per[expected]["tp"] += 1
        else:
            per[expected]["fn"] += 1
            if chosen in per:
                per[chosen]["fp"] += 1

    label_scores: dict[str, Any] = {}
    macro_p = []
    macro_r = []
    for label, counts in per.items():
        precision = _div(counts["tp"], counts["tp"] + counts["fp"])
        recall = _div(counts["tp"], counts["tp"] + counts["fn"])
        label_scores[label] = {
            "tp": counts["tp"],
            "fp": counts["fp"],
            "fn": counts["fn"],
            "precision": precision,
            "recall": recall,
        }
        if precision is not None:
            macro_p.append(precision)
        if counts["tp"] + counts["fn"] > 0 and recall is not None:
            macro_r.append(recall)

    correct = sum(1 for row in rows if is_correct(row))
    covered = sum(1 for row in rows if row.get("covered"))
    resolved = sum(1 for row in rows if has_prediction(row))
    return {
        "items": len(rows),
        "covered": covered,
        "resolved_predictions": resolved,
        "correct": correct,
        "accuracy_on_covered": _div(correct, covered),
        "accuracy_on_all": _div(correct, len(rows)),
        "micro_precision": _div(correct, resolved),
        "micro_recall": _div(correct, len(rows)),
        "macro_precision": (sum(macro_p) / len(macro_p)) if macro_p else None,
        "macro_recall": (sum(macro_r) / len(macro_r)) if macro_r else None,
        "labels": label_scores,
        "confusion": confusion,
    }


def classification_report(items: Sequence[Mapping[str, Any]]) -> dict[str, Any]:
    """Per-family precision, recall, and confusion for expected-label items.

    Label strings are scoped to a family. The overall block is micro accuracy
    only, so identical strings in different families are not merged.
    """
    label_items = [item for item in items if item.get("expected_type") == "label"]
    grouped: dict[str, list[Mapping[str, Any]]] = defaultdict(list)
    for item in label_items:
        grouped[str(item["family"])].append(item)
    scored = _family_scores(label_items)
    return {
        "overall": {
            "items": scored["items"],
            "covered": scored["covered"],
            "resolved_predictions": scored["resolved_predictions"],
            "correct": scored["correct"],
            "accuracy_on_covered": scored["accuracy_on_covered"],
            "accuracy_on_all": scored["accuracy_on_all"],
            "micro_precision": scored["micro_precision"],
            "micro_recall": scored["micro_recall"],
        },
        "families": {name: _family_scores(rows) for name, rows in sorted(grouped.items())},
    }


def coverage_report(items: Sequence[Mapping[str, Any]]) -> dict[str, Any]:
    """Protocol coverage and correctness on covered supported items, kept apart."""
    total = len(items)
    covered_n = sum(1 for item in items if item.get("covered"))
    supported = [item for item in items if item.get("expected_type") == "label"]
    supported_covered = [item for item in supported if item.get("covered")]
    correct = sum(1 for item in supported_covered if is_correct(item))
    return {
        "items": total,
        "covered": covered_n,
        "protocol_coverage": _div(covered_n, total),
        "supported_label_items": len(supported),
        "supported_label_covered": len(supported_covered),
        "correct_on_covered_supported": correct,
        "accuracy_on_covered_supported": _div(correct, len(supported_covered)),
        "note": (
            "protocol_coverage counts items with a worker or baseline result. "
            "accuracy_on_covered_supported uses only covered supported-label items. "
            "Uncovered items stay in the coverage rate and out of that accuracy denominator."
        ),
    }


def status_rates(items: Sequence[Mapping[str, Any]]) -> dict[str, Any]:
    counts: Counter[str] = Counter()
    for item in items:
        if not item.get("covered"):
            counts["missing"] += 1
        else:
            counts[str(item.get("status") or "missing")] += 1
    total = len(items)
    return {
        "items": total,
        "counts": dict(counts),
        "unsupported_rate": _div(counts["unsupported"], total),
        "unresolved_rate": _div(counts["unresolved"], total),
        "resolved_rate": _div(counts["resolved"], total),
        "missing_rate": _div(counts["missing"], total),
    }


def _selected_score(item: Mapping[str, Any]) -> Optional[float]:
    score = item.get("selected_score")
    if isinstance(score, bool) or not isinstance(score, (int, float)):
        return None
    return float(score)


def high_score_errors(
    items: Sequence[Mapping[str, Any]],
    thresholds: Sequence[float] = HIGH_SCORE_THRESHOLDS,
) -> dict[str, Any]:
    """Wrong resolved labels whose reported score clears a fixed cutoff.

    The cutoff is not a calibrated confidence. A high score is still a model score.
    """
    counts = {str(threshold): 0 for threshold in thresholds}
    recorded = []
    for item in items:
        if item.get("expected_type") != "label":
            continue
        if not has_prediction(item) or is_correct(item):
            continue
        score = _selected_score(item)
        if score is None:
            continue
        met = [threshold for threshold in thresholds if score >= threshold]
        if not met:
            continue
        for threshold in met:
            counts[str(threshold)] += 1
        recorded.append(
            {
                "id": item.get("id"),
                "family": item.get("family"),
                "question_id": item.get("question_id"),
                "expected_label": item.get("expected_label"),
                "label": item.get("label"),
                "selected_score": score,
                "thresholds_met": met,
            }
        )
    return {
        "scores_are_calibrated": False,
        "note": (
            "Selected-label scores are uncalibrated outputs. "
            "Crossing a threshold does not mean the model is probably correct."
        ),
        "thresholds": list(thresholds),
        "counts": counts,
        "items": recorded,
    }


def _explicit_limit_error(text: Any) -> bool:
    if not isinstance(text, str):
        return False
    lowered = text.lower()
    needles = ("token", "max_input", "too long", "exceed", "input limit", "context")
    return any(needle in lowered for needle in needles)


def capability_match(item: Mapping[str, Any]) -> Optional[bool]:
    """Whether a capability expectation was met. None means it is not graded."""
    if item.get("expected_type") != "capability":
        return None
    if item.get("capability_graded") is False:
        return None
    expected = item.get("expected_status")
    if expected == "outside_supported_profile":
        return None
    label = item.get("label")
    if expected == "unsupported":
        return bool(item.get("covered") and item.get("status") == "unsupported" and not label)
    if expected == "unresolved":
        if label:
            return False
        if item.get("status") == "unresolved":
            return True
        # A transport timeout is not an input-limit rejection.
        return bool(item.get("request_error") and _explicit_limit_error(item.get("request_error")))
    return False


def capability_report(items: Sequence[Mapping[str, Any]]) -> dict[str, Any]:
    graded: list[bool] = []
    outside: list[str] = []
    ungraded = 0
    for item in items:
        if item.get("expected_type") != "capability":
            continue
        if item.get("capability_graded") is False:
            ungraded += 1
            continue
        if item.get("expected_status") == "outside_supported_profile":
            outside.append(str(item.get("status") if item.get("covered") else "missing"))
            continue
        matched = capability_match(item)
        if matched is not None:
            graded.append(matched)
    agreed = sum(1 for matched in graded if matched)
    return {
        "graded_items": len(graded),
        "agreed": agreed,
        "agreement": _div(agreed, len(graded)),
        "ungraded_profile_rows": ungraded,
        "outside_profile_items": len(outside),
        "outside_profile_observed_status": dict(Counter(outside)),
        "outside_profile_excluded_from_accuracy": True,
    }


def latency_summary(seconds: Sequence[float]) -> dict[str, Any]:
    return {
        "n": len(seconds),
        "p50_seconds": percentile(seconds, 50),
        "p95_seconds": percentile(seconds, 95),
    }


def independence_rate(pairs: Sequence[Mapping[str, Any]]) -> dict[str, Any]:
    comparable = [
        pair
        for pair in pairs
        if pair.get("joint_label") is not None and pair.get("separate_label") is not None
    ]
    same = sum(1 for pair in comparable if pair["joint_label"] == pair["separate_label"])
    return {
        "comparable": len(comparable),
        "same_label": same,
        "agreement": _div(same, len(comparable)),
        "note": (
            "Agreement between a joint request and separate single-question requests. "
            "This is not classification accuracy."
        ),
    }


def adapter_mapping_failures(items: Sequence[Mapping[str, Any]]) -> dict[str, Any]:
    """Decoder or schema mismatches, kept apart from a wrong resolved label."""
    markers = (
        "score labels do not match",
        "decoder label disagrees",
        "outside the pinned option",
        "model returned no label distribution",
    )
    found = []
    for item in items:
        reason = str(item.get("reason") or "")
        if any(marker in reason for marker in markers):
            found.append(
                {
                    "id": item.get("id"),
                    "question_id": item.get("question_id"),
                    "family": item.get("family"),
                    "status": item.get("status"),
                    "label": item.get("label"),
                    "reason": reason,
                }
            )
    return {
        "count": len(found),
        "items": found,
        "note": (
            "These rows failed inside the adapter mapping. "
            "They are not rewritten into gold labels."
        ),
    }


def assemble_report(
    items: Sequence[Mapping[str, Any]],
    latencies: Optional[Sequence[float]] = None,
    independence: Optional[Sequence[Mapping[str, Any]]] = None,
) -> dict[str, Any]:
    return {
        "scores_are_calibrated": False,
        "release_gate": None,
        "release_qualified": False,
        "release_note": (
            "No numeric release gate is defined. A score on this sample does not "
            "qualify a model for release."
        ),
        "classification": classification_report(items),
        "coverage": coverage_report(items),
        "status_rates": status_rates(items),
        "high_score_errors": high_score_errors(items),
        "adapter_mapping_failures": adapter_mapping_failures(items),
        "capability": capability_report(items),
        "latency_warm_seconds": latency_summary(list(latencies or [])),
        "question_independence": independence_rate(list(independence or [])),
    }
