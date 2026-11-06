"""Unit tests for fake_llm.py's string values. Run with: python3 -m unittest discover -s scripts/llm-bench -p 'test_*.py'"""

from __future__ import annotations

import re
import sys
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


if __name__ == "__main__":
    unittest.main()
