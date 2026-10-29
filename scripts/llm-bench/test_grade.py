"""Unit tests for grade.py. Run with: python3 -m unittest discover -s scripts/llm-bench -p 'test_*.py'"""

from __future__ import annotations

import io
import json
import contextlib
import sys
import tempfile
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

import grade  # noqa: E402


def check(instance, schema, root=None):
    return grade.Validator(schema if root is None else root).validate(instance, schema)


def keywords(result):
    return sorted({v["keyword"] for v in result.violations})


class TypeTests(unittest.TestCase):
    def test_basic_types(self):
        self.assertTrue(check("x", {"type": "string"}).checked["type"] == 1)
        self.assertEqual(check("x", {"type": "string"}).violations, [])
        self.assertEqual(keywords(check(1, {"type": "string"})), ["type"])
        self.assertEqual(check(None, {"type": "null"}).violations, [])
        self.assertEqual(check(True, {"type": "boolean"}).violations, [])
        self.assertEqual(check([1], {"type": "array"}).violations, [])
        self.assertEqual(check({"a": 1}, {"type": "object"}).violations, [])

    def test_integer_vs_number(self):
        self.assertEqual(check(3, {"type": "integer"}).violations, [])
        self.assertEqual(check(2.0, {"type": "integer"}).violations, [])
        self.assertEqual(keywords(check(1.5, {"type": "integer"})), ["type"])
        self.assertEqual(check(1.5, {"type": "number"}).violations, [])
        self.assertEqual(check(2, {"type": "number"}).violations, [])

    def test_bool_is_not_number(self):
        self.assertEqual(keywords(check(True, {"type": "integer"})), ["type"])
        self.assertEqual(keywords(check(False, {"type": "number"})), ["type"])

    def test_type_list(self):
        schema = {"type": ["string", "null"]}
        self.assertEqual(check(None, schema).violations, [])
        self.assertEqual(check("a", schema).violations, [])
        self.assertEqual(keywords(check(1, schema)), ["type"])

    def test_unknown_type_is_skipped(self):
        result = check(1, {"type": "decimal"})
        self.assertEqual(result.violations, [])
        self.assertEqual(result.skipped["type"], 1)


class RequiredEnumConstTests(unittest.TestCase):
    def test_required(self):
        schema = {"type": "object", "required": ["id", "name"]}
        result = check({"id": 1}, schema)
        self.assertEqual(keywords(result), ["required"])
        self.assertEqual(result.violations[0]["path"], "/name")
        self.assertEqual(check({"id": 1, "name": "x"}, schema).violations, [])

    def test_enum(self):
        schema = {"enum": ["a", "b", 1]}
        self.assertEqual(check("a", schema).violations, [])
        self.assertEqual(keywords(check("c", schema)), ["enum"])

    def test_enum_does_not_confuse_bool_and_int(self):
        self.assertEqual(keywords(check(True, {"enum": [1]})), ["enum"])
        self.assertEqual(keywords(check(1, {"enum": [True]})), ["enum"])
        self.assertEqual(check(1.0, {"enum": [1]}).violations, [])

    def test_const(self):
        self.assertEqual(check("x", {"const": "x"}).violations, [])
        self.assertEqual(keywords(check("y", {"const": "x"})), ["const"])
        self.assertEqual(keywords(check(0, {"const": False})), ["const"])

    def test_enum_category_counts(self):
        result = check("c", {"enum": ["a"], "const": "c"})
        self.assertEqual(result.checked["enum"], 1)
        self.assertEqual(result.checked["const"], 1)
        self.assertEqual(result.failed["enum"], 1)
        self.assertEqual(result.failed.get("const", 0), 0)


class FormatTests(unittest.TestCase):
    def assertFormat(self, fmt, good, bad):
        for value in good:
            with self.subTest(format=fmt, value=value):
                self.assertEqual(check(value, {"format": fmt}).violations, [], value)
        for value in bad:
            with self.subTest(format=fmt, value=value):
                self.assertEqual(keywords(check(value, {"format": fmt})), ["format"], value)

    def test_date_time(self):
        self.assertFormat(
            "date-time",
            ["2026-10-29T12:30:00Z", "2026-10-29T12:30:00.123+02:00", "2024-02-29t00:00:00z"],
            ["2026-02-30T00:00:00Z", "2026-10-29T12:30:00", "2026-10-29 12:30:00Z", "yesterday"],
        )

    def test_date(self):
        self.assertFormat("date", ["2026-10-29", "2024-02-29"], ["2026-13-01", "2026-02-29", "29-10-2026"])

    def test_time(self):
        self.assertFormat("time", ["12:30:00Z", "00:00:00.5-05:00"], ["25:00:00Z", "12:30:00", "12:30"])

    def test_email(self):
        self.assertFormat("email", ["a@b.co"], ["ab.co", "a@b", "a b@c.de"])

    def test_uri_and_url(self):
        for fmt in ("uri", "url"):
            self.assertFormat(fmt, ["https://example.com/a?b=1", "mailto:x@y.z", "urn:isbn:123"], ["example.com", "http://", "ht tp://x"])

    def test_uuid(self):
        self.assertFormat(
            "uuid",
            ["123e4567-e89b-12d3-a456-426614174000"],
            ["123e4567-e89b-12d3-a456-42661417400", "not-a-uuid"],
        )

    def test_ipv4(self):
        self.assertFormat("ipv4", ["192.168.0.1", "0.0.0.0"], ["256.1.1.1", "1.2.3", "::1"])

    def test_ipv6(self):
        self.assertFormat("ipv6", ["::1", "2001:db8::8a2e:370:7334"], ["12345::", "192.168.0.1"])

    def test_hostname(self):
        self.assertFormat("hostname", ["example.com", "a-b.c1"], ["-bad.com", "a..b", "x" * 64 + ".com", "under_score.com"])

    def test_format_ignored_for_non_strings(self):
        result = check(42, {"format": "email"})
        self.assertEqual(result.violations, [])
        self.assertEqual(result.checked.get("format", 0), 0)

    def test_int32_and_int64(self):
        self.assertEqual(check(2**31 - 1, {"format": "int32"}).violations, [])
        self.assertEqual(keywords(check(2**31, {"format": "int32"})), ["format"])
        self.assertEqual(check(2**40, {"format": "int64"}).violations, [])

    def test_unknown_format_is_skipped(self):
        result = check("abc", {"format": "password"})
        self.assertEqual(result.violations, [])
        self.assertEqual(result.skipped["format:password"], 1)


class NumericTests(unittest.TestCase):
    def test_minimum_maximum_inclusive(self):
        schema = {"minimum": 1, "maximum": 10}
        self.assertEqual(check(1, schema).violations, [])
        self.assertEqual(check(10, schema).violations, [])
        self.assertEqual(keywords(check(0, schema)), ["minimum"])
        self.assertEqual(keywords(check(11, schema)), ["maximum"])

    def test_exclusive_json_schema_form(self):
        schema = {"exclusiveMinimum": 0, "exclusiveMaximum": 5}
        self.assertEqual(check(1, schema).violations, [])
        self.assertEqual(keywords(check(0, schema)), ["exclusiveMinimum"])
        self.assertEqual(keywords(check(5, schema)), ["exclusiveMaximum"])

    def test_exclusive_openapi_30_boolean_form(self):
        schema = {"minimum": 0, "exclusiveMinimum": True, "maximum": 5, "exclusiveMaximum": True}
        self.assertEqual(check(1, schema).violations, [])
        self.assertEqual(keywords(check(0, schema)), ["minimum"])
        self.assertEqual(keywords(check(5, schema)), ["maximum"])
        non_strict = {"minimum": 0, "exclusiveMinimum": False}
        self.assertEqual(check(0, non_strict).violations, [])

    def test_numeric_keywords_ignore_non_numbers(self):
        result = check("12", {"minimum": 100})
        self.assertEqual(result.violations, [])
        self.assertEqual(result.checked.get("minimum", 0), 0)

    def test_bool_is_not_range_checked(self):
        self.assertEqual(check(True, {"minimum": 5}).violations, [])


class StringTests(unittest.TestCase):
    def test_min_max_length(self):
        schema = {"minLength": 2, "maxLength": 4}
        self.assertEqual(check("abc", schema).violations, [])
        self.assertEqual(keywords(check("a", schema)), ["minLength"])
        self.assertEqual(keywords(check("abcde", schema)), ["maxLength"])

    def test_pattern_is_unanchored_search(self):
        self.assertEqual(check("pi_123", {"pattern": "^pi_[0-9]+$"}).violations, [])
        self.assertEqual(check("x pi_1 y", {"pattern": "pi_[0-9]"}).violations, [])
        self.assertEqual(keywords(check("abc", {"pattern": "^[0-9]+$"})), ["pattern"])

    def test_invalid_pattern_is_skipped(self):
        result = check("abc", {"pattern": "([unclosed"})
        self.assertEqual(result.violations, [])
        self.assertEqual(result.skipped["pattern"], 1)


class ArrayTests(unittest.TestCase):
    def test_min_max_items(self):
        schema = {"minItems": 1, "maxItems": 2}
        self.assertEqual(check([1], schema).violations, [])
        self.assertEqual(keywords(check([], schema)), ["minItems"])
        self.assertEqual(keywords(check([1, 2, 3], schema)), ["maxItems"])

    def test_unique_items(self):
        self.assertEqual(check([1, 2], {"uniqueItems": True}).violations, [])
        self.assertEqual(keywords(check([1, 1.0], {"uniqueItems": True})), ["uniqueItems"])
        self.assertEqual(check([1, True], {"uniqueItems": True}).violations, [])

    def test_items_schema_and_path(self):
        schema = {"type": "array", "items": {"type": "integer"}}
        result = check([1, "x", 3], schema)
        self.assertEqual(keywords(result), ["type"])
        self.assertEqual(result.violations[0]["path"], "/1")

    def test_tuple_items(self):
        schema = {"items": [{"type": "string"}, {"type": "integer"}]}
        self.assertEqual(check(["a", 1, "extra"], schema).violations, [])
        self.assertEqual(keywords(check([1, 1], schema)), ["type"])

    def test_prefix_items_with_items_tail(self):
        schema = {"prefixItems": [{"type": "string"}], "items": {"type": "integer"}}
        self.assertEqual(check(["a", 1, 2], schema).violations, [])
        result = check(["a", "b"], schema)
        self.assertEqual(result.violations[0]["path"], "/1")
        self.assertEqual(keywords(result), ["type"])


class ObjectTests(unittest.TestCase):
    def test_properties_recursion_and_path_escaping(self):
        schema = {
            "type": "object",
            "properties": {
                "a/b": {"type": "object", "properties": {"items": {"type": "array", "items": {"type": "string", "maxLength": 2}}}},
            },
        }
        result = check({"a/b": {"items": ["ok", "toolong"]}}, schema)
        self.assertEqual(keywords(result), ["maxLength"])
        self.assertEqual(result.violations[0]["path"], "/a~1b/items/1")

    def test_additional_properties_false(self):
        schema = {"properties": {"a": {}}, "additionalProperties": False}
        self.assertEqual(check({"a": 1}, schema).violations, [])
        result = check({"a": 1, "b": 2, "c": 3}, schema)
        self.assertEqual(keywords(result), ["additionalProperties"])
        self.assertEqual(sorted(v["path"] for v in result.violations), ["/b", "/c"])

    def test_additional_properties_respects_pattern_properties(self):
        schema = {
            "properties": {"a": {}},
            "patternProperties": {"^x-": {}},
            "additionalProperties": False,
        }
        self.assertEqual(check({"a": 1, "x-foo": 2}, schema).violations, [])
        self.assertEqual(keywords(check({"a": 1, "y": 2}, schema)), ["additionalProperties"])

    def test_additional_properties_schema(self):
        schema = {"additionalProperties": {"type": "integer"}}
        self.assertEqual(check({"a": 1}, schema).violations, [])
        self.assertEqual(keywords(check({"a": "x"}, schema)), ["type"])

    def test_additional_properties_true_is_noop(self):
        self.assertEqual(check({"a": 1}, {"additionalProperties": True}).violations, [])


class CombinatorTests(unittest.TestCase):
    def test_one_of_exactly_one(self):
        schema = {"oneOf": [{"type": "string"}, {"type": "integer"}]}
        self.assertEqual(check("a", schema).violations, [])
        self.assertEqual(keywords(check(True, schema)), ["oneOf"])

    def test_one_of_rejects_zero_and_many(self):
        schema = {"oneOf": [{"minimum": 0}, {"maximum": 10}]}
        self.assertEqual(check(-5, schema).violations, [])  # only the maximum branch matches
        self.assertEqual(keywords(check(5, schema)), ["oneOf"])  # matches both
        zero = {"oneOf": [{"minimum": 0}, {"maximum": -10}]}
        self.assertEqual(keywords(check(-5, zero)), ["oneOf"])  # matches neither

    def test_any_of(self):
        schema = {"anyOf": [{"type": "string", "maxLength": 1}, {"type": "integer"}]}
        self.assertEqual(check("a", schema).violations, [])
        self.assertEqual(check(2, schema).violations, [])
        self.assertEqual(keywords(check("abc", schema)), ["anyOf"])

    def test_all_of_reports_branch_violations(self):
        schema = {"allOf": [{"required": ["id"]}, {"properties": {"n": {"type": "integer"}}}]}
        result = check({"n": "x"}, schema)
        self.assertEqual(sorted(keywords(result)), ["required", "type"])
        self.assertEqual(result.checked["allOf"], 1)

    def test_combinator_branch_counts_only_from_passing_branch(self):
        schema = {"oneOf": [{"type": "string", "maxLength": 1}, {"type": "string"}]}
        result = check("abc", schema)
        self.assertEqual(result.violations, [])
        self.assertEqual(result.checked["oneOf"], 1)
        self.assertEqual(result.checked.get("maxLength", 0), 0)  # failing branch dropped
        self.assertEqual(result.checked["type"], 1)


class RefTests(unittest.TestCase):
    def test_local_ref_resolves_against_root(self):
        root = {
            "components": {"schemas": {"Pet": {"type": "object", "required": ["name"], "properties": {"name": {"type": "string"}}}}},
            "type": "object",
            "properties": {"pet": {"$ref": "#/components/schemas/Pet"}},
        }
        self.assertEqual(check({"pet": {"name": "rex"}}, root, root=root).violations, [])
        result = check({"pet": {}}, root, root=root)
        self.assertEqual(keywords(result), ["required"])
        self.assertEqual(result.violations[0]["path"], "/pet/name")

    def test_ref_pointer_escaping_and_percent_encoding(self):
        root = {"$defs": {"a/b~c": {"type": "integer"}}, "properties": {"x": {"$ref": "#/$defs/a~1b~0c"}}}
        self.assertEqual(check({"x": 1}, root, root=root).violations, [])
        self.assertEqual(keywords(check({"x": "s"}, root, root=root)), ["type"])
        root2 = {"$defs": {"sp ace": {"type": "string"}}}
        self.assertEqual(check("a", {"$ref": "#/$defs/sp%20ace"}, root=root2).violations, [])

    def test_ref_to_root(self):
        root = {"type": "object", "properties": {"child": {"$ref": "#"}}}
        result = check({"child": {"child": 1}}, root, root=root)
        self.assertEqual(keywords(result), ["type"])
        self.assertEqual(result.violations[0]["path"], "/child/child")

    def test_missing_or_external_ref_is_skipped(self):
        result = check(1, {"$ref": "#/components/schemas/Nope"}, root={})
        self.assertEqual(result.violations, [])
        self.assertEqual(result.skipped["$ref"], 1)
        result = check(1, {"$ref": "https://example.com/schema.json"}, root={})
        self.assertEqual(result.skipped["$ref"], 1)

    def test_cyclic_ref_is_skipped_not_recursed(self):
        root = {"$defs": {"loop": {"allOf": [{"$ref": "#/$defs/loop"}]}}}
        result = check({"a": 1}, {"$ref": "#/$defs/loop"}, root=root)
        self.assertEqual(result.violations, [])
        self.assertEqual(result.skipped["$ref:cycle"], 1)


class MiscKeywordTests(unittest.TestCase):
    def test_unknown_keyword_is_ignored_and_counted_as_skipped(self):
        result = check({"a": 1}, {"type": "object", "if": {"required": ["a"]}, "multipleOf": 2})
        self.assertEqual(result.violations, [])
        self.assertEqual(result.skipped["if"], 1)
        self.assertEqual(result.skipped["multipleOf"], 1)

    def test_annotations_and_extensions_are_not_skipped(self):
        result = check(
            "x",
            {"type": "string", "title": "T", "description": "D", "example": "e", "x-owner": "me"},
        )
        self.assertEqual(result.skipped, grade.Counter())

    def test_components_container_is_not_skipped(self):
        root = {"components": {"schemas": {"S": {"type": "string"}}}, "$ref": "#/components/schemas/S"}
        result = check("a", root, root=root)
        self.assertEqual(result.violations, [])
        self.assertEqual(result.skipped.get("components", 0), 0)

    def test_nullable_openapi_30(self):
        schema = {"type": "string", "nullable": True, "minLength": 3}
        self.assertEqual(check(None, schema).violations, [])
        self.assertEqual(keywords(check(None, {"type": "string"})), ["type"])

    def test_false_schema(self):
        self.assertEqual(keywords(check(1, False)), ["false"])
        self.assertEqual(check(1, True).violations, [])


class GradeSampleTests(unittest.TestCase):
    def test_no_body_is_a_violation(self):
        grade_row = grade.grade_sample({"schema": {"type": "object"}, "body": None, "error": "timeout"}, "m", "k")
        self.assertTrue(grade_row["graded"])
        self.assertFalse(grade_row["valid"])
        self.assertEqual(grade_row["violations"][0]["keyword"], "no_body")

    def test_no_schema_is_ungradable(self):
        grade_row = grade.grade_sample({"body": {}}, "m", "k")
        self.assertFalse(grade_row["graded"])
        self.assertIsNone(grade_row["valid"])
        self.assertEqual(grade_row["reason"], "no_schema")

    def test_valid_sample(self):
        grade_row = grade.grade_sample(
            {"api_type": "rest", "schema": {"type": "object", "required": ["id"]}, "body": {"id": "1"}},
            "m",
            "k",
        )
        self.assertTrue(grade_row["valid"])
        self.assertEqual(grade_row["checked_keywords"], {"type": 1, "required": 1})


class SummaryTests(unittest.TestCase):
    def test_rates(self):
        grades = [
            grade.grade_sample({"api_type": "rest", "schema": {"type": "object", "properties": {"n": {"type": "integer", "minimum": 0}}}, "body": {"n": 1}}, "m1", "a"),
            grade.grade_sample({"api_type": "rest", "schema": {"type": "object", "properties": {"n": {"type": "integer", "minimum": 0}}}, "body": {"n": -1}}, "m1", "b"),
            grade.grade_sample({"api_type": "graphql", "schema": {"type": "string", "format": "date"}, "body": "2026-10-29"}, "m1", "c"),
            grade.grade_sample({"schema": {"type": "string"}, "body": None}, "m2", "d"),
            grade.grade_sample({"body": 1}, "m2", "e"),
        ]
        summary = grade.summarize(grades)
        rest = summary["by_model_api_type"]["m1"]["rest"]
        self.assertEqual(rest["samples"], 2)
        self.assertEqual(rest["sample_conformance"], 0.5)
        self.assertEqual(rest["keywords"]["constraint"], {"checked": 2, "passed": 1, "pass_rate": 0.5})
        self.assertEqual(rest["keywords"]["format"], {"checked": 0, "passed": 0, "pass_rate": None})
        gql = summary["by_model_api_type"]["m1"]["graphql"]
        self.assertEqual(gql["keywords"]["format"], {"checked": 1, "passed": 1, "pass_rate": 1.0})
        m2 = summary["by_model"]["m2"]
        self.assertEqual(m2["samples"], 2)
        self.assertEqual(m2["ungradable"], 1)
        self.assertEqual(m2["sample_conformance"], 0.0)


class CliTests(unittest.TestCase):
    def test_end_to_end_derives_model_from_directory(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            model_dir = root / "vendor__model-x"
            model_dir.mkdir()
            samples = model_dir / "samples.jsonl"
            rows = [
                {"contract": "c", "target": "GET /a", "rep": 0, "api_type": "rest", "schema": {"type": "object", "required": ["id"]}, "body": {"id": 1}},
                {"contract": "c", "target": "GET /a", "rep": 1, "api_type": "rest", "schema": {"type": "object", "required": ["id"]}, "body": {}},
            ]
            samples.write_text("\n".join(json.dumps(r) for r in rows) + "\n", encoding="utf-8")
            out = root / "out" / "grades.jsonl"
            summary_path = root / "out" / "grade-summary.json"
            with contextlib.redirect_stderr(io.StringIO()):
                code = grade.main(["--samples", str(samples), "--out", str(out), "--summary", str(summary_path)])
            self.assertEqual(code, 0)
            grades = [json.loads(line) for line in out.read_text(encoding="utf-8").splitlines()]
            self.assertEqual([g["model"] for g in grades], ["vendor__model-x", "vendor__model-x"])
            self.assertEqual([g["valid"] for g in grades], [True, False])
            self.assertEqual(grades[1]["violations"][0]["keyword"], "required")
            self.assertNotEqual(grades[0]["key"], grades[1]["key"])
            summary = json.loads(summary_path.read_text(encoding="utf-8"))
            self.assertEqual(summary["by_model"]["vendor__model-x"]["sample_conformance"], 0.5)

    def test_sample_model_field_wins_and_duplicate_keys_are_disambiguated(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            first = root / "a" / "s.jsonl"
            second = root / "b" / "s.jsonl"
            first.parent.mkdir()
            second.parent.mkdir()
            row = {"model": "explicit", "contract": "c", "target": "t", "rep": 0, "schema": {}, "body": {}}
            first.write_text(json.dumps(row) + "\n", encoding="utf-8")
            second.write_text(json.dumps(row) + "\n", encoding="utf-8")
            out = root / "g.jsonl"
            with contextlib.redirect_stderr(io.StringIO()):
                grade.main(["--samples", str(first), "--samples", str(second), "--out", str(out), "--summary", str(root / "s.json")])
            grades = [json.loads(line) for line in out.read_text(encoding="utf-8").splitlines()]
            self.assertEqual({g["model"] for g in grades}, {"explicit"})
            self.assertEqual(len({g["key"] for g in grades}), 2)


if __name__ == "__main__":
    unittest.main()
