"""Tests for the pre-run provider health check (provider_health.py): a run whose pinned
OpenRouter endpoint is degraded refuses to start unless --allow-degraded-provider is
given, a run that cannot fetch the endpoint list warns and goes on, and every run
records the endpoints' status in <out>/providers-status.json.
Offline: a fake endpoint list on 127.0.0.1. Run:
python3 -m unittest scripts/llm-bench/test_provider_health.py"""

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

import provider_health  # noqa: E402
import replay  # noqa: E402
import run  # noqa: E402
import server_suites  # noqa: E402

PINNED = "vendor/pinned-model"
UNPINNED = "vendor/free-model"


def ep(tag, status=0, uptime=100.0, provider="Fireworks", service_tier=None, quantization="unknown"):
    return {"tag": tag, "provider_name": provider, "status": status, "uptime_last_30m": uptime,
            "service_tier": service_tier, "quantization": quantization, "name": f"{provider} | x"}


HEALTHY = [ep("fireworks"), ep("deepinfra/bf16", status=-2, uptime=85.7, provider="DeepInfra")]
DEGRADED = [ep("fireworks", status=-2, uptime=90.0), ep("coreweave/bf16", provider="CoreWeave")]


class FakeOpenRouter:
    """Serves GET /api/v1/models/<id>/endpoints from `lists` (model -> endpoints, or an int status)."""

    def __init__(self, test, lists):
        self.lists = lists
        self.requests = []
        fake = self

        class Handler(BaseHTTPRequestHandler):
            def log_message(self, *a):
                pass

            def do_GET(self):
                fake.requests.append((self.path, dict(self.headers)))
                prefix, suffix = "/api/v1/models/", "/endpoints"
                model = self.path[len(prefix):-len(suffix)] if (
                    self.path.startswith(prefix) and self.path.endswith(suffix)) else None
                found = fake.lists.get(model)
                if found is None or isinstance(found, int):
                    self.send_response(found or 404)
                    self.end_headers()
                    return
                body = json.dumps({"data": {"id": model, "endpoints": found}}).encode()
                self.send_response(200)
                self.send_header("Content-Type", "application/json")
                self.send_header("Content-Length", str(len(body)))
                self.end_headers()
                self.wfile.write(body)

        self.httpd = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        threading.Thread(target=self.httpd.serve_forever, kwargs={"poll_interval": 0.05}, daemon=True).start()
        test.addCleanup(self.httpd.server_close)
        test.addCleanup(self.httpd.shutdown)
        self.api = f"http://127.0.0.1:{self.httpd.server_address[1]}/api/v1"


def write_pins(dirpath, pins):
    path = os.path.join(dirpath, "providers.json")
    with open(path, "w", encoding="utf-8") as f:
        json.dump({"pins": {m: {"provider": p, "basis": "test"} for m, p in pins.items()}}, f)
    return path


FIREWORKS_PIN = {"order": ["fireworks"], "only": ["fireworks"], "allow_fallbacks": False}


class TmpDir(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.mkdtemp(prefix="provider-health-")
        self.addCleanup(shutil.rmtree, self.tmp, True)
        self.out = os.path.join(self.tmp, "out")
        self.pins = write_pins(self.tmp, {PINNED: FIREWORKS_PIN})

    def status_file(self):
        with open(os.path.join(self.out, provider_health.STATUS_FILE), encoding="utf-8") as f:
            return json.load(f)


class MatchingTest(unittest.TestCase):
    def test_base_slug_matches_the_providers_endpoints_but_not_service_tiers(self):
        m = provider_health.pin_entry_matches
        self.assertTrue(m("openai", ep("openai")))
        self.assertTrue(m("azure", ep("azure/us")))
        self.assertFalse(m("openai", ep("openai/flex", service_tier="flex")))
        self.assertFalse(m("openai", ep("openai/fast", service_tier="priority")))
        self.assertFalse(m("fireworks", ep("deepinfra/bf16")))

    def test_full_tag_matches_exactly(self):
        m = provider_health.pin_entry_matches
        self.assertTrue(m("wafer/us", ep("wafer/us")))
        self.assertFalse(m("wafer/us", ep("wafer/mxfp4")))
        self.assertFalse(m("wafer/us", ep("wafer")))

    def test_healthy_needs_status_zero_and_the_uptime_floor(self):
        h = provider_health.endpoint_healthy
        self.assertTrue(h(ep("x", 0, provider_health.MIN_UPTIME_30M)))
        self.assertFalse(h(ep("x", 0, provider_health.MIN_UPTIME_30M - 0.1)))
        self.assertFalse(h(ep("x", -2, 100)))
        self.assertFalse(h(ep("x", 0, None)))

    def test_a_pin_names_only_endpoints_of_its_quantization(self):
        pin = {"order": ["wafer"], "only": ["wafer"], "allow_fallbacks": False, "quantizations": ["mxfp4"]}
        rec = provider_health.assess(PINNED, pin, [ep("wafer/us", quantization="mxfp4"),
                                                   ep("wafer/fp8", status=-2, quantization="fp8")])
        self.assertEqual(rec["verdict"], "healthy")
        self.assertEqual([e["tag"] for e in rec["endpoints"] if e["pinned"]], ["wafer/us"])

    def test_a_pinned_entry_missing_from_the_list_is_degraded(self):
        rec = provider_health.assess(PINNED, FIREWORKS_PIN, [ep("coreweave/bf16")])
        self.assertEqual(rec["verdict"], "degraded")
        self.assertIn("fireworks", rec["problems"][0])


class ServerSuitesGateTest(TmpDir):
    """server_suites.py main: the check runs before any suite."""

    def main(self, fake, *extra, model=PINNED):
        argv = ["server_suites.py", "--model", model, "--proxy-base", "http://127.0.0.1:9", "--out", self.out,
                "--binary", sys.executable, "--suite", "consistency", "--provider-pins", self.pins,
                "--openrouter-api", fake.api] + list(extra)
        ran = []
        err = io.StringIO()
        with mock.patch.object(sys, "argv", argv), \
                mock.patch.object(server_suites, "run_suite", lambda s, a: ran.append(s) or False), \
                mock.patch.object(server_suites, "warn_headroom", lambda a, s: None), \
                contextlib.redirect_stderr(err), self.assertRaises(SystemExit) as cm:
            server_suites.main()
        return cm.exception.code, ran, err.getvalue()

    def test_healthy_starts_and_records(self):
        fake = FakeOpenRouter(self, {PINNED: HEALTHY})
        code, ran, err = self.main(fake)
        self.assertEqual(code, 0)
        self.assertEqual(ran, ["consistency"])
        rec = self.status_file()["models"][PINNED]
        self.assertEqual(rec["verdict"], "healthy")
        pinned = [e for e in rec["endpoints"] if e["pinned"]]
        self.assertEqual([(e["tag"], e["status"], e["uptime_last_30m"]) for e in pinned], [("fireworks", 0, 100.0)])
        self.assertIn("fireworks", err)
        # The endpoint list is public: the key is never sent to it.
        self.assertTrue(fake.requests)
        for _, headers in fake.requests:
            self.assertNotIn("authorization", {k.lower() for k in headers})

    def test_degraded_refuses_with_the_numbers(self):
        fake = FakeOpenRouter(self, {PINNED: DEGRADED})
        code, ran, err = self.main(fake)
        self.assertEqual(code, provider_health.EXIT_PROVIDER_DEGRADED)
        self.assertNotEqual(code, 0)
        self.assertEqual(ran, [])
        for part in (PINNED, "fireworks", "status -2", "90.0%", "--allow-degraded-provider"):
            self.assertIn(part, err)
        self.assertEqual(self.status_file()["models"][PINNED]["verdict"], "degraded")

    def test_degraded_with_the_flag_warns_and_starts(self):
        fake = FakeOpenRouter(self, {PINNED: DEGRADED})
        code, ran, err = self.main(fake, "--allow-degraded-provider")
        self.assertEqual(code, 0)
        self.assertEqual(ran, ["consistency"])
        self.assertIn("warning", err.lower())
        rec = self.status_file()["models"][PINNED]
        self.assertEqual(rec["verdict"], "degraded")
        self.assertTrue(rec["allowed_degraded"])

    def test_a_fetch_failure_warns_and_continues(self):
        fake = FakeOpenRouter(self, {PINNED: 500})
        code, ran, err = self.main(fake)
        self.assertEqual(code, 0)
        self.assertEqual(ran, ["consistency"])
        self.assertIn("warning", err.lower())
        rec = self.status_file()["models"][PINNED]
        self.assertEqual(rec["verdict"], "unknown")
        self.assertTrue(rec["fetch_error"])

    def test_an_unpinned_model_records_its_endpoints_and_starts(self):
        fake = FakeOpenRouter(self, {UNPINNED: DEGRADED})
        code, ran, _ = self.main(fake, model=UNPINNED)
        self.assertEqual(code, 0)
        self.assertEqual(ran, ["consistency"])
        rec = self.status_file()["models"][UNPINNED]
        self.assertEqual(rec["verdict"], "unpinned")
        self.assertEqual(len(rec["endpoints"]), 2)

    def test_a_bench_variant_uses_its_base_models_pin_and_list(self):
        fake = FakeOpenRouter(self, {PINNED: DEGRADED})
        code, ran, _ = self.main(fake, model=PINNED + "@nojson")
        self.assertEqual(code, provider_health.EXIT_PROVIDER_DEGRADED)
        self.assertEqual(ran, [])

    def test_no_provider_check_skips_it(self):
        fake = FakeOpenRouter(self, {PINNED: DEGRADED})
        code, ran, _ = self.main(fake, "--no-provider-check")
        self.assertEqual(code, 0)
        self.assertEqual(ran, ["consistency"])
        self.assertEqual(fake.requests, [])
        self.assertFalse(os.path.exists(os.path.join(self.out, provider_health.STATUS_FILE)))

    def test_records_of_other_models_are_kept(self):
        fake = FakeOpenRouter(self, {PINNED: HEALTHY, UNPINNED: HEALTHY})
        self.main(fake)
        self.main(fake, model=UNPINNED)
        self.assertEqual(sorted(self.status_file()["models"]), sorted([PINNED, UNPINNED]))


class RunFullGateTest(TmpDir):
    """run.py full checks each model just before it starts."""

    def args(self, fake, **kw):
        a = argparse.Namespace(provider_pins=self.pins, openrouter_api=fake.api, allow_degraded_provider=False,
                               no_provider_check=False, upstream=None)
        for k, v in kw.items():
            setattr(a, k, v)
        return a

    def gate(self, args):
        err = io.StringIO()
        with contextlib.redirect_stderr(err):
            why = run.provider_gate(args, self.out, PINNED)
        return why, err.getvalue()

    def test_healthy_passes_and_records(self):
        fake = FakeOpenRouter(self, {PINNED: HEALTHY})
        why, _ = self.gate(self.args(fake))
        self.assertIsNone(why)
        self.assertEqual(self.status_file()["models"][PINNED]["verdict"], "healthy")

    def test_degraded_refuses(self):
        fake = FakeOpenRouter(self, {PINNED: DEGRADED})
        why, err = self.gate(self.args(fake))
        self.assertIn("fireworks", why)
        self.assertIn("status -2", err)

    def test_degraded_with_the_flag_warns(self):
        fake = FakeOpenRouter(self, {PINNED: DEGRADED})
        why, err = self.gate(self.args(fake, allow_degraded_provider=True))
        self.assertIsNone(why)
        self.assertIn("warning", err.lower())

    def test_a_fetch_failure_warns_and_continues(self):
        fake = FakeOpenRouter(self, {})
        why, err = self.gate(self.args(fake))
        self.assertIsNone(why)
        self.assertIn("warning", err.lower())
        self.assertEqual(self.status_file()["models"][PINNED]["verdict"], "unknown")

    def test_a_fake_upstream_skips_the_check(self):
        fake = FakeOpenRouter(self, {PINNED: DEGRADED})
        why, _ = self.gate(self.args(fake, upstream="http://127.0.0.1:9/v1/chat/completions"))
        self.assertIsNone(why)
        self.assertEqual(fake.requests, [])

    def test_the_server_suites_get_the_same_check_settings(self):
        fake = FakeOpenRouter(self, {})
        args = argparse.Namespace(server_suites="s.py", server_jobs=1, world="none", suites="",
                                  provider_pins=self.pins, openrouter_api=fake.api,
                                  allow_degraded_provider=True, no_provider_check=False, upstream=None)
        seen = []

        class Kids:
            def run(self, cmd, env, log):
                seen.append(cmd)
                return 0

        class Proxy:
            base = "http://127.0.0.1:9"

        run.run_server_suites(args, Kids(), Proxy(), PINNED, 1, self.out)
        cmd = seen[0]
        self.assertIn("--allow-degraded-provider", cmd)
        self.assertEqual(cmd[cmd.index("--provider-pins") + 1], self.pins)
        self.assertEqual(cmd[cmd.index("--openrouter-api") + 1], fake.api)


class ReplayGateTest(unittest.TestCase):
    def test_replay_never_checks_providers(self):
        args = argparse.Namespace(binary=sys.executable, jobs=1)
        tmp = tempfile.mkdtemp(prefix="provider-health-replay-")
        self.addCleanup(shutil.rmtree, tmp, True)
        with mock.patch.object(replay.subprocess, "call", return_value=0) as call:
            replay.run_suites("m/x", ["journeys"], "http://127.0.0.1:9", tmp, args, 1)
        self.assertIn("--no-provider-check", call.call_args[0][0])


if __name__ == "__main__":
    unittest.main()
