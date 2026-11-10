"""Unit tests for e2e.py (the end-to-end eval harness) and fakegate's e2e tier. Run with:
python3 -m unittest discover -s scripts/llm-bench -p 'test_*.py'"""

import json
import os
import sys
import tempfile
import unittest

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))

import e2e  # noqa: E402
import fakegate  # noqa: E402
import world_author  # noqa: E402


def write(path, text):
    os.makedirs(os.path.dirname(path), exist_ok=True)
    with open(path, "w") as f:
        f.write(text)
    return path


class TaskTest(unittest.TestCase):
    def test_dev_suites_leave_out_blind14_and_blind15(self):
        suites = e2e.dev_suites()
        self.assertIn("journeys_blind13", suites)
        self.assertIn("consistency", suites)
        self.assertNotIn("journeys_blind14", suites)
        self.assertNotIn("journeys_blind15", suites)

    def test_a_skipped_suite_cannot_be_built(self):
        with self.assertRaises(SystemExit):
            e2e.tasks(["journeys_blind14"])

    def test_each_contract_is_one_task_with_every_use(self):
        ts = e2e.tasks(["journeys", "consistency"])
        stripe = [t for t in ts.values() if t["mock"] == "stripe"]
        self.assertEqual(len(stripe), len({t["sha"] for t in stripe}))
        for t in ts.values():
            self.assertTrue(t["id"].endswith(t["sha"][:12]))
            self.assertTrue(t["uses"])

    def test_the_task_directory_holds_the_contract_and_the_prompt_only(self):
        with tempfile.TemporaryDirectory() as d:
            contract = write(os.path.join(d, "src", "acme.openapi.yaml"), "openapi: 3.0.0\ninfo:\n  title: Acme API v2\n")
            task = {"contract": contract, "mock": "acme", "docs": None}
            out = e2e.write_task(task, os.path.join(d, "task"))
            self.assertEqual(sorted(os.listdir(out)), ["PROMPT.md", "contract.openapi.yaml"])
            with open(os.path.join(out, "PROMPT.md")) as f:
                prompt = f.read()
            self.assertEqual(prompt.count("\n"), 1)
            self.assertEqual(prompt.strip(), "My app integrates with the Acme API. The contract is in "
                             "./contract.openapi.yaml. Set up a Mockingbird mock so I can develop and test the "
                             "app against it.")

    def test_docs_are_copied_and_named_in_the_prompt(self):
        with tempfile.TemporaryDirectory() as d:
            contract = write(os.path.join(d, "acme.openapi.yaml"), "openapi: 3.0.0\ninfo:\n  title: Acme\n")
            write(os.path.join(d, "acme.docs", "guide.md"), "# Guide\n")
            task = {"contract": contract, "mock": "acme", "docs": e2e.docs_dir(contract)}
            out = e2e.write_task(task, os.path.join(d, "task"))
            self.assertTrue(os.path.exists(os.path.join(out, "docs", "guide.md")))
            self.assertIn("and docs are in ./docs/", e2e.prompt_for(task))

    def test_vendor_names_drop_versions_and_add_a_missing_brand(self):
        with tempfile.TemporaryDirectory() as d:
            def vendor(title, mock):
                return e2e.vendor_of(write(os.path.join(d, f"{mock}.yaml"), f"info:\n  title: {title}\n"), mock)
            self.assertEqual(vendor("GitLab REST API v4", "gitlab"), "GitLab")
            self.assertEqual(vendor("The Plaid API", "plaid"), "Plaid")
            self.assertEqual(vendor("Twilio - API", "twilio"), "Twilio")
            self.assertEqual(vendor("Orders", "paypal"), "Paypal Orders")
            self.assertEqual(vendor("Payments", "paypalpayments"), "Paypal Payments")
            self.assertEqual(vendor("Invoicing", "invoices"), "Invoicing")
            self.assertEqual(vendor("Cal.com API v2", "calcom"), "Cal.com")
            self.assertEqual(e2e.vendor_of(write(os.path.join(d, "x.wsdl"), "<definitions/>"), "ebay"), "Ebay")


class KeyTest(unittest.TestCase):
    SURF = {"skills_sha256": "s", "mcp_sha256": "m", "binary_sha256": "b"}

    def test_a_skill_or_tool_schema_change_changes_the_key(self):
        k = e2e.cache_key(self.SURF, "sonnet", 1.0, 60)
        self.assertNotEqual(k, e2e.cache_key(dict(self.SURF, skills_sha256="s2"), "sonnet", 1.0, 60))
        self.assertNotEqual(k, e2e.cache_key(dict(self.SURF, mcp_sha256="m2"), "sonnet", 1.0, 60))
        self.assertNotEqual(k, e2e.cache_key(self.SURF, "opus", 1.0, 60))

    def test_the_binary_joins_the_key_only_when_asked(self):
        other = dict(self.SURF, binary_sha256="b2")
        self.assertEqual(e2e.cache_key(self.SURF, "sonnet", 1.0, 60), e2e.cache_key(other, "sonnet", 1.0, 60))
        self.assertNotEqual(e2e.cache_key(self.SURF, "sonnet", 1.0, 60, key_binary=True),
                            e2e.cache_key(other, "sonnet", 1.0, 60, key_binary=True))


def tool_use(i, name, inp=None):
    return json.dumps({"type": "assistant", "message": {"content": [
        {"type": "tool_use", "id": i, "name": name, "input": inp or {}}]}})


def tool_result(i, text, error=False):
    return json.dumps({"type": "user", "message": {"content": [
        {"type": "tool_result", "tool_use_id": i, "is_error": error, "content": [{"type": "text", "text": text}]}]}})


class TranscriptTest(unittest.TestCase):
    def test_calls_errors_retries_skill_reads_and_refused_entries(self):
        mb = e2e.MCP_PREFIX
        lines = [
            tool_use("1", "Read", {"file_path": "/w/.claude/skills/mockingbird-world/SKILL.md"}), tool_result("1", "# skill"),
            tool_use("2", mb + "import_contract"), tool_result("2", "not found", error=True),
            tool_use("3", mb + "import_contract"), tool_result("3", "{}"),
            tool_use("4", mb + "patch_world"),
            tool_result("4", json.dumps({"revision": 2, "refused": [{"path": "/x", "code": "bad", "message": "m"}]})),
            tool_use("5", mb + "call_mock"), tool_result("5", "{}"),
            "not json",
        ]
        m = e2e.transcript_metrics(lines)
        self.assertEqual(m["tool_calls"], 5)
        self.assertEqual(m["tools"]["import_contract"], {"calls": 2, "errors": 1})
        self.assertEqual(m["retries"], 1)
        self.assertEqual(m["tool_errors"], [{"tool": "import_contract", "error": "not found"}])
        self.assertEqual(m["skill_reads"], ["mockingbird-world/SKILL.md"])
        self.assertEqual(m["refused_in_writes"], [{"path": "/x", "code": "bad", "message": "m"}])

    def test_a_skill_loaded_through_the_skill_tool_counts_as_read(self):
        lines = [tool_use("1", "Skill", {"skill": "mockingbird:mockingbird-world"}), tool_result("1", "ok")]
        self.assertEqual(e2e.transcript_metrics(lines)["skill_reads"], ["mockingbird:mockingbird-world"])

    def test_isolation_allows_only_the_mockingbird_plugin_and_mcp_server(self):
        with tempfile.TemporaryDirectory() as plugin:
            init = {"tools": ["Read", "Grep", "Glob", "Skill", e2e.MCP_PREFIX + "call_mock"],
                    "mcp_servers": [{"name": "mockingbird"}],
                    "plugins": [{"name": "mockingbird", "path": plugin}, {"name": "x", "source": "x@builtin"}]}
            self.assertEqual(e2e.isolation_problems(init, plugin), [])
            self.assertTrue(e2e.isolation_problems(dict(init, tools=init["tools"] + ["Bash"]), plugin))
            self.assertTrue(e2e.isolation_problems(dict(init, plugins=[]), plugin))
            self.assertTrue(e2e.isolation_problems(
                dict(init, plugins=init["plugins"] + [{"name": "other", "path": "/x"}]), plugin))

    def test_a_call_after_another_tool_failed_is_not_a_retry(self):
        mb = e2e.MCP_PREFIX
        lines = [tool_use("1", mb + "get_world"), tool_result("1", "x", error=True),
                 tool_use("2", mb + "call_mock"), tool_result("2", "{}")]
        self.assertEqual(e2e.transcript_metrics(lines)["retries"], 0)


def usable_result(d, sha, name="x"):
    """A usable stage-1 task directory for contract `sha` under keydir `d`."""
    t = os.path.join(d, f"{name}-{sha[:12]}")
    write(os.path.join(t, "result.json"), json.dumps({"is_error": False, "snapshot": "snapshot"}))
    os.makedirs(os.path.join(t, "snapshot"), exist_ok=True)
    return t


def snapshot(d, mocks, contract_text="openapi: 3.0.0\n"):
    """A `snapshot export` directory: {agent mock name: {world?, scenarios?, sessions?}}."""
    for name, parts in mocks.items():
        write(os.path.join(d, "contracts", f"{name}.yaml"), parts.get("contract", contract_text))
        write(os.path.join(d, "mocks", f"{name}.json"),
              json.dumps({"name": name, "route_prefix": f"/{name}", "consistency": "strict",
                          "persona": parts.get("persona"), "contract": f"contracts/{name}.yaml"}))
        if parts.get("world"):
            write(os.path.join(d, "worlds", f"{name}.json"), json.dumps({"version": 1}))
        for s in parts.get("scenarios", []):
            write(os.path.join(d, "scenarios", name, f"{s}.json"), "{}")
        for s in parts.get("sessions", []):
            write(os.path.join(d, "sessions", name, f"{s}.json"), "[]")
    return d


class SnapshotTest(unittest.TestCase):
    def test_the_mock_whose_contract_is_the_tasks_is_chosen(self):
        with tempfile.TemporaryDirectory() as d:
            snap = snapshot(d, {"a": {"contract": "other\n"}, "b": {"contract": "mine\n"}})
            sha = world_author.sha256_file(os.path.join(snap, "contracts", "b.yaml"))
            self.assertEqual(e2e.choose_mock(snap, sha), ("b", "contract matches"))

    def test_without_a_match_the_only_mock_or_the_first_with_a_world_is_chosen(self):
        with tempfile.TemporaryDirectory() as d:
            self.assertEqual(e2e.choose_mock(snapshot(os.path.join(d, "1"), {"a": {}}), "0" * 64), ("a", "only mock"))
            pick, why = e2e.choose_mock(snapshot(os.path.join(d, "2"), {"a": {}, "b": {"world": True}}), "0" * 64)
            self.assertEqual(pick, "b")
            self.assertIn("with a world", why)
            self.assertEqual(e2e.choose_mock(os.path.join(d, "empty"), "0" * 64), (None, "no mock built"))

    def test_the_rewrite_renames_the_mock_to_what_the_journeys_call(self):
        with tempfile.TemporaryDirectory() as d:
            snap = snapshot(os.path.join(d, "s"), {"my-acme": {"world": True, "scenarios": ["happy"],
                                                               "sessions": ["default"], "persona": "agent's"},
                                                   "other": {}})
            out = e2e.rewrite(snap, "my-acme", "acme", "/acme", os.path.join(d, "out"),
                              {"faker_seed": 1, "llm_profile": "bench_acme"})
            self.assertEqual(os.listdir(os.path.join(out, "mocks")), ["acme.json"])
            with open(os.path.join(out, "mocks", "acme.json")) as f:
                m = json.load(f)
            # The agent's persona stays: the suite's would hand the mock facts the journeys check.
            self.assertEqual((m["name"], m["route_prefix"], m["persona"], m["consistency"]),
                             ("acme", "/acme", "agent's", "strict"))
            self.assertEqual((m["faker_seed"], m["llm_profile"]), (1, "bench_acme"))
            self.assertEqual(m["contract"], "contracts/acme.yaml")
            self.assertTrue(os.path.exists(os.path.join(out, m["contract"])))
            self.assertTrue(os.path.exists(os.path.join(out, "worlds", "acme.json")))
            self.assertEqual(os.listdir(os.path.join(out, "scenarios", "acme")), ["happy.json"])
            self.assertEqual(os.listdir(os.path.join(out, "sessions", "acme")), ["default.json"])

    def test_the_result_of_a_contract_is_found_by_its_hash(self):
        with tempfile.TemporaryDirectory() as d:
            sha = "ab" * 32
            write(os.path.join(d, f"acme-{sha[:12]}", "result.json"), "{}")
            self.assertEqual(e2e.find_result(d, sha), os.path.join(d, f"acme-{sha[:12]}"))
            self.assertIsNone(e2e.find_result(d, "cd" * 32))

    def test_only_an_error_free_build_with_a_snapshot_is_reused(self):
        with tempfile.TemporaryDirectory() as d:
            t = usable_result(d, "ab" * 32)
            self.assertTrue(e2e.cached_result(t))
            for bad in ({"is_error": True, "snapshot": "snapshot", "error": "isolation: x"},
                        {"is_error": True, "snapshot": "snapshot", "error": "timed out"},
                        {"is_error": False, "snapshot": None}):
                write(os.path.join(t, "result.json"), json.dumps(bad))
                self.assertIsNone(e2e.cached_result(t))
            # A session the caps ended is not an error: what it built counts.
            write(os.path.join(t, "result.json"), json.dumps({"is_error": False, "capped": True,
                                                               "subtype": "error_max_turns", "snapshot": "snapshot"}))
            self.assertTrue(e2e.cached_result(t))


class FakeSrv:
    admin = "http://127.0.0.1:9"
    api = admin + "/api/v1/workspaces/default"


def fake_binary(d, code):
    """An executable standing in for `mockingbird snapshot import`: prints a summary, exits `code`."""
    path = write(os.path.join(d, "mb"), "#!/bin/sh\necho '{\"created\": [\"x\"], \"refused_world_entries\": "
                 "{\"travel\": [{\"path\": \"/p\"}]}}'\nexit %d\n" % code)
    os.chmod(path, 0o755)
    return path


class LoadTest(unittest.TestCase):
    MOCKS = {"travel": ("docs/benchmarks/contracts/travel.openapi.yaml", "/travel", None)}

    def load(self, keydir, binary, out):
        return e2e.load_snapshots(FakeSrv(), binary, keydir, "s", self.MOCKS, out)[0]

    def test_every_outcome_is_recorded_and_none_aborts_the_suite(self):
        sha = world_author.sha256_file(os.path.join(world_author.REPO, self.MOCKS["travel"][0]))
        with tempfile.TemporaryDirectory() as d:
            keydir, out = os.path.join(d, "k"), os.path.join(d, "out")
            os.makedirs(out)
            self.assertEqual(self.load(keydir, "/bin/false", out)["status"], "no_snapshot")
            t = usable_result(keydir, sha)
            write(os.path.join(t, "result.json"), json.dumps({"is_error": True, "snapshot": "snapshot"}))
            self.assertEqual(self.load(keydir, "/bin/false", out)["status"], "build_error")
            usable_result(keydir, sha)
            self.assertEqual(self.load(keydir, "/bin/false", out)["status"], "nothing_built")
            snapshot(os.path.join(t, "snapshot"), {"travel": {"world": True}})
            failed = self.load(keydir, fake_binary(d, 1), out)
            self.assertEqual(failed["status"], "import_failed")
            ok = self.load(keydir, fake_binary(d, 0), out)
            self.assertEqual((ok["status"], ok["agent_mock"], ok["refused_n"]), ("imported", "travel", 1))
            loads = e2e.load_statuses(out)
            self.assertEqual(sorted(loads), ["build_error", "import_failed", "imported", "no_snapshot",
                                             "nothing_built"])


class BuildTest(unittest.TestCase):
    def args(self, cache, **kw):
        a = dict(binary=sys.executable, model="m", budget=1.0, max_turns=60, key_binary=False, cache=cache,
                 suite=["consistency"], task=None, jobs=1, cap=2.5, timeout=1, refresh=False, claude="claude")
        a.update(kw)
        return type("A", (), a)()

    def run_build(self, args, results):
        calls = []

        def fake_run(task, a, keydir, surf):
            calls.append(task["id"])
            return dict(results.pop(0), task=task["id"])
        real = (e2e.run_task, e2e.surface)
        e2e.run_task = fake_run
        e2e.surface = lambda b: {"skills_sha256": "s", "mcp_sha256": "m", "binary_sha256": "b", "tools": []}
        try:
            return e2e.build(args), calls
        finally:
            e2e.run_task, e2e.surface = real

    def test_a_run_with_no_reported_cost_counts_its_whole_budget(self):
        with tempfile.TemporaryDirectory() as d:
            # $2.5 cap: the first task reports nothing (a timeout) and counts $1, the second $1.2;
            # the third would need another $1 of room and is skipped.
            code, calls = self.run_build(self.args(d), [{"is_error": True}, {"cost_usd": 1.2}, {"cost_usd": 0.1}])
            self.assertEqual(code, 3)
            self.assertEqual(len(calls), 2)

    def test_cached_builds_are_reused_whatever_the_cap(self):
        with tempfile.TemporaryDirectory() as d:
            key = e2e.cache_key({"skills_sha256": "s", "mcp_sha256": "m", "binary_sha256": "b"}, "m", 1.0, 60)
            for t in e2e.tasks(["consistency"]).values():
                usable_result(os.path.join(d, key), t["sha"], t["mock"])
            code, calls = self.run_build(self.args(d, cap=0), [])
            self.assertEqual((code, calls), (0, []))

    def test_the_cache_must_be_outside_the_repository(self):
        with self.assertRaises(SystemExit):
            self.run_build(self.args(os.path.join(world_author.REPO, "tmp-e2e-cache")), [])


class IsolationInputTest(unittest.TestCase):
    def test_paths_outside_the_task_and_urls_off_this_machine_are_problems(self):
        with tempfile.TemporaryDirectory() as work, tempfile.TemporaryDirectory() as plugin:
            mb = e2e.MCP_PREFIX
            ok = [tool_use("1", mb + "import_contract", {"mock": "a", "source": "./contract.openapi.yaml"}),
                  tool_use("2", mb + "import_contract", {"source": os.path.join(work, "contract.openapi.yaml")}),
                  tool_use("3", mb + "call_mock", {"method": "GET", "path": "/a/v1/things"}),
                  tool_use("4", mb + "admin_api", {"method": "GET", "path": "/workspaces/default/mocks"}),
                  tool_use("5", mb + "call_mock", {"url": "http://127.0.0.1:4010/a/v1/x"})]
            self.assertEqual(e2e.input_problems(ok, [work, plugin]), [])
            for bad in ({"source": "/etc/passwd"}, {"source": "../../scripts/llm-bench/server_suites.py"},
                        {"source": "https://example.com/openapi.yaml"}, {"source": "file:///tmp/x"}):
                self.assertTrue(e2e.input_problems([tool_use("9", mb + "import_contract", bad)], [work, plugin]),
                                bad)


class ReportTest(unittest.TestCase):
    ROWS = {("e2e", "s", "pass"): {"ok": True, "source": "state"},
            ("e2e", "s", "engine"): {"ok": False, "source": "state"},
            ("e2e", "s", "refused"): {"ok": False, "source": "rejected"},
            ("e2e", "s", "model"): {"ok": False, "source": "llm"},
            ("e2e", "s", "never"): {"ok": False, "source": "fallback"},
            ("e2e", "s", "unmocked"): {"ok": False, "source": "?"},
            ("e2e", "s", "error"): {"ok": False, "source": "error"}}

    def test_engine_decided_failures_are_floor(self):
        s = e2e.summarize(self.ROWS)[("e2e", "s")]
        self.assertEqual((s["passed"], s["total"]), (1, 7))
        # A failure that never reached the model (no mock, an error) is floor too.
        self.assertEqual(sorted(s["floor"]), ["engine", "error", "refused", "unmocked"])
        self.assertEqual(sorted(s["model_dependent"]), ["model", "never"])
        self.assertEqual(s["projected"], round(100 * 3 / 7, 1))

    def test_with_paid_runs_a_model_failure_needs_a_paid_pass(self):
        s = e2e.summarize(self.ROWS, paid={("s", "model")})[("e2e", "s")]
        self.assertEqual(s["model_dependent"], ["model"])
        self.assertEqual(sorted(s["floor"]), ["engine", "error", "never", "refused", "unmocked"])


class FakegateE2eTest(unittest.TestCase):
    def test_a_tier_neither_side_ran_is_left_out(self):
        base = {("none", "s", "a"): {"ok": True}}
        r = fakegate.compare(base, dict(base))
        self.assertEqual(list(r), ["none"])

    def test_the_e2e_tier_is_compared_like_the_others(self):
        base = {("e2e", "s", "a"): {"ok": True, "source": "state"}, ("e2e", "s", "b"): {"ok": True}}
        cand = {("e2e", "s", "a"): {"ok": False, "source": "state"}, ("e2e", "s", "b"): {"ok": False}}
        r = fakegate.compare(base, cand, noise=1)["e2e"]
        self.assertTrue(r["fails"])
        self.assertEqual(r["exec_new"], [("e2e", "s", "a")])

    def test_a_side_without_e2e_rows_for_a_covered_suite_is_incomplete(self):
        meta = {"authors": {}, "inputs": {"s": {}}, "e2e": {"suites": ["s"]}}
        self.assertEqual(fakegate.incomplete({("none", "s", "a"): {}}, meta), [("e2e", "s")])
        self.assertEqual(fakegate.incomplete({("none", "s", "a"): {}, ("e2e", "s", "a"): {}}, meta), [])

    def test_e2e_meta_covers_a_suite_only_when_every_contract_has_a_snapshot(self):
        tree = fakegate.REPO
        contracts = fakegate.suite_contracts(tree, "consistency")
        with tempfile.TemporaryDirectory() as d:
            for c in contracts[:-1]:
                usable_result(d, world_author.sha256_file(c))
            meta = fakegate.e2e_meta(tree, ["consistency"], d)
            self.assertEqual((meta["suites"], meta["uncovered"]), ([], ["consistency"]))
            last = usable_result(d, world_author.sha256_file(contracts[-1]))
            write(os.path.join(last, "result.json"), json.dumps({"is_error": True, "snapshot": "snapshot"}))
            self.assertEqual(fakegate.e2e_meta(tree, ["consistency"], d)["suites"], [])
            usable_result(d, world_author.sha256_file(contracts[-1]))
            self.assertEqual(fakegate.e2e_meta(tree, ["consistency"], d)["suites"], ["consistency"])

    def test_a_checkout_without_e2e_py_runs_no_e2e_tier(self):
        with tempfile.TemporaryDirectory() as tree:
            meta = fakegate.e2e_meta(tree, ["consistency"], tree)
            self.assertEqual((meta["suites"], meta["uncovered"]), ([], ["consistency"]))
            self.assertIn("no e2e.py", meta["missing"])


class ServerConfigTest(unittest.TestCase):
    def test_e2e_mocks_get_their_tagged_profiles_without_being_configured(self):
        import server_suites
        with tempfile.TemporaryDirectory() as d:
            path = server_suites.write_config(d, "m", "http://p", (1, 2, 3), {}, "s", tag_journeys=True,
                                              profile_names=["acme"])
            with open(path) as f:
                text = f.read()
            self.assertIn("bench_acme:", text)
            self.assertIn("x-bench-journey: \"acme\"", text)
            self.assertNotIn("mocks:", text)


if __name__ == "__main__":
    unittest.main()
