#!/usr/bin/env python3
"""A tiny fake OpenAI-compatible chat server, for testing the bench without spend.

    python3 scripts/llm-bench/fake_llm.py --port 18999 [--log calls.jsonl] [--delay 0.05]
        [--chunk-delay 0.05 --stream-chunks 200 --finish stop|length|error --tail-delay 0.2]

Answers `POST <anything>/chat/completions` (so it stands in for the proxy at
`http://127.0.0.1:P/r/<label>/api/v1`) with schema-shaped JSON built from the
prompt: a value for "Response/Message JSON Schema", NDJSON items for "Stream
item JSON Schema", and for smart-import extraction one operation per
`METHOD /path` line in the document excerpt. Supports `stream: true` (SSE).
The Authorization header is never read or logged. Test helper only; stdlib only.
"""

import argparse
import json
import re
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

LOCK = threading.Lock()
ARGS = None

FORMATS = {
    "date-time": "2026-10-29T12:00:00Z",
    "date": "2026-10-29",
    "time": "12:00:00",
    "email": "user@example.com",
    "uri": "https://example.com/x",
    "url": "https://example.com/x",
    "uuid": "6f1c2a4e-1b2c-4d3e-8f90-123456789abc",
    "ipv4": "192.0.2.1",
    "ipv6": "2001:db8::1",
    "hostname": "example.com",
}


def resolve(schema, root, depth):
    while isinstance(schema, dict) and "$ref" in schema and depth < 20:
        ref = schema["$ref"]
        if not ref.startswith("#/"):
            return {}
        node = root
        for part in ref[2:].split("/"):
            part = part.replace("~1", "/").replace("~0", "~")
            node = node.get(part, {}) if isinstance(node, dict) else {}
        schema = node
        depth += 1
    return schema if isinstance(schema, dict) else {}


def value_for(schema, root, depth=0, counter=None):
    """A plausible value for `schema` (not guaranteed valid; simple string patterns are honoured)."""
    counter = counter if counter is not None else [0]
    counter[0] += 1
    schema = resolve(schema, root, depth)
    if depth > 12:
        return None
    for key in ("const",):
        if key in schema:
            return schema[key]
    if schema.get("enum"):
        return schema["enum"][0]
    if "example" in schema:
        return schema["example"]
    if isinstance(schema.get("examples"), list) and schema["examples"]:
        return schema["examples"][0]
    if "default" in schema:
        return schema["default"]
    for key in ("oneOf", "anyOf"):
        if schema.get(key):
            return value_for(schema[key][0], root, depth + 1, counter)
    if schema.get("allOf"):
        merged = {"type": "object", "properties": {}, "required": []}
        for part in schema["allOf"]:
            part = resolve(part, root, depth)
            merged["properties"].update(part.get("properties", {}))
            merged["required"] += part.get("required", [])
        return value_for(merged, root, depth + 1, counter)
    typ = schema.get("type")
    if isinstance(typ, list):
        typ = next((t for t in typ if t != "null"), "null")
    if typ is None:
        typ = "object" if "properties" in schema else "string"
    if typ == "object":
        out = {}
        for name, sub in (schema.get("properties") or {}).items():
            out[name] = value_for(sub, root, depth + 1, counter)
        return out
    if typ == "array":
        n = max(schema.get("minItems", 0), min(2, schema.get("maxItems", 2)))
        return [value_for(schema.get("items", {}), root, depth + 1, counter) for _ in range(n)]
    if typ == "integer":
        low = schema.get("minimum", schema.get("exclusiveMinimum", 0) + 1 if "exclusiveMinimum" in schema else 1)
        return int(low) + counter[0] % 3
    if typ == "number":
        return float(schema.get("minimum", 1)) + 0.5
    if typ == "boolean":
        return False
    if typ == "null":
        return None
    fmt = schema.get("format")
    if fmt in FORMATS:
        return FORMATS[fmt]
    s = f"val{counter[0]}"
    min_len = schema.get("minLength", 0)
    max_len = schema.get("maxLength")
    if len(s) < min_len:
        s = s + "x" * (min_len - len(s))
    if max_len is not None:
        s = s[:max_len]
    pattern = schema.get("pattern")
    if isinstance(pattern, str):
        return pattern_value(pattern, counter[0], min_len, max_len, s)
    return s


# Values for a string schema's `pattern`. Only a small regex subset is generated:
# literals, escapes (\d \w \s and their negations, escaped punctuation), `.`,
# character classes (ranges, negation), groups ((...), (?:...)), alternation,
# anchors and the quantifiers ? * + {n} {n,} {n,m}. Anything else (lookaround,
# backreferences, \b, ...) is unsupported, and every generated value is checked
# with `re` before it is used, so a pattern this cannot satisfy keeps the plain value.

DIGITS = "0123456789"
LOWER = "abcdefghijklmnopqrstuvwxyz"
UPPER = LOWER.upper()
WORD = LOWER + UPPER + DIGITS + "_"
SPACE = " \t\n\r\f\v"
POOL = LOWER + DIGITS + UPPER  # what `.` and negated sets draw from
ESCAPE_SETS = {"d": DIGITS, "w": WORD, "s": SPACE}
ESCAPE_LITERALS = {"t": "\t", "n": "\n", "r": "\r", "f": "\f", "v": "\v"}


class Unsupported(Exception):
    pass


class PatternParser:
    """Parses the subset into nodes: ("lit", c) | ("set", chars) | ("seq", [n])
    | ("alt", [n]) | ("rep", n, lo, hi) | ("empty",)."""

    def __init__(self, src):
        self.src, self.i = src, 0

    def peek(self):
        return self.src[self.i] if self.i < len(self.src) else None

    def take(self):
        c = self.peek()
        if c is None:
            raise Unsupported("unexpected end")
        self.i += 1
        return c

    def parse(self):
        node = self.alt()
        if self.peek() is not None:
            raise Unsupported(f"unexpected {self.peek()!r}")
        return node

    def alt(self):
        branches = [self.seq()]
        while self.peek() == "|":
            self.i += 1
            branches.append(self.seq())
        return branches[0] if len(branches) == 1 else ("alt", branches)

    def seq(self):
        items = []
        while self.peek() not in (None, "|", ")"):
            atom = self.atom()
            items.append(self.quantified(atom))
        return ("seq", items)

    def atom(self):
        c = self.take()
        if c in "^$":
            return ("empty",)
        if c == ".":
            return ("set", POOL)
        if c == "(":
            if self.src.startswith("?:", self.i):
                self.i += 2
            elif self.peek() == "?":
                raise Unsupported("lookaround or named group")
            node = self.alt()
            if self.take() != ")":
                raise Unsupported("unclosed group")
            return node
        if c == "[":
            return ("set", self.char_class())
        if c == "\\":
            return self.escape()
        if c in "*+?{)]|":
            raise Unsupported(f"stray {c!r}")
        return ("lit", c)

    def escape(self):
        c = self.take()
        if c in ESCAPE_SETS:
            return ("set", ESCAPE_SETS[c])
        if c.lower() in ESCAPE_SETS:
            excluded = set(ESCAPE_SETS[c.lower()])
            return ("set", "".join(ch for ch in POOL + "-._~" if ch not in excluded))
        if c in ESCAPE_LITERALS:
            return ("lit", ESCAPE_LITERALS[c])
        if c.isalnum():
            raise Unsupported(f"escape \\{c}")
        return ("lit", c)

    def char_class(self):
        negate = self.peek() == "^"
        if negate:
            self.i += 1
        members, first = [], True
        while True:
            c = self.take()
            if c == "]" and not first:
                break
            first = False
            if c == "\\":
                node = self.escape()
                if node[0] == "set":
                    members.extend(node[1])
                    continue
                c = node[1]
            if self.peek() == "-" and self.i + 1 < len(self.src) and self.src[self.i + 1] != "]":
                self.i += 1
                end = self.take()
                if end == "\\":
                    node = self.escape()
                    if node[0] == "set":
                        raise Unsupported("range to a set")
                    end = node[1]
                if ord(end) < ord(c):
                    raise Unsupported("reversed range")
                members.extend(chr(o) for o in range(ord(c), ord(end) + 1))
            else:
                members.append(c)
        if negate:
            excluded = set(members)
            members = [ch for ch in POOL + "-._~" if ch not in excluded]
        chars = "".join(dict.fromkeys(members))
        if not chars:
            raise Unsupported("empty class")
        return chars

    def quantified(self, atom):
        c = self.peek()
        if c == "?":
            lo, hi = 0, 1
        elif c == "*":
            lo, hi = 0, None
        elif c == "+":
            lo, hi = 1, None
        elif c == "{":
            m = re.match(r"\{(\d+)(,(\d*))?\}", self.src[self.i:])
            if not m:
                raise Unsupported("bad brace quantifier")
            self.i += m.end() - 1
            lo = int(m.group(1))
            hi = lo if m.group(2) is None else (int(m.group(3)) if m.group(3) else None)
        else:
            return atom
        self.i += 1
        if self.peek() == "?":  # lazy: same language
            self.i += 1
        if atom[0] == "empty":
            return atom
        return ("rep", atom, lo, hi)


def generate(node, seed, spread, out):
    """Appends one string of `node` to `out`: the first branch of an alternation,
    each repeat `spread` times clamped to its bounds, and a set's character picked
    by `seed` and the position, so ids of different fields differ."""
    kind = node[0]
    if kind == "lit":
        out.append(node[1])
    elif kind == "set":
        chars = node[1]
        out.append(chars[(seed + len(out)) % len(chars)])
    elif kind == "seq":
        for item in node[1]:
            generate(item, seed, spread, out)
    elif kind == "alt":
        generate(node[1][0], seed, spread, out)
    elif kind == "rep":
        _, inner, lo, hi = node
        count = max(lo, spread if hi is None else min(hi, spread))
        for _ in range(count):
            generate(inner, seed, spread, out)


def pattern_value(pattern, seed, min_len, max_len, plain):
    """A value matching `pattern` (JSON Schema: unanchored search) and the length
    bounds, deterministic in `seed`; `plain` when it already matches or when no
    generated value does."""
    try:
        compiled = re.compile(pattern)
    except (re.error, RecursionError):
        return plain

    def fits(s):
        return (compiled.search(s) is not None and len(s) >= min_len
                and (max_len is None or len(s) <= max_len))

    if fits(plain):
        return plain
    try:
        tree = PatternParser(pattern).parse()
    except (Unsupported, RecursionError):
        return plain
    limit = max(64, min_len + 1)
    for spread in [3, *range(1, limit + 1)]:
        out = []
        try:
            generate(tree, seed, spread, out)
        except RecursionError:
            return plain
        s = "".join(out)
        if fits(s):
            return s
    return plain


def schema_after(text, marker):
    at = text.find(marker)
    if at < 0:
        return None
    try:
        value, _ = json.JSONDecoder().raw_decode(text[at + len(marker):].lstrip())
        return value
    except ValueError:
        return None


def answer(body):
    messages = body.get("messages") or []
    system = "\n".join(m.get("content", "") for m in messages if m.get("role") == "system")
    user = "\n".join(m.get("content", "") for m in messages if m.get("role") == "user")
    if "You extract HTTP API operations" in system:
        ops, seen = [], set()
        for method, path in re.findall(r"\b(GET|POST|PUT|PATCH|DELETE)\b(?:\s+request\s+to)?\s+`?(?:https?://[^/\s]+)?(/[^\s`<\"'.,]*)", user):
            if (method, path) in seen:
                continue
            seen.add((method, path))
            ops.append({
                "method": method,
                "path": path,
                "summary": f"{method} {path}",
                "responses": [{"status": 200, "schema": {"type": "object", "properties": {"id": {"type": "integer"}}}}],
                "confidence": 0.9,
                "evidence": f"{method} {path}",
            })
        return "import", json.dumps({"operations": ops})
    if "You write documentation for HTTP API operations" in system:
        ops = []
        for line in user.splitlines():
            parts = line.split(";")[0].split(" ", 1)
            if len(parts) == 2:
                ops.append({"method": parts[0], "path": parts[1], "summary": line[:60], "description": "Fake."})
        return "summaries", json.dumps({"operations": ops})
    stream_schema = schema_after(system, "Stream item JSON Schema:")
    if stream_schema is not None:
        lines = []
        for i in range(5):
            lines.append(json.dumps(value_for(stream_schema, stream_schema, counter=[i])))
        return "stream_items", "\n".join(lines)
    for marker in ("Response JSON Schema:", "Message JSON Schema:"):
        schema = schema_after(system, marker)
        if schema is not None:
            return "response", json.dumps(value_for(schema, schema))
    return "unknown", "{}"


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *a):
        pass

    def do_POST(self):
        length = int(self.headers.get("content-length") or 0)
        raw = self.rfile.read(length)
        if not self.path.endswith("/chat/completions"):
            self.send_error(404)
            return
        try:
            body = json.loads(raw)
        except ValueError:
            self.send_error(400)
            return
        kind, content = answer(body)
        if ARGS.delay:
            time.sleep(ARGS.delay)
        if ARGS.log:
            with LOCK, open(ARGS.log, "a") as f:
                f.write(json.dumps({"path": self.path, "model": body.get("model"), "stream": bool(body.get("stream")),
                                    "kind": kind, "response_format": body.get("response_format"),
                                    "max_tokens": body.get("max_tokens"),
                                    "temperature": body.get("temperature")}) + "\n")
        usage = {"prompt_tokens": len(raw) // 4, "completion_tokens": len(content) // 4,
                 "total_tokens": len(raw) // 4 + len(content) // 4}
        if body.get("stream"):
            self.send_response(200)
            self.send_header("content-type", "text/event-stream")
            self.send_header("connection", "close")
            self.end_headers()
            chunks = [content[i:i + 40] for i in range(0, len(content), 40)] or [""]
            # --stream-chunks pads a short answer with whitespace pieces, so a slow
            # stream (--chunk-delay) can run long, like a looping model.
            chunks += [" "] * max(0, (ARGS.stream_chunks or 0) - len(chunks))
            sent, gone = 0, False
            try:
                for piece in chunks:
                    event = {"id": "fake", "object": "chat.completion.chunk", "model": body.get("model"),
                             "choices": [{"index": 0, "delta": {"content": piece}, "finish_reason": None}]}
                    self.wfile.write(f"data: {json.dumps(event)}\n\n".encode())
                    self.wfile.flush()
                    sent += 1
                    if ARGS.chunk_delay:
                        time.sleep(ARGS.chunk_delay)
                base = {"id": "fake", "object": "chat.completion.chunk", "model": body.get("model")}
                if ARGS.finish == "stop":
                    final = dict(base, choices=[{"index": 0, "delta": {}, "finish_reason": "stop"}], usage=usage)
                    self.wfile.write(f"data: {json.dumps(final)}\n\ndata: [DONE]\n\n".encode())
                    self.wfile.flush()
                else:
                    # The answer ends (cut at max_tokens, or an in-band error); the usage
                    # comes in its own chunk after --tail-delay, as OpenRouter sends it.
                    if ARGS.finish == "length":
                        end = dict(base, choices=[{"index": 0, "delta": {}, "finish_reason": "length"}])
                    else:
                        end = dict(base, error={"code": 502, "message": "fake provider error"},
                                   choices=[{"index": 0, "delta": {}, "finish_reason": "error"}])
                    self.wfile.write(f"data: {json.dumps(end)}\n\n".encode())
                    self.wfile.flush()
                    if ARGS.tail_delay:
                        time.sleep(ARGS.tail_delay)
                    tail = dict(base, choices=[], usage=usage)
                    for line in (f"data: {json.dumps(tail)}\n\n", ": tail\n\n", "data: [DONE]\n\n"):
                        self.wfile.write(line.encode())
                        self.wfile.flush()
                        if ARGS.tail_delay:
                            time.sleep(ARGS.tail_delay)
            except (BrokenPipeError, ConnectionResetError, ConnectionAbortedError):
                gone = True  # the caller closed the connection mid-stream
            if ARGS.log:
                with LOCK, open(ARGS.log, "a") as f:
                    f.write(json.dumps({"stream_end": True, "chunks": len(chunks), "chunks_sent": sent,
                                        "caller_closed": gone}) + "\n")
            self.close_connection = True
            return
        out = json.dumps({
            "id": "fake", "object": "chat.completion", "model": body.get("model"),
            "choices": [{"index": 0, "message": {"role": "assistant", "content": content}, "finish_reason": "stop"}],
            "usage": usage,
        }).encode()
        self.send_response(200)
        self.send_header("content-type", "application/json")
        self.send_header("content-length", str(len(out)))
        self.end_headers()
        self.wfile.write(out)


def main():
    global ARGS
    p = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    p.add_argument("--port", type=int, required=True)
    p.add_argument("--log", help="append one JSON line per call (path, model, kind; never headers)")
    p.add_argument("--delay", type=float, default=0.0, help="seconds to sleep before answering")
    p.add_argument("--chunk-delay", type=float, default=0.0, help="seconds to sleep after each streamed chunk")
    p.add_argument("--stream-chunks", type=int, default=0,
                   help="stream at least this many content chunks, padding with whitespace")
    p.add_argument("--finish", choices=("stop", "length", "error"), default="stop",
                   help="how a stream ends: stop (usage in the last chunk), length or error (an in-band "
                        "error), each followed by a separate usage chunk")
    p.add_argument("--tail-delay", type=float, default=0.0,
                   help="seconds to sleep before each line after a length/error end (usage, keep-alive, [DONE])")
    ARGS = p.parse_args()
    server = ThreadingHTTPServer(("127.0.0.1", ARGS.port), Handler)
    server.daemon_threads = True
    server.serve_forever()


if __name__ == "__main__":
    main()
