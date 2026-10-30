"""Tests for the W1 harness fixes: echo-aware grading, fallback causes and
header-safe session ids. Run: python3 -m unittest scripts/llm-bench/test_w1.py"""

import os
import sys
import unittest

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))

import aggregate  # noqa: E402
import grade  # noqa: E402
import server_suites  # noqa: E402

UUID_ID = {"type": "object", "properties": {"id": {"type": "string", "format": "uuid"}}}


class EchoTest(unittest.TestCase):
    def test_echoed_path_id_is_not_a_violation(self):
        sample = {"schema": UUID_ID, "body": {"id": "garnet"}, "request": {"path": {"id": "garnet"}}}
        g = grade.grade_sample(sample, "m", "k")
        self.assertTrue(g["valid"])
        self.assertEqual([e["path"] for e in g["echoed"]], ["/id"])

    def test_invented_bad_id_still_fails(self):
        sample = {"schema": UUID_ID, "body": {"id": "garnet"}, "request": {"path": {"id": "kestrel"}}}
        g = grade.grade_sample(sample, "m", "k")
        self.assertFalse(g["valid"])
        self.assertEqual(g["echoed"], [])

    def test_only_format_violations_can_be_echoes(self):
        schema = {"type": "object", "properties": {"id": {"type": "string", "pattern": "^pi_"}}}
        sample = {"schema": schema, "body": {"id": "garnet"}, "request": {"query": {"q": ["garnet"]}}}
        self.assertFalse(grade.grade_sample(sample, "m", "k")["valid"])


class CauseTest(unittest.TestCase):
    def test_causes(self):
        cases = {
            "LLM output truncated: provider X, stopped at max_tokens (16384)": "truncated",
            "repaired output still invalid: output is not valid JSON": "non_json",
            "repaired output still invalid: /a: type: x": "schema",
            "LLM request timed out": "timeout",
            "timed out waiting for a generation slot": "guard",
            "token budget exhausted": "guard",
            "response of 9 bytes exceeds the 8-byte cap": "guard",
            "LLM returned HTTP 400: nope": "other",
            "": "other",
        }
        for reason, cause in cases.items():
            self.assertEqual(aggregate.fallback_cause({"fallback_reason": reason}), cause, reason)
        self.assertTrue(set(cases.values()) <= set(aggregate.FALLBACK_CAUSES))


class PatchedSourceTest(unittest.TestCase):
    def test_patched_counts_as_schema_valid_and_is_reported_apart(self):
        items = [({"source": src}, None) for src in ("llm", "patched", "repaired", "fallback")]
        t = aggregate.tally(items, has_grades=False)
        self.assertEqual(t["schema_valid"], 0.75)
        self.assertEqual(t["patch"], 0.25)
        self.assertEqual(t["repair"], 0.25)
        self.assertIn("patched", aggregate.ENGINE_SOURCES)


class SessionSlugTest(unittest.TestCase):
    def test_variant_suffix_is_header_safe(self):
        self.assertEqual(server_suites.session_slug("thinkingmachines/inkling@nojson"),
                         "thinkingmachines__inkling-nojson")


if __name__ == "__main__":
    unittest.main()
