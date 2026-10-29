#!/usr/bin/env python3
"""Deterministic JSON Schema conformance grader for llm-bench samples.

Stdlib only. Reads one or more samples.jsonl files (one JSON object per line,
as written by the mb-evals runner) and checks each sample's `body` against its
`schema`. No model is involved; the result is reproducible.

Usage:
  grade.py --samples samples.jsonl [--samples more.jsonl ...] \
           --out grades.jsonl --summary grade-summary.json

Model name: taken from the sample's `model` field when present, otherwise from
the name of the directory that holds the samples file.

Supported keywords: type, required, enum, const, format, minimum, maximum,
exclusiveMinimum, exclusiveMaximum, minLength, maxLength, pattern, minItems,
maxItems, uniqueItems, additionalProperties, properties, patternProperties,
items, prefixItems, allOf, anyOf, oneOf, local $ref (JSON pointer, "#..."),
nullable (OpenAPI 3.0). Annotation keywords (title, description, example, ...)
and "x-" extensions are ignored silently. Every other keyword is ignored and
counted under "skipped" in the output.

Keyword categories used for keyword-level rates in the summary:
  format      : format
  enum        : enum, const
  constraint  : minimum, maximum, exclusiveMinimum, exclusiveMaximum,
                minLength, maxLength, minItems, maxItems, pattern, uniqueItems
"""

from __future__ import annotations

import argparse
import datetime as _dt
import ipaddress
import json
import re
import sys
from collections import Counter, defaultdict
from pathlib import Path
from typing import Any, Callable
from urllib.parse import unquote, urlparse

MISSING: Any = object()

ANNOTATION_KEYWORDS = frozenset(
    {
        "$schema",
        "$id",
        "$anchor",
        "$comment",
        "$defs",
        "definitions",
        "components",  # OpenAPI container that holds referenced schemas
        "title",
        "description",
        "summary",
        "default",
        "example",
        "examples",
        "deprecated",
        "readOnly",
        "writeOnly",
        "externalDocs",
        "xml",
        "contentMediaType",
        "contentEncoding",
    }
)

HANDLED_KEYWORDS = frozenset(
    {
        "$ref",
        "nullable",
        "type",
        "enum",
        "const",
        "format",
        "minimum",
        "maximum",
        "exclusiveMinimum",
        "exclusiveMaximum",
        "minLength",
        "maxLength",
        "pattern",
        "minItems",
        "maxItems",
        "uniqueItems",
        "required",
        "properties",
        "patternProperties",
        "additionalProperties",
        "items",
        "prefixItems",
        "allOf",
        "anyOf",
        "oneOf",
    }
)

KEYWORD_CATEGORIES: dict[str, frozenset[str]] = {
    "format": frozenset({"format"}),
    "enum": frozenset({"enum", "const"}),
    "constraint": frozenset(
        {
            "minimum",
            "maximum",
            "exclusiveMinimum",
            "exclusiveMaximum",
            "minLength",
            "maxLength",
            "minItems",
            "maxItems",
            "pattern",
            "uniqueItems",
        }
    ),
}


# --------------------------------------------------------------------------
# JSON value helpers
# --------------------------------------------------------------------------


def _is_int(value: Any) -> bool:
    return isinstance(value, int) and not isinstance(value, bool)


def _is_number(value: Any) -> bool:
    return isinstance(value, (int, float)) and not isinstance(value, bool)


def json_equal(a: Any, b: Any) -> bool:
    """JSON equality: booleans are never equal to numbers (Python's 1 == True is not JSON)."""
    if isinstance(a, bool) or isinstance(b, bool):
        return isinstance(a, bool) and isinstance(b, bool) and a == b
    if _is_number(a) and _is_number(b):
        return a == b
    if a is None or b is None:
        return a is None and b is None
    if isinstance(a, list) and isinstance(b, list):
        return len(a) == len(b) and all(json_equal(x, y) for x, y in zip(a, b))
    if isinstance(a, dict) and isinstance(b, dict):
        return set(a) == set(b) and all(json_equal(a[k], b[k]) for k in a)
    return type(a) is type(b) and a == b


_TYPE_CHECKS: dict[str, Callable[[Any], bool]] = {
    "null": lambda v: v is None,
    "boolean": lambda v: isinstance(v, bool),
    "integer": lambda v: _is_int(v) or (isinstance(v, float) and v.is_integer()),
    "number": _is_number,
    "string": lambda v: isinstance(v, str),
    "array": lambda v: isinstance(v, list),
    "object": lambda v: isinstance(v, dict),
}


def _child(path: str, token: str) -> str:
    """Append one reference token to a JSON pointer."""
    return path + "/" + token.replace("~", "~0").replace("/", "~1")


# --------------------------------------------------------------------------
# Format checks
# --------------------------------------------------------------------------

_DATE_RE = re.compile(r"(\d{4})-(\d{2})-(\d{2})")
_TIME_RE = re.compile(r"(\d{2}):(\d{2}):(\d{2})(\.\d+)?([Zz]|[+-](\d{2}):(\d{2}))")
_UUID_RE = re.compile(
    r"[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{12}"
)
_EMAIL_RE = re.compile(r"[^@\s]+@[^@\s]+\.[^@\s]+")
_SCHEME_RE = re.compile(r"[A-Za-z][A-Za-z0-9+.\-]*:")
_HOST_LABEL_RE = re.compile(r"[A-Za-z0-9](?:[A-Za-z0-9-]{0,61}[A-Za-z0-9])?")


def _is_date(value: str) -> bool:
    match = _DATE_RE.fullmatch(value)
    if not match:
        return False
    try:
        _dt.date(*(int(g) for g in match.groups()))
    except ValueError:
        return False
    return True


def _is_time(value: str) -> bool:
    """RFC 3339 full-time: hh:mm:ss[.frac] with a required offset."""
    match = _TIME_RE.fullmatch(value)
    if not match:
        return False
    hour, minute, second = int(match.group(1)), int(match.group(2)), int(match.group(3))
    if hour > 23 or minute > 59 or second > 60:
        return False
    if match.group(6) is not None:
        if int(match.group(6)) > 23 or int(match.group(7)) > 59:
            return False
    return True


def _is_date_time(value: str) -> bool:
    if len(value) < 20 or value[10] not in "Tt":
        return False
    return _is_date(value[:10]) and _is_time(value[11:])


def _is_email(value: str) -> bool:
    return _EMAIL_RE.fullmatch(value) is not None


def _is_uri(value: str) -> bool:
    if re.search(r"\s", value) or not _SCHEME_RE.match(value):
        return False
    scheme = value.split(":", 1)[0].lower()
    if scheme in ("http", "https"):
        return bool(urlparse(value).netloc)
    return True


def _is_uuid(value: str) -> bool:
    return _UUID_RE.fullmatch(value) is not None


def _is_ipv4(value: str) -> bool:
    try:
        ipaddress.IPv4Address(value)
    except ValueError:
        return False
    return True


def _is_ipv6(value: str) -> bool:
    try:
        ipaddress.IPv6Address(value)
    except ValueError:
        return False
    return True


def _is_hostname(value: str) -> bool:
    if not 0 < len(value) <= 253:
        return False
    return all(_HOST_LABEL_RE.fullmatch(label) for label in value.split("."))


_STRING_FORMATS: dict[str, Callable[[str], bool]] = {
    "date-time": _is_date_time,
    "date": _is_date,
    "time": _is_time,
    "email": _is_email,
    "uri": _is_uri,
    "url": _is_uri,
    "uuid": _is_uuid,
    "ipv4": _is_ipv4,
    "ipv6": _is_ipv6,
    "hostname": _is_hostname,
}

_INT_FORMATS: dict[str, tuple[int, int]] = {
    "int32": (-(2**31), 2**31 - 1),
    "int64": (-(2**63), 2**63 - 1),
}


# --------------------------------------------------------------------------
# Validation
# --------------------------------------------------------------------------


class Result:
    """Violations plus per-keyword counters for one validation pass."""

    def __init__(self) -> None:
        self.violations: list[dict[str, str]] = []
        self.checked: Counter[str] = Counter()
        self.failed: Counter[str] = Counter()
        self.skipped: Counter[str] = Counter()

    def report(self, keyword: str, problems: list[tuple[str, str]]) -> None:
        """Record one evaluation of `keyword`; `problems` is a list of (path, message)."""
        self.checked[keyword] += 1
        if problems:
            self.failed[keyword] += 1
            for path, message in problems:
                self.violations.append({"path": path, "keyword": keyword, "message": message})

    def absorb_counts(self, other: "Result") -> None:
        self.checked.update(other.checked)
        self.failed.update(other.failed)
        self.skipped.update(other.skipped)


def resolve_pointer(root: Any, ref: Any) -> Any:
    """Resolve a local JSON pointer reference ("#", "#/a/b"). Returns MISSING otherwise."""
    if not isinstance(ref, str) or not ref.startswith("#"):
        return MISSING
    fragment = unquote(ref[1:])
    if fragment == "":
        return root
    if not fragment.startswith("/"):
        return MISSING
    current = root
    for raw in fragment[1:].split("/"):
        token = raw.replace("~1", "/").replace("~0", "~")
        if isinstance(current, dict) and token in current:
            current = current[token]
        elif isinstance(current, list) and token.isdigit() and int(token) < len(current):
            current = current[int(token)]
        else:
            return MISSING
    return current


class Validator:
    """Evaluates a JSON instance against a schema, collecting violations and counters."""

    def __init__(self, root: Any) -> None:
        self.root = root
        self._active_refs: set[tuple[str, int]] = set()

    def validate(self, instance: Any, schema: Any | None = None) -> Result:
        result = Result()
        self._node(instance, self.root if schema is None else schema, "", result)
        return result

    # -- dispatch ----------------------------------------------------------

    def _node(self, inst: Any, schema: Any, path: str, res: Result) -> None:
        if schema is True:
            return
        if schema is False:
            res.report("false", [(path, "schema is false")])
            return
        if not isinstance(schema, dict):
            res.skipped["schema"] += 1
            return
        if inst is None and schema.get("nullable") is True:
            return

        if "$ref" in schema:
            self._ref(inst, schema["$ref"], path, res)
        if "type" in schema:
            self._type(inst, schema["type"], path, res)
        if "enum" in schema:
            if isinstance(schema["enum"], list):
                matched = any(json_equal(inst, v) for v in schema["enum"])
                res.report("enum", [] if matched else [(path, "value not in enum")])
            else:
                res.skipped["enum"] += 1
        if "const" in schema:
            same = json_equal(inst, schema["const"])
            res.report("const", [] if same else [(path, "value differs from const")])
        if "format" in schema:
            self._format(inst, schema["format"], path, res)
        if _is_number(inst):
            self._numeric(inst, schema, path, res)
        if isinstance(inst, str):
            self._string(inst, schema, path, res)
        if isinstance(inst, list):
            self._array(inst, schema, path, res)
        if isinstance(inst, dict):
            self._object(inst, schema, path, res)
        self._combinators(inst, schema, path, res)

        for keyword in schema:
            if keyword in HANDLED_KEYWORDS or keyword in ANNOTATION_KEYWORDS:
                continue
            if keyword.startswith("x-") or keyword == "nullable":
                continue
            res.skipped[keyword] += 1

    # -- references and combinators ---------------------------------------

    def _ref(self, inst: Any, ref: Any, path: str, res: Result) -> None:
        target = resolve_pointer(self.root, ref)
        if target is MISSING:
            res.skipped["$ref"] += 1
            return
        key = (ref, id(inst))
        if key in self._active_refs:
            res.skipped["$ref:cycle"] += 1
            return
        self._active_refs.add(key)
        try:
            self._node(inst, target, path, res)
        finally:
            self._active_refs.discard(key)

    def _combinators(self, inst: Any, schema: dict, path: str, res: Result) -> None:
        for keyword in ("allOf", "anyOf", "oneOf"):
            if keyword not in schema:
                continue
            subs = schema[keyword]
            if not isinstance(subs, list) or not subs:
                res.skipped[keyword] += 1
                continue
            if keyword == "allOf":
                # Branch violations flow up with their own keywords.
                res.report("allOf", [])
                for sub in subs:
                    self._node(inst, sub, path, res)
                continue

            branches: list[Result] = []
            for sub in subs:
                branch = Result()
                self._node(inst, sub, path, branch)
                branches.append(branch)
            passing = [b for b in branches if not b.violations]
            if keyword == "anyOf":
                ok = len(passing) >= 1
            else:
                ok = len(passing) == 1
            if ok:
                res.absorb_counts(passing[0])
                res.report(keyword, [])
            else:
                res.report(
                    keyword,
                    [(path, f"matched {len(passing)} of {len(subs)} branches")],
                )

    # -- keyword groups ----------------------------------------------------

    def _type(self, inst: Any, declared: Any, path: str, res: Result) -> None:
        names = declared if isinstance(declared, list) else [declared]
        if not all(isinstance(n, str) and n in _TYPE_CHECKS for n in names):
            res.skipped["type"] += 1
            return
        if any(_TYPE_CHECKS[n](inst) for n in names):
            res.report("type", [])
        else:
            res.report("type", [(path, "expected " + "|".join(names))])

    def _format(self, inst: Any, fmt: Any, path: str, res: Result) -> None:
        if not isinstance(fmt, str):
            res.skipped["format"] += 1
            return
        if fmt in _STRING_FORMATS:
            if not isinstance(inst, str):
                return
            ok = _STRING_FORMATS[fmt](inst)
        elif fmt in _INT_FORMATS:
            if not _is_int(inst):
                return
            low, high = _INT_FORMATS[fmt]
            ok = low <= inst <= high
        else:
            res.skipped[f"format:{fmt}"] += 1
            return
        res.report("format", [] if ok else [(path, f"not a valid {fmt}")])

    def _numeric(self, inst: float, schema: dict, path: str, res: Result) -> None:
        minimum = schema.get("minimum")
        if _is_number(minimum):
            strict = schema.get("exclusiveMinimum") is True  # OpenAPI 3.0 form
            bad = inst <= minimum if strict else inst < minimum
            res.report("minimum", [(path, f"below minimum {minimum}")] if bad else [])
        maximum = schema.get("maximum")
        if _is_number(maximum):
            strict = schema.get("exclusiveMaximum") is True
            bad = inst >= maximum if strict else inst > maximum
            res.report("maximum", [(path, f"above maximum {maximum}")] if bad else [])
        exclusive_min = schema.get("exclusiveMinimum")
        if _is_number(exclusive_min):  # JSON Schema 2020-12 form
            bad = inst <= exclusive_min
            res.report("exclusiveMinimum", [(path, f"not above {exclusive_min}")] if bad else [])
        exclusive_max = schema.get("exclusiveMaximum")
        if _is_number(exclusive_max):
            bad = inst >= exclusive_max
            res.report("exclusiveMaximum", [(path, f"not below {exclusive_max}")] if bad else [])

    def _string(self, inst: str, schema: dict, path: str, res: Result) -> None:
        min_length = schema.get("minLength")
        if _is_number(min_length):
            bad = len(inst) < min_length
            res.report("minLength", [(path, f"shorter than {min_length}")] if bad else [])
        max_length = schema.get("maxLength")
        if _is_number(max_length):
            bad = len(inst) > max_length
            res.report("maxLength", [(path, f"longer than {max_length}")] if bad else [])
        if "pattern" in schema:
            pattern = schema["pattern"]
            try:
                compiled = re.compile(pattern) if isinstance(pattern, str) else None
            except re.error:
                compiled = None
            if compiled is None:
                res.skipped["pattern"] += 1
            else:
                matched = compiled.search(inst) is not None
                res.report("pattern", [] if matched else [(path, "does not match pattern")])

    def _array(self, inst: list, schema: dict, path: str, res: Result) -> None:
        min_items = schema.get("minItems")
        if _is_number(min_items):
            bad = len(inst) < min_items
            res.report("minItems", [(path, f"fewer than {min_items} items")] if bad else [])
        max_items = schema.get("maxItems")
        if _is_number(max_items):
            bad = len(inst) > max_items
            res.report("maxItems", [(path, f"more than {max_items} items")] if bad else [])
        if schema.get("uniqueItems") is True:
            duplicate = any(
                json_equal(inst[i], inst[j])
                for i in range(len(inst))
                for j in range(i + 1, len(inst))
            )
            res.report("uniqueItems", [(path, "items are not unique")] if duplicate else [])

        prefix = schema.get("prefixItems")
        if isinstance(prefix, list):
            head = min(len(prefix), len(inst))
            for i in range(head):
                self._node(inst[i], prefix[i], _child(path, str(i)), res)
            tail = schema.get("items")
            if isinstance(tail, (dict, bool)):
                for i in range(head, len(inst)):
                    self._node(inst[i], tail, _child(path, str(i)), res)
            return

        items = schema.get("items")
        if isinstance(items, list):  # draft-7 tuple form
            for i in range(min(len(items), len(inst))):
                self._node(inst[i], items[i], _child(path, str(i)), res)
        elif isinstance(items, (dict, bool)):
            for i in range(len(inst)):
                self._node(inst[i], items, _child(path, str(i)), res)

    def _object(self, inst: dict, schema: dict, path: str, res: Result) -> None:
        required = schema.get("required")
        if isinstance(required, list):
            missing = [
                (_child(path, name), "missing required property")
                for name in required
                if isinstance(name, str) and name not in inst
            ]
            res.report("required", missing)

        properties = schema.get("properties")
        if not isinstance(properties, dict):
            properties = {}
        for name, sub in properties.items():
            if name in inst:
                self._node(inst[name], sub, _child(path, name), res)

        matched: set[str] = set()
        pattern_properties = schema.get("patternProperties")
        if isinstance(pattern_properties, dict):
            for pattern, sub in pattern_properties.items():
                try:
                    compiled = re.compile(pattern)
                except (re.error, TypeError):
                    res.skipped["patternProperties"] += 1
                    continue
                for key in inst:
                    if compiled.search(key):
                        matched.add(key)
                        self._node(inst[key], sub, _child(path, key), res)

        if "additionalProperties" in schema:
            policy = schema["additionalProperties"]
            extra = [k for k in inst if k not in properties and k not in matched]
            if policy is False:
                res.report(
                    "additionalProperties",
                    [(_child(path, k), "property not allowed") for k in extra],
                )
            elif isinstance(policy, dict):
                for key in extra:
                    self._node(inst[key], policy, _child(path, key), res)


# --------------------------------------------------------------------------
# Per-sample grading
# --------------------------------------------------------------------------


def _request_values(request: Any) -> set[str]:
    """Every string in the request's `path` and `query` parts."""
    out: set[str] = set()

    def walk(v: Any) -> None:
        if isinstance(v, str):
            out.add(v)
        elif isinstance(v, dict):
            for x in v.values():
                walk(x)
        elif isinstance(v, list):
            for x in v:
                walk(x)

    if isinstance(request, dict):
        walk(request.get("path"))
        walk(request.get("query"))
    return out


def _at_pointer(doc: Any, pointer: str) -> Any:
    cur = doc
    for raw in pointer.split("/")[1:] if pointer else []:
        token = raw.replace("~1", "/").replace("~0", "~")
        if isinstance(cur, dict) and token in cur:
            cur = cur[token]
        elif isinstance(cur, list) and token.isdigit() and int(token) < len(cur):
            cur = cur[int(token)]
        else:
            return None
    return cur


def split_echoes(violations: list[dict], body: Any, request: Any) -> tuple[list[dict], list[dict]]:
    """(echoed, rest): a `format` violation whose value is one of the
    request's path or query values is an echo, not an invalid answer. The
    response repeats the id the client asked for (`GET /orders/{id}`), which
    is what a real API does, even when the contract gave the parameter no
    format."""
    values = _request_values(request)
    echoed, rest = [], []
    for v in violations:
        value = _at_pointer(body, v.get("path", ""))
        if v.get("keyword") == "format" and isinstance(value, str) and value in values:
            echoed.append(v)
        else:
            rest.append(v)
    return echoed, rest


def grade_sample(sample: dict, model: str, key: str) -> dict:
    """Grade one sample. Never includes the body or schema in the output."""
    api_type = sample.get("api_type") or "unknown"
    base = {
        "key": key,
        "model": model,
        "api_type": api_type,
        "domain": sample.get("domain"),
        "contract": sample.get("contract"),
        "target": sample.get("target"),
        "rep": sample.get("rep"),
        "source": sample.get("source"),
    }
    schema = sample.get("schema")
    if schema is None:
        return {
            **base,
            "graded": False,
            "reason": "no_schema",
            "valid": None,
            "violations": [],
            "echoed": [],
            "checked_keywords": {},
            "failed_keywords": {},
            "skipped": {},
        }
    body = sample.get("body")
    if body is None:
        # No output to check: the call produced nothing conforming.
        return {
            **base,
            "graded": True,
            "reason": "no_body",
            "valid": False,
            "violations": [{"path": "", "keyword": "no_body", "message": "no body returned"}],
            "echoed": [],
            "checked_keywords": {},
            "failed_keywords": {},
            "skipped": {},
        }
    result = Validator(schema).validate(body)
    echoed, violations = split_echoes(result.violations, body, sample.get("request"))
    return {
        **base,
        "graded": True,
        "reason": None,
        "valid": not violations,
        "violations": violations,
        "echoed": echoed,
        "checked_keywords": dict(result.checked),
        "failed_keywords": dict(result.failed),
        "skipped": dict(result.skipped),
    }


# --------------------------------------------------------------------------
# Aggregation
# --------------------------------------------------------------------------


class _Bucket:
    def __init__(self) -> None:
        self.samples = 0
        self.ungradable = 0
        self.graded = 0
        self.conforming = 0
        self.checked: Counter[str] = Counter()
        self.failed: Counter[str] = Counter()
        self.skipped: Counter[str] = Counter()

    def add(self, grade: dict) -> None:
        self.samples += 1
        if not grade["graded"]:
            self.ungradable += 1
            return
        self.graded += 1
        if grade["valid"]:
            self.conforming += 1
        self.checked.update(grade["checked_keywords"])
        self.failed.update(grade["failed_keywords"])
        self.skipped.update(grade["skipped"])

    def summary(self) -> dict:
        def rate(num: int, den: int) -> float | None:
            return round(num / den, 4) if den else None

        keywords = {}
        for category, names in KEYWORD_CATEGORIES.items():
            checked = sum(self.checked[n] for n in names)
            failed = sum(self.failed[n] for n in names)
            keywords[category] = {
                "checked": checked,
                "passed": checked - failed,
                "pass_rate": rate(checked - failed, checked),
            }
        return {
            "samples": self.samples,
            "graded": self.graded,
            "ungradable": self.ungradable,
            "conforming": self.conforming,
            "sample_conformance": rate(self.conforming, self.graded),
            "keywords": keywords,
            "skipped": dict(sorted(self.skipped.items())),
        }


def summarize(grades: list[dict]) -> dict:
    by_model_api: dict[str, dict[str, _Bucket]] = defaultdict(lambda: defaultdict(_Bucket))
    by_model: dict[str, _Bucket] = defaultdict(_Bucket)
    overall = _Bucket()
    for grade in grades:
        by_model_api[grade["model"]][grade["api_type"]].add(grade)
        by_model[grade["model"]].add(grade)
        overall.add(grade)
    return {
        "keyword_categories": {k: sorted(v) for k, v in KEYWORD_CATEGORIES.items()},
        "overall": overall.summary(),
        "by_model": {m: b.summary() for m, b in sorted(by_model.items())},
        "by_model_api_type": {
            m: {api: b.summary() for api, b in sorted(types.items())}
            for m, types in sorted(by_model_api.items())
        },
    }


# --------------------------------------------------------------------------
# CLI
# --------------------------------------------------------------------------


def load_samples(path: Path) -> list[tuple[int, dict]]:
    rows: list[tuple[int, dict]] = []
    with path.open(encoding="utf-8") as handle:
        for lineno, line in enumerate(handle, start=1):
            line = line.strip()
            if not line:
                continue
            try:
                sample = json.loads(line)
            except json.JSONDecodeError as exc:
                raise SystemExit(f"{path}:{lineno}: invalid JSON ({exc.msg})")
            if not isinstance(sample, dict):
                raise SystemExit(f"{path}:{lineno}: sample is not a JSON object")
            rows.append((lineno, sample))
    return rows


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description="Grade llm-bench samples against their schemas.")
    parser.add_argument(
        "--samples",
        action="append",
        required=True,
        type=Path,
        help="samples.jsonl file (repeatable)",
    )
    parser.add_argument("--out", required=True, type=Path, help="per-sample grades (JSONL)")
    parser.add_argument("--summary", required=True, type=Path, help="aggregate summary (JSON)")
    args = parser.parse_args(argv)

    grades: list[dict] = []
    seen: Counter[str] = Counter()
    for samples_path in args.samples:
        default_model = samples_path.resolve().parent.name
        for _lineno, sample in load_samples(samples_path):
            model = str(sample.get("model") or default_model)
            base_key = f"{model}::{sample.get('contract')}::{sample.get('target')}::{sample.get('rep')}"
            seen[base_key] += 1
            key = base_key if seen[base_key] == 1 else f"{base_key}::dup{seen[base_key]}"
            grades.append(grade_sample(sample, model, key))

    args.out.parent.mkdir(parents=True, exist_ok=True)
    with args.out.open("w", encoding="utf-8") as handle:
        for grade in grades:
            handle.write(json.dumps(grade, ensure_ascii=False) + "\n")

    args.summary.parent.mkdir(parents=True, exist_ok=True)
    with args.summary.open("w", encoding="utf-8") as handle:
        json.dump(summarize(grades), handle, indent=2, ensure_ascii=False)
        handle.write("\n")

    print(f"graded {len(grades)} samples -> {args.out}, {args.summary}", file=sys.stderr)
    return 0


if __name__ == "__main__":
    sys.exit(main())
