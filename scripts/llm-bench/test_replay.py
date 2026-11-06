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
import socket
import subprocess
import sys
import tempfile
import threading
import time
import unittest
from http.server import ThreadingHTTPServer
from unittest import mock

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)

import proxy  # noqa: E402
import replay  # noqa: E402
import server_suites  # noqa: E402
import world_author  # noqa: E402

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


class WorldTierTest(unittest.TestCase):
    """A recording of the agent tier (`<label>@agent`) is a set of its own: its results
    are the rows of that world only, and the gate replays it with --world agent."""

    def setUp(self):
        self.dir = tempfile.mkdtemp()
        self.addCleanup(shutil.rmtree, self.dir)

    def test_split_label(self):
        self.assertEqual(replay.split_label("a__b~journeys_blind11@agent"), ("a__b", "journeys_blind11", "agent"))
        self.assertEqual(replay.split_label("a__b~journeys"), ("a__b", "journeys", "none"))

    def test_pack_keeps_the_tiers_apart(self):
        raw = os.path.join(self.dir, "raw.jsonl")
        with open(raw, "w") as f:
            for label in ("a__b~journeys", "a__b~journeys@agent"):
                f.write(json.dumps(dict(call("jira", "createIssue", 0, "{}"), label=label)) + "\n")
        server = os.path.join(self.dir, "server.jsonl")
        with open(server, "w") as f:
            f.write(json.dumps({"model": "a/b", "suite": "journeys", "case": "jira.x", "rep": 0, "ok": False}) + "\n")
            f.write(json.dumps({"model": "a/b", "suite": "journeys", "case": "jira.x", "rep": 0, "ok": True,
                                "world": "agent"}) + "\n")
        paths = replay.pack(raw, server, os.path.join(self.dir, "out"), source="test")
        docs = {os.path.basename(p): replay.read_recording(p) for p in paths}
        self.assertEqual(sorted(docs), ["a__b~journeys.json.gz", "a__b~journeys@agent.json.gz"])
        none, agent = docs["a__b~journeys.json.gz"], docs["a__b~journeys@agent.json.gz"]
        self.assertNotIn("world", none, "a none-tier recording is as before")
        self.assertEqual((agent["suite"], agent["world"], agent["results"]), ("journeys", "agent", {"jira.x": True}))
        self.assertEqual(none["results"], {"jira.x": False})

    def test_compare_reads_the_rows_of_its_world(self):
        doc = dict(recording([call("jira", "createIssue", 0, "{}")], {"jira.x": True}, label="m~journeys_x@agent"),
                   world="agent")
        rows = [{"model": "m", "suite": "journeys_x", "case": "jira.x", "rep": 0, "ok": False},
                {"model": "m", "suite": "journeys_x", "case": "jira.x", "rep": 0, "ok": True, "world": "agent"}]
        calls = [{"label": "m~journeys_x@agent", "journey": "jira", "rep": 0, "op": "createIssue", "n": 0,
                  "match": "exact", "used": ["jira", 0, "createIssue", 0]}]
        [r] = replay.verdicts([doc], rows, calls)
        self.assertEqual((r["suite"], r["regressions"], r["replay_ok"]), ("journeys_x@agent", [], 1))

    def test_a_suite_with_no_result_fails_rather_than_masks(self):
        doc = recording([call("jira", "createIssue", 0, "{}")], {"jira.x": True}, label="m~journeys_x@agent")
        r = replay.compare(dict(doc, world="agent"), [], [])
        self.assertTrue(r["suite_error"])
        self.assertIn("jira", r["dirty"])

    def test_the_gate_runs_each_tier_with_its_world(self):
        docs = [recording([], label="m~journeys_x"),
                dict(recording([], label="m~journeys_x@agent"), world="agent")]
        args = argparse.Namespace(binary="b", jobs=1, keep=False, world_cache=None)
        runs = []
        with mock.patch.object(replay, "run_suites", lambda *a, **kw: runs.append((a[0], a[1], kw["world"]))), \
                mock.patch.object(replay, "wait_http", lambda *a, **kw: None), \
                mock.patch.object(replay.subprocess, "Popen", mock.Mock()), \
                mock.patch.object(replay, "stop", lambda p: None):
            replay.replay_once(docs, [], args)
        self.assertEqual(runs, [("m", ["journeys_x"], "agent"), ("m", ["journeys_x"], "none")])

    def test_the_agent_tier_reads_the_cache_only(self):
        seen = []
        args = argparse.Namespace(binary="b", jobs=1, world_cache=None)
        with mock.patch.object(replay.subprocess, "call", lambda cmd, **kw: seen.append(cmd) or 0):
            replay.run_suites("m", ["journeys_x"], "http://p", self.dir, args, 1, world="agent")
            replay.run_suites("m", ["journeys_x"], "http://p", self.dir, args, 1)
        agent, none = seen
        self.assertEqual(agent[agent.index("--world") + 1], "agent")
        self.assertIn("--world-cache-only", agent)
        self.assertNotIn("--world", none)
        self.assertIn("--tag-journeys", none)

    def test_record_defaults_to_the_suites_with_cached_descriptions(self):
        suites = replay.cached_suites()
        for s in ("journeys_blind11", "journeys_blind12", "journeys_blind13"):
            self.assertIn(s, suites)
        self.assertNotIn("journeys", suites)
        self.assertEqual(replay.cached_suites(self.dir), [])


class DumpTest(unittest.TestCase):
    """A proxy --dump carries each call's journey tag, so any run's dump packs into a
    recording (`replay.py pack-dump`)."""

    def setUp(self):
        self.dir = tempfile.mkdtemp()
        self.addCleanup(shutil.rmtree, self.dir)
        port = replay.free_port()
        self.fake = subprocess.Popen([sys.executable, os.path.join(HERE, "fake_llm.py"), "--port", str(port)],
                                     stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        self.addCleanup(replay.stop, self.fake)
        self.dump = os.path.join(self.dir, "exchanges.jsonl")
        args = argparse.Namespace(upstream=f"http://127.0.0.1:{port}/api/v1/chat/completions", no_stream_upgrade=True,
                                  dump=self.dump, ledger=os.path.join(self.dir, "calls.jsonl"), budget_usd=1e9,
                                  prior_ledger=None, prices=None, provider_pins="", no_format_fallback=True,
                                  record=None, replay=None, replay_log=None)
        with mock.patch.dict(os.environ, {"OPENROUTER_API_KEY": "sk-or-v1-dump-test-key"}):
            self.px = proxy.Proxy(args)
        self.addCleanup(self.px.ledger.fh.close)
        proxy.Handler.proxy = self.px
        self.httpd = ThreadingHTTPServer(("127.0.0.1", 0), proxy.Handler)
        threading.Thread(target=self.httpd.serve_forever, daemon=True).start()
        self.addCleanup(self.httpd.shutdown)
        self.addCleanup(self.httpd.server_close)
        for _ in range(100):
            try:
                with socket.create_connection(("127.0.0.1", port), timeout=1):
                    break
            except OSError:
                time.sleep(0.05)

    def post(self, path, doc, headers=None):
        conn = http.client.HTTPConnection("127.0.0.1", self.httpd.server_address[1], timeout=30)
        conn.request("POST", path, body=json.dumps(doc), headers=dict({"content-type": "application/json"},
                                                                       **(headers or {})))
        try:
            resp = conn.getresponse()
            return resp.status, resp.read().decode()
        finally:
            conn.close()

    def test_a_tagged_dump_packs_into_a_recording_that_replays(self):
        label = "m__x~journeys_heldout"
        tagged = {replay.JOURNEY_HEADER: "jira"}
        schema = {"type": "json_schema", "json_schema": {"name": "issue", "schema": {
            "type": "object", "properties": {"key": {"type": "string"}}, "required": ["key"]}}}
        for rep in (0, 1):
            self.assertEqual(self.post("/bench/mark", {"label": label, "journey": "jira", "rep": rep})[0], 200)
            for op in ("createIssue", "createIssue", "getIssue"):
                status, _ = self.post(f"/r/{label}/api/v1/chat/completions",
                                      dict(body(op=op), model="m/x", response_format=schema), tagged)
                self.assertEqual(status, 200)
        dumped = lines(self.dump)
        self.assertEqual(len(dumped), 6)
        self.assertEqual([(d["journey"], d["rep"], d["op"], d["n"]) for d in dumped[3:]],
                         [("jira", 1, "createIssue", 0), ("jira", 1, "createIssue", 1), ("jira", 1, "getIssue", 0)])
        for d in dumped:
            self.assertEqual((d["status"], d["label"]), (200, label))
            self.assertTrue(d["hash"])
            self.assertIsNotNone(d["now"])
        with open(self.dump) as f:
            self.assertNotIn("sk-or-", f.read())
        server = os.path.join(self.dir, "server.jsonl")
        with open(server, "w") as f:
            for rep, ok in ((0, True), (1, False)):
                f.write(json.dumps({"model": "m/x", "suite": "journeys_heldout", "case": "jira.x", "rep": rep, "ok": ok})
                        + "\n")
            # Another suite of the run with no row for this model: not packed.
            f.write(json.dumps({"model": "other/y", "suite": "journeys_y", "case": "a.b", "rep": 1, "ok": True})
                    + "\n")
        out = os.path.join(self.dir, "rec")
        [path] = replay.pack_dump(self.dump, server, out, "test", rep=1)
        doc = replay.read_recording(path)
        self.assertEqual((doc["label"], doc["model"], doc["results"]), (label, "m/x", {"jira.x": False}))
        self.assertEqual([(c["op"], c["n"]) for c in doc["calls"]],
                         [("createIssue", 0), ("createIssue", 1), ("getIssue", 0)])
        self.assertEqual(replay.scan([path]), [])
        # The recording answers the same prompts exactly.
        rp = replay.Replayer([path])
        rp.mark(label, "jira", 1)
        self.assertEqual(rp.answer(label, tagged, body(op="createIssue"))["match"], "key")
        # The CLI does the same, and refuses a dump with an untagged call.
        cli = subprocess.run([sys.executable, os.path.join(HERE, "replay.py"), "pack-dump", self.dump, server,
                              "--rep", "1", "--dest", os.path.join(self.dir, "cli")],
                             capture_output=True, text=True, timeout=60)
        self.assertEqual(cli.returncode, 0, cli.stderr)
        self.assertEqual(os.listdir(os.path.join(self.dir, "cli")), ["m__x~journeys_heldout.json.gz"])
        with open(self.dump, "a") as f:
            f.write(json.dumps({"label": label, "status": 200, "content": "{}"}) + "\n")
        with self.assertRaises(replay.PackRefused) as e:
            replay.pack_dump(self.dump, server, os.path.join(self.dir, "none"), "test", rep=1)
        self.assertIn("without a journey tag", str(e.exception))
        self.assertFalse(os.path.exists(os.path.join(self.dir, "none")))

    def test_a_failed_call_in_the_ledger_beside_the_dump_refuses(self):
        with open(self.dump, "w") as f:
            f.write(json.dumps(dict(call("jira", "createIssue", 0, "{}"), label="m__x~journeys_heldout")) + "\n")
        with open(os.path.join(self.dir, "calls.jsonl"), "a") as f:
            f.write(json.dumps({"label": "m__x~journeys_heldout", "status": 502, "error_kind": "stream_error"}) + "\n")
        server = os.path.join(self.dir, "server.jsonl")
        with open(server, "w") as f:
            f.write(json.dumps({"model": "m/x", "suite": "journeys_heldout", "case": "jira.x", "rep": 0, "ok": True}) + "\n")
        with self.assertRaises(replay.PackRefused) as e:
            replay.pack_dump(self.dump, server, os.path.join(self.dir, "out"), "test")
        self.assertIn("ledger", str(e.exception))

    def test_a_dump_packs_only_its_journeys_suites(self):
        # A run's dump holds every suite of the run, each tagged: only journeys suites pack.
        labels = ("m__x~journeys_heldout", "m__x~consistency", "m__x~journeys_heldout@agent", "m__x~stateful@agent")
        with open(self.dump, "w") as f:
            for label in labels:
                f.write(json.dumps(dict(call("jira", "createIssue", 0, "{}"), label=label)) + "\n")
        server = os.path.join(self.dir, "server.jsonl")
        with open(server, "w") as f:
            for suite, world in (("journeys_heldout", None), ("consistency", None), ("journeys_heldout", "agent"),
                                 ("stateful", "agent")):
                row = {"model": "m/x", "suite": suite, "case": "jira.x", "rep": 0, "ok": True}
                if world:
                    row["world"] = world
                f.write(json.dumps(row) + "\n")
        out = os.path.join(self.dir, "out")
        skipped = []
        written = replay.pack_dump(self.dump, server, out, "test", skipped=skipped)
        self.assertEqual(sorted(os.path.basename(p) for p in written),
                         ["m__x~journeys_heldout.json.gz", "m__x~journeys_heldout@agent.json.gz"])
        self.assertEqual(sorted(os.listdir(out)), sorted(os.path.basename(p) for p in written))
        self.assertEqual(skipped, ["m__x~consistency", "m__x~stateful@agent"])
        cli = subprocess.run([sys.executable, os.path.join(HERE, "replay.py"), "pack-dump", self.dump, server,
                              "--dest", os.path.join(self.dir, "cli")], capture_output=True, text=True, timeout=60)
        self.assertEqual(cli.returncode, 0, cli.stderr)
        self.assertNotIn("consistency", "".join(os.listdir(os.path.join(self.dir, "cli"))))
        self.assertIn("skipped m__x~consistency", cli.stderr)
        self.assertIn("skipped m__x~stateful@agent", cli.stderr)
        # A dump with no journeys suite writes nothing.
        with open(self.dump, "w") as f:
            f.write(json.dumps(dict(call("jira", "createIssue", 0, "{}"), label="m__x~consistency")) + "\n")
        self.assertEqual(replay.pack_dump(self.dump, server, os.path.join(self.dir, "none"), "test"), [])
        self.assertFalse(os.path.exists(os.path.join(self.dir, "none")))


class DefaultTaggingTest(unittest.TestCase):
    """server_suites.py tags journeys by default, so every run's dump is seedable."""

    def setUp(self):
        self.ss = server_suites

    def parse(self, *extra):
        return self.ss.build_parser().parse_args(["--model", "m/x", "--proxy-base", "http://p", "--out", "o",
                                                  *extra])

    def test_on_by_default_and_off_on_request(self):
        self.assertTrue(self.parse().tag_journeys)
        self.assertTrue(self.parse("--tag-journeys").tag_journeys)
        self.assertFalse(self.parse("--no-tag-journeys").tag_journeys)

    def test_the_config_tags_each_mock_by_default(self):
        d = tempfile.mkdtemp()
        self.addCleanup(shutil.rmtree, d)
        args = self.parse()
        path = self.ss.write_config(d, "m/x", "http://p/r/l/api/v1", (1, 2, 3),
                                    {"jira": ("fixtures/openapi/orders-3.0.yaml", "/jira", None)}, "journeys_x",
                                    tag_journeys=args.tag_journeys)
        with open(path) as f:
            text = f.read()
        self.assertIn("x-bench-journey", text)
        self.assertIn("faker_seed: 1", text)

    def test_a_proxy_that_takes_no_marks_is_warned_about_once(self):
        args = self.parse()
        srv = argparse.Namespace(suite="journeys_x")
        refused = mock.Mock(side_effect=OSError("404"))
        err = io.StringIO()
        self.ss.MARK_WARNED.clear()
        with mock.patch.object(self.ss.urllib.request, "urlopen", refused), contextlib.redirect_stderr(err):
            self.ss.mark_rep(srv, args, "jira", 0)
            self.ss.mark_rep(srv, args, "jira", 1)
        self.assertEqual(refused.call_count, 2)
        self.assertEqual(err.getvalue().count("WARNING"), 1)
        self.ss.MARK_WARNED.clear()


WORLD_CACHE = os.path.join(world_author.REPO, world_author.DEFAULT_CACHE)


def tracked_cache_files():
    """The cache files git tracks (relative to the cache), or None outside a checkout."""
    try:
        out = subprocess.run(["git", "ls-files", "--", "."], cwd=WORLD_CACHE, capture_output=True, text=True,
                             timeout=30)
    except (OSError, subprocess.SubprocessError):
        return None
    return out.stdout.split() if out.returncode == 0 else None


class CommittedCacheTest(unittest.TestCase):
    """docs/benchmarks/world-cache, which the agent tier replays on, holds the authored
    descriptions and their metadata only: never an authoring transcript, a lock or a
    failure, and nothing a scan flags."""

    def setUp(self):
        self.files = tracked_cache_files()
        if self.files is None:
            self.skipTest("not a git checkout")

    def test_only_entries_are_tracked(self):
        self.assertTrue(any(f.endswith(".json") for f in self.files), "the cache is committed")
        self.assertFalse([f for f in self.files if f.endswith(".transcript.jsonl")], "a transcript is tracked")
        self.assertEqual(world_author.committed_cache_problems(WORLD_CACHE, self.files), [])

    def test_the_tracked_files_scan_clean(self):
        self.assertEqual(world_author.scan_files([os.path.join(WORLD_CACHE, f) for f in self.files]), [])

    def test_every_journeys_suite_with_cached_descriptions_reads_them(self):
        """The committed layout is a cache hit for the default author: each contract of
        blind11-13 has its entry, found by `--world-cache-only` as by a normal run."""
        for suite in ("journeys_blind11", "journeys_blind12", "journeys_blind13"):
            contracts = [os.path.join(world_author.REPO, c) for c, _, _ in server_suites.RUNNERS[suite][0].values()]
            args = argparse.Namespace(author="claude", author_model=world_author.DEFAULT_AUTHOR_MODEL, world_cache=WORLD_CACHE,
                                      world_cache_only=True)
            with contextlib.redirect_stderr(io.StringIO()):
                got = world_author.ensure_authored(contracts, args)
            self.assertEqual(len(got), len(contracts), suite)
            self.assertTrue(all(e.get("document") is not None or e.get("revision") == 0 for e in got.values()))


class PackRefusalTest(unittest.TestCase):
    """`pack` (and so `record`) writes nothing for a run that tripped its budget or
    has a failed call: such a recording would hold answers the model never gave."""

    def setUp(self):
        self.dir = tempfile.mkdtemp()
        self.addCleanup(shutil.rmtree, self.dir)
        self.raw = os.path.join(self.dir, "raw.jsonl")
        self.server = os.path.join(self.dir, "server.jsonl")
        self.ledger = os.path.join(self.dir, "calls.jsonl")
        self.dest = os.path.join(self.dir, "out")
        self.write(self.raw, [dict(call("jira", "createIssue", 0, "{}"), label="a__b~journeys")])
        self.write(self.server, [{"model": "a/b", "suite": "journeys", "case": "jira.x", "rep": 0, "ok": True,
                                  "detail": {}}])
        self.write(self.ledger, [{"label": "a__b~journeys", "status": 200, "error_kind": None}])

    def write(self, path, rows):
        with open(path, "w") as f:
            for r in rows:
                f.write(json.dumps(r) + "\n")

    def refused(self, needle):
        with self.assertRaises(replay.PackRefused) as cm:
            replay.pack(self.raw, self.server, self.dest, source="test", ledger=self.ledger)
        self.assertIn(needle, " ".join(cm.exception.problems))
        self.assertFalse(os.path.exists(self.dest), "a refused pack must write nothing")

    def test_a_clean_run_packs(self):
        self.assertEqual(replay.pack_problems(self.raw, self.server, self.ledger), [])
        self.assertEqual(len(replay.pack(self.raw, self.server, self.dest, source="test", ledger=self.ledger)), 1)

    def test_budget_tripped(self):
        with open(os.path.join(self.dir, "BUDGET_TRIPPED"), "w") as f:
            f.write("{}\n")
        self.refused("budget tripped")

    def test_a_402_in_the_ledger(self):
        # A budget refusal never reaches the raw recording, only the ledger.
        self.write(self.ledger, [{"status": 200}, {"status": 402, "error_kind": "budget"}])
        self.refused("402")

    def test_a_non_200_call(self):
        self.write(self.raw, [dict(call("jira", "createIssue", 0, "", status=503), label="a__b~journeys",
                                   error_kind="provider_unavailable")])
        self.refused("raw: 1 call(s) with status 503")

    def test_an_in_stream_error_with_a_200_head(self):
        self.write(self.ledger, [{"status": 200, "error_kind": "stream_error"}])
        self.refused("error stream_error")

    def test_a_client_hang_up(self):
        self.write(self.ledger, [{"status": 499, "error_kind": "client_closed"}])
        self.refused("status 499")

    def test_an_infra_label(self):
        for label in ("infra", "infra_cascade"):
            self.write(self.server, [{"model": "a/b", "suite": "journeys", "case": "jira.x", "rep": 0,
                                      "ok": False, "detail": {label: True}}])
            self.refused(f"detail.{label}")

    def test_a_model_refusal_or_empty_answer_is_recorded(self):
        # The model's own answer, not a failed call.
        self.write(self.ledger, [{"status": 200, "error_kind": "refusal"}, {"status": 200, "error_kind": "empty"}])
        self.assertEqual(replay.pack_problems(self.raw, self.server, self.ledger), [])

    def test_a_missing_ledger_refuses(self):
        os.remove(self.ledger)
        self.refused("no ledger")

    def test_pack_cli_exits_non_zero(self):
        self.write(self.ledger, [{"status": 402, "error_kind": "budget"}])
        out = subprocess.run([sys.executable, os.path.join(HERE, "replay.py"), "pack", "--raw", self.raw,
                              "--server", self.server, "--ledger", self.ledger, "--dest", self.dest,
                              "--source", "test"], capture_output=True, text=True, timeout=60)
        self.assertEqual(out.returncode, 1, out.stdout + out.stderr)
        self.assertIn("refusing to pack", out.stderr)
        self.assertFalse(os.path.exists(self.dest))

    def test_record_refuses_a_tripped_run(self):
        """cmd_record packs nothing when the proxy tripped its budget mid-run."""
        def fake_run(model, suites, base, work, args, repeat, world="none"):
            self.write(os.path.join(work, "raw.jsonl"), [dict(call("jira", "createIssue", 0, "{}"),
                                                              label="a__b~journeys")])
            self.write(os.path.join(work, "server.jsonl"), [{"model": "a/b", "suite": "journeys",
                                                             "case": "jira.x", "rep": 0, "ok": True}])
            with open(os.path.join(work, "BUDGET_TRIPPED"), "w") as f:
                f.write("{}\n")
            return 0
        args = argparse.Namespace(binary="target/release/mockingbird", suite=["journeys"], fake=True, model=None,
                                  dest=self.dest, jobs=1, budget_usd=1.5, prices=None, calibrate=0, keep=False)
        with mock.patch.object(replay, "run_suites", fake_run), \
                mock.patch.object(replay, "calibrate", side_effect=AssertionError("calibrated a refused run")):
            self.assertEqual(replay.cmd_record(args), 1)
        self.assertFalse(os.path.exists(self.dest))


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


    def test_agent_tier_record_and_gate(self):
        """--world agent: recorded on the committed world-cache's descriptions (never
        authored), stored as `<label>@agent`, replayed green with the gate's --world agent;
        a cache with no description for the suite fails the gate."""
        d = tempfile.mkdtemp()
        self.addCleanup(shutil.rmtree, d)
        env = {k: v for k, v in os.environ.items() if k != "OPENROUTER_API_KEY"}
        script = os.path.join(HERE, "replay.py")
        rec = subprocess.run([sys.executable, script, "record", "--fake", "--world", "agent",
                              "--suite", "journeys_blind11", "--dest", d, "--binary", binary(), "--calibrate", "1"],
                             env=env, capture_output=True, text=True, timeout=600)
        self.assertEqual(rec.returncode, 0, rec.stdout + rec.stderr)
        [path] = replay.recording_paths([d])
        self.assertTrue(path.endswith("~journeys_blind11@agent.json.gz"), path)
        doc = replay.read_recording(path)
        self.assertEqual((doc["suite"], doc["world"]), ("journeys_blind11", "agent"))
        self.assertGreater(len(doc["calls"]), 0)
        self.assertTrue(any(doc["results"].values()))
        gate = [sys.executable, script, "gate", "--recordings", d, "--binary", binary(), "--confirm", "1"]
        skipped = subprocess.run(gate, env=env, capture_output=True, text=True, timeout=600)
        self.assertEqual(skipped.returncode, 2, "the default tier is none: nothing to replay here")
        ok = subprocess.run(gate + ["--world", "agent"], env=env, capture_output=True, text=True, timeout=600)
        self.assertEqual(ok.returncode, 0, ok.stdout + ok.stderr)
        self.assertIn("| journeys_blind11@agent |", ok.stdout)
        empty = os.path.join(d, "empty-cache")
        os.makedirs(empty)
        miss = subprocess.run(gate + ["--world", "agent", "--world-cache", empty, "--confirm", "0"],
                              env=env, capture_output=True, text=True, timeout=600)
        self.assertEqual(miss.returncode, 1, miss.stdout + miss.stderr)


if __name__ == "__main__":
    unittest.main()
