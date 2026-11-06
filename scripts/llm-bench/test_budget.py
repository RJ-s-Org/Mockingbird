"""Tests for the bench's own budget: a run that hits it stops and is marked invalid,
aggregate.py keeps its rows out of the scores, the proxy's in-flight reservation
follows measured cost, and a cap too small for the concurrency is warned about.
Offline. Run: python3 -m unittest scripts/llm-bench/test_budget.py"""

import argparse
import contextlib
import functools
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


def stats_with(label, refused, **extra):
    """A proxy /stats answer whose `label` has `refused` budget refusals."""
    return dict({"spent_usd": 0.5, "budget_usd": 0.55, "per_label": {label: {"calls": 3, "budget_refused": refused}}},
                **extra)


class SseHandler(BaseHTTPRequestHandler):
    """A Mockingbird stand-in for the SSE suite: `/events/ticks` streams valid ticks
    with no fallback header (the SSE handler sends its headers before generating),
    and the request log reports `fallback_reason` as `SseHandler.logged_reason`."""

    logged_reason = None

    def log_message(self, *a):
        pass

    def do_GET(self):
        if self.path.startswith("/mocks/events/requests"):
            raw = json.dumps([{"source": "fallback" if SseHandler.logged_reason else "llm", "prompt_tokens": 1,
                               "completion_tokens": 1, "fallback_reason": SseHandler.logged_reason}]).encode()
            self.send_response(200)
            self.send_header("content-type", "application/json")
            self.send_header("content-length", str(len(raw)))
            self.end_headers()
            self.wfile.write(raw)
            return
        want = int(self.headers.get("x-mockingbird-stream-items") or 1)
        body = "".join(f"event: tick\ndata: {json.dumps({'id': 'a', 'seq': i, 'price': 1.0})}\n\n" for i in range(want))
        raw = body.encode()
        self.send_response(200)
        self.send_header("content-type", "text/event-stream")
        self.send_header("content-length", str(len(raw)))
        self.end_headers()
        self.wfile.write(raw)


class SseBudgetBackstopTest(Reset):
    """The SSE handler never reports a fallback reason, so a budget refusal there is
    only visible in the proxy's /stats for the suite's label."""

    def args(self):
        return argparse.Namespace(model="m/x", timeout=10, repeat=1, jobs=1, out=self.out, world="none",
                                  stream_items=2, proxy_base="http://127.0.0.1:9")

    def run_sse_suite(self, stats_seq):
        SseHandler.logged_reason = getattr(self, "logged", None)
        srv = serve(SseHandler, self)
        srv.api = srv.data

        @contextlib.contextmanager
        def fake_server(args, suite, mocks):
            yield srv

        answers = iter(stats_seq)
        err = io.StringIO()
        with mock.patch.dict(server_suites.RUNNERS, {"sse": ({}, server_suites.run_sse)}), \
                mock.patch.object(server_suites, "Server", fake_server), \
                mock.patch.object(server_suites, "proxy_stats", lambda a: next(answers)), \
                contextlib.redirect_stderr(err):
            failed = server_suites.run_suite("sse", self.args())
        return failed, err.getvalue()

    def test_a_refusal_in_the_proxy_stats_marks_the_sse_suite_invalid(self):
        label = "m__x~sse"
        failed, err = self.run_sse_suite([stats_with(label, 2), stats_with(label, 3), stats_with(label, 3)])
        self.assertTrue(failed)
        self.assertTrue(server_suites.budget_exhausted())
        self.assertTrue(server_suites.STOP.is_set())
        [inv] = self.written("invalid.jsonl")
        self.assertEqual((inv["suite"], inv["model"]), ("sse", "m/x"))
        self.assertIn("1 call(s)", inv["fallback_reason"])
        self.assertIn("sse: INVALID: bench budget exhausted", err)
        self.assertEqual(server_suites.exit_code(False), server_suites.EXIT_BUDGET)
        # The rep's row went out before the backstop: aggregate leaves it out by the record.
        [row] = self.written()
        kept, dropped = aggregate.drop_invalid_runs([row], [inv])
        self.assertEqual((kept, dropped), ([], 1))

    def test_refusals_from_before_the_suite_do_not_count(self):
        label = "m__x~sse"
        failed, err = self.run_sse_suite([stats_with(label, 2), stats_with(label, 2), stats_with(label, 2)])
        self.assertFalse(failed)
        self.assertFalse(server_suites.budget_exhausted())
        self.assertEqual(self.written("invalid.jsonl"), [])
        self.assertEqual(len(self.written()), 1)

    def test_no_baseline_no_verdict(self):
        failed, _ = self.run_sse_suite([None, stats_with("m__x~sse", 1), stats_with("m__x~sse", 1)])
        self.assertFalse(failed)
        self.assertEqual(self.written("invalid.jsonl"), [])

    def test_a_logged_budget_fallback_reason_stops_the_rep(self):
        self.logged = BUDGET_REASON
        failed, _ = self.run_sse_suite([stats_with("m__x~sse", 0)] * 3)
        self.assertTrue(failed)
        self.assertEqual(self.written(), [])  # the refused answer writes no row
        [inv] = self.written("invalid.jsonl")
        self.assertIn("402", inv["fallback_reason"])

    def test_budget_refusals_reads_the_label(self):
        self.assertEqual(server_suites.budget_refusals(stats_with("a~b", 4), "a~b"), 4)
        self.assertEqual(server_suites.budget_refusals(stats_with("a~b", 4), "a~c"), 0)
        self.assertIsNone(server_suites.budget_refusals(None, "a~b"))
        self.assertIsNone(server_suites.budget_refusals({"per_label": {"a~b": {"calls": 1}}}, "a~b"))


class InvalidSuiteScopeTest(Reset):
    def row(self, suite, run_id="r1", world=None):
        r = {"model": "m/x", "suite": suite, "case": "c", "rep": 0, "ok": True, "latency_ms": 1.0,
             "source": "llm", "detail": {}, "run_id": run_id}
        if world:
            r["world"] = world
        return r

    def test_only_the_stopped_suite_of_a_run_is_left_out(self):
        rows = [self.row("consistency"), self.row("consistency"), self.row("sse"), self.row("sse", run_id="r2"),
                self.row("sse", world="agent")]
        inv = [{"run_id": "r1", "model": "m/x", "suite": "sse", "world": "none"}]
        kept, dropped = aggregate.drop_invalid_runs(rows, inv)
        self.assertEqual(dropped, 1)
        self.assertEqual([(r["suite"], r["run_id"], r.get("world")) for r in kept],
                         [("consistency", "r1", None), ("consistency", "r1", None), ("sse", "r2", None),
                          ("sse", "r1", "agent")])

    def test_two_suites_in_one_process_one_stopped(self):
        srv = serve(FakeMock, self)

        def journey_runner(kinds):
            def runner(srv_, sink, args):
                server_suites.run_one_journey("fake", steps_of(*kinds), srv_, sink, args, 0)
            return runner

        @contextlib.contextmanager
        def fake_server(args, suite, mocks):
            yield srv

        args = argparse.Namespace(model="m/x", timeout=10, repeat=1, jobs=1, out=self.out, world="none",
                                  proxy_base="http://127.0.0.1:9", suites_parallel=False)
        with mock.patch.dict(server_suites.JOURNEY_CHECKS, {"fake": ["check 0", "check 1"]}), \
                mock.patch.dict(server_suites.RUNNERS, {"journeys": ({}, journey_runner(["ok", "ok"])),
                                                        "journeys_blind": ({}, journey_runner(["ok", "budget"])),
                                                        "sse": ({}, journey_runner(["ok", "ok"]))}), \
                mock.patch.object(server_suites, "Server", fake_server), \
                mock.patch.object(server_suites, "proxy_stats", lambda a: None), \
                contextlib.redirect_stderr(io.StringIO()):
            server_suites.run_suites(["journeys", "journeys_blind", "sse"], args)
        rows = self.written()
        self.assertEqual([r["suite"] for r in rows], ["journeys", "journeys", "journeys_blind"])
        self.assertEqual(len({r["run_id"] for r in rows}), 1)  # one process, one run_id
        invalid = self.written("invalid.jsonl")
        self.assertEqual([r["suite"] for r in invalid], ["journeys_blind"])  # sse never started
        kept, dropped = aggregate.drop_invalid_runs(rows, invalid)
        self.assertEqual(([r["suite"] for r in kept], dropped), (["journeys", "journeys"], 1))


class ParallelStopTest(Reset):
    """--suites-parallel with --jobs > 1: a budget refusal in one suite stops every
    suite still running; a suite that finished before it stays valid."""

    def test_parallel_suites_and_units_stop_together(self):
        # Events order every step; SAFETY only keeps a broken run from hanging forever,
        # it never decides the order.
        SAFETY = 60
        srv = serve(FakeMock, self)
        clean_done, victim_started = threading.Event(), threading.Event()

        def clean(srv_, sink, args):
            units = [lambda i=i: server_suites.run_one_journey("fake", steps_of("ok", "ok"), srv_, sink, args, i)
                     for i in range(2)]
            server_suites.run_units(units, args)

        # The clean suite counts as done only once run_suite has returned for it, after
        # it took its own view of the stop: setting the event inside its runner left a
        # window in which the budget suite could trip before that snapshot.
        real_run_suite = server_suites.run_suite

        wrapped = []

        def run_suite(suite, args_):
            wrapped.append(suite)
            try:
                return real_run_suite(suite, args_)
            finally:
                if suite == "journeys":
                    clean_done.set()

        def budget(srv_, sink, args):
            def unit(i):
                if i == 1:
                    victim_started.wait(SAFETY)
                    server_suites.run_one_journey("fake", steps_of("ok", "budget"), srv_, sink, args, i)
                else:
                    server_suites.run_one_journey("fake", steps_of("ok", "ok"), srv_, sink, args, i)
            clean_done.wait(SAFETY)
            server_suites.run_units([functools.partial(unit, i) for i in range(2)], args)

        def victim(srv_, sink, args):
            def unit(i):
                def steps(j):
                    j.call("GET", "/ok")
                    j.check("check 0", True)
                    victim_started.set()
                    server_suites.STOP.wait(SAFETY)
                    j.call("GET", "/ok")
                    j.check("check 1", True)
                server_suites.run_one_journey("fake", steps, srv_, sink, args, i)
            clean_done.wait(SAFETY)
            server_suites.run_units([functools.partial(unit, i) for i in range(2)], args)

        @contextlib.contextmanager
        def fake_server(args, suite, mocks):
            yield srv

        args = argparse.Namespace(model="m/x", timeout=10, repeat=1, jobs=2, out=self.out, world="none",
                                  proxy_base="http://127.0.0.1:9", suites_parallel=True)
        with mock.patch.dict(server_suites.JOURNEY_CHECKS, {"fake": ["check 0", "check 1"]}), \
                mock.patch.dict(server_suites.RUNNERS, {"journeys": ({}, clean), "journeys_blind": ({}, budget),
                                                        "journeys_blind2": ({}, victim)}), \
                mock.patch.object(server_suites, "Server", fake_server), \
                mock.patch.object(server_suites, "run_suite", run_suite), \
                mock.patch.object(server_suites, "proxy_stats", lambda a: None), \
                contextlib.redirect_stderr(io.StringIO()):
            failed = server_suites.run_suites(["journeys", "journeys_blind", "journeys_blind2"], args)
        self.assertTrue(failed)
        self.assertIn("journeys", wrapped)  # the ordering wrapper ran
        self.assertTrue(server_suites.STOP.is_set())  # the refusal stopped the run
        self.assertEqual(server_suites.exit_code(failed), server_suites.EXIT_BUDGET)
        rows = self.written()
        self.assertNotIn("suite_error", [r["case"] for r in rows])
        by_suite = {}
        for r in rows:
            by_suite.setdefault(r["suite"], []).append(r["case"])
        self.assertEqual(len(by_suite["journeys"]), 4)  # finished before the refusal
        self.assertNotIn("fake.check 1", by_suite.get("journeys_blind2", []))  # nothing after the stop
        self.assertEqual(sorted(r["suite"] for r in self.written("invalid.jsonl")),
                         ["journeys_blind", "journeys_blind2"])
        kept, _ = aggregate.drop_invalid_runs(rows, self.written("invalid.jsonl"))
        self.assertEqual({r["suite"] for r in kept}, {"journeys"})


class TrippedProxyRefusesTest(unittest.TestCase):
    def test_a_tripped_proxy_refuses_the_run_loudly(self):
        args = argparse.Namespace(jobs=1, suites_parallel=False, model="m/x", proxy_base="http://127.0.0.1:9")
        answer = {"reserve_usd": 0.001, "basis": "measured", "budget_usd": 9.0, "spent_usd": 9.0,
                  "reserved_usd": 0.0, "tripped": True}
        err = io.StringIO()
        with mock.patch.object(server_suites, "proxy_get", lambda a, path: answer), contextlib.redirect_stderr(err):
            self.assertTrue(server_suites.warn_headroom(args, ["journeys"]))
        self.assertIn("ERROR", err.getvalue())
        self.assertIn("already tripped", err.getvalue())

    def test_an_untripped_proxy_lets_the_run_start(self):
        args = argparse.Namespace(jobs=1, suites_parallel=False, model="m/x", proxy_base="http://127.0.0.1:9")
        answer = {"reserve_usd": 0.001, "basis": "measured", "budget_usd": 9.0, "spent_usd": 0.0,
                  "reserved_usd": 0.0, "tripped": False}
        with mock.patch.object(server_suites, "proxy_get", lambda a, path: answer), \
                contextlib.redirect_stderr(io.StringIO()):
            self.assertFalse(server_suites.warn_headroom(args, ["journeys"]))


class ProxyBudgetAccountingTest(unittest.TestCase):
    BODY = MeasuredReservationTest.BODY
    make = MeasuredReservationTest.make
    settle = MeasuredReservationTest.settle

    def test_stats_count_budget_refusals_per_label(self):
        px = self.make()
        self.settle(px, 0.0, status=402, error_kind="budget")
        self.settle(px, 0.001)
        entry = px.ledger.stats()["per_label"]["m__x~journeys"]
        self.assertEqual((entry["calls"], entry["budget_refused"]), (2, 1))

    def test_a_proxy_error_is_charged_the_worst_case(self):
        px = self.make()
        for _ in range(proxy.MEASURED_MIN_CALLS):
            self.settle(px, 0.001)
        self.assertEqual(px.reservation("m/x", self.BODY)[1], "measured")
        handler = type("H", (proxy.Handler,), {"proxy": px})
        srv = serve(handler, self)
        import urllib.error
        import urllib.request
        req = urllib.request.Request(f"{srv.data}/r/m__x~journeys/api/v1/chat/completions",
                                     data=json.dumps(self.BODY).encode(), method="POST",
                                     headers={"content-type": "application/json"})
        with mock.patch.object(proxy.Handler, "forward", side_effect=RuntimeError("boom")):
            with self.assertRaises(urllib.error.HTTPError) as cm:
                urllib.request.urlopen(req, timeout=10)
        cm.exception.close()
        self.assertEqual(cm.exception.code, 502)
        with open(px.ledger.path) as f:
            last = [json.loads(line) for line in f if line.strip()][-1]
        self.assertEqual(last["error_kind"], "stream_error")
        self.assertAlmostEqual(last["cost_usd"], px.worst_case("m/x", self.BODY)[1])


class RunRecordsBudgetStopTest(unittest.TestCase):
    def test_exit_3_is_an_invalid_run_not_a_failure(self):
        import run
        self.assertEqual(run.model_status(0, run.SERVER_EXIT_BUDGET, None),
                         ("invalid", {"invalid_reason": proxy.BUDGET_EXHAUSTED_MESSAGE}))
        self.assertEqual(run.model_status(0, 1, None), ("failed", {}))
        self.assertEqual(run.model_status(0, 0, None), ("done", {}))
        self.assertEqual(run.model_status(0, None, "budget tripped"), ("partial", {}))
        self.assertEqual(run.SERVER_EXIT_BUDGET, server_suites.EXIT_BUDGET)

    def test_a_budget_stop_in_any_suite_is_the_exit(self):
        import run

        class Kids:
            def __init__(self, rcs):
                self.rcs = list(rcs)

            def run(self, cmd, env, log):
                return self.rcs.pop(0)

        args = argparse.Namespace(server_suites="server_suites.py", suites="journeys,sse,import", provider_pins="",
                                  no_provider_check=True)
        px = argparse.Namespace(base="http://127.0.0.1:9")
        self.assertEqual(run.run_server_suites(args, Kids([1, 3, 3]), px, "m/x", 1, "/tmp/x"), 3)
        self.assertEqual(run.run_server_suites(args, Kids([0, 1, 0]), px, "m/x", 1, "/tmp/x"), 1)


if __name__ == "__main__":
    unittest.main()
