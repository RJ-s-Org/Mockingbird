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
| `claude/header-ids-alt-keys` (not opened yet; queued after #252 and #253) | 1 (a create whose id is only in a response header: `-id`/`_id`/`ID` header or `Location`), 5 (a path value that names a record by either of two members) | `write_state` create arm, `mint_create_id`, `by_path_fields` field list, `http_rest/handler.rs` header placement | 2, 5 |
| `claude/effect-members` (queued after `claude/header-ids-alt-keys`) | 6 (ack update drops request members), 12 (effect re-homed under `attributes`), 15 (effect id punctuation) | `write_memory.rs` (`named` exclusion, merge onto the held record), `mended_set` | 6 (both edit `write_memory::onto_record`) |

Fixes 9 (echo over a withheld null) and 11 (JSON Patch bodies) have no branch yet and touch no task here. Three blind11 mechanisms are world work in this plan, not engine fixes: 4 (soft delete / archive) is Task 7; 10 (an action route that deletes) is Tasks 1 and 6; 14 (child records from a create's request array, and sums derived from them) is Task 5 (`request.records`) and Task 11 (sums), except a child of a fixed kind (see "Not in this plan").

## Global Constraints

- **Optional, never required.** A contract alone still works. With no revision, the effective world is tier 0, and every answer stays as it is today (Task 1 and Task 3 prove this).
- **Validated, never trusted.** The runtime reads only the effective world: tier 0 overlaid with the newest revision's entries that validate (`mb_mcm::world::effective`, which both `mb_admin::world::effective_world` and the runtime's `runtime_world` call). It never reads a raw revision.
- **No API names in the engine.** No code path, rule or constant is keyed to an API, a vendor or an operation id. Test contracts are synthetic (folders, contacts, orders, templates).
- **Every answer still matches the contract.** An engine-made value can be a list page, a derived member, a transition state, a minted patterned id, an id header or a conflict body. Each one is checked against the declared schema the way a stored record is (`Engine::serve_stored`) before it is served. A value that does not fit falls back to the heuristic path (generation), never to an invalid answer.
- **A gap never refuses a request.** `world_gap` is a log mark only.
- **Heuristics stay.** Every module falls back to today's inference for each key the description leaves absent. No heuristic is deleted in this plan.
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
   - **A described list after a rebuild.** Expected: the next list of a collection the session already listed or wrote is still answered from state. It is not generated and seeded again (`WriteOrigin::Seeded`), so model records are never mixed into a collection that holds real ones. This holds because Task 10 keeps its "listed or written before" mark in the state store, not in `Collections`.
   - **A revision that changes a type's identity member or pattern.** Expected: records stored before stay under their old key and nothing is migrated. A read, update or delete by the new member's value still reaches them, because `by_path_fields` scans the type's current identity member (Task 5). A read by a bare or a patterned id reaches a record stored under either form (`records::bare_id`/`patterned`). New records are stored under the new member.

   Tests: Task 4 `a_world_revision_mid_session_keeps_stored_records`, Task 10 `a_world_revision_mid_session_keeps_lists_from_state`, Task 5 `a_revision_that_changes_the_identity_member_still_reaches_earlier_records` and `a_revision_that_adds_a_pattern_still_reaches_bare_keyed_records`.
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
   - **A store error never drops the description silently.** The runtime is built with the previous runtime's world when the previous runtime served the same contract version, else with `base` (tier 0, which needs no store, keeps the gaps marked, and changes nothing per Task 1). It is marked `world_pending`, and the registry retries the build with backoff (1 s doubling to 60 s) until the load succeeds or another mock change rebuilds it. The error is logged with `tracing::warn!`.
   - A request pays only `HashMap` lookups by operation id and type name.
2. **Hot reload by rebuild, as `put_scenario` does.**
   - `add_world_revision` (SQLite and Postgres) publishes `ControlChange::Mock` for the mock when the revision's contract version is the active one. The registry's watcher already rebuilds on that event, and HA bridges carry it to other nodes.
   - This covers every writer: `put_world`, `patch_world`, restore, carry-forward at activation, config-file `world:` and snapshot import.
   - No `ArcSwap` is added. A rebuilt runtime recomputes `BundleIndex` from the applied bundle.
3. **Two places the description lands.**
   - `mb_mcm::world::apply(bundle, base, world) -> Bundle` rewrites each operation where `world` differs from `base` (= `tier0(bundle)`, passed in so it is computed once per build). It compares field by field, so tier 0 is a no-op by construction. The fields are kind, resource type, `identity.path_param`/`input_field`, `identity.field` (from the type's `identity.member`) and `identity.record` (from `answer.record`, a deep pointer allowed).
   - **An absent field keeps the bundle's value.** `apply` writes only a world value that is set (`Some`) and differs from `base`'s; it never writes a `None`. With Task 2's field-wise overlay of `identity`, `target` and `answer`, an author who writes only `answer: {identity_header: X}` keeps tier 0's `answer.record`.
   - **A wrong pointer is corrected by replacing it, not by clearing it.** An author writes the right pointer; `""` (the RFC 6901 root) says the answer itself is the record. The same holds for a wrong identity member or target. A description that says "this answer holds no record at all" is not expressible in v1 (see "Not in this plan").
   - Everything else goes into `call::described::Described`. Applying the world to the bundle makes kind, target and pointer changes reach all of today's heuristics without editing them.
4. **`tier0` moves to `mb-mcm`** (`mb_mcm::world::tier0`), so `apply` can diff against it without a dependency from `mb-protocols` on `mb-import`. `mb_import::world::tier0` stays as a re-export.
5. **Deep record pointers.** `mb_mcm::RecordAt` gains `Pointer(Vec<String>)`, the RFC 6901 tokens.
   - In the token list, `"0"` means "the one item of an array" when the value there is an array, and a member named `0` otherwise.
   - `apply` writes the three existing variants where they fit (`/m` → `Member`, `/0` → `Item`, `/m/0` → `MemberItem`). So stored bundles and tier 0 never contain the new variant.
6. **How `world_gap` is marked.**
   - `mb_engine::GeneratedResponse` gains `world_gaps: Vec<String>`. Like `effect_outcomes`, it is filled by the caller in `mb-protocols`.
   - The static gaps of an operation come from `world_gaps::gaps(bundle, world, &[])`, computed once in `Described`: `no_identity` for its type, `no_target`, `no_record`.
   - The dynamic gap `no_record` is added when a 2xx create or update stored nothing. `write_state` cannot mark it (`WriteRequest.generated` is a shared reference), so it returns a `WriteOutcome` and `complete`, which owns the response, pushes the mark before returning it to the handler (Task 5).
   - `call::record_effects` writes them into the request record's `request` JSON as `"world_gap": ["no_identity", …]`, next to `effects`. A `tracing::info!` line carries `world_gap = …`.
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
   - This targets the largest mechanism across blind7-10, "List answers come from the model, not from state": about 175 checks on the default model.
9. **Vocabulary added here** (spec amendment in Task 2):
   - `types.<t>.identity.alternates: [member]`, for a path value that names a record by either of two members;
   - `operations.<op>.answer.identity_header: <header>`, for a create that names its id only in a response header.

   These are the World overrides for blind11 mechanisms 1 (a header-only create id, 132 checks) and 5 (an id-or-alias path value, 36). Their heuristics are `claude/header-ids-alt-keys` (blind11 fixes 1 and 5). The described path and the heuristic path are **one implementation**: the heuristic's detectors produce the header name and the alternate members, tier 0 emits them as `answer.identity_header` and `identity.alternates` wherever they detect something, and the runtime functions take the header name or the members as parameters (Task 5). Nothing else is added. `errors`, `masked` and child records of a fixed kind from a create's request array stay out (see "Not in this plan").

## Order, and why

The spec's order is kept: identity → kind/target/transition → lifecycle → unique/concurrency → paging/filters/scope → derived. Two changes come from the evidence:
- **Paging, filters and scope** are the largest mechanism across blind7-10 (about 175 checks). After the plumbing, Task 9 runs **in parallel** with identity (Task 5) and with unique/concurrency (Task 8, about 68), not after lifecycle.
- **Lists from state** (Task 10) needs identity (records are stored), lifecycle (the soft-delete exclusion) and the described query, so it comes late.
- **Soft-deleted records** are excluded from Task 8's unique check by Task 7, which owns `lifecycle::Exclusion`, so Task 8 does not wait for Task 7.

"PR" in the Depends column is a prerequisite engine PR from the table under "Base".

| Task | Depends on | Parallel with | Files it owns |
| --- | --- | --- | --- |
| 1 `RecordAt::Pointer`, `tier0` move, `apply` | — | — | `mb-mcm/src/lib.rs`, `mb-mcm/src/world/{tier0,apply}.rs`, `mb-mcm/src/world/validate.rs` (`RecordAt` use), `mb-import/src/world.rs`, `mb-import/src/heuristics.rs` (`RecordAt` uses), every `RecordAt` match in `mb-protocols`/`mb-engine` (`mb-engine/src/compact.rs` among them) |
| 2 Vocabulary: `alternates`, `identity_header`; field-wise overlay | 1, PR `claude/header-ids-alt-keys` | 3 | `mb-mcm/src/world/{mod,validate}.rs`, `mb-mcm/schemas/world.v1.json`, spec, `docs/world.md` vocabulary table, `mockingbird-world` skill |
| 3 `Described`, `with_world`, gap marking | 1 | 2 | `mb-protocols/src/call/described.rs`, `mb-protocols/src/lib.rs`, `call.rs::record_effects`, `mb-engine/src/lib.rs` (`GeneratedResponse` field), `mb-mcm/src/world/effective.rs` (new) |
| 4 Load at build, hot reload | 3 | 5, 8, 9 | `mb-store/src/{sqlite,postgres}.rs`, `mb-admin/src/world.rs` (`runtime_world`), `mockingbird/src/runtime.rs`, `mockingbird/src/registry.rs` (`build_mounted` retry) |
| 5 Identity and record pointers | 2, 3, PR `claude/header-ids-alt-keys` | 4, 8, 9 | `call/records.rs` (new), `mb-mcm/src/world/tier0.rs` (header and alternate emission), `call.rs`: `write_state` create arm (returns `WriteOutcome`), `complete` (the create-id call site before `write_state` and the `no_record` mark after it), `mint_create_id`, the field lists passed to `by_path_fields`; `http_rest/handler.rs` header placement |
| 6 Kind, target, transition, changes | 5, PR `claude/effect-members` | 8, 9 | `call/transitions.rs` (new), `call.rs` `write_state` update/action arms, `call/write_memory.rs` (`onto_record` replace) |
| 7 Lifecycle, soft delete, `not_found` | 6, 8, 9, PR #253 | — | `call/lifecycle.rs` (new), `call/deleted.rs`, `call.rs` `write_state` delete arm, `call/list_query.rs` (exclusion), `call/repeats.rs` (exclusion in `described_conflict`) |
| 8 Unique, concurrency, idempotency | 3 | 4, 5, 6, 9 | `call/repeats.rs`, `call/tokens.rs`, `call/replays.rs` (new), `call.rs`: `prepare` (refusal and replay call sites), `complete` (the `replays::keep` call site), `repeated_record` (the described idempotency header); `call/value_index.rs` (`same_values`, the same header); `mb-engine/src/guards.rs` (`carries_an_idempotency_key` takes the described header); `mb-mcm/src/world/tier0.rs` (idempotency emission, only if tier 0 lacks it; Task 5 edits the same file, in other functions) |
| 9 Paging, filters, scope, order | 3, PR #252 | 4, 5, 6, 8 | `call/list_query.rs` (new), `call/lists.rs`, `call/runs.rs`, `call/pages.rs`, `call/cursors.rs` |
| 10 Lists answered from state | 7 | — | `call/state_lists.rs` (new), `call.rs`: `prepare` list branch, one `#listed` mark call in `complete` (seeding) and in `write_state`'s create arm |
| 11 Derived counts and sums | 10 | — | `call/derived.rs` (new), hooks in `serve_held`, the generated-answer path of a held record, and `state_lists.rs` |
| 12 Docs, changelog, regression run | all | — | `docs/world.md`, `CHANGELOG.md`, `mockingbird-world` skill |

Tasks 5, 6 and 8 all edit `crates/mb-protocols/src/call.rs`. The functions they edit:
- Task 5: `write_state`'s create arm and signature (it returns `WriteOutcome`), `mint_create_id`, the `fields` lists built for `by_path_fields` (in `read_state`, `any_owner_holds` and the update/delete target lookups), and two call sites in `complete` around the `write_state` call;
- Task 6: `write_state`'s update and action arms;
- Task 8: `prepare` (two call sites), one call site in `complete` after the answer is settled, and the guard in `repeated_record`.

Tasks 5 and 8 both add call sites in `complete`, at different points (Task 5 around `write_state`, Task 8 after the answer is final). Task 5's `write_state` signature change touches the one `write_state` call in `complete` that Task 8 does not edit. Each task puts its logic in its own new module and adds only call sites in `call.rs`. Rebase rather than merge when two of them land close together; the expected conflicts are limited to those call-site lines.

**Coordination.**
- **`claude/header-ids-alt-keys`** (blind11 fixes 1 and 5) lands **before** Tasks 2 and 5, which build on it:
  - Fix 1 adds the header-id detector (a declared 2xx response header whose name ends in `-id`/`_id`/`ID`, or `Location` matching the item route) and the create path that mints the id, stores the request body shaped onto the read schema, and echoes the id in the header. Task 5 reuses those functions with the header name as a parameter. It does not write a second `header_id`.
  - Fix 5 adds id-like members that a path value may name. Task 5 assumes they land as extra members in the `fields` list that `by_path_fields` already scans (`call.rs`, `by_path_fields`: `PathAliases` first, then one `MAX_SCAN` listing, first match in field order). If they land as a separate lookup, Task 5's first step folds that lookup into `by_path_fields`' field list before adding described alternates. There is one lookup and one rule.
  - If `claude/header-ids-alt-keys` puts its bundle-level detectors in `mb-protocols` or `mb-engine`, Task 5 moves them into `mb-mcm` (next to `tier0`), so `tier0` can call them without a dependency cycle.
- **#252** (fixes 7, 8, 13) edits `runs.rs`, `lists.rs` and `pages.rs`, which Task 9 owns. Task 9 starts after #252 merges.
- **#253** (fixes 2, 3) changes the not-found status and body that `deleted.rs` serves. Task 7 starts after #253 merges.
- **`claude/effect-members`** (fixes 6, 12, 15) edits `write_memory.rs`. Task 6's `onto_record` replace starts after it merges.

---

### Task 1: Deep record pointers, `tier0` in `mb-mcm`, and `apply`

**Files:**
- Modify: `crates/mb-mcm/src/lib.rs` (`RecordAt`, `Identity::record_in`, `Identity::answer_holding`)
- Create: `crates/mb-mcm/src/world/tier0.rs` (moved from `crates/mb-import/src/world.rs`), `crates/mb-mcm/src/world/apply.rs`
- Modify: `crates/mb-mcm/src/world/mod.rs` (`mod tier0; mod apply;` and re-exports only)
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
#[test] fn the_root_pointer_says_the_answer_is_the_record() { /* answer.record "" → identity.record None where tier0 had Member("data") */ }
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
Expected: FAIL (`RecordAt::Pointer`, `apply` and `record_at` are not defined).

- [ ] **Step 3: Implement**
  - Add `RecordAt::Pointer`. `record_in` walks the tokens: an object takes the member; an array at token `"0"` must hold exactly one object. `answer_holding` builds the same path.
  - Move `tier0`, `identities` and `record_pointer` verbatim into `world/tier0.rs`.
  - `apply` clones the bundle and takes `base` from the caller. For each operation, each field (`kind`, `type`, `target.path_param`, `target.member`, `answer.record`) is written in only when the world's value is set and differs from `base`'s. **An absent (`None`) world value means "keep the bundle's value"; `apply` never writes a `None`.**
    - kind: `Create`/`Update`/`Replace`/`Delete`/`Action` → `Command(Create/Update/Update/Delete/Other)`; `Read` → `Query` with an identity; `List` → `Query` with `identity: None`.
    - `answer.record`: `""` → `identity.record = None` (the answer is the record); `/m`, `/0`, `/m/0` → the shallow variants; anything deeper → `Pointer`.
  - **Operations whose `identity` is `None`.** This is the usual case for a create, and for a type tier 0 gave no identity. `apply` creates an `Identity` for an operation of kind create, read, update, replace, delete or action (never list or stream) when its identity is `None` and either:
    - the world sets a type `identity.member` that differs from `base`'s (a type with no identity in tier 0 gets one), or
    - the world sets a `target` or `answer.record` for the operation that differs from `base`'s.

    The new identity is `Identity { field, path_param, input_field, path_field: None, record }`:
    - `field` = the type's effective identity member, else `"id"`;
    - `path_param` = the effective `target.path_param` (a create has none unless it names its id in its path);
    - `input_field` = the effective `target.member`;
    - `record` = the effective `answer.record`, mapped as above.

    "Effective" is the world's value when set, else `base`'s, else `None`. Every input is a diff against `base`, so a tier-0 world creates nothing.
  - A type's `identity.member` that differs from `base`'s is written into `identity.field` of every operation of that type that already has an identity. Stream operations are left alone.
  - Update each `RecordAt` use from the Files list. `record_schema_of` walks `properties`/`items` per token.

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p mb-mcm && cargo test -p mb-import --test it world_tier0 && cargo test -p mb-protocols`
Expected: PASS. The existing `mb-protocols` suite passes unchanged.

- [ ] **Step 5: Commit**

```bash
git add crates/mb-mcm crates/mb-import crates/mb-protocols/src crates/mb-engine/src
git commit -m "mcm: deep record pointers, tier0 in mb-mcm, apply a world to a bundle"
```

### Task 2: Vocabulary for id-or-alias paths and header ids

**Files:**
- Prerequisite: `claude/header-ids-alt-keys` merged. The two keys name what its detectors find, and Task 5 has tier 0 emit them.
- Modify: `crates/mb-mcm/src/world/mod.rs`:
  - `IdentityDesc.alternates`, `AnswerDesc.identity_header`;
  - `overlay` merges `identity`, `target` and `answer` **field by field** instead of replacing them whole (`overlay_type`/`overlay_op` call `set`/`set_vec` per sub-field, as they already do per entry). An author who writes `answer: {identity_header: X}` keeps tier 0's `answer.record`, and one who writes `identity: {alternates: [..]}` keeps tier 0's `member`. `alternates` is a vector, so a set one replaces tier 0's whole, like other vectors.
- Modify: `crates/mb-mcm/src/world/validate.rs`, `crates/mb-mcm/schemas/world.v1.json`, `crates/mb-mcm/tests/fixtures/world-spec-excerpt.json` (unchanged unless the roundtrip needs it)
- Modify: `docs/superpowers/specs/2026-11-04-world-description-design.md` (the vocabulary list: `identity`: member, optional pattern, optional `alternates`; `answer`: optional `identity_header`), `docs/world.md` (vocabulary table), `plugins/mockingbird/skills/mockingbird-world/SKILL.md` (one line each)
- Test: `crates/mb-mcm/src/world/validate.rs` tests, `crates/mb-mcm/tests/it/world_schema.rs`

**Interfaces:**
- Consumes: Task 1's module layout.
- Produces:
  - `IdentityDesc { member, pattern, alternates: Vec<String>, extra }`, where `alternates` is skipped when empty;
  - `AnswerDesc { record, records, long_running, identity_header: Option<String>, extra }`.

- [ ] **Step 1: Write the failing tests**

```rust
#[test] fn alternates_must_be_members_of_the_record() {
    let v = validate(&world(json!({"types": {"template": {"identity": {"member": "TemplateId", "alternates": ["Alias", "Nope"]}}}})), &templates());
    assert_eq!(v.applied.types["template"].identity.as_ref().unwrap().alternates, vec!["Alias"]);
    assert!(v.refused.iter().any(|r| r.path == "/types/template/identity/alternates/1" && r.code == RefusalCode::Unresolved));
}
#[test] fn identity_header_must_be_a_declared_2xx_response_header() { /* "Resource-ID" declared on 201: applied; "X-Other": refused unresolved */ }
#[test] fn identity_header_is_not_applicable_off_http() { /* GraphQL mutation: refused not_applicable */ }
#[test] fn schema_accepts_alternates_and_identity_header() { /* world_schema.rs: jsonschema validates the excerpt plus both keys */ }
#[test] fn overlay_keeps_base_answer_members_the_top_leaves_out() {
    // base answer {record: "/data"}; top answer {identity_header: "Resource-ID"} → {record: "/data", identity_header: "Resource-ID"}
}
#[test] fn overlay_keeps_base_identity_member_when_only_alternates_are_set() {}
#[test] fn overlay_keeps_base_target_members_the_top_leaves_out() { /* base target {path_param: id}; top {member: id} → both */ }
```

- [ ] **Step 2: Run and verify the tests fail.** `cargo test -p mb-mcm` → FAIL: unknown fields land in `extra` and are refused as `unknown_key`.
- [ ] **Step 3: Implement.** Add the fields, their JSON Schema entries and the validator checks. Reuse `check_member` for alternates. For the header, check it the way `check_type_header` does, against the operation's 2xx outputs, case-insensitively. Make `overlay` field-wise for `identity`, `target` and `answer`, and update the spec's overlay paragraph to say so. `mb-admin`'s `attribute` keeps its `/types/{t}/{field}` granularity: an entry whose sub-field changed is attributed as a whole, as today.
- [ ] **Step 4: Run and verify the tests pass.** `cargo test -p mb-mcm && cargo test -p mb-admin --test it world` → PASS.
- [ ] **Step 5: Commit** `git commit -m "world: identity alternates and answer identity_header"`

### Task 3: `Described`, `MockRuntime::with_world`, and `world_gap` marks

**Files:**
- Create: `crates/mb-protocols/src/call/described.rs`
- Create: `crates/mb-mcm/src/world/effective.rs`, with `pub fn effective(bundle: &Bundle, base: &World, doc: &World) -> (World, Vec<Refusal>)` = `validate(doc, bundle)` overlaid on `base`. `mb_admin::world::effective_world` calls it in place of its inline validate-and-overlay, so the admin view, the runtime (Task 4) and the test harness share one path.
- Modify: `crates/mb-protocols/src/call.rs` (`mod described;`, `record_effects` also writes `world_gap`), `crates/mb-protocols/src/lib.rs` (`MockRuntime` field, `with_world`, `world_revision`, `described`), `crates/mb-admin/src/world.rs` (`effective_world` calls `effective`)
- Modify: `crates/mb-engine/src/lib.rs` (`GeneratedResponse.world_gaps: Vec<String>`, default empty, and every constructor)
- Modify: each protocol's call site that records a generated response (`http_rest/handler.rs`, `graphql`, `grpc/service.rs`, `soap`, `events/ws.rs`, `http_rest/sse.rs`). Each one sets `generated.world_gaps` from `rt.described().gaps(&op.id)` before recording.
- Modify: `crates/mb-protocols/tests/it/support.rs` (`Described` harness), `crates/mb-protocols/tests/it/main.rs`
- Test: `crates/mb-protocols/tests/it/described_runtime.rs` (new)

**Interfaces:**
- Consumes: `mb_mcm::world::apply` (Task 1); `world_gaps::{gaps, GapCode}` (step 1).
- Produces:
  - `pub fn MockRuntime::with_world(self, base: &World, world: &World, revision: i64) -> Self`. `world` is already effective (tier 0 overlaid with the validated revision). It applies the world to the bundle (`apply(&bundle, base, world)`), then rebuilds `index`, `credential_names` and `described`.
  - `pub fn MockRuntime::world_revision(&self) -> i64` (`0` with no world).
  - `pub(crate) fn MockRuntime::described(&self) -> &call::described::Described`.
  - `Described` methods:
    - `Described::new(bundle: &Bundle, world: &World, revision: i64) -> Described`;
    - `Described::op(&self, op_id: &str) -> Option<&OpDesc>`;
    - `Described::type_desc(&self, t: &str) -> Option<&TypeDesc>`;
    - `Described::gaps(&self, op_id: &str) -> &[GapCode]`;
    - `Described::state_value(&self, t: &str, state: &str) -> Option<Value>`. It returns a lifecycle state in the member's declared type: `"true"` → `true` for a boolean, a number for an integer, the text otherwise. `None` when it does not fit.
  - `Described::default()` describes nothing.
  - Test support: `support::described(contract: &str, world: Value, script: Vec<Value>) -> DescribedHarness`, with `send(method, uri, body, headers) -> (StatusCode, HeaderMap, Value)`, `records() -> Vec<RequestRecord>` and `stored(t, id) -> Option<Value>`.
    - **It treats `world` exactly as the runtime does.** It parses `world` as a revision document, computes `base = tier0(&bundle)`, runs `mb_mcm::world::effective(&bundle, &base, &doc)`, and builds the runtime with `with_world(&base, &effective, 1)`. It never passes the raw document in.
    - It **panics when `effective` refuses any entry**, with the refusals in the message, so a test whose world has a typo fails loudly instead of testing tier 0. A test that means to exercise a refusal uses `support::described_allowing_refusals`.

- [ ] **Step 1: Write the failing tests** (`described_runtime.rs`)

```rust
#[tokio::test] async fn tier0_world_answers_as_no_world() {
    // Same contract and script, one runtime with_world(&tier0(&bundle), 0), one without.
    // A create, a read, a list, an update and a delete answer equal status, headers (minus Date) and bodies; stored state is equal.
}
#[tokio::test] async fn world_gap_marks_an_operation_whose_type_has_no_identity() {
    let h = support::described(NO_ID_CONTRACT, json!({}), vec![create_answer()]);
    h.send("POST", "/widgets", Some(json!({"label": "a"})), &[]).await;
    assert_eq!(h.records()[0].request["world_gap"], json!(["no_identity"]));
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
#[test] fn state_value_follows_the_member_type() { /* boolean member: "true" → true; string: "ARCHIVED"; integer member with "x" → None */ }
```

- [ ] **Step 2: Run and verify the tests fail.** `cargo test -p mb-protocols --test it described_runtime` → FAIL (`with_world` is not defined).
- [ ] **Step 3: Implement.** (`NO_ID_CONTRACT` has `POST /widgets` and `GET /widgets/{label}`, and no member tier 0 takes for an identity.) `with_world` replaces `self.bundle` with `Arc::new(apply(&self.bundle, base, world))` and recomputes the derived fields, as `new` does. `Described::new` keeps `world.operations`/`world.types` keyed as they are and precomputes `gaps` per operation (each `Gap` with `operation: Some(id)`, plus `NoIdentity` for the op's type). It lowercases header names for lookup. `record_effects` writes `world_gap` whenever `generated.world_gaps` is non-empty, even with no effects.
- [ ] **Step 4: Run and verify the tests pass.** `cargo test -p mb-protocols && cargo test -p mb-engine` → PASS.
- [ ] **Step 5: Commit** `git commit -m "runtime: compile the effective world into the runtime, mark world gaps"`

### Task 4: Load the effective world at build; hot reload on a revision

**Files:**
- Modify: `crates/mb-store/src/sqlite.rs`, `crates/mb-store/src/postgres.rs` (`add_world_revision`: after commit, `publish_mock` when the version's status is active; the status is read in the same transaction)
- Modify: `crates/mb-admin/src/world.rs` (new `runtime_world`)
- Modify: `crates/mockingbird/src/runtime.rs` (`build_runtime_tracked`), `crates/mockingbird/src/registry.rs` (`build_mounted` passes the previous runtime's world and schedules the retry)
- Test: `crates/mb-store/tests/it/conformance.rs`, `crates/mb-admin/tests/it/world.rs`, `crates/mockingbird/tests/it/world_runtime.rs` (new; register it in `tests/it/main.rs`)

**Interfaces:**
- Consumes: `MockRuntime::with_world` (Task 3), `mb_mcm::world::effective` (Task 3).
- Produces:
  - `mb_admin::world::runtime_world(store, mock_id, version, bundle, base: &World) -> Result<(World, i64), StoreError>`. It makes one `latest_world_revision` read, then `effective(bundle, base, &doc)`. It does no `attribute`, no gap list and no earlier-revision reads. With no revision it returns `(base.clone(), 0)`.
  - `MockRuntime::world_pending(&self) -> bool`: the world could not be loaded and a retry is scheduled.
  - A runtime whose `world_revision()` is the newest revision of its active contract version.

- [ ] **Step 1: Write the failing tests**

```rust
// conformance.rs, run on both backends
async fn a_world_revision_on_the_active_version_publishes_a_mock_change(store) { /* subscribe; add revision; recv ControlChange::Mock{workspace,name} */ }
async fn a_world_revision_on_an_inactive_version_publishes_nothing(store) { /* try_recv is Empty */ }

// mockingbird world_runtime.rs (server with a scripted provider, as scenarios_e2e.rs builds it)
#[tokio::test] async fn a_world_revision_applies_without_restart() {
    // Contract whose widget type has no identity. The first create is logged with world_gap ["no_identity"].
    // PUT /mocks/m/contracts/1/world {types: {widget: {identity: {member: label}}}}; wait until mock runtime world_revision == 1.
    // The next create has no world_gap.
}
#[tokio::test] async fn a_world_revision_mid_session_keeps_stored_records() {
    // Create a record in session s; put a revision; GET the record in session s → 200 from state (X-Mockingbird-Source: state).
}
#[tokio::test] async fn a_store_error_on_the_first_build_serves_tier0_and_retries() {
    // A store whose latest_world_revision fails twice, then succeeds, with revision 1 stored.
    // The first runtime: world_revision 0, world_pending true; a create is logged with tier 0's world_gap
    // (the same marks a tier-0 runtime gives). Within the backoff the runtime is rebuilt: world_revision 1, world_pending false.
}
#[tokio::test] async fn a_store_error_on_a_rebuild_keeps_the_previous_world() {
    // Revision 1 loaded. A scenario change rebuilds while latest_world_revision fails:
    // the new runtime serves revision 1's world (world_revision 1), world_pending true; the scenario change is in effect.
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
  - The fallback is the previous runtime's `(world, revision)` when `build_mounted` passed one for the same contract version, else `(base, 0)`. It sets `world_pending` and logs `tracing::warn!(mock = %mock.name, error = %e, "world description not loaded; retrying")`.
  - `build_mounted` gets the previous mounted runtime's `(contract version, world, revision)` from the registry map, when there is one.
  - When the built runtime is `world_pending`, the registry spawns one delayed rebuild of that mock (1 s, doubling to 60 s, reset on success). A rebuild for any other reason cancels the pending retry and schedules its own if it is also pending.
  - Call `with_world` before `set_data_plane_auth`, or rely on `with_world` recomputing the credential names.
- [ ] **Step 4: Run and verify the tests pass.** Run the same commands, plus `LC_ALL=en_US.UTF-8 scripts/test-postgres.sh`. Expected: PASS.
- [ ] **Step 5: Commit** `git commit -m "runtime: build with the effective world, rebuild on a new revision"`

### Task 5: Identity and record pointers

The cascades come from here: about 80 checks across blind7-10, plus 132 (header id) and 36 (id or alias) in blind11.

**Prerequisite:** `claude/header-ids-alt-keys` (blind11 fixes 1 and 5) merged, and Task 2. This task does not write a second header-id or alias lookup. It parameterises that PR's functions, so the described path and the heuristic path are the same code. Below, "the fix-1 functions" means that PR's header-id detector and its create path (mint, store the request shaped onto the read schema, echo in the header). "The fix-5 members" means the id-like members it adds to the `fields` list that `by_path_fields` scans.

**Files:**
- Create: `crates/mb-protocols/src/call/records.rs`
- Modify: `crates/mb-mcm/src/world/tier0.rs`: tier 0 emits `answer.identity_header` for each create where the fix-1 detector finds a header, and `identity.alternates` for each type where fix 5 finds id-like members. If those detectors are not in `mb-mcm`, move them there first (they read only the bundle).
- Modify: `crates/mb-protocols/src/call.rs`:
  - `write_state` returns `WriteOutcome { stored: usize }` instead of `()`. Its create arm tries `records::created(..)` first. (`WriteRequest.generated` is `&GeneratedResponse` and cannot be pushed to, so `write_state` marks nothing itself.)
  - `complete`, before its `write_state` call: `records::settle_create(rt, plan, call, &mut generated)` mints a missing id and places it in the answer (see "Data flow").
  - `complete`, after its `write_state` call: when the call is a 2xx create or update and `outcome.stored == 0`, push `"no_record"` to `generated.world_gaps`. `complete` owns `mut generated` and returns it to the protocol handler, which records and serves it after this point.
  - The `fields` lists built for `by_path_fields` (`read_state`, `any_owner_holds`, and the update/delete target lookups) come from `records::path_fields(rt, t, param)`. That function returns the type's current identity member, then `name_fields(param)` and `index.path_fields(t)` as today, then the type's alternates. `by_path_fields` itself is unchanged: `PathAliases` first, then one `MAX_SCAN` listing, first match in field order.
  - `mint_create_id`: format a minted id with `identity.pattern`.
- Modify: `crates/mb-engine/src/lib.rs`: `GeneratedResponse.created_id: Option<String>`, filled by the caller like `effect_outcomes`. If fix 1 already carries the minted id to the handler some other way, use that carrier instead and skip this field.
- Modify: `crates/mb-protocols/src/http_rest/handler.rs`: after `mb_engine::headers::declared_headers` fills the response headers, the fix-1 header placement runs with the header name from `records::identity_header(rt, op)` and the id from `generated.created_id`. `Location` becomes the item URL. The engine stays unaware of it.
- Test: `crates/mb-protocols/tests/it/described_identity.rs` (new), `crates/mb-import/tests/it/world_tier0.rs`

**Data flow of a create** (all in `complete`, in this order):
1. The answer is settled (generated, repaired, validated), as today.
2. `records::settle_create(rt, plan, call, &mut generated)`. It runs when the op is a 2xx create and the record at the answer pointer has no identity value. When the type is described with an `identity_header`, or the fix-1 detector found one, it mints the id with `mint_create_id`, patterned when the type has a pattern. It writes the id into `generated.body` at the record pointer (when the schema declares the member there) and into `generated.created_id`. It then re-validates the body (`Engine::serve_stored`). When validation fails, it restores the body, leaves `created_id` as `None`, and the create stores nothing (marked `no_record` at step 4).
3. `write_state(..)` stores what `records::created` finds in the now-final `generated`, and returns `WriteOutcome`.
4. `"no_record"` is pushed when `outcome.stored == 0`.
5. `complete` returns `generated`. The handler places the identity header from `created_id`, then records the request with `world_gaps` and serves it.

**Interfaces:**
- Consumes: `Described::{op, type_desc}` (Task 3); `IdentityDesc.alternates`, `AnswerDesc.identity_header` (Task 2); `RecordAt::Pointer` (Task 1); the fix-1 functions and fix-5 members.
- Produces:
  - `records::created(rt: &MockRuntime, op: &Operation, request: Option<&Value>, answer: Option<&Value>) -> Vec<Value>`, the records a create makes:
    - `request.records`/`answer.records` items, one each;
    - else the `long_running.result` member of the answer;
    - else the one record at `answer.record`;
    - else, for a create with an identity header, the fix-1 record (the request shaped onto the read schema, plus the id).
  - `records::identity_header(rt, op) -> Option<String>`: the effective world's `answer.identity_header`. With no world attached (`Described::default()`), it is the fix-1 detector's result. Tier 0 emits that same value, so the two are equal for a tier-0 world.
  - `records::alternates(rt, t) -> Vec<String>`: the effective `identity.alternates`, else the fix-5 members. It has the same tier-0 equality.
  - `records::path_fields(rt, t, param) -> Vec<&str>` (see Files).
  - `records::settle_create(rt, plan, call, generated: &mut GeneratedResponse)`.
  - `records::patterned(pattern: &str, id: &str) -> String` and `records::bare_id(pattern: &str, value: &str) -> Option<String>`. A read by a path value tries both forms, so a record keyed under either one is found.
- One rule for alternates: they are more members in `by_path_fields`' list, after the identity member and the path fields, so they follow its rule (first match in field order, owner check, `PathAliases` memo). There is no separate "exactly one" rule. If fix 5 needs uniqueness for guessed members, it adds that to `by_path_fields` for every field, and described alternates get it too.

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
#[tokio::test] async fn a_long_running_create_stores_its_result() {
    // answer {"name":"operations/1","done":true,"response":{"name":"folders/7","displayName":"a"}}, long_running {done: done, result: response}
    // GET /folders/7 → state
}
#[tokio::test] async fn a_patterned_identity_reads_by_bare_or_full_id() { /* pattern "folders/{id}" */ }
#[tokio::test] async fn an_alternate_member_reaches_the_same_record() {
    // identity TemplateId, alternates [Alias] on a type fix 5 does not detect; create {Alias: "welcome"};
    // GET /templates/welcome and GET /templates/{TemplateId} both state, same body;
    // DELETE /templates/welcome → GET /templates/{TemplateId} 404
}
#[tokio::test] async fn a_detected_alternate_and_the_same_alternate_described_answer_alike() { /* as the header twin, for Alias */ }
#[tokio::test] async fn the_identity_member_is_tried_before_an_alternate() {
    // record A has TemplateId "x"; record B has Alias "x": GET /templates/x → A (by_path_fields' field order)
}
#[tokio::test] async fn a_create_that_stores_nothing_is_marked_no_record() { /* answer with no id anywhere and no description: request.world_gap contains "no_record" */ }
#[tokio::test] async fn a_minted_id_the_schema_refuses_stores_nothing() { /* pattern makes the id exceed maxLength: generated body served, no record, world_gap no_record */ }
#[tokio::test] async fn a_revision_that_changes_the_identity_member_still_reaches_earlier_records() {
    // create under identity "id"; put a revision with identity.member "slug"; GET /things/{slug} → state (scan by the new member);
    // DELETE /things/{slug} → the earlier record is gone; a new create is stored under its slug
}
#[tokio::test] async fn a_revision_that_adds_a_pattern_still_reaches_bare_keyed_records() {
    // create stored under "7"; revision adds pattern "folders/{id}"; GET /folders/7 and GET by "folders/7" both → state
}

// mb-import/tests/it/world_tier0.rs
#[test] fn tier0_emits_identity_header_and_alternates_where_the_heuristics_detect_them() {
    // Resource-ID create → operations.createConversation.answer.identity_header == "Resource-ID";
    // the alias fixture → types.template.identity.alternates == ["Alias"]; every repo contract: apply_of_tier0_is_identity still holds
}
```

`apply_of_tier0_is_identity` (Task 1) and `tier0_world_answers_as_no_world` (Task 3) must still pass. Both hold only because the runtime reads `identity_header` and `alternates` through the same functions with or without a world, and tier 0 restates what the detectors find.

- [ ] **Step 2: Run and verify the tests fail.** `cargo test -p mb-protocols --test it described_identity && cargo test -p mb-import --test it world_tier0` → FAIL. Only the first test may already pass, through Task 1's `apply`. If it does, keep it as the regression guard.
- [ ] **Step 3: Implement** per the Interfaces and the data flow above.
  - A record a create makes with no identity value gets its id from `settle_create`, not from `write_state`.
  - `long_running.done` is set to `true` in the answer, in `settle_create`, when the member is a boolean.
- [ ] **Step 4: Run and verify the tests pass.** `cargo test -p mb-protocols && cargo test -p mb-import` → PASS, including the fix-1 and fix-5 tests from `claude/header-ids-alt-keys`, unchanged.
- [ ] **Step 5: Commit** `git commit -m "runtime: identity, record pointers, bulk and long-running creates from the world"`

### Task 6: Operation kind, target, transition and changes

**Files:**
- Create: `crates/mb-protocols/src/call/transitions.rs`
- Prerequisite: `claude/effect-members` (blind11 fixes 6, 12, 15) merged; it edits `write_memory.rs` too.
- Modify: `crates/mb-protocols/src/call.rs`. In the `write_state` update and action arms, after a 2xx, call `transitions::apply(..)`.
- Modify: `crates/mb-protocols/src/call/write_memory.rs`. For a `Replace`, `onto_record` replaces instead of merging.
- Test: `crates/mb-protocols/tests/it/described_kinds.rs` (new)

**Interfaces:**
- Consumes: `Described::op` (Task 3); `apply` kind/target (Task 1).
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
- Create: `crates/mb-protocols/src/call/lifecycle.rs`
- Modify:
  - `crates/mb-protocols/src/call.rs` (`write_state` delete arm: `lifecycle::soft_delete(..)` instead of `state.delete` when described; create arm: `lifecycle::initial(..)`);
  - `crates/mb-protocols/src/call/deleted.rs` (a soft-deleted record is no deleted marker; `not_found` status);
  - `crates/mb-protocols/src/call/list_query.rs` (add `ListQuery.lifecycle: Option<lifecycle::Exclusion>`, filled by `of`; `matches` excludes soft-deleted records unless `listed_with` is sent);
  - `crates/mb-protocols/src/call/repeats.rs` (`described_conflict` from Task 8 skips records that `lifecycle::Exclusion` marks soft-deleted, so a soft-deleted record's unique values are free again).
- Prerequisite: #253 (`claude/engine-error-bodies`) merged; `not_found` builds on its not-found status and body.
- Test: `crates/mb-protocols/tests/it/described_lifecycle.rs` (new)

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

**Files:**
- Create: `crates/mb-protocols/src/call/replays.rs`
- Modify:
  - `crates/mb-protocols/src/call/repeats.rs` (`described_conflict(..)`, checked first);
  - `crates/mb-protocols/src/call/tokens.rs` (`check` and `after_update` take a described pairing, confirmed from the first write);
  - `crates/mb-protocols/src/call.rs`:
    - `prepare`: call `replays::replayed`, then `repeats::described_conflict`, before generation;
    - `complete`: one call site, `replays::keep`, after the answer is final;
    - `repeated_record`: its idempotency guard also counts the described header;
  - `crates/mb-protocols/src/call/value_index.rs` (`same_values`: the same guard);
  - `crates/mb-engine/src/guards.rs` (`carries_an_idempotency_key(bundle, op, call, described: Option<&str>)`: true also when the call carries the described idempotency header).
- Test: `crates/mb-protocols/tests/it/described_conflicts.rs` (new)

**Interaction with today's idempotency-key paths.** Today a create that carries a header the contract names as an idempotency key (`carries_an_idempotency_key`) skips `repeated_record` and `same_values`: a repeat may be a retry, so it is no conflict, and it is generated as a new create. With a described `idempotency` header:
1. `replays::replayed` runs first. A key already kept for this op in this session answers the kept response. Nothing else runs: no generation, no unique check, no store write.
2. A key not kept yet goes on as today. `carries_an_idempotency_key` is true, also for a described header the name heuristic misses, so `repeated_record` and `same_values` still skip. `described_conflict` still runs: a new key with a taken unique set is a new request and is refused.
3. `replays::keep` keeps the final answer (2xx or a refusal), so a retry of a refused create is refused the same way.
4. Without a described `idempotency`, nothing changes. If tier 0 does not emit `idempotency` from the same header rule, Task 8 adds that emission (in `tier0.rs`, from `carries_an_idempotency_key`'s header test), so a tier-0 world still answers as no world.

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
#[tokio::test] async fn an_idempotent_replay_answers_as_the_first() { /* Idempotency-Key twice → same status/body, one record, provider called once */ }
#[tokio::test] async fn a_new_key_with_a_taken_unique_set_is_refused() {}
#[tokio::test] async fn a_described_key_header_the_name_rule_misses_skips_repeat_detection() { /* header "X-Request-Token" described as idempotency */ }
#[tokio::test] async fn a_replayed_refusal_is_refused_again() {}
#[tokio::test] async fn an_undescribed_idempotency_key_behaves_as_today() { /* create_repeats suite's key cases, unchanged */ }
```

- [ ] **Step 2: Run and verify the tests fail.** `cargo test -p mb-protocols --test it described_conflicts` → FAIL.
- [ ] **Step 3: Implement** per the Interfaces. Refusal bodies come from `Engine::refuse_from_state`. When the operation declares no response for the code, nothing is refused and the call goes on as today.
- [ ] **Step 4: Run and verify the tests pass.** `cargo test -p mb-protocols` → PASS. The `concurrency*`, `create_repeats` and `create_refusals*` suites are unchanged.
- [ ] **Step 5: Commit** `git commit -m "runtime: described unique sets, concurrency tokens and idempotent replays"`

### Task 9: Paging, filters, scope and order

The biggest mechanism across blind7-10: about 175 checks.

**Files:**
- Prerequisite: #252 (`claude/param-styles-and-lists`, blind11 fixes 7, 8, 13) merged; it edits `runs.rs`, `lists.rs` and `pages.rs`.
- Create: `crates/mb-protocols/src/call/list_query.rs`
- Modify:
  - `crates/mb-protocols/src/call/lists.rs`: `narrow` uses `list_query::of` when it returns `Some`, else today's name matching;
  - `crates/mb-protocols/src/call/runs.rs`: `position` takes a described `page`/`offset` parameter;
  - `crates/mb-protocols/src/call/pages.rs`: `slot`/`header_slot` take a described `response`/`response_header`;
  - `crates/mb-protocols/src/call/cursors.rs`: a described cursor parameter is shown.
- Test: `crates/mb-protocols/tests/it/described_lists.rs` (new)

**Interfaces:**
- Consumes: `Described::op` (Task 3).
- Produces:
  - `list_query::ListQuery { filters: Vec<Filter>, scope: Option<(String, String)>, order: Option<(String, bool)>, page: Option<PageAsk> }`. Task 7 adds a `lifecycle` field.
  - `list_query::Filter { member: String, kind: FilterKind, values: Vec<Value> }`.
  - `list_query::PageAsk { style: PagingStyle, position: Option<String>, size: Option<usize>, response: Option<String>, response_header: Option<String> }`.
  - `list_query::of(rt: &MockRuntime, op: &Operation, call: &OperationCall) -> Option<ListQuery>`.
  - `list_query::matches(q: &ListQuery, item: &Value) -> bool`.
  - `list_query::sort(q: &ListQuery, items: &mut [Value])`.
  - `list_query::page<'a>(q: &ListQuery, items: &'a [Value]) -> (&'a [Value], Option<String> /*next position*/)`.
- Filter semantics, fixed here:
  - `eq`: query text against the member by value (`"3"` = `3`, `"true"` = `true`); an array parameter is any-of.
  - `contains`: on an array member, any item equal to the value, or an object item with exactly one string member equal to it; on a string member, a case-sensitive substring.
  - `gte`/`lte`: numbers by value; RFC 3339 date-times when both sides parse as one; otherwise no narrowing.
  - `flag`: the member equals the boolean.
  - `text`: a case-insensitive substring over the item's string members, or over `member` when given.
  - A value that does not parse as the member's type narrows nothing.
- Paging semantics: the position is an offset into the matched, ordered items.
  - `cursor`/`next_url`/`header_cursor` mint opaque tokens with the existing `pages` token format.
  - `page` is 1-based unless the parameter's `minimum` is 0.
  - The size is the request's size parameter, else its declared `default`, else no cut.

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
```

- [ ] **Step 2: Run and verify the tests fail.** `cargo test -p mb-protocols --test it described_lists` → FAIL.
- [ ] **Step 3: Implement** `list_query.rs` per the semantics above, then switch the three modules to consult it first.
- [ ] **Step 4: Run and verify the tests pass.** `cargo test -p mb-protocols` → PASS. The existing `world_lists`, `runs` and `pages` tests are unchanged.
- [ ] **Step 5: Commit** `git commit -m "runtime: described paging, filters, scope and order"`

### Task 10: Lists answered from state

**Files:**
- Create: `crates/mb-protocols/src/call/state_lists.rs`
- Modify: `crates/mb-protocols/src/call.rs` (`prepare`: a described list tries `state_lists::answer` before generation; `complete`'s seeding and `write_state`'s create arm call `state_lists::mark_listed`)
- Test: `crates/mb-protocols/tests/it/described_state_lists.rs` (new)

**Interfaces:**
- Consumes: `list_query::{of, matches, sort, page}` (Tasks 9 and 7); `rt.collections()`; `Engine::held_answer`/`serve_stored`.
- Produces: `state_lists::answer(state: &mut StateAccess<'_>, plan: &CallPlan<'_>, call: &OperationCall) -> Option<GeneratedResponse>`, tagged `Source::State`.
  - It gathers the held records of the item type, at most `MAX_SCAN`, then matches, sorts and pages them.
  - It places the page at `answer.records`, else at the list member `lists::list_parts` finds.
  - It writes the next position at `paging.response` or `response_header`.
  - It fills the envelope's other required members as `held_answer` does.
  - It returns `None`, so the list is generated, when the collection was neither listed nor written this session, or the answer does not validate.
  - **"Listed or written" is kept in the state store, not in memory.** `Collections` is reset by every rebuild, including the one each `put_world`/`patch_world` triggers. A session-scoped mark under `mb_state::INTERNAL_TYPE`, `#listed/<item type>/<collection key>`, is written when a list of that collection is seeded and when a create lands in it. `answer` reads the mark. After a rebuild, the next described list is answered from state; it is not generated and seeded again (`WriteOrigin::Seeded`), so model records are never mixed into a collection that holds real ones. The mark costs one store read per described list. It is written once per collection and session.
  - Records stored before a revision that changed the type's identity member are gathered like any other held record of the type; their stored key does not matter to a list.

- [ ] **Step 1: Write the failing tests**

```rust
#[tokio::test] async fn a_second_list_is_answered_from_state_with_the_new_record() { /* list (generated, seeded), create, list → state, includes it, provider called twice not three times */ }
#[tokio::test] async fn a_described_filter_answers_from_state_exactly() {}
#[tokio::test] async fn pages_from_state_never_repeat_an_item() {}
#[tokio::test] async fn a_list_page_whose_envelope_cannot_be_filled_is_generated() { /* required free-number member → source llm */ }
#[tokio::test] async fn an_undescribed_list_is_generated_as_today() {}
#[tokio::test] async fn a_world_revision_mid_session_keeps_lists_from_state() {
    // list (generated, seeded), create; put a revision (rebuild); list → source state, exactly the seeded items plus the created one,
    // no new Seeded records, provider called twice not three times
}
#[tokio::test] async fn lists_after_an_identity_change_still_hold_earlier_records() {}
```

- [ ] **Step 2: Run and verify the tests fail.** `cargo test -p mb-protocols --test it described_state_lists` → FAIL.
- [ ] **Step 3: Implement** per the Interfaces.
- [ ] **Step 4: Run and verify the tests pass.** `cargo test -p mb-protocols` → PASS.
- [ ] **Step 5: Commit** `git commit -m "runtime: described lists answered from state"`

### Task 11: Derived counts and sums

**Files:**
- Create: `crates/mb-protocols/src/call/derived.rs`
- Modify: `crates/mb-protocols/src/call.rs` (`serve_held` and the generated-answer path of a held record call `derived::fill`), `crates/mb-protocols/src/call/state_lists.rs` (each item)
- Test: `crates/mb-protocols/tests/it/described_derived.rs` (new)

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
- **A description that clears a pointer to nothing** ("this answer holds no record"). In v1 an author corrects a wrong `answer.record`, identity member or target by replacing it; `""` makes the answer itself the record. An explicit "none" needs a spec amendment, because a JSON `null` reads as absent.
- **Vocabulary the diagnosis asks for beyond Task 2:**
  - `errors` (`{status, cause} → member values`), for blind11 mechanism 2, engine-made error bodies;
  - `masked` write-only members, for blind11 mechanism 9, a withheld sensitive value the request echo puts back;
  - child records of a fixed kind made from a create's request array, for blind11 mechanism 14 (e.g. order lines, or a note becoming a `type: note` thread).

  Each needs a spec amendment first.
- **Step 4** (`--world` bench tiers), **step 5** (convention detectors) and **step 6** (`mockingbird world draft`). Detectors from step 5 produce tier-0 entries. This plan's runtime reads them with no further change, because `apply` and `Described` read the effective world whatever its author.
