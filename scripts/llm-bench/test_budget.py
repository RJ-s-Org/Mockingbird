"""Tests for the bench's own budget: a run that hits it stops and is marked invalid,
aggregate.py keeps its rows out of the scores, the proxy's in-flight reservation
follows measured cost, and a cap too small for the concurrency is warned about.
Offline. Run: python3 -m unittest scripts/llm-bench/test_budget.py"""

import argparse
import contextlib
import io
import json
import os
import shutil
import sys
import tempfile
import threading
import unittest
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from unittest import mock

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))

import aggregate  # noqa: E402
import proxy  # noqa: E402
import server_suites  # noqa: E402

# What Mockingbird puts in x-mockingbird-fallback-reason when the bench proxy refuses a call.
BUDGET_REASON = 'LLM returned HTTP 402: {"error": {"message": "%s", "code": 402}}' % proxy.BUDGET_EXHAUSTED_MESSAGE


def serve(handler, test):
    httpd = ThreadingHTTPServer(("127.0.0.1", 0), handler)
    threading.Thread(target=httpd.serve_forever, daemon=True).start()
    test.addCleanup(httpd.server_close)
    test.addCleanup(httpd.shutdown)

    class Srv:
        data = f"http://127.0.0.1:{httpd.server_address[1]}"

    return Srv


class FakeMock(BaseHTTPRequestHandler):
    """`/<mock>/ok` answers from the LLM; `/<mock>/budget` is the faker answer Mockingbird
    sends after the bench proxy refused the call on its budget. Records each path."""

    calls = []

    def log_message(self, *a):
        pass

    def do_GET(self):
        kind = self.path.split("?")[0].rstrip("/").split("/")[-1]
        FakeMock.calls.append(kind)
        raw = b'{"id": "x"}'
        self.send_response(200)
        self.send_header("content-type", "application/json")
        if kind == "budget":
            self.send_header("x-mockingbird-source", "fallback")
            self.send_header("x-mockingbird-fallback-reason", BUDGET_REASON)
        else:
            self.send_header("x-mockingbird-source", "llm")
        self.send_header("content-length", str(len(raw)))
        self.end_headers()
        self.wfile.write(raw)


def steps_of(*kinds):
    def steps(j):
        for i, kind in enumerate(kinds):
            j.call("GET", f"/{kind}")
            j.check(f"check {i}", True)
    return steps


class Reset(unittest.TestCase):
    def setUp(self):
        server_suites.reset_budget_state()
        self.addCleanup(server_suites.reset_budget_state)
        FakeMock.calls = []
        self.out = tempfile.mkdtemp()
        self.addCleanup(shutil.rmtree, self.out, True)

    def written(self, name="server.jsonl"):
        path = os.path.join(self.out, name)
        if not os.path.exists(path):
            return []
        with open(path) as f:
            return [json.loads(line) for line in f if line.strip()]


class BudgetReasonTest(unittest.TestCase):
    def test_matches_the_proxy_refusal_only(self):
        for reason in (
            BUDGET_REASON,
            'LLM returned HTTP 402: {"error":{"message":"bench budget exhausted","code":402}}',
            # A repair call refused after a bad first answer: still the bench's budget.
            "LLM output truncated: stopped; not salvaged: " + BUDGET_REASON,
        ):
            self.assertTrue(proxy.budget_exhausted(reason), reason)
        for reason in (
            None, "", "bench budget exhausted",
            "LLM returned HTTP 402: Payment Required",
            'LLM returned HTTP 402: {"error":{"message":"Insufficient credits","code":402}}',
            'LLM returned HTTP 503: {"error":{"message":"bench budget exhausted"}}',
            'LLM returned HTTP 402: {"error":{"message":"bench budget exhausted soon"}}',
        ):
            self.assertFalse(proxy.budget_exhausted(reason), reason)

    def test_the_proxy_answers_with_the_shared_message(self):
        self.assertEqual(proxy.budget_refusal(), {"error": {"message": proxy.BUDGET_EXHAUSTED_MESSAGE, "code": 402}})
        self.assertTrue(proxy.budget_exhausted("LLM returned HTTP 402: " + json.dumps(proxy.budget_refusal())))


class JourneyStopsOnBudgetTest(Reset):
    class Args:
        model, timeout, repeat = "m/x", 10, 1

    def test_a_budget_fallback_stops_the_journey_and_writes_no_row_for_it(self):
        srv = serve(FakeMock, self)
        sink = server_suites.Sink(self.out, "m/x", "journeys")
        with mock.patch.dict(server_suites.JOURNEY_CHECKS, {"fake": ["check 0", "check 1", "check 2"]}):
            with self.assertRaises(server_suites.Stopped):
                server_suites.run_one_journey("fake", steps_of("ok", "budget", "ok"), srv, sink, self.Args, 0)
        self.assertEqual(FakeMock.calls, ["ok", "budget"])  # nothing after the refused call
        self.assertEqual([r["case"] for r in self.written()], ["fake.check 0"])
        self.assertTrue(server_suites.STOP.is_set())
        self.assertTrue(server_suites.budget_exhausted())
        # Any row another unit tries to add afterwards is dropped.
        sink.add("late", 0, False, 1.0, "fallback", {})
        self.assertEqual(len(self.written()), 1)

    def test_run_suite_marks_the_run_invalid(self):
        srv = serve(FakeMock, self)

        def runner(srv_, sink, args):
            units = [lambda i=i: server_suites.run_one_journey(
                "fake", steps_of(*(["ok", "budget"] if i == 1 else ["ok", "ok"])), srv_, sink, args, i)
                for i in range(2)]
            server_suites.run_units(units, args)

        @contextlib.contextmanager
        def fake_server(args, suite, mocks):
            yield srv

        args = argparse.Namespace(model="m/x", timeout=10, repeat=1, jobs=1, out=self.out, world="none",
                                  proxy_base="http://127.0.0.1:9")
        err = io.StringIO()
        stats = {"spent_usd": 0.4012, "budget_usd": 0.55}
        with mock.patch.dict(server_suites.JOURNEY_CHECKS, {"fake": ["check 0", "check 1"]}), \
                mock.patch.dict(server_suites.RUNNERS, {"journeys": ({}, runner)}), \
                mock.patch.object(server_suites, "Server", fake_server), \
                mock.patch.object(server_suites, "proxy_stats", lambda a: stats), \
                contextlib.redirect_stderr(err):
            failed = server_suites.run_suite("journeys", args)
        self.assertTrue(failed)
        rows = self.written()
        self.assertEqual([(r["case"], r["rep"]) for r in rows],
                         [("fake.check 0", 0), ("fake.check 1", 0), ("fake.check 0", 1)])
        self.assertNotIn("suite_error", [r["case"] for r in rows])
        [inv] = self.written("invalid.jsonl")
        self.assertEqual(inv["reason"], "bench budget exhausted")
        self.assertEqual((inv["model"], inv["suite"], inv["rows_written"]), ("m/x", "journeys", 3))
        self.assertEqual(inv["run_id"], rows[0]["run_id"])
        self.assertEqual((inv["spent_usd"], inv["budget_usd"]), (0.4012, 0.55))
        self.assertIn("402", inv["fallback_reason"])
        self.assertIn("journeys: INVALID: bench budget exhausted (spent $0.4012 of $0.55 cap)", err.getvalue())
        self.assertEqual(server_suites.exit_code(False), server_suites.EXIT_BUDGET)
        self.assertNotEqual(server_suites.EXIT_BUDGET, 0)

    def test_a_clean_run_is_not_invalid(self):
        self.assertFalse(server_suites.budget_exhausted())
        self.assertEqual(server_suites.exit_code(False), 0)
        self.assertEqual(server_suites.exit_code(True), 1)


class AggregateInvalidTest(Reset):
    def rows(self, run_id, ok, n=3, suite="journeys"):
        return [{"model": "m/x", "suite": suite, "case": f"fake.c{i}", "rep": 0, "ok": ok, "latency_ms": 1.0,
                 "source": "llm", "detail": {}, "run_id": run_id} for i in range(n)]

    def test_rows_of_an_invalid_run_are_left_out_and_labelled(self):
        rows = self.rows("good", True) + self.rows("bad", False)
        with open(os.path.join(self.out, "server.jsonl"), "w") as f:
            f.writelines(json.dumps(r) + "\n" for r in rows)
        inv = {"ts": "t", "reason": "bench budget exhausted", "run_id": "bad", "model": "m/x", "suite": "journeys",
               "world": "none", "rows_written": 3, "spent_usd": 0.4, "budget_usd": 0.55}
        os.makedirs(os.path.join(self.out, "server", "m__x"))
        with open(os.path.join(self.out, "server", "m__x", "invalid.jsonl"), "w") as f:
            f.write(json.dumps(inv) + "\n")
        err = io.StringIO()
        with contextlib.redirect_stderr(err):
            self.assertEqual(aggregate.main(["--out", self.out]), 0)
        with open(os.path.join(self.out, "results.json")) as f:
            results = json.load(f)
        self.assertEqual(results["invalid_runs"]["rows_excluded"], 3)
        self.assertEqual([r["run_id"] for r in results["invalid_runs"]["runs"]], ["bad"])
        block = results["models"]["m/x"]["server"]["journeys"]["journeys"]
        self.assertEqual((block["n"], block["ok"]), (3, 3))
        self.assertIn("INVALID", err.getvalue())
        with open(os.path.join(self.out, "tables.md")) as f:
            self.assertIn("bench budget exhausted", f.read())

    def test_without_invalid_runs_nothing_changes(self):
        with open(os.path.join(self.out, "server.jsonl"), "w") as f:
            f.writelines(json.dumps(r) + "\n" for r in self.rows("good", True))
        with contextlib.redirect_stderr(io.StringIO()):
            aggregate.main(["--out", self.out])
        with open(os.path.join(self.out, "results.json")) as f:
            results = json.load(f)
        self.assertEqual(results["invalid_runs"], {"runs": [], "rows_excluded": 0})


class MeasuredReservationTest(unittest.TestCase):
    BODY = {"model": "m/x", "max_tokens": 16384, "messages": [{"role": "user", "content": "hi"}]}

    def make(self):
        d = tempfile.mkdtemp()
        self.addCleanup(shutil.rmtree, d, True)
        args = argparse.Namespace(upstream="http://127.0.0.1:9/api/v1/chat/completions", no_stream_upgrade=True,
                                  dump=None, ledger=os.path.join(d, "calls.jsonl"), budget_usd=5.0, prior_ledger=[],
                                  prices=None, provider_pins="", no_format_fallback=True)
        with mock.patch.dict(os.environ, {}, clear=False):
            os.environ.pop("OPENROUTER_API_KEY", None)
            px = proxy.Proxy(args)
        self.addCleanup(px.ledger.fh.close)
        px.prices = {"m/x": (1e-6, 4e-6)}
        return px

    def settle(self, px, cost, model="m/x", status=200, error_kind=None):
        px.ledger.settle(0.0, {"label": "m__x~journeys", "model_slug": "m__x", "model_requested": model,
                               "status": status, "error_kind": error_kind, "cost_usd": cost}, reserved=False)

    def test_worst_case_until_enough_calls_then_measured(self):
        px = self.make()
        worst = px.estimate("m/x", self.BODY)
        self.assertAlmostEqual(worst, 16384 * 4e-6 + len(json.dumps(self.BODY["messages"])) / 3.0 * 1e-6)
        for _ in range(proxy.MEASURED_MIN_CALLS - 1):
            self.settle(px, 0.001)
        self.settle(px, 0.5, status=502, error_kind="stream_error")  # errors are not measured
        self.settle(px, 0.002, model="m/other")  # nor other models
        self.assertEqual(px.estimate("m/x", self.BODY), worst)
        self.settle(px, 0.003)
        measured = px.estimate("m/x", self.BODY)
        costs = sorted([0.001] * (proxy.MEASURED_MIN_CALLS - 1) + [0.003])
        self.assertAlmostEqual(measured, proxy.percentile(costs, proxy.MEASURED_PERCENTILE) * proxy.MEASURED_SAFETY)
        self.assertLess(measured, worst / 10)
        # A variant of the model shares its measurements; the worst case still caps it.
        self.assertEqual(px.estimate("m/x@nojson", self.BODY), measured)
        small = dict(self.BODY, max_tokens=1)
        self.assertEqual(px.estimate("m/x", small), px.worst_case("m/x", small)[1])

    def test_a_resumed_ledger_counts(self):
        px = self.make()
        for _ in range(proxy.MEASURED_MIN_CALLS):
            self.settle(px, 0.001)
        px.ledger.fh.close()
        again = proxy.Ledger(px.ledger.path, 5.0, [])
        self.addCleanup(again.fh.close)
        self.assertEqual(again.measured_reserve("m/x"), (0.001 * proxy.MEASURED_SAFETY, proxy.MEASURED_MIN_CALLS))

    def test_reserve_endpoint(self):
        px = self.make()
        handler = type("H", (proxy.Handler,), {"proxy": px})
        srv = serve(handler, self)
        import urllib.request
        with urllib.request.urlopen(f"{srv.data}/reserve?model=m/x&max_tokens=1000") as r:
            got = json.load(r)
        self.assertEqual(got["basis"], "worst_case")
        self.assertAlmostEqual(got["reserve_usd"], 1000 * 4e-6, places=5)
        self.assertEqual((got["budget_usd"], got["spent_usd"]), (5.0, 0.0))


class HeadroomWarningTest(unittest.TestCase):
    def test_a_low_cap_warns_with_the_numbers(self):
        msg = server_suites.headroom_warning(budget=0.55, spent=0.0, reserved=0.0, per_call=0.066,
                                             in_flight=15, basis="worst_case")
        self.assertIsNotNone(msg)
        for part in ("$0.55", "15", "$0.0660", "$0.99"):
            self.assertIn(part, msg)

    def test_a_generous_cap_is_quiet(self):
        self.assertIsNone(server_suites.headroom_warning(budget=9.0, spent=0.5, reserved=0.0, per_call=0.066,
                                                         in_flight=15, basis="worst_case"))

    def test_spend_and_other_reservations_eat_headroom(self):
        self.assertIsNotNone(server_suites.headroom_warning(budget=9.0, spent=7.0, reserved=0.5, per_call=0.066,
                                                            in_flight=15, basis="measured"))

    def test_in_flight_follows_the_flags(self):
        a = argparse.Namespace(jobs=5, suites_parallel=True)
        self.assertEqual(server_suites.calls_in_flight(a, ["journeys", "journeys_blind"]), 10)
        a.suites_parallel = False
        self.assertEqual(server_suites.calls_in_flight(a, ["journeys", "journeys_blind"]), 5)

    def test_warn_queries_the_proxy_and_prints(self):
        args = argparse.Namespace(jobs=5, suites_parallel=True, model="m/x", proxy_base="http://127.0.0.1:9")
        answer = {"reserve_usd": 0.066, "basis": "worst_case", "budget_usd": 0.55, "spent_usd": 0.0,
                  "reserved_usd": 0.0}
        err = io.StringIO()
        with mock.patch.object(server_suites, "proxy_get", lambda a, path: answer), contextlib.redirect_stderr(err):
            server_suites.warn_headroom(args, ["journeys", "journeys_blind", "journeys_blind2"])
        self.assertIn("WARNING", err.getvalue())
        err = io.StringIO()
        with mock.patch.object(server_suites, "proxy_get", lambda a, path: None), contextlib.redirect_stderr(err):
            server_suites.warn_headroom(args, ["journeys"])
        self.assertEqual(err.getvalue(), "")


if __name__ == "__main__":
    unittest.main()
