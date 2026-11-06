"""Unit tests for fake_llm.py's string values. Run with: python3 -m unittest discover -s scripts/llm-bench -p 'test_*.py'"""

from __future__ import annotations

import re
import sys
import time
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

import fake_llm  # noqa: E402


def value(schema):
    return fake_llm.value_for(schema, {"root": True})


class PatternValueTests(unittest.TestCase):
    PATTERNS = [
        r"^[0-9]+$",
        r"^\d+$",
        r"^inv_[a-z0-9]+$",
        r"^[A-Z]{3}-\d{4}$",
        r"^(draft|sent|paid)$",
        r"^(?:cus|sub)_[A-Za-z0-9]{14}$",
        r"^[a-f0-9]{24}$",
        r"^[1-9][0-9]{0,5}$",
        r"^[A-Z][a-z]+ [A-Z][a-z]+$",
        r"^\+[1-9]\d{1,14}$",
        r"^[^@\s]+@[^@\s]+\.[a-z]{2,}$",
        r"^v\d+\.\d+(\.\d+)?$",
        r"^ch_\w{8,}$",
        r"^[a-z0-9-]{3,63}$",
        r"^[A-Z]{2}\.[0-9]{3}$",
        r"^.{5}$",
    ]

    def test_values_satisfy_simple_patterns(self):
        for pattern in self.PATTERNS:
            with self.subTest(pattern=pattern):
                got = value({"type": "string", "pattern": pattern})
                self.assertIsInstance(got, str)
                self.assertIsNotNone(re.search(pattern, got), f"{got!r} does not match {pattern}")

    def test_digits_only_invoice_id(self):
        got = value({"type": "string", "pattern": "^[0-9]+$"})
        self.assertRegex(got, r"^[0-9]+$")

    def test_alternation_of_literals_picks_a_literal(self):
        self.assertIn(value({"type": "string", "pattern": "^(draft|sent|paid)$"}), {"draft", "sent", "paid"})

    def test_deterministic(self):
        schema = {"type": "object", "properties": {
            "invoice_id": {"type": "string", "pattern": "^[0-9]+$"},
            "customer_id": {"type": "string", "pattern": "^cus_[a-z0-9]+$"},
            "name": {"type": "string"},
        }}
        self.assertEqual(value(schema), value(schema))

    def test_unsatisfiable_or_unsupported_pattern_keeps_todays_value(self):
        for pattern in [r"^(?=x)y$", r"^(a)\1$", r"^[0-9]+$" + "[", r"^\bfoo$x"]:
            with self.subTest(pattern=pattern):
                self.assertRegex(value({"type": "string", "pattern": pattern}), r"^val\d+$")

    def test_value_already_matching_is_unchanged(self):
        # An unanchored pattern today's value already satisfies does not change it.
        plain = value({"type": "string"})
        self.assertEqual(value({"type": "string", "pattern": "[a-z]"}), plain)

    def test_length_bounds_still_hold(self):
        got = value({"type": "string", "pattern": "^[0-9]+$", "minLength": 6, "maxLength": 8})
        self.assertRegex(got, r"^[0-9]{6,8}$")

    def test_conflicting_length_falls_back(self):
        # The pattern allows exactly 2 characters but minLength wants 5: no pattern value fits.
        got = value({"type": "string", "pattern": "^[0-9]{2}$", "minLength": 5})
        self.assertEqual(got, "val1x")

    def test_no_pattern_unchanged(self):
        self.assertEqual(value({"type": "string"}), "val1")
        self.assertEqual(value({"type": "string", "minLength": 6}), "val1xx")
        self.assertEqual(value({"type": "string", "format": "email", "pattern": "^x$"}), "user@example.com")


    def test_pinned_values(self):
        # Fixed expected values, so determinism is checked against a known answer and
        # not only against a second call in the same process.
        self.assertEqual(value({"type": "string", "pattern": "^[0-9]+$"}), "123")
        self.assertEqual(value({"type": "string", "pattern": "^inv_[a-z0-9]+$"}), "inv_fgh")
        schema = {"type": "object", "properties": {
            "name": {"type": "string"},
            "invoice_id": {"type": "string", "pattern": "^[0-9]+$"},
        }}
        self.assertEqual(value(schema), {"name": "val2", "invoice_id": "345"})

    def test_alternation_tries_later_branches_for_length(self):
        got = value({"type": "string", "pattern": "^(a|bbbbb)$", "minLength": 5})
        self.assertEqual(got, "bbbbb")
        got = value({"type": "string", "pattern": "^(?:long_[0-9]+|id)$", "maxLength": 2})
        self.assertEqual(got, "id")

    def test_short_spread_tried_when_the_default_is_too_long(self):
        got = value({"type": "string", "pattern": r"^(\d+)+$", "maxLength": 2})
        self.assertRegex(got, r"^\d{1,2}$")

    def test_first_branch_still_preferred(self):
        self.assertEqual(value({"type": "string", "pattern": "^(draft|sent|paid)$"}), "draft")


class MatcherTests(unittest.TestCase):
    """`matches` agrees with `re.search` on patterns where backtracking is cheap. Negated
    sets and `.` draw from a fixed pool, so outside it `matches` may only say no."""

    UNIVERSE = set(fake_llm.POOL + "-._~")

    PATTERNS = [
        r"^[a-z]+$", r"[0-9]{3}", r"^a|b$", r"^(ab|cd)*e?$", r"x{2,3}", r"^$", r"a^b", r"a$",
        r"^\d{2}-\d{2}$", r"[^a-c]+", r"\W", r"^(?:a?){3}$", r"^.{2}$", r"(a|)+b", r"^\s*x\s*$",
        r"^val\d+$", r"^[A-Z][a-z]*(?:_[a-z]+)*$", r"\.", r"^(?:x|xy)(?:z|yz)$", r"a{0}b",
    ]
    STRINGS = ["", "a", "b", "ab", "abcde", "cdabe", "xx", "xxx", "x", "12-34", "123", "a\n",
               "val1", "Abc_de", " x ", "xyz", "b.c", "d", "aab", "e", "ba", "\n"]

    def test_agrees_with_re(self):
        for pattern in self.PATTERNS:
            tree = fake_llm.PatternParser(pattern).parse()
            for s in self.STRINGS:
                with self.subTest(pattern=pattern, s=s):
                    want = re.search(pattern, s) is not None
                    got = fake_llm.matches(tree, s, fake_llm.Budget(fake_llm.MAX_PATTERN_STEPS))
                    if set(s) <= self.UNIVERSE:
                        self.assertEqual(got, want)
                    else:
                        self.assertFalse(got and not want)


class CostBoundTests(unittest.TestCase):
    """Adversarial patterns: each returns quickly, with the plain value when no bounded value fits."""

    BOUND_S = 0.25  # the target is ~50 ms; the slack is for a loaded machine

    def timed(self, schema):
        start = time.perf_counter()
        got = value(schema)
        elapsed = time.perf_counter() - start
        self.assertLess(elapsed, self.BOUND_S, f"{schema} took {elapsed:.3f} s")
        return got

    def test_nested_repeats_with_max_length_zero(self):
        got = self.timed({"type": "string", "pattern": r"^(((\d+)+)+)+$", "maxLength": 0})
        self.assertEqual(got, "")

    def test_nested_repeats_unbounded(self):
        got = self.timed({"type": "string", "pattern": r"^(((\d+)+)+)+$"})
        self.assertRegex(got, r"^\d+$")
        self.assertLessEqual(len(got), fake_llm.MAX_PATTERN_LEN)

    def test_huge_fixed_count_is_unsupported(self):
        got = self.timed({"type": "string", "pattern": r"\d{100000000}"})
        self.assertEqual(got, "val1")
        # A huge upper bound alone is fine: the value uses the lower one.
        got = self.timed({"type": "string", "pattern": r"^x{5,100000000}$"})
        self.assertEqual(got, "xxxxx")

    def test_nested_fixed_counts_under_the_cap(self):
        for pattern in [r"^((x{4000}){4000}){4000}$", r"^(((){4000}){4000}){4000}y$", r"^((a|b){4000}){4000}$"]:
            with self.subTest(pattern=pattern):
                got = self.timed({"type": "string", "pattern": pattern})
                self.assertLessEqual(len(got), fake_llm.MAX_PATTERN_LEN)

    def test_wide_alternation_with_impossible_length(self):
        branches = "|".join(f"(?:b{i}[0-9]+)+" for i in range(200))
        got = self.timed({"type": "string", "pattern": f"^({branches})$", "minLength": 3000, "maxLength": 3001})
        self.assertTrue(got == "val1" + "x" * 2996 or re.search(f"^({branches})$", got))

    def test_padded_plain_value_on_exponential_pattern(self):
        # `re.search` on the padded plain value ('val1' + 24 'x') backtracks for seconds.
        got = self.timed({"type": "string", "pattern": r"(x+x+)+y", "minLength": 28})
        self.assertGreaterEqual(len(got), 28)

    def test_generated_value_on_nested_nullable_repeats(self):
        # The generated value fits at once, but `re.search` on it does not finish.
        pattern = r"^(?:(?:(?:(?:[a-z]{0,4000}){0,4000}){0,4000}){0,4000})y{4000}$"
        got = self.timed({"type": "string", "pattern": pattern})
        self.assertLessEqual(len(got), fake_llm.MAX_PATTERN_LEN)
        self.assertRegex(got, r"^[a-z]*y{4000}$")  # the same language, cheap for `re`

    def test_misplaced_anchors_pick_another_branch(self):
        for pattern in [r"a^b|c", r"a$b|^c$"]:
            with self.subTest(pattern=pattern):
                got = self.timed({"type": "string", "pattern": pattern})
                self.assertRegex(got, pattern)

    def test_short_plain_value_on_nested_stars(self):
        got = self.timed({"type": "string", "pattern": r"(((((((((.*)*)*)*)*)*)*)*)*)*Z"})
        self.assertTrue(got == "val1" or got.endswith("Z"))

    def test_large_min_length_is_bounded(self):
        got = self.timed({"type": "string", "pattern": r"^[0-9]+$", "minLength": 100000})
        self.assertEqual(len(got), 100000)  # the plain value, padded as today


if __name__ == "__main__":
    unittest.main()
