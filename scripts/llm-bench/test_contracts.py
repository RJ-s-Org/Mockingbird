"""Contracts parse as written: no flow mapping loses text to an unquoted comma.
Run: python3 -m unittest scripts/llm-bench/test_contracts.py"""

import os
import sys
import unittest

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))

import flow_yaml_lint  # noqa: E402


class FlowMappingLintTest(unittest.TestCase):
    def lint(self, text):
        return [message for _, message in flow_yaml_lint.lint_text(text)]

    def test_an_unquoted_comma_in_a_description_is_caught(self):
        found = self.lint("name: {type: string, description: Shown as typed, in its displayed case.}\n")
        self.assertEqual(found, ["bare key 'in its displayed case.' reads as a null value"])
        self.assertEqual(len(self.lint("a:\n  - {x: 1, y}\n  - b: {c: {d: 2, e}}\n")), 2)
        self.assertEqual(len(self.lint("a: {x: 1,\n    y: }\n")), 1)

    def test_valid_flow_mappings_pass(self):
        clean = "\n".join([
            'name: {type: string, description: "Shown as typed, in its displayed case."}',
            "a: {type: integer, description: The card's number on its board.}",
            "b: {pattern: '^[A-Za-z0-9]{8}$', example: null, other: ~}",
            "c: {$ref: '#/components/schemas/X'}  # a comment, with a comma",
            "sasl: {mechanism: PLAIN, password: env:RABBIT_PASSWORD}",
            'd: {"json": 1, "nested": {"k": [1, 2]}}',
            "e: {}",
            "f: [one, two, {g: 1}]",
            "description: Returns {id}, the id, of the thing",
            "text: |",
            "  {not, yaml, here}",
            "h: {i: 1}",
            "",
        ])
        self.assertEqual(self.lint(clean), [])

    def test_every_contract_is_clean(self):
        files = flow_yaml_lint.contract_files()
        for blind in ("blind3", "blind4", "blind5", "blind6", "blind7", "blind8", "blind9", "blind10", "blind11"):
            self.assertTrue(any(f"/contracts/{blind}/" in f for f in files), blind)
        found = [f"{os.path.relpath(path, flow_yaml_lint.REPO)}:{line}: {message}"
                 for path in files for line, message in flow_yaml_lint.lint_file(path)]
        self.assertEqual(found, [])


if __name__ == "__main__":
    unittest.main()
