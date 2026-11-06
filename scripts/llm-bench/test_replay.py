"""Tests for replay.py and the proxy's record and replay modes.

    python3 -m unittest discover -s scripts/llm-bench -p 'test_*.py'

The end-to-end test needs the release binary (target/release/mockingbird, or
MB_BENCH_BINARY); without it, it is skipped.
"""

import argparse
import gzip
import http.client
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
    def doc(self, **kw):
        return recording([], results={"a.read": True, "a.write": True, "a.list": False, "b.read": True,
                                      "b.gone": True, "c.flip": True}, **kw)

    def rows(self, **ok):
        return [{"case": c.replace("_", "."), "ok": v, "rep": 0} for c, v in ok.items()]

    def test_regressions_masked_improvements(self):
        calls = [{"label": "m~journeys_x", "journey": "a", "match": "exact"},
                 {"label": "m~journeys_x", "journey": "a", "match": "key"},
                 {"label": "m~journeys_x", "journey": "b", "match": "nearest"},
                 {"label": "m~journeys_x", "journey": "c", "match": "none"},
                 {"label": "other", "journey": "a", "match": "none"}]
        r = replay.compare(self.doc(), self.rows(a_read=False, a_write=True, a_list=True, b_read=False,
                                                 c_flip=False), calls)
        self.assertEqual(r["regressions"], ["a.read"])
        self.assertEqual(sorted(r["masked"]), ["b.gone", "b.read", "c.flip"])
        self.assertEqual(r["improvements"], ["a.list"])
        self.assertEqual(r["missing"], ["b.gone"])
        self.assertEqual(r["calls"], {"exact": 1, "key": 1, "nearest": 1, "none": 1})
        self.assertIn("a.read", replay.render([r]))

    def test_missing_check_of_a_clean_journey_regresses(self):
        r = replay.compare(self.doc(), self.rows(a_write=True), [])
        self.assertIn("a.read", r["regressions"])

    def test_unstable_checks_never_count(self):
        r = replay.compare(self.doc(unstable=["a.read"]), self.rows(a_read=False, a_write=True), [])
        self.assertNotIn("a.read", r["regressions"])
        self.assertEqual(r["unstable"], 1)

    def test_suite_error_is_reported(self):
        rows = [{"case": "suite_error", "ok": False, "rep": -1, "detail": {"error": "boom"}}]
        self.assertEqual(replay.compare(self.doc(), rows, [])["suite_error"], [{"error": "boom"}])


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
        def fake_run(model, suites, base, work, args, repeat):
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


if __name__ == "__main__":
    unittest.main()
