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


    def test_a_passing_check_the_candidate_did_not_run_is_lost(self):
        base = rows("agent", a=True, b=True)
        cand = rows("agent", a=True)
        r = fakegate.compare(base, cand, noise=0)
        self.assertEqual(r["agent"]["lost"], [("agent", "journeys_x", "b")])
        self.assertEqual(r["agent"]["missing"], [("agent", "journeys_x", "b")])
        self.assertTrue(r["agent"]["fails"])


class LoadTest(unittest.TestCase):
    def test_rows_are_keyed_by_tier_from_every_model_directory(self):
        with tempfile.TemporaryDirectory() as d:
            for sub, case in (("agent/sonnet", "a"), ("agent/opus", "b"), ("none", "a")):
                os.makedirs(os.path.join(d, sub))
                with open(os.path.join(d, sub, "server.jsonl"), "w") as f:
                    f.write(json.dumps({"suite": "s", "case": case, "ok": True}) + "\nnot json\n")
            self.assertEqual(sorted(fakegate.load(d)), [("agent", "s", "a"), ("agent", "s", "b"), ("none", "s", "a")])


class ReuseTest(unittest.TestCase):
    def test_results_are_reused_only_for_the_same_inputs(self):
        meta = {"binary": "b1", "tree": "t1", "authors": {}, "agent_uncovered": [], "inputs": {}}
        with tempfile.TemporaryDirectory() as d:
            done = os.path.join(d, "done")
            self.assertFalse(fakegate.reusable(done, meta))
            with open(done, "w") as f:
                json.dump(meta, f)
            self.assertTrue(fakegate.reusable(done, meta))
            self.assertFalse(fakegate.reusable(done, dict(meta, tree="t2")))
            self.assertFalse(fakegate.reusable(done, dict(meta, binary="b2")))

    def test_side_meta_names_the_checkout_and_each_description(self):
        """A binary gated again after the base moved runs with another tree, so its old
        results are not reused."""
        cache = os.path.join(fakegate.REPO, world_author.DEFAULT_CACHE)
        binary = os.path.join(fakegate.HERE, "fake_llm.py")
        meta = fakegate.side_meta(fakegate.REPO, binary, ["journeys_blind11"], list(fakegate.DEFAULT_MODELS), cache)
        self.assertEqual(meta["tree"], fakegate.git("rev-parse", "HEAD"))
        self.assertEqual(meta["binary"], world_author.sha256_file(binary))
        entries = meta["inputs"]["journeys_blind11"]
        self.assertEqual(len(entries), 5)
        self.assertTrue(all(contract and description for contract, description in entries.values()))


class RunDirTest(unittest.TestCase):
    def test_other_inputs_get_another_directory(self):
        meta = {"binary": "b", "tree": "t", "authors": {"m": ["s1", "s2"]}, "agent_uncovered": [],
                "inputs": {"s1": {}, "s2": {}}}
        subset = dict(meta, authors={"m": ["s1"]}, inputs={"s1": {}})
        self.assertEqual(fakegate.run_dir("/w", "abc", meta), fakegate.run_dir("/w", "abc", dict(meta)))
        self.assertNotEqual(fakegate.run_dir("/w", "abc", meta), fakegate.run_dir("/w", "abc", subset))

    def test_a_suite_with_no_rows_is_incomplete(self):
        meta = {"authors": {"m": ["s1"]}, "inputs": {"s1": {}, "s2": {}}}
        rows = {("agent", "s1", "c"): {}, ("none", "s1", "c"): {}}
        self.assertEqual(fakegate.incomplete(rows, meta), [("none", "s2")])
        rows[("none", "s2", "c")] = {}
        self.assertEqual(fakegate.incomplete(rows, meta), [])


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
