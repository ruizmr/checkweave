"""Fixture integrity and baseline isolation tests."""

from __future__ import annotations

import copy
import inspect
import json
import sys
import tempfile
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

import baselines  # noqa: E402
import runner  # noqa: E402
import sample_data  # noqa: E402


class SampleTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.sample = sample_data.build_sample()
        sample_data.validate_sample(cls.sample, sample_data.exploratory_texts())

    def test_heldout_size_and_families(self):
        label_n = 0
        families = set()
        for case in self.sample["cases"]:
            self.assertEqual(case["source"], "synthetic")
            self.assertEqual(case["split"], "heldout")
            families.add(case["family"])
            for question in case["questions"]:
                if question["expectation"]["type"] == "label":
                    label_n += 1
        self.assertGreaterEqual(label_n, 60)
        self.assertGreaterEqual(len(self.sample["cases"]), 60)
        for family in (
            "collection_routing",
            "negation",
            "absent_evidence",
            "prompt_injection",
            "paraphrase",
            "label_order",
            "multi_question",
            "over_limit",
            "unsupported_language",
            "unsupported_predicate",
        ):
            self.assertIn(family, families)

    def test_not_the_exploratory_set(self):
        exploratory = sample_data.exploratory_texts()
        self.assertGreaterEqual(len(exploratory), 24)
        ours = {case["text"] for case in self.sample["cases"]}
        self.assertTrue(ours.isdisjoint(exploratory))

    def test_written_sample_matches_builder(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "sample.json"
            sample_data.write_sample(path)
            loaded = json.loads(path.read_text(encoding="utf-8"))
        self.assertEqual(loaded, self.sample)


class BaselineTests(unittest.TestCase):
    def test_predictors_do_not_take_expected_labels(self):
        for name in ("predict_fixed_constant", "predict_lexical", "_predict"):
            parameters = inspect.signature(getattr(baselines, name)).parameters
            self.assertNotIn("expected", parameters)
            self.assertNotIn("expectation", parameters)

    def test_expectation_mutation_does_not_change_prediction(self):
        sample = sample_data.build_sample()
        case = next(row for row in sample["cases"] if row["id"] == "sv-neg-01")
        question = case["questions"][0]
        original = baselines.predict_lexical(case, question)
        mutated = copy.deepcopy(question)
        mutated["expectation"]["label"] = "completed"
        self.assertEqual(baselines.predict_lexical(case, mutated), original)
        constant = baselines.predict_fixed_constant(case, question)
        self.assertEqual(constant["label"], "completed")
        self.assertNotEqual(constant["label"], question["expectation"]["label"])

    def test_capability_rules_are_fixed(self):
        sample = sample_data.build_sample()
        predicate = next(row for row in sample["cases"] if row["id"] == "sv-pred-01")
        over = next(row for row in sample["cases"] if row["id"] == "sv-over-01")
        language = next(row for row in sample["cases"] if row["id"] == "sv-lang-01")
        for predict in (baselines.predict_fixed_constant, baselines.predict_lexical):
            self.assertEqual(predict(predicate, predicate["questions"][0])["status"], "unsupported")
            self.assertEqual(predict(over, over["questions"][0])["status"], "unresolved")
            self.assertIsNone(predict(over, over["questions"][0])["label"])
            self.assertEqual(predict(language, language["questions"][0])["status"], "unsupported")

    def test_label_order_constant_is_stable(self):
        sample = sample_data.build_sample()
        left = next(row for row in sample["cases"] if row["id"] == "sv-order-01a")
        right = next(row for row in sample["cases"] if row["id"] == "sv-order-01b")
        left_label = baselines.predict_fixed_constant(left, left["questions"][0])["label"]
        right_label = baselines.predict_fixed_constant(right, right["questions"][0])["label"]
        self.assertEqual(left_label, right_label)
        self.assertEqual(left_label, "access")


class ResponseRowTests(unittest.TestCase):
    def test_one_request_records_only_the_question_it_asked(self):
        sample = sample_data.build_sample()
        case = next(row for row in sample["cases"] if row["id"] == "sv-multi-01")
        question = case["questions"][0]
        response = {
            "version": 1,
            "id": "q",
            "results": [
                {
                    "state_id": case["id"],
                    "question_id": question["id"],
                    "status": "resolved",
                    "label": "incident_record",
                    "scores": {"incident_record": 0.5},
                }
            ],
        }
        rows = runner.predictions_from_response(
            [case],
            {case["id"]: [question]},
            response,
            0.1,
        )
        self.assertEqual(len(rows), 1)
        self.assertEqual(rows[0]["question_id"], question["id"])
        self.assertTrue(rows[0]["covered"])


class PredicateGoldTests(unittest.TestCase):
    def test_gold_is_frozen_and_separate_from_the_64(self):
        payload = runner.load_predicate_gold(runner.PREDICATE_GOLD_PATH)
        self.assertTrue(payload["frozen_before_model_run"])
        sample = sample_data.load_sample()
        label_n = sum(
            1
            for case in sample["cases"]
            for question in case["questions"]
            if question["expectation"]["type"] == "label"
        )
        self.assertEqual(label_n, 64)
        sample_ids = {case["id"] for case in sample["cases"]}
        gold_ids = {case["id"] for case in payload["cases"]}
        self.assertTrue(sample_ids.isdisjoint(gold_ids))


class RunnerBaselineTests(unittest.TestCase):
    def test_baselines_only_does_not_invent_a_model_run(self):
        with tempfile.TemporaryDirectory() as tmp:
            sample_path = Path(tmp) / "sample.json"
            output_path = Path(tmp) / "out.json"
            sample_data.write_sample(sample_path)
            code = runner.main(
                ["--baselines-only", "--sample", str(sample_path), "--output", str(output_path)]
            )
            self.assertEqual(code, 0)
            report = json.loads(output_path.read_text(encoding="utf-8"))
        self.assertFalse(report["fabricated"])
        self.assertFalse(report["model"]["benchmark_run"])
        self.assertIsNone(report["model"]["results"])
        self.assertFalse(report["release_qualified"])
        self.assertIsNone(report["release_gate"])
        self.assertIn("fixed_constant", report["baselines"])
        self.assertIn("lexical_overlap", report["baselines"])
        self.assertFalse(report["baselines"]["fixed_constant"]["scores_are_calibrated"])
        self.assertGreaterEqual(
            report["baselines"]["fixed_constant"]["classification"]["overall"]["items"],
            60,
        )


if __name__ == "__main__":
    unittest.main()
