# World Descriptions (steps 1–2) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Every contract version gets a validated world description. Heuristics produce it, it can be read with its gaps through MCP, CLI and the Admin API, and an assistant or person can write it as revisions. No runtime behaviour changes yet.

**Architecture:** World types and a validator live in `mb-mcm`, because they describe a compiled `Bundle`. `mb-import` builds the tier-0 description from what the heuristics already put in the bundle. Storage keeps only the authored overlay, as numbered revisions per contract version. The effective description is computed on read: tier 0 for the bundle, overlaid with the newest revision, then validated. The Admin API, MCP tools and CLI expose it. Steps 3–6 (runtime reads, bench tiers, convention detectors, LLM drafting) get their own plans after this one lands.

**Tech Stack:** Rust workspace; serde/serde_json; `jsonschema` 0.60 and `json-patch` 4 (both already workspace deps); sqlx migrations for SQLite and Postgres; axum and utoipa in `mb-admin`; the MCP server in `crates/mockingbird/src/mcp.rs`; agent skills in `plugins/mockingbird/skills`.

**Spec:** `docs/superpowers/specs/2026-11-04-world-description-design.md`

## Global Constraints

- **Optional:** a contract alone still works, and no request path changes behaviour in this plan.
- **Validated:** an entry that doesn't resolve against the contract is refused with an error naming its JSON pointer. The rest applies.
- **No API names in the engine.** Per-API knowledge lives only in descriptions.
- **No secrets:** a description holds no values from traffic and no credentials.
- **Authors:** `heuristic`, `agent`, `llm` or `user`. A later tier overrides an earlier one entry by entry.
- **Revisions:** a change adds a numbered revision attached to its contract version. It never recompiles the contract or adds a contract version. The newest revision applies at once.
- **Carry-forward:** a new contract version starts from the previous version's newest overlay. Entries that still validate carry forward; the rest are reported as gaps.
- **Repo rules:**
  - no `unwrap()`/`expect()` outside tests;
  - Admin API errors are `application/problem+json`;
  - after an Admin API change, run `npm run gen:api` in `crates/mb-console/web`;
  - new config keys go in `docs/configuration.md` and `docs/examples/mockingbird.full.yaml`;
  - skills are embedded via `crates/mockingbird/src/agents.rs`;
  - `plugins/mockingbird/bin/install.sh` stays a copy of `install.sh`.
- **Gates for every task:**
  - `cargo fmt --all --check`
  - `cargo clippy --workspace --all-targets --all-features -- -D warnings`
  - `cargo test --workspace --all-features`
  - `RUSTDOCFLAGS="-D warnings" cargo doc --workspace --all-features --no-deps`
  - `LC_ALL=en_US.UTF-8 scripts/test-postgres.sh` when `mb-store` or `mb-state` change
  - console `npm run typecheck && npm run lint && npm test` when the console client changes

## Review Focus

1. **Stale entries after a contract change:** an overlay names a member, operation or status the new contract no longer has. Expected: those entries are listed under `refused` with code `unresolved`, everything else applies, and reads never fail. (Test in Task 3.)
2. **Two writers at once:** an assistant and a person both patch from revision 3. Expected: the second write sent with `If-Match: 3` gets 412 `revision_conflict`; without `If-Match` it adds revision 5 on top of 4. (Test in Task 7.)
3. **Oversized or hostile documents:** a 5 MB description, deep nesting, a pointer such as `/../`, or unknown keys. Expected:
   - over 256 KiB gives 413;
   - depth over 32, or a pointer that doesn't parse, gives 422 naming the path;
   - unknown keys are refused as `unknown_key`, not ignored.

   (Tests in Tasks 2 and 7.)
4. **Contracts without HTTP routes** (GraphQL, gRPC, SOAP, AsyncAPI). Expected: `get_world` works, keyed by operation id. Tier 0 holds kind, type and identity, there are no skipped routes, and HTTP-only keys on such an operation (e.g. a `header` token) are refused as `not_applicable`. (Test in Task 3.)
5. **Old versions with no revisions** (created before this change). Expected: `get_world` returns tier 0 with revision `0` and no error, and the first write creates revision 1. (Test in Task 6.)

---

## File structure

| File | Responsibility |
| --- | --- |
| `crates/mb-mcm/src/world.rs` | World types (serde), `Author`, `overlay`, `Pointer` parsing |
| `crates/mb-mcm/src/world/validate.rs` | `validate(&World, &Bundle) -> Validation` |
| `crates/mb-mcm/schemas/world.v1.json` | Published JSON Schema, exported as `mb_mcm::world::SCHEMA` |
| `crates/mb-import/src/world.rs` | `tier0(&Bundle) -> World` |
| `crates/mb-protocols/src/world_gaps.rs` | `gaps(&Bundle, &World) -> Vec<Gap>`, including skipped HTTP routes from `RouteTable::new(bundle).skipped()` |
| `crates/mb-store/migrations/{sqlite,postgres}/1200_world_revisions.sql` | Revision table |
| `crates/mb-store/src/{lib,model,sqlite,postgres}.rs` | Revision store methods |
| `crates/mb-admin/src/world.rs` | Admin routes, views, effective-world assembly |
| `crates/mockingbird/src/mcp.rs` | MCP tools |
| `crates/mockingbird/src/client.rs` | `mockingbird world …` CLI |
| `crates/mockingbird/src/file_config.rs`, `reconcile.rs` | `mocks.<name>.world` |
| `crates/mockingbird/src/snapshot.rs` | `SnapshotPart::World` |
| `plugins/mockingbird/skills/mockingbird-world/SKILL.md` | Assistant instructions |
| `docs/world.md` | User docs |

---

### Task 1: World types and JSON Schema

**Files:**
- Create: `crates/mb-mcm/src/world.rs`, `crates/mb-mcm/schemas/world.v1.json`
- Modify: `crates/mb-mcm/src/lib.rs` (add `pub mod world;`), `crates/mb-mcm/Cargo.toml` (dev-dep `jsonschema`)
- Test: `crates/mb-mcm/src/world.rs` (unit), `crates/mb-mcm/tests/world_schema.rs`

**Interfaces:**
- Produces:
  - `pub struct World { pub version: u32, pub types: BTreeMap<String, TypeDesc>, pub operations: BTreeMap<String, OpDesc> }`. `version` is always `1`. Every field is serde `default`, and unknown keys are kept in `extra: BTreeMap<String, Value>` via `#[serde(flatten)]`, so the validator can refuse them.
  - `pub struct TypeDesc { identity: Option<IdentityDesc>, parent: Option<ParentDesc>, lifecycle: Option<LifecycleDesc>, unique: Vec<UniqueDesc>, concurrency: Option<ConcurrencyDesc>, derived: BTreeMap<String, DerivedDesc>, extra }`
  - `pub struct IdentityDesc { member: String, pattern: Option<String> }`. `pattern` holds exactly one `{id}`.
  - `pub struct ParentDesc { r#type: String, member: Option<String>, path_param: Option<String>, pattern: Option<String> }`
  - `pub struct LifecycleDesc { member: String, states: Vec<String>, initial: Option<String>, soft_delete: Option<SoftDelete> }` and `pub struct SoftDelete { state: String, listed_with: Option<ParamValue> }`
  - `pub struct UniqueDesc { members: Vec<String>, scope: Option<String>, conflict: Option<u16> }`
  - `pub struct ConcurrencyDesc { member: Option<String>, header: Option<String>, conflict: Option<u16> }`
  - `pub enum DerivedDesc { Count { of: String }, Sum { of: String, member: String, r#where: Option<BTreeMap<String, Value>> } }`, serde tag `kind`
  - `pub struct OpDesc { kind: Option<OpKind>, r#type: Option<String>, target: Option<TargetDesc>, request: Option<RecordsAt>, answer: Option<AnswerDesc>, transition: Option<BTreeMap<String, Value>>, changes: BTreeMap<String, ChangeDesc>, scope: Option<ScopeDesc>, filters: Vec<FilterDesc>, order: Option<OrderDesc>, paging: Option<PagingDesc>, idempotency: Option<HeaderRef>, not_found: Option<u16>, extra }`
  - `pub enum OpKind { Create, Read, List, Update, Replace, Delete, Action }`, snake_case
  - `pub struct TargetDesc { path_param: Option<String>, member: Option<String> }`
  - `pub struct RecordsAt { record: Option<Pointer>, records: Option<Pointer> }`
  - `pub struct AnswerDesc { record: Option<Pointer>, records: Option<Pointer>, long_running: Option<LongRunning> }` and `pub struct LongRunning { done: String, result: String }`
  - `pub enum ChangeDesc { FromRequest(Pointer), Value(Value) }`, serde keys `from_request` and `value`
  - `pub struct ScopeDesc { param: String, member: String }`
  - `pub struct FilterDesc { param: String, member: Option<String>, kind: FilterKind }`, with `FilterKind = Eq | Contains | Gte | Lte | Flag | Text`
  - `pub struct OrderDesc { member: String, descending: bool }`
  - `pub struct PagingDesc { style: PagingStyle, request: Option<String>, response: Option<String>, response_header: Option<String>, size: Option<String> }`, with `PagingStyle = Cursor | Page | Offset | NextUrl | HeaderCursor`
  - `pub struct HeaderRef { header: String }`
  - `pub struct ParamValue { param: String, value: Value }`
  - `pub struct Pointer(String)`, an RFC 6901 JSON pointer. `Pointer::parse(&str) -> Result<Pointer, PointerError>` rejects strings not starting with `/`, empty, and invalid `~` escapes.
  - `pub enum Author { Heuristic, Agent, Llm, User }`, snake_case
  - `pub fn overlay(base: &World, top: &World) -> World`: entry by entry, where a set field of `top` replaces `base`'s field. Vectors replace whole.
  - `pub const SCHEMA: &str = include_str!("../schemas/world.v1.json");`
  - `pub const MAX_BYTES: usize = 262_144; pub const MAX_DEPTH: usize = 32;`

- [ ] **Step 1: Write the failing tests**
  - `world_roundtrips_the_spec_excerpt`: parse the YAML excerpt from the spec, converted to JSON, into `World`. Serialize it, parse it again, and assert equality. Assert `operations["MoveFolder"].kind == Some(OpKind::Action)`.
  - `unknown_keys_are_kept_in_extra`: `{"version":1,"types":{"t":{"identty":{}}}}` → `types["t"].extra` contains `identty`.
  - `overlay_replaces_set_fields_only`: base `identity.member="id"` with `lifecycle` set; top `identity.member="name"` → the result has member `name` and keeps the base `lifecycle`.
  - `pointer_parse_rejects_bad_pointers`: `""`, `"a"`, `"/a~2"` are errors; `"/Contacts"` and `"/result/0"` are ok.
  - In `tests/world_schema.rs`, `schema_accepts_spec_example_and_rejects_unknown_kind`: the spec excerpt validates against `SCHEMA`; `{"version":1,"operations":{"x":{"kind":"explode"}}}` fails.
- [ ] **Step 2:** run `cargo test -p mockingbird-mcm world`. Expected: FAIL (module missing).
- [ ] **Step 3:** implement the types, `overlay`, `Pointer::parse`, and the JSON Schema (draft 2020-12, `additionalProperties: false` everywhere, the enums above).
- [ ] **Step 4:** run `cargo test -p mockingbird-mcm`. Expected: PASS.
- [ ] **Step 5:** commit `feat(mcm): world description types and schema`.

### Task 2: Validator

**Files:**
- Create: `crates/mb-mcm/src/world/validate.rs` (make `world` a directory module: `world/mod.rs` holds Task 1)
- Test: unit tests in the same file, with a small synthetic `Bundle` built in a test helper `fn bundle(ops: &[(&str, &str, &str, Value)]) -> Bundle` (op id, method, path, success schema)

**Interfaces:**
- Consumes: Task 1 types; `Bundle`, `Operation`, `success_output`, `defs`.
- Produces:
  - `pub fn validate(world: &World, bundle: &Bundle) -> Validation`
  - `pub struct Validation { pub applied: World, pub refused: Vec<Refusal> }`
  - `pub struct Refusal { pub path: String, pub code: RefusalCode, pub message: String }`
  - `pub enum RefusalCode { UnknownKey, Unresolved, NotApplicable, Invalid }`. Here `path` is a JSON pointer into the description, e.g. `/operations/MoveFolder/target/path_param`.
  - `pub fn check_size(bytes: &[u8]) -> Result<(), Refusal>`, which enforces `MAX_BYTES` and `MAX_DEPTH`.

Rules (each failing entry is dropped from `applied` and listed once):
- **`operations` keys:** an operation id in the bundle, else `unresolved`.
- **`types` keys:** a `resource_type` some operation has, or a key of `defs`, else `unresolved`.
- **Members** (`identity.member`, `lifecycle.member`, `unique.members`, `unique.scope`, `concurrency.member`, `scope.member`, `filters.member`, `order.member`, `derived.*.member`, `changes` keys) must be a property of the type's record schema. The record schema is the success output of the type's read operation, or the first operation of that type, resolved through `answer.record` when set and `$ref` into `defs`. A member is a dotted path through nested properties.
- **Parameters** (`target.path_param`, `scope.param`, `filters.param`, `paging.request`, `paging.size`, `listed_with.param`) must be declared on that operation, or on some list operation of the type for `listed_with`.
- **Headers** (`concurrency.header`, `idempotency.header`, `paging.response_header`): declared request or response headers of the operation, or of the type's write operations for `concurrency`. On a non-HTTP operation they are `not_applicable`.
- **Statuses** (`conflict`, `not_found`) must be declared responses on the operation, or on the type's writes. On a non-HTTP operation (or a type with no HTTP operation) they are `not_applicable`.
- **Pointers** (`request`, `answer`) must resolve inside the operation's request or success schema. `records` must point to an array.
- **Other rules:**
  - `identity.pattern` holds exactly one `{id}`;
  - `lifecycle.initial` and `soft_delete.state` are in `states`, when `states` is non-empty;
  - `derived.of` names a type whose `parent.type` is this type;
  - `transition` keys are the lifecycle member of the op's type; with no applied lifecycle for that type, every key is `unresolved`;
  - a `unique` set whose `scope` does not resolve is refused whole, so it never widens to global uniqueness;
  - a sub-object (`target`, `request`, `answer`, `concurrency`) whose every entry is refused is dropped, as `transition` is, so it cannot mask a lower tier; one authored empty stays;
  - schema lookups through `$ref`s and `allOf`/`anyOf`/`oneOf` visit each schema at most once, so a self-referencing composition cannot blow up;
  - each unknown key in any `extra` is `unknown_key`.

- [ ] **Step 1: Write the failing tests:**
  - `validate_keeps_resolving_entries`;
  - `validate_refuses_unknown_operation` (expect `/operations/Nope`, `unresolved`);
  - `validate_refuses_member_not_in_schema` (expect `/types/folder/identity/member`);
  - `validate_resolves_member_through_answer_pointer` (record under `/result`);
  - `validate_refuses_records_pointer_to_non_array`;
  - `validate_refuses_header_on_graphql_operation` (`not_applicable`);
  - `validate_refuses_undeclared_conflict_status`;
  - `validate_reports_unknown_keys`;
  - `check_size_rejects_over_256k_and_depth_33`.
- [ ] **Step 2:** run `cargo test -p mockingbird-mcm validate`. Expected: FAIL.
- [ ] **Step 3:** implement. Reuse `mb_mcm`'s existing `$ref` resolution helpers; don't write a second resolver.
- [ ] **Step 4:** run `cargo test -p mockingbird-mcm`. Expected: PASS.
- [ ] **Step 5:** commit `feat(mcm): validate world descriptions against the bundle`.

### Task 3: Tier 0 and gaps

**Files:**
- Create: `crates/mb-import/src/world.rs`, `crates/mb-protocols/src/world_gaps.rs`
- Modify: `crates/mb-import/src/lib.rs` (`pub mod world;`), `crates/mb-protocols/src/lib.rs`
- Test: `crates/mb-import/tests/it/world_tier0.rs`, `crates/mb-protocols/tests/it/world_gaps.rs`

**Interfaces:**
- Consumes: Tasks 1–2; `RouteTable::new(&Bundle).skipped() -> &[RouteError]`.
- Produces:
  - `pub fn tier0(bundle: &Bundle) -> World` (mb-import). It maps only what the bundle already holds:
    - `OperationKind` and `Effect` to `OpKind` (Query with identity → `Read`, Query without → `List`, `Command(Create|Update|Delete)` → the same, `Other` → `Action`);
    - `resource_type` → `type`;
    - `Identity { path_param, field, record }` → `target.path_param`, `types[t].identity.member`, and `answer.record` (`RecordAt::Member(m)` → `/m`, `RecordAt::Item` → `/0`, and `MemberItem(m)` once #236 lands → `/m/0`).

    It adds nothing else.
  - `pub struct Gap { pub code: GapCode, pub operation: Option<String>, pub r#type: Option<String>, pub message: String }` and `pub enum GapCode { NoIdentity, NoTarget, NoRecord, SkippedRoute, Refused }`
  - `pub fn gaps(bundle: &Bundle, world: &World, refused: &[Refusal]) -> Vec<Gap>` (mb-protocols):
    - `NoIdentity`: a type with writes and no `identity`;
    - `NoTarget`: an update, delete, action or read with a type and no `target`;
    - `NoRecord`: a create or update whose success schema is an object with no member fitting the type and no `answer.record`;
    - `SkippedRoute`: one per `RouteError`, with its `Display` text;
    - `Refused`: one per refusal.

- [ ] **Step 1: Write the failing tests:**
  - `tier0_validates_cleanly_on_every_repo_contract`: compile every contract under `examples/apis/**` and `scripts/llm-bench/contracts/**`, then assert `validate(&tier0(&b), &b).refused.is_empty()`.
  - `tier0_matches_bundle_identity`: for each operation with identity, the tier-0 `target.path_param` equals `identity.path_param`, and the type's identity member equals `identity.field`.
  - `gaps_list_skipped_routes`: a synthetic contract with `/a/{x}` and `/a/{y}` GET gives one `SkippedRoute`.
  - `gaps_list_no_identity_for_write_only_type`.
  - `gaps_for_graphql_have_no_skipped_routes`.
  - `stale_entries_are_refused_not_fatal`: an overlay naming a removed member, overlaid on tier 0 and validated, keeps the other entries and gives one `Refused` gap.
- [ ] **Step 2:** run the tests. Expected: FAIL.
- [ ] **Step 3:** implement.
- [ ] **Step 4:** run `cargo test -p mockingbird-import -p mockingbird-protocols world`. Expected: PASS.
- [ ] **Step 5:** commit `feat: tier-0 world descriptions and gap report`.

### Task 4: Revision storage

**Files:**
- Create: `crates/mb-store/migrations/sqlite/1200_world_revisions.sql`, `crates/mb-store/migrations/postgres/1200_world_revisions.sql`
- Modify: `crates/mb-store/src/lib.rs` (trait), `model.rs`, `sqlite.rs`, `postgres.rs`
- Test: the store's existing conformance tests module (add cases run on both backends)

**Interfaces:**
- Produces:
  - Table `world_revisions(id UUID/TEXT PK, mock_id, version INTEGER, revision INTEGER, author TEXT, document TEXT, created_by TEXT NULL, created_at, UNIQUE(mock_id, version, revision))`, with `ON DELETE CASCADE` from the mock.
  - `pub struct WorldRevision { id: Uuid, mock_id: Uuid, version: i64, revision: i64, author: Author, document: Value, created_by: Option<String>, created_at: DateTime<Utc> }` and `WorldRevisionInfo` (without `document`)
  - `pub struct NewWorldRevision { author: Author, document: Value, created_by: Option<String>, expect: Option<i64> }`
  - Trait methods:
    - `async fn add_world_revision(&self, mock_id: Uuid, version: i64, r: NewWorldRevision) -> Result<WorldRevision, StoreError>`: the new revision is the current max + 1. If `expect` is `Some(n)` and the current max isn't `n`, it returns `StoreError::Conflict`. A missing contract version gives `NotFound`.
    - `async fn list_world_revisions(&self, mock_id: Uuid, version: i64) -> Result<Vec<WorldRevisionInfo>, StoreError>`, newest first.
    - `async fn get_world_revision(&self, mock_id: Uuid, version: i64, revision: i64) -> Result<WorldRevision, StoreError>`
    - `async fn latest_world_revision(&self, mock_id: Uuid, version: i64) -> Result<Option<WorldRevision>, StoreError>`
  - Add a `StoreError::Conflict` variant only if it doesn't exist; check `error.rs` first.

- [ ] **Step 1:** write the failing conformance tests:
  - `world_revisions_number_from_one_per_version`;
  - `world_revision_expect_mismatch_conflicts`;
  - `world_revisions_go_with_the_mock`, where deleting the mock removes them.
- [ ] **Step 2:** run `cargo test -p mockingbird-store` and `LC_ALL=en_US.UTF-8 scripts/test-postgres.sh`. Expected: FAIL.
- [ ] **Step 3:** implement both backends.
- [ ] **Step 4:** rerun both. Expected: PASS.
- [ ] **Step 5:** commit `feat(store): world description revisions`.

### Task 5: Effective world assembly and carry-forward

**Files:**
- Create: `crates/mb-admin/src/world.rs` (the assembly part)
- Modify: the places that add contract versions. Find them with `grep -n add_contract_version crates/`: `mb-admin/src/contracts.rs`, `drafts.rs` approve, `imports/worker.rs`, and `crates/mockingbird/src/reconcile.rs`.
- Test: `crates/mb-admin/tests/it/world.rs`

**Interfaces:**
- Consumes: Tasks 1–4.
- Produces:
  - `pub struct EffectiveWorld { pub world: World, pub authors: BTreeMap<String, Author>, pub revision: i64, pub refused: Vec<Refusal>, pub gaps: Vec<Gap> }`. `authors` maps each entry pointer set by the overlay (at the granularity of `/types/{t}/{field}` and `/operations/{op}/{field}`) to the author of the newest revision that set it. Every other entry is `heuristic`.
  - `pub async fn effective_world(store: &dyn ControlStore, mock_id: Uuid, version: i64, bundle: &Bundle) -> Result<EffectiveWorld, StoreError>`: `tier0(bundle)` overlaid with the latest revision's document, validated, plus `gaps`. When there is no revision, `revision = 0`.
  - `pub async fn carry_forward(store: &dyn ControlStore, mock_id: Uuid, from: i64, to: i64, to_bundle: &Bundle) -> Result<Option<WorldRevision>, StoreError>`: copies `from`'s latest overlay, keeping only the entries `validate` applies against `to_bundle`, as revision 1 of `to`. It does nothing when `from` has no revision. It is called right after every new contract version is added.

- [ ] **Step 1:** write the failing tests:
  - `effective_world_without_revisions_is_tier0_at_revision_0`;
  - `overlay_entries_are_marked_with_their_author`;
  - `carry_forward_keeps_valid_entries_and_drops_stale`, where the new version removes a member;
  - `carry_forward_runs_on_import_of_a_new_version`.
- [ ] **Step 2:** run `cargo test -p mockingbird-admin world`. Expected: FAIL.
- [ ] **Step 3:** implement, and wire `carry_forward` into each path that adds a version.
- [ ] **Step 4:** run the tests. Expected: PASS.
- [ ] **Step 5:** commit `feat(admin): effective world and carry-forward`.

### Task 6: Read API: Admin, MCP, CLI

**Files:**
- Modify: `crates/mb-admin/src/world.rs` (handlers), `crates/mb-admin/src/lib.rs` (routes), `crates/mb-admin/src/openapi.rs` (utoipa), `crates/mockingbird/src/mcp.rs` (tool + dispatch), `crates/mockingbird/src/client.rs` (`WorldCommand`), `crates/mb-console/web` (`npm run gen:api`)
- Test: `crates/mb-admin/tests/it/world.rs`, the MCP tests next to the other tool tests in `crates/mockingbird/tests`

**Interfaces:**
- Produces:
  - `GET /mocks/{mock}/contracts/{version}/world` → `200 WorldView { world, authors, revision, refused, gaps }`. `version=active` is accepted. A missing mock or version gives a 404 problem.
  - MCP tool `get_world { mock, version? }` (read-only), returning the same JSON.
  - CLI `mockingbird world show <mock> [--version N] [--json]`. The human output lists gaps first.
  - CLI `mockingbird world schema`, which prints `mb_mcm::world::SCHEMA`.

- [ ] **Step 1:** write the failing tests:
  - `get_world_returns_tier0_and_gaps` (Admin);
  - `get_world_on_pre_existing_version_is_revision_0` (Review Focus 5);
  - `mcp_get_world_matches_admin`;
  - `cli_world_schema_prints_valid_json`.
- [ ] **Step 2:** run them. Expected: FAIL.
- [ ] **Step 3:** implement, then run `npm run gen:api` and the console checks.
- [ ] **Step 4:** run the gates. Expected: PASS.
- [ ] **Step 5:** commit `feat: read world descriptions and gaps through Admin API, MCP and CLI`.

### Task 7: Authoring API: put, patch, revisions, restore, diff

**Files:**
- Modify: `crates/mb-admin/src/world.rs`, `lib.rs`, `openapi.rs`, `crates/mockingbird/src/mcp.rs`, `client.rs`, the console client
- Test: `crates/mb-admin/tests/it/world.rs`

**Interfaces:**
- Consumes: Task 5's `effective_world`; Task 4's `add_world_revision`.
- Produces:
  - `PUT /mocks/{m}/contracts/{v}/world`: the body is an overlay `World`, and it replaces the overlay. It takes an optional `If-Match: <revision>`. `check_size` failures give 413/422. The answer is `201 WorldWriteResult { revision, applied: World, refused: Vec<Refusal> }`. Only `applied` is stored, so refused entries aren't saved. The author comes from the `author` query parameter (`agent`|`llm`|`user`, default `user`); `heuristic` is refused.
  - `PATCH` (same path, `application/merge-patch+json`): merges into the current overlay with `json_patch::merge`, then behaves as PUT.
  - `GET .../world/revisions` → `[WorldRevisionInfo]`; `GET .../world/revisions/{r}` → `WorldRevision`.
  - `POST .../world/revisions/{r}/restore`: adds a new revision whose document is `r`'s, validated again. The answer is `WorldWriteResult`.
  - `GET .../world/diff?from=a&to=b` → `{ added: [pointer], removed: [pointer], changed: [pointer] }`, at the leaf level of the overlay documents.
  - A conflict gives `412` problem `revision_conflict`.
  - Some malformed entries fail deserialization of the whole `World` rather than reaching the validator: unknown keys inside externally tagged or enum-valued entries (`changes.a = {from_request: "/a", bogus: 1}`, `derived` kind `avg`, an unknown `OpKind` or `FilterKind`). Map that serde error to a 422 problem that names the JSON pointer (e.g. with `serde_path_to_error`), or make those enums tolerant. Check this in the Task 7 review.
  - MCP tools: `put_world {mock, version?, world, author?, expect_revision?}`, `patch_world {mock, version?, patch, author?, expect_revision?}`, `list_world_revisions`, `restore_world_revision`, `diff_world`.
  - CLI: `mockingbird world put|patch|revisions|restore|diff`.

- [ ] **Step 1:** write the failing tests:
  - `put_world_stores_applied_only_and_reports_refused`;
  - `patch_world_merges_and_bumps_revision`;
  - `stale_if_match_is_412` (Review Focus 2);
  - `put_over_256k_is_413` (Review Focus 3);
  - `restore_adds_a_new_revision`;
  - `diff_lists_changed_pointers`;
  - `heuristic_author_is_refused`.
- [ ] **Step 2:** run them. Expected: FAIL.
- [ ] **Step 3:** implement, then run `npm run gen:api`.
- [ ] **Step 4:** run the gates. Expected: PASS.
- [ ] **Step 5:** commit `feat: author world descriptions as revisions`.

### Task 8: Config-file mocks and snapshots

**Files:**
- Modify: `crates/mockingbird/src/file_config.rs` (`FileMock.world: Option<PathBuf>`), `crates/mockingbird/src/reconcile.rs`, `crates/mockingbird/src/snapshot.rs` (`SnapshotPart::World`), `docs/configuration.md`, `docs/examples/mockingbird.full.yaml`
- Test: the existing reconcile and snapshot tests

**Interfaces:**
- Produces:
  - `mocks.<name>.world: <path>` takes YAML or JSON, relative to the config file. On reconcile the file is validated and stored as a revision with author `user`, only when its content differs from the newest revision's. Refused entries are logged at warn level and don't fail startup; an unreadable file fails startup with its path.
  - `SnapshotPart::World` (`--include world`) writes `worlds/<mock>.json`, the newest overlay of the active version, and imports it as a revision with author `user`.

- [ ] **Step 1:** write the failing tests:
  - `config_world_file_becomes_a_revision`;
  - `unchanged_config_world_adds_no_revision`;
  - `snapshot_roundtrips_world`;
  - the existing test that checks `configuration.md` and `mockingbird.full.yaml` list every key, which should fail until the docs are updated.
- [ ] **Step 2:** run. Expected: FAIL.
- [ ] **Step 3:** implement and document.
- [ ] **Step 4:** run the gates. Expected: PASS.
- [ ] **Step 5:** commit `feat: world descriptions in config files and snapshots`.

### Task 9: The `mockingbird-world` skill and docs

**Files:**
- Create: `plugins/mockingbird/skills/mockingbird-world/SKILL.md`, `docs/world.md`
- Modify: `crates/mockingbird/src/agents.rs` (`SKILLS`), `plugins/mockingbird/skills/mockingbird/SKILL.md` (router), `plugins/mockingbird/skills/mockingbird-mock-api/SKILL.md` (review the world after every import), `CHANGELOG.md`
- Test: the existing embedding test in `agents.rs` (every skill dir is embedded), plus a new `skill_names_only_real_tools` test that checks every tool the skill names exists in `mcp::tools()`

**Content the skill fixes:**
1. After `import_contract`, call `get_world` and read `gaps` first.
2. For each gap, read that part of the contract and its docs, then write the entries with `patch_world` (`author: "agent"`). Record only what the contract or docs state.
3. Read `refused` in the answer and fix those entries.
4. Check with `call_mock` on a create, then read it back.
5. Never put values from traffic or credentials in a description.

Include the spec's excerpt as the worked example.

- [ ] **Step 1:** add the skill and docs. Run `cargo test -p mockingbird-cli agents`. Expected: FAIL until it's added to `SKILLS`.
- [ ] **Step 2:** register it and update the router and mock-api skills.
- [ ] **Step 3:** run the gates. Expected: PASS.
- [ ] **Step 4:** commit `docs: mockingbird-world skill and world description guide`.

---

## Follow-on plans (outlined; each gets its own plan once this one lands)

- **Step 3, the runtime reads the description:**
  1. identity and record pointers (identity, `envelopes`);
  2. kind, target, transition and changes;
  3. lifecycle and soft delete (`deleted.rs`, `lists.rs`);
  4. unique members and concurrency (`guards.rs`, `tokens.rs`);
  5. paging, filters, scope and order (`pages.rs`, `runs.rs`, `lists.rs`);
  6. derived counts and sums.

  Each module reads `EffectiveWorld` first and falls back to its heuristic. A `world_gap` mark goes in the request log.
- **Step 4, bench tiers:** `server_suites.py --world heuristic|llm|agent`. The agent annotator gets the contract text and `SKILL.md` only, and writes through `put_world`. Report each tier per model. Commission a blind tenth set.
- **Step 5, convention detectors** feeding tier 0, one PR each: AIP names, LROs and custom methods; Relay; OData; HAL and JSON:API; `Link` and header cursors; RPC-style POST.
- **Step 6, `mockingbird world draft`:** on request only. It uses the import profile and budget and writes revisions with author `llm`.
