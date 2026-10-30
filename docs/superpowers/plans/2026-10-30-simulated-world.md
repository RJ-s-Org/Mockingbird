# Simulated World Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** A mock served from a contract alone remembers everything it has said, resolves references between resources, and shows the consequences of writes, so multi-step client journeys hold together.

**Architecture:** Generation stays full-document (the pilot rejected values-only). The contract compiler recognises action routes such as `POST /x/{id}/confirm` as acting on `x`. After each answer the protocol layer stores nested entities and follows references when building prompt context. A write's prompt may ask for an envelope holding the answer and its effects on related resources, which the protocol layer validates and applies. A bench journey suite measures it all.

**Tech Stack:** Rust 1.97 workspace (`mb-import`, `mb-engine`, `mb-protocols`), Python 3 standard library (`scripts/llm-bench`).

**Spec:** `docs/superpowers/specs/2026-10-30-simulated-world-design.md`, including its "Pilot outcome". Evidence: `docs/benchmarks/pilot-2026-10-30/README.md`.

## Global Constraints

- A contract is all a mock needs: no new required configuration, and no new config key in this plan.
- Every answer still matches the contract, on every protocol. No LLM output reaches a client or the store without validation.
- `consistency: off` turns off everything here that stores or reads session state for consistency.
- No `unwrap()`/`expect()` outside tests. Run the AGENTS.md gates before each PR.
- Secrets are references only. Bench runs follow `scripts/llm-bench/README.md`: the key stays in `proxy.py`.
- LLM spend for this plan: at most $1.00 through `proxy.py`'s budget, from the about $2.34 left under the cap.
- `scripts/llm-bench/pilot_world.py` is throwaway. Port what Task 1 needs; never import it.

## Review Focus

1. **The model ignores the envelope** and answers with the plain document: it is accepted with no effects, costing no repair call (Task 5 test).
2. **A resource really has `response` or `effects` members:** its answer is not unwrapped by mistake (Task 5 test).
3. **An LLM effect aimed at a missing resource, another type, an identity field or an invalid value** is rejected and counted, and the answer is still served (Task 5 tests).
4. **A later answer repeats an entity with different values:** the stored entity is not overwritten by a read, and writes keep working (Task 3 test).
5. **A long session:** thousands of stored entities add no more than the existing context limits to a prompt (Task 4 test).

---

### Task 1: Journey suite in the bench, with today's baseline

**Files:**
- Modify: `scripts/llm-bench/server_suites.py` (`SUITES` at line 55, new `run_journeys`)
- Modify: `scripts/llm-bench/test_w1.py`, `scripts/llm-bench/README.md`
- Create: `docs/benchmarks/journeys-baseline-2026-10-30/` (results)

**Interfaces:**
- Produces: suite name `journeys`; `server.jsonl` rows with `case` = `<journey>.<check>`, `ok`, `rep`; a `journey_check(sink, journey, rep, name, ok, detail)` helper.

- [ ] **Step 1: Write failing tests in `test_w1.py`**

`test_journeys_is_a_suite` asserts `"journeys" in server_suites.SUITES`. `test_stripe_journey_scores_a_consistent_run` runs `journey_stripe` against a small in-process fake server implementing the journey correctly and asserts every check passes. `test_stripe_journey_catches_a_lost_status` makes the fake's read return the old status and asserts `read shows the confirmed status` fails.

- [ ] **Step 2: Run them to see them fail**

Run: `python3 -m unittest discover -s scripts/llm-bench -p 'test_*.py'`. Expected: errors naming `journey_stripe`.

- [ ] **Step 3: Port the three journeys into `server_suites.py`**

`journey_stripe`, `journey_travel` and `journey_github`, with the same steps and check names as `pilot_world.py` so results compare with the pilot. Each uses `request()` and a fresh `X-Mockingbird-Session`. A journey that breaks records its remaining checks as failed. Stripe bodies are form-encoded. Mocks have no persona.

- [ ] **Step 4: Run the tests to see them pass**

Same command. Expected: `OK`.

- [ ] **Step 5: Record the baseline**

Build `target/release/mockingbird` from this branch and run the suite for `deepseek/deepseek-v4.1-flash-1017`, `openai/gpt-6-luna` and `nvidia/nemotron-3.5-lightning` with `--repeat 2`, through `proxy.py --prior-ledger docs/benchmarks/pilot-2026-10-30/calls.jsonl`. That ledger counts the pilot's $0.39 toward the budget, so raise `--budget-usd` to 1.39 to leave $1.00 for this plan. Expected: about 56% of checks pass, as in the pilot. If not, find out why before going on.

- [ ] **Step 6: Commit**

`bench: add a multi-step journey suite and today's baseline`

### Task 2: Action routes act on their parent resource

The root of the engine's worst journey failures. `POST /v1/payment_intents/{intent}/confirm` compiles to `Command(Create)` on a resource type called `confirm`, so it invents a new id and never touches the intent.

**Files:**
- Modify: `crates/mb-import/src/heuristics.rs`, `openapi3.rs:793-813`, `swagger2.rs:271-291`
- Test: `crates/mb-import/tests/it/` (the OpenAPI test file), `crates/mb-protocols/tests/it/call.rs`
- Docs: `docs/concepts.md`, `docs/upgrading.md`, `CHANGELOG.md`

**Interfaces:**
- Produces: `pub(crate) struct ActionRoute { resource_type: String, path_param: String }` and `pub(crate) fn action_route(method: HttpMethod, path: &str, all_paths: &[(&str, &[HttpMethod])]) -> Option<ActionRoute>` in `heuristics.rs`.

A route is an action when all of these hold:
- the method is POST;
- the path ends `<static>/{param}/<verb>`, and `<verb>` does not end in `s` (`/refunds` stays a collection);
- the contract has no GET on the same path, and no `<path>/{...}` route.

An action's operation takes `resource_type` from `<static>`, and `identity.path_param` from `{param}`. Its kind is `Command(Effect::Update)` when its success schema equals that of `GET <static>/{param}`, so the answer replaces the resource. Otherwise it is `Command(Effect::Other)`, which stores nothing itself: its target is context, and Task 5's effects change it.

- [ ] **Step 1: Write failing tests**

In `mb-import`: `confirm_acts_on_its_payment_intent` (Stripe shape: type `payment_intents`, `Effect::Update`, path param `intent`). `cancel_with_its_own_response_is_other` (travel: type `bookings`, `Effect::Other`). `a_plural_tail_stays_a_collection` (`POST /v1/customers/{id}/sources`: unchanged). `a_tail_with_a_get_or_detail_route_stays_a_collection` (`/repos/{o}/{r}/issues`: unchanged).

In `mb-protocols/tests/it/call.rs`, with `ScriptedProvider`: `confirm_changes_the_resource_a_read_returns` creates an intent, confirms it with a scripted `status: succeeded`, then reads it and asserts `status == "succeeded"`, the same `id`, and no further LLM call.

- [ ] **Step 2: Run them to see them fail**

Run: `cargo test -p mockingbird-import action; cargo test -p mockingbird-protocols --all-features --test it confirm_changes`. Expected: failures on type `confirm`.

- [ ] **Step 3: Implement `action_route` and use it at both compile sites**

Collect `(path, methods)` before the operations loop. The `update` flow already overlays the stored target, so `id` holds.

- [ ] **Step 4: Run the workspace tests and fix what the change exposes**

Run: `cargo test --workspace --all-features --no-fail-fast`. An existing test that pins the old behaviour for an action route is updated only with a one-line reason in the commit message.

- [ ] **Step 5: Document it and commit**

Concepts: actions in one sentence. Upgrading: a bundle takes this on recompile; re-upload or restart to pick it up. Commit: `feat(import): action routes act on their parent resource`.

### Task 3: Nested entities are remembered

**Files:**
- Modify: `crates/mb-protocols/src/call.rs` (`BundleIndex`, `complete`, `read_context`, generalising `store_wrapped` at line 1207)
- Test: `crates/mb-protocols/tests/it/call.rs`
- Docs: `docs/concepts.md`, `CHANGELOG.md`

**Interfaces:**
- Produces: `BundleIndex::types_by_def: HashMap<String, String>`, a `$defs` name to resource type, from each query operation whose success schema or list items are that `$ref`. `fn entity_type(index, def: Option<&str>, member: &str) -> Option<String>`: the def's type, else the listed type named `member` or `<member>s`.
- Produces: `async fn seed_entities(state, session, rt, body, schema)`: walks an answer with its schema, to depth 6 and at most 100 entities. Each nested object with an `id` or `number` and a known type goes through `insert_seeded`, never `upsert`.

- [ ] **Step 1: Write failing tests**

`a_nested_customer_reads_back` (an order answer holding `customer: {id: "c1", name: "Ana"}`; `GET /customers/c1` returns source `state` and name `Ana`). `search_items_seed_the_detail_route` (GitHub shape: `/search/repositories` items use `$ref` `repository`, and `GET /repos/{o}/{r}` for the first returns the same `stargazers_count`). `a_read_does_not_overwrite_a_stored_entity` (a later answer repeating `c1` with another name leaves `Ana`). `consistency_off_stores_no_entities`. `a_faker_fallback_seeds_nothing`.

- [ ] **Step 2: Run them to see them fail**

Run: `cargo test -p mockingbird-protocols --all-features --test it nested search_items`. Expected: failures on the second read generating anew.

- [ ] **Step 3: Implement**

Call `seed_entities` from `complete` for 2xx answers, under the same conditions as list seeding (`rt.consistency`, not `transient_fallback`). Replace `store_wrapped`'s body with it and keep its `upsert` for a create's own wrapped member.

For a detail route whose trailing parameter names no field, `read_context` also tries `name`, `login`, `slug` and `key` through `by_path_field`, so `/repos/{o}/{r}` finds a repo seeded under its numeric id. When a stored `full_name` exists it must end with `{o}/{r}`, so that two owners' repos of one name don't collide.

- [ ] **Step 4: Run the tests to see them pass**

Same command, then `cargo test -p mockingbird-protocols --all-features`. Expected: all pass.

- [ ] **Step 5: Commit**

`feat(protocols): remember nested entities from answers`

### Task 4: References reach the prompt

**Files:**
- Modify: `crates/mb-protocols/src/call.rs` (`related` at line 1915, `references_in`)
- Test: `crates/mb-protocols/tests/it/call.rs`

**Interfaces:**
- Consumes: `entity_type` from Task 3.
- Produces: `related` also gathers path parameters and, one hop out, the target's own reference fields. New limit `MAX_RELATED_HOPS = 1`. `MAX_REFERENCES` and `prompt::MAX_RELATED` stay as they are.

A reference field is `<x>Id`, `<x>_id`, or a string member named after a resource type (`customer`, `payment_intent`), which is new.

- [ ] **Step 1: Write failing tests**

Assert on the scripted provider's recorded prompt. `a_booking_prompt_holds_its_offer` (body `offerId` of a stored offer). `a_refund_prompt_holds_its_intent` (form body `payment_intent`). `a_read_prompt_holds_what_the_target_refers_to` (one hop: an intent's `customer`). `an_unknown_id_is_generated_knowing_what_refers_to_it` (a stored order names `customer: c9`; `GET /customers/c9` prompt holds that order). `a_long_session_stays_within_the_limits` (2,000 stored resources: the prompt holds at most `MAX_RELATED`).

- [ ] **Step 2: Run them to see them fail**

Run: `cargo test -p mockingbird-protocols --all-features --test it prompt_holds`. Expected: resources missing from prompts.

- [ ] **Step 3: Implement**

The unknown-id case scans at most `MAX_SCAN` resources of the types that have a reference field of the wanted type, per `BundleIndex`, and runs only when the target is missing on a query.

- [ ] **Step 4: Run the tests to see them pass**

Same command, then `cargo test -p mockingbird-protocols --all-features`.

- [ ] **Step 5: Commit**

`feat(protocols): follow references into a call's prompt context`

### Task 5: Writes have effects

**Files:**
- Modify: `crates/mb-engine/src/guards.rs` (`Checks`), `crates/mb-engine/src/lib.rs` (`GeneratedResponse`, `generate`), `crates/mb-engine/src/prompt.rs`
- Modify: `crates/mb-protocols/src/call.rs` (`complete`, new `apply_effects`), `crates/mb-protocols/src/*` (constructors of `GeneratedResponse`)
- Test: `crates/mb-engine/tests/it/generate.rs`, `crates/mb-protocols/tests/it/call.rs`
- Docs: `docs/concepts.md`, `docs/architecture.md`, `docs/operations` (metric), `CHANGELOG.md`

**Interfaces:**
- Consumes: `related` from Task 4; `Effect::Other` targets from Task 2.
- Produces: `pub struct ResourceEffect { pub resource_type: String, pub id: String, pub set: Map<String, Value> }`; `GeneratedResponse::effects: Vec<ResourceEffect>`; `Checks::effects_envelope: bool`; metric `mb_effects_total{mock,outcome}` with `outcome` = `applied` or `rejected`.

The envelope is `{"response": <the answer>, "effects": [{"type": .., "id": .., "set": {..}}]}`. It is asked for only on a command with a non-empty target or `related`, not on streams or joint GraphQL parts. In `json_schema` mode the response format's schema wraps the answer's the same way. It follows the `items_envelope` pattern.

The prompt adds: this call acts on the API; the answer shows the state after it, which usually differs from `<existing_resource>` (an action moves a status on); and effects list what it changes in the other resources shown. The pilot found that models otherwise keep an old status.

- [ ] **Step 1: Write failing engine tests**

`an_envelope_answer_yields_body_and_effects`. `a_plain_answer_is_accepted_without_effects` (source `llm`, one call). `a_resource_with_response_and_effects_members_is_not_unwrapped` (schema declares both). `a_malformed_effect_list_is_ignored` (`"effects": "none"`). `an_oversized_effect_list_is_cut_to_ten`. `no_envelope_is_asked_for_without_context` (a query or a first create: prompt unchanged).

- [ ] **Step 2: Run them to see them fail**

Run: `cargo test -p mockingbird-engine --test it envelope effects`. Expected: failures.

- [ ] **Step 3: Implement in `mb-engine`**

Unwrap in `check_parsed` beside `unwrap_items`, when the object's only non-null members are `response` and `effects` and the validator does not declare them. Validation, patching and repair apply to the inner answer. A repair request repeats the envelope instruction.

- [ ] **Step 4: Write failing protocol tests**

`cancel_shows_on_the_booking` (effect sets `status: cancelled`; a later read returns it with source `state`). `an_effect_on_a_missing_resource_is_rejected`. `an_effect_on_an_identity_field_is_rejected`. `an_effect_breaking_the_schema_is_rejected`. In each rejected case the answer is still served and `rejected` is counted. `an_effect_clears_the_cached_read`. `an_effect_publishes_a_state_change`. `consistency_off_applies_no_effects`. `a_degraded_store_still_serves_the_answer`.

- [ ] **Step 5: Run them to see them fail**

Run: `cargo test -p mockingbird-protocols --all-features --test it effect`. Expected: failures.

- [ ] **Step 6: Implement `apply_effects` in `complete`**

For each effect on a 2xx answer: `state.get(session, type, id)` must find it. No key in `set` may be `id`, `number` or the type's identity field. The merged value must validate against the type's schema, from `BundleIndex` (a query operation of that type). Then `state.upsert`. Effects apply in this session only and can never delete.

- [ ] **Step 7: Run the workspace tests and commit**

Run: `cargo test --workspace --all-features --no-fail-fast`. Commit: `feat: a write's effects change related resources`.

### Task 6: Measure, and decide

**Files:**
- Create: `docs/benchmarks/journeys-2026-10-30/`
- Modify: `docs/benchmarks.md`, `docs/superpowers/specs/2026-10-30-simulated-world-design.md`

- [ ] **Step 1: Run the journey suite on the final branch**

Same models, reps and proxy as Task 1, budget still counted from the ledgers.

- [ ] **Step 2: Check single answers have not regressed**

Replay 72 calls of round 2 per model through mb-evals for the pilot's 6 contracts, and blind-judge 12 targets against round 2's answers, as in the pilot.

- [ ] **Step 3: Compare against the bar**

| Measure | Pass |
|---|---|
| Journey checks passed | At least 75%, from the Task 1 baseline of about 56% |
| "confirm moves the status on" | Not below the baseline's 6/6 by more than one |
| Blind-judged coherence and realism on single answers | Each no more than 0.2 below round 2's |
| Mean prompt tokens on the journeys | No more than 1.5 times the baseline's |

- [ ] **Step 4: Report and commit**

Write the results honestly, including anything that fails the bar, and what it implies for fallback templates and learned invariants. Commit: `bench: simulated-world journeys on full-document generation`.
