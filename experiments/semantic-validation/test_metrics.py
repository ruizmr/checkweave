"""Unit tests for metric arithmetic on a known confusion pattern."""

from __future__ import annotations

import sys
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

import metrics  # noqa: E402


def _item(**overrides):
    row = {
        "id": "x",
        "family": "demo",
        "question_id": "q",
        "expected_type": "label",
        "expected_status": "resolved",
        "expected_label": "cat",
        "covered": True,
        "status": "resolved",
        "label": "cat",
        "selected_score": None,
    }
    row.update(overrides)
    return row


class PercentileTests(unittest.TestCase):
    def test_interpolation(self):
        self.assertEqual(metrics.percentile([1, 2, 3, 4], 50), 2.5)
        self.assertAlmostEqual(metrics.percentile([1, 2, 3, 4], 95), 3.85)
        self.assertIsNone(metrics.percentile([], 50))
        self.assertEqual(metrics.percentile([7], 95), 7.0)


class ConfusionTests(unittest.TestCase):
    def setUp(self):
        # Two cats: one correct at 0.95, one predicted dog at 0.91.
        # One dog correct at 0.40. Two birds missed: unresolved, and missing.
        self.items = [
            _item(id="c1", expected_label="cat", label="cat", selected_score=0.95),
            _item(id="c2", expected_label="cat", label="dog", selected_score=0.91),
            _item(id="d1", expected_label="dog", label="dog", selected_score=0.40),
            _item(id="b1", expected_label="bird", label=None, status="unresolved", selected_score=None),
            _item(id="b2", expected_label="bird", label=None, status=None, covered=False, selected_score=None),
        ]
        self.family = metrics.classification_report(self.items)["families"]["demo"]

    def test_label_counts(self):
        labels = self.family["labels"]
        self.assertEqual(labels["cat"], {"tp": 1, "fp": 0, "fn": 1, "precision": 1.0, "recall": 0.5})
        self.assertEqual(labels["dog"]["tp"], 1)
        self.assertEqual(labels["dog"]["fp"], 1)
        self.assertEqual(labels["dog"]["fn"], 0)
        self.assertEqual(labels["dog"]["precision"], 0.5)
        self.assertEqual(labels["dog"]["recall"], 1.0)
        self.assertEqual(labels["bird"]["tp"], 0)
        self.assertEqual(labels["bird"]["fp"], 0)
        self.assertEqual(labels["bird"]["fn"], 2)
        self.assertIsNone(labels["bird"]["precision"])
        self.assertEqual(labels["bird"]["recall"], 0.0)

    def test_macro_and_micro(self):
        self.assertAlmostEqual(self.family["macro_precision"], 0.75)
        self.assertAlmostEqual(self.family["macro_recall"], 0.5)
        self.assertAlmostEqual(self.family["micro_precision"], 2 / 3)
        self.assertAlmostEqual(self.family["micro_recall"], 2 / 5)
        self.assertEqual(self.family["correct"], 2)
        self.assertAlmostEqual(self.family["accuracy_on_covered"], 0.5)
        self.assertAlmostEqual(self.family["accuracy_on_all"], 0.4)

    def test_confusion_cells(self):
        confusion = self.family["confusion"]
        self.assertEqual(confusion["cat"]["cat"], 1)
        self.assertEqual(confusion["cat"]["dog"], 1)
        self.assertEqual(confusion["bird"][metrics.UNRESOLVED], 1)
        self.assertEqual(confusion["bird"][metrics.MISSING], 1)
        self.assertEqual(confusion["dog"]["dog"], 1)


class CoverageAndScoreTests(unittest.TestCase):
    def test_coverage_is_separate_from_accuracy(self):
        items = [
            _item(id="c1", expected_label="cat", label="cat"),
            _item(id="c2", expected_label="cat", label="dog", selected_score=0.91),
            _item(id="d1", expected_label="dog", label="dog", selected_score=0.40),
            _item(id="b1", expected_label="bird", label=None, status="unresolved"),
            _item(id="b2", expected_label="bird", label=None, covered=False),
        ]
        coverage = metrics.coverage_report(items)
        self.assertEqual(coverage["protocol_coverage"], 0.8)
        self.assertEqual(coverage["accuracy_on_covered_supported"], 0.5)
        self.assertNotEqual(coverage["protocol_coverage"], coverage["accuracy_on_covered_supported"])

    def test_high_score_errors_are_not_called_calibrated(self):
        items = [
            _item(id="c1", label="cat", selected_score=0.95),
            _item(id="c2", expected_label="cat", label="dog", selected_score=0.91),
            _item(id="d1", expected_label="dog", label="dog", selected_score=0.40),
            _item(id="m1", expected_label="bird", label="cat", selected_score=0.4),
        ]
        report = metrics.high_score_errors(items)
        self.assertFalse(report["scores_are_calibrated"])
        self.assertEqual(report["counts"]["0.8"], 1)
        self.assertEqual(report["counts"]["0.9"], 1)
        self.assertEqual(report["items"][0]["id"], "c2")

    def test_status_rates(self):
        items = [
            _item(status="resolved", label="cat"),
            _item(status="unsupported", label=None),
            _item(status="unresolved", label=None),
            _item(covered=False, status=None, label=None),
        ]
        rates = metrics.status_rates(items)
        self.assertEqual(rates["unsupported_rate"], 0.25)
        self.assertEqual(rates["unresolved_rate"], 0.25)
        self.assertEqual(rates["missing_rate"], 0.25)

    def test_capability_and_release_gate(self):
        items = [
            _item(),
            {
                "id": "p",
                "family": "unsupported_predicate",
                "question_id": "claim",
                "expected_type": "capability",
                "expected_status": "unsupported",
                "expected_label": None,
                "covered": True,
                "status": "unsupported",
                "label": None,
            },
            {
                "id": "o",
                "family": "over_limit",
                "question_id": "collection",
                "expected_type": "capability",
                "expected_status": "unresolved",
                "expected_label": None,
                "covered": True,
                "status": "resolved",
                "label": "how_to",
                "selected_score": 0.99,
            },
            {
                "id": "lang",
                "family": "unsupported_language",
                "question_id": "collection",
                "expected_type": "capability",
                "expected_status": "outside_supported_profile",
                "expected_label": None,
                "covered": True,
                "status": "resolved",
                "label": "how_to",
                "selected_score": 0.99,
            },
        ]
        report = metrics.assemble_report(items, latencies=[0.1, 0.2, 0.4])
        self.assertFalse(report["release_qualified"])
        self.assertIsNone(report["release_gate"])
        self.assertFalse(report["scores_are_calibrated"])
        self.assertEqual(report["capability"]["graded_items"], 2)
        self.assertEqual(report["capability"]["agreed"], 1)
        self.assertEqual(report["capability"]["outside_profile_items"], 1)
        timeout = {
            "expected_type": "capability",
            "expected_status": "unresolved",
            "covered": False,
            "status": None,
            "label": None,
            "request_error": "timed out waiting for a protocol line",
        }
        rejected = dict(timeout, request_error="input exceeds max_input_tokens")
        self.assertFalse(metrics.capability_match(timeout))
        self.assertTrue(metrics.capability_match(rejected))
        self.assertTrue(report["capability"]["outside_profile_excluded_from_accuracy"])
        self.assertEqual(report["classification"]["overall"]["items"], 1)
        self.assertEqual(report["high_score_errors"]["counts"]["0.9"], 0)
        self.assertEqual(report["latency_warm_seconds"]["p50_seconds"], 0.2)

    def test_predicate_insufficient_option_is_correct_when_remapped(self):
        item = _item(
            question_kind="predicate",
            family="predicate_gold",
            expected_label="insufficient",
            status="unresolved",
            label="insufficient",
        )
        self.assertTrue(metrics.is_correct(item))
        report = metrics.classification_report([item])
        self.assertEqual(report["overall"]["correct"], 1)
        wrong = _item(
            question_kind="predicate",
            expected_label="supported",
            status="resolved",
            label="contradicted",
            selected_score=0.97,
        )
        errors = metrics.high_score_errors([wrong])
        self.assertEqual(errors["counts"]["0.9"], 1)

    def test_ungraded_unsupported_rows_are_not_capability_failures(self):
        report = metrics.capability_report(
            [
                {
                    "expected_type": "capability",
                    "expected_status": "unsupported",
                    "capability_graded": False,
                    "covered": True,
                    "status": "resolved",
                    "label": "supported",
                }
            ]
        )
        self.assertEqual(report["graded_items"], 0)
        self.assertEqual(report["ungraded_profile_rows"], 1)

    def test_perfect_accuracy_still_has_no_release_gate(self):
        report = metrics.assemble_report([_item()])
        self.assertEqual(report["classification"]["overall"]["accuracy_on_covered"], 1.0)
        self.assertFalse(report["release_qualified"])

    def test_independence(self):
        rate = metrics.independence_rate(
            [
                {"joint_label": "how_to", "separate_label": "how_to"},
                {"joint_label": "none", "separate_label": "all_users"},
                {"joint_label": None, "separate_label": "how_to"},
            ]
        )
        self.assertEqual(rate["comparable"], 2)
        self.assertEqual(rate["same_label"], 1)
        self.assertEqual(rate["agreement"], 0.5)


if __name__ == "__main__":
    unittest.main()
