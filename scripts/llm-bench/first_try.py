#!/usr/bin/env python3
"""First-try experiment: which request settings make an LLM's first answer valid?

Replays round 2's exact first prompts per variant through proxy.py, the budget
hard stop, and writes the raw answers. The engine's own checks then grade them.
This script never reads the API key.

    python3 scripts/llm-bench/first_try.py --proxy-base URL --out DIR --variants control,low
"""

import argparse
import concurrent.futures
import gzip
import json
import os
import random
import re
import sys
import time
import urllib.request

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from pilot_world import draw_pattern, flatten, leaf_type  # noqa: E402

REPO = os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
ROUND2 = os.path.join(REPO, "docs/benchmarks/2026-10-30")
MODELS = ["deepseek/deepseek-v4.1-flash-1017", "openai/gpt-6-luna", "nvidia/nemotron-3.5-lightning"]
SCHEMA_MARK = "Response JSON Schema:"
MAX_HINTS = 30
RULES = (
    "Rules the answer is checked against: output every required member; add no wrapper around the answer; "
    "where a field has a `pattern`, its value must match it exactly, including its length and any prefix."
)


def read_gz(name):
    with gzip.open(os.path.join(ROUND2, name), "rt") as f:
        return [json.loads(line) for line in f]


def pattern_fields(schema, root, path="", depth=0, out=None):
    """(path, pattern) of each patterned string the schema can hold."""
    out = {} if out is None else out
    if depth > 7 or len(out) >= MAX_HINTS:
        return out
    flat, _ = flatten(schema, root)
    kind = leaf_type(flat)
    if kind == "object":
        for key, sub in (flat.get("properties") or {}).items():
            pattern_fields(sub, root, f"{path}.{key}" if path else key, depth + 1, out)
    elif kind == "array":
        pattern_fields(flat.get("items") or {}, root, f"{path}[]", depth + 1, out)
    elif kind == "string" and "pattern" in flat:
        out.setdefault(path, flat["pattern"])
    return out


def hints_for(schema, rng):
    lines = []
    for path, pattern in pattern_fields(schema, schema).items():
        drawn = draw_pattern(pattern, rng)
        if drawn is not None:
            lines.append(f"- {path or 'value'}: {json.dumps(drawn)}")
    text = RULES
    if lines:
        text += "\nValid examples of the patterned fields (write your own value of the same shape):\n" + "\n".join(lines)
    return text


def body_for(variant, row, rng):
    messages = [dict(m) for m in row["messages"]]
    schema = row["schema"]
    parts = set(variant.split("+"))
    if "hints" in parts:
        system = messages[0]["content"]
        at = system.find(SCHEMA_MARK)
        if at >= 0:
            messages[0]["content"] = f"{system[:at]}{hints_for(schema, rng)}\n\n{system[at:]}"
    body = {"model": row["model"], "messages": messages, "temperature": 0.7, "max_tokens": 16384,
            "response_format": {"type": "json_object"}, "reasoning": {"enabled": False}}
    if "schema" in parts:
        body["response_format"] = {"type": "json_schema",
                                   "json_schema": {"name": "answer", "schema": schema, "strict": "strict" in parts}}
    if "require" in parts:
        body["provider"] = {"require_parameters": True}
    for effort in ("minimal", "low", "medium"):
        if effort in parts:
            body["reasoning"] = {"effort": effort, "exclude": True}
    return body


def call(proxy, variant, row):
    rng = random.Random(f"{row['model']}|{row['target']}|{variant}")
    body = body_for(variant, row, rng)
    slug = row["model"].replace("/", "__")
    url = f"{proxy.rstrip('/')}/r/{slug}~{variant.replace('+', '-')}/api/v1/chat/completions"
    req = urllib.request.Request(url, json.dumps(body).encode(), {
        "Content-Type": "application/json", "Authorization": "Bearer bench-dummy"})
    start = time.time()
    text, usage, error = "", {}, None
    try:
        with urllib.request.urlopen(req, timeout=120) as r:
            reply = json.loads(r.read())
        text = reply["choices"][0]["message"].get("content") or ""
        usage = reply.get("usage") or {}
    except Exception as e:  # noqa: BLE001 - recorded; counts as a failed first try
        error = f"{type(e).__name__}: {e}"[:200]
    return {"variant": variant, "model": row["model"], "target": row["target"], "api_type": row["api_type"],
            "schema": row["schema"], "raw": text, "echoes": row["echoes"], "items": row["items"], "error": error,
            "ms": int((time.time() - start) * 1000), "pt": usage.get("prompt_tokens") or 0,
            "ct": usage.get("completion_tokens") or 0,
            "rt": (usage.get("completion_tokens_details") or {}).get("reasoning_tokens") or 0}


def rows():
    samples = {}
    for s in read_gz("samples.jsonl.gz"):
        if s["model"] in MODELS and isinstance(s["schema"], dict):
            samples.setdefault((s["model_slug"], s["target"]), s)
    seen, out = set(), []
    for e in read_gz("exchanges.jsonl.gz"):
        if not e["label"].endswith("~engine") or len(e["messages"]) != 2:
            continue
        slug = e["label"].split("~")[0]
        m = re.search(r"^(?:Operation|Channel|Message|Messages): (.+)$", e["messages"][0]["content"], re.M)
        target = m.group(1) if m else None
        sample = samples.get((slug, target))
        if sample is None or (slug, target) in seen:
            continue
        seen.add((slug, target))
        echoes = []
        request = sample.get("request") or {}
        for value in list((request.get("path") or {}).values()) + list((request.get("query") or {}).values()):
            if isinstance(value, str):
                echoes.append(value)
            elif isinstance(value, list):
                echoes += [x for x in value if isinstance(x, str)]
        out.append({"model": sample["model"], "target": target, "api_type": sample["api_type"],
                    "schema": sample["schema"], "messages": e["messages"], "echoes": echoes,
                    "items": sample["schema"].get("type") == "array" and '"items"' in e["messages"][0]["content"]})
    return out


def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument("--proxy-base", required=True)
    p.add_argument("--out", required=True)
    p.add_argument("--variants", required=True)
    p.add_argument("--concurrency", type=int, default=8)
    args = p.parse_args()
    os.makedirs(args.out, exist_ok=True)
    work = rows()
    print(len(work), "prompts", flush=True)
    for variant in args.variants.split(","):
        with concurrent.futures.ThreadPoolExecutor(args.concurrency) as pool:
            done = list(pool.map(lambda r: call(args.proxy_base, variant, r), work))
        with open(os.path.join(args.out, f"{variant.replace('+', '-')}.jsonl"), "w") as f:
            for d in done:
                f.write(json.dumps(d) + "\n")
        print(variant, "errors", sum(1 for d in done if d["error"]), flush=True)


if __name__ == "__main__":
    main()
