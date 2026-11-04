"""Tests for the W1 harness fixes: echo-aware grading, fallback causes and
header-safe session ids. Run: python3 -m unittest scripts/llm-bench/test_w1.py"""

import argparse
import builtins
import contextlib
import fcntl
import io
import json
import os
import random
import shutil
import signal
import subprocess
import sys
import tempfile
import threading
import time
import unittest
import urllib.parse
import urllib.request
from unittest import mock
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))

import aggregate  # noqa: E402
import grade  # noqa: E402
import run  # noqa: E402
import server_suites  # noqa: E402

UUID_ID = {"type": "object", "properties": {"id": {"type": "string", "format": "uuid"}}}


class EchoTest(unittest.TestCase):
    def test_echoed_path_id_is_not_a_violation(self):
        sample = {"schema": UUID_ID, "body": {"id": "garnet"}, "request": {"path": {"id": "garnet"}}}
        g = grade.grade_sample(sample, "m", "k")
        self.assertTrue(g["valid"])
        self.assertEqual([e["path"] for e in g["echoed"]], ["/id"])

    def test_invented_bad_id_still_fails(self):
        sample = {"schema": UUID_ID, "body": {"id": "garnet"}, "request": {"path": {"id": "kestrel"}}}
        g = grade.grade_sample(sample, "m", "k")
        self.assertFalse(g["valid"])
        self.assertEqual(g["echoed"], [])

    def test_only_format_violations_can_be_echoes(self):
        schema = {"type": "object", "properties": {"id": {"type": "string", "pattern": "^pi_"}}}
        sample = {"schema": schema, "body": {"id": "garnet"}, "request": {"query": {"q": ["garnet"]}}}
        self.assertFalse(grade.grade_sample(sample, "m", "k")["valid"])


class CauseTest(unittest.TestCase):
    def test_causes(self):
        cases = {
            "LLM output truncated: provider X, stopped at max_tokens (16384)": "truncated",
            "repaired output still invalid: output is not valid JSON": "non_json",
            "repaired output still invalid: /a: type: x": "schema",
            "LLM request timed out": "timeout",
            "timed out waiting for a generation slot": "guard",
            "token budget exhausted": "guard",
            "response of 9 bytes exceeds the 8-byte cap": "guard",
            "LLM returned HTTP 400: nope": "other",
            "": "other",
        }
        for reason, cause in cases.items():
            self.assertEqual(aggregate.fallback_cause({"fallback_reason": reason}), cause, reason)
        self.assertTrue(set(cases.values()) <= set(aggregate.FALLBACK_CAUSES))


class PatchedSourceTest(unittest.TestCase):
    def test_patched_counts_as_schema_valid_and_is_reported_apart(self):
        items = [({"source": src}, None) for src in ("llm", "patched", "repaired", "fallback")]
        t = aggregate.tally(items, has_grades=False)
        self.assertEqual(t["schema_valid"], 0.75)
        self.assertEqual(t["patch"], 0.25)
        self.assertEqual(t["repair"], 0.25)
        self.assertIn("patched", aggregate.ENGINE_SOURCES)


class PerCallRatesTest(unittest.TestCase):
    def test_per_call_rates_are_not_averaged_over_api_types(self):
        items = [({"source": "llm", "api_type": "grpc"}, None)]
        items += [({"source": s, "api_type": "rest"}, None) for s in ("patched", "patched", "repaired")]
        block = aggregate.engine_block(items, False, {})
        self.assertEqual(block["macro"]["first_try"], 0.5)
        self.assertEqual(block["first_try"], 0.25)
        self.assertEqual(block["no_second_call"], 0.75)


class OutputModeTest(unittest.TestCase):
    def test_nojson_variant_runs_the_engine_with_no_response_format(self):
        self.assertEqual(run.output_mode("thinkingmachines/inkling@nojson", "json_object"), "none")
        self.assertEqual(run.output_mode("deepseek/deepseek-v4.1-flash", "json_object"), "json_object")
        self.assertEqual(run.output_mode("deepseek/deepseek-v4.1-flash", "json_schema"), "json_schema")


class SliceTest(unittest.TestCase):
    def test_slice_keeps_only_the_selections_of_kept_operations(self):
        m = {"entries": [{"name": "g", "api_type": "graphql", "operations": ["Query.a", "Query.b"],
                          "selections": {"Query.a": {"x": {}}, "Query.b": {"y": {}}}}]}
        e = run.slice_manifest(m, 1)["entries"][0]
        self.assertEqual(e["operations"], ["Query.a"])
        self.assertEqual(e["selections"], {"Query.a": {"x": {}}})


class SessionSlugTest(unittest.TestCase):
    def test_variant_suffix_is_header_safe(self):
        self.assertEqual(server_suites.session_slug("thinkingmachines/inkling@nojson"),
                         "thinkingmachines__inkling-nojson")


class FakeStripe(BaseHTTPRequestHandler):
    """A tiny Stripe that remembers what it was told. `stale_read` makes a
    read of a confirmed intent return the old status."""

    store = {}
    stale_read = False

    def log_message(self, *a):
        pass

    def reply(self, obj, code=200):
        raw = json.dumps(obj).encode()
        self.send_response(code)
        self.send_header("content-type", "application/json")
        self.send_header("x-mockingbird-source", "llm")
        self.send_header("content-length", str(len(raw)))
        self.end_headers()
        self.wfile.write(raw)

    def form(self):
        n = int(self.headers.get("content-length") or 0)
        if "x-www-form-urlencoded" not in (self.headers.get("content-type") or ""):
            return {}
        return {k: v[0] for k, v in urllib.parse.parse_qs(self.rfile.read(n).decode()).items()}

    def do_POST(self):
        path, body, st = self.path.split("?")[0], self.form(), FakeStripe.store
        if path == "/stripe/v1/customers":
            c = {"id": f"cus_{len(st)}", "name": body.get("name"), "email": body.get("email")}
            st[c["id"]] = c
            return self.reply(c)
        if path == "/stripe/v1/payment_intents":
            pi = {"id": f"pi_{len(st)}", "amount": int(body["amount"]), "currency": body["currency"],
                  "customer": body.get("customer"), "status": "requires_confirmation"}
            st[pi["id"]] = pi
            return self.reply(pi)
        if path.endswith("/confirm"):
            pi = st[path.split("/")[-2]]
            if not FakeStripe.stale_read:
                pi["status"] = "succeeded"
            pi["amount_received"] = pi["amount"]
            return self.reply(dict(pi, status="succeeded"))
        if path == "/stripe/v1/refunds":
            pi = st[body["payment_intent"]]
            return self.reply({"id": "re_1", "payment_intent": pi["id"], "amount": int(body["amount"]),
                               "currency": pi["currency"]})
        self.reply({}, 404)

    def do_GET(self):
        item = FakeStripe.store.get(self.path.split("?")[0].split("/")[-1])
        self.reply(item or {}, 200 if item else 404)


class FakeSquare(FakeStripe):
    """A tiny Square that remembers its payments. `stale_read` makes a read of a
    completed payment return the old status."""

    store = {}
    stale_read = False

    def do_POST(self):
        path, st = self.path.split("?")[0], FakeSquare.store
        body = json.loads(self.rfile.read(int(self.headers.get("content-length") or 0)) or b"{}")
        if path == "/square/v2/payments":
            pay = {k: body.get(k) for k in ("amount_money", "tip_money", "location_id", "reference_id", "note")}
            total = body["amount_money"]["amount"] + body["tip_money"]["amount"]
            pay.update(id=f"pay{len(st)}", order_id="ord1", status="APPROVED",
                       total_money={"amount": total, "currency": "USD"})
            st[pay["id"]] = pay
            return self.reply({"payment": pay})
        if path.endswith("/complete"):
            pay = st[path.split("/")[-2]]
            if not FakeSquare.stale_read:
                pay["status"] = "COMPLETED"
            return self.reply({"payment": dict(pay, status="COMPLETED")})
        if path == "/square/v2/refunds":
            pay = st[body["payment_id"]]
            refund = {"id": "ref1", "payment_id": pay["id"], "amount_money": body["amount_money"],
                      "reason": body["reason"], "status": "PENDING"}
            pay.update(refund_ids=[refund["id"]], refunded_money=body["amount_money"])
            return self.reply({"refund": refund})
        self.reply({}, 404)

    def do_GET(self):
        path = self.path.split("?")[0]
        if path == "/square/v2/payments":
            return self.reply({"payments": list(FakeSquare.store.values())})
        pay = FakeSquare.store.get(path.split("/")[-1])
        self.reply({"payment": pay} if pay else {}, 200 if pay else 404)


class HeldOutJourneyTest(unittest.TestCase):
    def run_square(self, stale):
        FakeSquare.store, FakeSquare.stale_read = {}, stale
        httpd = ThreadingHTTPServer(("127.0.0.1", 0), FakeSquare)
        threading.Thread(target=httpd.serve_forever, daemon=True).start()
        self.addCleanup(httpd.server_close)
        self.addCleanup(httpd.shutdown)

        class Srv:
            data = f"http://127.0.0.1:{httpd.server_address[1]}"

        class Args:
            model, timeout, repeat = "m/x", 10, 1

        with tempfile.TemporaryDirectory() as out:
            sink = server_suites.Sink(out, "m/x", "journeys_heldout")
            server_suites.run_one_journey("square", server_suites.square_steps, Srv, sink, Args, 0)
        return {r["case"]: r for r in sink.rows}

    def test_journeys_blind_is_a_suite(self):
        self.assertIn("journeys_blind", server_suites.SUITES)
        self.assertNotIn("journeys_blind", server_suites.DEFAULT_SUITES)
        mocks, runner = server_suites.RUNNERS["journeys_blind"]
        self.assertIs(runner, server_suites.run_journeys_blind)
        self.assertEqual(sorted(mocks), ["auth0", "discord", "hubspot", "slack", "spotify"])
        for contract, _, persona in mocks.values():
            self.assertIsNone(persona)
            self.assertTrue(os.path.exists(os.path.join(server_suites.REPO, contract)), contract)

    def test_journeys_blind2_is_a_suite(self):
        self.assertIn("journeys_blind2", server_suites.SUITES)
        self.assertNotIn("journeys_blind2", server_suites.DEFAULT_SUITES)
        mocks, runner = server_suites.RUNNERS["journeys_blind2"]
        self.assertIs(runner, server_suites.run_journeys_blind2)
        self.assertEqual(sorted(mocks), ["openweathermap", "paypalpayments", "plaid", "sendgrid", "twilio"])
        for contract, _, persona in mocks.values():
            self.assertIsNone(persona)
            self.assertTrue(os.path.exists(os.path.join(server_suites.REPO, contract)), contract)
        for name in mocks:
            self.assertEqual(len(server_suites.JOURNEY_CHECKS[name]), 14, name)

    def test_journeys_blind3_is_a_suite(self):
        self.assertIn("journeys_blind3", server_suites.SUITES)
        self.assertNotIn("journeys_blind3", server_suites.DEFAULT_SUITES)
        mocks, runner = server_suites.RUNNERS["journeys_blind3"]
        self.assertIs(runner, server_suites.run_journeys_blind3)
        self.assertEqual(sorted(mocks), ["asana", "freshdesk", "gitlab", "mailchimp", "trello"])
        for contract, _, persona in mocks.values():
            self.assertIsNone(persona)
            self.assertTrue(os.path.exists(os.path.join(server_suites.REPO, contract)), contract)
        for name in mocks:
            # An unknown id is generated by default (`unknown_ids: generate`), so the
            # check that it is not found was dropped from these.
            want = 13 if name in ("sentry", "calcom") else 14
            self.assertEqual(len(server_suites.JOURNEY_CHECKS[name]), want, name)
            self.assertEqual(len(set(server_suites.JOURNEY_CHECKS[name])), want, name)

    def test_journeys_blind4_is_a_suite(self):
        self.assertIn("journeys_blind4", server_suites.SUITES)
        self.assertNotIn("journeys_blind4", server_suites.DEFAULT_SUITES)
        mocks, runner = server_suites.RUNNERS["journeys_blind4"]
        self.assertIs(runner, server_suites.run_journeys_blind4)
        self.assertEqual(sorted(mocks), ["contentful", "digitalocean", "pipedrive", "sentry", "zoom"])
        for contract, _, persona in mocks.values():
            self.assertIsNone(persona)
            self.assertTrue(os.path.exists(os.path.join(server_suites.REPO, contract)), contract)
        for name in mocks:
            # An unknown id is generated by default (`unknown_ids: generate`), so the
            # check that it is not found was dropped from these.
            want = 13 if name in ("sentry", "calcom") else 14
            self.assertEqual(len(server_suites.JOURNEY_CHECKS[name]), want, name)
            self.assertEqual(len(set(server_suites.JOURNEY_CHECKS[name])), want, name)

    def test_journeys_blind5_is_a_suite(self):
        self.assertIn("journeys_blind5", server_suites.SUITES)
        self.assertNotIn("journeys_blind5", server_suites.DEFAULT_SUITES)
        mocks, runner = server_suites.RUNNERS["journeys_blind5"]
        self.assertIs(runner, server_suites.run_journeys_blind5)
        self.assertEqual(sorted(mocks), ["airtable", "box", "calcom", "intercom", "okta"])
        for contract, _, persona in mocks.values():
            self.assertIsNone(persona)
            self.assertTrue(os.path.exists(os.path.join(server_suites.REPO, contract)), contract)
        for name in mocks:
            # An unknown id is generated by default (`unknown_ids: generate`), so the
            # check that it is not found was dropped from these.
            want = 13 if name in ("sentry", "calcom") else 14
            self.assertEqual(len(server_suites.JOURNEY_CHECKS[name]), want, name)
            self.assertEqual(len(set(server_suites.JOURNEY_CHECKS[name])), want, name)

    def test_journeys_blind6_is_a_suite(self):
        self.assertIn("journeys_blind6", server_suites.SUITES)
        self.assertNotIn("journeys_blind6", server_suites.DEFAULT_SUITES)
        mocks, runner = server_suites.RUNNERS["journeys_blind6"]
        self.assertIs(runner, server_suites.run_journeys_blind6)
        self.assertEqual(sorted(mocks), ["dropbox", "klaviyo", "statuspage", "woocommerce", "zohobooks"])
        for contract, _, persona in mocks.values():
            self.assertIsNone(persona)
            self.assertTrue(os.path.exists(os.path.join(server_suites.REPO, contract)), contract)
        for name in mocks:
            # An unknown id is generated by default (`unknown_ids: generate`), so the
            # check that it is not found was dropped from these.
            want = 13 if name in ("sentry", "calcom") else 14
            self.assertEqual(len(server_suites.JOURNEY_CHECKS[name]), want, name)
            self.assertEqual(len(set(server_suites.JOURNEY_CHECKS[name])), want, name)

    def test_journeys_blind7_is_a_suite(self):
        self.assertIn("journeys_blind7", server_suites.SUITES)
        self.assertNotIn("journeys_blind7", server_suites.DEFAULT_SUITES)
        mocks, runner = server_suites.RUNNERS["journeys_blind7"]
        self.assertIs(runner, server_suites.run_journeys_blind7)
        self.assertEqual(sorted(mocks), ["bitbucket", "chargebee", "cloudflare", "linear", "mollie"])
        self.assertEqual(sorted(server_suites.BLIND7_JOURNEYS), sorted(mocks))
        self.assertTrue(mocks["linear"][0].endswith("/linear.graphql"))
        for contract, prefix, persona in mocks.values():
            self.assertIsNone(persona)
            self.assertTrue(os.path.exists(os.path.join(server_suites.REPO, contract)), contract)
        for name in mocks:
            self.assertEqual(len(server_suites.JOURNEY_CHECKS[name]), 14, name)
            self.assertEqual(len(set(server_suites.JOURNEY_CHECKS[name])), 14, name)

    def test_journeys_heldout_is_a_suite(self):
        self.assertIn("journeys_heldout", server_suites.SUITES)
        self.assertNotIn("journeys_heldout", server_suites.DEFAULT_SUITES)
        mocks, runner = server_suites.RUNNERS["journeys_heldout"]
        self.assertIs(runner, server_suites.run_journeys_heldout)
        self.assertEqual(sorted(mocks), ["jira", "notion", "paypal", "square", "zendesk"])
        for contract, _, persona in mocks.values():
            self.assertIsNone(persona)
            self.assertTrue(os.path.exists(os.path.join(server_suites.REPO, contract)), contract)
        for name in mocks:
            self.assertTrue(8 <= len(server_suites.JOURNEY_CHECKS[name]) <= 14, name)

    def test_square_journey_scores_a_consistent_fake_and_catches_a_stale_read(self):
        rows = self.run_square(stale=False)
        self.assertEqual(sorted(rows), sorted(f"square.{n}" for n in server_suites.JOURNEY_CHECKS["square"]))
        self.assertEqual([c for c, r in rows.items() if not r["ok"]], [])
        rows = self.run_square(stale=True)
        self.assertEqual(sorted(c for c, r in rows.items() if not r["ok"]),
                         ["square.list shows the completed payment", "square.read shows the completed status"])


class JourneyTest(unittest.TestCase):
    def run_stripe(self, stale):
        FakeStripe.store, FakeStripe.stale_read = {}, stale
        httpd = ThreadingHTTPServer(("127.0.0.1", 0), FakeStripe)
        threading.Thread(target=httpd.serve_forever, daemon=True).start()
        self.addCleanup(httpd.server_close)
        self.addCleanup(httpd.shutdown)

        class Srv:
            data = f"http://127.0.0.1:{httpd.server_address[1]}"

        class Args:
            model, timeout = "m/x", 10

        with tempfile.TemporaryDirectory() as out:
            sink = server_suites.Sink(out, "m/x", "journeys")
            server_suites.journey_stripe(Srv, sink, Args, 0)
            with open(os.path.join(out, "server.jsonl")) as f:
                return {r["case"]: r for r in map(json.loads, f)}

    def test_journeys_is_a_suite(self):
        self.assertIn("journeys", server_suites.SUITES)
        self.assertIn("journeys", server_suites.RUNNERS)
        self.assertNotIn("journeys", server_suites.DEFAULT_SUITES)

    def test_stripe_journey_scores_a_consistent_run(self):
        rows = self.run_stripe(stale=False)
        self.assertEqual(sorted(rows), sorted(f"stripe.{n}" for n in server_suites.JOURNEY_CHECKS["stripe"]
                                              + ["a succeeded intent received its amount"]))
        self.assertEqual([c for c, r in rows.items() if not r["ok"]], [])
        self.assertEqual(rows["stripe.refund names its intent"]["source"], "llm")

    def test_stripe_journey_catches_a_lost_status(self):
        rows = self.run_stripe(stale=True)
        self.assertFalse(rows["stripe.read shows the confirmed status"]["ok"])
        self.assertTrue(rows["stripe.refund names its intent"]["ok"])

    def test_a_broken_journey_fails_its_remaining_checks(self):
        class Args:
            model, timeout = "m/x", 1

        class Srv:
            data = "http://127.0.0.1:1"

        with tempfile.TemporaryDirectory() as out:
            sink = server_suites.Sink(out, "m/x", "journeys")
            server_suites.journey_stripe(Srv, sink, Args, 0)
        self.assertEqual(len(sink.rows), len(server_suites.JOURNEY_CHECKS["stripe"]))
        self.assertFalse(any(r["ok"] for r in sink.rows))


class FakeDropbox(FakeStripe):
    """A Dropbox whose folder listings hold one entry without an `id`."""

    def do_POST(self):
        self.rfile.read(int(self.headers.get("content-length") or 0))
        self.reply({"metadata": {"id": "id:aaaaaaaaaaaaaaaaaaaaaa"}, "has_more": False,
                    "entries": [{"id": "id:aaaaaaaaaaaaaaaaaaaaaa"}, {"name": "no-id"}]})


class MissingIdTest(unittest.TestCase):
    def test_same_items_never_raises(self):
        self.assertTrue(server_suites.same_items(["b", "a"], ["a", "b"]))
        self.assertTrue(server_suites.same_items([2, 1], [1, 2]))
        self.assertFalse(server_suites.same_items(["a", None], ["a", "b"]))
        self.assertFalse(server_suites.same_items([1, "1"], ["1", "2"]))
        self.assertFalse(server_suites.same_items(None, ["a"]))
        self.assertTrue(server_suites.same_items([None, "a"], ["a", None]))

    def test_an_entry_without_an_id_fails_its_check_and_the_journey_goes_on(self):
        httpd = ThreadingHTTPServer(("127.0.0.1", 0), FakeDropbox)
        threading.Thread(target=httpd.serve_forever, daemon=True).start()
        self.addCleanup(httpd.server_close)
        self.addCleanup(httpd.shutdown)

        class Srv:
            data = f"http://127.0.0.1:{httpd.server_address[1]}"

        class Args:
            model, timeout, repeat = "m/x", 10, 1

        with tempfile.TemporaryDirectory() as out:
            sink = server_suites.Sink(out, "m/x", "journeys_blind6")
            server_suites.run_one_journey("dropbox", server_suites.dropbox_steps, Srv, sink, Args, 0)
        rows = {r["case"]: r for r in sink.rows}
        listed = rows["dropbox.list_folder holds the children"]
        self.assertFalse(listed["ok"])
        self.assertNotEqual(listed["source"], "skipped")
        self.assertEqual([c for c, r in rows.items() if r["source"] == "skipped"], [])


class FakeEmpty(FakeStripe):
    """An API that answers every call with an empty JSON object."""

    def answer(self):
        self.rfile.read(int(self.headers.get("content-length") or 0))
        self.reply({})

    do_GET = do_POST = do_PUT = do_PATCH = do_DELETE = answer


class EmptyAnswersTest(unittest.TestCase):
    def test_each_blind7_journey_records_every_check_on_empty_answers(self):
        # Every check runs and fails on its own: none gates the rest and none raises.
        httpd = ThreadingHTTPServer(("127.0.0.1", 0), FakeEmpty)
        threading.Thread(target=httpd.serve_forever, daemon=True).start()
        self.addCleanup(httpd.server_close)
        self.addCleanup(httpd.shutdown)

        class Srv:
            data = f"http://127.0.0.1:{httpd.server_address[1]}"

        class Args:
            model, timeout, repeat = "m/x", 10, 1

        for name, steps in server_suites.BLIND7_JOURNEYS.items():
            with tempfile.TemporaryDirectory() as out:
                sink = server_suites.Sink(out, "m/x", "journeys_blind7")
                server_suites.run_one_journey(name, steps, Srv, sink, Args, 0)
            rows = {r["case"]: r for r in sink.rows}
            self.assertEqual(sorted(rows), sorted(f"{name}.{c}" for c in server_suites.JOURNEY_CHECKS[name]), name)
            self.assertEqual([c for c, r in rows.items() if r["source"] == "skipped"], [], name)
            self.assertEqual([c for c, r in rows.items() if r["ok"]], [], name)


class FakeServer(BaseHTTPRequestHandler):
    """One fake `mockingbird serve` for the consistency, SSE and journeys suites:
    thread safe, state kept per `x-mockingbird-session`, and ids counted per
    session, so its answers do not depend on the order the sessions run in.
    Every call waits a little, so parallel units overlap; `peak` counts the
    calls in flight at once. Paths it does not know answer `{}`."""

    lock = threading.Lock()
    stores, streamed = {}, set()
    inflight = peak = 0

    def log_message(self, *a):
        pass

    def reply(self, obj, code=200):
        raw = json.dumps(obj).encode()
        self.send_response(code)
        self.send_header("content-type", "application/json")
        self.send_header("x-mockingbird-source", "llm")
        self.send_header("content-length", str(len(raw)))
        self.end_headers()
        self.wfile.write(raw)

    def body(self):
        raw = self.rfile.read(int(self.headers.get("content-length") or 0))
        if "x-www-form-urlencoded" in (self.headers.get("content-type") or ""):
            return {k: v[0] for k, v in urllib.parse.parse_qs(raw.decode()).items()}
        try:
            return json.loads(raw) if raw else {}
        except ValueError:
            return {}

    def handle_one(self, method):
        cls = FakeServer
        with cls.lock:
            cls.inflight += 1
            cls.peak = max(cls.peak, cls.inflight)
        try:
            time.sleep(0.02)
            url = urllib.parse.urlsplit(self.path)
            body = self.body() if method != "GET" else {}
            if url.path == "/events/ticks":
                return self.stream()
            with cls.lock:
                obj, code = self.route(method, url, body, cls.stores.setdefault(self.session(), {}))
            self.reply(obj, code)
        finally:
            with cls.lock:
                cls.inflight -= 1

    def session(self):
        return self.headers.get("x-mockingbird-session") or ""

    def stream(self):
        want = int(self.headers.get("x-mockingbird-stream-items") or 1)
        self.send_response(200)
        self.send_header("content-type", "text/event-stream")
        self.end_headers()
        for i in range(want):
            self.wfile.write(f"event: tick\ndata: {json.dumps({'id': f't{i}', 'seq': i, 'price': 1.5})}\n\n".encode())
        self.wfile.flush()
        with FakeServer.lock:
            FakeServer.streamed.add(self.session())
        self.close_connection = True

    def route(self, method, url, body, st):
        path, n = url.path, len(st)
        if path.startswith("/api/v1/workspaces/default/mocks/events/requests"):
            session = urllib.parse.parse_qs(url.query).get("session", [""])[0]
            seen = session in FakeServer.streamed
            return ([{"source": "llm", "prompt_tokens": 10, "completion_tokens": 5}] if seen else []), 200
        if method == "POST" and path == "/stripe/v1/customers":
            c = {"id": f"cus_{n}", "name": body.get("name"), "email": body.get("email")}
            st[c["id"]] = c
            return c, 200
        if method == "POST" and path == "/stripe/v1/payment_intents":
            pid = f"pi_{n}"
            pi = {"id": pid, "amount": int(body["amount"]), "currency": body["currency"], "object": "payment_intent",
                  "customer": body.get("customer"), "status": "requires_confirmation",
                  "client_secret": f"{pid}_secret_abc"}
            st[pid] = pi
            return pi, 200
        if method == "POST" and path.endswith("/confirm"):
            pi = st[path.split("/")[-2]]
            pi.update(status="succeeded", amount_received=pi["amount"])
            return pi, 200
        if method == "POST" and path == "/stripe/v1/refunds":
            pi = st[body["payment_intent"]]
            return {"id": f"re_{n}", "payment_intent": pi["id"], "amount": int(body["amount"]),
                    "currency": pi["currency"]}, 200
        if path.startswith("/stripe/v1/"):
            item = st.get(path.split("/")[-1])
            return (item, 200) if item else ({}, 404)
        if path == "/tmdb/3/movie/popular":
            page = int(urllib.parse.parse_qs(url.query).get("page", ["1"])[0])
            return {"results": [{"id": 100 + page, "title": f"Film {page}"}]}, 200
        if path.startswith("/tmdb/3/movie/"):
            mid = int(path.split("/")[-1])
            return {"id": mid, "title": f"Film {mid - 100}"}, 200
        if method == "POST" and path == "/square/v2/payments":
            pay = {"id": f"pay{n}", "amount_money": body.get("amount_money"), "status": "COMPLETED"}
            st[pay["id"]] = pay
            return {"payment": pay}, 200
        if path.startswith("/square/v2/payments/"):
            pay = st.get(path.split("/")[-1])
            return ({"payment": pay}, 200) if pay else ({}, 404)
        return {}, 200

    def do_GET(self):
        self.handle_one("GET")

    def do_POST(self):
        self.handle_one("POST")

    def do_PUT(self):
        self.handle_one("PUT")

    def do_PATCH(self):
        self.handle_one("PATCH")

    def do_DELETE(self):
        self.handle_one("DELETE")


class JobsTest(unittest.TestCase):
    """--jobs N gives the rows of --jobs 1, up to their order and latencies."""

    def setUp(self):
        httpd = ThreadingHTTPServer(("127.0.0.1", 0), FakeServer)
        threading.Thread(target=httpd.serve_forever, daemon=True).start()
        self.addCleanup(httpd.server_close)
        self.addCleanup(httpd.shutdown)
        base = f"http://127.0.0.1:{httpd.server_address[1]}"

        class Srv:
            data, api = base, f"{base}/api/v1/workspaces/default"

        self.srv = Srv

    def run_suite(self, suite, jobs, **extra):
        FakeServer.stores, FakeServer.streamed, FakeServer.peak = {}, set(), 0
        args = argparse.Namespace(**dict(dict(model="m/x", timeout=10, repeat=3, journey=None, stream_items=3),
                                         jobs=jobs, **extra))
        _, runner = server_suites.RUNNERS[suite]
        with tempfile.TemporaryDirectory() as out:
            sink = server_suites.Sink(out, "m/x", suite)
            runner(self.srv, sink, args)
            with open(os.path.join(out, "server.jsonl")) as f:
                written = [json.loads(line) for line in f]
        self.assertEqual(written, json.loads(json.dumps(sink.rows)))  # every row written whole, once
        rows = sorted(json.dumps({k: v for k, v in r.items() if k not in ("latency_ms", "ttfe_ms")}, sort_keys=True)
                      for r in sink.rows)
        return rows, FakeServer.peak

    def assert_same_rows(self, suite, **extra):
        seq, seq_peak = self.run_suite(suite, 1, **extra)
        par, par_peak = self.run_suite(suite, 4, **extra)
        self.assertEqual(seq_peak, 1, suite)
        self.assertGreater(par_peak, 1, suite)
        self.assertEqual(par, seq, suite)
        return [json.loads(r) for r in seq]

    def test_consistency(self):
        rows = self.assert_same_rows("consistency")
        self.assertEqual(len(rows), 18)
        self.assertTrue(all(r["ok"] for r in rows), [r for r in rows if not r["ok"]])

    def test_sse(self):
        rows = self.assert_same_rows("sse")
        self.assertEqual([(r["rep"], r["ok"], r["source"]) for r in sorted(rows, key=lambda r: r["rep"])],
                         [(0, True, "llm"), (1, True, "llm"), (2, True, "llm")])

    def test_journeys(self):
        rows = self.assert_same_rows("journeys", journey=["stripe", "travel", "github"])
        stripe = [r for r in rows if r["case"].startswith("stripe.")]
        self.assertTrue(stripe and all(r["ok"] for r in stripe))
        self.assertEqual(sorted({r["rep"] for r in rows}), [0, 1, 2])

    def test_journeys_blind7(self):
        rows = self.assert_same_rows("journeys_blind7")
        self.assertEqual(len(rows), 3 * sum(len(server_suites.JOURNEY_CHECKS[n]) for n in server_suites.BLIND7_JOURNEYS))

    def test_a_unit_error_is_raised_and_stops_units_not_started(self):
        ran = []

        def unit(i):
            def go():
                time.sleep(0.01)
                ran.append(i)
                if i == 1:
                    raise ValueError("unit 1")
            return go

        with self.assertRaisesRegex(ValueError, "unit 1"):
            server_suites.run_units([unit(i) for i in range(50)], argparse.Namespace(jobs=2))
        self.assertLess(len(ran), 50)
        ran.clear()
        with self.assertRaisesRegex(ValueError, "unit 1"):
            server_suites.run_units([unit(i) for i in range(5)], argparse.Namespace(jobs=1))
        self.assertEqual(ran, [0, 1])


class InterruptTest(unittest.TestCase):
    def setUp(self):
        server_suites.STOP.clear()
        self.addCleanup(server_suites.STOP.clear)
        old = signal.signal(signal.SIGINT, signal.default_int_handler)
        self.addCleanup(signal.signal, signal.SIGINT, old)

    def test_ctrl_c_cancels_the_queued_units(self):
        started = []

        def unit():
            started.append(1)
            time.sleep(0.2)

        timer = threading.Timer(0.3, signal.pthread_kill, (threading.main_thread().ident, signal.SIGINT))
        timer.start()
        self.addCleanup(timer.cancel)
        t0 = time.monotonic()
        with self.assertRaises(KeyboardInterrupt):
            server_suites.run_units([unit] * 20, argparse.Namespace(jobs=2))
        self.assertLess(time.monotonic() - t0, 1.0)
        time.sleep(0.3)  # the units that were running finish; nothing queued starts
        self.assertLessEqual(len(started), 6)
        # Units still running stop at their next call.
        self.assertTrue(server_suites.STOP.is_set())
        with self.assertRaises(server_suites.Stopped):
            server_suites.request("http://127.0.0.1:1/")

    def test_suites_parallel_goes_through_the_same_pool(self):
        with open(server_suites.__file__) as f:
            source = f.read()
        self.assertIn("run_units([functools.partial(run_suite, suite, args) for suite in suites]", source)


# A stand-in for `mockingbird serve`: binds the three ports of its config after a
# delay (a slow start widens the window a port lock must cover), exits 98 when
# one is taken, then answers every GET. MODE "fail-once" exits 3 on its first
# start in a directory, "fail-always" on every start.
FAKE_BINARY = """#!/usr/bin/env python3
import os, re, socket, sys, threading, time
from http.server import BaseHTTPRequestHandler, HTTPServer
MODE = "@MODE@"
if MODE == "fail-always" or (MODE == "fail-once" and not os.path.exists("tried")):
    open("tried", "w").close()
    sys.exit(3)
config = open(sys.argv[sys.argv.index("-c") + 1]).read()
ports = [int(re.search(name + r": (\\d+)", config).group(1)) for name in (" port", "admin_port", "grpc_port")]

class Ready(BaseHTTPRequestHandler):
    def log_message(self, *a):
        pass

    def do_GET(self):
        self.send_response(200)
        self.end_headers()

time.sleep(0.3)
servers = []
try:
    for port in ports:
        servers.append(HTTPServer(("127.0.0.1", port), Ready))
except OSError:
    sys.exit(98)
for s in servers[:-1]:
    threading.Thread(target=s.serve_forever, daemon=True).start()
servers[-1].serve_forever()
"""

# Runs in its own process, so the flock path between processes is what is tested.
# "server": starts a Server with a port picker that always offers the lowest free
# ports from `base`, so two servers picking before either binds collide for sure.
# "sink": appends large rows whose writes are split in two with a pause between.
CHILD = """
import argparse, builtins, contextlib, json, socket, sys, time, types
sys.path.insert(0, @BENCH@)
import server_suites as ss
cfg = json.loads(sys.argv[1])
ss.PORT_LOCK_PATH = cfg["lock"]
if cfg.get("no_lock"):
    ss.port_reservation = lambda timeout: contextlib.nullcontext()
    ss.fcntl = types.SimpleNamespace(flock=lambda *a: None, LOCK_EX=0, LOCK_UN=0, LOCK_NB=0)

def lowest_free(n):
    out, port = [], cfg.get("base", 0)
    while len(out) < n:
        with socket.socket() as s:
            try:
                s.bind(("127.0.0.1", port))
                out.append(port)
            except OSError:
                pass
        port += 1
    return tuple(out)

class SplitFile:
    def __init__(self, path, mode):
        self.f = builtins.open(path, mode)
    def fileno(self):
        return self.f.fileno()
    def write(self, data):
        half = len(data) // 2
        self.f.write(data[:half]); self.f.flush(); time.sleep(0.002); self.f.write(data[half:])
        return len(data)
    def flush(self):
        self.f.flush()
    def __enter__(self):
        return self
    def __exit__(self, *a):
        self.f.close()

while time.time() < cfg["at"]:
    time.sleep(0.001)
if cfg["mode"] == "server":
    ss.free_ports = lowest_free
    ss.Server.START_ATTEMPTS = 1
    args = argparse.Namespace(model="m/x", proxy_base="http://127.0.0.1:1", out=cfg["out"], binary=cfg["binary"],
                              ready_timeout=20, keep=False)
    try:
        with ss.Server(args, cfg["suite"], {}) as srv:
            print(json.dumps({"ports": srv.ports}), flush=True)
            time.sleep(cfg["hold"])
    except ss.ServerExited as e:
        print(json.dumps({"error": str(e)}), flush=True)
else:
    ss.open = SplitFile
    sink = ss.Sink(cfg["out"], cfg["model"], "journeys")
    for i in range(cfg["rows"]):
        sink.add(f"case{i}", 0, True, 1.0, "llm", {"blob": "x" * 50000, "i": i})
"""


def write_binary(directory, mode="ok"):
    path = os.path.join(directory, f"fake-mockingbird-{mode}")
    with open(path, "w") as f:
        f.write(FAKE_BINARY.replace("/usr/bin/env python3", sys.executable, 1).replace("@MODE@", mode))
    os.chmod(path, 0o755)
    return path


def run_children(directory, configs):
    """Starts one CHILD per config at the same moment; their stdout JSON lines."""
    script = os.path.join(directory, "child.py")
    with open(script, "w") as f:
        f.write(CHILD.replace("@BENCH@", repr(os.path.dirname(os.path.abspath(__file__)))))
    at = time.time() + 1.5
    procs = [subprocess.Popen([sys.executable, script, json.dumps(dict(c, at=at))], stdout=subprocess.PIPE,
                              stderr=subprocess.DEVNULL, text=True) for c in configs]
    out = []
    for p in procs:
        stdout, _ = p.communicate(timeout=60)
        out += [json.loads(line) for line in stdout.splitlines() if line.strip()]
    return out


class ServerPortsTest(unittest.TestCase):
    def setUp(self):
        self.dir = tempfile.mkdtemp(prefix="mb-ports-test-")
        self.addCleanup(shutil.rmtree, self.dir, ignore_errors=True)
        self.lock = os.path.join(self.dir, "ports.lock")  # not the real one: a live bench may hold that

    def start_at_once(self, no_lock):
        binary = write_binary(self.dir)
        base = random.randint(21000, 29000)
        return run_children(self.dir, [
            {"mode": "server", "lock": self.lock, "no_lock": no_lock, "base": base, "out": self.dir,
             "binary": binary, "suite": f"suite{i}", "hold": 4} for i in range(4)])

    def test_free_ports_are_distinct(self):
        self.assertEqual(len(set(server_suites.free_ports(8))), 8)

    def test_servers_started_at_once_in_separate_processes_get_their_own_ports(self):
        got = self.start_at_once(no_lock=False)
        self.assertEqual([g for g in got if "error" in g], [])
        ports = [p for g in got for p in g["ports"]]
        self.assertEqual((len(ports), len(set(ports))), (12, 12))

    def test_without_the_port_lock_the_same_start_collides(self):
        # The control for the test above: the picker and timing do force a collision.
        got = self.start_at_once(no_lock=True)
        ports = [p for g in got for p in g.get("ports", ())]
        self.assertTrue(any("error" in g for g in got) or len(set(ports)) < len(ports), got)

    def test_a_server_that_exits_before_ready_is_retried_on_new_ports(self):
        args = argparse.Namespace(model="m/x", proxy_base="http://127.0.0.1:1", out=self.dir,
                                  binary=write_binary(self.dir, "fail-once"), ready_timeout=20, keep=False)
        with mock.patch.object(server_suites, "PORT_LOCK_PATH", self.lock):
            with server_suites.Server(args, "journeys", {}) as srv:
                with urllib.request.urlopen(f"{srv.admin}/readyz", timeout=5) as r:
                    self.assertEqual(r.status, 200)
                log_path = srv.log_path
        with open(log_path) as f:
            self.assertEqual(f.read().count("retrying on new ports"), 1)

    def test_a_server_that_never_gets_ready_fails_after_its_attempts(self):
        args = argparse.Namespace(model="m/x", proxy_base="http://127.0.0.1:1", out=self.dir,
                                  binary=write_binary(self.dir, "fail-always"), ready_timeout=20, keep=False)
        with mock.patch.object(server_suites, "PORT_LOCK_PATH", self.lock):
            with self.assertRaises(server_suites.ServerExited):
                with server_suites.Server(args, "journeys", {}):
                    pass
        with open(os.path.join(self.dir, "server-logs", "m__x~journeys.log")) as f:
            self.assertEqual(f.read().count("retrying on new ports"), server_suites.Server.START_ATTEMPTS - 1)

    def test_a_held_port_lock_is_waited_for_only_until_its_timeout(self):
        with open(self.lock, "a") as held, mock.patch.object(server_suites, "PORT_LOCK_PATH", self.lock), \
                contextlib.redirect_stderr(io.StringIO()) as err:
            fcntl.flock(held, fcntl.LOCK_EX)
            t0 = time.monotonic()
            with server_suites.port_reservation(0.5):
                waited = time.monotonic() - t0
        self.assertGreaterEqual(waited, 0.45)
        self.assertLess(waited, 3)
        self.assertIn("going on without it", err.getvalue())


class SlowCloseFile:
    """A file whose writes are split in two and whose close pauses, so threads
    that do not hold the sink's lock across write and record reorder."""

    def __init__(self, path, mode):
        self.f = builtins.open(path, mode)

    def fileno(self):
        return self.f.fileno()

    def write(self, data):
        half = len(data) // 2
        self.f.write(data[:half])
        self.f.flush()
        time.sleep(0.0005)
        self.f.write(data[half:])
        return len(data)

    def flush(self):
        self.f.flush()

    def __enter__(self):
        return self

    def __exit__(self, *a):
        self.f.close()
        time.sleep(random.random() * 0.002)


class SinkConcurrencyTest(unittest.TestCase):
    def test_threads_write_whole_rows_in_the_order_the_sink_records(self):
        with tempfile.TemporaryDirectory() as out:
            sink = server_suites.Sink(out, "m/x", "journeys")

            def writer(t):
                for i in range(25):
                    sink.add(f"case{t}-{i}", t, True, 1.0, "llm", {"blob": "x" * 20000})

            with mock.patch.object(server_suites, "open", SlowCloseFile, create=True), \
                    contextlib.redirect_stderr(io.StringIO()):
                threads = [threading.Thread(target=writer, args=(t,)) for t in range(8)]
                for t in threads:
                    t.start()
                for t in threads:
                    t.join()
            with open(os.path.join(out, "server.jsonl")) as f:
                written = [json.loads(line) for line in f]
        self.assertEqual(len(written), 200)
        self.assertEqual([r["case"] for r in written], [r["case"] for r in sink.rows])

    def run_writers(self, no_lock):
        with tempfile.TemporaryDirectory() as out:
            run_children(out, [{"mode": "sink", "lock": os.path.join(out, "unused.lock"), "no_lock": no_lock,
                                "out": out, "model": f"m/{n}", "rows": 30} for n in range(4)])
            bad, good = 0, 0
            with open(os.path.join(out, "server.jsonl")) as f:
                for line in f:
                    try:
                        json.loads(line)
                        good += 1
                    except ValueError:
                        bad += 1
            return good, bad

    def test_processes_appending_large_rows_never_interleave(self):
        self.assertEqual(self.run_writers(no_lock=False), (120, 0))

    def test_without_the_flock_the_same_writers_interleave(self):
        # The control for the test above.
        good, bad = self.run_writers(no_lock=True)
        self.assertGreater(bad, 0)


if __name__ == "__main__":
    unittest.main()
