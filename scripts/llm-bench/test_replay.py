"""Tests for replay.py and the proxy's record and replay modes.

    python3 -m unittest discover -s scripts/llm-bench -p 'test_*.py'

The end-to-end test needs the release binary (target/release/mockingbird, or
MB_BENCH_BINARY); without it, it is skipped.
"""

import argparse
import contextlib
import gzip
import http.client
import io
import json
import os
import shutil
import subprocess
import sys
import tempfile
import threading
import unittest
from http.server import ThreadingHTTPServer
from unittest import mock

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)

import proxy  # noqa: E402
import replay  # noqa: E402

REPO = os.path.abspath(os.path.join(HERE, "..", ".."))


def lines(path):
    with open(path) as f:
        return [json.loads(line) for line in f if line.strip()]


def body(op="createIssue", http="POST /issues", now="2026-11-05T10:00:00Z", request='{"title":"a"}', schema=None):
    system = f"You generate data.\nAPI: Jira\nOperation: {op}\nHTTP: {http}\nResponse JSON Schema:\n{{}}"
    user = f"<now>{now}</now> (Unix 1793872800): timestamps ...\n<request>\n{request}\n</request>"
    b = {"model": "m", "messages": [{"role": "system", "content": system}, {"role": "user", "content": user}]}
    if schema:
        b["response_format"] = {"type": "json_schema", "json_schema": {"name": schema, "schema": {}}}
    return b


def recording(calls, results=None, label="m~journeys_x", unstable=()):
    return {"format": replay.FORMAT, "label": label, "model": "m", "suite": "journeys_x", "source": "test",
            "rep": 0, "recorded": "2026-11-05", "calls": calls, "results": results or {}, "unstable": list(unstable)}


def call(journey, op, n, content, hash_="h", now=None, status=200):
    return {"journey": journey, "rep": 0, "op": op, "n": n, "hash": hash_, "now": now, "status": status,
            "finish_reason": "stop", "content": content, "error": None}


class KeyTest(unittest.TestCase):
    def test_op_of_prefers_the_operation_line(self):
        self.assertEqual(replay.op_of(body()), "createIssue")

    def test_op_of_falls_back_to_http_then_schema(self):
        b = body()
        b["messages"][0]["content"] = "HTTP: GET /x\n"
        self.assertEqual(replay.op_of(b), "GET /x")
        b = body(schema="GetX")
        b["messages"][0]["content"] = "no lines"
        self.assertEqual(replay.op_of(b), "schema:GetX")
        b["response_format"] = None
        self.assertEqual(replay.op_of(b), "?")

    def test_hash_ignores_the_clock_only(self):
        a = replay.prompt_hash(body(now="2026-11-05T10:00:00Z"))
        self.assertEqual(a, replay.prompt_hash(body(now="2026-12-01T08:30:00Z")))
        self.assertNotEqual(a, replay.prompt_hash(body(request='{"title":"b"}')))

    def test_now_of(self):
        self.assertEqual(replay.now_of(body(now="2026-11-05T10:00:00Z")), 1793872800.0)
        self.assertIsNone(replay.now_of({"messages": [{"role": "user", "content": "no clock"}]}))


class ShiftTest(unittest.TestCase):
    anchor = 1793872800.0  # 2026-11-05T10:00:00Z

    def test_moves_iso_strings_and_time_fields(self):
        content = json.dumps({"id": 1793872000, "created": 1793872000, "created_at": "2026-11-05T09:00:00Z",
                              "when": "2026-11-05T09:00:00.123+02:00", "name": "2026-11-05T09:00:00Z!"})
        out = json.loads(replay.shift_times(content, 3600, self.anchor))
        self.assertEqual(out["id"], 1793872000)  # not a time field
        self.assertEqual(out["created"], 1793875600)
        self.assertEqual(out["created_at"], "2026-11-05T10:00:00Z")
        self.assertEqual(out["when"], "2026-11-05T10:00:00.123+02:00")
        self.assertEqual(out["name"], "2026-11-05T09:00:00Z!")  # not a whole timestamp

    def test_leaves_far_times_and_non_json(self):
        content = json.dumps({"born": "1970-01-01T00:00:00Z"})
        self.assertEqual(json.loads(replay.shift_times(content, 3600, self.anchor))["born"], "1970-01-01T00:00:00Z")
        self.assertEqual(replay.shift_times("not json 2026-11-05T09:00:00Z", 3600, self.anchor),
                         "not json 2026-11-05T09:00:00Z")
        self.assertEqual(replay.shift_times(content, 0, self.anchor), content)

    def test_json_lines(self):
        content = '{"ts": "2026-11-05T09:00:00Z"}\n{"ts": "2026-11-05T09:30:00Z"}'
        out = replay.shift_times(content, 60, self.anchor).split("\n")
        self.assertEqual(json.loads(out[1])["ts"], "2026-11-05T09:31:00Z")


class MatchTest(unittest.TestCase):
    def setUp(self):
        self.dir = tempfile.mkdtemp()
        self.addCleanup(shutil.rmtree, self.dir)
        h = replay.prompt_hash(body(request='{"exact":1}'))
        calls = [call("jira", "createIssue", 0, '{"key":"exact"}', hash_=h),
                 call("jira", "createIssue", 1, '{"key":"second"}'),
                 call("jira", "getIssue", 0, '{"key":"read0"}'),
                 call("jira", "getIssue", 3, '{"key":"read3"}'),
                 call("zendesk", "listTickets", 0, '{"tickets":[]}')]
        self.path = os.path.join(self.dir, "r.json.gz")
        replay.write_recording(self.path, recording(calls))
        self.log = os.path.join(self.dir, "log.jsonl")
        self.r = replay.Replayer([self.dir], self.log)

    def ask(self, b, journey="jira", label="m~journeys_x"):
        return self.r.answer(label, {replay.JOURNEY_HEADER: journey}, b)

    def test_exact_then_key_then_nearest_then_none(self):
        a = self.ask(body(request='{"exact":1}'))
        self.assertEqual((a["match"], a["content"]), ("exact", '{"key":"exact"}'))
        a = self.ask(body(request='{"drifted":1}'))  # createIssue #1
        self.assertEqual((a["match"], a["content"]), ("key", '{"key":"second"}'))
        self.ask(body(op="getIssue"))  # getIssue #0: key
        self.ask(body(op="getIssue"))  # #1: nearest is #0
        a = self.ask(body(op="getIssue"))  # #2: nearest is #3
        self.assertEqual((a["match"], a["content"]), ("nearest", '{"key":"read3"}'))
        a = self.ask(body(op="deleteIssue"))
        self.assertEqual(a["match"], "none")
        self.assertEqual(a["status"], 503)
        self.assertIn(replay.UNMATCHED, a["error"])
        matches = [line["match"] for line in lines(self.log)]
        self.assertEqual(matches, ["exact", "key", "key", "nearest", "nearest", "none"])

    def test_a_consumed_exact_call_is_reused_not_exact(self):
        self.ask(body(request='{"exact":1}'))  # createIssue #0: exact
        self.ask(body(op="createIssue", request='{"x":1}'))  # #1: key
        a = self.ask(body(request='{"exact":1}'))  # #2: the exact call again, already used
        self.assertEqual((a["match"], a["content"]), ("reused", '{"key":"exact"}'))
        log = lines(self.log)
        self.assertEqual([line["match"] for line in log], ["exact", "key", "reused"])
        self.assertEqual(log[0]["used"], ["jira", 0, "createIssue", 0])
        self.assertEqual(log[2]["used"], ["jira", 0, "createIssue", 0])

    def test_nearest_falls_back_to_another_journey(self):
        a = self.ask(body(op="listTickets"), journey="jira")
        self.assertEqual((a["match"], a["content"]), ("nearest", '{"tickets":[]}'))

    def test_unknown_label_is_unmatched(self):
        self.assertEqual(self.ask(body(), label="other~suite")["match"], "none")

    def test_marks_set_the_rep(self):
        self.r.mark("m~journeys_x", "jira", 1)
        self.assertEqual(self.r.tag("m~journeys_x", {replay.JOURNEY_HEADER: "jira"}, body())["rep"], 1)
        self.assertEqual(self.r.tag("m~journeys_x", {replay.JOURNEY_HEADER: "zendesk"}, body())["rep"], 0)

    def test_answers_move_to_the_replay_clock_per_journey(self):
        path = os.path.join(self.dir, "t.json.gz")
        replay.write_recording(path, recording([call("j", "createIssue", 0, '{"created_at":"2026-11-05T10:00:00Z"}',
                                                     now=1793872800.0),
                                                call("j", "createIssue", 1, '{"created_at":"2026-11-05T10:05:00Z"}',
                                                     now=1793873100.0)], label="t~s"))
        r = replay.Replayer([path])
        a = r.answer("t~s", {replay.JOURNEY_HEADER: "j"}, body(now="2026-11-06T10:00:00Z"))
        self.assertEqual(json.loads(a["content"])["created_at"], "2026-11-06T10:00:00Z")
        # The second call comes sooner than it did when recorded: the same shift, so
        # timestamps two answers share stay equal.
        a = r.answer("t~s", {replay.JOURNEY_HEADER: "j"}, body(now="2026-11-06T10:00:30Z"))
        self.assertEqual(json.loads(a["content"])["created_at"], "2026-11-06T10:05:00Z")


class CompareTest(unittest.TestCase):
    # Journey a: read, write; b: read; c: flip. One recorded call per (journey, op).
    RECORDED = [call("a", "read", 0, "{}"), call("a", "write", 0, "{}"), call("b", "read", 0, "{}"),
                call("c", "flip", 0, "{}")]

    def doc(self, calls=None, **kw):
        return recording(self.RECORDED if calls is None else calls,
                         results={"a.read": True, "a.write": True, "a.list": False, "b.read": True,
                                  "b.gone": True, "c.flip": True}, **kw)

    def rows(self, **ok):
        return [{"case": c.replace("_", "."), "ok": v, "rep": 0} for c, v in ok.items()]

    def a_read_fails(self):
        """Every recorded check as recorded, but a.read now fails."""
        return self.rows(a_read=False, a_write=True, a_list=False, b_read=True, b_gone=True, c_flip=True)

    @staticmethod
    def replayed(journey, op, n, match, used=True, label="m~journeys_x"):
        return {"label": label, "journey": journey, "rep": 0, "op": op, "n": n, "match": match,
                "used": [journey, 0, op, n] if used and match != "none" else None}

    def clean(self):
        return [self.replayed(c["journey"], c["op"], c["n"], "exact" if c["journey"] != "b" else "key")
                for c in self.RECORDED]

    def test_regressions_masked_improvements(self):
        calls = [self.replayed("a", "read", 0, "exact"), self.replayed("a", "write", 0, "key"),
                 self.replayed("b", "read", 0, "nearest"), self.replayed("c", "flip", 0, "none"),
                 self.replayed("a", "read", 0, "none", label="other")]
        r = replay.compare(self.doc(), self.rows(a_read=False, a_write=True, a_list=True, b_read=False,
                                                 c_flip=False), calls)
        self.assertEqual(r["regressions"], ["a.read"])
        self.assertEqual(sorted(r["masked"]), ["b.gone", "b.read", "c.flip"])
        self.assertEqual(r["improvements"], ["a.list"])
        self.assertEqual(r["missing"], ["b.gone"])
        self.assertEqual(r["calls"], {"exact": 1, "reused": 0, "key": 1, "nearest": 1, "none": 1})
        self.assertEqual(sorted(r["dirty"]), ["b", "c"])
        out = replay.render([r])
        self.assertIn("a.read", out)
        self.assertIn("c: flip #0 matched none", out)

    def test_missing_check_of_a_clean_journey_regresses(self):
        r = replay.compare(self.doc(), self.rows(a_write=True), self.clean())
        self.assertIn("a.read", r["regressions"])

    def test_unstable_checks_never_count(self):
        r = replay.compare(self.doc(unstable=["a.read"]), self.rows(a_read=False, a_write=True), self.clean())
        self.assertNotIn("a.read", r["regressions"])
        self.assertEqual(r["unstable"], 1)

    def test_an_extra_call_of_an_operation_makes_the_journey_dirty(self):
        # Every call matched exactly or by key, but a.read was asked twice, recorded once.
        calls = self.clean() + [self.replayed("a", "read", 1, "key", used=False)]
        r = replay.compare(self.doc(), self.a_read_fails(), calls)
        self.assertEqual(r["regressions"], [])
        self.assertIn("a.read", r["masked"])
        self.assertEqual(r["dirty"], {"a": ["read called 2 time(s), recorded 1"]})

    def test_a_missing_call_of_an_operation_makes_the_journey_dirty(self):
        calls = [c for c in self.clean() if (c["journey"], c["op"]) != ("a", "write")]
        r = replay.compare(self.doc(), self.a_read_fails(), calls)
        self.assertEqual(r["regressions"], [])
        self.assertIn("write called 0 time(s), recorded 1", r["dirty"]["a"])
        self.assertIn("recorded write #0 never used", r["dirty"]["a"])

    def test_an_unused_recorded_call_makes_the_journey_dirty(self):
        # Same counts per operation and only exact/key matches, but the replay's two
        # reads both answered from recorded read #1: read #0 went unused.
        recorded = [call("a", "read", 0, "{}"), call("a", "read", 1, "{}")]
        calls = [dict(self.replayed("a", "read", 0, "exact"), used=["a", 0, "read", 1]),
                 dict(self.replayed("a", "read", 1, "key"), used=["a", 0, "read", 1])]
        r = replay.compare(self.doc(recorded), self.a_read_fails(), calls)
        self.assertEqual(r["regressions"], [])
        self.assertEqual(r["dirty"], {"a": ["recorded read #0 never used"]})

    def test_a_reused_answer_makes_the_journey_dirty(self):
        calls = self.clean()
        calls[0] = self.replayed("a", "read", 0, "reused")
        r = replay.compare(self.doc(), self.a_read_fails(), calls)
        self.assertEqual(r["regressions"], [])
        self.assertIn("a.read", r["masked"])
        self.assertEqual(r["calls"]["reused"], 1)

    def test_a_clean_journey_has_no_dirt(self):
        r = replay.compare(self.doc(), self.rows(a_read=True, a_write=True, b_read=True, c_flip=True), self.clean())
        self.assertEqual(r["dirty"], {})

    def test_expected_change_checks_are_not_compared(self):
        d = tempfile.mkdtemp()
        self.addCleanup(shutil.rmtree, d)
        path = os.path.join(d, "r.json.gz")
        replay.write_recording(path, self.doc())
        with contextlib.redirect_stdout(io.StringIO()), contextlib.redirect_stderr(io.StringIO()):
            self.assertEqual(replay.cmd_expect(argparse.Namespace(recording=path, check=["a.read"])), 0)
            self.assertEqual(replay.cmd_expect(argparse.Namespace(recording=path, check=["a.nope"])), 2)
        doc = replay.read_recording(path)
        self.assertEqual(doc["expected_change"], ["a.read"])
        r = replay.compare(doc, self.a_read_fails(), self.clean())
        self.assertEqual((r["regressions"], r["expected"]), ([], ["a.read"]))
        self.assertIn("expected to change", replay.render([r]))

    def test_suite_error_is_reported(self):
        rows = [{"case": "suite_error", "ok": False, "rep": -1, "detail": {"error": "boom"}}]
        self.assertEqual(replay.compare(self.doc(), rows, [])["suite_error"], [{"error": "boom"}])


class CalibrateTest(unittest.TestCase):
    """Record time reports, and keeps, the journeys that are dirty on the recording's own binary."""

    def test_baseline_dirty_journeys_are_reported_and_kept(self):
        d = tempfile.mkdtemp()
        self.addCleanup(shutil.rmtree, d)
        path = os.path.join(d, "r.json.gz")
        recorded = [call("a", "read", 0, "{}"), call("b", "read", 0, "{}")]
        replay.write_recording(path, recording(recorded, results={"a.x": True, "b.x": True}))
        rows = [{"model": "m", "suite": "journeys_x", "case": c, "ok": True, "rep": 0} for c in ("a.x", "b.x")]
        runs = [[{"label": "m~journeys_x", "journey": "a", "rep": 0, "op": "read", "n": 0, "match": "exact",
                  "used": ["a", 0, "read", 0]},
                 {"label": "m~journeys_x", "journey": "b", "rep": 0, "op": "read", "n": 0, "match": "key",
                  "used": ["b", 0, "read", 0]},
                 {"label": "m~journeys_x", "journey": "b", "rep": 0, "op": "read", "n": 1, "match": "reused",
                  "used": ["b", 0, "read", 0]}]]
        with mock.patch.object(replay, "replay_once", side_effect=[(rows, c) for c in runs * 2]):
            dirty = replay.calibrate([path], argparse.Namespace(), 2)
        self.assertEqual(list(dirty), ["m~journeys_x"])
        self.assertEqual(sorted(dirty["m~journeys_x"]), ["b"])
        self.assertIn("read called 2 time(s), recorded 1", dirty["m~journeys_x"]["b"])
        doc = replay.read_recording(path)
        self.assertEqual(doc["baseline_dirty"], ["b"])
        self.assertEqual(doc["unstable"], [])
        text = replay.render_baseline_dirty(dirty)
        self.assertIn("m~journeys_x / b:", text)
        self.assertIn("matched reused", text)
        self.assertIn("replays clean", replay.render_baseline_dirty({}))


class RecordTest(unittest.TestCase):
    def setUp(self):
        self.dir = tempfile.mkdtemp()
        self.addCleanup(shutil.rmtree, self.dir)

    def test_recorder_writes_no_header_and_no_key(self):
        rec = replay.Recorder(os.path.join(self.dir, "raw.jsonl"))
        headers = {replay.JOURNEY_HEADER: "jira", "Authorization": "Bearer sk-or-v1-abc"}
        tag = rec.tag("m~s", headers, body())
        rec.write(tag, {"status": 200}, '{"token":"sk-or-v1-leaked"}', "stop")
        with open(rec.path) as f:
            text = f.read()
        self.assertNotIn("sk-or-", text)
        self.assertNotIn("Authorization", text)
        line = json.loads(text)
        self.assertEqual((line["journey"], line["op"], line["n"]), ("jira", "createIssue", 0))

    def test_pack_keeps_rep_zero_and_results(self):
        raw = os.path.join(self.dir, "raw.jsonl")
        with open(raw, "w") as f:
            for rep in (0, 1):
                f.write(json.dumps(dict(call("jira", "createIssue", 0, "{}"), label="a__b~journeys", rep=rep)) + "\n")
        server = os.path.join(self.dir, "server.jsonl")
        with open(server, "w") as f:
            for rep, ok in ((0, True), (1, False)):
                f.write(json.dumps({"model": "a/b", "suite": "journeys", "case": "jira.x", "rep": rep, "ok": ok}) + "\n")
            f.write(json.dumps({"model": "a/b", "suite": "journeys", "case": "suite_error", "rep": 0, "ok": False})
                    + "\n")
        [path] = replay.pack(raw, server, os.path.join(self.dir, "out"), source="test")
        doc = replay.read_recording(path)
        self.assertEqual((doc["model"], doc["suite"], len(doc["calls"])), ("a/b", "journeys", 1))
        self.assertEqual(doc["results"], {"jira.x": True})
        with open(path, "rb") as f:
            first = f.read()
        replay.write_recording(path, doc)
        with open(path, "rb") as f:
            self.assertEqual(f.read(), first)  # the same recording gives the same bytes


class SecurityScanTest(unittest.TestCase):
    """Committed recordings never hold an Authorization header or anything like a key."""

    def test_committed_recordings_are_clean(self):
        paths = replay.recording_paths([replay.RECORDINGS])
        self.assertTrue(paths, "no committed recordings")
        self.assertEqual(replay.scan(paths), [])
        for path in paths:
            with gzip.open(path, "rt", encoding="utf-8") as f:
                text = f.read()
            self.assertNotIn("sk-or-", text, path)
            self.assertIsNone(replay.AUTH_HEADER.search(text), path)
            doc = json.loads(text)
            for c in doc["calls"]:
                self.assertFalse(set(c) - {"journey", "rep", "op", "n", "hash", "now", "status", "finish_reason",
                                           "content", "error"}, path)

    def test_scan_finds_a_planted_header_and_key(self):
        d = tempfile.mkdtemp()
        self.addCleanup(shutil.rmtree, d)
        clean = os.path.join(d, "clean.json.gz")
        replay.write_recording(clean, recording([call("j", "op", 0, '{"payments":{"authorizations":[]}}')]))
        self.assertEqual(replay.scan([d]), [])
        with gzip.open(os.path.join(d, "header.json.gz"), "wt") as f:
            f.write(json.dumps({"content": json.dumps({"Authorization": "Bearer abc"})}))
        with gzip.open(os.path.join(d, "key.json.gz"), "wt") as f:
            f.write('{"content": "sk-or-v1-0123"}')
        with gzip.open(os.path.join(d, "headers.json.gz"), "wt") as f:
            f.write('{"calls": [{"headers": {"x": "y"}}]}')
        found = " ".join(replay.scan([d]))
        self.assertIn("header.json.gz: holds an Authorization header", found)
        self.assertIn("headers.json.gz: holds an Authorization header", found)
        self.assertIn("key.json.gz: holds an 'sk-or-' string", found)
        self.assertNotIn("clean", found)


class ProxyReplayTest(unittest.TestCase):
    """The proxy in replay mode answers from the recording, with no key and no upstream."""

    def setUp(self):
        self.dir = tempfile.mkdtemp()
        self.addCleanup(shutil.rmtree, self.dir)
        replay.write_recording(os.path.join(self.dir, "r.json.gz"),
                               recording([call("jira", "createIssue", 0, '{"key":"PLNT-1"}')], label="m~s"))
        args = argparse.Namespace(upstream=proxy.OPENROUTER_URL, no_stream_upgrade=False, dump=None,
                                  ledger=os.path.join(self.dir, "calls.jsonl"),
                                  budget_usd=0.0, prior_ledger=None, prices=None, provider_pins=None,
                                  no_format_fallback=False, record=None, replay=[self.dir],
                                  replay_log=os.path.join(self.dir, "log.jsonl"))
        with mock.patch.dict(os.environ, {"OPENROUTER_API_KEY": "sk-or-v1-not-used"}):
            px = proxy.Proxy(args)
        self.assertEqual(px.key, "")
        self.addCleanup(px.ledger.fh.close)
        proxy.Handler.proxy = px
        self.httpd = ThreadingHTTPServer(("127.0.0.1", 0), proxy.Handler)
        threading.Thread(target=self.httpd.serve_forever, daemon=True).start()
        self.addCleanup(self.httpd.shutdown)
        self.addCleanup(self.httpd.server_close)

    def post(self, path, doc, headers=None):
        conn = http.client.HTTPConnection("127.0.0.1", self.httpd.server_address[1], timeout=10)
        conn.request("POST", path, body=json.dumps(doc), headers=dict({"content-type": "application/json"},
                                                                       **(headers or {})))
        try:
            resp = conn.getresponse()
            return resp.status, resp.read().decode()
        finally:
            conn.close()

    def test_answers_without_the_network(self):
        never = mock.Mock(side_effect=AssertionError("replay must not call upstream"))
        with mock.patch.object(proxy.Proxy, "post", never), \
                mock.patch.object(http.client.HTTPSConnection, "connect", never):
            status, text = self.post("/bench/mark", {"label": "m~s", "journey": "jira", "rep": 0})
            self.assertEqual(status, 200)
            status, text = self.post("/r/m~s/api/v1/chat/completions", body(), {replay.JOURNEY_HEADER: "jira"})
            self.assertEqual(status, 200)
            self.assertEqual(json.loads(text)["choices"][0]["message"]["content"], '{"key":"PLNT-1"}')
            status, text = self.post("/r/m~s/api/v1/chat/completions", dict(body(), stream=True),
                                     {replay.JOURNEY_HEADER: "jira"})
            self.assertEqual(status, 200)  # createIssue #1: nearest is #0
            self.assertIn('"content": "{\\"key\\":\\"PLNT-1\\"}"', text)
            self.assertIn("data: [DONE]", text)
            status, text = self.post("/r/m~s/api/v1/chat/completions", body(op="getIssue"),
                                     {replay.JOURNEY_HEADER: "jira"})
            self.assertEqual(status, 503)
            self.assertIn(replay.UNMATCHED, text)
        never.assert_not_called()
        matches = [line["match"] for line in lines(os.path.join(self.dir, "log.jsonl"))]
        self.assertEqual(matches, ["key", "nearest", "none"])


def binary():
    target = os.environ.get("CARGO_TARGET_DIR") or "target"
    path = os.environ.get("MB_BENCH_BINARY") or os.path.join(REPO, target, "release", "mockingbird")
    return path if os.path.exists(path) else None


@unittest.skipUnless(binary(), "needs the release binary (cargo build --release -p mockingbird-cli)")
class EndToEndTest(unittest.TestCase):
    """Records the `journeys` suite against fake_llm.py, replays it (green), then makes
    the recording claim a check passed that the engine fails, and the gate goes red."""

    def test_record_replay_gate(self):
        d = tempfile.mkdtemp()
        self.addCleanup(shutil.rmtree, d)
        env = {k: v for k, v in os.environ.items() if k != "OPENROUTER_API_KEY"}
        script = os.path.join(HERE, "replay.py")
        rec = subprocess.run([sys.executable, script, "record", "--fake", "--suite", "journeys", "--dest", d,
                              "--binary", binary(), "--calibrate", "1"],
                             env=env, capture_output=True, text=True, timeout=300)
        self.assertEqual(rec.returncode, 0, rec.stdout + rec.stderr)
        [path] = replay.recording_paths([d])
        doc = replay.read_recording(path)
        self.assertGreater(len(doc["calls"]), 0)
        self.assertEqual(replay.scan([d]), [])
        gate = [sys.executable, script, "gate", "--recordings", d, "--binary", binary(), "--confirm", "1"]
        ok = subprocess.run(gate, env=env, capture_output=True, text=True, timeout=300)
        self.assertEqual(ok.returncode, 0, ok.stdout + ok.stderr)
        self.assertIn("| journeys |", ok.stdout)
        # A check the engine fails, claimed as passed: the gate must call it a regression.
        failing = sorted(c for c, v in doc["results"].items() if not v and c not in doc.get("unstable", []))
        self.assertTrue(failing)
        doc["results"][failing[0]] = True
        replay.write_recording(path, doc)
        bad = subprocess.run(gate, env=env, capture_output=True, text=True, timeout=300)
        self.assertEqual(bad.returncode, 1, bad.stdout + bad.stderr)
        self.assertIn(failing[0], bad.stdout)
        self.assertIn("REGRESSED", bad.stdout)


if __name__ == "__main__":
    unittest.main()
