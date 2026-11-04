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
import proxy  # noqa: E402
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

    def test_journeys_blind8_is_a_suite(self):
        self.assertIn("journeys_blind8", server_suites.SUITES)
        self.assertNotIn("journeys_blind8", server_suites.DEFAULT_SUITES)
        mocks, runner = server_suites.RUNNERS["journeys_blind8"]
        self.assertIs(runner, server_suites.run_journeys_blind8)
        self.assertEqual(sorted(mocks), ["azuredevops", "businesscentral", "commercetools", "ebay", "meilisearch"])
        self.assertEqual(sorted(server_suites.BLIND8_JOURNEYS), sorted(mocks))
        self.assertTrue(mocks["ebay"][0].endswith("/ebay-trading.wsdl"))
        for contract, prefix, persona in mocks.values():
            self.assertIsNone(persona)
            self.assertTrue(os.path.exists(os.path.join(server_suites.REPO, contract)), contract)
        for name in mocks:
            self.assertEqual(len(server_suites.JOURNEY_CHECKS[name]), 14, name)
            self.assertEqual(len(set(server_suites.JOURNEY_CHECKS[name])), 14, name)

    def test_journeys_blind9_is_a_suite(self):
        self.assertIn("journeys_blind9", server_suites.SUITES)
        self.assertNotIn("journeys_blind9", server_suites.DEFAULT_SUITES)
        mocks, runner = server_suites.RUNNERS["journeys_blind9"]
        self.assertIs(runner, server_suites.run_journeys_blind9)
        self.assertEqual(sorted(mocks), ["cloudinary", "directus", "moderntreasury", "resourcemanager", "xero"])
        self.assertEqual(sorted(server_suites.BLIND9_JOURNEYS), sorted(mocks))
        for name, (contract, prefix, persona) in mocks.items():
            self.assertIsNone(persona)
            self.assertEqual(prefix, f"/{name}")
            self.assertTrue(contract.startswith("scripts/llm-bench/contracts/blind9/"), contract)
            self.assertTrue(os.path.exists(os.path.join(server_suites.REPO, contract)), contract)
        for name in mocks:
            self.assertEqual(len(server_suites.JOURNEY_CHECKS[name]), 14, name)
            self.assertEqual(len(set(server_suites.JOURNEY_CHECKS[name])), 14, name)
        # A blind set's journeys are new: none shares a name with an earlier suite's.
        earlier = set(server_suites.JOURNEYS) | set(server_suites.HELDOUT_JOURNEYS) | set(server_suites.BLIND_JOURNEYS)
        for n in range(2, 9):
            earlier |= set(getattr(server_suites, f"BLIND{n}_MOCKS"))
        self.assertEqual(earlier & set(mocks), set())

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
    def test_each_blind7_to_blind9_journey_records_every_check_on_empty_answers(self):
        # Every check runs and fails on its own: none gates the rest and none raises.
        httpd = ThreadingHTTPServer(("127.0.0.1", 0), FakeEmpty)
        threading.Thread(target=httpd.serve_forever, daemon=True).start()
        self.addCleanup(httpd.server_close)
        self.addCleanup(httpd.shutdown)

        class Srv:
            data = f"http://127.0.0.1:{httpd.server_address[1]}"

        class Args:
            model, timeout, repeat = "m/x", 10, 1

        for suite, journeys in (("journeys_blind7", server_suites.BLIND7_JOURNEYS),
                                ("journeys_blind8", server_suites.BLIND8_JOURNEYS),
                                ("journeys_blind9", server_suites.BLIND9_JOURNEYS)):
            for name, steps in journeys.items():
                self.run_on_empty_answers(Srv, Args, suite, name, steps)

    def run_on_empty_answers(self, Srv, Args, suite, name, steps):
        with tempfile.TemporaryDirectory() as out:
            sink = server_suites.Sink(out, "m/x", suite)
            server_suites.run_one_journey(name, steps, Srv, sink, Args, 0)
        rows = {r["case"]: r for r in sink.rows}
        self.assertEqual(sorted(rows), sorted(f"{name}.{c}" for c in server_suites.JOURNEY_CHECKS[name]), name)
        self.assertEqual([c for c, r in rows.items() if r["source"] == "skipped"], [], name)
        self.assertEqual([c for c, r in rows.items() if r["ok"]], [], name)


class FakeCursorHeader(FakeStripe):
    """Answers every call with an empty JSON array and an `X-After-Cursor` header."""

    def answer(self):
        self.rfile.read(int(self.headers.get("content-length") or 0))
        raw = b"[]"
        self.send_response(200)
        self.send_header("content-type", "application/json")
        self.send_header("x-after-cursor", "cur_2")
        self.send_header("content-length", str(len(raw)))
        self.end_headers()
        self.wfile.write(raw)

    do_GET = answer


class Blind9HelpersTest(unittest.TestCase):
    def test_a_journey_keeps_the_headers_of_the_latest_answer(self):
        httpd = ThreadingHTTPServer(("127.0.0.1", 0), FakeCursorHeader)
        threading.Thread(target=httpd.serve_forever, daemon=True).start()
        self.addCleanup(httpd.server_close)
        self.addCleanup(httpd.shutdown)

        class Srv:
            data = f"http://127.0.0.1:{httpd.server_address[1]}"

        class Args:
            model, timeout, repeat = "m/x", 10, 1

        with tempfile.TemporaryDirectory() as out:
            j = server_suites.Journey("moderntreasury", Srv, server_suites.Sink(out, "m/x", "journeys_blind9"), Args, 0)
            self.assertEqual(j.call("GET", "/api/ledger_transactions"), [])
        self.assertEqual(j.answer_headers.get("X-After-Cursor"), "cur_2")

    def test_the_signature_is_cloudinarys_documented_example(self):
        # From Cloudinary's "Generating authentication signatures" guide.
        params = {"eager": "w_400,h_300,c_pad|w_260,h_200,c_crop", "public_id": "sample_image",
                  "timestamp": "1315060510", "file": "bike.png"}
        signed = server_suites.cloudinary_signed(params, key="1234", secret="abcd")
        self.assertEqual(signed["signature"], "bfd09f95f331f558cbd1320e67aa8d488770583e")
        self.assertEqual(signed["api_key"], "1234")

    def test_a_multipart_body_parses_back_into_its_parts(self):
        from email.parser import BytesParser
        from email.policy import HTTP

        class J:
            rng = random.Random(1)

        png = server_suites.tiny_png()
        kind, body = server_suites.multipart_body(J, {"public_id": "bike_1", "context": "alt=Red bike|caption=x"},
                                                  {"file": ("bike.png", "image/png", png)})
        self.assertTrue(kind.startswith("multipart/form-data; boundary="))
        message = BytesParser(policy=HTTP).parsebytes(f"Content-Type: {kind}\r\n\r\n".encode() + body)
        parts = {p.get_param("name", header="content-disposition"): p for p in message.iter_parts()}
        self.assertEqual(sorted(parts), ["context", "file", "public_id"])
        self.assertEqual(parts["public_id"].get_content().strip(), "bike_1")
        self.assertEqual(parts["file"].get_filename(), "bike.png")
        self.assertEqual(parts["file"].get_content(), png)
        self.assertTrue(png.startswith(b"\x89PNG\r\n\x1a\n") and png.endswith(b"IEND\xaeB`\x82"))


class SoapXmlTest(unittest.TestCase):
    def test_a_request_element_renders_attributes_text_and_children_in_order(self):
        xml = server_suites.xml_element("Item", {"ItemID": "1", "StartPrice": {"@currencyID": "USD", "#text": "19.99"},
                                                 "Title": "A & B", "Include": True, "Fee": ["x", "y"]})
        self.assertEqual(xml, '<Item><ItemID>1</ItemID><StartPrice currencyID="USD">19.99</StartPrice>'
                              "<Title>A &amp; B</Title><Include>true</Include><Fee>x</Fee><Fee>y</Fee></Item>")

    def test_a_response_is_read_by_local_names(self):
        raw = (b'<?xml version="1.0"?><soapenv:Envelope xmlns:soapenv="http://schemas.xmlsoap.org/soap/envelope/">'
               b'<soapenv:Body><GetItemResponse xmlns="urn:ebay:apis:eBLBaseComponents"><Ack>Success</Ack>'
               b'<Item><ItemID>110</ItemID><StartPrice currencyID="USD">24.99</StartPrice></Item>'
               b"</GetItemResponse></soapenv:Body></soapenv:Envelope>")
        answer = server_suites.soap_answer(raw, "GetItemResponse")
        self.assertEqual(server_suites.xml_text(answer, "Ack"), "Success")
        self.assertEqual(server_suites.xml_text(answer, "Item/ItemID"), "110")
        self.assertEqual(server_suites.xml_one(answer, "Item/StartPrice").get("currencyID"), "USD")
        for bad in (b"{}", b"", b"<a>", raw.replace(b"GetItemResponse", b"Other")):
            self.assertIsNone(server_suites.xml_text(server_suites.soap_answer(bad, "GetItemResponse"), "Ack"))


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

    def test_journeys_blind8(self):
        rows = self.assert_same_rows("journeys_blind8")
        self.assertEqual(len(rows), 3 * sum(len(server_suites.JOURNEY_CHECKS[n]) for n in server_suites.BLIND8_JOURNEYS))

    def test_journeys_blind9(self):
        rows = self.assert_same_rows("journeys_blind9")
        self.assertEqual(len(rows), 3 * 5 * 14)
        self.assertEqual({r["case"].split(".")[0] for r in rows}, set(server_suites.BLIND9_JOURNEYS))

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


class FakeFlakySquare(FakeSquare):
    """FakeSquare whose provider fails: the first call of the first session
    (`fail = "once"`) or of every session (`"always"`) is answered with the
    fallback Mockingbird sends when the LLM answered 503."""

    fail = "once"
    reason = "LLM returned HTTP 503: no capacity"
    sessions = []

    def failing(self):
        session = self.headers.get("x-mockingbird-session")
        if session in FakeFlakySquare.sessions:
            return False
        FakeFlakySquare.sessions.append(session)
        return FakeFlakySquare.fail == "always" or len(FakeFlakySquare.sessions) == 1

    def fallback(self):
        self.rfile.read(int(self.headers.get("content-length") or 0))
        raw = b"{}"
        self.send_response(200)
        self.send_header("content-type", "application/json")
        self.send_header("x-mockingbird-source", "fallback")
        self.send_header("x-mockingbird-fallback-reason", FakeFlakySquare.reason)
        self.send_header("content-length", str(len(raw)))
        self.end_headers()
        self.wfile.write(raw)

    def do_POST(self):
        return self.fallback() if self.failing() else super().do_POST()

    def do_GET(self):
        return self.fallback() if self.failing() else super().do_GET()


class FakeDiscord(FakeStripe):
    """A Discord whose guild has only voice and category channels."""

    posted = []

    def do_GET(self):
        path = self.path.split("?")[0]
        gid = path.split("/")[3] if path.startswith("/discord/guilds/") else None
        if path == "/discord/users/@me":
            return self.reply({"id": "42"})
        if path.endswith("/channels"):
            return self.reply([{"id": "7", "type": 2, "guild_id": gid}, {"id": "8", "type": 4, "guild_id": gid}])
        if gid:
            return self.reply({"id": gid, "approximate_member_count": 3})
        self.reply({}, 404)

    def do_POST(self):
        body = json.loads(self.rfile.read(int(self.headers.get("content-length") or 0)) or b"{}")
        FakeDiscord.posted.append(self.path)
        self.reply(dict(body, id=str(900 + len(FakeDiscord.posted)), channel_id=self.path.split("/")[3]))


def serve(handler, test):
    httpd = ThreadingHTTPServer(("127.0.0.1", 0), handler)
    threading.Thread(target=httpd.serve_forever, daemon=True).start()
    test.addCleanup(httpd.server_close)
    test.addCleanup(httpd.shutdown)

    class Srv:
        data = f"http://127.0.0.1:{httpd.server_address[1]}"

    return Srv


class FakeScripted(FakeStripe):
    """Answers `/scripted/<kind>`: `model` is an LLM answer of {}, `infra` the fallback
    sent when the provider answered 503, `timeout` the one sent when the LLM timed out.
    Records each call's session."""

    calls = []
    REASONS = {"infra": "LLM returned HTTP 503: no capacity", "timeout": "LLM request timed out"}

    def do_GET(self):
        kind = self.path.split("?")[0].split("/")[-1]
        FakeScripted.calls.append((self.headers.get("x-mockingbird-session"), kind))
        if kind not in self.REASONS:
            return self.reply({})
        raw = b"{}"
        self.send_response(200)
        self.send_header("content-type", "application/json")
        self.send_header("x-mockingbird-source", "fallback")
        self.send_header("x-mockingbird-fallback-reason", self.REASONS[kind])
        self.send_header("content-length", str(len(raw)))
        self.end_headers()
        self.wfile.write(raw)


def scripted_steps(*kinds):
    """A journey that judges one failing check on each call in `kinds`, the last of
    them a gate, then would go on to `rest`."""
    def steps(j):
        for i, kind in enumerate(kinds):
            j.call("GET", f"/{kind}")
            if i == len(kinds) - 1:
                server_suites.need(j, f"check {i}", False)
            else:
                j.check(f"check {i}", False)
    return steps


class DiscordChannelTest(unittest.TestCase):
    def test_no_check_asks_for_a_text_channel(self):
        checks = server_suites.JOURNEY_CHECKS["discord"]
        self.assertNotIn("guild has a text channel", checks)
        self.assertEqual(len(checks), 13)
        self.assertEqual(len(set(checks)), 13)

    def test_a_guild_without_a_text_channel_still_gets_its_messages(self):
        FakeDiscord.posted = []
        srv = serve(FakeDiscord, self)

        class Args:
            model, timeout, repeat = "m/x", 10, 1

        with tempfile.TemporaryDirectory() as out:
            sink = server_suites.Sink(out, "m/x", "journeys_blind")
            server_suites.run_one_journey("discord", server_suites.discord_steps, srv, sink, Args, 0)
        rows = {r["case"]: r for r in sink.rows}
        self.assertEqual(sorted(rows), sorted(f"discord.{c}" for c in server_suites.JOURNEY_CHECKS["discord"]))
        self.assertTrue(rows["discord.message has an id"]["ok"])
        self.assertTrue(rows["discord.message keeps what was sent"]["ok"])
        self.assertTrue(FakeDiscord.posted)
        # Neither listed channel takes messages, so it posts to a DM channel id it makes up.
        self.assertFalse(any(p.split("/")[3] in ("7", "8") for p in FakeDiscord.posted), FakeDiscord.posted)
        self.assertEqual([c for c, r in rows.items() if r["source"] == "skipped"], [])


class InfraFailureTest(unittest.TestCase):
    """The provider/model split is a label on rows; it never drops, replaces or re-runs one."""

    def run_square(self, fail, out, reason="LLM returned HTTP 503: no capacity"):
        FakeSquare.store, FakeSquare.stale_read = {}, False
        FakeFlakySquare.fail, FakeFlakySquare.reason, FakeFlakySquare.sessions = fail, reason, []
        srv = serve(FakeFlakySquare, self)

        class Args:
            model, timeout, repeat = "m/x", 10, 1

        sink = server_suites.Sink(out, "m/x", "journeys_heldout")
        server_suites.run_one_journey("square", server_suites.square_steps, srv, sink, Args, 0)
        with open(os.path.join(out, "server.jsonl")) as f:
            written = [json.loads(line) for line in f]
        self.assertEqual(written, json.loads(json.dumps(sink.rows)))
        return written

    def square(self, fail, reason="LLM returned HTTP 503: no capacity"):
        with tempfile.TemporaryDirectory() as out:
            return self.run_square(fail, out, reason)

    def test_infra_reason(self):
        def last(reason, source="fallback"):
            return {"source": source, "fallback_reason": reason}

        for reason in ("LLM returned HTTP 503: no capacity", "LLM returned HTTP 502: x",
                       "LLM returned HTTP 429: slow down", "LLM transport error: connection reset"):
            self.assertEqual(server_suites.infra_reason(last(reason)), reason)
        for reason in (
            # A timeout is often the model being slow: it stays the model's.
            "LLM request timed out",
            # The model's answer was cut short, then the repair call failed: the model's.
            "LLM output truncated: stopped at max_tokens; not salvaged: LLM returned HTTP 503: no capacity",
            # The bench proxy after repeated upstream 4xx: the request's failure (424 now, 502 before).
            'LLM returned HTTP 424: {"error":{"message":"upstream kept rejecting the request"}}',
            'LLM returned HTTP 502: {"error":{"message":"upstream kept rejecting the request"}}',
            "LLM returned HTTP 400: bad", "repaired output still invalid", "stopped at max_tokens",
            "timed out waiting for a generation slot", None,
        ):
            self.assertIsNone(server_suites.infra_reason(last(reason)), reason)
        self.assertIsNone(server_suites.infra_reason(last("LLM returned HTTP 503: x", source="llm")))
        # Known limit: a repair call that gets a 503 after an invalid first answer is reported
        # as a bare `LLM returned HTTP 503`, the same as a first call's 503, so it is labelled
        # infra. That is why the label is only a label: it never changes a row or re-runs one.
        self.assertIsNotNone(server_suites.infra_reason(last("LLM returned HTTP 503: repair call")))

    # The reason full8 recorded for DeepSeek's 429s (cut where Mockingbird's header cuts it).
    FULL8_429 = ('LLM returned HTTP 429: {"error":{"message":"Provider returned error","code":429,'
                 '"metadata":{"raw":"deepseek/deepseek-v4.1-flash-1017 is temporarily rate-limited upstream. '
                 'Please retry shortly, or add your own provider key to use your own limits: '
                 'https://openrouter.ai/workspaces/default/byok","provid')

    def test_a_rate_limit_is_the_providers_on_a_structured_signal_only(self):
        def last(reason, source="fallback"):
            return {"source": source, "fallback_reason": reason}

        for reason in (
            self.FULL8_429,
            "LLM returned HTTP 429",
            # Under another status: an error code 429 in the body, or OpenRouter's exact phrase.
            'LLM returned HTTP 400: {"error":{"message":"Provider returned error","code":429}}',
            'LLM returned HTTP 400: {"error":{"code": 429}}',
            'LLM returned HTTP 400: {"error":{"metadata":{"raw":"x is temporarily rate-limited upstream"}}}',
        ):
            self.assertEqual(server_suites.infra_reason(last(reason)), reason)
        for reason in (
            # A 400 whose body only mentions a rate-limit field is the request's.
            'LLM returned HTTP 400: {"error":{"message":"invalid field","param":"rate_limit"}}',
            'LLM returned HTTP 400: {"error":{"message":"Rate limit exceeded: free-models-per-min"}}',
            'LLM returned HTTP 403: {"error":{"code":"rate_limit_exceeded","message":"slow down"}}',
            "LLM returned HTTP 408: Too Many Requests",
            'LLM returned HTTP 400: {"error":{"code":4290}}',
            # The model's own failure that only ends in a rate limit stays the model's.
            "LLM output truncated: stopped at max_tokens; not salvaged: LLM returned HTTP 429: x",
            'repaired output still invalid: {"code":429}',
            "LLM request timed out; temporarily rate-limited upstream",
            # A 4xx without a signal stays the request's, and so does the proxy's 424.
            'LLM returned HTTP 400: {"error":{"message":"Provider returned error"}}',
            'LLM returned HTTP 424: {"error":{"message":"upstream kept rejecting the request","code":429}}',
        ):
            self.assertIsNone(server_suites.infra_reason(last(reason)), reason)
        # A mock answer from the LLM is no fallback: nothing to label.
        self.assertIsNone(server_suites.infra_reason(last(self.FULL8_429, source="llm")))

    def test_a_429_at_the_gate_is_labelled_like_a_503(self):
        rows = self.square("once", reason=self.FULL8_429)
        checks = server_suites.JOURNEY_CHECKS["square"]
        self.assertFalse(any(r["ok"] for r in rows))
        self.assertTrue(all(r["detail"].get("infra") is True for r in rows), rows)
        block = aggregate.journeys_block(rows)["journeys_heldout"]
        self.assertEqual((block["failed_model"], block["failed_timeout"], block["failed_infra"]),
                         (0, 0, len(checks)))

    def test_a_provider_failure_at_the_gate_is_labelled_and_never_rerun(self):
        rows = self.square("once")
        checks = server_suites.JOURNEY_CHECKS["square"]
        self.assertEqual(len(FakeFlakySquare.sessions), 1)  # one run, no re-run
        self.assertEqual(sorted(r["case"] for r in rows), sorted(f"square.{c}" for c in checks))
        self.assertFalse(any(r["ok"] for r in rows))
        self.assertTrue(all(r["detail"].get("infra") is True for r in rows), rows)
        self.assertFalse(any("retried_after" in r["detail"] for r in rows))
        gate = next(r for r in rows if r["case"] == "square.payment has an id")
        self.assertEqual(gate["source"], "fallback")
        self.assertEqual(sum(1 for r in rows if r["source"] == "skipped"), len(checks) - 1)
        block = aggregate.journeys_block(rows + [{"suite": "consistency", "case": "stripe", "ok": False}])
        self.assertEqual(list(block), ["journeys_heldout"])
        self.assertEqual(block["journeys_heldout"], {"n": len(checks), "ok": 0, "ok_rate": 0.0, "failed_model": 0,
                                                     "failed_timeout": 0, "failed_infra": len(checks),
                                                     "failed_infra_cascade": 0})

    def test_rows_are_those_of_a_run_without_infra_but_for_the_label(self):
        labelled = self.square("once")
        plain = self.square("once", reason="repaired output still invalid")  # the model's failure

        def unlabelled(rows):
            out = []
            for r in rows:
                detail = {k: v for k, v in r["detail"].items() if k not in ("infra", "infra_cascade", "fallback_reason")}
                out.append(dict({k: v for k, v in r.items() if k != "latency_ms"}, detail=detail))
            return out

        self.assertFalse(any(r["detail"].get("infra") for r in plain))
        self.assertEqual(unlabelled(labelled), unlabelled(plain))
        a = aggregate.journeys_block(labelled)["journeys_heldout"]
        b = aggregate.journeys_block(plain)["journeys_heldout"]
        self.assertEqual((a["n"], a["ok"], a["ok_rate"]), (b["n"], b["ok"], b["ok_rate"]))  # the label changes no rate
        self.assertEqual((a["failed_model"], a["failed_infra"]), (0, a["n"]))
        self.assertEqual((b["failed_model"], b["failed_infra"]), (b["n"], 0))

    def test_a_timeout_at_the_gate_is_a_model_failure(self):
        rows = self.square("once", reason="LLM request timed out")
        checks = server_suites.JOURNEY_CHECKS["square"]
        self.assertEqual(len(FakeFlakySquare.sessions), 1)
        self.assertFalse(any(r["ok"] or r["detail"].get("infra") for r in rows))
        block = aggregate.journeys_block(rows)["journeys_heldout"]
        self.assertEqual((block["failed_model"], block["failed_timeout"], block["failed_infra"]), (len(checks), 1, 0))

    def test_a_model_failure_at_the_gate_is_not_infra(self):
        with tempfile.TemporaryDirectory() as out:
            sink = server_suites.Sink(out, "m/x", "journeys_heldout")
            srv = serve(FakeEmpty, self)

            class Args:
                model, timeout, repeat = "m/x", 10, 1

            server_suites.run_one_journey("square", server_suites.square_steps, srv, sink, Args, 0)
        self.assertFalse(any(r["ok"] or r["detail"].get("infra") for r in sink.rows))
        block = aggregate.journeys_block(sink.rows)["journeys_heldout"]
        self.assertEqual((block["failed_model"], block["failed_infra"]), (len(sink.rows), 0))

    def run_scripted(self, steps, checks):
        FakeScripted.calls = []
        srv = serve(FakeScripted, self)

        class Args:
            model, timeout, repeat = "m/x", 10, 1

        with mock.patch.dict(server_suites.JOURNEY_CHECKS, {"scripted": checks}):
            with tempfile.TemporaryDirectory() as out:
                sink = server_suites.Sink(out, "m/x", "journeys_heldout")
                server_suites.run_one_journey("scripted", steps, srv, sink, Args, 0)
        rows = {r["case"].split(".", 1)[1]: r for r in sink.rows}
        self.assertEqual(sorted(rows), sorted(checks))
        self.assertEqual(len({session for session, _ in FakeScripted.calls}), 1)  # never re-run
        return rows

    def test_each_row_is_labelled_by_its_own_call(self):
        def gated(*kinds):
            return self.run_scripted(scripted_steps(*kinds), [f"check {i}" for i in range(len(kinds))] + ["rest"])

        rows = gated("model", "infra")
        self.assertFalse(rows["check 0"]["detail"].get("infra"))
        self.assertTrue(rows["check 1"]["detail"].get("infra") and rows["rest"]["detail"].get("infra"))
        rows = gated("infra", "model")
        self.assertTrue(rows["check 0"]["detail"].get("infra"))
        self.assertFalse(rows["check 1"]["detail"].get("infra") or rows["rest"]["detail"].get("infra"))
        rows = gated("infra", "timeout")
        self.assertTrue(rows["check 0"]["detail"].get("infra"))
        self.assertFalse(rows["check 1"]["detail"].get("infra") or rows["rest"]["detail"].get("infra"))

    def test_an_exception_after_an_infra_call_is_the_models(self):
        # `call()` never raises, so an exception comes from reading an answer, which may be
        # an earlier one: the checks it skips are not labelled infra.
        def steps(j):
            first = j.call("GET", "/model")
            j.call("GET", "/infra")
            first["missing"]  # noqa: B018 - reading the earlier answer raises KeyError

        rows = self.run_scripted(steps, ["check 0", "rest"])
        self.assertTrue(all(r["source"] == "skipped" and not r["ok"] for r in rows.values()))
        self.assertFalse(any(r["detail"].get("infra") for r in rows.values()), rows)
        self.assertIn("KeyError", rows["rest"]["detail"]["reason"])
        # The infra call came before the skip, so the skipped checks are a cascade.
        self.assertTrue(all(r["detail"].get("infra_cascade") for r in rows.values()), rows)

    def test_a_failure_after_an_earlier_provider_failure_is_a_cascade(self):
        def steps(j):
            j.call("GET", "/model")
            j.check("before", False)  # no provider failure yet: the model's, unlabelled
            j.call("GET", "/infra")
            j.check("own call", False)  # its own call failed: infra, not a cascade
            j.call("GET", "/model")
            j.check("after, passes", True)  # a pass is never labelled
            j.check("after, fails", False)
            j.call("GET", "/timeout")
            j.check("after, times out", False)
            server_suites.need(j, "gate", False)

        names = ["before", "own call", "after, passes", "after, fails", "after, times out", "gate", "rest"]
        rows = self.run_scripted(steps, names)
        label = {n: (rows[n]["ok"], bool(rows[n]["detail"].get("infra")),
                     bool(rows[n]["detail"].get("infra_cascade"))) for n in names}
        self.assertEqual(label, {
            "before": (False, False, False),
            "own call": (False, True, False),
            "after, passes": (True, False, False),
            "after, fails": (False, False, True),
            "after, times out": (False, False, True),
            "gate": (False, False, True),
            "rest": (False, False, True),  # skipped after a model gate, in a run that had one
        })
        block = aggregate.journeys_block(list(rows.values()))["journeys_heldout"]
        # Raw passes unchanged; cascades stay the model's and are only shown apart. The gate
        # judges the timeout call too, so two timeouts.
        self.assertEqual((block["n"], block["ok"], block["failed_model"], block["failed_timeout"],
                          block["failed_infra"], block["failed_infra_cascade"]), (7, 1, 5, 2, 1, 4))

    def test_no_provider_failure_means_no_cascade(self):
        rows = self.run_scripted(scripted_steps("model", "timeout", "model"),
                                 ["check 0", "check 1", "check 2", "rest"])
        self.assertFalse(any(r["detail"].get("infra_cascade") or r["detail"].get("infra")
                             for r in rows.values()), rows)

    def test_tables_report_the_cascade_next_to_the_raw_passes(self):
        def row(case, ok, **detail):
            return {"model": "m/x", "suite": "journeys_heldout", "case": case, "rep": 0, "ok": ok,
                    "latency_ms": 1.0, "source": "llm", "detail": detail}

        rows = [row("a", True), row("b", False, infra=True), row("c", False, infra_cascade=True),
                row("d", False, infra_cascade=True), row("e", False)]
        with tempfile.TemporaryDirectory() as out:
            with open(os.path.join(out, "server.jsonl"), "w") as f:
                f.writelines(json.dumps(r) + "\n" for r in rows)
            with contextlib.redirect_stderr(io.StringIO()):
                aggregate.main(["--out", out])
            with open(os.path.join(out, "results.json")) as f:
                block = json.load(f)["models"]["m/x"]["server"]["journeys"]["journeys_heldout"]
            with open(os.path.join(out, "tables.md")) as f:
                tables = f.read()
        self.assertEqual((block["ok"], block["failed_model"], block["failed_infra"], block["failed_infra_cascade"]),
                         (1, 3, 1, 2))
        self.assertIn("of which after a provider failure", tables)
        self.assertIn("| m/x | journeys_heldout | 5 | 1 | 3 | 0 | 2 | 1 |", tables)


class FakeRouter(BaseHTTPRequestHandler):
    """A fake OpenRouter chat endpoint: keeps each request body (never headers) and
    answers with `error` (status, message) when set, else served by `served`, as SSE
    when the request asks for a stream."""

    protocol_version = "HTTP/1.1"
    bodies = []
    error = None
    served = "Fireworks"

    def log_message(self, *a):
        pass

    def do_POST(self):
        body = json.loads(self.rfile.read(int(self.headers.get("content-length") or 0)) or b"{}")
        FakeRouter.bodies.append(body)
        if FakeRouter.error:
            status, msg = FakeRouter.error
            doc = {"error": {"code": status, "message": msg}}
        elif body.get("stream"):
            self.send_response(200)
            self.send_header("content-type", "text/event-stream")
            self.send_header("connection", "close")
            self.end_headers()
            self.wfile.write(b": OPENROUTER PROCESSING\n\n")
            for piece, finish in (("{", None), ("}", "stop")):
                chunk = {"id": "gen-fake", "object": "chat.completion.chunk", "provider": FakeRouter.served,
                         "choices": [{"index": 0, "delta": {"content": piece}, "finish_reason": finish}]}
                self.wfile.write(f"data: {json.dumps(chunk)}\n\n".encode())
            self.wfile.write(b"data: [DONE]\n\n")
            self.close_connection = True
            return
        else:
            status, doc = 200, {
                "id": "gen-fake", "object": "chat.completion", "model": body.get("model"),
                "provider": FakeRouter.served,
                "choices": [{"index": 0, "message": {"role": "assistant", "content": "{}"}, "finish_reason": "stop"}],
                "usage": {"prompt_tokens": 3, "completion_tokens": 1, "cost": 0.0},
            }
        raw = json.dumps(doc).encode()
        self.send_response(status)
        self.send_header("content-type", "application/json")
        self.send_header("content-length", str(len(raw)))
        self.end_headers()
        self.wfile.write(raw)


class ProviderPinTest(unittest.TestCase):
    PIN = {"order": ["fireworks"], "allow_fallbacks": False}

    def test_apply_adds_the_pin(self):
        up = {"model": "nvidia/x", "messages": []}
        self.assertEqual(proxy.apply_provider_pin(up, self.PIN), "proxy")
        self.assertEqual(up["provider"], self.PIN)
        up["provider"]["order"].append("other")  # a copy: the configured pin is untouched
        self.assertEqual(self.PIN["order"], ["fireworks"])

    def test_apply_keeps_the_clients_provider(self):
        mine = {"order": ["deepinfra"], "allow_fallbacks": True}
        up = {"model": "nvidia/x", "provider": dict(mine)}
        self.assertEqual(proxy.apply_provider_pin(up, self.PIN), "client")
        self.assertEqual(up["provider"], mine)

    def test_apply_without_a_pin_changes_nothing(self):
        up = {"model": "nvidia/x", "messages": [{"role": "user", "content": "hi"}]}
        before = json.loads(json.dumps(up))
        self.assertIsNone(proxy.apply_provider_pin(up, None))
        self.assertEqual(up, before)

    def test_the_bench_pins_its_three_models_without_fallbacks(self):
        pins = proxy.load_provider_pins(proxy.DEFAULT_PROVIDER_PINS)
        self.assertEqual(pins, {
            "nvidia/nemotron-3.5-lightning": {"order": ["fireworks"], "only": ["fireworks"], "allow_fallbacks": False},
            "openai/gpt-6-luna": {"order": ["openai"], "only": ["openai"], "allow_fallbacks": False},
            "deepseek/deepseek-v4.1-flash-1017": {
                "order": ["wafer/us"], "only": ["wafer/us"], "allow_fallbacks": False,
                "quantizations": ["mxfp4"]},
        })

    # The `quantizations` values OpenRouter documents. It defines no precision order
    # between them, so none is asserted here. `fp4` covers mxfp4 and nvfp4, and `fp8`
    # covers mxfp8, so a filter naming one of those admits more than it names.
    QUANTIZATIONS = {"int4", "int8", "fp4", "mxfp4", "nvfp4", "fp6", "fp8", "mxfp8", "fp16", "bf16", "fp32",
                     "unknown"}
    UMBRELLA = {"fp4", "fp8"}

    def test_every_pin_is_built_the_comparable_way(self):
        # `only` is `order`, no fallbacks, a short list, and a `quantizations` filter (when
        # given) that names documented, known values exactly: no `unknown`, no umbrella value.
        for model, pin in proxy.load_provider_pins(proxy.DEFAULT_PROVIDER_PINS).items():
            self.assertEqual(pin["only"], pin["order"], model)
            self.assertIs(pin["allow_fallbacks"], False, model)
            self.assertLessEqual(len(pin["order"]), 3, model)
            self.assertEqual(len(set(pin["order"])), len(pin["order"]), model)
            q = pin.get("quantizations")
            if q is None:
                self.assertEqual(len(pin["order"]), 1, f"{model}: an ordered list needs a quantizations filter")
                continue
            self.assertIsInstance(q, list, model)
            self.assertTrue(q, model)
            self.assertTrue(set(q) <= self.QUANTIZATIONS, model)
            self.assertNotIn("unknown", q, model)
            self.assertFalse(set(q) & self.UMBRELLA, model)

    def test_preflight_records_the_pins_it_is_given(self):
        d = tempfile.mkdtemp()
        self.addCleanup(shutil.rmtree, d, True)
        pins = os.path.join(d, "pins.json")
        with open(pins, "w") as f:
            json.dump({"pins": {"nvidia/x": {"provider": self.PIN}}}, f)
        args = argparse.Namespace(out=d, models="nvidia/x@nojson,other/y", openrouter_api="http://unused",
                                  no_key=True, strict=False, provider_pins=pins)
        doc = {"data": [{"id": "nvidia/x", "pricing": {}}, {"id": "other/y", "pricing": {}}]}
        with mock.patch.object(run, "get_json", return_value=doc), contextlib.redirect_stdout(io.StringIO()):
            self.assertEqual(run.preflight(args), 0)
        with open(os.path.join(d, "roster.json")) as f:
            roster = {m["id"]: m["provider_pin"] for m in json.load(f)["models"]}
        self.assertEqual(roster, {"nvidia/x@nojson": self.PIN, "other/y": None})
        args.provider_pins = ""
        with mock.patch.object(run, "get_json", return_value=doc), contextlib.redirect_stdout(io.StringIO()):
            run.preflight(args)
        with open(os.path.join(d, "roster.json")) as f:
            self.assertFalse(any(m["provider_pin"] for m in json.load(f)["models"]))

    def test_pin_allows_matches_display_names_and_slugs(self):
        self.assertTrue(proxy.pin_allows({"order": ["io-net"]}, "Io Net"))
        self.assertTrue(proxy.pin_allows({"order": ["deepinfra/bf16"]}, "DeepInfra"))
        self.assertFalse(proxy.pin_allows({"order": ["fireworks"]}, "CoreWeave"))
        self.assertTrue(proxy.pin_allows({"order": ["fireworks"]}, None))


class ProviderPinProxyTest(unittest.TestCase):
    """Calls through the proxy, against a fake OpenRouter (no key, no spend)."""

    PIN = ProviderPinTest.PIN

    def setUp(self):
        FakeRouter.bodies, FakeRouter.error, FakeRouter.served = [], None, "Fireworks"
        up = serve(FakeRouter, self)
        self.dir = tempfile.mkdtemp()
        self.addCleanup(shutil.rmtree, self.dir, True)
        pins = os.path.join(self.dir, "providers.json")
        with open(pins, "w") as f:
            json.dump({"pins": {"nvidia/x": {"provider": self.PIN, "basis": "test"}}}, f)
        self.ledger = os.path.join(self.dir, "calls.jsonl")
        with open(self.ledger, "w") as f:  # an earlier run's row: not this run's provider mix
            f.write(json.dumps({"label": "nvidia__x~test", "provider": "CoreWeave", "cost_usd": 0.0}) + "\n")
        args = argparse.Namespace(
            upstream=f"{up.data}/api/v1/chat/completions",
            no_stream_upgrade=True, dump=None, ledger=self.ledger, budget_usd=5.0, prior_ledger=[],
            prices=None, provider_pins=pins, no_format_fallback=True)
        env = mock.patch.dict(os.environ, {}, clear=False)
        env.start()
        self.addCleanup(env.stop)
        os.environ.pop("OPENROUTER_API_KEY", None)  # the fake upstream never sees a key
        px = proxy.Proxy(args)
        self.addCleanup(px.ledger.fh.close)
        handler = type("PinHandler", (proxy.Handler,), {"proxy": px})
        self.px = px
        self.base = serve(handler, self).data

    def call(self, model, **extra):
        body = dict({"model": model, "messages": [{"role": "user", "content": "hi"}]}, **extra)
        req = urllib.request.Request(f"{self.base}/r/{model.replace('/', '__')}~test/api/v1/chat/completions",
                                     data=json.dumps(body).encode(), headers={"content-type": "application/json"})
        try:
            with urllib.request.urlopen(req, timeout=10) as r:
                raw = r.read()
                status, doc = r.status, (raw.decode() if extra.get("stream") else json.loads(raw))
        except urllib.error.HTTPError as e:
            with e:
                status, doc = e.code, json.loads(e.read() or b"{}")
        with open(self.ledger) as f:
            rec = [json.loads(line) for line in f][-1]
        return status, doc, rec

    def test_a_pinned_model_is_sent_with_its_pin_and_its_provider_is_recorded(self):
        status, doc, rec = self.call("nvidia/x")
        self.assertEqual(status, 200)
        self.assertEqual(FakeRouter.bodies[-1]["provider"], self.PIN)
        self.assertEqual((rec["provider"], rec["provider_pin"], rec["provider_pin_by"]),
                         ("Fireworks", ["fireworks"], "proxy"))
        # A variant of a pinned model is pinned too.
        self.call("nvidia/x@nojson")
        self.assertEqual(FakeRouter.bodies[-1]["provider"], self.PIN)
        self.assertEqual(self.px.ledger.stats()["per_label"]["nvidia__x~test"]["providers"], {"Fireworks": 1})

    def test_the_clients_provider_field_is_not_overridden(self):
        mine = {"order": ["deepinfra"], "allow_fallbacks": True}
        FakeRouter.served = "DeepInfra"
        status, _, rec = self.call("nvidia/x", provider=mine)
        self.assertEqual(status, 200)  # the client's routing is the client's: not enforced
        self.assertEqual(FakeRouter.bodies[-1]["provider"], mine)
        self.assertEqual((rec["provider_pin"], rec["provider_pin_by"]), (["deepinfra"], "client"))

    def test_an_unpinned_model_is_sent_unchanged(self):
        status, _, rec = self.call("other/y")
        self.assertEqual(status, 200)
        self.assertNotIn("provider", FakeRouter.bodies[-1])
        self.assertEqual((rec["provider_pin"], rec["provider_pin_by"]), (None, None))

    def test_an_unavailable_pinned_provider_is_a_provider_failure(self):
        FakeRouter.error = (404, "No allowed providers are available for the selected model.")
        status, doc, rec = self.call("nvidia/x")
        self.assertEqual(status, 503)
        self.assertEqual(len(FakeRouter.bodies), 1)  # not retried elsewhere
        self.assertEqual((rec["status"], rec["error_kind"]), (503, "provider_unavailable"))
        self.assertIn("pinned provider fireworks unavailable", rec["error_excerpt"])
        # Mockingbird reports it as `LLM returned HTTP 503: ...`: the bench's infra label.
        last = {"source": "fallback", "fallback_reason": f"LLM returned HTTP 503: {doc['error']['message']}"}
        self.assertIsNotNone(server_suites.infra_reason(last))
        # Without a pin the same 404 stays the request's own error.
        _, _, rec = self.call("other/y")
        self.assertEqual((rec["status"], rec["error_kind"]), (404, "http_4xx"))
        # OpenRouter's documented routing failure is a 503: a provider error as it is.
        FakeRouter.error = (503, "There is no available model provider that meets your routing requirements")
        status, _, rec = self.call("nvidia/x")
        self.assertEqual((status, rec["error_kind"]), (503, "http_5xx"))

    def test_an_ordered_pin_is_sent_whole_and_takes_any_endpoint_it_lists(self):
        pin = {"order": ["wafer/us", "stepfun/fp8"], "only": ["wafer/us", "stepfun/fp8"],
               "allow_fallbacks": False, "quantizations": ["mxfp4", "mxfp8"]}
        self.px.pins["deepseek/x"] = pin
        for served in ("Wafer", "StepFun"):
            FakeRouter.served = served
            status, _, rec = self.call("deepseek/x")
            self.assertEqual(status, 200, served)
            self.assertEqual(FakeRouter.bodies[-1]["provider"], pin)  # quantizations included
            self.assertEqual((rec["provider"], rec["provider_pin"]), (served, ["wafer/us", "stepfun/fp8"]))
        FakeRouter.served = "Together"
        status, doc, rec = self.call("deepseek/x")
        self.assertEqual((status, rec["error_kind"]), (503, "provider_unavailable"))
        self.assertTrue(rec["provider_pin_mismatch"])

    def test_the_proxy_checks_the_provider_not_the_endpoint(self):
        # An answer names only the provider (`Wafer`), so the proxy matches by base slug:
        # it cannot tell `wafer/us` from `wafer/mxfp4`. Keeping to the endpoint is
        # OpenRouter's routing (`only` with the full tag); the proxy cannot verify it.
        self.assertTrue(proxy.pin_allows({"order": ["wafer/us"]}, "Wafer"))
        self.assertEqual(proxy.provider_key("wafer/us"), proxy.provider_key("wafer/mxfp4"))

    def test_an_upstream_429_reaches_mockingbird_as_a_429(self):
        # As in full8: every pinned endpoint is rate-limited. The proxy passes the 429 on,
        # Mockingbird falls back with `LLM returned HTTP 429: ...`, and that is infra.
        FakeRouter.error = (429, "deepseek/x is temporarily rate-limited upstream. Please retry shortly")
        status, doc, rec = self.call("nvidia/x")
        self.assertEqual((status, rec["status"], rec["error_kind"]), (429, 429, "http_4xx"))
        self.assertEqual(len(FakeRouter.bodies), 1)  # not retried elsewhere
        last = {"source": "fallback", "fallback_reason": f"LLM returned HTTP 429: {json.dumps(doc)}"}
        self.assertIsNotNone(server_suites.infra_reason(last))

    def test_ordinary_request_errors_on_a_pinned_model_stay_4xx(self):
        # mb-llm steps down (schema, reasoning) on these, so they must reach it unchanged.
        for status, msg in (
            (400, "Provider returned error"),
            (400, "This endpoint's maximum context length is 8192 tokens. However, you requested about 9000."),
            (404, "No endpoints found that can handle the requested parameters."),
            (404, "No endpoints found that support the provided 'response_format' parameter."),
        ):
            FakeRouter.error = (status, msg)
            got, doc, rec = self.call("nvidia/x")
            self.assertEqual((got, rec["status"], rec["error_kind"]), (status, status, "http_4xx"), msg)
            self.assertEqual(doc["error"]["message"], msg)

    def test_a_stream_from_another_provider_is_refused_before_anything_is_relayed(self):
        FakeRouter.served = "CoreWeave"
        status, doc, rec = self.call("nvidia/x", stream=True)
        self.assertEqual(status, 503)  # a plain error answer: no 200 head, no chunk went out
        self.assertIn("served by CoreWeave", doc["error"]["message"])
        self.assertEqual((rec["status"], rec["error_kind"], rec.get("provider_pin_mismatch")),
                         (503, "provider_unavailable", True))
        FakeRouter.served = "Fireworks"
        status, raw, rec = self.call("nvidia/x", stream=True)
        self.assertEqual((status, rec["error_kind"], rec["provider"]), (200, None, "Fireworks"))
        self.assertIn(": OPENROUTER PROCESSING", raw)  # held keep-alives are relayed once checked
        self.assertIn("data: [DONE]", raw)

    def test_an_answer_from_another_provider_is_refused(self):
        FakeRouter.served = "CoreWeave"
        status, _, rec = self.call("nvidia/x")
        self.assertEqual(status, 503)
        self.assertEqual((rec["error_kind"], rec["provider"], rec.get("provider_pin_mismatch")),
                         ("provider_unavailable", "CoreWeave", True))

    def test_the_run_summary_reports_the_provider_mix(self):
        stats = {"per_label": {"nvidia__x~engine": {"providers": {"Fireworks": 3, "Io Net": 1}},
                               "nvidia__x~journeys": {"providers": {"Fireworks": 4}},
                               "other__y~engine": {"providers": {"OpenAI": 9}}}}
        mix = run.model_providers(stats, "nvidia/x")
        self.assertEqual(mix, {"Fireworks": 7, "Io Net": 1})
        self.assertEqual(run.provider_mix(mix), "Fireworks 7 (87.5%), Io Net 1 (12.5%)")
        self.assertEqual(run.provider_mix({}), "none recorded")
        self.call("nvidia/x")
        args = argparse.Namespace(model="nvidia/x", proxy_base=self.base)
        self.assertEqual(server_suites.provider_mix(args, "test"), "Fireworks 1 (100.0%)")


class ClientDisconnectTest(unittest.TestCase):
    """A client that hangs up mid-generation (Mockingbird aborting a looping answer)
    stops the proxy reading from the provider; one that hangs up after the answer ended
    does not. Offline: fake_llm streams slowly, no key."""

    CHUNKS, DELAY = 200, 0.02  # the whole stream would take 4 s

    def wait_for(self, what, timeout=10.0):
        end = time.monotonic() + timeout
        while time.monotonic() < end:
            got = what()
            if got:
                return got
            time.sleep(0.02)
        self.fail("timed out waiting")

    def start(self, *fake_args):
        """Starts fake_llm with `fake_args` and an in-process proxy in front of it."""
        import socket
        self.dir = tempfile.mkdtemp()
        self.addCleanup(shutil.rmtree, self.dir, True)
        self.fake_log = os.path.join(self.dir, "fake.jsonl")
        with socket.socket() as s:
            s.bind(("127.0.0.1", 0))
            port = s.getsockname()[1]
        env = {k: v for k, v in os.environ.items() if k != "OPENROUTER_API_KEY"}
        fake = subprocess.Popen(
            [sys.executable, os.path.join(os.path.dirname(os.path.abspath(__file__)), "fake_llm.py"),
             "--port", str(port), "--log", self.fake_log, *fake_args], env=env)
        self.addCleanup(fake.wait, 5)
        self.addCleanup(fake.terminate)

        def up():
            try:
                with socket.create_connection(("127.0.0.1", port), timeout=0.2):
                    return True
            except OSError:
                return False

        self.wait_for(up)
        self.ledger = os.path.join(self.dir, "calls.jsonl")
        args = argparse.Namespace(
            upstream=f"http://127.0.0.1:{port}/api/v1/chat/completions",
            no_stream_upgrade=True, dump=None, ledger=self.ledger, budget_usd=5.0, prior_ledger=[],
            prices=None, provider_pins="", no_format_fallback=True)
        patch = mock.patch.dict(os.environ, {}, clear=False)
        patch.start()
        self.addCleanup(patch.stop)
        os.environ.pop("OPENROUTER_API_KEY", None)  # the fake upstream never sees a key
        self.px = proxy.Proxy(args)
        self.addCleanup(self.px.ledger.fh.close)
        handler = type("DisconnectHandler", (proxy.Handler,), {"proxy": self.px})
        self.port = int(serve(handler, self).data.rsplit(":", 1)[1])

    def rows(self):
        if not os.path.exists(self.ledger):
            return []
        with open(self.ledger) as f:
            return [json.loads(line) for line in f if line.strip()]

    def fake_ends(self):
        if not os.path.exists(self.fake_log):
            return []
        with open(self.fake_log) as f:
            return [r for r in map(json.loads, f) if r.get("stream_end")]

    BODY = json.dumps({"model": "m/x", "stream": True, "max_tokens": 1000,
                       "messages": [{"role": "user", "content": "hi"}]}).encode()

    def hang_up_after(self, marker, count=1):
        """Streams through the proxy, reads until `marker` has been seen `count` times,
        then resets the connection (as a client dropping an aborted stream does)."""
        import socket
        import struct
        sock = socket.create_connection(("127.0.0.1", self.port), timeout=10)
        sock.sendall(b"POST /r/m__x~test/api/v1/chat/completions HTTP/1.1\r\nHost: x\r\n"
                     b"Content-Type: application/json\r\nContent-Length: " + str(len(self.BODY)).encode()
                     + b"\r\n\r\n" + self.BODY)
        seen = b""
        while seen.count(marker) < count:
            part = sock.recv(4096)
            self.assertTrue(part, seen)
            seen += part
        self.assertTrue(seen.startswith(b"HTTP/1.1 200"), seen[:80])
        sock.setsockopt(socket.SOL_SOCKET, socket.SO_LINGER, struct.pack("ii", 1, 0))
        sock.close()
        return seen

    def assert_budget_settled(self, rec):
        st = self.px.ledger.stats()
        self.assertEqual((st["reserved_usd"], st["in_flight"], st["calls"]), (0.0, 0, 1))
        self.assertAlmostEqual(st["ledger_spent_usd"], round(rec["cost_usd"], 6), places=6)

    def test_a_hang_up_mid_generation_stops_the_upstream_read(self):
        self.start("--chunk-delay", str(self.DELAY), "--stream-chunks", str(self.CHUNKS))
        started = time.monotonic()
        self.hang_up_after(b"data: ", 3)
        rec = self.wait_for(self.rows)[-1]
        end = self.wait_for(self.fake_ends)[-1]
        self.assertLess(time.monotonic() - started, self.CHUNKS * self.DELAY / 2)  # well before its end
        # The proxy closed the upstream connection: the provider stopped mid-stream.
        self.assertTrue(end["caller_closed"], end)
        self.assertLess(end["chunks_sent"], self.CHUNKS // 4, end)
        self.assertEqual((rec["status"], rec["error_kind"], rec["client_closed"]), (499, "client_closed", True))
        self.assertGreater(rec["received_chars"], 0)
        self.assertEqual(rec["generation_id"], "fake")  # kept, to look the generation up
        self.assertIsNone(rec["finish_reason"])
        self.assertIsNotNone(rec["ttft_ms"])
        # No usage arrived (it comes last), so the cost is the prompt plus the output seen,
        # far below the reservation for 1000 tokens; the reservation is released.
        self.assertEqual(rec["cost_source"], "estimate")
        self.assertGreater(rec["cost_usd"], 0.0)
        self.assertLess(rec["cost_usd"], self.px.estimate("m/x", json.loads(self.BODY)) / 10)
        self.assert_budget_settled(rec)
        # Neither an error nor a success, wherever calls are counted.
        self.assertFalse(aggregate.is_success(rec))
        self.assertNotIn(rec["error_kind"], aggregate.ERROR_KINDS)
        self.assertFalse(proxy.is_error(rec))
        label = self.px.ledger.stats()["per_label"]["m__x~test"]
        self.assertEqual((label["errors"], label["client_closed"]), (0, 1))
        summary = run.summarize([rec])
        self.assertEqual((summary["errors"], summary["error_kinds"], summary["client_closed"]), (0, [], 1))
        rel = aggregate.ledger_block([rec])["reliability"]
        self.assertEqual((rel["errors"], rel["client_closed"]), (0, 1))

    def assert_recorded_as_usual_after(self, finish, marker):
        # Mockingbird drops the stream right after the chunk that ends the answer. The
        # proxy keeps reading, unrelayed, so the usage that comes after still arrives.
        self.start("--stream-chunks", "3", "--finish", finish, "--tail-delay", "0.2")
        self.hang_up_after(marker)
        rec = self.wait_for(self.rows)[-1]
        end = self.wait_for(self.fake_ends)[-1]
        self.assertFalse(end["caller_closed"], end)  # the proxy read the stream to its end
        self.assertNotIn("client_closed", rec)
        self.assertNotEqual(rec["status"], 499)
        self.assertTrue(rec.get("client_gone_after_end"), rec)
        self.assertEqual(rec["finish_reason"], finish)
        self.assertEqual(rec["cost_source"], "estimate")  # from the usage's tokens (fake sends no cost)
        self.assertIsNotNone(rec["completion_tokens"])  # the usage arrived after the hang-up
        self.assert_budget_settled(rec)
        return rec

    def test_a_hang_up_after_a_length_chunk_is_recorded_as_usual(self):
        rec = self.assert_recorded_as_usual_after("length", b'"finish_reason": "length"')
        self.assertEqual((rec["status"], rec["error_kind"]), (200, None))

    def test_a_hang_up_after_an_in_band_error_is_recorded_as_usual(self):
        rec = self.assert_recorded_as_usual_after("error", b'"finish_reason": "error"')
        # As before this change: the 200 head had gone out, the error went in-stream.
        self.assertEqual((rec["status"], rec["error_kind"]), (200, "stream_error"))

    def test_a_client_that_reads_to_the_end_is_not_marked(self):
        self.start("--stream-chunks", "3")
        req = urllib.request.Request(
            f"http://127.0.0.1:{self.port}/r/m__x~test/api/v1/chat/completions",
            data=self.BODY, headers={"content-type": "application/json"})
        with urllib.request.urlopen(req, timeout=30) as r:
            raw = r.read()
        self.assertIn(b"[DONE]", raw)
        rec = self.wait_for(self.rows)[-1]
        self.assertEqual((rec["status"], rec["error_kind"]), (200, None))
        self.assertNotIn("client_closed", rec)
        self.assertNotIn("client_gone_after_end", rec)
        self.assertFalse(self.wait_for(self.fake_ends)[-1]["caller_closed"])

if __name__ == "__main__":
    unittest.main()
