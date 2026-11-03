"""Tests for the W1 harness fixes: echo-aware grading, fallback causes and
header-safe session ids. Run: python3 -m unittest scripts/llm-bench/test_w1.py"""

import json
import os
import sys
import tempfile
import threading
import unittest
import urllib.parse
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


if __name__ == "__main__":
    unittest.main()
