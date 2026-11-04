"""Finds flow-mapping entries that YAML reads as a key with a null value.

An unquoted comma ends a plain scalar inside `{...}`, so
`{type: string, description: Shown as typed, in its displayed case.}` keeps only
"Shown as typed" as the description and adds a key "in its displayed case." with
a null value. A bare key (`{a: 1, b}`) or an empty value (`{a: }`) inside a flow
mapping is reported; an explicit `null` or `~` is not.

The scan reads the text, so it needs no YAML library. Flow collections are only
looked for where a value starts (after `key:` or `- `), so braces inside plain or
block scalars are left alone.

Run: python3 scripts/llm-bench/flow_yaml_lint.py [files...]  (default: the bench's contracts)
"""

import glob
import os
import sys

REPO = os.path.abspath(os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", ".."))
# Every contract the bench suites, docs and examples load.
CONTRACT_GLOBS = [
    "scripts/llm-bench/contracts/**/*.yaml",
    "docs/examples/*.yaml",
    "docs/benchmarks/contracts/*.yaml",
    "examples/apis/**/*.yaml",
    "fixtures/openapi/*.yaml",
    "fixtures/asyncapi/*.yaml",
]

ENDS_INDICATOR = " \t\r\n,}]"


def contract_files():
    found = set()
    for pattern in CONTRACT_GLOBS:
        found.update(glob.glob(os.path.join(REPO, pattern), recursive=True))
    return sorted(found)


def skip_quoted(text, i):
    """Index just past the quoted scalar that starts at `i`."""
    quote, i = text[i], i + 1
    while i < len(text):
        c = text[i]
        if quote == '"' and c == "\\":
            i += 2
            continue
        if c == quote:
            if quote == "'" and text[i + 1:i + 2] == "'":
                i += 2
                continue
            return i + 1
        i += 1
    return i


def scan_flow(text, i, findings):
    """Scans the flow collection that starts at `i`; returns the index past it."""
    # One frame per open collection: [kind, entry_start, has_key, has_colon, has_value]
    stack = []
    after_quote = False
    in_plain = False  # inside a plain scalar, where a quote is just a character
    while i < len(text):
        c = text[i]
        frame = stack[-1] if stack else None
        if c == "#" and (i == 0 or text[i - 1] in " \t\n"):
            while i < len(text) and text[i] != "\n":
                i += 1
            continue
        if c in "\"'" and not in_plain:
            i = skip_quoted(text, i)
            mark(frame)
            after_quote = True
            continue
        was_quote, after_quote = after_quote, False
        if c in "{}[],":
            in_plain = False
        if c in "{[":
            mark(frame)
            stack.append([c, i + 1, False, False, False])
        elif c in "}]":
            finish(text, frame, i, findings)
            stack.pop()
            if not stack:
                return i + 1
        elif c == ",":
            finish(text, frame, i, findings)
            frame[1:] = [i + 1, False, False, False]
        elif c == ":" and frame and frame[0] == "{" and not frame[3] and (
                was_quote or i + 1 >= len(text) or text[i + 1] in ENDS_INDICATOR):
            frame[3] = True
            in_plain = False
        elif c not in " \t\r\n":
            mark(frame)
            in_plain = True
        i += 1
    return i


def mark(frame):
    if frame:
        frame[4 if frame[3] else 2] = True


def finish(text, frame, end, findings):
    kind, start, has_key, has_colon, has_value = frame
    if kind != "{" or not (has_key or has_colon):
        return
    entry = " ".join(text[start:end].split())
    if not has_colon:
        findings.append((text.count("\n", 0, start) + 1, f"bare key {entry!r} reads as a null value"))
    elif not has_value:
        findings.append((text.count("\n", 0, start) + 1, f"entry {entry!r} has an empty value"))


def value_start(line):
    """Column where the block value of `line` starts, or None when there is none."""
    i = len(line) - len(line.lstrip(" "))
    while line.startswith("- ", i) or line[i:].rstrip() == "-":
        i += 2
        while i < len(line) and line[i] == " ":
            i += 1
    if i < len(line) and line[i] in "{[":
        return i
    if i < len(line) and line[i] in "\"'":
        j = skip_quoted(line, i)
        if line[j:j + 1] == ":":
            i = j + 1
    else:
        j = i
        while j < len(line):
            if line[j] == ":" and (j + 1 == len(line) or line[j + 1] in " \t"):
                i = j + 1
                break
            if line[j] == "#" and j > 0 and line[j - 1] in " \t":
                break
            j += 1
    while i < len(line) and line[i] in " \t":
        i += 1
    return i if i < len(line) else None


def lint_text(text):
    findings = []
    lines = text.split("\n")
    offsets, pos = [], 0
    for line in lines:
        offsets.append(pos)
        pos += len(line) + 1
    block_indent = None
    resume = 0  # text index the last flow scan reached
    for n, line in enumerate(lines):
        if offsets[n] + len(line) < resume:
            continue
        indent = len(line) - len(line.lstrip(" "))
        if block_indent is not None:
            if not line.strip() or indent > block_indent:
                continue
            block_indent = None
        if not line.strip() or line.lstrip().startswith("#"):
            continue
        col = value_start(line)
        if col is None:
            continue
        if line[col] in "|>":
            block_indent = indent
        elif line[col] in "{[" and offsets[n] + col >= resume:
            resume = scan_flow(text, offsets[n] + col, findings)
    return findings


def lint_file(path):
    with open(path, encoding="utf-8") as f:
        return lint_text(f.read())


def main(paths):
    bad = 0
    for path in paths or contract_files():
        for line, message in lint_file(path):
            bad += 1
            print(f"{os.path.relpath(path, REPO)}:{line}: {message}")
    return 1 if bad else 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
