# World Descriptions, Step 3: Runtime Reads the Description (Implementation Plan)

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** The runtime modules that guess today (identity, envelopes, kinds, lifecycle, uniqueness, concurrency, paging, filters, scope) answer from the effective world description first. Each one keeps its heuristic as the fallback wherever the description is silent. Derived counts and sums are computed exactly from stored records. An operation the description leaves open is marked `world_gap` in the request log.

**Architecture:** The effective world is compiled into the runtime once, when the runtime is built, so a request only does hash lookups.
- **Into the bundle.** The entries the bundle already models (kind, type, target, the answer's record pointer, the type's identity member) are written into a copy of the `Bundle` by `mb_mcm::world::apply`. Every heuristic that reads `op.kind` or `op.identity` then follows the description with no change of its own.
- **Into `Described`.** The entries the bundle has no place for (lifecycle, unique, concurrency, paging, filters, scope, order, transition, changes, derived, bulk and long-running records, idempotency, `not_found`) go into a `Described` index on `MockRuntime`. The modules consult it before their heuristic.
- **Hot reload.** A new world revision publishes the same mock change that a scenario change publishes, and the registry rebuilds the runtime with the newest effective world.

**Tech Stack:** Rust workspace (`mb-mcm`, `mb-import`, `mb-engine`, `mb-protocols`, `mb-store`, `mb-admin`, `mockingbird`); serde_json; sqlx (SQLite and Postgres); the integration-test harness in `crates/mb-protocols/tests/it` with `mb_llm::ScriptedProvider`.

**Spec:** `docs/superpowers/specs/2026-11-04-world-description-design.md` (step 3 of "Delivery order"). Evidence for the order and scope: the blind11 diagnosis, sections "blind7-blind10, default model only: the top 5 mechanisms across sets" and "Engine now vs World". The diagnosis lives in the orchestrator's side-session notes; the numbers this plan uses are quoted where they apply.

**Base:** `origin/main` with PR #249 (`claude/world-description`, steps 1–2) merged. Execution starts only after #249 is on `main`.

## Global Constraints

- **Optional, never required.** A contract alone still works. With no revision, the effective world is tier 0, and every answer stays as it is today (Task 1 and Task 3 prove this).
- **Validated, never trusted.** The runtime reads only the effective world: tier 0 overlaid with the newest revision's entries that validate (`mb_admin::world::effective_world`). It never reads a raw revision.
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
3. **A revision written mid-session.** Expected: the runtime is rebuilt and the session's stored records survive, because they live in the state store. In-memory indexes (path aliases, collections) restart, as they do on a scenario change today. The next request follows the new revision. Test: Task 4 `a_world_revision_mid_session_keeps_stored_records`.
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
   - `crates/mockingbird/src/runtime.rs::build_runtime_tracked` calls `mb_admin::world::effective_world(store, mock.id, contract.version, &contract.bundle)` once, then `MockRuntime::with_world(&eff.world, eff.revision)`.
   - A store error is logged with `tracing::warn!` and the runtime is served with no world: the description is optional.
   - A request pays only `HashMap` lookups by operation id and type name.
2. **Hot reload by rebuild, as `put_scenario` does.**
   - `add_world_revision` (SQLite and Postgres) publishes `ControlChange::Mock` for the mock when the revision's contract version is the active one. The registry's watcher already rebuilds on that event, and HA bridges carry it to other nodes.
   - This covers every writer: `put_world`, `patch_world`, restore, carry-forward at activation, config-file `world:` and snapshot import.
   - No `ArcSwap` is added. A rebuilt runtime recomputes `BundleIndex` from the applied bundle.
3. **Two places the description lands.**
   - `mb_mcm::world::apply(bundle, world) -> Bundle` rewrites each operation where the world differs from `tier0(bundle)`. It compares field by field, so tier 0 is a no-op by construction. The fields are kind, resource type, `identity.path_param`/`input_field`, `identity.field` (from the type's `identity.member`) and `identity.record` (from `answer.record`, a deep pointer allowed).
   - Everything else goes into `call::described::Described`. Applying the world to the bundle makes kind, target and pointer changes reach all of today's heuristics without editing them.
4. **`tier0` moves to `mb-mcm`** (`mb_mcm::world::tier0`), so `apply` can diff against it without a dependency from `mb-protocols` on `mb-import`. `mb_import::world::tier0` stays as a re-export.
5. **Deep record pointers.** `mb_mcm::RecordAt` gains `Pointer(Vec<String>)`, the RFC 6901 tokens.
   - In the token list, `"0"` means "the one item of an array" when the value there is an array, and a member named `0` otherwise.
   - `apply` writes the three existing variants where they fit (`/m` → `Member`, `/0` → `Item`, `/m/0` → `MemberItem`). So stored bundles and tier 0 never contain the new variant.
6. **How `world_gap` is marked.**
   - `mb_engine::GeneratedResponse` gains `world_gaps: Vec<String>`. Like `effect_outcomes`, it is filled by the caller in `mb-protocols`.
   - The static gaps of an operation come from `world_gaps::gaps(bundle, world, &[])`, computed once in `Described`: `no_identity` for its type, `no_target`, `no_record`.
   - The dynamic gap `no_record` is added when a 2xx create or update stored nothing.
   - `call::record_effects` writes them into the request record's `request` JSON as `"world_gap": ["no_identity", …]`, next to `effects`. A `tracing::info!` line carries `world_gap = …`.
   - No store migration and no Admin API schema change: the request log already stores and serves `request` as JSON.
7. **Derived values are computed on serve, never stored.** A derived member is computed when a record of its type is answered and the computation is exact:
   - the record was created in this session, or at least one child under it is held;
   - the answer is from state, a list from state, or a generated answer about a held record.

   Sums use exact decimal arithmetic, a small `Decimal { mantissa: i128, scale: u32 }` in `call/derived.rs` with no new dependency. The result is written in the member's declared type.
8. **Lists from state.**
   - A list read is answered from state, before any LLM call, when all of these hold:
     - the description gives its operation `paging`, `filters`, `scope` or `order`;
     - the session has listed or written that collection before;
     - the envelope can be filled.
   - The first list of a collection in a session is still generated and seeded as today, then narrowed by the described query.
   - This targets the largest mechanism across blind7-10, "List answers come from the model, not from state": about 175 checks on the default model.
9. **Vocabulary added here** (spec amendment in Task 2):
   - `types.<t>.identity.alternates: [member]`, for a path value that names a record by either of two members;
   - `operations.<op>.answer.identity_header: <header>`, for a create that names its id only in a response header.

   These are the World overrides the diagnosis names for its mechanisms 1 (132 checks) and 5 (36). Nothing else is added. `errors`, `masked` and child records of a fixed kind from a create's request array stay out (see "Not in this plan").

## Order, and why

The spec's order is kept: identity → kind/target/transition → lifecycle → unique/concurrency → paging/filters/scope → derived. Two changes come from the evidence:
- **Paging, filters and scope** are the largest mechanism across blind7-10 (about 175 checks). After the plumbing, Task 9 runs **in parallel** with identity (Task 5) and with unique/concurrency (Task 8, about 68), not after lifecycle.
- **Lists from state** (Task 10) needs identity (records are stored), lifecycle (the soft-delete exclusion) and the described query, so it comes late.

| Task | Depends on | Parallel with | Files it owns |
| --- | --- | --- | --- |
| 1 `RecordAt::Pointer`, `tier0` move, `apply` | — | — | `mb-mcm/src/lib.rs`, `mb-mcm/src/world/{tier0,apply}.rs`, `mb-import/src/world.rs`, `RecordAt` match arms in `mb-protocols`/`mb-engine` |
| 2 Vocabulary: `alternates`, `identity_header` | 1 | 3 | `mb-mcm/src/world/{mod,validate}.rs`, `mb-mcm/schemas/world.v1.json`, spec, `docs/world.md` vocabulary table, `mockingbird-world` skill |
| 3 `Described`, `with_world`, gap marking | 1 | 2 | `mb-protocols/src/call/described.rs`, `mb-protocols/src/lib.rs`, `call.rs::record_effects`, `mb-engine/src/lib.rs` (`GeneratedResponse` field) |
| 4 Load at build, hot reload | 3 | 5, 8, 9 | `mb-store/src/{sqlite,postgres}.rs`, `mockingbird/src/runtime.rs` |
| 5 Identity and record pointers | 2, 3 | 4, 8, 9 | `call/records.rs` (new), `call.rs` `write_state` create arm and `by_path_fields`, `http_rest/handler.rs` header placement |
| 6 Kind, target, transition, changes | 5 | 8, 9 | `call/transitions.rs` (new), `call.rs` `write_state` update/action arms |
| 7 Lifecycle, soft delete, `not_found` | 6, 9 | — | `call/lifecycle.rs` (new), `call/deleted.rs`, `call.rs` delete arm, `call/list_query.rs` (exclusion) |
| 8 Unique, concurrency, idempotency | 3 | 4, 5, 6, 9 | `call/repeats.rs`, `call/tokens.rs`, `call/replays.rs` (new), `call.rs::prepare` only |
| 9 Paging, filters, scope, order | 3 | 4, 5, 6, 8 | `call/list_query.rs` (new), `call/lists.rs`, `call/runs.rs`, `call/pages.rs` |
| 10 Lists answered from state | 7 | — | `call/state_lists.rs` (new), `call.rs::prepare` list branch |
| 11 Derived counts and sums | 10 | — | `call/derived.rs` (new), hooks in `serve_held` and `state_lists.rs` |
| 12 Docs, changelog, regression run | all | — | `docs/world.md`, `CHANGELOG.md`, `mockingbird-world` skill |

Tasks 5, 6 and 8 all edit `crates/mb-protocols/src/call.rs`, in disjoint functions:
- Task 5 edits `write_state`'s create arm and `by_path_fields`;
- Task 6 edits `write_state`'s update and action arms;
- Task 8 edits only `prepare`.

Each puts its logic in its own new module and adds only a call site in `call.rs`. Rebase rather than merge when two of them land close together.

**Coordination.** The blind11 "engine now" fixes go in separate PRs: 1 (header-only create id), 2, 3, 5–9, 11–13 and 15.
- Fix 1 edits the same `write_state` create arm as Task 5. Whichever lands second rebases. Task 5's `identity_header` stays the override on top of that fix's detector.
- Fixes 7 and 13 edit `runs.rs` and `lists.rs`, the files Task 9 owns. Task 9 rebases onto them.

---

### Task 1: Deep record pointers, `tier0` in `mb-mcm`, and `apply`

**Files:**
- Modify: `crates/mb-mcm/src/lib.rs` (`RecordAt`, `Identity::record_in`, `Identity::answer_holding`)
- Create: `crates/mb-mcm/src/world/tier0.rs` (moved from `crates/mb-import/src/world.rs`), `crates/mb-mcm/src/world/apply.rs`
- Modify: `crates/mb-mcm/src/world/mod.rs` (`mod tier0; mod apply;` and re-exports only)
- Modify: `crates/mb-import/src/world.rs` (becomes `pub use mb_mcm::world::tier0;`)
- Modify: every `match` on `RecordAt` in `crates/mb-protocols/src` and `crates/mb-engine/src`, about 19 arms (`record_schema_of` in `call.rs` among them)
- Test: `crates/mb-mcm/src/world/apply.rs` (unit), `crates/mb-import/tests/it/world_tier0.rs` (repo contracts)

**Interfaces:**
- Produces:
  - `mb_mcm::RecordAt::Pointer(Vec<String>)`, serialized as `{"pointer": [..]}`.
  - `mb_mcm::world::record_at(p: &Pointer) -> RecordAt`.
  - `mb_mcm::world::record_pointer(at: &RecordAt) -> Option<Pointer>` (moved from `mb-import` and made public).
  - `mb_mcm::world::tier0(bundle: &Bundle) -> World`.
  - `mb_mcm::world::apply(bundle: &Bundle, world: &World) -> Bundle`.

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
    let op = apply(&b, &w).operation("safeDelete").cloned().unwrap();
    assert_eq!(op.kind, OperationKind::Command(Effect::Delete));
    assert_eq!(op.identity.unwrap().path_param.as_deref(), Some("id"));
}
#[test] fn apply_sets_identity_field_from_the_type_only_where_it_differs_from_tier0() { /* type identity `name`, ops keep their own path_field */ }
#[test] fn apply_replace_is_an_update() { /* kind replace → Command(Update) */ }

// mb-import/tests/it/world_tier0.rs
#[test] fn apply_of_tier0_is_identity() {
    for (path, b) in bundles() {
        assert_eq!(mb_mcm::world::apply(&b, &tier0(&b)), b, "{}", path.display());
    }
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p mb-mcm world::apply && cargo test -p mb-import --test it world_tier0::apply_of_tier0_is_identity`
Expected: FAIL (`RecordAt::Pointer`, `apply` and `record_at` are not defined).

- [ ] **Step 3: Implement**
  - Add `RecordAt::Pointer`. `record_in` walks the tokens: an object takes the member; an array at token `"0"` must hold exactly one object. `answer_holding` builds the same path.
  - Move `tier0`, `identities` and `record_pointer` verbatim into `world/tier0.rs`.
  - `apply` clones the bundle and computes `base = tier0(bundle)`. For each operation, each field (`kind`, `type`, `target`, `answer.record`) whose world value differs from `base`'s is written in:
    - kind: `Create`/`Update`/`Replace`/`Delete`/`Action` → `Command(Create/Update/Update/Delete/Other)`; `Read` → `Query` with an identity; `List` → `Query` with `identity: None`;
    - a `Read` with no identity gets `field` = the type's identity member, else `"id"`.

    A type's `identity.member` that differs from `base`'s is written into `identity.field` of every operation of that type. Stream operations are left alone.
  - Add the new arm to each `RecordAt` match in `mb-protocols` and `mb-engine`. `record_schema_of` walks `properties`/`items` per token.

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
- Modify: `crates/mb-mcm/src/world/mod.rs` (`IdentityDesc.alternates`, `AnswerDesc.identity_header`; `overlay` replaces `identity` and `answer` whole, as it does now)
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
```

- [ ] **Step 2: Run and verify the tests fail.** `cargo test -p mb-mcm` → FAIL: unknown fields land in `extra` and are refused as `unknown_key`.
- [ ] **Step 3: Implement.** Add the fields, their JSON Schema entries and the validator checks. Reuse `check_member` for alternates. For the header, check it the way `check_type_header` does, against the operation's 2xx outputs, case-insensitively.
- [ ] **Step 4: Run and verify the tests pass.** `cargo test -p mb-mcm && cargo test -p mb-admin --test it world` → PASS.
- [ ] **Step 5: Commit** `git commit -m "world: identity alternates and answer identity_header"`

### Task 3: `Described`, `MockRuntime::with_world`, and `world_gap` marks

**Files:**
- Create: `crates/mb-protocols/src/call/described.rs`
- Modify: `crates/mb-protocols/src/call.rs` (`mod described;`, `record_effects` also writes `world_gap`), `crates/mb-protocols/src/lib.rs` (`MockRuntime` field, `with_world`, `world_revision`, `described`)
- Modify: `crates/mb-engine/src/lib.rs` (`GeneratedResponse.world_gaps: Vec<String>`, default empty, and every constructor)
- Modify: each protocol's call site that records a generated response (`http_rest/handler.rs`, `graphql`, `grpc/service.rs`, `soap`, `events/ws.rs`, `http_rest/sse.rs`). Each one sets `generated.world_gaps` from `rt.described().gaps(&op.id)` before recording.
- Modify: `crates/mb-protocols/tests/it/support.rs` (`Described` harness), `crates/mb-protocols/tests/it/main.rs`
- Test: `crates/mb-protocols/tests/it/described_runtime.rs` (new)

**Interfaces:**
- Consumes: `mb_mcm::world::apply` (Task 1); `world_gaps::{gaps, GapCode}` (step 1).
- Produces:
  - `pub fn MockRuntime::with_world(self, world: &World, revision: i64) -> Self`. It applies the world to the bundle, then rebuilds `index`, `credential_names` and `described`.
  - `pub fn MockRuntime::world_revision(&self) -> i64` (`0` with no world).
  - `pub(crate) fn MockRuntime::described(&self) -> &call::described::Described`.
  - `Described` methods:
    - `Described::new(bundle: &Bundle, world: &World, revision: i64) -> Described`;
    - `Described::op(&self, op_id: &str) -> Option<&OpDesc>`;
    - `Described::type_desc(&self, t: &str) -> Option<&TypeDesc>`;
    - `Described::gaps(&self, op_id: &str) -> &[GapCode]`;
    - `Described::state_value(&self, t: &str, state: &str) -> Option<Value>`. It returns a lifecycle state in the member's declared type: `"true"` → `true` for a boolean, a number for an integer, the text otherwise. `None` when it does not fit.
  - `Described::default()` describes nothing.
  - Test support: `support::described(contract: &str, world: Value, script: Vec<Value>) -> DescribedHarness`, with `send(method, uri, body, headers) -> (StatusCode, HeaderMap, Value)` and `records() -> Vec<RequestRecord>`.

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
    h.send("POST", "/widgets", Some(json!({"label": "a"})), &[]).await;
    assert!(h.records()[0].request.get("world_gap").is_none());
}
#[test] fn state_value_follows_the_member_type() { /* boolean member: "true" → true; string: "ARCHIVED"; integer member with "x" → None */ }
```

- [ ] **Step 2: Run and verify the tests fail.** `cargo test -p mb-protocols --test it described_runtime` → FAIL (`with_world` is not defined).
- [ ] **Step 3: Implement.** `with_world` replaces `self.bundle` with `Arc::new(apply(&self.bundle, world))` and recomputes the derived fields, as `new` does. `Described::new` keeps `world.operations`/`world.types` keyed as they are and precomputes `gaps` per operation (each `Gap` with `operation: Some(id)`, plus `NoIdentity` for the op's type). It lowercases header names for lookup. `record_effects` writes `world_gap` whenever `generated.world_gaps` is non-empty, even with no effects.
- [ ] **Step 4: Run and verify the tests pass.** `cargo test -p mb-protocols && cargo test -p mb-engine` → PASS.
- [ ] **Step 5: Commit** `git commit -m "runtime: compile the effective world into the runtime, mark world gaps"`

### Task 4: Load the effective world at build; hot reload on a revision

**Files:**
- Modify: `crates/mb-store/src/sqlite.rs`, `crates/mb-store/src/postgres.rs` (`add_world_revision`: after commit, `publish_mock` when the version's status is active; the status is read in the same transaction)
- Modify: `crates/mockingbird/src/runtime.rs` (`build_runtime_tracked`)
- Test: `crates/mb-store/tests/it/conformance.rs`, `crates/mockingbird/tests/it/world_runtime.rs` (new; register it in `tests/it/main.rs`)

**Interfaces:**
- Consumes: `MockRuntime::with_world` (Task 3), `mb_admin::world::effective_world` (step 1).
- Produces: a runtime whose `world_revision()` is the newest revision of its active contract version.

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
#[tokio::test] async fn a_store_error_loading_the_world_serves_without_it() { /* failing latest_world_revision → runtime built, world_revision 0 */ }
```

- [ ] **Step 2: Run and verify the tests fail.** `cargo test -p mb-store --test it conformance && cargo test -p mockingbird --test it world_runtime` → FAIL.
- [ ] **Step 3: Implement.** In `build_runtime_tracked`, after `MockRuntime::new`: `match effective_world(..) { Ok(e) => rt = rt.with_world(&e.world, e.revision), Err(e) => tracing::warn!(mock = %mock.name, error = %e, "serving without the world description") }`. Call it before `set_data_plane_auth`, or rely on `with_world` recomputing the credential names.
- [ ] **Step 4: Run and verify the tests pass.** Run the same commands, plus `LC_ALL=en_US.UTF-8 scripts/test-postgres.sh`. Expected: PASS.
- [ ] **Step 5: Commit** `git commit -m "runtime: build with the effective world, rebuild on a new revision"`

### Task 5: Identity and record pointers

The cascades come from here: about 80 checks across blind7-10, plus 132 (header id) and 36 (id or alias) in blind11.

**Files:**
- Create: `crates/mb-protocols/src/call/records.rs`
- Modify: `crates/mb-protocols/src/call.rs`:
  - the `write_state` create arm: try `records::created(..)` first, and push `"no_record"` to `generated.world_gaps` when a 2xx create stores nothing;
  - `by_path_fields`/`read_state`: try `records::by_alternate(..)` on a miss;
  - `mint_create_id`: format a minted id with `identity.pattern`.
- Modify: `crates/mb-protocols/src/http_rest/handler.rs` (after `mb_engine::headers` fills the response headers, the operation's `identity_header` is set to `records::header_id`'s id; the engine stays unaware of it)
- Test: `crates/mb-protocols/tests/it/described_identity.rs` (new)

**Interfaces:**
- Consumes: `Described::{op, type_desc}` (Task 3); `IdentityDesc.alternates`, `AnswerDesc.identity_header` (Task 2); `RecordAt::Pointer` (Task 1).
- Produces:
  - `records::created(rt: &MockRuntime, op: &Operation, request: Option<&Value>, answer: Option<&Value>) -> Vec<Value>`, the records a create makes:
    - `request.records`/`answer.records` items, one each;
    - else the `long_running.result` member of the answer;
    - else the one record at `answer.record`.
  - `records::header_id(rt, op, generated: &GeneratedResponse) -> Option<(String /*header*/, String /*id*/)>`.
  - `records::by_alternate(state: &mut StateAccess<'_>, session, t: &str, value: &str) -> Option<StoredResource>`. It returns the record only when exactly one held record of `t` has an alternate member equal to `value`.
  - `records::patterned(pattern: &str, id: &str) -> String` and `records::bare_id(pattern: &str, value: &str) -> Option<String>`.

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
#[tokio::test] async fn a_header_only_create_id_is_stored_and_echoed() {
    // POST /conversations answers 201 {"threadIds":[1]} with Resource-ID declared; world answer.identity_header "Resource-ID"
    // response header Resource-ID == id; GET /conversations/{id} → state, holds the request's subject
}
#[tokio::test] async fn a_location_identity_header_is_the_item_url() { /* Location: <base>/conversations/{id} */ }
#[tokio::test] async fn a_long_running_create_stores_its_result() {
    // answer {"name":"operations/1","done":true,"response":{"name":"folders/7","displayName":"a"}}, long_running {done: done, result: response}
    // GET /folders/7 → state
}
#[tokio::test] async fn a_patterned_identity_reads_by_bare_or_full_id() { /* pattern "folders/{id}" */ }
#[tokio::test] async fn an_alternate_member_reaches_the_same_record() {
    // identity TemplateId, alternates [Alias]; create {Alias: "welcome"}; GET /templates/welcome and GET /templates/{TemplateId} both state, same body;
    // DELETE /templates/welcome → GET /templates/{TemplateId} 404
}
#[tokio::test] async fn two_records_sharing_an_alternate_value_resolve_neither() { /* ambiguous: falls back to generation */ }
#[tokio::test] async fn a_create_that_stores_nothing_is_marked_no_record() { /* answer with no id anywhere and no description: request.world_gap contains "no_record" */ }
```

- [ ] **Step 2: Run and verify the tests fail.** `cargo test -p mb-protocols --test it described_identity` → FAIL. Only the first test may already pass, through Task 1's `apply`. If it does, keep it as the regression guard.
- [ ] **Step 3: Implement** per the Interfaces.
  - A record a create makes with no identity value gets one minted by `mint_create_id`, patterned when the type has a pattern. It is stored, and its id is placed in the answer at the pointer and in the `identity_header`.
  - The answer is re-validated (`Engine::serve_stored`). When it fails, keep the generated answer and store nothing.
  - `long_running.done` is set to `true` in the answer when the member is a boolean.
- [ ] **Step 4: Run and verify the tests pass.** `cargo test -p mb-protocols` → PASS.
- [ ] **Step 5: Commit** `git commit -m "runtime: identity, record pointers, bulk and long-running creates from the world"`

### Task 6: Operation kind, target, transition and changes

**Files:**
- Create: `crates/mb-protocols/src/call/transitions.rs`
- Modify: `crates/mb-protocols/src/call.rs`. In the `write_state` update and action arms, after a 2xx, call `transitions::apply(..)`. For a `Replace`, `write_memory::onto_record` replaces instead of merging.
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
  - `crates/mb-protocols/src/call/list_query.rs` (add `ListQuery.lifecycle: Option<lifecycle::Exclusion>`, filled by `of`; `matches` excludes soft-deleted records unless `listed_with` is sent).
- Test: `crates/mb-protocols/tests/it/described_lifecycle.rs` (new)

**Interfaces:**
- Consumes: `Described::state_value` (Task 3); `ListQuery` (Task 9); the delete arm as Task 6 left it.
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
  - `crates/mb-protocols/src/call.rs` (`prepare` only: call `repeats::described_conflict` and `replays::replayed` before generation; `complete` keeps the answer for `replays::keep`).
- Test: `crates/mb-protocols/tests/it/described_conflicts.rs` (new)

**Interfaces:**
- Consumes: `Described::{op, type_desc}` (Task 3); `Engine::refuse_from_state(bundle, op, call, code)`.
- Produces:
  - `repeats::described_conflict(state: &mut StateAccess<'_>, plan: &CallPlan<'_>, call: &OperationCall, target: Option<&str>) -> Option<u16>`. A create or update whose member set equals a held record's (another record for an update), within the same `scope` member value, answers `conflict`, else the first of `guards::refusal_codes(op)`. Soft-deleted records do not count. The scan is bounded by `MAX_SCAN`.
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
```

- [ ] **Step 2: Run and verify the tests fail.** `cargo test -p mb-protocols --test it described_conflicts` → FAIL.
- [ ] **Step 3: Implement** per the Interfaces. Refusal bodies come from `Engine::refuse_from_state`. When the operation declares no response for the code, nothing is refused and the call goes on as today.
- [ ] **Step 4: Run and verify the tests pass.** `cargo test -p mb-protocols` → PASS. The `concurrency*`, `create_repeats` and `create_refusals*` suites are unchanged.
- [ ] **Step 5: Commit** `git commit -m "runtime: described unique sets, concurrency tokens and idempotent replays"`

### Task 9: Paging, filters, scope and order

The biggest mechanism across blind7-10: about 175 checks.

**Files:**
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
- Modify: `crates/mb-protocols/src/call.rs` (`prepare`: a described list tries `state_lists::answer` before generation)
- Test: `crates/mb-protocols/tests/it/described_state_lists.rs` (new)

**Interfaces:**
- Consumes: `list_query::{of, matches, sort, page}` (Tasks 9 and 7); `rt.collections()`; `Engine::held_answer`/`serve_stored`.
- Produces: `state_lists::answer(state: &mut StateAccess<'_>, plan: &CallPlan<'_>, call: &OperationCall) -> Option<GeneratedResponse>`, tagged `Source::State`.
  - It gathers the held records of the item type, at most `MAX_SCAN`, then matches, sorts and pages them.
  - It places the page at `answer.records`, else at the list member `lists::list_parts` finds.
  - It writes the next position at `paging.response` or `response_header`.
  - It fills the envelope's other required members as `held_answer` does.
  - It returns `None`, so the list is generated, when the collection was neither listed nor written this session, or the answer does not validate.

- [ ] **Step 1: Write the failing tests**

```rust
#[tokio::test] async fn a_second_list_is_answered_from_state_with_the_new_record() { /* list (generated, seeded), create, list → state, includes it, provider called twice not three times */ }
#[tokio::test] async fn a_described_filter_answers_from_state_exactly() {}
#[tokio::test] async fn pages_from_state_never_repeat_an_item() {}
#[tokio::test] async fn a_list_page_whose_envelope_cannot_be_filled_is_generated() { /* required free-number member → source llm */ }
#[tokio::test] async fn an_undescribed_list_is_generated_as_today() {}
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
  - `derived::fill(state: &mut StateAccess<'_>, session: &SessionKey, t: &str, record: &mut Value) -> Vec<String>`, which returns the members it set;
  - `derived::Decimal::{parse(&Value) -> Option<Decimal>, add, to_value(&self, member_schema: &Value) -> Option<Value>}`.
- Rules:
  - A child is a held record of type `of` whose `parent.member` (pattern-aware) equals the record's identity value, or which is stored under a collection whose `parent.path_param` value is that id.
  - `sum` adds `member` over the children matching `where`.
  - The result goes into an `integer` member only when integral, into a `number` member as a number, and into a `string` member as a decimal string at the children's largest scale.
  - Nothing is set unless the record was created this session or a child under it is held. A result that does not fit the member's schema is not set.

- [ ] **Step 1: Write the failing tests**

```rust
#[tokio::test] async fn a_count_follows_children_created_and_deleted() { /* order.line_count after 2 lines, then 1 */ }
#[tokio::test] async fn a_sum_filters_by_state() { /* balance = sum(amount where status posted) */ }
#[tokio::test] async fn a_sum_into_a_string_member_keeps_the_children_scale() { /* "12.50" + "1.25" → "13.75" */ }
#[tokio::test] async fn a_generated_parent_with_no_held_children_keeps_the_models_value() {}
#[test] fn decimal_adds_exactly() { /* 0.1 + 0.2 == 0.3 */ }
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

- **The blind11 "engine now" fixes:** 1, 2, 3, 5–9, 11–13 and 15. They are structural fixes with no description needed, each in its own PR. See "Coordination" above.
- **Vocabulary the diagnosis asks for beyond Task 2:**
  - `errors` (`{status, cause} → member values`, blind11 mechanism 2);
  - `masked` write-only members (mechanism 9);
  - child records of a fixed kind made from a create's request array (mechanism 14, e.g. order lines).

  Each needs a spec amendment first.
- **Step 4** (`--world` bench tiers), **step 5** (convention detectors) and **step 6** (`mockingbird world draft`). Detectors from step 5 produce tier-0 entries. This plan's runtime reads them with no further change, because `apply` and `Described` read the effective world whatever its author.
