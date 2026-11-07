"""Unit tests for fakegate.py's comparison and author choice. Run with:
python3 -m unittest discover -s scripts/llm-bench -p 'test_*.py'"""

import json
import os
import sys
import tempfile
import unittest

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))

import fakegate  # noqa: E402
import world_author  # noqa: E402


def rows(tier, **cases):
    return {(tier, "journeys_x", c): {"suite": "journeys_x", "case": c, "ok": ok} for c, ok in cases.items()}


class CompareTest(unittest.TestCase):
    def test_lost_and_gained_are_listed_per_tier(self):
        base = {**rows("agent", a=True, b=False, c=True), **rows("none", a=True)}
        cand = {**rows("agent", a=False, b=True, c=True), **rows("none", a=True)}
        r = fakegate.compare(base, cand, noise=1)
        self.assertEqual(r["agent"]["lost"], [("agent", "journeys_x", "a")])
        self.assertEqual(r["agent"]["gained"], [("agent", "journeys_x", "b")])
        self.assertEqual((r["agent"]["base"], r["agent"]["cand"], r["agent"]["net"]), (2, 2, 0))
        self.assertFalse(r["agent"]["fails"])
        self.assertEqual(r["none"]["net"], 0)

    def test_a_net_loss_of_one_is_noise_and_two_fails(self):
        base = rows("agent", a=True, b=True, c=True)
        one = fakegate.compare(base, rows("agent", a=False, b=True, c=True), noise=1)
        two = fakegate.compare(base, rows("agent", a=False, b=False, c=True), noise=1)
        self.assertFalse(one["agent"]["fails"])
        self.assertTrue(two["agent"]["fails"])
        self.assertTrue(fakegate.compare(base, rows("agent", a=False, b=True, c=True), noise=0)["agent"]["fails"])

    def test_a_check_new_in_the_candidate_counts_as_gained_only_when_it_passes(self):
        base = rows("agent", a=True)
        cand = rows("agent", a=True, new_pass=True, new_fail=False)
        r = fakegate.compare(base, cand)
        self.assertEqual(r["agent"]["gained"], [("agent", "journeys_x", "new_pass")])
        self.assertEqual(r["agent"]["lost"], [])


class AuthorForTest(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.cache = os.path.join(self.tmp.name, "cache")
        self.contracts = []
        for name in ("one.yaml", "two.yaml"):
            path = os.path.join(self.tmp.name, name)
            with open(path, "w") as f:
                f.write(f"openapi: 3.0.0 # {name}\n")
            self.contracts.append(path)

    def tearDown(self):
        self.tmp.cleanup()

    def cache_entry(self, model, contract, **extra):
        sha = world_author.sha256_file(contract)
        path = world_author.cache_path(self.cache, world_author.author_id("claude", model), sha)
        os.makedirs(os.path.dirname(path), exist_ok=True)
        with open(path, "w") as f:
            json.dump(dict({"contract_sha256": sha, "document": {"version": 1}}, **extra), f)

    def test_the_first_model_covering_every_contract_is_taken(self):
        self.cache_entry("sonnet", self.contracts[0])
        for c in self.contracts:
            self.cache_entry("opus", c)
        self.assertEqual(fakegate.author_for(self.contracts, self.cache, ["sonnet", "opus"]), "opus")
        self.cache_entry("sonnet", self.contracts[1])
        self.assertEqual(fakegate.author_for(self.contracts, self.cache, ["sonnet", "opus"]), "sonnet")

    def test_no_model_when_a_contract_has_no_usable_entry(self):
        self.cache_entry("opus", self.contracts[0])
        self.cache_entry("opus", self.contracts[1], is_error=True)
        self.assertIsNone(fakegate.author_for(self.contracts, self.cache, ["opus"]))


class CommittedCacheTest(unittest.TestCase):
    def test_every_gated_suite_has_cached_descriptions(self):
        """The committed world cache covers every suite the gate runs, so its agent tier
        leaves none out."""
        cache = os.path.join(fakegate.REPO, world_author.DEFAULT_CACHE)
        missing = [s for s in fakegate.all_suites(fakegate.REPO)
                   if not fakegate.author_for(fakegate.suite_contracts(fakegate.REPO, s), cache, fakegate.DEFAULT_MODELS)]
        self.assertEqual(missing, [])


if __name__ == "__main__":
    unittest.main()
