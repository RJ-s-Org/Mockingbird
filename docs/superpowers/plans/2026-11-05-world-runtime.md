# World Descriptions, Step 3: Runtime Reads the Description (Implementation Plan)

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** The runtime modules that guess today (identity, envelopes, kinds, lifecycle, uniqueness, concurrency, paging, filters, scope) answer from the effective world description first. Each one keeps its heuristic as the fallback wherever the description is silent. Derived counts and sums are computed exactly from stored records. An operation the description leaves open is marked `world_gap` in the request log.

**Architecture:** The effective world is compiled into the runtime once, when the runtime is built, so a request only does hash lookups.
- **Into the bundle.** The entries the bundle already models (kind, type, target, the answer's record pointer, the type's identity member) are written into a copy of the `Bundle` by `mb_mcm::world::apply`. Every heuristic that reads `op.kind` or `op.identity` then follows the description with no change of its own.
- **Into `Described`.** The entries the bundle has no place for (lifecycle, unique, concurrency, paging, filters, scope, order, transition, changes, derived, bulk and long-running records, idempotency, `not_found`) go into a `Described` index on `MockRuntime`. The modules consult it before their heuristic.
- **Hot reload.** A new world revision publishes the same mock change that a scenario change publishes, and the registry rebuilds the runtime with the newest effective world.

**Tech Stack:** Rust workspace (`mb-mcm`, `mb-import`, `mb-engine`, `mb-protocols`, `mb-store`, `mb-admin`, `mockingbird`); serde_json; sqlx (SQLite and Postgres); the integration-test harness in `crates/mb-protocols/tests/it` with `mb_llm::ScriptedProvider`.

**Spec:** `docs/superpowers/specs/2026-11-04-world-description-design.md` (step 3 of "Delivery order"). Evidence for the order and scope: the blind11 diagnosis (journeys_blind11 on the full10 run), which is not in the repo. This plan quotes every number it uses and names every fix by its PR or branch below, so an implementer never needs the diagnosis.

**Base:** `origin/main` with PR #249 (`claude/world-description`, steps 1–2) merged (it is, at `5e7d9028`).

**Prerequisite engine PRs.** The blind11 "engine now" fixes are structural fixes with no description, one PR per group. They run in the same engine workflow, before this plan's tasks that touch the same code:

| Branch / PR | blind11 fixes | What it changes | Tasks that wait for it |
| --- | --- | --- | --- |
| #253 `claude/engine-error-bodies` | 2 (engine-made error bodies, undeclared 404 for a missing record), 3 (API description cap 2000 → 6000) | `mb-engine` refusal bodies, `call/deleted.rs` not-found status | 7 (`not_found` builds on its not-found status) |
| #252 `claude/param-styles-and-lists` | 7 (page parameter with a minimum and no default), 8 (`explode: false` form arrays), 13 (empty list as 204, nested-member filters) | `call/runs.rs`, `call/lists.rs`, `call/pages.rs` | 9 |
| `claude/header-ids-alt-keys` (not opened yet; queued after #252 and #253) | 1 (a create whose id is only in a response header: `-id`/`_id`/`ID` header or `Location`), 5 (a path value that names a record by either of two members) | `write_state` create arm, `mint_create_id`, `by_path_fields` field list, `http_rest/handler.rs` header placement | 5 only (Task 2 adds the two keys' vocabulary and uses none of this PR's code; Tasks 6 and later wait for it only through Task 5) |
| `claude/effect-members` (queued after `claude/header-ids-alt-keys`) | 6 (ack update drops request members), 12 (effect re-homed under `attributes`), 15 (effect id punctuation) | `write_memory.rs` (`named` exclusion, merge onto the held record), `mended_set` | 6 (both edit `write_memory::onto_record`) |

Fixes 9 (echo over a withheld null) and 11 (JSON Patch bodies) have no branch yet and touch no task here. Three blind11 mechanisms are world work in this plan, not engine fixes: 4 (soft delete / archive) is Task 7; 10 (an action route that deletes) is Tasks 1 and 6; 14 (child records from a create's request array, and sums derived from them) is Task 5 (`request.records`) and Task 11 (sums), except a child of a fixed kind (see "Not in this plan").

## Where `call.rs` code lives now

`crates/mb-protocols/src/call.rs` was split into submodules after this plan was written (pure moves, no behaviour change). Where the plan says "`call.rs`" for an item below, edit the file named here. `call.rs` keeps the pipeline itself: `target_id`, `generate_once`, `generate_narrowed`, `generate_pending`, `Prepared`, `prepare`, `serve_held`, `nothing_held`, `member_read`, `complete`, the `mod` lines and `reason_header`. The ordering rule for tasks that edit `call.rs` applies to each of these files the same way.

| File under `crates/mb-protocols/src/call/` | Items the plan names |
| --- | --- |
| `admission.rs` | `plan`, `CallPlan`, `declared_rate_limited`, `PlanError` |
| `effects.rs` | `apply_effects`, `record_effects`, `apply_effect`, `mended_set`, `Place`, `fill_stubs` |
| `creates.rs` | `repeated_record`, `request_values`, `mint_create_id`, `unique_create_id`, `place_new_record` |
| `bundle_index.rs` | `BundleIndex`, `record_schema_of`, `MAX_SCAN`, `entity_type` |
| `session_memory.rs` | `PathAliases`, `OwnReads`, `WrittenTypes`, `next_numbers` |
| `entities.rs` | `store_wrapped`, `seed_own_read`, `seed_entities` |
| `lookup.rs` | `read_context`, `any_owner_holds`, `by_path_fields`, `alternate_members`, `by_alternate_member` |
| `owners.rs` | `Owner`, `Collection`, `owner_of`, `in_create_collection`, `key_fits` |
| `collections.rs` | `Collections` |
| `state_access.rs` | `StateAccess` |
| `relations.rs` | `related`, `follow`, `referrers` |
| `writes.rs` | `WriteOrigin`, `WriteRequest`, `write_state` |
| `reporting.rs`, `streams.rs`, `canonical.rs` | request-log reporting, streams and change feeds, cache keys |
| `tests.rs` | `call.rs`'s former `mod tests` |

## Global Constraints

- **Optional, never required.** A contract alone still works. With no revision, the effective world is tier 0, and every answer stays as it is today (Task 1 and Task 3 prove this).
- **Validated, never trusted.** The runtime reads only the effective world: tier 0 overlaid with the newest revision's entries that validate (`mb_mcm::world::effective`, which both `mb_admin::world::effective_world` and the runtime's `runtime_world` call). It never reads a raw revision.
- **No API names in the engine.** No code path, rule or constant is keyed to an API, a vendor or an operation id. Test contracts are synthetic (folders, contacts, orders, templates).
- **Every answer still matches the contract.** An engine-made value can be a list page, a derived member, a transition state, a minted patterned id, an id header or a conflict body. Each one is checked against the declared schema the way a stored record is (`Engine::serve_stored`) before it is served. A value that does not fit falls back to the heuristic path (generation), never to an invalid answer.
- **A gap never refuses a request.** `world_gap` is a log mark only.
- **Heuristics stay.** Every module falls back to today's inference for each key the description leaves absent, key by key, not operation by operation: a described `paging` does not turn off the heuristic filters (Decision 8, Task 9). No heuristic is deleted in this plan.
- **Repo rules:**
  - no `unwrap()`/`expect()` outside tests;
  - Admin API errors are `application/problem+json`;
  - after an Admin API change, run `npm run gen:api` in `crates/mb-console/web` (no task here changes the Admin API schema);
  - the skills in `plugins/mockingbird/skills` are embedded via `crates/mockingbird/src/agents.rs`.
- **Gates for every task:**
  - `cargo fmt --all --check`
  - `cargo clippy --workspace --all-targets --all-features -- -D warnings`
  - `cargo test --workspace --all-features`
  - `RUSTDOCFLAGS="-D warnings" cargo doc --workspace --all-features --no-deps`
  - `LC_ALL=en_US.UTF-8 scripts/test-postgres.sh` when `mb-store` changes (Task 4)

## Review Focus

1. **Tier 0 must change nothing.** The runtime reads tier 0, which restates the heuristics. Expected: every answer, status and stored record is byte-identical to a runtime built without a world. Tests: Task 1 `apply_of_tier0_is_identity` (every repo contract) and Task 3 `tier0_world_answers_as_no_world`.
2. **An engine-made value that the schema refuses.** Examples: a sum written to a `string` member with a `pattern`, a soft-delete state written to a boolean member, a patterned id over `maxLength`, a list page whose envelope requires a member nothing fills. Expected: the value is coerced to the member's declared type where that is exact; otherwise the answer falls back to generation and is still valid. Tests: Task 7 `a_boolean_soft_delete_state_is_stored_as_true`, Task 10 `a_list_page_whose_envelope_cannot_be_filled_is_generated`, Task 11 `a_sum_into_a_string_member_keeps_the_children_scale`.
3. **A revision written mid-session.** A rebuild resets the runtime's in-memory indexes: `collections`, `path_aliases`, `value_index`, `written_types` and `own_reads` (`mb-protocols/src/lib.rs`). Stored records survive, because they live in the state store. The next request follows the new revision. Two cases need more than that, because an assistant iterating on revisions hits them often:
   - **A described list after a rebuild.** Expected: the next list of a collection the session already listed or wrote is still answered from state. It is not generated and seeded again (`WriteOrigin::Seeded`), so model records are never mixed into a collection that holds real ones. This holds because Task 10 keeps its "listed or written before" mark in the state store, not in `Collections`. The mark also records which records the session placed in the collection, so a nested list (`/parents/{p}/children`) after a rebuild holds that parent's children only, including children whose members do not name the parent (Task 10's membership rule).
   - **A revision that changes a type's identity member or pattern.** Expected: records stored before stay under their old key and nothing is migrated. A read, update or delete by the new member's value still reaches them, because `by_path_fields` scans the type's current identity member (Task 5). A read, update or delete by a bare or a patterned id reaches a record stored under either form, because every direct key read by a path value goes through `records::held_by_key`, which tries the other form on a miss (`records::bare_id`/`patterned`; the four sites are listed in Task 5). New records are stored under the new member.

   Tests: Task 4 `a_world_revision_mid_session_keeps_stored_records`, Task 10 `a_world_revision_mid_session_keeps_lists_from_state` and `a_nested_list_keeps_its_parents_children_across_a_rebuild`, Task 5 `a_revision_that_changes_the_identity_member_still_reaches_earlier_records` and `a_revision_that_adds_a_pattern_still_reaches_bare_keyed_records`.
4. **Client input the description does not cover:**
   - a page token the mock never minted;
   - a page past the end;
   - a filter value of the wrong type (`minAmount=abc`);
   - `If-Match: *`;
   - a write with no concurrency token.

   Expected, in order:
   - the unknown token falls back to today's path (generation);
   - a page past the end is an empty page with no next token;
   - the mistyped filter narrows nothing;
   - `*` and an absent token pass.

   Tests: Task 9 `an_unknown_page_token_falls_back_to_generation`, `a_mistyped_filter_value_narrows_nothing`; Task 8 `if_match_star_and_no_token_pass`.
5. **Operations without HTTP routes** (GraphQL, gRPC, SOAP). Expected: kind, target (by request member), transition and lifecycle work, keyed by operation id. Header-based entries never apply (the validator already refuses them as `not_applicable`). Test: Task 6 `a_described_delete_on_a_graphql_mutation_removes_the_record`.

---

## Decisions this plan settles

1. **How the runtime gets the world: compiled at build, not looked up per request.**
   - `crates/mockingbird/src/runtime.rs::build_runtime_tracked` computes `base = tier0(&contract.bundle)` once, then calls `mb_admin::world::runtime_world(store, mock.id, contract.version, &contract.bundle, &base)` (new, Task 4), then `MockRuntime::with_world(&base, &world, revision)`.
   - `runtime_world` reads only the newest revision (`latest_world_revision`), validates it and overlays it on `base`. It skips `effective_world`'s author attribution (`attribute`, which reads up to `MAX_ATTRIBUTION_REVISIONS` = 64 earlier revisions) and its gap list, which the runtime never uses. A rebuild therefore adds one store read and one way to fail.
   - **A store error never drops the description silently.** The runtime is built with the previous runtime's world when the previous runtime served the same contract version, else with `base` (tier 0, which needs no store, keeps the gaps marked, and changes nothing per Task 1). Its `LoadedWorld` is marked `pending` (Task 4; the flag lives in the registry, not on `MockRuntime`), and the registry retries the build with backoff (1 s doubling to 60 s) until a build loads the world. A retry whose own build fails (the store is usually still down, so `active_contract` or `list_scenarios` fails before the world read) is rescheduled with the next backoff; it never ends the loop. The error is logged with `tracing::warn!`.
   - **A revision whose document does not deserialize as a `World`** is not a store error. `runtime_world` treats it as `effective_world` does: tier 0 at that revision (`(base.clone(), revision)`), with a whole-document refusal logged. It returns `Ok`, so it never starts the retry loop.
   - A request pays only `HashMap` lookups by operation id and type name.
2. **Hot reload by rebuild, as `put_scenario` does.**
   - `add_world_revision` (SQLite and Postgres) publishes `ControlChange::Mock` for the mock when the revision's contract version is the active one. The registry's watcher already rebuilds on that event, and HA bridges carry it to other nodes.
   - This covers every writer: `put_world`, `patch_world`, restore, carry-forward at activation, config-file `world:` and snapshot import.
   - No `ArcSwap` is added. A rebuilt runtime recomputes `BundleIndex` from the applied bundle.
3. **Two places the description lands.**
   - `mb_mcm::world::apply(bundle, base, world) -> Bundle` rewrites each operation where `world` differs from `base` (= `tier0(bundle)`, passed in so it is computed once per build). It compares field by field, so tier 0 is a no-op by construction. The fields are kind, resource type, `identity.path_param`/`input_field`, `identity.field` (from the type's `identity.member`) and `identity.record` (from `answer.record`, a deep pointer allowed).
   - **An absent field keeps the bundle's value.** `apply` writes only a world value that is set (`Some`) and differs from `base`'s; it never writes a `None` **for an absent field**. One set value is written as a `None` in the bundle: `kind: list`. The bundle has no list kind; `OpKind::of` reads a `Query` with `identity: None` as a list. So a set `kind: list` that differs from `base`'s kind clears the op's identity, and this rule wins over "never a `None`", which is about absent fields only (Task 1 test `apply_kind_list_over_a_read_clears_its_identity`). With Task 2's field-wise overlay of `identity`, `target` and `answer`, an author who writes only `answer: {identity_header: X}` keeps tier 0's `answer.record`.
   - **A wrong pointer is corrected by replacing it, not by clearing it.** An author writes the right pointer; `""` (the RFC 6901 root) says the answer itself is the record. The same holds for a wrong identity member or target. A description that says "this answer holds no record at all" is not expressible in v1 (see "Not in this plan").
   - **`""` is new syntax, added by Task 1.** On `main`, `Pointer::check` refuses `""` (`PointerError::Empty`), and so do `validate.rs::check_pointer`, the `world.v1.json` pointer pattern and `mb-admin`'s `bad_pointer`. Task 1 adds a root-allowing check (`Pointer::check_record`) used for `answer.record` and `request.record` only; `answer.records`, `request.records`, `changes.*.from_request` and `long_running` members keep refusing `""`. Task 1 changes the schema, the validator, `bad_pointer` and the spec to match.
   - Everything else goes into `call::described::Described`. Applying the world to the bundle makes kind, target and pointer changes reach all of today's heuristics without editing them.
4. **`tier0` moves to `mb-mcm`** (`mb_mcm::world::tier0`), so `apply` can diff against it without a dependency from `mb-protocols` on `mb-import`. `mb_import::world::tier0` stays as a re-export.
5. **Deep record pointers.** `mb_mcm::RecordAt` gains `Pointer(Vec<String>)`, the RFC 6901 tokens.
   - In the token list, `"0"` means "the one item of an array" when the value there is an array, and a member named `0` otherwise.
   - `apply` writes the three existing variants where they fit (`/m` → `Member`, `/0` → `Item`, `/m/0` → `MemberItem`). So stored bundles and tier 0 never contain the new variant.
6. **How `world_gap` is marked.**
   - `mb_engine::GeneratedResponse` gains `world_gaps: Vec<String>`. Like `effect_outcomes`, it is filled by the caller in `mb-protocols`.
   - The static gaps of an operation come from `world_gaps::gaps(bundle, world, &[])`, computed once in `Described`: `no_identity` for its type, `no_target`, `no_record`.
   - **Seeded where an answer leaves `call.rs`, and in the handlers only for answers that never reach `prepare`** (below). Not every call reaches `call::complete`. On `main`, `prepare` returns `Prepared::Answered` without calling `complete` for answers from state (`serve_held`, `call.rs` ~1028-1094), for refusals (~974) and for `nothing_held` (~919/942). This plan adds two more such answers: Task 8's `replays::replayed` and Task 10's `state_lists::answer`. So `call::seed_world_gaps(rt, op, &mut generated)` (static gaps, `rt.described().gaps(&op.id)` mapped through `GapCode::as_str` to `String`, **extending and deduplicating**, never assigning the field) is called at three points:
     - at the start of `complete`, for every generated answer (the shared path, `generate_pending` and the GraphQL batch join all end there);
     - in `generate_narrowed`'s `Prepared::Answered` arm (`call.rs:713` on `main`), the shared path's prepare-side answers;
     - in `graphql/resolve.rs`'s `call::Prepared::Answered` arm, where it becomes `Route::Answered` (batched root fields; `Route::Alone` goes through `generate_narrowed`).

     Those are the only two callers of `prepare` on `main`, so every answer `prepare` returns is seeded once, whatever task added it.
   - **Answers that never reach `prepare` carry the op's static gaps too.** The static gaps describe the operation, not the answer, so every recorded `GeneratedResponse` of an operation carries them, whatever answered it. HTTP gets this through **one helper**, so no site can be missed: `CallLog::generated` is replaced, for every production caller, by `CallLog::answered(&mut self, rt: &MockRuntime, op: &Operation, generated: GeneratedResponse) -> Response`, which calls `call::seed_world_gaps(rt, op, &mut generated)` and then the old `generated` body. `generated` stays as the private step `answered` ends in, and only the unit tests call it directly. Seeding twice is harmless, because `seed_world_gaps` extends and deduplicates. Every `log.generated` call site in `http_rest/handler.rs` on `main` (`5e7d9028`), and what each becomes:

     | Line on `main` | Answer | Reaches `prepare`/`complete`? | Becomes |
     | --- | --- | --- | --- |
     | ~435 | the declared rate-limit answer, `call::declared_rate_limited` in the `PlanError::RateLimited` arm | no (the plan failed before `prepare`) | `log.answered(rt, op, as_xml(rt, op, generated))` |
     | ~496 | the chaos-injected error in the `default` response's shape, `filled_error` | no (`engine.fallback`, no `prepare`) | `log.answered(rt, op, filled_error(..))` |
     | ~547 | the shared path's answer, `call::generate_once` → `generate_narrowed` | yes (seeded in `complete` or in the `Prepared::Answered` arm) | `log.answered(rt, op, as_xml(rt, op, generated))`; the second seeding adds nothing |
     | ~967 | the declared invalid-input answer, `filled_error` with `Source::Rejected` (in `invalid_input`, which has `rt` and `op`) | no (input validation runs before `prepare`) | `log.answered(rt, op, rejected)` |
     | ~1369, ~1376 | the module's unit tests (`fn generated()` fixture) | — | unchanged (`log.generated`) |

     `stream()` (~605) records no `GeneratedResponse` through `CallLog` (`log.streaming = true`; the SSE stream records its own calls in `http_rest/sse.rs`). Task 3's first step re-runs `git grep -n "log\.generated" crates/mb-protocols/src/http_rest` and converts any production site this table misses.

     Answers that are not a `GeneratedResponse` (`log.problem`, `log.rejected`: the mock's own problem bodies, with no operation answer, among them the native 503 chaos answer at ~472 and every `PlanError` but a declared rate limit) are recorded with no `world_gap`, as they carry no `effects` today. The same rule holds in the other protocols:
     - **SOAP** records two `GeneratedResponse`s without `record_effects` on `main`, so their `world_gap` would be dropped: the declared rate-limit answer (`soap/mod.rs` ~534, passed to `rate_limited_fault`) and `forced_fault`'s generated 500 detail (~623, which went through `generate_once` and so is seeded). Task 3 adds, at ~534, `call::seed_world_gaps(rt, op, &mut generated)` and `call::record_effects(&mut report.request, &generated)` before `rate_limited_fault`, and at ~623 `call::record_effects(&mut report.request, &generated)` after the generation. `soap/mod.rs` joins Task 3's files for these two lines.
     - **gRPC and GraphQL** answer a rate limit and an injected error as a status or a GraphQL error (`grpc/status.rs` ~72, `grpc/service.rs` ~323, `graphql/resolve.rs` ~185/~376), not as a `GeneratedResponse`, so they carry no `world_gap`, as `log.problem` does not. Their generated answers call `record_effects` on the recorded request (`grpc/service.rs` ~441, `graphql/resolve.rs` ~492) after `generate_once`/`complete`, which seeded them.
     - **Streams** (`http_rest/sse.rs`, `graphql/subscription.rs`, `events/ws.rs`): Task 3 Step 3 traces each recorded call to a seeding point; one that reaches none is seeded at its site before recording, and its file joins Task 3's files.
   - **The HTTP request record must keep the field.** gRPC, SOAP and GraphQL call `record_effects` on the request `Value` they record, so a `world_gap` it writes is kept. HTTP does not: on `main`, `CallLog::generated` (`handler.rs` ~170-179) calls `record_effects` on a scratch `recorded` value and keeps only `recorded["effects"]`, and the record's `request` JSON is rebuilt from `log.effects` alone (~658-671). Task 3 therefore owns `handler.rs` for this: `CallLog` gains `world_gap: Value` (default `Null`), `generated` keeps `recorded["world_gap"].take()` next to `effects`, and the request JSON writes `request["world_gap"] = log.world_gap` when it is not `Null`, right after `effects`.
   - The dynamic gap `no_record` is added when a 2xx create or update found no record to store. `write_state` cannot mark it (`WriteRequest.generated` is a shared reference), so it returns a `WriteOutcome { stored, no_record }` and `complete`, which owns the response, pushes the mark after `write_state` and before returning it to the handler (Task 5). Only `no_record: true` marks; it is set by name in the create and update arms where they find no record, and every other return (the `Seeded` path, an update merged onto a held record, a delete, an action, a store refusal) leaves it `false`. Task 5's "`WriteOutcome` per arm" table defines both fields for every return. The push is skipped when the static gaps already hold `no_record`, so the list never has duplicates.
   - `call::record_effects` writes them into the request record's `request` JSON as `"world_gap": ["no_identity", …]`, next to `effects` (for HTTP, through `CallLog.world_gap`, above). A `tracing::info!` line carries `world_gap = …`.
   - No store migration and no Admin API schema change: the request log already stores and serves `request` as JSON.
7. **Derived values are computed on serve, never stored.** A derived member is computed when a record of its type is answered and the computation is exact:
   - the record was created in this session, or at least one child under it is held;
   - the answer is from state, a list from state, or a generated answer about a held record.

   Sums use exact decimal arithmetic, a small `Decimal { mantissa: i128, scale: u32 }` in `call/derived.rs` with no new dependency. The result is written in the member's declared type.
8. **Lists from state.**
   - A list read is answered from state, before any LLM call, when all of these hold:
     - the description gives its operation `paging`, `filters`, `scope` or `order`;
     - the session has listed or written that collection before, by a mark kept in the state store so it survives a rebuild (Task 10);
     - the envelope can be filled.
   - The first list of a collection in a session is still generated and seeded as today, then narrowed by the described query.
   - **Each described key replaces only its own heuristic** (the spec: "an absent key falls back to today's inference"). A description that sets only `paging` keeps today's name-matched query filters; one that sets only `order` keeps today's filters and paging. An empty `filters` list counts as absent. This holds on every path that puts records into a list answer, which Task 9 lists one by one ("Every list path on `main`"): `lists::narrow` (called from `complete`, `pages::serve` and `runs::serve`), `lists::overlay_stored` (same three callers), `lists::stored_rest`/`stored_page` (through `stored_records`), `lists::collection_records` (through `ranges::apply`), the kept-cut pages `pages::serve`/`runs::serve`, and Task 10's `state_lists::answer`. Task 9 defines the rule; Task 7's soft-delete exclusion rides on it.
   - This targets the largest mechanism across blind7-10, "List answers come from the model, not from state": about 175 checks on the default model.
9. **Vocabulary added here** (spec amendment in Task 2):
   - `types.<t>.identity.alternates: [member]`, for a path value that names a record by either of two members;
   - `operations.<op>.answer.identity_header: <header> | false`, for a create that names its id only in a response header. `false` says the create has no identity header: it turns off a header that tier 0's detector guessed wrongly, as `alternates: []` turns off a guessed alias. (`null` reads as absent, so it cannot be the off form.)

   These are the World overrides for blind11 mechanisms 1 (a header-only create id, 132 checks) and 5 (an id-or-alias path value, 36). Their heuristics are `claude/header-ids-alt-keys` (blind11 fixes 1 and 5). The described path and the heuristic path are **one implementation**: the heuristic's detectors produce the header name and the alternate members, tier 0 emits them as `answer.identity_header` (on every create where fix 1 detects a header) and `identity.alternates` (**only on a type for which tier 0 also emits an identity member**; see the rule below), and the runtime reads the header name and the members through one function each (`records::identity_header`, `records::alternates`). Each returns the effective world's value when it is **set**, and the detector's result when it is not. "Not set" covers three cases: no world is attached (`rt.world().is_none()`, Task 3's accessor; this is how the runtime tells it, not `Described::default()`), the op or type has no entry for the key, or (for alternates) the type has no effective identity at all. That last case exists because of **the tier-0 alternates rule**, which every task follows: tier 0 emits `identity.alternates` for a type only when it also emits that type's identity `member`; for a type where fix 5 detects aliases but tier 0's `identities()` finds no member, tier 0 emits no identity entry at all (no member, no alternates), and the type stays heuristic-only (the fix-5 detector is the only source of its alternates) until an author describes a member for it. Tier 0 never emits a member-less identity, and `effective` drops one (Task 2). For a type where fix 5 detects aliases but tier 0's `identities()` finds no member, the detector therefore still answers, under tier 0 and with no world alike, so `tier0_world_answers_as_no_world` holds. An author overrides such a type's aliases only by describing its identity with a member, which Task 2's refusal message already asks for. So each `by_path_fields` site takes fix 5's members from `records::alternates(rt, t)`, in the position fix 5 gave them, not from the detector: a described `alternates` list replaces the detected one entry by entry, and `alternates: []` removes a wrongly guessed alias (Task 5). The same holds for the header: a described `identity_header` replaces the detected one, and `identity_header: false` removes it, so `records::identity_header` is `None` and no id is minted, stored from the request or placed in a header for that op (Task 5). Nothing else is added. (One exception, if fix 5's members turn out per path parameter rather than per type; Task 5's "Coordination cases" says what happens then.)
10. **The request's record pointer is read at runtime.** `request.record` (Task 1 allows `""`) says where a create's or update's request body holds the one record, for a body that wraps it (`{data: {...}}`). `records::request_record(rt, op, body) -> &Value` (Task 5) returns the value there, and the whole body when the effective world sets none (tier 0 emits no `request.record`, so today's behaviour is kept) or the pointer does not resolve in this body. It is used by:
    - create shaping (Task 5): the fix-1 record (the request shaped onto the read schema, plus the id) and every other place a create takes the record from its request take `request_record` instead of the whole body;
    - the update merge (Task 6): the request members that `write_state`'s update arm lays onto the held record (`claude/effect-members`' merge in `write_memory.rs`) come from `request_record`, not the whole body.

    `request.records` (an array of records) stays Task 5's `records::created`. A body where the pointer does not resolve is logged with `tracing::info!`.
11. **Idempotency is never inferred into the world.** Tier 0 does not emit `operations.<op>.idempotency`, and Task 8 adds no such emission. Replay (`replays::replayed`/`keep`) runs only for a header an author described. A contract whose `Idempotency-Key` header only `carries_an_idempotency_key`'s name rule detects behaves as today under tier 0 and with no world: the repeat skips repeat detection and is generated as a new create. So `tier0_world_answers_as_no_world` holds and production answers for undescribed contracts do not change. Replaying on the heuristic header too would be a behaviour change for every such contract, and is not in this plan. `errors`, `masked` and child records of a fixed kind from a create's request array stay out (see "Not in this plan").

## Order, and why

The spec's order is kept: identity → kind/target/transition → lifecycle → unique/concurrency → paging/filters/scope → derived. Two changes come from the evidence:
- **Paging, filters and scope** are the largest mechanism across blind7-10 (about 175 checks). After the plumbing, Task 9 runs **in parallel** with identity (Task 5) and with unique/concurrency (Task 8, about 68), not after lifecycle.
- **Lists from state** (Task 10) needs identity (records are stored), lifecycle (the soft-delete exclusion) and the described query, so it comes late.
- **Soft-deleted records** are excluded from Task 8's unique check by Task 7, which owns `lifecycle::Exclusion`, so Task 8 does not wait for Task 7.
- **Every task that edits `call.rs` runs in sequence**: 3 → 8 → 5 → 6 → 7 → 10 → 11. Task 8 goes before Task 5 because it needs only Task 3, while Task 5 waits for `claude/header-ids-alt-keys`; Task 8 uses that wait. Task 3 also goes after Task 2, because `Described` reads Task 2's `Option<String>` member.
- **Only Task 5 waits for `claude/header-ids-alt-keys`.** Task 2 adds vocabulary only (serde fields, schema, validator, overlay) and uses none of that PR's code, so it depends on Task 1 alone. Tasks 3, 4, 8 and 9 therefore start without the PR; Task 5 (which calls its detectors and has tier 0 emit them) is the one task gated on it, and Task 6 and later wait for it only through Task 5.

"PR" in the Depends column is a prerequisite engine PR from the table under "Base".

| Task | Depends on | Parallel with | Files it owns |
| --- | --- | --- | --- |
| 1 `RecordAt::Pointer`, root pointer `""`, `tier0` move, `apply`, `effective` | — | — | `mb-mcm/src/lib.rs`, `mb-mcm/src/world/{tier0,apply,effective}.rs`, `mb-mcm/src/world/mod.rs` (`mod` lines, `Pointer::check_record`), `mb-mcm/src/world/validate.rs` (`RecordAt` use, root pointer), `mb-mcm/schemas/world.v1.json` (record pointer pattern), `mb-admin/src/world.rs` (`bad_pointer`, `effective_world` calls `effective`), spec and `docs/world.md` (the `""` sentence), `mb-import/src/world.rs`, `mb-import/src/heuristics.rs` (`RecordAt` uses), every `RecordAt` match in `mb-protocols`/`mb-engine` (`mb-engine/src/compact.rs` among them) |
| 2 Vocabulary: `alternates`, `identity_header` (with its `false` form); field-wise overlay; optional `identity.member` | 1 | — | `mb-mcm/src/world/{mod,validate,effective}.rs`, `mb-mcm/src/world/apply.rs` (Task 1's reads of the type's `identity.member`), `mb-mcm/schemas/world.v1.json`, `mb-mcm/src/world/tier0.rs` (`member: Some(..)`), `mb-protocols/src/world_gaps.rs` (`member.as_deref()`), tests that read `IdentityDesc.member` (`mb-import/tests/it/world_tier0.rs`), spec, `docs/world.md` vocabulary table, `mockingbird-world` skill |
| 3 `Described`, `with_world`, gap marking | 1, 2 | — | `mb-protocols/src/call/described.rs`, `mb-protocols/src/lib.rs`, `call.rs` (`record_effects`, `seed_world_gaps`, its call at the start of `complete` and in `generate_narrowed`'s `Prepared::Answered` arm), `graphql/resolve.rs` (one `seed_world_gaps` call in the `Prepared::Answered` arm), `http_rest/handler.rs` (`CallLog.world_gap`, kept in `generated` and written into the request JSON; `CallLog::answered` at the four production `log.generated` sites), `soap/mod.rs` (the rate-limit and `forced_fault` record lines), the `mod` lines in `call.rs` for every new `call/` module of Tasks 3-11 and their stub files, any other protocol site Step 3 finds that records an answer no seeding point sees, `mb-engine/src/lib.rs` (`GeneratedResponse` field), `mb-protocols/tests/it/main.rs` (registers every `described_*` module of Tasks 3-11) and `support.rs` (the harness), plus the stub files of those modules. It edits no `mb-mcm` file. It is written against Task 2's `IdentityDesc.member: Option<String>`. |
| 4 Load at build, hot reload | 3 | 5, 6, 8, 9 | `mb-store/src/{sqlite,postgres}.rs`, `mb-admin/src/world.rs` (`runtime_world`), `mockingbird/src/runtime.rs` (`LoadedWorld`, `build_runtime_tracked`, its wrapper `build_runtime_with_budget`), `mockingbird/src/registry.rs` (`Mounted.world`, `build_mounted` retry and its cancellation, `MockRegistry::world_status`). It edits no `mb-protocols` file: the pending flag lives in `LoadedWorld`, not on `MockRuntime`. |
| 5 Identity and record pointers | 2, 3, 8, PR `claude/header-ids-alt-keys` | 4, 9 | `call/records.rs` (fills Task 3's stub), `mb-mcm/src/world/tier0.rs` (header and alternate emission), `call.rs`: `write_state` (returns `WriteOutcome`; create arm), `complete` (the create-id call site before `write_state` and the `no_record` mark after it), `mint_create_id`, the fix-5 members in the field lists passed to `by_path_fields` (read from `records::alternates`) and the one extra attempt per site, the direct key reads that take `records::held_by_key` (`read_context`'s target read, the `foreign_key` check in `prepare`, `any_owner_holds`' first read, `follow`'s reference read); `mb-engine/src/lib.rs` (`created_id`); `http_rest/handler.rs` header placement; `records::request_record` in create shaping |
| 6 Kind, target, transition, changes | 5, PR `claude/effect-members` | 4, 9 | `call/transitions.rs` (fills Task 3's stub), `call.rs` `write_state` update/action arms, `call/write_memory.rs` (`onto_record` replace; the request merge takes `records::request_record`) |
| 7 Lifecycle, soft delete, `not_found` | 6, 8, 9, PR #253 | — | `call/lifecycle.rs` (fills Task 3's stub), `call/deleted.rs`, `call.rs` `write_state` delete arm and create arm (the `lifecycle::initial` call), `call/list_query.rs` (exclusion in `keeps`, and `of`'s `Some` rule for a lifecycle-only type; `lists.rs` is not edited, because Task 9 already routes every list path through `keeps`), `call/repeats.rs` (exclusion in `described_conflict`) |
| 8 Unique, concurrency, idempotency | 3 | 4, 9 | `call/repeats.rs`, `call/tokens.rs`, `call/replays.rs` (fills Task 3's stub), `call.rs`: `prepare` (refusal and replay call sites), `complete` (the `replays::keep` call site), `repeated_record` (the described idempotency header); `call/value_index.rs` (`same_values`, the same header); `mb-engine/src/guards.rs` (`carries_an_idempotency_key` takes the described header). No `tier0.rs` change (Decision 11). |
| 9 Paging, filters, scope, order | 3, PR #252 | 4, 5, 6, 8 | `call/list_query.rs` (fills Task 3's stub), `call/lists.rs` (`narrow`, and the held-record paths `overlay_stored`, `stored_rest`, `stored_page`, `collection_records`/`stored_records`), `call/runs.rs`, `call/pages.rs`, `call/cursors.rs` |
| 10 Lists answered from state | 7 | — | `call/state_lists.rs` (fills Task 3's stub), `call.rs`: `prepare` list branch, one `#listed` mark call in `complete` (seeding) and in `write_state`'s create arm; `call/lists.rs` (`in_collection`, `filters_of`, `holds_wanted` made `pub(super)`, no other change) |
| 11 Derived counts and sums | 10 | — | `call/derived.rs` (fills Task 3's stub), hooks in `serve_held`, the generated-answer path of a held record, and `state_lists.rs` |
| 12 Docs, changelog, regression run | all | — | `docs/world.md`, `CHANGELOG.md`, `mockingbird-world` skill |

**Parallel tasks touch disjoint files.** Every pair in the Parallel column owns disjoint files; a file two tasks edit is edited by them in sequence, through the Depends column.
- `crates/mb-protocols/src/call.rs` is edited by Tasks 3, 8, 5, 6, 7, 10 and 11, in that order, one at a time. None of them is parallel with another. What each edits there:
  - Task 3: the `mod` lines of every new `call/` module (`described`, `records`, `transitions`, `lifecycle`, `replays`, `list_query`, `state_lists`, `derived`), `record_effects`, the new `seed_world_gaps`, one call to it at the start of `complete` and one in `generate_narrowed`'s `Prepared::Answered` arm;
  - Task 8: `prepare` (two call sites), one call site in `complete` after the answer is settled, and the guard in `repeated_record`;
  - Task 5: `write_state`'s signature (it returns `WriteOutcome`) and create arm, every other `return` in `write_state` (each returns its `WriteOutcome` per the table in Task 5, with no other change), `mint_create_id`, the fix-5 members (read from `records::alternates`) and one appended `by_path_fields` attempt at each of three sites (the update/delete target lookup, `any_owner_holds`, which `read_state` calls, and the parent lookup in `related`), the four direct key reads that become `records::held_by_key` (`read_context`'s target read, which serves reads through `read_state` and is the update/delete key read before its `by_path_fields` attempts; the `foreign_key` check in `prepare`; `any_owner_holds`' first read; `follow`'s reference read), and two call sites in `complete` around the `write_state` call;
  - Task 6: `write_state`'s update and action arms;
  - Task 7: `write_state`'s delete arm and one `lifecycle::initial` call in its create arm;
  - Task 10: `prepare`'s list branch, one `#listed` mark in `complete` and one in `write_state`'s create arm;
  - Task 11: `serve_held` and the generated-answer path of a held record.
- `crates/mb-protocols/src/http_rest/handler.rs` is edited by Task 3 (`CallLog.world_gap`, the request JSON, `CallLog::answered` at four sites), then Task 5 (the identity header placement after `declared_headers`). Task 5 already runs after Task 3, so the order holds through the Depends column.
- `crates/mb-engine/src/lib.rs` is edited by Task 3, then Task 5 (after 3). `mb-mcm/src/world/{tier0,apply,effective,validate,mod}.rs` by Task 1, then Task 2, then Task 5 (`tier0.rs`). `mb-admin/src/world.rs` by Task 1, then Task 4. `call/list_query.rs` by Task 9, then Task 7. `call/repeats.rs` by Task 8, then Task 7. `call/lists.rs` by Task 9, then Task 10 (visibility changes only); Task 7 does not edit it.
- **The integration-test crate's shared files.** `crates/mb-protocols/tests/it/main.rs` and `support.rs` are edited by **Task 3 only**. Task 3 registers every module the later tasks fill (`mod described_runtime; mod described_identity; mod described_kinds; mod described_lifecycle; mod described_conflicts; mod described_lists; mod described_state_lists; mod described_derived;`) and creates each file as a stub holding only its module doc line, so Tasks 5, 6, 8 and 9 (parallel in pairs) each edit only their own module file. A helper a later task needs goes in its own module file; if one is needed by two modules, it is added to `support.rs` by the earlier of the two tasks in the Depends order, never by two parallel tasks. `crates/mockingbird/tests/it/main.rs` is edited by Task 4 only (`mod world_runtime;`).
- The parallel pairs that remain: Task 4 (`mb-store`, `mb-admin/src/world.rs`, `mockingbird/src/{runtime,registry}.rs`) with any of 5, 6, 8 and 9; Task 9 (`call/{list_query,lists,runs,pages,cursors}.rs`) with any of 4, 5, 6 and 8. Neither edits `call.rs`, `mb-engine`, `mb-mcm` or a file of the other.
- **Rebase and merge rule for parallel pairs.** Parallel tasks run on separate branches cut from `main` after their last dependency merged, and merge one at a time, each through its own reviewed PR. The files that would collide if a task added to them are handed out up front so no parallel pair shares one:
  - `call.rs`: neither Task 4 nor Task 9 edits it; Task 9's `mod list_query;` (and every other new `call/` module's `mod` line) is written by Task 3. Tasks 5, 6 and 8, which do edit `call.rs`, are never parallel with each other.
  - `crates/mb-protocols/tests/it/main.rs` and `support.rs`: Task 3 writes every `mod described_*;` line and the stubs; no parallel task touches them.
  - `crates/mockingbird/tests/it/main.rs`: Task 4 only.
  - `Cargo.toml`/`Cargo.lock`: no task adds a dependency.

  The second branch of a pair to merge is rebased onto `main` after the first merges (`git fetch origin && git rebase origin/main`), and its gates are re-run on the rebased tree before it merges, even when the rebase is clean, because the first branch can change behaviour the second's tests rely on. A rebase conflict in a file this section gives to one task means the ownership above is wrong: the implementer stops, reports the file and the two tasks, and the plan is corrected before either side is edited to resolve it. The sequential `call.rs` tasks (3 → 8 → 5 → 6 → 7 → 10 → 11) each branch from `main` after the previous one merged, so they never rebase over each other's unmerged work.

**Coordination.**
- **`claude/header-ids-alt-keys`** (blind11 fixes 1 and 5) lands **before** Task 5, which builds on it (Task 2 names the two keys its detectors find, but needs none of its code, so it does not wait):
  - Fix 1 adds the header-id detector (a declared 2xx response header whose name ends in `-id`/`_id`/`ID`, or `Location` matching the item route) and the create path that mints the id, stores the request body shaped onto the read schema, and echoes the id in the header. Task 5 reuses those functions with the header name as a parameter. It does not write a second `header_id`.
  - Fix 5 adds id-like members that a path value may name. Task 5 assumes they land as extra members in the `fields` list that `by_path_fields` already scans (`call.rs`, `by_path_fields`: `PathAliases` first, then one `MAX_SCAN` listing, first match in field order). If they land as a separate lookup, Task 5's first step folds that lookup into `by_path_fields`' field list before adding described alternates. There is one lookup and one rule.
  - **Fix 5 is asked to land per type**: the id-like members are a function of the type (`fn id_like_members(bundle, t) -> Vec<String>`, from the type's record schema), appended **after** each site's existing fields. That is what lets tier 0 restate them as `types.<t>.identity.alternates`, on the types it gives an identity member (the tier-0 alternates rule, Decision 9); a type with detected aliases and no tier-0 member stays heuristic-only. Task 5's first step writes down, in `records.rs`'s module doc, where fix 5 put its members in each site's list as merged. If fix 5 lands per path parameter instead (for example from `name_fields(param)` or an `idOrAlias`-style parameter name), Task 5 follows "Coordination cases" under Task 5.
  - If `claude/header-ids-alt-keys` puts its bundle-level detectors in `mb-protocols` or `mb-engine`, Task 5 moves them into `mb-mcm` (next to `tier0`), so `tier0` can call them without a dependency cycle.
- **#252** (fixes 7, 8, 13) edits `runs.rs`, `lists.rs` and `pages.rs`, which Task 9 owns. Task 9 starts after #252 merges.
- **#253** (fixes 2, 3) changes the not-found status and body that `deleted.rs` serves. Task 7 starts after #253 merges.
- **`claude/effect-members`** (fixes 6, 12, 15) edits `write_memory.rs`. Task 6's `onto_record` replace starts after it merges.

---

### Task 1: Deep record pointers, `tier0` in `mb-mcm`, and `apply`

**Files:**
- Modify: `crates/mb-mcm/src/lib.rs` (`RecordAt`, `Identity::record_in`, `Identity::answer_holding`)
- Create: `crates/mb-mcm/src/world/tier0.rs` (moved from `crates/mb-import/src/world.rs`), `crates/mb-mcm/src/world/apply.rs`, `crates/mb-mcm/src/world/effective.rs`
- Modify: `crates/mb-mcm/src/world/mod.rs` (`mod tier0; mod apply; mod effective;`, re-exports, and `Pointer::check_record`). Task 1 adds all three `mod` lines, so no later task edits this file's module list.
- **Root pointer `""`** (Decision 3). On `main` it is refused at four places; each changes here:
  - `crates/mb-mcm/src/world/mod.rs`: `Pointer::check_record(s)` accepts `""` and otherwise is `Pointer::check`. `Pointer::is_root()`. `Pointer::check` is unchanged.
  - `crates/mb-mcm/src/world/validate.rs`: `check_pointer` gains `root_ok: bool`. It is `true` only for `answer.record` and `request.record`. A root pointer resolves to the whole answer (or request) schema, which must be an object schema, else `unresolved`.
  - `crates/mb-mcm/schemas/world.v1.json`: a `$defs/recordPointer` with pattern `^$|^(/([^~]|~[01])*)+$`, used by `answer.record` and `request.record` only; the other pointer members keep `$defs/pointer`.
  - `crates/mb-admin/src/world.rs`: `bad_pointer` uses `check_record` for the two `record` members.
  - `docs/superpowers/specs/2026-11-04-world-description-design.md` and `docs/world.md`: one sentence, "`answer.record: \"\"` (and `request.record: \"\"`) says the answer (the request body) is itself the record".
- `crates/mb-mcm/src/world/effective.rs`: `pub fn effective(bundle: &Bundle, base: &World, doc: &World) -> (World, Vec<Refusal>)` = `validate(doc, bundle)` overlaid on `base`. `mb_admin::world::effective_world` calls it in place of its inline validate-and-overlay, so the admin view, the runtime (Task 4) and the test harness (Task 3) share one path. (Task 2 later makes `overlay` field-wise; `effective` picks that up with no change.)
- Modify: `crates/mb-import/src/world.rs` (becomes `pub use mb_mcm::world::tier0;`)
- Modify: every use of `RecordAt`, not only the `match` arms. On `main` they are in:
  - `crates/mb-protocols/src/call.rs` (`record_schema_of` among them);
  - `crates/mb-engine/src/lib.rs` and `crates/mb-engine/src/compact.rs`;
  - `crates/mb-import/src/heuristics.rs` (where the heuristics set `identity.record`) and `crates/mb-import/src/world.rs` (moving);
  - `crates/mb-mcm/src/world/validate.rs` (the `answer.record` check);
  - tests in `crates/mb-import/tests/it/{openapi3,world_tier0}.rs` and `crates/mb-protocols/tests/it/create_repeats.rs`.

  Run `git grep -n RecordAt` at the start of the task. For each use, decide how it treats `Pointer`: a match arm, a `record_in` call, or a deliberate "not a pointer" path. The heuristics never produce `Pointer`; `validate` accepts a deep pointer; `compact` walks it.
- Test: `crates/mb-mcm/src/world/apply.rs` (unit), `crates/mb-import/tests/it/world_tier0.rs` (repo contracts)

**Interfaces:**
- Produces:
  - `mb_mcm::RecordAt::Pointer(Vec<String>)`, serialized as `{"pointer": [..]}`.
  - `mb_mcm::world::record_at(p: &Pointer) -> RecordAt`.
  - `mb_mcm::world::record_pointer(at: &RecordAt) -> Option<Pointer>` (moved from `mb-import` and made public).
  - `mb_mcm::world::tier0(bundle: &Bundle) -> World`.
  - `mb_mcm::world::apply(bundle: &Bundle, base: &World, world: &World) -> Bundle`, where `base` is `tier0(bundle)`, computed once by the caller.
  - `mb_mcm::world::effective(bundle: &Bundle, base: &World, doc: &World) -> (World, Vec<Refusal>)`.
  - `mb_mcm::world::Pointer::{check_record, is_root}`; `record_at` of the root pointer is not called (`apply` maps `""` to `identity.record = None` before it).

- [ ] **Step 1: Write the failing tests**

```rust
// mb-mcm/src/world/apply.rs
#[test] fn record_in_follows_a_deep_pointer() {
    let id = Identity { record: Some(RecordAt::Pointer(vec!["result".into(), "0".into()])), ..identity("id") };
    assert_eq!(id.record_in(&json!({"success": true, "result": [{"id": "a"}]})), Some(&json!({"id": "a"})));
    assert_eq!(id.record_in(&json!({"result": [{"id": "a"}, {"id": "b"}]})), None);
}
#[test] fn record_at_keeps_the_three_shallow_forms() {
    assert_eq!(record_at(&Pointer::parse("/m").unwrap()), RecordAt::Member("m".into()));
    assert_eq!(record_at(&Pointer::parse("/0").unwrap()), RecordAt::Item);
    assert_eq!(record_at(&Pointer::parse("/m/0").unwrap()), RecordAt::MemberItem("m".into()));
    assert_eq!(record_at(&Pointer::parse("/a/b").unwrap()), RecordAt::Pointer(vec!["a".into(), "b".into()]));
}
#[test] fn apply_rewrites_an_action_described_as_delete() {
    // POST /workspaces/{id}/actions/safe-delete: Command(Other) in the bundle.
    let b = bundle_with_action();
    let w = world(json!({"operations": {"safeDelete": {"kind": "delete", "target": {"path_param": "id"}}}}));
    let op = apply(&b, &tier0(&b), &w).operation("safeDelete").cloned().unwrap();
    assert_eq!(op.kind, OperationKind::Command(Effect::Delete));
    assert_eq!(op.identity.unwrap().path_param.as_deref(), Some("id"));
}
#[test] fn apply_sets_identity_field_from_the_type_only_where_it_differs_from_tier0() { /* type identity `name`, ops keep their own path_field */ }
#[test] fn apply_replace_is_an_update() { /* kind replace → Command(Update) */ }
#[test] fn an_absent_answer_record_keeps_the_bundles_pointer() {
    // tier0 has answer.record "/data" for createZone; the world passed to apply has
    // createZone.answer = {records: None, record: None, long_running: Some(..)}.
    // The applied op keeps identity.record == Member("data"). (Task 2 adds the overlay half of this.)
}
#[test] fn the_root_pointer_says_the_answer_is_the_record() {
    // Through effective, as the runtime reads it: doc answer.record "" on createZone, where tier0 had "/data".
    // effective(&b, &base, &doc) refuses nothing; apply gives identity.record None (was Member("data")).
}
#[test] fn the_root_pointer_is_refused_where_it_is_not_a_record() {
    // answer.records "", request.records "", changes.x.from_request "": each refused invalid, as on main.
}
#[test] fn the_root_pointer_needs_an_object_answer() { /* answer.record "" on an op whose 2xx answer is an array: refused unresolved */ }
#[test] fn a_kind_only_read_becomes_a_read() {
    // listThings (Query, identity None) with world {kind: read} and no target:
    // identity Some { field: type member else "id", path_param: None, input_field: None, .. }; OpKind::of(op) == Read.
}
#[test] fn apply_kind_list_over_a_read_clears_its_identity() {
    // getBundle: Query with Identity { field: "id", path_param: "id", .. } in the bundle (tier0 kind read);
    // world {kind: list}: identity None, OpKind::of(op) == List. A world {kind: list, target: {path_param: id}} gives the same.
}
#[test] fn a_kind_only_delete_takes_the_effective_target() { /* action with tier0 target.path_param "id", world kind delete: identity path_param "id" */ }

// mb-mcm/tests/it/world_schema.rs
#[test] fn schema_accepts_a_root_answer_record_only() { /* answer.record "": valid; answer.records "": invalid */ }

// mb-admin/tests/it/world.rs
#[tokio::test] async fn put_world_accepts_a_root_answer_record() { /* 200, effective view shows answer.record "" with no refusal */ }
#[test] fn a_described_type_identity_gives_identities_to_ops_that_had_none() {
    // type widget: no identity in tier0; world identity.member "label".
    // createWidget (Create, identity None) → Identity { field: "label", path_param: None, input_field: None, path_field: None, record: None }
    // getWidget (Read by {label}) → field "label", path_param "label"; listWidgets keeps identity None.
}
#[test] fn a_described_target_gives_an_identity_to_an_action_that_had_none() { /* action with target.member "id": Identity { field: type member else "id", input_field: "id" } */ }

// mb-import/tests/it/world_tier0.rs
#[test] fn apply_of_tier0_is_identity() {
    for (path, b) in bundles() {
        let base = tier0(&b);
        assert_eq!(mb_mcm::world::apply(&b, &base, &base), b, "{}", path.display());
    }
}
#[test] fn apply_writes_nothing_for_a_world_with_no_entries() {
    // overlay(tier0, World::default()) applied: equal to the bundle, for every repo contract.
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p mb-mcm world::apply && cargo test -p mb-import --test it world_tier0::apply_of_tier0_is_identity`
Expected: FAIL (`RecordAt::Pointer`, `apply`, `effective` and `record_at` are not defined; `""` is refused).

- [ ] **Step 3: Implement**
  - Add `RecordAt::Pointer`. `record_in` walks the tokens: an object takes the member; an array at token `"0"` must hold exactly one object. `answer_holding` builds the same path.
  - Move `tier0`, `identities` and `record_pointer` verbatim into `world/tier0.rs`.
  - `apply` clones the bundle and takes `base` from the caller. For each operation, each field (`kind`, `type`, `target.path_param`, `target.member`, `answer.record`) is written in only when the world's value is set and differs from `base`'s. **An absent (`None`) world value means "keep the bundle's value"; `apply` never writes a `None` for an absent field.**
    - kind: `Create`/`Update`/`Replace`/`Delete`/`Action` → `Command(Create/Update/Update/Delete/Other)`; `Read` → `Query` with an identity; `List` → `Query` with `identity: None`.
    - **`kind: list` is the one set value that clears.** When the world sets `list` and `base`'s kind is not `list`, `apply` sets the op's `identity` to `None`, because that is how the bundle spells a list (`OpKind::of`). This wins over the "never a `None`" rule, which covers absent fields only (Decision 3). The op's `target` and `answer.record` are then not written (a list has no target and no single record); its `answer.records` stays a `Described` entry. A tier-0 world never sets a differing kind, so this never fires under tier 0.
    - `answer.record`: `""` → `identity.record = None` (the answer is the record); `/m`, `/0`, `/m/0` → the shallow variants; anything deeper → `Pointer`.
  - **Operations whose `identity` is `None`.** This is the usual case for a create, and for a type tier 0 gave no identity. `apply` creates an `Identity` for an operation of kind create, read, update, replace, delete or action (never list or stream) when its identity is `None` and either:
    - the world sets a type `identity.member` that differs from `base`'s (a type with no identity in tier 0 gets one), or
    - the world sets a `target` or `answer.record` for the operation that differs from `base`'s, or
    - the world sets a `kind` that differs from `base`'s and is read, update, replace, delete or action. Without this, a kind-only `read` on an op the bundle holds as `Query` with no identity would stay a `List` under `OpKind::of`.

    The new identity is `Identity { field, path_param, input_field, path_field: None, record }`:
    - `field` = the type's effective identity member, else `"id"`;
    - `path_param` = the effective `target.path_param` (a create has none unless it names its id in its path);
    - `input_field` = the effective `target.member`;
    - `record` = the effective `answer.record`, mapped as above.

    "Effective" is the world's value when set, else `base`'s, else `None`. Every input is a diff against `base`, so a tier-0 world creates nothing.

    An identity made this way with neither `path_param` nor `input_field` (a kind-only read with no target anywhere) names no target id. Task 1 checks that `target_id` returns `None` for it, so the read is generated as a read of an unheld id is today, and the op keeps its static `no_target` gap (`world_gaps::gaps` finds no target). This is the expected outcome; the gap tells the author to add `target`.
  - A type's `identity.member` that differs from `base`'s is written into `identity.field` of every operation of that type that already has an identity. Stream operations are left alone.
  - Update each `RecordAt` use from the Files list. `record_schema_of` walks `properties`/`items` per token.
  - Add `check_record`, the `root_ok` flag, the schema def and the `bad_pointer` change for `""`. Add `effective` and switch `effective_world` to it.

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p mb-mcm && cargo test -p mb-import --test it world_tier0 && cargo test -p mb-admin --test it world && cargo test -p mb-protocols`
Expected: PASS. The existing `mb-protocols` suite passes unchanged.

- [ ] **Step 5: Commit**

```bash
git add crates/mb-mcm crates/mb-import crates/mb-admin crates/mb-protocols/src crates/mb-engine/src docs
git commit -m "mcm: deep and root record pointers, tier0 in mb-mcm, apply and effective"
```

### Task 2: Vocabulary for id-or-alias paths and header ids

**Files:**
- No PR prerequisite: this task depends on Task 1 only. The two keys name what `claude/header-ids-alt-keys`' detectors find, but this task adds only their vocabulary (serde fields, schema, validator, overlay) and calls none of that PR's code. Task 5, which has tier 0 emit them from the detectors, is the task that waits for the PR.
- Modify: `crates/mb-mcm/src/world/mod.rs`:
  - `IdentityDesc.alternates`, `AnswerDesc.identity_header`;
  - `overlay` merges `identity`, `target` and `answer` **field by field** instead of replacing them whole (`overlay_type`/`overlay_op` call `set` per sub-field, as they already do per entry). An author who writes `answer: {identity_header: X}` keeps tier 0's `answer.record`, and one who writes `identity: {alternates: [..]}` keeps tier 0's `member`.
  - **`IdentityDesc.member` becomes `Option<String>`** (`#[serde(default, skip_serializing_if = "Option::is_none")]`). On `main` it is a required `String` with no serde default, so a revision `types.t.identity: {alternates: [...]}` does not deserialize as a `World` at all, and Task 4 would serve the whole revision as tier 0. With `Option`, an absent member means "unset": the overlay keeps the lower tier's member.
  - **`IdentityDesc.alternates` is `Option<Vec<String>>`**, not a `Vec` with "empty means unset". `None` keeps the lower tier's alternates; `Some(vec![])` says the type has none, which is how an author removes a wrongly guessed alias (Task 5). A set list replaces the lower tier's whole.
  - **`AnswerDesc.identity_header` is `Option<IdentityHeader>`**, with `enum IdentityHeader { Named(String), Off }` serialized untagged as a header name or `false` (`true` does not deserialize). `None` keeps the lower tier's header; `Some(Named(h))` replaces it; `Some(Off)` says the create names its id in no header, which is how an author turns off a header tier 0's detector guessed wrongly (Task 5). This is the header's equivalent of `alternates: []`. `IdentityHeader::name(&self) -> Option<&str>` gives the name. Tier 0 emits only `Named`.
- **What reads an absent member** (every place on `main` that reads `IdentityDesc.member`, plus Task 1's `apply.rs`, which is not on `main` yet). Run `git grep -n '\.member'` over `crates/mb-mcm`, `crates/mb-import`, `crates/mb-admin` and `crates/mb-protocols` at the start of the task and add any site this list misses:
  - `crates/mb-mcm/src/world/apply.rs` (Task 1's): it reads the type's `identity.member` twice, to diff it against `base`'s (the `identity.field` rewrite and the "a type with no identity in tier 0 gets one" trigger) and for the new identity's `field`. Both read `member.as_deref()`; the effective world never holds an identity with no member (below), so `None` only appears in `base` for a type tier 0 gave no identity, and reads as "differs" when the world sets one.
  - `crates/mb-mcm/schemas/world.v1.json`: `$defs/identity` drops `"required": ["member"]` (an empty `identity: {}` stays valid JSON Schema and is refused below).
  - `crates/mb-mcm/src/world/validate.rs`: `check_member` runs only when `member` is `Some`; `pattern` and `alternates` are checked against the record schema either way. The validator sees only the revision, so it does not refuse an absent member.
  - `crates/mb-mcm/src/world/effective.rs` (Task 1's): after the overlay, an effective identity whose `member` is still `None` (the revision set `pattern` or `alternates` on a type tier 0 gave no identity) is dropped from the effective world and refused `invalid` at `/types/<t>/identity/member` with the message "tier 0 names no member for this type; set identity.member". The rest of the revision applies. So the runtime, `Described` and `apply` never see an identity with no member.
  - `crates/mb-mcm/src/world/tier0.rs` (moved by Task 1): writes `member: Some(..)`; tier 0 never emits an identity without a member.
  - `crates/mb-protocols/src/world_gaps.rs` (`i.member.as_str()` at ~160 becomes `as_deref()`; a `None` member reads as no identity, which the effective world never holds).
  - Tests: `crates/mb-import/tests/it/world_tier0.rs` (`member.member.as_str()` at ~108 and `i.member.as_str()` at ~210 become `as_deref()`).
  - `crates/mb-admin/src/world.rs`: `effective_world` goes through `effective`, so the admin view shows tier 0's member under the revision's alternates, and the same refusal. `attribute` keeps its `/types/{t}/{field}` granularity: an identity entry whose sub-fields changed is attributed to the revision as a whole, member included, as today. `bad_pointer` does not read the identity. No Admin API schema change (the world document is served as an object).
- Modify: `crates/mb-mcm/src/world/validate.rs`, `crates/mb-mcm/schemas/world.v1.json`, `crates/mb-mcm/tests/fixtures/world-spec-excerpt.json` (unchanged unless the roundtrip needs it)
- Modify: `docs/superpowers/specs/2026-11-04-world-description-design.md` (the vocabulary list: `identity`: member, optional pattern, optional `alternates`; `answer`: optional `identity_header`, a header name or `false` for none), `docs/world.md` (vocabulary table), `plugins/mockingbird/skills/mockingbird-world/SKILL.md` (one line each)
- Test: `crates/mb-mcm/src/world/validate.rs` tests, `crates/mb-mcm/tests/it/world_schema.rs`

**Interfaces:**
- Consumes: Task 1's module layout.
- Produces:
  - `IdentityDesc { member: Option<String>, pattern, alternates: Option<Vec<String>>, extra }`, both skipped when `None`;
  - `AnswerDesc { record, records, long_running, identity_header: Option<IdentityHeader>, extra }`;
  - `enum IdentityHeader { Named(String), Off }` (a header name or `false` in JSON), with `name() -> Option<&str>`.
- Task 3 starts after this task merges and writes `Described` against these types.

- [ ] **Step 1: Write the failing tests**

```rust
#[test] fn alternates_must_be_members_of_the_record() {
    let v = validate(&world(json!({"types": {"template": {"identity": {"member": "TemplateId", "alternates": ["Alias", "Nope"]}}}})), &templates());
    assert_eq!(v.applied.types["template"].identity.as_ref().unwrap().alternates, vec!["Alias"]);
    assert!(v.refused.iter().any(|r| r.path == "/types/template/identity/alternates/1" && r.code == RefusalCode::Unresolved));
}
#[test] fn identity_header_must_be_a_declared_2xx_response_header() { /* "Resource-ID" declared on 201: applied; "X-Other": refused unresolved */ }
#[test] fn identity_header_is_not_applicable_off_http() { /* GraphQL mutation: refused not_applicable, for a name and for false */ }
#[test] fn identity_header_false_is_applied_without_a_header_check() {
    // answer.identity_header false on an HTTP create: applied as Some(Off) whatever headers the op declares;
    // identity_header true: refused invalid (does not deserialize as IdentityHeader)
}
#[test] fn overlay_identity_header_false_replaces_the_base_header() { /* base Named("Resource-ID"), top Off → Off; top None → Named("Resource-ID") */ }
#[test] fn schema_accepts_alternates_and_identity_header() { /* world_schema.rs: jsonschema validates the excerpt plus both keys, identity_header as a name and as false; true is invalid */ }
#[test] fn overlay_keeps_base_answer_members_the_top_leaves_out() {
    // base answer {record: "/data"}; top answer {identity_header: "Resource-ID"} → {record: "/data", identity_header: "Resource-ID"}
}
#[test] fn overlay_keeps_base_identity_member_when_only_alternates_are_set() {
    // the revision document {types: {template: {identity: {alternates: ["Alias"]}}}} deserializes as a World;
    // base identity {member: TemplateId} → {member: TemplateId, alternates: [Alias]}
}
#[test] fn an_identity_with_no_member_on_a_type_tier0_gave_none_is_refused() {
    // effective: type widget has no tier-0 identity; revision identity {alternates: [Alias]} →
    // refused invalid at /types/widget/identity/member, widget has no effective identity, the rest of the revision applies
}
#[test] fn empty_alternates_are_kept_as_set() { /* revision alternates [] over tier0 [Alias] → effective Some([]) */ }
#[test] fn schema_accepts_an_identity_without_member() { /* world_schema.rs: {identity: {alternates: [a]}} valid */ }
#[test] fn overlay_keeps_base_target_members_the_top_leaves_out() { /* base target {path_param: id}; top {member: id} → both */ }
```

- [ ] **Step 2: Run and verify the tests fail.** `cargo test -p mb-mcm` → FAIL: unknown fields land in `extra` and are refused as `unknown_key`.
- [ ] **Step 3: Implement.** Make `member` optional and add the fields, their JSON Schema entries and the validator checks, then the `effective` refusal and the `world_gaps` change (see Files). Reuse `check_member` for alternates. For a header name, check it the way `check_type_header` does, against the operation's 2xx outputs, case-insensitively; `false` is checked only for `not_applicable` (off HTTP). In `world.v1.json`, `identity_header` is `{"oneOf": [{"type": "string", "minLength": 1}, {"const": false}]}`. The spec and `docs/world.md` say what `false` means (the vocabulary lines in Files). Make `overlay` field-wise for `identity`, `target` and `answer`, and update the spec's overlay paragraph to say so. `mb-admin`'s `attribute` keeps its `/types/{t}/{field}` granularity: an entry whose sub-field changed is attributed as a whole, as today.
- [ ] **Step 4: Run and verify the tests pass.** `cargo test -p mb-mcm && cargo test -p mb-admin --test it world && cargo test -p mb-protocols --test it world_gaps` → PASS.
- [ ] **Step 5: Commit** `git commit -m "world: identity alternates and answer identity_header"`

### Task 3: `Described`, `MockRuntime::with_world`, and `world_gap` marks

**Starts after Task 2 merges.** `Described::new` and `Described::changed_identity_member` read `IdentityDesc.member`, which Task 2 makes `Option<String>`; this task is written against that type (`member.as_deref()`), never against `main`'s `String`.

**Files:**
- Create: `crates/mb-protocols/src/call/described.rs`, and stubs `crates/mb-protocols/src/call/{records,transitions,lifecycle,replays,list_query,state_lists,derived}.rs`, each holding only its `//!` module doc (filled by Tasks 5, 6, 7, 8, 9, 10 and 11). `call.rs` gets all eight `mod` lines in this task, so no later task edits `call.rs`'s module list; Task 9 in particular needs `mod list_query;` and does not edit `call.rs` at all.
- Modify: `crates/mb-protocols/src/call.rs`:
  - `mod described; mod records; mod transitions; mod lifecycle; mod replays; mod list_query; mod state_lists; mod derived;` (the seven later ones point at the stubs above);
  - `record_effects` also writes `world_gap`;
  - new `pub(crate) fn seed_world_gaps(rt: &MockRuntime, op: &Operation, generated: &mut GeneratedResponse)`: for each `GapCode` in `rt.described().gaps(&op.id)`, push `code.as_str().to_owned()` unless the list already holds it. It **extends and deduplicates**; it never assigns the field, so a mark pushed earlier is kept;
  - `complete` calls `seed_world_gaps` first, before any other work, so Task 5's dynamic `no_record` (pushed later in `complete`) lands after the static marks and is skipped when one is already there;
  - `generate_narrowed`'s `Prepared::Answered(answer)` arm calls `seed_world_gaps` on the answer before returning it. That covers every answer `prepare` returns through the shared path: `serve_held` from state, refusals, `nothing_held`, and the prepare-side answers later tasks add (Task 8's `replays::replayed`, Task 10's `state_lists::answer`), with no edit in those tasks.
- Modify: `crates/mb-protocols/src/graphql/resolve.rs`: in `call::Prepared::Answered(answer)` (the batched root field that `prepare` answered), call `call::seed_world_gaps` before it becomes `Route::Answered`. `Route::Join` ends in `complete`; `Route::Alone` goes through `generate_narrowed`.
- Modify: `crates/mb-protocols/src/lib.rs` (`MockRuntime` field, `with_world`, `world_revision`, `described`)
- Modify: `crates/mb-engine/src/lib.rs` (`GeneratedResponse.world_gaps: Vec<String>`, default empty, and every constructor)
- Modify: `crates/mb-protocols/src/http_rest/handler.rs` (Decision 6). On `main` the HTTP record drops anything `record_effects` writes except `effects`: `CallLog::generated` (~170-179) runs it on a scratch `recorded` value and keeps `recorded["effects"]` only, and the request JSON (~658-671) is rebuilt from `log.effects`. So:
  - `CallLog` gains `world_gap: Value` (`Null` in `CallLog::new`);
  - `CallLog::generated` also does `self.world_gap = recorded["world_gap"].take();`
  - the request JSON writes `request["world_gap"] = log.world_gap` when it is not `Null`, right after the `effects` line;
  - new `CallLog::answered(rt, op, generated)` (Decision 6): `seed_world_gaps`, then `generated`. All four production `log.generated` sites (~435 rate limit, ~496 chaos `filled_error`, ~547 the shared path, ~967 invalid input) call `answered` instead; only the unit tests (~1369/1376) call `generated`. `log.problem` and `log.rejected` record no `world_gap`.
  Task 5 edits this file after this task (header placement).
- Modify: `crates/mb-protocols/src/soap/mod.rs`, two sites only (Decision 6): the declared rate-limit answer (~534) is seeded and passed through `record_effects(&mut report.request, ..)` before `rate_limited_fault`; `forced_fault` (~623) calls `record_effects(&mut report.request, &generated)` after its `generate_once`.
- `grpc/service.rs` and `graphql/resolve.rs` (beyond the one arm above) are not edited for the record: they call `record_effects` on the request `Value` they record, after an answer that was seeded, and their rate-limit and chaos answers are statuses, not `GeneratedResponse`s. Step 3 re-checks this with `git grep -n "record_effects\|log\.generated\|declared_rate_limited"` over `crates/mb-protocols/src` and traces each stream's recorded call (`events/ws.rs`, `http_rest/sse.rs`, `graphql/subscription.rs`). A recorded `GeneratedResponse` that reaches no seeding point calls `seed_world_gaps` at its site before recording, and that file joins this task's Files.
- `crates/mb-mcm/src/world/effective.rs` and the `effective_world` switch are Task 1's; this task only consumes `effective`.
- Modify: `crates/mb-protocols/tests/it/support.rs` (`Described` harness), `crates/mb-protocols/tests/it/main.rs` (one `mod` line for each `described_*` module of Tasks 3-11; see "The integration-test crate's shared files")
- Create: stubs `crates/mb-protocols/tests/it/{described_identity,described_kinds,described_lifecycle,described_conflicts,described_lists,described_state_lists,described_derived}.rs`, each holding only its `//!` module doc, filled by Tasks 5, 6, 7, 8, 9, 10 and 11
- Test: `crates/mb-protocols/tests/it/described_runtime.rs` (new)

**Interfaces:**
- Consumes: `mb_mcm::world::{apply, effective}` (Task 1); `world_gaps::{gaps, GapCode}` (on `main`; `GapCode::as_str` gives the logged name).
- Produces:
  - `pub fn MockRuntime::with_world(self, base: &World, world: &World, revision: i64) -> Self`. `world` is already effective (tier 0 overlaid with the validated revision). It applies the world to the bundle (`apply(&bundle, base, world)`), then rebuilds `index`, `credential_names` and `described`.
  - `pub fn MockRuntime::world_revision(&self) -> i64` (`0` with no world).
  - `pub fn MockRuntime::world(&self) -> Option<&Arc<World>>`: the effective world `with_world` was given (`None` with no world). Task 4 keeps it in `Mounted.world`.
  - `pub(crate) fn MockRuntime::described(&self) -> &call::described::Described`.
  - `Described` methods:
    - `Described::new(bundle: &Bundle, base: &World, world: &World, revision: i64) -> Described`;
    - `Described::changed_identity_member(&self, t: &str) -> Option<&str>`: the effective identity member of `t` (`member.as_deref()`) when it differs from `base`'s, a `None` in `base` counting as different (Task 5's `extra_fields`);
    - `Described::op(&self, op_id: &str) -> Option<&OpDesc>`;
    - `Described::type_desc(&self, t: &str) -> Option<&TypeDesc>`;
    - `Described::gaps(&self, op_id: &str) -> &[GapCode]`;
    - `Described::state_value(&self, t: &str, state: &str) -> Option<Value>`. It returns a lifecycle state in the member's declared type: `"true"` → `true` for a boolean, a number for an integer, the text otherwise. `None` when it does not fit.
  - `Described::default()` describes nothing. It is not how a module tells that no world is attached; that is `rt.world().is_none()` (Task 5's `records::identity_header`/`records::alternates` do not need even that, because they fall back to the detector on any unset value; Decision 9).
  - `#[doc(hidden)] pub fn MockRuntime::with_test_gaps(self, gaps: HashMap<String, Vec<mb_protocols::world_gaps::GapCode>>) -> Self`, for the seeding tests only. It replaces the static gaps of each named operation in the runtime's `Described` (operations it does not name keep theirs) and changes nothing else. It is the only way a `Described` is set from outside the crate: `call` is a private module (`mod call;` in `lib.rs`), so `Described` and its constructors are `pub(crate)`, and `tests/it`, which sees neither `call` nor `#[cfg(test)]` items, reaches them only through this method. `world_gaps` is already `pub mod`, so `GapCode` is reachable. There is no `Described::for_test`.
  - Test support: `support::described(contract: &str, world: Value, script: Vec<Value>) -> DescribedHarness`, and `support::described_with_gaps(contract, world, script, gaps)`, which builds the same runtime and then calls `with_test_gaps(gaps)`, with `send(method, uri, body, headers) -> (StatusCode, HeaderMap, Value)`, `records() -> Vec<RequestRecord>` and `stored(t, id) -> Option<Value>`.
    - **It treats `world` exactly as the runtime does.** It parses `world` as a revision document, computes `base = tier0(&bundle)`, runs `mb_mcm::world::effective(&bundle, &base, &doc)`, and builds the runtime with `with_world(&base, &effective, 1)`. It never passes the raw document in.
    - It **panics when `effective` refuses any entry**, with the refusals in the message, so a test whose world has a typo fails loudly instead of testing tier 0. A test that means to exercise a refusal uses `support::described_allowing_refusals`.

- [ ] **Step 1: Write the failing tests** (`described_runtime.rs`)

```rust
#[tokio::test] async fn tier0_world_answers_as_no_world() {
    // Same contract and script. let base = tier0(&bundle);
    // one runtime MockRuntime::new(..).with_world(&base, &base, 0), one without with_world.
    // A create, a read, a list, an update and a delete answer equal status, headers (minus Date) and bodies; stored state is equal.
}
#[tokio::test] async fn world_gap_marks_an_operation_whose_type_has_no_identity() {
    let h = support::described(NO_ID_CONTRACT, json!({}), vec![create_answer()]);
    h.send("POST", "/widgets", Some(json!({"label": "a"})), &[]).await;
    assert_eq!(h.records()[0].request["world_gap"], json!(["no_identity"]));
    // Only the writing ops of the type are marked: the read carries no "no_identity".
    h.send("GET", "/widgets/a", None, &[]).await;
    assert!(!h.records()[1].request.get("world_gap").is_some_and(|g| g.as_array().unwrap().contains(&json!("no_identity"))));
}
#[tokio::test] async fn a_described_identity_clears_the_gap() {
    let h = support::described(NO_ID_CONTRACT, json!({"types": {"widget": {"identity": {"member": "label"}}}}), vec![create_answer()]);
    let (status, _, _) = h.send("POST", "/widgets", Some(json!({"label": "a"})), &[]).await;
    assert_eq!(status, StatusCode::CREATED);
    assert!(h.records()[0].request.get("world_gap").is_none());
    // The described identity also changes what is stored, not only the log mark:
    // createWidget had identity None; apply gave it Identity { field: "label", .. }.
    assert_eq!(h.stored("widget", "a").map(|r| r["label"].clone()), Some(json!("a")));
    let (status, headers, body) = h.send("GET", "/widgets/a", None, &[]).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers["x-mockingbird-source"], "state");
    assert_eq!(body["label"], "a");
}
#[tokio::test] async fn a_refused_entry_in_a_test_world_fails_the_harness() { /* #[should_panic]: identity.member "nope" */ }
#[tokio::test] async fn an_answer_prepare_refuses_is_marked() {
    // PATCH /things/{id} answers an Ack {ok} (static gap no_record); consistency on; PATCH of an unheld id
    // is answered by nothing_held in prepare (404) → recorded with world_gap ["no_record"], provider not called
}
#[tokio::test] async fn an_answer_from_state_is_marked() {
    // A held record read back through serve_held (source state). A read served from state has an identity and a target,
    // so its op has no static gap of its own; the harness is support::described_with_gaps(.., {getThing: [GapCode::NoTarget]}),
    // which calls the `#[doc(hidden)] pub` MockRuntime::with_test_gaps, to prove the Prepared::Answered arm seeds: world_gap ["no_target"]
}
#[tokio::test] async fn a_graphql_field_prepare_answers_in_a_batch_is_marked() {
    // two root fields in one GraphQL document (batched); one is an updateThing of an unheld id answered by nothing_held
    // in prepare (Route::Answered): its recorded call carries ["no_record"], like the same call sent alone
}
#[tokio::test] async fn an_http_record_keeps_world_gap_next_to_effects() {
    // NO_ID_CONTRACT create whose answer also carries an effect: the HTTP record's request holds both
    // "effects" and "world_gap" ["no_identity"] (CallLog keeps both, not effects alone)
}
#[tokio::test] async fn answers_that_never_reach_prepare_carry_the_static_gaps() {
    // NO_ID_CONTRACT's create declares 429 and a 400 body: a rate-limited create (declared_rate_limited),
    // a chaos-injected error and an invalid-input create (filled_error, Source::Rejected) are each recorded
    // with world_gap ["no_identity"]; a problem+json refusal (log.problem) has no world_gap
}
#[tokio::test] async fn a_soap_rate_limited_answer_carries_the_static_gaps() {
    // a SOAP operation with a static gap and a declared rate-limit fault: the recorded call's request holds world_gap;
    // a forced-500 fault whose detail is generated also holds it
}
#[test] fn seed_world_gaps_extends_and_deduplicates() {
    // generated.world_gaps = ["no_record"]; static gaps [NoIdentity, NoRecord] → ["no_record", "no_identity"]
}
#[test] fn state_value_follows_the_member_type() { /* boolean member: "true" → true; string: "ARCHIVED"; integer member with "x" → None */ }
```

- [ ] **Step 2: Run and verify the tests fail.** `cargo test -p mb-protocols --test it described_runtime` → FAIL (`with_world` is not defined).
- [ ] **Step 3: Implement.** (`NO_ID_CONTRACT` has `POST /widgets` and `GET /widgets/{label}`, and no member tier 0 takes for an identity.) `with_world` replaces `self.bundle` with `Arc::new(apply(&self.bundle, base, world))` and recomputes the derived fields, as `new` does. `Described::new` keeps `world.operations`/`world.types` keyed as they are and precomputes `gaps` per operation (each `Gap` with `operation: Some(id)`, plus `NoIdentity` for the op's type on the ops `world_gaps::gaps` lists for that gap). `gaps` reports `NoIdentity` once per type, with `operation: None`, for a type that an operation creates, updates, replaces, deletes or acts on (`is_write` of the effective kind). `Described` marks exactly those writing ops of the type. A read or list of a type with no identity gets no `no_identity` mark; it carries its own `no_target` when `gaps` reports one. A test in `world_gap_marks_an_operation_whose_type_has_no_identity` checks this: `GET /widgets/{label}` on `NO_ID_CONTRACT` records no `no_identity`. It lowercases header names for lookup. `record_effects` writes `world_gap` whenever `generated.world_gaps` is non-empty, even with no effects.
- [ ] **Step 4: Run and verify the tests pass.** `cargo test -p mb-protocols && cargo test -p mb-engine` → PASS.
- [ ] **Step 5: Commit** `git commit -m "runtime: compile the effective world into the runtime, mark world gaps"`

### Task 4: Load the effective world at build; hot reload on a revision

**Files:**
- Modify: `crates/mb-store/src/sqlite.rs`, `crates/mb-store/src/postgres.rs` (`add_world_revision`: after commit, `publish_mock` when the version's status is active; the status is read in the same transaction). `add_world_revision` has only `mock_id`, and `publish_mock(workspace, name)` needs the mock's key, so it looks the key up the way `put_scenario` does:
  - SQLite: `let (ws, name) = self.require_mock_key(mock_id).await?;` at the start, as `put_scenario` does (`require_mock_key` wraps `mock_key`; a missing mock is `NotFound`).
  - Postgres: `let (ws, name) = require_locked_mock_key(&mut tx, mock_id, LOCK_KEEP).await?;` inside the transaction, as `put_scenario` does, taken **before** the existing `contract_versions ... FOR NO KEY UPDATE` read, so the lock order is mock row then version row, the order the version writers use (`require_locked_mock_key(.., LOCK_FOR_VERSIONS)` first).
  - After commit: `self.publish_mock(&ws, &name)` when the version is active.
- Modify: `crates/mb-admin/src/world.rs` (new `runtime_world`)
- Modify: `crates/mockingbird/src/runtime.rs` (`LoadedWorld`; `build_runtime_tracked` takes the previous `LoadedWorld` and returns the new one), `crates/mockingbird/src/registry.rs` (`Mounted.world`; `build_mounted` passes the previous runtime's world and schedules the retry; `MockRegistry::world_status` for the tests)
- **No `mb-protocols` file.** `crates/mb-protocols/src/lib.rs` is Task 3's. This task uses Task 3's `MockRuntime::world()` and `world_revision()` and adds nothing to `MockRuntime`: the pending flag is kept in `LoadedWorld`.
- Test: `crates/mb-store/tests/it/conformance.rs`, `crates/mb-admin/tests/it/world.rs`, `crates/mockingbird/tests/it/world_runtime.rs` (new; register it in `tests/it/main.rs`)

**Interfaces:**
- Consumes: `MockRuntime::with_world` (Task 3), `mb_mcm::world::effective` (Task 1; Task 3 only consumes it).
- Produces:
  - `mb_admin::world::runtime_world(store, mock_id, version, bundle, base: &World) -> Result<(World, i64), StoreError>`. It makes one `latest_world_revision` read, then `effective(bundle, base, &doc)`. It does no `attribute`, no gap list and no earlier-revision reads. With no revision it returns `(base.clone(), 0)`.
    - The stored document is JSON (`latest.document`). When `serde_json::from_value::<World>` fails, `runtime_world` does what `effective_world` does: the world is tier 0 at that revision, `Ok((base.clone(), latest.revision))`, and the whole-document refusal is logged with `tracing::warn!(mock, revision, error)`. It is not a `StoreError`, so it does not set `pending` and does not start the retry loop. `Err` is returned only for a failed store read.
  - `pub(crate) struct LoadedWorld { version: i64, world: Arc<World>, revision: i64, pending: bool }` in `runtime.rs`. `pending` is true when the world could not be loaded (the runtime was built with the fallback) and a retry is scheduled.
  - `build_runtime_tracked(deps, mock, previous: Option<&LoadedWorld>)` returns the `LoadedWorld` it built with as a fourth tuple member, next to the runtime, budget and profiles. Its `world` is `MockRuntime::world()` (Task 3) of the runtime it built, or `base` when the runtime has none.
  - Its public wrapper `build_runtime_with_budget(deps, mock)` (`runtime.rs` ~486; `build_runtime` calls it) keeps its signature: it calls `build_runtime_tracked(deps, mock, None)` and drops the `LoadedWorld` (`.map(|(rt, budget, _, _)| (rt, budget))`). Its callers (`tests/it/{registry,import_llm,webhooks_e2e}.rs` through `build_runtime`) have no previous world and schedule no retry; a store error there builds with `base`, as a first build does. `registry.rs`'s `build_mounted` is the only caller of `build_runtime_tracked`.
  - `Mounted.world: Option<LoadedWorld>` in `registry.rs`. On `main`, `Mounted` holds `workspace`, `name`, `served`, `state_health`, `events`, `channels`, `budget`, `profiles` and `uses_default`, and nothing about the runtime's world, so this field is new. `build_mounted` fills it from what `build_runtime_tracked` returned, and reads the previous one from the registry map entry for the mock.
  - `#[doc(hidden)] pub fn MockRegistry::world_status(&self, workspace: &str, name: &str) -> Option<(i64 /*revision*/, bool /*pending*/)>`, read from `Mounted.world`, for `tests/it` (which do not see `#[cfg(test)]` items).
  - A runtime whose `world_revision()` is the newest revision of its active contract version.

- [ ] **Step 1: Write the failing tests**

```rust
// conformance.rs, run on both backends
async fn a_world_revision_on_the_active_version_publishes_a_mock_change(store) { /* subscribe; add revision; recv ControlChange::Mock{workspace,name} */ }
async fn a_world_revision_on_an_inactive_version_publishes_nothing(store) { /* try_recv is Empty */ }

// mockingbird world_runtime.rs (server with a scripted provider, as scenarios_e2e.rs builds it)
#[tokio::test] async fn a_world_revision_applies_without_restart() {
    // Contract whose widget type has no identity. The first create is logged with world_gap ["no_identity"].
    // PUT /mocks/m/contracts/1/world {types: {widget: {identity: {member: label}}}}; wait until `world_status` reports revision 1.
    // The next create has no world_gap.
}
#[tokio::test] async fn a_world_revision_mid_session_keeps_stored_records() {
    // Create a record in session s; put a revision; GET the record in session s → 200 from state (X-Mockingbird-Source: state).
}
#[tokio::test] async fn a_store_error_on_the_first_build_serves_tier0_and_retries() {
    // A store whose latest_world_revision fails twice, then succeeds, with revision 1 stored.
    // The first runtime: world_status (0, pending true); a create is logged with tier 0's world_gap
    // (the same marks a tier-0 runtime gives). Within the backoff the runtime is rebuilt: world_status (1, pending false).
}
#[tokio::test] async fn a_store_error_on_a_rebuild_keeps_the_previous_world() {
    // Revision 1 loaded. A scenario change rebuilds while latest_world_revision fails:
    // the new runtime serves revision 1's world, world_status (1, pending true); the scenario change is in effect.
}
#[tokio::test] async fn a_retry_whose_build_fails_is_rescheduled() {
    // Revision 1 stored. A store whose active_contract and latest_world_revision both fail for the first three builds:
    // the first retry's build_mounted returns Err (the pending runtime stays mounted), a later retry is still scheduled,
    // and once the store recovers world_status is (1, pending false), with no other mock change.
}
#[tokio::test] async fn a_pending_retry_stops_when_the_mock_is_deleted() {
    // latest_world_revision always fails: world_status (0, pending true). Delete the mock: the next retry finds it gone,
    // the counting store sees no further build for it, and world_status is None.
    // Same with the active version deactivated (build returns Ok(None)): the mock is unmounted and no retry fires again.
}
#[tokio::test] async fn a_version_change_replaces_the_pending_retry() {
    // pending on version 1; activate version 2 while the store has recovered: world_status reports version 2's revision,
    // pending false, and exactly one build ran after the activation (the stale version-1 retry did nothing).
}
#[tokio::test] async fn a_revision_that_is_not_a_world_serves_tier0_without_retrying() {
    // Revision 1's document is {"types": 5} (does not deserialize as World): world_status (1, pending false),
    // a create is logged with tier 0's world_gap; the counting store sees no second build.
}
#[tokio::test] async fn a_rebuild_reads_only_the_newest_revision() {
    // Counting store with 5 revisions: one build makes exactly one latest_world_revision call and no world_revision(n) calls.
}

// mb-admin tests/it/world.rs
#[tokio::test] async fn runtime_world_equals_effective_world_without_authors() { /* same world and revision for several revision histories */ }
```

- [ ] **Step 2: Run and verify the tests fail.** `cargo test -p mb-store --test it conformance && cargo test -p mb-admin --test it world && cargo test -p mockingbird --test it world_runtime` → FAIL.
- [ ] **Step 3: Implement.**
  - In `build_runtime_tracked`, after `MockRuntime::new`, compute `base = tier0(&contract.bundle)` once.
  - `match runtime_world(.., &base) { Ok((w, rev)) => rt = rt.with_world(&base, &w, rev), Err(e) => fallback }`.
  - The fallback is the previous runtime's `(world, revision)` when `build_mounted` passed one for the same contract version, else `(base, 0)`. It returns a `LoadedWorld` with `pending: true` and logs `tracing::warn!(mock = %mock.name, error = %e, "world description not loaded; retrying")`.
  - `build_mounted` gets the previous `LoadedWorld` from the registry map's `Mounted.world` (new field, see Interfaces), when there is one, and passes it to `build_runtime_tracked`.
  - The registry schedules one delayed rebuild of that mock (1 s, doubling to 60 s, reset on success) when either:
    - the built `LoadedWorld` is `pending`, or
    - a retry's own `build_mounted` returned `Err` (the store is still down; `active_contract` or `list_scenarios` usually fails before the world read). The old runtime stays mounted, as today, and the retry is rescheduled with the next backoff.
  - A rebuild for any other reason that **succeeds** cancels the pending retry and schedules its own if it is also pending. One that fails while the mounted `LoadedWorld` is `pending` leaves the retry scheduled. So the loop ends when a build loads the world, or when one of the cancellation conditions below holds.
  - **What cancels a pending retry.** The retry is keyed by `(workspace, name)` and carries the build generation that scheduled it (a counter `build_mounted` bumps per mock on every build it starts). At most one retry per mock is scheduled. When it fires, it does nothing and schedules nothing when any of these holds:
    - the mock is no longer in the registry map: it was unmounted, deleted, or its workspace was removed (a `ControlChange::Mock` or `ControlChange::Workspace` rebuild that dropped it, or a `Resync` that did not find it);
    - a build since it was scheduled returned `Ok(None)`: the mock was deleted or has no active contract, so the registry unmounts it (as today) and a retry has nothing to load. An `Ok(None)` build also cancels the scheduled retry at once, rather than letting it fire and find the mock gone;
    - its generation is older than the mounted runtime's: a later rebuild (a scenario change, a new revision, a version activation) replaced the runtime, and that rebuild already cancelled or rescheduled the retry by the rule above;
    - the mounted `LoadedWorld` is not `pending` (another rebuild loaded the world);
    - the registry is shutting down (the hub closed, `ChangeBatch.closed`): every pending retry is dropped with the registry's task set.
    An **active-version change** is a rebuild like any other: it bumps the generation, so the old version's retry is stale. The new build's fallback uses the previous `(world, revision)` only when it served the same contract version (see the fallback above), so a store error during it falls back to the new version's `base` and schedules a fresh retry for the new version.
  - **A retry is a full rebuild, and that is accepted.** Each retry rebuild restarts the mock's `ChannelRuntime` and webhooks and resets the in-memory indexes (`collections`, `path_aliases`, `value_index`, `written_types`, `own_reads`), exactly as a scenario change or a new revision does. Stored records, the Task 10 `#listed` marks and the Task 8 replay keys live in the state store and survive. Retries happen only while the control store is failing, with backoff to 60 s, so the restarts are rare; retrying only the world load would need an in-place runtime swap (`ArcSwap`), which this plan does not add.
  - Call `with_world` before `set_data_plane_auth`, or rely on `with_world` recomputing the credential names.
- [ ] **Step 4: Run and verify the tests pass.** Run the same commands, plus `LC_ALL=en_US.UTF-8 scripts/test-postgres.sh`. Expected: PASS.
- [ ] **Step 5: Commit** `git commit -m "runtime: build with the effective world, rebuild on a new revision"`

### Task 5: Identity and record pointers

The cascades come from here: about 80 checks across blind7-10, plus 132 (header id) and 36 (id or alias) in blind11.

**Prerequisite:** `claude/header-ids-alt-keys` (blind11 fixes 1 and 5) merged, and Tasks 2, 3 and 8 (Task 8 edits `call.rs` first; see "Parallel tasks touch disjoint files"). This task does not write a second header-id or alias lookup. It parameterises that PR's functions, so the described path and the heuristic path are the same code. Below, "the fix-1 functions" means that PR's header-id detector and its create path (mint, store the request shaped onto the read schema, echo in the header). "The fix-5 members" means the id-like members it adds to the `fields` list that `by_path_fields` scans.

**Files:**
- Fill: `crates/mb-protocols/src/call/records.rs` (Task 3's stub, already declared `mod records;` in `call.rs`; this task adds no `mod` line)
- Modify: `crates/mb-mcm/src/world/tier0.rs`: tier 0 emits `answer.identity_header` for each create where the fix-1 detector finds a header, and `identity.alternates` for each type where fix 5 finds id-like members **and** tier 0's `identities()` gives the type a member. A type with detected aliases but no tier-0 member gets no identity entry at all (Task 2: tier 0 never emits a member-less identity); `records::alternates` falls back to the detector for it (Decision 9). If those detectors are not in `mb-mcm`, move them there first (they read only the bundle).
- Modify: `crates/mb-protocols/src/call.rs`:
  - `write_state` returns `#[derive(Default)] WriteOutcome { stored: usize, no_record: bool }` instead of `()`. Its create arm tries `records::created(..)` first. (`WriteRequest.generated` is `&GeneratedResponse` and cannot be pushed to, so `write_state` marks nothing itself.) Every `return` in `write_state` returns the outcome the table below gives it; no other line of the other arms changes.
  - `complete`, before its `write_state` call: `records::settle_create(rt, plan, call, &mut generated)` mints a missing id and places it in the answer (see "Data flow").
  - `complete`, after its `write_state` call: when the call is a 2xx create or update and `outcome.no_record` is true, push `"no_record"` to `generated.world_gaps` unless it is already there (Task 3's `seed_world_gaps` ran first and may have put the static `no_record` in). `complete` owns `mut generated` and returns it to the protocol handler, which records and serves it after this point.
  - **`WriteOutcome` per arm.** `stored` counts the records the write located and handed to the store (`upsert`, `insert_seeded`, `insert_seeded_all`), whatever the store then answered. `no_record` is true only where the arm found no record of the type in the answer or the request, which is the description gap `no_record` names. A store refusal (the state breaker open, `!state.open()`) is the breaker's matter, not a world gap, so it never sets `no_record`. The default (`stored: 0, no_record: false`) marks nothing, so an arm this table misses can only fail to mark, never mark falsely. On `main`'s arms, with Task 5's create-arm additions:

    | Return | `stored` | `no_record` |
    | --- | --- | --- |
    | `Seeded` origin (a list seeding), any return | items with an id handed to `insert_seeded_all` (0 when the body holds no list) | false |
    | create: `records::created` finds records (Task 5) | their count | false |
    | create: `data` is `None` (no answer body and no request body) | 0 | true |
    | create: the identity member is found in `data` (upsert) | 1 | false |
    | create, member miss: no answer body or no request body | 0 | true |
    | create, member miss: `fitting_member` finds no member | 0 | true |
    | create, member miss: the fitting member has no identity value | 0 | true |
    | create, member miss: the fitting member has an id (`insert_seeded`), stored or declined because a record is held under that id or the session deleted it | 1 | false |
    | update: `target_id` is `None` | 0 | false (a missing target is `no_target`, not `no_record`) |
    | update: `data` is `None` (no answer body and no request body) | 0 | false |
    | update: merged onto a held record (`was_held && !generated.new_record`, `write_memory::onto_record`), whatever the answer was, an Ack included | 1 | false |
    | update: nothing held, the answer does not fit the type, `fitting_member` finds the record | 1 | false |
    | update: nothing held, the answer does not fit the type, `fitting_member` finds none (an Ack about an unheld record) | 0 | true |
    | update: nothing held, the answer fits the type or there is no answer body (`_ => data`, upsert) | 1 | false |
    | delete arm | 0 | false |
    | any other kind (`_ => return`: actions, queries) | 0 | false |

    Task 6's transition and Task 7's soft delete and `initial` state change none of these rows (they act on a record already located). `complete` consults `no_record` only for a 2xx create or update whose origin is not `Seeded`.
  - **The `by_path_fields` sites keep their gating and their order.** On `main` the three sites build different lists under different conditions:
    - the update/delete target lookup (after the key read misses): `read_fields` = `index.path_fields(t)` only when `delete && rt.consistency && !param_is_id`; then `identity.path_field` when set; else `name_fields(param)` only when `rt.consistency && param.is_some() && (delete || identity.is_some()) && !path_names_identity`;
    - `any_owner_holds` (called from `read_state`): `name_fields(param)` then `index.path_fields(t)`;
    - the parent lookup in `related`: `name_fields(field)` only.

    None of those lists, conditions or orders changes, with one substitution: **where fix 5 put its id-like members in a site's list, the site now takes them from `records::alternates(rt, t)` instead of calling the detector.** `records::alternates` reads the effective world's `identity.alternates` when they are set, and calls the fix-5 detector itself when they are not (no world attached, `rt.world().is_none()`; no `alternates` on the type; or no effective identity for the type). Tier 0 emits exactly the detector's members as `alternates` on every type it gives an identity member and on no other (the tier-0 alternates rule, Decision 9), and the detector answers for the rest, so under tier 0 and with no world each site's list is the same members in the same position as after fix 5. The fallback calls the detector per call, as fix 5's sites do after it merges, so it adds no cost over fix 5. An author's `alternates` then replaces the guessed members in that position, entry by entry: `[Handle]` replaces a wrongly guessed `Alias` (which no longer reaches the record), and `[]` removes it. This is what makes "a later tier overrides an earlier one" hold for alternates; the detector feeds tier 0 only.

    Task 5 also adds `records::extra_fields(rt, t, already: &[&str]) -> Vec<&str>`, which returns at most one member: the type's identity member from the effective world, when it differs from `base`'s (tier 0's) member for the type and `already` does not hold it. It is needed even though `apply` already wrote it into the op's `identity.field`: a record stored before the revision is keyed by the old member, so the key read misses and only a scan by the new member finds it (Review Focus 3). Each site, after its existing attempts miss, makes **one more** `by_path_fields` call with `extra_fields(..)` only when it is non-empty, and under the site's own conditions (the update/delete lookup only when `rt.consistency` is on, as its scans are today). With no world and with a tier-0 world it is empty (it diffs against `base`), so no scan is added and every site behaves byte-for-byte as after fix 5. An extra attempt costs at most one `MAX_SCAN` (500) listing per miss, and only for a type the author described. `by_path_fields` itself is unchanged: `PathAliases` first, then one `MAX_SCAN` listing, first match in field order.
  - `mint_create_id`: format a minted id with `identity.pattern`.
  - **The direct key reads try both forms of a patterned id.** A record is keyed by its identity value as it was stored: bare (`7`) when it was stored before the type had a pattern, patterned (`folders/7`) when minted or answered under one. The path value may be either. Four `state.get(session, t, id)` calls read a record by the path value as its key, and each becomes `records::held_by_key(state, session, t, id)`:
    - `read_context`'s target read (`call.rs` ~5373). It serves every read (`read_state` calls `read_context`) and is also the update/delete key read that runs **before** that function's `by_path_fields` attempts, which stay as they are (above). `prepare` and `write_state` reach it through `read_context` too, so there is no second update/delete key read to edit;
    - the `foreign_key` check in `prepare` (~896: whether the target's key holds another owner's record), so it agrees with the target read on which key is meant;
    - `any_owner_holds`' first read (~6338), before its `by_path_fields` scan;
    - `follow`'s reference read (~6900), so a nested route's parent (`/folders/{folder}/files` with `folder=7`) reaches a parent keyed `folders/7`.
    `records::held_by_key` reads `id` first. On a miss, and only when the type's effective identity has a `pattern`, it reads the other form: `records::patterned(pattern, id)` when `id` does not already fit the pattern, else `records::bare_id(pattern, id)`. It returns the hit with its stored key (`StoredResource.id`), and the existing `target_id = Some(found.id.clone())` (~889) then sends the update or delete to that stored key, so a write never creates a second record under the other form. With no pattern (every type with no world, and under tier 0, which emits no pattern) it is the one `state.get` it replaces, byte for byte. A miss costs at most one extra key read, never a scan. `by_path_fields`, `PathAliases` and the scans compare member values as given; the dual form is a key-read rule only. The other `state.get` calls in `call.rs` (by an id the runtime itself holds: effect targets, stubs, `held_answer`, minting checks) read keys already in stored form and stay as they are.
- Modify: `crates/mb-engine/src/lib.rs`: `GeneratedResponse.created_id: Option<String>`, filled by the caller like `effect_outcomes`. If fix 1 already carries the minted id to the handler some other way, use that carrier instead and skip this field.
- Modify: `crates/mb-protocols/src/http_rest/handler.rs`: after `mb_engine::headers::declared_headers` fills the response headers, the fix-1 header placement runs with the header name from `records::identity_header(rt, op)` and the id from `generated.created_id`. `Location` becomes the item URL. The engine stays unaware of it.
- Test: `crates/mb-protocols/tests/it/described_identity.rs` (Task 3's stub; this task fills it and edits no other file in `mb-protocols/tests/it`), `crates/mb-import/tests/it/world_tier0.rs`

**Data flow of a create** (all in `complete`, in this order):
1. The answer is settled (generated, repaired, validated), as today.
2. `records::settle_create(rt, plan, call, &mut generated)`. It runs when the op is a 2xx create and the record at the answer pointer has no identity value. When `records::identity_header(rt, op)` is `Some` (the one place the header name is read, Decision 9: the effective world's `answer.identity_header` when set, else the fix-1 detector), it mints the id with `mint_create_id`, patterned when the type has a pattern. It writes the id into `generated.body` at the record pointer (when the schema declares the member there) and into `generated.created_id`. It then re-validates:
   - the body, with `Engine::serve_stored`;
   - the id as the header value, against the identity header's declared schema on the 2xx response (`mb_engine::headers`' schema check for declared headers; for `Location`, the item URL is checked as a string against that header's schema). An undeclared schema is a string with no constraints.

   When either check fails, it restores the body, leaves `created_id` as `None`, the header is not placed (the answer keeps whatever `declared_headers` filled), and the create stores nothing (marked `no_record` at step 4).
3. `write_state(..)` stores what `records::created` finds in the now-final `generated`, and returns `WriteOutcome`.
4. `"no_record"` is pushed when `outcome.no_record` is true (the `WriteOutcome` table).
5. `complete` returns `generated`. The handler places the identity header from `created_id`, then records the request with `world_gaps` and serves it.

**Interfaces:**
- Consumes: `Described::{op, type_desc}` (Task 3); `IdentityDesc.alternates`, `AnswerDesc.identity_header` (Task 2); `RecordAt::Pointer` (Task 1); the fix-1 functions and fix-5 members.
- Produces:
  - `records::created(rt: &MockRuntime, op: &Operation, request: Option<&Value>, answer: Option<&Value>) -> Vec<Value>`, the records a create makes:
    - `request.records`/`answer.records` items, one each;
    - else the `long_running.result` member of the answer;
    - else the one record at `answer.record`;
    - else, for a create whose `records::identity_header(rt, op)` is `Some`, the fix-1 record (`records::request_record` of the request, shaped onto the read schema, plus the id).
  - `records::identity_header(rt, op) -> Option<String>`: the effective world's `answer.identity_header` name (`IdentityHeader::name`); `None` when it is `false` (`IdentityHeader::Off`). When the effective world does not set it (no world attached, `rt.world().is_none()`, or no `identity_header` on the op), it is the fix-1 detector's result. Tier 0 emits the detector's header as `Named` wherever it finds one, so an unset value under any world means the detector finds none, and the two paths agree. Nothing else reads the header name: `settle_create`, `records::created`'s fix-1 fallback and the `handler.rs` placement all call this function.
  - `records::alternates(rt, t) -> Vec<String>`: the effective `identity.alternates` when the effective world sets them (`Some`, empty included); otherwise the fix-5 detector's members. "Otherwise" is: no world attached (`rt.world().is_none()`), a type whose effective identity sets no `alternates`, or a type with no effective identity (tier 0 emits `alternates` only on a type it gives a member, so a type with detected aliases and no tier-0 member is answered by the detector, under tier 0 and with no world alike). With a world attached the detector is therefore still called for such types, which keeps `tier0_world_answers_as_no_world`. Every `by_path_fields` site reads fix 5's members through this function (see Files).
  - `records::extra_fields(rt, t, already: &[&str]) -> Vec<&str>` (see Files): only a changed identity member. There is no `records::path_fields`: the existing lists stay where they are built.
  - `records::request_record<'a>(rt, op, body: &'a Value) -> &'a Value` (Decision 10): the value at the effective `request.record`, else the whole body. Used by the fix-1 create record (the request shaped onto the read schema) and by every other place the create arm takes its record from the request. Task 6 uses it for the update merge.
  - `records::settle_create(rt, plan, call, generated: &mut GeneratedResponse)`.
  - `records::patterned(pattern: &str, id: &str) -> String` and `records::bare_id(pattern: &str, value: &str) -> Option<String>`. A read by a path value tries both forms, so a record keyed under either one is found.
  - `records::held_by_key(state: &mut StateAccess<'_>, session: &SessionKey, t: &str, id: &str) -> Option<StoredResource>`: the dual-form key read (see Files), used at exactly the four sites listed there.
- One rule for alternates: they are members tried by `by_path_fields`, in the position fix 5 gave its members, so they follow its rule (first match in field order, owner check, `PathAliases` memo). There is no separate "exactly one" rule. If fix 5 needs uniqueness for guessed members, it adds that to `by_path_fields` for every field, and described alternates get it too.

**Coordination cases for `claude/header-ids-alt-keys`.** Step 1 of this task records, in `records.rs`'s module doc, what the merged branch did. Then:
- **Fix 5's members are per type and appended after each site's fields** (what Coordination asks for). Each site's `fix5::id_like_members(bundle, t)` call is replaced by `records::alternates(rt, t)`, in the same position; tier 0 emits `id_like_members` as `alternates` on the types it gives an identity member, and on no other (the tier-0 alternates rule). `a_detected_alternate_and_the_same_alternate_described_answer_alike`, `described_alternates_replace_the_detected_ones` and `empty_alternates_remove_the_detected_ones` hold.
- **Fix 5's members are per path parameter** (derived from `name_fields(param)`, an `idOrAlias`-style name, or anything else not a function of the type alone). Tier 0 cannot restate them as type-level `alternates`, so:
  - tier 0 emits no `identity.alternates`; `records::alternates(rt, t)` returns only the described alternates (empty with no world or a tier-0 world);
  - fix 5's per-parameter lookup stays as it merged, untouched and in its place; described alternates are tried after it, as one more `by_path_fields` attempt with `records::alternates(rt, t)` minus the site's list (in this case only, `extra_fields` returns the changed identity member followed by the described alternates). An author can add members but cannot remove or replace a per-parameter guess, so "a later tier overrides" does not hold for fix 5's members in this case; that limitation is stated in `docs/world.md`, and `described_alternates_replace_the_detected_ones`/`empty_alternates_remove_the_detected_ones` are dropped;
  - `a_detected_alternate_and_the_same_alternate_described_answer_alike` is replaced by `a_described_alternate_adds_to_the_detected_members` (a type where fix 5 detects `Alias` per parameter and the world adds `Handle`: both reach the record; with no world only `Alias` does, as on `main` plus fix 5), and the tier-0 test asserts no `alternates` are emitted;
  - "one implementation" then holds for the header (fix 1) only, and the plan's Decision 9 sentence about alternates is read with this exception.
- **Fix 5 lands as a separate lookup** (not in a field list): the first step folds it into the site's list at the position it held, then the per-type or per-parameter case above applies.

- [ ] **Step 1: Write the failing tests**

```rust
#[tokio::test] async fn a_record_one_item_deep_in_an_envelope_is_stored() {
    // answer {"success": true, "result": [{"id": "z1", "name": "a"}]}, world answer.record "/result/0"
    // then GET /zones/z1 → 200, source state, {"success": true, "result": [{"id":"z1","name":"a"}]}
}
#[tokio::test] async fn a_bulk_create_stores_each_record() {
    // PUT /contacts {"Contacts": [{"Name":"a"},{"Name":"b"}]}, request.records "/Contacts", answer.records "/Contacts"
    // both GET /contacts/{ContactID} answer from state with their Name
}
#[tokio::test] async fn a_described_identity_header_reaches_a_header_the_detector_misses() {
    // POST /conversations answers 201 {"threadIds":[1]} with "X-Conv" declared (no -id suffix, so fix 1 does not detect it);
    // world answer.identity_header "X-Conv" → response header X-Conv == id; GET /conversations/{id} → state, holds the request's subject
}
#[tokio::test] async fn a_detected_header_and_the_same_header_described_answer_alike() {
    // Resource-ID contract: one runtime with no world, one with world answer.identity_header "Resource-ID":
    // equal status, headers (minus Date), bodies and stored records
}
#[tokio::test] async fn a_location_identity_header_is_the_item_url() { /* Location: <base>/conversations/{id} */ }
#[tokio::test] async fn identity_header_false_turns_the_detected_header_off() {
    // Resource-ID contract, where fix 1 detects Resource-ID; world answer.identity_header false:
    // no id is minted, no Resource-ID header is placed by the mint (declared_headers' value stays), nothing is shaped
    // from the request, and the create stores what the answer itself holds (here nothing: world_gap ["no_record"]).
    // With no world the same create mints, places the header and stores, as after fix 1.
}
#[tokio::test] async fn a_described_header_replaces_the_detected_one() { /* fix 1 detects Resource-ID; world names X-Conv (declared): X-Conv carries the id, Resource-ID is not minted */ }
#[tokio::test] async fn a_long_running_create_stores_its_result() {
    // answer {"name":"operations/1","done":true,"response":{"name":"folders/7","displayName":"a"}}, long_running {done: done, result: response}
    // GET /folders/7 → state
}
#[tokio::test] async fn a_patterned_identity_reads_by_bare_or_full_id() {
    // pattern "folders/{id}": a create stores under "folders/7"; GET /folders/7 and GET by "folders/7" → state (read_context's key read);
    // PATCH /folders/7 updates that record (no second record under "7"); DELETE /folders/7 → both reads 404
}
#[tokio::test] async fn a_nested_route_reaches_a_patterned_parent_by_its_bare_id() { /* parent stored "folders/7": GET /folders/7/files relates it (follow's reference read) */ }
#[tokio::test] async fn a_type_with_no_pattern_makes_one_key_read() { /* counting state store: a missed read makes exactly the get calls it makes on main */ }
#[tokio::test] async fn an_alternate_member_reaches_the_same_record() {
    // identity TemplateId, alternates [Alias] on a type fix 5 does not detect; create {Alias: "welcome"};
    // GET /templates/welcome and GET /templates/{TemplateId} both state, same body;
    // DELETE /templates/welcome → GET /templates/{TemplateId} 404
}
#[tokio::test] async fn a_detected_alternate_and_the_same_alternate_described_answer_alike() { /* as the header twin, for Alias */ }
#[tokio::test] async fn described_alternates_replace_the_detected_ones() {
    // fix 5 detects Alias on template; world alternates [Handle]: GET /templates/{Handle} → state,
    // GET /templates/{Alias} → not found from state (Alias is no longer tried), same list calls as with Alias
}
#[tokio::test] async fn detected_aliases_on_a_type_with_no_tier0_identity_still_reach_the_record() {
    // fix 5 detects Alias on widget, but tier 0's identities() gives widget no member: tier 0 emits no identity for it;
    // with a tier-0 world and with no world, GET /widgets/{Alias} reaches the record the same way (records::alternates
    // falls back to the detector); a revision {identity: {alternates: [Handle]}} alone is refused (Task 2) and changes nothing
}
#[tokio::test] async fn empty_alternates_remove_the_detected_ones() { /* world alternates []: GET /templates/{Alias} no longer reaches the record */ }
#[tokio::test] async fn a_create_whose_request_wraps_the_record_stores_the_inner_record() {
    // POST /conversations {"data": {"subject": "a"}} answering 201 with only a Resource-ID header; world request.record "/data":
    // the stored record holds subject "a" (not a "data" member); with no world it is shaped from the whole body, as today
}
#[tokio::test] async fn the_identity_member_is_tried_before_an_alternate() {
    // record A has TemplateId "x"; record B has Alias "x": GET /templates/x → A (by_path_fields' field order)
}
#[tokio::test] async fn a_create_that_stores_nothing_is_marked_no_record() { /* answer with no id anywhere and no description: request.world_gap contains "no_record" */ }
#[tokio::test] async fn a_minted_id_the_schema_refuses_stores_nothing() { /* pattern makes the id exceed maxLength: generated body served, no record, world_gap no_record */ }
#[tokio::test] async fn a_minted_id_the_header_schema_refuses_is_not_placed() {
    // identity_header "Resource-ID" declared {type: string, format: uuid}; pattern "folders/{id}":
    // no Resource-ID header from the mint, no record, world_gap no_record
}
#[tokio::test] async fn an_undescribed_type_adds_no_scan() {
    // counting state store, no world and a tier-0 world: a missed update, delete, read and parent lookup make
    // exactly the list calls they make on main (no extra MAX_SCAN listing)
}
#[tokio::test] async fn consistency_off_adds_no_scan_to_the_update_delete_lookup() { /* described alternates, rt.consistency false: same list calls as no world */ }
#[tokio::test] async fn no_record_is_logged_once() { /* static gap no_record and a create that stores nothing: world_gap == ["no_record"] */ }
#[tokio::test] async fn a_tier0_update_of_a_held_record_logs_no_no_record() {
    // Tier-0 world and no world, one runtime each. Create a thing (stored), then:
    // PATCH /things/{id} answering the full record, and PATCH answering a record of another schema
    // (merged onto the held record by onto_record): neither recorded call carries "no_record" in world_gap
    // (the op's static gaps hold none), and the stored record has the merged members.
    // Also a DELETE and a first list (Seeded): no "no_record".
    // tier0_world_answers_as_no_world compares answers only, so this test guards the log.
}
#[tokio::test] async fn an_update_of_an_unheld_record_answering_an_ack_is_marked_no_record() { /* consistency off, nothing held, answer {ok: true}: world_gap holds "no_record" once */ }
#[tokio::test] async fn a_create_the_state_store_refuses_is_not_a_world_gap() { /* state breaker open: the create's record is located (stored 1), no "no_record" mark */ }
#[tokio::test] async fn a_revision_that_changes_the_identity_member_still_reaches_earlier_records() {
    // create under identity "id"; put a revision with identity.member "slug"; GET /things/{slug} → state (scan by the new member);
    // DELETE /things/{slug} → the earlier record is gone; a new create is stored under its slug
}
#[tokio::test] async fn a_revision_that_adds_a_pattern_still_reaches_bare_keyed_records() {
    // create stored under "7"; revision adds pattern "folders/{id}"; GET /folders/7 and GET by "folders/7" both → state;
    // PATCH by "folders/7" updates the record stored under "7"; DELETE by "folders/7" removes it
}

// mb-import/tests/it/world_tier0.rs
#[test] fn tier0_emits_identity_header_and_alternates_where_the_heuristics_detect_them() {
    // Resource-ID create → operations.createConversation.answer.identity_header == "Resource-ID";
    // the alias fixture → types.template.identity.alternates == ["Alias"];
    // a type with detected aliases and no tier-0 member → no types.<t>.identity entry; every repo contract: apply_of_tier0_is_identity still holds
}
```

`apply_of_tier0_is_identity` (Task 1) and `tier0_world_answers_as_no_world` (Task 3) must still pass. Both hold only because the runtime reads `identity_header` and `alternates` through the same functions with or without a world, tier 0 restates what the detectors find on the types it gives a member, and the detector answers directly for the types it gives none (the tier-0 alternates rule).

- [ ] **Step 2: Run and verify the tests fail.** `cargo test -p mb-protocols --test it described_identity && cargo test -p mb-import --test it world_tier0` → FAIL. Only the first test may already pass, through Task 1's `apply`. If it does, keep it as the regression guard.
- [ ] **Step 3: Implement** per the Interfaces and the data flow above.
  - A record a create makes with no identity value gets its id from `settle_create`, not from `write_state`.
  - `long_running.done` is set to `true` in the answer, in `settle_create`, when the member is a boolean.
- [ ] **Step 4: Run and verify the tests pass.** `cargo test -p mb-protocols && cargo test -p mb-import` → PASS, including the fix-1 and fix-5 tests from `claude/header-ids-alt-keys`, unchanged.
- [ ] **Step 5: Commit** `git commit -m "runtime: identity, record pointers, bulk and long-running creates from the world"`

### Task 6: Operation kind, target, transition and changes

**Files:**
- Fill: `crates/mb-protocols/src/call/transitions.rs` (Task 3's stub, already declared `mod transitions;` in `call.rs`; this task adds no `mod` line)
- Prerequisite: `claude/effect-members` (blind11 fixes 6, 12, 15) merged; it edits `write_memory.rs` too.
- Modify: `crates/mb-protocols/src/call.rs`. In the `write_state` update and action arms, after a 2xx, call `transitions::apply(..)`. Each return keeps the `WriteOutcome` Task 5's table gives it.
- Modify: `crates/mb-protocols/src/call/write_memory.rs`. For a `Replace`, `onto_record` replaces instead of merging. The request members laid onto the held record (the merge `claude/effect-members` adds for fix 6) come from `records::request_record(rt, op, body)` (Task 5, Decision 10), not the whole body; a `Replace` replaces with that value too.
- Test: `crates/mb-protocols/tests/it/described_kinds.rs` (Task 3's stub; this task fills it and edits no other file in `mb-protocols/tests/it`)

**Interfaces:**
- Consumes: `Described::op` (Task 3); `apply` kind/target (Task 1); `records::request_record` (Task 5).
- Produces:
  - `transitions::apply(rt: &MockRuntime, op: &Operation, request: Option<&Value>, record: &mut Value) -> Vec<String>`. It returns the members it set: `transition` members (each value fit to its member's schema), then `changes` (`from_request` pointer or literal `value`).
  - `transitions::replaces(rt, op) -> bool`.

- [ ] **Step 1: Write the failing tests**

```rust
#[tokio::test] async fn an_action_described_as_delete_removes_the_record() {
    // POST /workspaces/{id}/actions/safe-delete → 204; world kind delete; GET /workspaces/{id} → 404 (deleted marker), not state 200
}
#[tokio::test] async fn a_body_target_reads_from_state() { /* RPC POST /files/get {"path": "/a"} with kind read, target.member "path" → state */ }
#[tokio::test] async fn a_transition_sets_the_state_whatever_the_model_answered() {
    // POST /folders/{id}:archive, transition {state: ARCHIVED}; model answers state ACTIVE; stored and served state ARCHIVED
}
#[tokio::test] async fn changes_apply_request_members_to_the_target() { /* :move with changes {parent: {from_request: /destinationParent}} */ }
#[tokio::test] async fn an_update_whose_request_wraps_the_record_merges_the_inner_record() {
    // PATCH /things/{id} {"data": {"title": "b"}} answering an Ack; world request.record "/data":
    // the held record's title becomes "b" and gains no "data" member; with no world the merge is as today
}
#[tokio::test] async fn a_replace_drops_members_it_does_not_send() { /* PUT kind replace: an unsent optional member is gone, identity and readOnly members kept */ }
#[tokio::test] async fn a_described_delete_on_a_graphql_mutation_removes_the_record() { /* GraphQL mutation removeWidget(id:) kind delete target.member id */ }
```

- [ ] **Step 2: Run and verify the tests fail.** `cargo test -p mb-protocols --test it described_kinds` → FAIL. The first two pass through `apply` alone; if they do, keep them as guards.
- [ ] **Step 3: Implement.** Transition and change values are set on the stored record and on the answer's record, when the answer holds it. The record is then validated: a value that does not fit is dropped, and the drop is logged with `tracing::info!`.
- [ ] **Step 4: Run and verify the tests pass.** `cargo test -p mb-protocols` → PASS.
- [ ] **Step 5: Commit** `git commit -m "runtime: described kinds, targets, transitions and changes"`

### Task 7: Lifecycle, soft delete and `not_found`

About 42 checks across blind7-10, plus 32 in blind11.

**Files:**
- Fill: `crates/mb-protocols/src/call/lifecycle.rs` (Task 3's stub, already declared `mod lifecycle;` in `call.rs`; this task adds no `mod` line)
- Modify:
  - `crates/mb-protocols/src/call.rs` (`write_state` delete arm: `lifecycle::soft_delete(..)` instead of `state.delete` when described, still returning `WriteOutcome::default()`; create arm: `lifecycle::initial(..)`, which changes no `WriteOutcome`);
  - `crates/mb-protocols/src/call/deleted.rs` (a soft-deleted record is no deleted marker; `not_found` status);
  - `crates/mb-protocols/src/call/list_query.rs`:
    - add `ListQuery.lifecycle: Option<lifecycle::Exclusion>`, filled by `of` from the list's item type;
    - **`of` returns `Some` also when the item type's effective lifecycle has a soft-delete state**, even when the op describes no `paging`, `filters`, `scope` or `order`. Such a `ListQuery` has every query key unset, so `keeps` runs the heuristic filters and `describes_query()` is false (Task 10 does not answer it from state);
    - `matches` and `keeps` exclude a soft-deleted record unless `listed_with` is sent. The exclusion is a key of its own: it applies whether or not `filters`, `scope`, `order` or `paging` is described, and it changes none of their fallbacks (a lifecycle-only `ListQuery` keeps the heuristic filters, scope, order and paging). Every path in Task 9's "Every list path on `main`" table reaches `keeps`, so a soft-deleted record, which stays stored with its state set, is never put into a default list by any of them:
      - `lists::narrow` drops it from the answer's items (callers: `complete`, `pages::serve`, `runs::serve`);
      - `lists::overlay_stored` can turn an answer item into the soft-deleted record's view; the `narrow` that follows at each of its three callers drops it;
      - `lists::stored_rest`/`stored_page` (through `stored_records`) and `lists::collection_records` (and so `ranges::apply`) skip it when gathering held records;
      - `pages::serve`/`runs::serve` drop it from a kept cut's next page through their `overlay_stored` + `narrow`, so a record soft-deleted after the first page was minted is not served on a later one;
      - Task 10's `state_lists::answer` skips it (Task 10 runs after this task and calls `keeps`).
      So `lists.rs`, `pages.rs`, `runs.rs` and `ranges.rs` need no edit here;
  - `crates/mb-protocols/src/call/repeats.rs` (`described_conflict` from Task 8 skips records that `lifecycle::Exclusion` marks soft-deleted, so a soft-deleted record's unique values are free again).
- Prerequisite: #253 (`claude/engine-error-bodies`) merged; `not_found` builds on its not-found status and body.
- Test: `crates/mb-protocols/tests/it/described_lifecycle.rs` (Task 3's stub; this task fills it and edits no other file in `mb-protocols/tests/it`)

**Interfaces:**
- Consumes: `Described::state_value` (Task 3); `ListQuery` (Task 9); `repeats::described_conflict` (Task 8); the delete arm as Task 6 left it.
- Produces:
  - `lifecycle::initial(rt, t: &str, record: &mut Value, request: Option<&Value>)`, which sets the `initial` state when the request does not name the member;
  - `lifecycle::soft_delete(rt, t: &str, record: &mut Value) -> bool`;
  - `lifecycle::Exclusion { member: String, state: Value, unless: Option<(String, Value)> }`;
  - `lifecycle::not_found(rt, op) -> Option<u16>`.

- [ ] **Step 1: Write the failing tests**

```rust
#[tokio::test] async fn a_soft_delete_keeps_the_record_readable_in_its_state() { /* DELETE → 200 GET with state DELETE_REQUESTED */ }
#[tokio::test] async fn a_soft_deleted_record_leaves_the_default_list() {}
#[tokio::test] async fn a_soft_deleted_record_is_not_added_back_by_the_held_record_paths() {
    // the list op describes nothing but its type has a lifecycle (of is Some through the lifecycle only);
    // create two, soft delete one, list: the generated list is not given the soft-deleted one by overlay_stored
    // or stored_rest (a full page whose rest the store holds), and name-matched filters still narrow as on main
}
#[tokio::test] async fn a_record_soft_deleted_after_the_first_page_is_not_on_the_next() {
    // a paged list (kept cut, pages::serve); soft delete a record of the cut's rest; the next page leaves it out
}
#[tokio::test] async fn a_range_paged_list_leaves_soft_deleted_records_out() { /* Range header list: collection_records skips it */ }
#[tokio::test] async fn listed_with_lists_it_again() { /* ?showDeleted=true includes it */ }
#[tokio::test] async fn a_boolean_soft_delete_state_is_stored_as_true() { /* member is_deleted boolean, state "true" */ }
#[tokio::test] async fn a_create_starts_in_the_initial_state() { /* model answered state PENDING, initial ACTIVE → ACTIVE */ }
#[tokio::test] async fn a_soft_deleted_records_unique_values_are_free() { /* unique [displayName]; create, soft delete, create same displayName → 201 */ }
#[tokio::test] async fn a_hard_deleted_read_answers_the_described_not_found_status() {
    // op declares only 422 {ErrorCode, Message}; not_found 422 → 422 with a body valid against it (Engine::refuse_from_state)
}
```

- [ ] **Step 2: Run and verify the tests fail.** `cargo test -p mb-protocols --test it described_lifecycle` → FAIL.
- [ ] **Step 3: Implement.** A soft delete upserts the record with the state set and writes no deleted marker. The delete's answer is the declared 2xx as today.
- [ ] **Step 4: Run and verify the tests pass.** `cargo test -p mb-protocols` → PASS.
- [ ] **Step 5: Commit** `git commit -m "runtime: described lifecycle, soft delete and not_found"`

### Task 8: Unique members, concurrency and idempotency

About 68 checks across blind7-10.

**Runs after Task 3 and before Task 5.** It is the second task to edit `call.rs` (see "Parallel tasks touch disjoint files"); Task 5 rebases on it.

**Files:**
- Fill: `crates/mb-protocols/src/call/replays.rs` (Task 3's stub, already declared `mod replays;` in `call.rs`; this task adds no `mod` line)
- Modify:
  - `crates/mb-protocols/src/call/repeats.rs` (`described_conflict(..)`, checked first);
  - `crates/mb-protocols/src/call/tokens.rs` (`check` and `after_update` take a described pairing, confirmed from the first write);
  - `crates/mb-protocols/src/call.rs`:
    - `prepare`: call `replays::replayed`, then `repeats::described_conflict`, before generation;
    - `complete`: one call site, `replays::keep`, after the answer is final;
    - `repeated_record`: its idempotency guard also counts the described header;
  - `crates/mb-protocols/src/call/value_index.rs` (`same_values`: the same guard);
  - `crates/mb-engine/src/guards.rs` (`carries_an_idempotency_key(bundle, op, call, described: Option<&str>)`: true also when the call carries the described idempotency header).
- Test: `crates/mb-protocols/tests/it/described_conflicts.rs` (Task 3's stub; this task fills it and edits no other file in `mb-protocols/tests/it`)

**Interaction with today's idempotency-key paths.** Today a create that carries a header the contract names as an idempotency key (`carries_an_idempotency_key`) skips `repeated_record` and `same_values`: a repeat may be a retry, so it is no conflict, and it is generated as a new create. With a described `idempotency` header:
1. `replays::replayed` runs first. A key already kept for this op in this session answers the kept response. Nothing else runs: no generation, no unique check, no store write.
2. A key not kept yet goes on as today. `carries_an_idempotency_key` is true, also for a described header the name heuristic misses, so `repeated_record` and `same_values` still skip. `described_conflict` still runs: a new key with a taken unique set is a new request and is refused.
3. `replays::keep` keeps the final answer (2xx or a refusal), so a retry of a refused create is refused the same way.
4. Without a described `idempotency`, nothing changes (Decision 11). Tier 0 does not emit `idempotency`, and this task adds no emission: a header that only `carries_an_idempotency_key`'s name rule detects keeps today's behaviour (skip repeat detection, generate a new create) under tier 0 and with no world (`Described::default()`). Replay is only for an author-described header. So `tier0_world_answers_as_no_world` holds for a contract with an `Idempotency-Key` header.

**Interfaces:**
- Consumes: `Described::{op, type_desc}` (Task 3); `Engine::refuse_from_state(bundle, op, call, code)`.
- Produces:
  - `repeats::described_conflict(state: &mut StateAccess<'_>, plan: &CallPlan<'_>, call: &OperationCall, target: Option<&str>) -> Option<u16>`. A create or update whose member set equals a held record's (another record for an update), within the same `scope` member value, answers `conflict`, else the first of `guards::refusal_codes(op)`. The scan is bounded by `MAX_SCAN`. (Task 7 later makes soft-deleted records not count.)
  - `tokens::described(rt, t: &str) -> Option<(Option<String> /*member*/, Option<String> /*header, lowercase*/, Option<u16>)>`.
  - `replays::replayed(state, plan, call) -> Option<GeneratedResponse>` and `replays::keep(state, plan, call, &GeneratedResponse)`. Keys live under `mb_state::INTERNAL_TYPE` as `#replay/<op id>/<sha256(header value)>`, holding status, body and declared headers. A replay with a different body still answers the first call's answer.

- [ ] **Step 1: Write the failing tests**

```rust
#[tokio::test] async fn a_taken_unique_set_is_refused_before_generation() { /* second create with same displayName under same parent → 409, provider called once */ }
#[tokio::test] async fn the_same_values_under_another_parent_are_accepted() {}
#[tokio::test] async fn an_update_onto_another_records_values_is_refused() {}
#[tokio::test] async fn a_stale_if_match_is_refused_on_the_first_write() { /* concurrency {member: etag, header: If-Match, conflict: 412} */ }
#[tokio::test] async fn a_described_token_advances_after_an_update() { /* integer version +1; string etag re-minted */ }
#[tokio::test] async fn if_match_star_and_no_token_pass() {}
#[tokio::test] async fn an_idempotent_replay_answers_as_the_first() { /* Idempotency-Key twice → same status/body, one record, provider called once; the replay's recorded call carries the op's world_gap (seeded in generate_narrowed's Answered arm, Task 3) */ }
#[tokio::test] async fn a_new_key_with_a_taken_unique_set_is_refused() {}
#[tokio::test] async fn a_described_key_header_the_name_rule_misses_skips_repeat_detection() { /* header "X-Request-Token" described as idempotency */ }
#[tokio::test] async fn a_replayed_refusal_is_refused_again() {}
#[tokio::test] async fn an_undescribed_idempotency_key_behaves_as_today() { /* create_repeats suite's key cases, unchanged */ }
#[tokio::test] async fn a_heuristic_idempotency_key_does_not_replay_under_tier0() {
    // Idempotency-Key declared, world {} (tier 0): the same key twice → two generated creates, two records,
    // provider called twice; equal to a runtime with no world
}
```

- [ ] **Step 2: Run and verify the tests fail.** `cargo test -p mb-protocols --test it described_conflicts` → FAIL.
- [ ] **Step 3: Implement** per the Interfaces. Refusal bodies come from `Engine::refuse_from_state`. When the operation declares no response for the code, nothing is refused and the call goes on as today.
- [ ] **Step 4: Run and verify the tests pass.** `cargo test -p mb-protocols` → PASS. The `concurrency*`, `create_repeats` and `create_refusals*` suites are unchanged.
- [ ] **Step 5: Commit** `git commit -m "runtime: described unique sets, concurrency tokens and idempotent replays"`

### Task 9: Paging, filters, scope and order

The biggest mechanism across blind7-10: about 175 checks.

**Files:**
- Prerequisite: #252 (`claude/param-styles-and-lists`, blind11 fixes 7, 8, 13) merged; it edits `runs.rs`, `lists.rs` and `pages.rs`.
- Fill: `crates/mb-protocols/src/call/list_query.rs` (Task 3's stub, already declared `mod list_query;` in `call.rs`; this task adds no `mod` line)
- Modify:
  - `crates/mb-protocols/src/call/lists.rs`, **key by key, on every path that puts records into a list** (see "Per-key fallback" below):
    - `narrow` (the generated list);
    - the held-record paths that `complete` runs on a generated list (`call.rs` ~1345-1360 on `main`): `overlay_stored`, `stored_rest`, `stored_page` (both through `stored_records`) and `collection_records`. On `main` they keep held records with the heuristic `filters_of`/`holds_wanted` and `in_collection`. Each now keeps a held record with `list_query::keeps(q, item, heuristic)` and orders what it adds with `list_query::sort` when `order` is described, so described filters, scope and order apply to the records a list adds, not only to the generated items. `in_collection` stays the collection-membership rule on all of them;
    - the **query gates** of `stored_records` (the `shown` loop, ~1008-1060: a query value that is not a known filter or the size parameter stops the store read) and of `collection_records` (~847: any query key that is not a `filters_of` filter returns `None`), per key: a parameter a described `filters` entry, `scope` or `paging` names counts as known, so a described parameter the name rule misses does not turn the store read off; every other parameter is judged by the heuristic gate as on `main`. `stored_records`' page detection (`requested_page`, `page_size`, `pages::slot`, `runs::starts`) takes the described `paging` when it is set and is unchanged when it is not.
  - **Every list path on `main`, and how each gets the rule.** Each list function computes `let q = list_query::of(rt, plan.op, call);` itself, so its callers are not edited:

    | Path | Where on `main` | What it puts in the list | How it applies the per-key rule (and Task 7's exclusion) |
    | --- | --- | --- | --- |
    | `lists::narrow` | `lists.rs` ~475; called from `call.rs` ~1349, `pages::serve` ~1597, `runs::serve` ~570 | keeps or drops the answer's items | `keeps(q, item, |i| !contradicts(..))` per item; `sort` when `order` is set; `page` when `paging` is set |
    | `lists::overlay_stored` | `lists.rs` ~1274; same three callers, each right before `narrow` | replaces an answer item's members with the held record's (adds no item) | none of its own: the `narrow` that follows it at every caller drops an overlaid item that `keeps` refuses (a soft-deleted one among them) |
    | `lists::stored_rest`, `lists::stored_page` → `stored_records` | `lists.rs` ~797, ~814, ~929; called from `call.rs` ~1354/1358 | appends held records past the page (the rest), or fills an absent list member | `keeps(q, record, |r| filters.all(holds_wanted))` per held record; the query gate per key (above); `sort` of what it appends when `order` is set |
    | `lists::collection_records` | `lists.rs` ~832; called from `ranges::apply` (`ranges.rs` ~214, `call.rs` ~1603) | the held records of a `Range`-paged list | `keeps` per held record; the query gate per key. `ranges::apply` adds the answer's own items after them, which `narrow` already kept; it sorts by the `Range` field, so a described `order` does not apply there (the client's `Range` order wins) |
    | `pages::serve`, `runs::serve` | `pages.rs` ~1544, `runs.rs` ~496 | the next page of a kept cut (items captured at mint, held records among them) | through the `overlay_stored` + `narrow` they already run (rows above), so a record soft-deleted or no longer matching since the mint is dropped |
    | `state_lists::answer` | Task 10 | the held records of a described list | Task 10 (`keeps`, `sort`, `page` key by key) |

    Removing paths add nothing and are not changed: `deleted::drop_from_list` (delete markers), `pages::drop_served`, `pages::followed`. `lists::absent_list` inserts only `[]`. Task 9's first step re-runs `git grep -n "lists::\|pages::serve\|runs::serve\|ranges::apply" crates/mb-protocols/src` and adds any list path this table misses;
  - `crates/mb-protocols/src/call/runs.rs`: `position` takes a described `page`/`offset` parameter;
  - `crates/mb-protocols/src/call/pages.rs`: `slot`/`header_slot` take a described `response`/`response_header`;
  - `crates/mb-protocols/src/call/cursors.rs`: a described cursor parameter is shown.
- Test: `crates/mb-protocols/tests/it/described_lists.rs` (Task 3's stub; this task fills it and edits no other file in `mb-protocols/tests/it`)

**Interfaces:**
- Consumes: `Described::op` (Task 3).
- Produces:
  - `list_query::ListQuery { filters: Vec<Filter>, scope: Option<(String, String)>, order: Option<(String, bool)>, page: Option<PageAsk> }`. Task 7 adds a `lifecycle` field. An empty `filters` means "filters not described"; `None` for the others means that key is not described.
  - `ListQuery::describes_query(&self) -> bool`: true when any of `filters` (non-empty), `scope`, `order` or `page` is set. Task 10 gates lists from state on it, not on `of` being `Some` (Task 7 makes `of` `Some` for a lifecycle-only type too).
  - `list_query::Filter { member: String, kind: FilterKind, values: Vec<Value> }`.
  - `list_query::PageAsk { style: PagingStyle, position: Option<String>, size: Option<usize>, response: Option<String>, response_header: Option<String> }`.
  - `list_query::of(rt: &MockRuntime, op: &Operation, call: &OperationCall) -> Option<ListQuery>`: `Some` when the effective world describes at least one of the op's `paging`, `filters` (non-empty), `scope` or `order`, else `None` (and every path behaves as on `main`). Task 7 widens the rule.
  - `list_query::matches(q: &ListQuery, item: &Value) -> bool`: the described keys only (filters when non-empty, scope; Task 7 adds the exclusion).
  - `list_query::keeps(q: Option<&ListQuery>, item: &Value, heuristic: impl Fn(&Value) -> bool) -> bool`: the one predicate every list path uses. `heuristic` is that path's own filter test on `main` (`contradicts` over `filters_of` in `narrow`, `holds_wanted` in the held-record paths). With `q` `None`, or `q.filters` empty, the heuristic filter test runs; with described filters, the heuristic filter test is skipped and the described filters run. A described scope is checked in addition. (Task 7 adds the exclusion here, so it applies whatever `of` returned for the other keys.)
  - `list_query::sort(q: &ListQuery, items: &mut [Value])`.
  - `list_query::page<'a>(q: &ListQuery, items: &'a [Value]) -> (&'a [Value], Option<String> /*next position*/)`.
- Filter semantics, fixed here:
  - `eq`: query text against the member by value (`"3"` = `3`, `"true"` = `true`); an array parameter is any-of.
  - `contains`: on an array member, any item equal to the value, or an object item with exactly one string member equal to it; on a string member, a case-sensitive substring.
  - `gte`/`lte`: numbers by value; RFC 3339 date-times when both sides parse as one; otherwise no narrowing.
  - `flag`: the member equals the boolean.
  - `text`: a case-insensitive substring over the item's string members, or over `member` when given.
  - A value that does not parse as the member's type narrows nothing.
- **Per-key fallback** (Decision 8; the spec's "an absent key falls back to today's inference"). Each described key replaces only its own heuristic, on every path of the table above and in Task 10. `of` returning `Some` never turns a heuristic off by itself; only a set key does:
  - `filters` set (non-empty): replaces the name-matched query filters (`filters_of`). Unset or empty: `filters_of` as on `main`.
  - `scope` set: the described parameter/member pair is checked as well; the held-record paths keep `in_collection`. Unset: today's behaviour.
  - `order` set: the matched items, the held records added included, are sorted by it. Unset: today's order (the model's, then the store's append order).
  - `paging` set: replaces today's page parameter and slot detection (`runs::position`, `pages::slot`/`header_slot`, `page_size`). Unset: as on `main`.
  So a description that sets only `paging` keeps the name-matched filters, and one that sets only `order` keeps both filters and paging.
- Paging semantics: the position is an offset into the matched, ordered items.
  - `cursor`/`next_url`/`header_cursor` mint opaque tokens with the existing `pages` token format.
  - `page` is 1-based unless the parameter's `minimum` is 0.
  - The size is the request's size parameter, else its declared `default`, else no cut.
- **Re-validation before serving.** A generated list that `lists::narrow` narrows or `list_query::page` cuts is an engine-made value (Global Constraints). After narrowing and paging, and after the next position is written into the body (`paging.response`) or a header (`paging.response_header`), the body is validated with `Engine::serve_stored` against the op's declared 2xx schema, and the header value against that header's declared schema. When either fails (a `minItems` the cut breaks, a token over the member's `maxLength`, a required member a cut emptied), the answer falls back to the generated list as it was before narrowing, with no token written, and the fallback is logged with `tracing::info!`.

- [ ] **Step 1: Write the failing tests**

```rust
#[tokio::test] async fn a_described_page_number_reaches_the_second_page() {
    // page[number] with minimum 1, no default; paging {style: page, request: "page[number]", size: "page[size]"}
    // page 2 of size 2 holds items 3-4 of the first answer's run, not page 1 again
}
#[tokio::test] async fn a_described_cursor_pages_through_and_ends() { /* pageToken/nextPageToken; last page has no nextPageToken */ }
#[tokio::test] async fn a_header_cursor_is_served_in_its_header() { /* response_header "X-Next-Cursor" */ }
#[tokio::test] async fn a_nested_member_filter_narrows() { /* filters [{param: email, member: "billing_address.email", kind: eq}] */ }
#[tokio::test] async fn a_contains_filter_matches_an_array_of_objects() { /* tag=urgent vs tags: [{tag: "urgent"}] */ }
#[tokio::test] async fn scope_keeps_only_the_parents_records() { /* scope {param: parent, member: parent} */ }
#[tokio::test] async fn order_descending_sorts_newest_first() { /* order {member: createdAt, descending: true} */ }
#[tokio::test] async fn an_unknown_page_token_falls_back_to_generation() { /* token never minted → source llm, no cut */ }
#[tokio::test] async fn a_mistyped_filter_value_narrows_nothing() { /* minAmount=abc on gte over integer */ }
#[tokio::test] async fn a_page_past_the_end_is_empty_with_no_next() {}
#[tokio::test] async fn a_described_paging_alone_keeps_name_matched_filters() {
    // GET /contacts?email=a@x with a name-matched email filter on main; world sets only paging for listContacts:
    // the answer is narrowed to email a@x exactly as with no world, and paged by the described parameters
}
#[tokio::test] async fn a_described_order_alone_keeps_filters_and_paging() { /* world sets only order: same items and page cut as no world, sorted */ }
#[tokio::test] async fn empty_described_filters_keep_name_matching() { /* filters [] plus order: name-matched filters still narrow */ }
#[tokio::test] async fn held_records_a_list_adds_follow_the_described_query() {
    // two held contacts (created this session), one matching a described nested-member eq filter
    // (billing_address.email) and scope, one not: the generated list gains only the matching one through
    // overlay_stored/stored_rest, in the described order; with no world the held records are added as on main
}
#[tokio::test] async fn a_narrowed_page_the_schema_refuses_is_served_unnarrowed() { /* items minItems 1, filter matches none → the generated list, no next token */ }
#[tokio::test] async fn a_next_token_the_schema_refuses_is_not_written() { /* nextPageToken maxLength 4 → the generated list, unnarrowed, no token */ }
```

- [ ] **Step 2: Run and verify the tests fail.** `cargo test -p mb-protocols --test it described_lists` → FAIL.
- [ ] **Step 3: Implement** `list_query.rs` per the semantics above, then switch `lists.rs` (`narrow` and the four held-record paths, through `keeps` and `sort`), `runs.rs`, `pages.rs` and `cursors.rs` to consult it key by key.
- [ ] **Step 4: Run and verify the tests pass.** `cargo test -p mb-protocols` → PASS. The existing `world_lists`, `runs` and `pages` tests are unchanged.
- [ ] **Step 5: Commit** `git commit -m "runtime: described paging, filters, scope and order"`

### Task 10: Lists answered from state

**Files:**
- Fill: `crates/mb-protocols/src/call/state_lists.rs` (Task 3's stub, already declared `mod state_lists;` in `call.rs`; this task adds no `mod` line)
- Modify: `crates/mb-protocols/src/call.rs` (`prepare`: a described list tries `state_lists::answer` before generation; `complete`'s seeding and `write_state`'s create arm call `state_lists::mark_listed`)
- Modify: `crates/mb-protocols/src/call/lists.rs`: `in_collection`, `filters_of` and `holds_wanted` become `pub(super)` so `state_lists` reuses them; their bodies do not change.
- Test: `crates/mb-protocols/tests/it/described_state_lists.rs` (Task 3's stub)

**Interfaces:**
- Consumes: `list_query::{of, keeps, sort, page}` and `ListQuery::describes_query` (Tasks 9 and 7); `lists::{filters_of, holds_wanted}` (the heuristic filter test); `lists::in_collection` (which reads `rt.collections()`); `collection_of`; `Engine::held_answer`/`serve_stored`.
- Produces: `state_lists::answer(state: &mut StateAccess<'_>, plan: &CallPlan<'_>, call: &OperationCall) -> Option<GeneratedResponse>`, tagged `Source::State`.
  - It runs only when `list_query::of` is `Some` and `describes_query()` is true (Decision 8; a lifecycle-only `ListQuery` is not enough).
  - It gathers the held records of the item type, at most `MAX_SCAN`, keeps those **in the collection** the call's path names (the membership rule below), then keeps, sorts and pages them **key by key** (Task 9's per-key fallback): `list_query::keeps` with the held-record paths' heuristic (`holds_wanted` over `filters_of`) when `filters` is not described, the described filters when it is; the described `scope` checked in addition to the membership rule when it is set, the membership rule alone when not; the soft-delete exclusion always (its own key, Task 7); `sort` only when `order` is described (else the store's order); the described page, else today's page size rule. No key's heuristic is turned off because another key is described.
  - **Membership rule.** A held record is in the collection when either:
    - its id is in the collection's persisted member list (the `#listed` mark's `ids`, below), or
    - `lists::in_collection` says so, reused as it is: a route with no path parameters is one collection; else `rt.collections()` (this process's in-memory placement, empty after a rebuild), else every path parameter is held by a member tied to it.
    The persisted list is what survives a rebuild: `Collections` is in memory and every `put_world`/`patch_world` rebuild empties it, so without the list a child whose members do not name its parent would be dropped after a revision, and nothing else would place it. The member fallback cannot mix other parents' children in: it requires, for each path parameter, a member tied to that parameter holding the call's value, so a child of `p2` with a parent member does not match `p1`, and a child of `p2` with no parent member is in `p2`'s list only. `lists::in_collection` is not changed and is still what `stored_rest` uses for undescribed lists.
  - It places the page at `answer.records`, else at the list member `lists::list_parts` finds.
  - It writes the next position at `paging.response` or `response_header`.
  - It fills the envelope's other required members as `held_answer` does.
  - It returns `None`, so the list is generated, when the collection was neither listed nor written this session, or the answer does not validate.
  - **"Listed or written", and membership, are kept in the state store, not in memory.** `Collections` is reset by every rebuild, including the one each `put_world`/`patch_world` triggers. A session-scoped record under `mb_state::INTERNAL_TYPE`, `#listed/<item type>/<collection key>` (the collection key is the one `collection_of` in `call.rs` gives, so `/parents/p1/children` and `/parents/p2/children` have separate marks), holds `{ids: [..]}`: the ids of the records the seeding placed there, and of each create that landed there since. `mark_listed` writes it when a list of that collection is seeded (all the seeded ids at once) and appends the new id when a create lands in it (one upsert per create into a described collection). `answer` reads the mark once per described list; with no mark it returns `None` and the list is generated and seeded as today. After a rebuild, the next described list is answered from state with the same members it had before; it is not generated and seeded again (`WriteOrigin::Seeded`), so model records are never mixed into a collection that holds real ones.
    - A deleted record needs no mark update: it is no longer held, so it is not gathered.
    - The list is capped at `MAX_SCAN` ids. A collection that would pass the cap stops being answered from state (`answer` returns `None`, the list is generated as today), so a list from state never drops a member silently.
    - Two creates into one collection in the same session at the same moment can race on the upsert and lose one id. That record is still found by the member fallback when its members name the parent, and otherwise it leaves the list until the next seeding. This is accepted and stated in `state_lists.rs`'s module doc.
  - Records stored before a revision that changed the type's identity member are gathered like any other held record of the type; their stored key does not matter to a list.

- [ ] **Step 1: Write the failing tests**

```rust
#[tokio::test] async fn a_second_list_is_answered_from_state_with_the_new_record() { /* list (generated, seeded), create, list → state, includes it, provider called twice not three times */ }
#[tokio::test] async fn a_described_filter_answers_from_state_exactly() {}
#[tokio::test] async fn pages_from_state_never_repeat_an_item() {}
#[tokio::test] async fn a_list_page_whose_envelope_cannot_be_filled_is_generated() { /* required free-number member → source llm */ }
#[tokio::test] async fn an_undescribed_list_is_generated_as_today() {}
#[tokio::test] async fn a_list_from_state_with_only_paging_keeps_name_matched_filters() { /* second list ?email=a@x, world sets only paging: from state, only a@x's records */ }
#[tokio::test] async fn a_list_from_state_carries_the_ops_world_gap() { /* answered in prepare by state_lists::answer: the recorded call has the same world_gap as a generated list of that op */ }
#[tokio::test] async fn a_world_revision_mid_session_keeps_lists_from_state() {
    // list (generated, seeded), create; put a revision (rebuild); list → source state, exactly the seeded items plus the created one,
    // no new Seeded records, provider called twice not three times
}
#[tokio::test] async fn a_nested_list_keeps_its_parents_children_across_a_rebuild() {
    // described list GET /parents/{p}/children whose child schema has no member naming the parent.
    // List p1 (generated, seeded: c1, c2), list p2 (seeded: c3), create c4 under p1; put a revision (rebuild, Collections empty);
    // list p1 → source state, exactly c1, c2, c4 (no c3); list p2 → exactly c3; provider called three times, not five.
    // A second contract variant whose children carry parentId: the same answers, so the member fallback and the mark agree.
}
#[tokio::test] async fn a_collection_past_the_cap_is_generated() { /* MAX_SCAN ids in the mark, one more create: the next list is source llm */ }
#[tokio::test] async fn lists_after_an_identity_change_still_hold_earlier_records() {}
```

- [ ] **Step 2: Run and verify the tests fail.** `cargo test -p mb-protocols --test it described_state_lists` → FAIL.
- [ ] **Step 3: Implement** per the Interfaces.
- [ ] **Step 4: Run and verify the tests pass.** `cargo test -p mb-protocols` → PASS.
- [ ] **Step 5: Commit** `git commit -m "runtime: described lists answered from state"`

### Task 11: Derived counts and sums

**Files:**
- Fill: `crates/mb-protocols/src/call/derived.rs` (Task 3's stub, already declared `mod derived;` in `call.rs`; this task adds no `mod` line)
- Modify: `crates/mb-protocols/src/call.rs` (`serve_held` and the generated-answer path of a held record call `derived::fill`), `crates/mb-protocols/src/call/state_lists.rs` (each item)
- Test: `crates/mb-protocols/tests/it/described_derived.rs` (Task 3's stub; this task fills it and edits no other file in `mb-protocols/tests/it`)

**Interfaces:**
- Consumes: `Described::type_desc` (`parent`, `derived`); `rt.collections()`; `StateAccess::list`.
- Produces:
  - `derived::Children`, the held children of one type for one session, grouped by parent id. It is built with **one** `state.list(session, of, MAX_SCAN)` per child type, and is `cut` when the listing returned `MAX_SCAN` records.
  - `derived::fill_page(state, session, t: &str, items: &mut [Value]) -> Vec<Vec<String>>`, for a list from state. It builds one `Children` per derived child type of `t`, then fills every item from those groups.
  - `derived::fill(state: &mut StateAccess<'_>, session: &SessionKey, t: &str, record: &mut Value) -> Vec<String>`, for one record. It is `fill_page` over one item, and returns the members it set;
  - `derived::Decimal::{parse(&Value) -> Option<Decimal>, add, to_value(&self, member_schema: &Value) -> Option<Value>}`.
- Rules:
  - A child is a held record of type `of` whose `parent.member` (pattern-aware) equals the record's identity value, or which is stored under a collection whose `parent.path_param` value is that id.
  - `sum` adds `member` over the children matching `where`.
  - The result goes into an `integer` member only when integral, into a `number` member as a number, and into a `string` member as a decimal string at the children's largest scale.
  - Nothing is set unless the record was created this session or a child under it is held. A result that does not fit the member's schema is not set.
- **Request-path bound.** An answer costs at most one children listing per derived child type of the answered type, whatever the page size: a page of N items costs the same store reads as one record, not N scans. A type has at most `MAX_DERIVED_CHILD_TYPES` = 4 child types consulted; derived members over further child types are not set, and the skip is logged once per runtime with `tracing::info!`. When a listing is `cut` (it hit `MAX_SCAN`), the result is not exact, so nothing is set from it and the model's value stays.

- [ ] **Step 1: Write the failing tests**

```rust
#[tokio::test] async fn a_count_follows_children_created_and_deleted() { /* order.line_count after 2 lines, then 1 */ }
#[tokio::test] async fn a_sum_filters_by_state() { /* balance = sum(amount where status posted) */ }
#[tokio::test] async fn a_sum_into_a_string_member_keeps_the_children_scale() { /* "12.50" + "1.25" → "13.75" */ }
#[tokio::test] async fn a_generated_parent_with_no_held_children_keeps_the_models_value() {}
#[test] fn decimal_adds_exactly() { /* 0.1 + 0.2 == 0.3 */ }
#[tokio::test] async fn a_page_of_parents_lists_the_children_once() { /* counting state store: list of 10 orders from state → exactly one list call for line */ }
#[tokio::test] async fn a_children_listing_cut_at_max_scan_sets_nothing() {}
```

- [ ] **Step 2: Run and verify the tests fail.** `cargo test -p mb-protocols --test it described_derived && cargo test -p mb-protocols derived` → FAIL.
- [ ] **Step 3: Implement** per the Interfaces.
- [ ] **Step 4: Run and verify the tests pass.** `cargo test -p mb-protocols` → PASS.
- [ ] **Step 5: Commit** `git commit -m "runtime: derived counts and sums from stored records"`

### Task 12: Docs, changelog and the no-regression run

**Files:**
- Modify: `docs/world.md`:
  - replace the "runtime does not yet answer from it" note with an "At runtime" section, one paragraph per module, with the `world_gap` request-log field;
  - document the hot reload.
- Modify: `CHANGELOG.md`, and `plugins/mockingbird/skills/mockingbird-world/SKILL.md` (gaps in the request log tell the assistant what to describe next).
- Test: `crates/mockingbird/tests/it/docs_reference.rs`, which checks the docs links.

- [ ] **Step 1: Write the docs**, with each runtime behaviour as Tasks 3–11 fixed it.
- [ ] **Step 2: Run the doc gates.** `cargo test -p mockingbird --test it docs_reference && RUSTDOCFLAGS="-D warnings" cargo doc --workspace --all-features --no-deps` → PASS.
- [ ] **Step 3: Run the heuristic regression on the blind sets.**
  - Run blind1–11 on the default model with no revisions, so tier 0 is in effect, and compare with the latest committed results.
  - Expected: no suite drops beyond its run-to-run spread.
  - Stop only your own proxy, by PID. Check that the spend cap is not turning calls into faker answers.
  - The `--world agent` measurement is step 4's.
- [ ] **Step 4: Commit** `git commit -m "docs: the runtime reads the world description"`

---

## Not in this plan

- **The blind11 "engine now" fixes:** 1 and 5 (`claude/header-ids-alt-keys`), 2 and 3 (#253), 6, 12 and 15 (`claude/effect-members`), 7, 8 and 13 (#252), plus 9 and 11 (no branch yet). They are structural fixes with no description needed, each group in its own PR. See "Base" and "Coordination" above.
- **A description that clears a pointer to nothing** ("this answer holds no record"). In v1 an author corrects a wrong `answer.record`, identity member or target by replacing it; `""` makes the answer itself the record (new syntax, added by Task 1 for `answer.record` and `request.record` only). An explicit "none" needs a spec amendment, because a JSON `null` reads as absent.
- **Replaying on a heuristically detected idempotency header.** Only an author-described `idempotency` header replays (Decision 11); tier 0 emits none.
- **Vocabulary the diagnosis asks for beyond Task 2:**
  - `errors` (`{status, cause} → member values`), for blind11 mechanism 2, engine-made error bodies;
  - `masked` write-only members, for blind11 mechanism 9, a withheld sensitive value the request echo puts back;
  - child records of a fixed kind made from a create's request array, for blind11 mechanism 14 (e.g. order lines, or a note becoming a `type: note` thread).

  Each needs a spec amendment first.
- **Step 4** (`--world` bench tiers), **step 5** (convention detectors) and **step 6** (`mockingbird world draft`). Detectors from step 5 produce tier-0 entries. This plan's runtime reads them with no further change, because `apply` and `Described` read the effective world whatever its author.
