#!/usr/bin/env python3
"""A tiny fake OpenAI-compatible chat server, for testing the bench without spend.

    python3 scripts/llm-bench/fake_llm.py --port 18999 [--log calls.jsonl] [--delay 0.05]

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
    """A plausible value for `schema` (not guaranteed valid: patterns are ignored)."""
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
    return s


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
                                    "kind": kind, "response_format": body.get("response_format")}) + "\n")
        usage = {"prompt_tokens": len(raw) // 4, "completion_tokens": len(content) // 4,
                 "total_tokens": len(raw) // 4 + len(content) // 4}
        if body.get("stream"):
            self.send_response(200)
            self.send_header("content-type", "text/event-stream")
            self.send_header("connection", "close")
            self.end_headers()
            chunks = [content[i:i + 40] for i in range(0, len(content), 40)] or [""]
            for piece in chunks:
                event = {"id": "fake", "object": "chat.completion.chunk", "model": body.get("model"),
                         "choices": [{"index": 0, "delta": {"content": piece}, "finish_reason": None}]}
                self.wfile.write(f"data: {json.dumps(event)}\n\n".encode())
                self.wfile.flush()
            final = {"id": "fake", "object": "chat.completion.chunk", "model": body.get("model"),
                     "choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}], "usage": usage}
            self.wfile.write(f"data: {json.dumps(final)}\n\ndata: [DONE]\n\n".encode())
            self.wfile.flush()
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
    ARGS = p.parse_args()
    server = ThreadingHTTPServer(("127.0.0.1", ARGS.port), Handler)
    server.daemon_threads = True
    server.serve_forever()


if __name__ == "__main__":
    main()
