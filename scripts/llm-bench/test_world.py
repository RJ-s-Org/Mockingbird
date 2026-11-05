"""Tests for the `--world` mode: blind authoring, the cache per contract hash,
applying a description, and the aggregate split. Fakes only, no paid calls.
Run: python3 -m unittest scripts/llm-bench/test_world.py

The end-to-end test (`StubAuthorEndToEndTest`) runs the stub author against a real
`mockingbird serve` and is skipped when target/release/mockingbird is not built."""

import argparse
import io
import json
import os
import shutil
import subprocess
import sys
import tempfile
import threading
import unittest
from contextlib import redirect_stderr
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from unittest import mock

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))

import aggregate  # noqa: E402
import server_suites  # noqa: E402
import world_author  # noqa: E402

BINARY = os.path.join(world_author.REPO, "target", "release", "mockingbird")
CONTRACT = os.path.join(world_author.REPO, "fixtures", "openapi", "orders-3.0.yaml")


def author_args(**kw):
    a = argparse.Namespace(author="stub", author_model="claude-opus-5-5", author_isolation="restricted", author_budget=5.0, author_timeout=600,
                           author_jobs=1, world_cache=None, refresh=False, claude="claude", binary=BINARY,
                           keep=False)
    for k, v in kw.items():
        setattr(a, k, v)
    return a


def tool_use(i, name, **inp):
    return json.dumps({"type": "assistant", "message": {"content": [
        {"type": "tool_use", "id": i, "name": f"mcp__mockingbird__{name}", "input": inp}]}})


def tool_result(i, text, error=False):
    return json.dumps({"type": "user", "message": {"content": [
        {"type": "tool_result", "tool_use_id": i, "is_error": error, "content": [{"type": "text", "text": text}]}]}})


class TranscriptTest(unittest.TestCase):
    def test_world_writes_and_the_result_are_read(self):
        lines = [
            json.dumps({"type": "system", "subtype": "init", "model": "claude-opus-5-5"}),
            tool_use("a", "get_world", mock="m"),
            tool_result("a", json.dumps({"world": {}})),
            tool_use("b", "patch_world", mock="m", patch={}),
            tool_result("b", json.dumps({"revision": 1, "applied": {}, "refused": [
                {"path": "/operations/x", "code": "unresolved", "message": "no such operation"}]})),
            tool_use("c", "put_world", mock="m", world={}),
            tool_result("c", json.dumps({"type": "about:blank", "status": 422, "detail": "invalid"}), error=True),
            "not json",
            json.dumps({"type": "result", "total_cost_usd": 1.25, "num_turns": 7, "duration_ms": 90000}),
        ]
        result, init, writes = world_author.parse_transcript(lines)
        self.assertEqual(init["model"], "claude-opus-5-5")
        self.assertEqual(result["total_cost_usd"], 1.25)
        self.assertEqual([w["tool"] for w in writes], ["patch_world", "put_world"])
        self.assertTrue(writes[0]["ok"])
        self.assertEqual(writes[0]["revision"], 1)
        self.assertEqual(writes[0]["refused"][0]["code"], "unresolved")
        self.assertFalse(writes[1]["ok"])
        self.assertEqual(writes[1]["problem"]["status"], 422)

    def test_an_unparsed_answer_is_unknown_not_failed(self):
        _, _, writes = world_author.parse_transcript([
            tool_use("a", "patch_world", mock="m", patch={}), tool_result("a", "[output saved to a file]")])
        self.assertIsNone(writes[0]["ok"])
        row = world_author.summary_row({"writes": writes}, False, "x")
        self.assertEqual((row["writes_failed"], row["writes_unparsed"]), (0, 1))

    def test_the_stored_transcript_drops_paths_and_session_ids(self):
        text = world_author.clean_transcript([json.dumps({"type": "system", "subtype": "init", "cwd": "/var/x",
                                                          "session_id": "s", "model": "m"}), "junk"])
        self.assertEqual(json.loads(text), {"type": "system", "subtype": "init", "model": "m"})


class BlindAgentTest(unittest.TestCase):
    SRV = argparse.Namespace(admin="http://127.0.0.1:9", data="http://127.0.0.1:8")

    def test_the_agent_sees_the_contract_and_the_world_tools_only(self):
        args = author_args(author="claude", mcp_tools=["get_world", "patch_world", "import_contract", "admin_api"])
        cmd = world_author.claude_command(args, "/w", self.SRV, "c.yaml", "c")
        for flag in ("--restricted", "--strict-mcp-config", "--no-session-persistence"):
            self.assertIn(flag, cmd)
        self.assertEqual(cmd[cmd.index("--tools") + 1], "Read,Grep,Glob")
        self.assertEqual(cmd[cmd.index("--permission-prompts") + 1], "none")
        allowed = cmd[cmd.index("--allowedTools") + 1].split(",")
        self.assertIn("mcp__mockingbird__patch_world", allowed)
        for tool in ("Bash", "WebFetch", "WebSearch", "mcp__mockingbird__import_contract",
                     "mcp__mockingbird__start_server", "mcp__mockingbird__open_key_setup"):
            self.assertNotIn(tool, allowed)
        self.assertEqual(cmd[cmd.index("--disallowedTools") + 1].split(","),
                         ["mcp__mockingbird__import_contract", "mcp__mockingbird__admin_api"])
        self.assertNotIn("--bare", cmd)
        self.assertIn("--bare", world_author.claude_command(author_args(author="claude", author_isolation="bare"),
                                                            "/w", self.SRV, "c.yaml", "c"))
        mcp = json.loads(cmd[cmd.index("--mcp-config") + 1])
        self.assertEqual(list(mcp["mcpServers"]), ["mockingbird"])
        # call_mock must reach the authoring server's data plane, not a server on :8080.
        self.assertEqual(mcp["mcpServers"]["mockingbird"]["env"], {"MB_DATA_URL": "http://127.0.0.1:8"})
        prompt = cmd[2].lower()
        for word in ("journey", "check", "bench", "score"):
            self.assertNotIn(word, prompt)

    def test_the_agent_gets_no_openrouter_key(self):
        with mock.patch.dict(os.environ, {"OPENROUTER_API_KEY": "sk-or-v1-abcdefabcdef", "MB_STORAGE_URL": "x",
                                          "COPY_OF_IT": "sk-or-v1-abcdefabcdef", "OPENAI_API_KEY": "o",
                                          "ANTHROPIC_API_KEY": "a"}):
            env = world_author.author_env(self.SRV)
        for name in ("OPENROUTER_API_KEY", "MB_STORAGE_URL", "COPY_OF_IT", "OPENAI_API_KEY"):
            self.assertNotIn(name, env)
        self.assertEqual(env["ANTHROPIC_API_KEY"], "a", "the author's own login stays")
        self.assertEqual((env["MB_ADMIN_URL"], env["MB_DATA_URL"]), ("http://127.0.0.1:9", "http://127.0.0.1:8"))

    def test_a_session_that_offers_more_is_refused(self):
        ok = {"tools": ["Read", "Grep", "Glob", "mcp__mockingbird__get_world"],
              "mcp_servers": [{"name": "mockingbird", "status": "connected"}], "plugins": []}
        self.assertEqual(world_author.isolation_problems(ok, "claude"), [])
        self.assertEqual(len(world_author.isolation_problems(dict(ok, tools=ok["tools"] + ["Bash"]), "claude")), 1)
        self.assertEqual(len(world_author.isolation_problems(
            dict(ok, mcp_servers=ok["mcp_servers"] + [{"name": "github"}]), "claude")), 1)
        self.assertEqual(len(world_author.isolation_problems(dict(ok, plugins=[{"name": "p"}]), "claude")), 1)
        self.assertEqual(len(world_author.isolation_problems(None, "claude")), 1)

    def test_a_key_is_never_written(self):
        d = tempfile.mkdtemp()
        self.addCleanup(shutil.rmtree, d)
        with self.assertRaises(RuntimeError):
            world_author.write_json_atomic(os.path.join(d, "x.json"), {"summary": "sk-or-v1-0123456789abcdef"})
        with self.assertRaises(RuntimeError):
            world_author.write_text_atomic(os.path.join(d, "t.jsonl"), "sk-ant-api03-0123456789abcdef")
        with self.assertRaises(RuntimeError):
            world_author.append_jsonl(os.path.join(d, "x.jsonl"), {"s": "sk-or-v1-0123456789abcdef"})
        self.assertEqual(os.listdir(d), [])


class CacheTest(unittest.TestCase):
    def setUp(self):
        self.dir = tempfile.mkdtemp()
        self.addCleanup(shutil.rmtree, self.dir)
        self.a = os.path.join(self.dir, "a.yaml")
        self.b = os.path.join(self.dir, "b.yaml")
        for path, text in ((self.a, "openapi: 3.0.0 # a\n"), (self.b, "openapi: 3.0.0 # b\n")):
            with open(path, "w") as f:
                f.write(text)
        self.calls = []
        self.failing = False

    def fake_author(self, contract, args, sha=None):
        self.calls.append(os.path.basename(contract))
        return {"inputs": dict(args.author_inputs), "is_error": self.failing, "error": "exit code 1" if self.failing else None,"contract": contract, "contract_sha256": sha, "mock": world_author.mock_name_of(contract),
                "author": {"id": world_author.author_id(args.author, args.author_model)},
                "cost_usd": 0.5, "duration_ms": 60000, "num_turns": 4, "revision": 2,
                "document": {"version": 1, "types": {}}, "writes": [{"ok": True, "refused": []}],
                "refused": [], "gaps": [], "heuristic_gaps": [{}], "_transcript": "{}"}

    def run_once(self, contracts, **kw):
        out = os.path.join(self.dir, "out")
        inputs = {"skills_sha256": kw.pop("skills", "s1"), "world_schema_sha256": "w1"}
        args = author_args(world_cache=os.path.join(self.dir, "cache"), **kw)
        with mock.patch.object(world_author, "author_contract", self.fake_author), \
                mock.patch.object(world_author, "author_inputs", lambda binary: inputs), \
                mock.patch.object(world_author, "mcp_tool_names", lambda binary: ["get_world"]), \
                redirect_stderr(io.StringIO()):
            return world_author.ensure_authored(contracts, args, out)

    def rows(self):
        with open(os.path.join(self.dir, "out", "world-authoring.jsonl")) as f:
            return [json.loads(line) for line in f]

    def test_a_rerun_reads_the_cache_and_pays_nothing(self):
        first = self.run_once([self.a, self.b, self.a])
        self.assertEqual(sorted(self.calls), ["a.yaml", "b.yaml"])
        self.assertNotIn("_transcript", first[self.a])
        second = self.run_once([self.a, self.b])
        self.assertEqual(sorted(self.calls), ["a.yaml", "b.yaml"], "nothing authored again")
        self.assertEqual(second[self.a]["document"], first[self.a]["document"])
        rows = self.rows()
        self.assertEqual([r["cached"] for r in rows], [False, False, True, True])
        self.assertTrue(all(r["cost_usd"] == 0.5 for r in rows), "a cache hit keeps the first cost")
        sha = world_author.sha256_file(self.a)
        aid = world_author.author_id("stub", "opus")
        self.assertTrue(os.path.exists(os.path.join(self.dir, "cache", aid, f"{sha}.json")))
        self.assertTrue(os.path.exists(os.path.join(self.dir, "cache", aid, f"{sha}.transcript.jsonl")))

    def test_a_changed_contract_or_author_is_authored_again(self):
        self.run_once([self.a])
        with open(self.a, "a") as f:
            f.write("# changed\n")
        self.run_once([self.a])
        self.run_once([self.a], author="claude", author_model="claude-sonnet-5-5")
        self.run_once([self.a], author="claude", author_model="claude-sonnet-5-5", skills="s2")
        self.assertEqual(self.calls, ["a.yaml"] * 4)

    def test_the_same_content_under_two_paths_is_authored_once(self):
        c = os.path.join(self.dir, "copy.yaml")
        shutil.copyfile(self.a, c)
        got = self.run_once([self.a, c])
        self.assertEqual(len(self.calls), 1)
        self.assertIs(got[self.a], got[c])

    def test_a_failed_authoring_is_kept_apart_and_tried_again(self):
        self.failing = True
        got = self.run_once([self.a])
        self.assertTrue(got[self.a]["is_error"])
        sha = world_author.sha256_file(self.a)
        folder = os.path.join(self.dir, "cache", world_author.author_id("stub", "x"))
        self.assertTrue(os.path.exists(os.path.join(folder, f"{sha}.failed.json")))
        self.assertFalse(os.path.exists(os.path.join(folder, f"{sha}.json")))
        self.failing = False
        self.run_once([self.a])
        self.assertEqual(len(self.calls), 2, "a failure is never read back from the cache")

    def test_two_processes_author_a_contract_once(self):
        """A second process waits on the contract's lock and then reads the cache."""
        script = (
            "import sys, os, json, time\n"
            f"sys.path.insert(0, {json.dumps(os.path.dirname(os.path.abspath(__file__)))})\n"
            "import world_author as w, argparse\n"
            "def fake(contract, args, sha=None):\n"
            "    open(sys.argv[2], 'a').write('x')\n"
            "    time.sleep(1)\n"
            "    return {'contract_sha256': sha, 'document': {}, 'writes': [], '_transcript': ''}\n"
            "w.author_contract = fake\n"
            "w.author_inputs = lambda b: {}\n"
            "a = argparse.Namespace(author='stub', author_model='m', world_cache=sys.argv[3], refresh=False,\n"
            "                       author_jobs=1, binary='b')\n"
            "w.ensure_authored([sys.argv[1]], a)\n")
        marks = os.path.join(self.dir, "marks")
        procs = [subprocess.Popen([sys.executable, "-c", script, self.a, marks, os.path.join(self.dir, "cache")],
                                  stderr=subprocess.DEVNULL) for _ in range(2)]
        self.assertEqual([p.wait(timeout=60) for p in procs], [0, 0])
        with open(marks) as f:
            self.assertEqual(f.read(), "x")

    def test_refresh_authors_again(self):
        self.run_once([self.a])
        self.run_once([self.a], refresh=True)
        self.assertEqual(self.calls, ["a.yaml", "a.yaml"])


class FakeAdmin(BaseHTTPRequestHandler):
    """The world endpoints of the Admin API: GET answers a tier-0 view, PUT records
    the document and author and answers a revision; a 'bad' mock answers 422."""
    puts = []

    def log_message(self, *a):
        pass

    def answer(self, status, body):
        raw = json.dumps(body).encode()
        self.send_response(status)
        self.send_header("content-type", "application/json")
        self.send_header("content-length", str(len(raw)))
        self.end_headers()
        self.wfile.write(raw)

    def do_GET(self):
        self.answer(200, {"version": 1, "revision": 0, "world": {"version": 1, "types": {"pet": {
            "identity": {"member": "id"}}}}, "refused": [], "gaps": [], "authors": {}})

    def do_PUT(self):
        body = json.loads(self.rfile.read(int(self.headers["content-length"])))
        FakeAdmin.puts.append((self.path, body))
        if "/mocks/bad/" in self.path:
            return self.answer(422, {"status": 422, "detail": "invalid", "path": "/types"})
        self.answer(201, {"version": 1, "revision": 1, "applied": body, "refused": [
            {"path": "/operations/x", "code": "unresolved", "message": "m"}]})


class ApplyTest(unittest.TestCase):
    def setUp(self):
        FakeAdmin.puts = []
        self.httpd = ThreadingHTTPServer(("127.0.0.1", 0), FakeAdmin)
        threading.Thread(target=self.httpd.serve_forever, daemon=True).start()
        self.addCleanup(self.httpd.server_close)
        self.addCleanup(self.httpd.shutdown)
        self.api = f"http://127.0.0.1:{self.httpd.server_address[1]}/api/v1/workspaces/default"

    def test_none_writes_nothing(self):
        rec = world_author.apply_world(self.api, "pets", "none")
        self.assertEqual(rec["revision"], 0)
        self.assertEqual(FakeAdmin.puts, [])

    def test_derived_writes_the_heuristic_description_back(self):
        rec = world_author.apply_world(self.api, "pets", "derived")
        (path, body), = FakeAdmin.puts
        self.assertIn("/mocks/pets/contracts/active/world?author=user", path)
        self.assertEqual(body["types"]["pet"]["identity"]["member"], "id")
        self.assertEqual((rec["revision"], len(rec["refused"])), (1, 1))

    def test_agent_writes_the_cached_document_as_the_agent(self):
        entry = {"document": {"version": 1, "types": {"pet": {"identity": {"member": "name"}}}},
                 "author": {"id": "stub~p1"}, "contract_sha256": "abc", "revision": 3}
        rec = world_author.apply_world(self.api, "pets", "agent", entry)
        (path, body), = FakeAdmin.puts
        self.assertIn("author=agent", path)
        self.assertEqual(body, entry["document"])
        self.assertEqual((rec["author_id"], rec["authored_revision"], rec["revision"]), ("stub~p1", 3, 1))

    def test_an_agent_that_wrote_nothing_leaves_the_heuristics(self):
        rec = world_author.apply_world(self.api, "pets", "agent", {"document": None, "revision": 0})
        self.assertEqual(rec["status"], "nothing_authored")
        self.assertEqual(FakeAdmin.puts, [])

    def test_a_failed_authoring_fails_the_suite(self):
        with self.assertRaises(RuntimeError):
            world_author.apply_world(self.api, "pets", "agent", {"is_error": True, "error": "exit code 1",
                                                                 "document": {"version": 1}})
        self.assertEqual(FakeAdmin.puts, [])

    def test_a_whole_document_refusal_fails_the_suite(self):
        with self.assertRaises(RuntimeError):
            world_author.apply_world(self.api, "bad", "agent", {"document": {"version": 1}})

    def test_apply_worlds_records_each_mock(self):
        out = tempfile.mkdtemp()
        self.addCleanup(shutil.rmtree, out)
        srv = argparse.Namespace(api=self.api)
        args = argparse.Namespace(world="derived", out=out, model="m/x", authored={})
        mocks = {"pets": ("fixtures/openapi/orders-3.0.yaml", "/pets", None)}
        with redirect_stderr(io.StringIO()):
            server_suites.apply_worlds(srv, args, "journeys", mocks)
        with open(os.path.join(out, "world-applied.jsonl")) as f:
            rec, = [json.loads(line) for line in f]
        self.assertEqual((rec["model"], rec["suite"], rec["world"], rec["refused_n"]), ("m/x", "journeys", "derived", 1))


class RowsTest(unittest.TestCase):
    def test_rows_carry_the_world_only_when_one_is_written(self):
        out = tempfile.mkdtemp()
        self.addCleanup(shutil.rmtree, out)
        with redirect_stderr(io.StringIO()):
            server_suites.Sink(out, "m/x", "journeys").add("c", 0, True, 1.0, "llm", {})
            server_suites.Sink(out, "m/x", "journeys", "agent").add("c", 0, True, 1.0, "llm", {})
        with open(os.path.join(out, "server.jsonl")) as f:
            rows = [json.loads(line) for line in f]
        self.assertNotIn("world", rows[0])
        self.assertEqual(rows[1]["world"], "agent")

    def test_the_proxy_label_names_the_world(self):
        self.assertEqual(server_suites.run_label("a/b", "journeys", argparse.Namespace(world="none")), "a__b~journeys")
        self.assertEqual(server_suites.run_label("a/b", "journeys", argparse.Namespace(world="agent")),
                         "a__b~journeys@agent")


def row(suite, ok, world=None, case="c"):
    r = {"model": "m/x", "suite": suite, "case": case, "rep": 0, "ok": ok, "source": "llm", "detail": {}}
    if world:
        r["world"] = world
    return r


class AggregateSplitTest(unittest.TestCase):
    ROWS = [row("journeys_blind11", True), row("journeys_blind11", False),
            row("journeys_blind11", True, "derived"), row("journeys_blind11", False, "derived"),
            row("journeys_blind11", True, "agent"), row("journeys_blind11", True, "agent"),
            row("consistency", True), row("consistency", False, "agent"), row("journeys_blind10", True)]

    def test_the_tiers_sit_side_by_side(self):
        split = aggregate.world_split(self.ROWS)
        self.assertEqual(set(split), {"journeys_blind11", "consistency"}, "suites run without a world are left out")
        self.assertEqual({t: v["ok_rate"] for t, v in split["journeys_blind11"].items()},
                         {"none": 0.5, "derived": 0.5, "agent": 1.0})
        self.assertEqual(list(split["journeys_blind11"]), ["none", "derived", "agent"])

    def test_world_calls_stay_out_of_the_ledger_blocks(self):
        d = tempfile.mkdtemp()
        self.addCleanup(shutil.rmtree, d)
        call = {"model_requested": "m/x", "model_slug": "m__x", "status": 200, "cost_usd": 0.01, "total_ms": 100}
        with open(os.path.join(d, "calls.jsonl"), "w") as f:
            f.write(json.dumps(dict(call, label="m__x~journeys", workload="journeys")) + "\n")
            for _ in range(3):
                f.write(json.dumps(dict(call, label="m__x~journeys@agent", workload="journeys@agent",
                                        status=503, error_kind="http")) + "\n")
        with open(os.path.join(d, "server.jsonl"), "w") as f:
            for r in self.ROWS:
                f.write(json.dumps(r) + "\n")
        res = aggregate.analyse(d, os.path.join(d, "judge.jsonl"), os.path.join(d, "judge-key.json"), None, "m/x")
        self.assertEqual(res["models"]["m/x"]["ledger_rows"], 1)
        self.assertEqual(res["world"]["ledger"], {"agent": {"calls": 3, "usd": 0.03}})

    def test_no_world_key_without_world_runs(self):
        self.assertEqual(aggregate.world_summary([], [], []), {})

    def test_a_world_run_changes_no_scored_block(self):
        block = aggregate.server_block(self.ROWS)
        self.assertEqual(block["consistency"]["n"], 1)
        self.assertEqual(set(block["journeys"]), {"journeys_blind10", "journeys_blind11",
                                                  "journeys_blind11@derived", "journeys_blind11@agent"})
        self.assertEqual(block["journeys"]["journeys_blind11"]["n"], 2)
        self.assertEqual(block["journeys"]["journeys_blind11@agent"]["ok_rate"], 1.0)

    def test_authoring_keeps_the_first_cost_over_cache_hits(self):
        authored = {"author_id": "a", "contract_sha256": "s", "mock": "m", "cached": False, "cost_usd": 1.5,
                    "duration_ms": 120000, "is_error": False}
        hit = dict(authored, cached=True)
        again = dict(authored, cost_usd=2.0)
        w = aggregate.world_summary([authored, hit, again], [{"world": "agent", "refused_n": 2},
                                                             {"world": "agent", "refused_n": 0},
                                                             {"world": "agent", "status": "nothing_authored"}])
        self.assertEqual(len(w["contracts"]), 1)
        self.assertEqual((w["contracts"][0]["cached"], w["contracts"][0]["cost_usd"]), (False, 2.0))
        self.assertEqual((w["authoring_runs"], w["authoring_cost_usd"]), (2, 3.5), "a --refresh is paid twice")
        self.assertEqual(w["applied"], {"agent": {"writes": 2, "nothing_authored": 1, "refused": 2}})

    def test_a_tier_that_failed_to_apply_shows_its_errors(self):
        split = aggregate.world_split([row("j", True), dict(row("j", False, "agent"), case="suite_error")])
        self.assertEqual(split["j"]["agent"], {"n": 0, "ok": 0, "ok_rate": None, "suite_errors": 1})

    def test_tables_show_the_split(self):
        results = {"models": {"m/x": {"server": aggregate.server_block(self.ROWS)}},
                   "world": aggregate.world_summary([{"author_id": "a", "contract_sha256": "s", "mock": "bc",
                                                      "contract": "c.yaml", "cost_usd": 1.0, "duration_ms": 60000,
                                                      "num_turns": 3, "writes": 2, "writes_failed": 0,
                                                      "entries_refused_in_writes": 1, "refused_final": 0,
                                                      "gaps_before": 5, "gaps_after": 1}], [])}
        text = "\n".join(aggregate.world_tables(results))
        self.assertIn("| m/x | journeys_blind11 | 50.0% (1/2) | 50.0% (1/2) | 100.0% (2/2) |", text)
        self.assertIn("1 authoring runs", text)
        self.assertIn("5 → 1", text)
        self.assertEqual(aggregate.world_tables({"models": {"m/x": {"server": aggregate.server_block(
            [row("journeys", True)])}}, "world": {}}), [])


@unittest.skipUnless(os.path.exists(BINARY), "target/release/mockingbird is not built")
class StubAuthorEndToEndTest(unittest.TestCase):
    """The stub author writes through the real CLI onto a real server: the entry holds
    the accepted revision, the writes' answers (one refusal) and a zero cost."""

    def test_stub_authoring_round_trip(self):
        with redirect_stderr(io.StringIO()):
            entry = world_author.author_contract(CONTRACT, author_args())
        self.assertEqual(entry["exit_code"], 0, entry["stderr_tail"])
        self.assertFalse(entry["is_error"])
        self.assertEqual(entry["cost_usd"], 0.0)
        self.assertEqual(entry["author"]["model"], "stub")
        self.assertGreaterEqual(entry["revision"], 1)
        self.assertIsInstance(entry["document"], dict)
        refused = [r for w in entry["writes"] for r in (w.get("refused") or [])]
        self.assertTrue(any("stubNoSuchOperation" in r.get("path", "") for r in refused), entry["writes"])
        # A refused entry is left out of the stored revision: only the write's answer has it.
        self.assertNotIn("stubNoSuchOperation", json.dumps(entry["document"]))
        self.assertEqual(entry["refused"], [])
        self.assertEqual([w["revision"] for w in entry["writes"]], [r["revision"] for r in entry["revisions"]])
        self.assertTrue(entry["authors"])
        self.assertTrue(all(a == "agent" for a in entry["authors"].values()), entry["authors"])
        self.assertNotIn("_transcript_raw", entry)
        self.assertIn('"type": "result"', entry["_transcript"])

    def test_call_mock_reaches_the_authoring_server(self):
        """The MCP server the agent gets, with the agent's environment, sends call_mock
        to the authoring server's data plane (not the default :8080)."""
        work = tempfile.mkdtemp()
        self.addCleanup(shutil.rmtree, work)
        shutil.copyfile(CONTRACT, os.path.join(work, "orders.yaml"))
        with world_author.AuthoringServer(BINARY, os.path.join(work, "orders.yaml"), "orders", work) as srv:
            cmd = world_author.claude_command(author_args(author="claude"), work, srv, "orders.yaml", "orders")
            mcp = json.loads(cmd[cmd.index("--mcp-config") + 1])["mcpServers"]["mockingbird"]
            msgs = [{"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {
                        "protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "t", "version": "1"}}},
                    {"jsonrpc": "2.0", "method": "notifications/initialized"},
                    {"jsonrpc": "2.0", "id": 2, "method": "tools/call", "params": {"name": "call_mock", "arguments": {
                        "mock": "orders", "method": "GET", "path": "/orders"}}}]
            env = dict(world_author.author_env(srv), **mcp["env"])
            proc = subprocess.run([mcp["command"]] + mcp["args"], input="".join(json.dumps(m) + "\n" for m in msgs),
                                  capture_output=True, text=True, env=env, timeout=60)
        answer = [json.loads(line) for line in proc.stdout.splitlines() if '"id":2' in line.replace(" ", "")][0]
        text = json.dumps(answer)
        self.assertNotIn("cannot reach the data plane", text)
        self.assertIn(srv.data.split(":")[-1], text + "", text[:500])


if __name__ == "__main__":
    unittest.main()
