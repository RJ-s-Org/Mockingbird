# WS3 Import and Drafts Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Smart-import drafts that need less hand repair (required/nullable, real operation ids, no invented defaults, low-confidence operations dropped), OpenAPI blocks embedded in docs pages taken as-is, imports that merge several sources into the existing contract, approval that activates, and warnings for response schemas too big for the prompt.

**Architecture:** Everything that decides draft content stays in `mb-import` (`smart/*` and `lib.rs`), as pure functions tested with `ScriptedProvider` and fixtures. `mb-store` gains one migration (contract-version warnings, import-job mode) and one trait method. `mb-admin` threads the new request fields (`urls`, `documents`, `mode`, `activate`) through the import job and approve endpoints and returns warnings on version summaries. The CLI `import` command and `validate` print the new data. The console client is regenerated; the console keeps its two-step approve/activate by sending `activate: false`.

**Tech Stack:** Rust workspace (toolchain from `rust-toolchain.toml`), axum + utoipa (Admin API), sqlx (SQLite/Postgres migrations), `scraper`, `pulldown-cmark`, `serde_saphyr` (already deps), `mb_llm::ScriptedProvider` and `wiremock` for tests, console `openapi-typescript` (`npm run gen:api`). No new dependencies.

**Spec:** `docs/superpowers/specs/2026-10-22-improvements-backlog-design.md`, section **WS3: Import and drafts** (3.1–3.4, Tests).

## Global Constraints

- Branch `feat/ws3-import-drafts` from `main`, in its own worktree; one PR; do **not** merge (the user approves each merge).
- Product code stays in WS3's files: `crates/mb-import/src/smart/*`, `crates/mb-import/src/lib.rs`, `crates/mb-admin/src/{drafts,contracts}.rs`, `crates/mb-admin/src/imports/*`, the CLI `import` path in `crates/mockingbird/src/client.rs` (`ImportArgs`, `import`, `document_body`, `smart_import`, new helpers next to them) and `validate` in `crates/mockingbird/src/main.rs`, plus the `mb-store` changes of Task 2 (no other workstream touches contract versions or import jobs). Three unavoidable one-line touches outside, each named in its task: `crates/mb-engine/src/compact.rs:16` (re-export, value unchanged), `crates/mockingbird/src/mcp.rs:1134` (new `ImportArgs` fields filled with defaults; WS5 owns the file and builds on it in wave 2), `crates/mockingbird/tests/observability.rs` (`StubControl` forwards the new store method).
- `mb-engine` depends on `mb-import`, so the budget constant moves **into** `mb-import` and `mb-engine` re-exports it: `pub const PROMPT_SCHEMA_BUDGET: usize = 64 * 1024;` (65 536 bytes of compact JSON).
- `mb_import::compile` keeps its signature (≈60 call sites in WS1/WS2/WS4 files); the spec's `compile -> (Bundle, Vec<Warning>)` is `mb_import::compile_with_warnings`.
- Low-confidence cut-off: operations with `confidence < 0.4` are dropped (`MIN_CONFIDENCE: f32 = 0.4`).
- Migration number **0650** (`0650_import_drafts.sql`) in both `crates/mb-store/migrations/sqlite` and `.../postgres`, leaving 0600–0640 to WS1/WS4/WS6.
- No `unwrap()`/`expect()` outside tests; Admin API errors are `application/problem+json` (`Problem::*`); no new config keys (so `docs/configuration.md` and `docs/examples/mockingbird.full.yaml` are unchanged).
- After each Admin API change: `npm run gen:api` in `crates/mb-console/web`, then `npm run typecheck && npm run lint && npm test`.
- Warning codes, exactly: `schema_too_large`, `low_confidence`, `invented_default`, `embedded_swagger2`, `source_skipped`, `merge_skipped`, `conflicting_component`. Existing codes are unchanged.
- Commits end with `Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>`; the PR body ends with `🤖 Generated with [Claude Code](https://claude.com/claude-code)`.
- Never print or write API keys (tests use `ScriptedProvider`/`wiremock`, no real profiles).

## Review Focus

1. **Re-importing the same docs** (default `merge` mode now applies whenever a draft or active version exists): the merged draft must have the same operations and ids as before, not `listPets_2` duplicates. Test: Task 9 `merging_a_draft_into_itself_is_idempotent`.
2. **Default merge over a non-OpenAPI active contract** (a GraphQL or proto mock that gets a docs import): the job must not fail; it imports as `replace` with a `merge_skipped` warning. An explicit `mode: merge` fails with a clear error. Test: Task 10 `default_merge_over_graphql_falls_back_to_replace`.
3. **Existing approve callers** (`{}` or an empty JSON body): now activate; the console must keep its explicit approve-then-activate flow. Tests: Task 11 `approve_activates_by_default` and the console `draft.test.tsx` body assertion.
4. **A ReadMe page whose embedded definition holds the whole API** while the page documents one endpoint: the draft holds every operation of the definition (block wins, no LLM call), not just the page's. Test: Task 7 `readme_page_imports_every_embedded_operation_without_llm`.
5. **A malformed embedded block** (`openapi: 3.0.0` with a `$ref` to a missing component, or YAML that does not parse): the import must not fail; the bad operation is skipped with `invalid_operation`, an unparseable block stays in the text for the LLM. Test: Task 7 `broken_embedded_block_is_skipped_not_fatal`.

---

### Task 1: Schema-size warnings from compilation

**Files:**
- Modify: `crates/mb-import/src/lib.rs` (new items after `compile`, `:177`)
- Modify: `crates/mb-engine/src/compact.rs:15-16` (re-export only)
- Modify: `crates/mockingbird/src/main.rs:195-215` (`validate`)
- Create: `fixtures/openapi/huge-response.json`
- Test: `crates/mb-import/tests/openapi3.rs`, `crates/mockingbird/tests/client.rs`

**Interfaces:**
- Consumes: `mb_mcm::Bundle::with_reachable_defs(&self, &Value) -> Value`, `OutputVariant { status, schema, .. }`.
- Produces (in `mb_import`):
  ```rust
  /// Bytes of compact JSON the generation prompt shows of a response schema.
  pub const PROMPT_SCHEMA_BUDGET: usize = 64 * 1024;

  /// A non-fatal finding of compilation.
  #[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
  pub struct Warning {
      /// `schema_too_large`.
      pub code: String,
      /// The operation id the warning is about.
      #[serde(default, skip_serializing_if = "Option::is_none")]
      pub operation: Option<String>,
      pub message: String,
      /// Compact JSON bytes of the schema with its reachable `$defs`.
      #[serde(default, skip_serializing_if = "Option::is_none")]
      pub size: Option<u64>,
  }
  pub fn compile_with_warnings(bytes: &[u8]) -> Result<(Bundle, Vec<Warning>), ImportError>;
  pub fn bundle_warnings(bundle: &Bundle) -> Vec<Warning>;
  ```
  `compile(bytes)` stays and equals `compile_with_warnings(bytes).map(|(b, _)| b)`.

- [ ] **Step 1: Create the fixture** — `fixtures/openapi/huge-response.json`: OpenAPI 3.0.3, title `Huge`, version `1`; `GET /big` (`operationId: getBig`, 200 → `$ref: '#/components/schemas/Big'`) and `GET /small` (`getSmall`, 200 → `{type: object}`); `Big` is an object with 2000 properties `field_00000`…`field_01999`, each `{"type": "string", "description": "<40 × x>"}` (≈140 KB). Generate it once with a short script; commit the JSON.

- [ ] **Step 2: Write the failing tests**

```rust
// crates/mb-import/tests/openapi3.rs
#[test]
fn compile_with_warnings_flags_schemas_over_the_prompt_budget() {
    let bytes = fixture("openapi/huge-response.json");
    let (bundle, warnings) = mb_import::compile_with_warnings(&bytes).unwrap();
    assert_eq!(bundle.operations.len(), 2);
    assert_eq!(warnings.len(), 1, "{warnings:?}");
    let w = &warnings[0];
    assert_eq!(w.code, "schema_too_large");
    assert_eq!(w.operation.as_deref(), Some("getBig"));
    assert!(w.size.unwrap() > mb_import::PROMPT_SCHEMA_BUDGET as u64);
    assert!(w.message.contains("getBig") && w.message.contains("trimmed"), "{}", w.message);
    assert_eq!(mb_import::compile(&bytes).unwrap(), bundle);
    let small = fixture("openapi/orders-3.0.yaml");
    assert!(mb_import::compile_with_warnings(&small).unwrap().1.is_empty());
}

// crates/mockingbird/tests/client.rs
#[test]
fn validate_prints_schema_warnings() {
    let out = Command::cargo_bin("mockingbird").unwrap()
        .arg("validate").arg(repo_path("fixtures/openapi/huge-response.json"))
        .output().unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "{stdout}");
    assert!(stdout.lines().any(|l| l.starts_with("warning: ") && l.contains("getBig")), "{stdout}");
}
```

- [ ] **Step 3: Run to verify they fail**

Run: `cargo test -p mb-import --test openapi3 compile_with_warnings && cargo test -p mockingbird --test client validate_prints_schema_warnings`
Expected: FAIL to compile (`compile_with_warnings` not found), then no `warning:` line.

- [ ] **Step 4: Implement** in `crates/mb-import/src/lib.rs`: `bundle_warnings` measures, per operation, the largest `outputs[*].schema` as `serde_json::to_string(&bundle.with_reachable_defs(schema)).len()` (the same measure `mb-engine` `compact::size` uses) and emits one `Warning` when it exceeds `PROMPT_SCHEMA_BUDGET`. Message, exactly: `format!("operation {id}: the {status} response schema is {size} bytes, over the prompt budget of {PROMPT_SCHEMA_BUDGET}; the prompt shows a trimmed schema (validation uses the full one)")` with `status` the declared code or `default`. In `crates/mb-engine/src/compact.rs` replace the `const` line with `pub use mb_import::PROMPT_SCHEMA_BUDGET;` (keep the doc comment). In `main.rs` `validate`, call `compile_with_warnings` and after the operation lines print `warning: {message}` per warning to stdout.

- [ ] **Step 5: Run to verify they pass**

Run: `cargo test -p mb-import --test openapi3 && cargo test -p mb-engine && cargo test -p mockingbird --test client validate_`
Expected: PASS

- [ ] **Step 6: Commit**

```bash
git add fixtures/openapi/huge-response.json crates/mb-import/src/lib.rs crates/mb-import/tests/openapi3.rs crates/mb-engine/src/compact.rs crates/mockingbird/src/main.rs crates/mockingbird/tests/client.rs
git commit -m "feat(import): warn when a response schema exceeds the prompt budget"
```

---

### Task 2: Store warnings on contract versions and a mode on import jobs

**Files:**
- Create: `crates/mb-store/migrations/sqlite/0650_import_drafts.sql`, `crates/mb-store/migrations/postgres/0650_import_drafts.sql`
- Modify: `crates/mb-store/src/model.rs:250-266` (`ContractVersion`), `:601-629` (`ImportJob`, `NewImportJob`)
- Modify: `crates/mb-store/src/lib.rs` (trait, next to `approve_contract_version` `:218`)
- Modify: `crates/mb-store/src/sqlite.rs` (`VERSION_COLUMNS` `:38`, `JOB_COLUMNS` `:48`, `version_from_row` `:282`, `job_from_row` `:302`, inserts `:176`, `:1499`), `crates/mb-store/src/postgres.rs` (same places, `:214`, `:257`, `:308`, `:1529`)
- Modify: `crates/mockingbird/tests/observability.rs` (`StubControl` forwards the new method), `crates/mb-admin/src/imports/mod.rs` (`NewImportJob { mode: None, .. }` until Task 10)
- Test: `crates/mb-store/tests/common/conformance.rs` (add to `run_all`)

**Interfaces:**
- Produces:
  ```rust
  // ContractVersion
  /// Compilation warnings (a JSON array of `mb_import::Warning`); None when none were recorded.
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub warnings: Option<Value>,
  // ImportJob and NewImportJob
  /// `merge` or `replace`; None = merge when the mock has a draft or active version.
  pub mode: Option<String>,
  // ControlStore
  /// Replaces the version's warnings. `NotFound` for a missing version. Publishes nothing.
  async fn set_contract_version_warnings(&self, mock_id: Uuid, version: i64, warnings: Value)
      -> Result<ContractVersion, StoreError>;
  ```
- Migration SQL: SQLite `ALTER TABLE contract_versions ADD COLUMN warnings TEXT; ALTER TABLE import_jobs ADD COLUMN mode TEXT;` — Postgres `ALTER TABLE contract_versions ADD COLUMN warnings JSON; ALTER TABLE import_jobs ADD COLUMN mode TEXT;`.

- [ ] **Step 1: Write the failing conformance tests** (and register both in `run_all`)

```rust
async fn contract_version_warnings_round_trip(store: &Arc<dyn ControlStore>) {
    let mock = create_mock(store, "warned").await; // existing helper pattern in this file
    let v = store.add_contract_version(mock.id, new_version(ContractStatus::Approved, "a")).await.unwrap();
    assert_eq!(v.warnings, None);
    let w = json!([{"code": "schema_too_large", "operation": "getBig", "message": "m", "size": 70000}]);
    let set = store.set_contract_version_warnings(mock.id, v.version, w.clone()).await.unwrap();
    assert_eq!(set.warnings, Some(w.clone()));
    let read = store.get_contract_version(mock.id, v.version).await.unwrap().unwrap();
    assert_eq!(read.warnings, Some(w.clone()));
    assert_eq!(store.list_contract_versions(mock.id).await.unwrap()[0].warnings, Some(w));
    assert!(matches!(
        store.set_contract_version_warnings(mock.id, 99, json!([])).await,
        Err(StoreError::NotFound(_))
    ));
}

async fn import_job_mode_round_trips(store: &Arc<dyn ControlStore>) {
    let mock = create_mock(store, "moded").await;
    let mut j = new_job(mock.id, b"x");
    j.mode = Some("replace".into());
    let created = store.create_import_job(j).await.unwrap();
    assert_eq!(created.mode.as_deref(), Some("replace"));
    assert_eq!(store.get_import_job(created.id).await.unwrap().unwrap().mode.as_deref(), Some("replace"));
    let claimed = store.claim_import_job(Utc::now()).await.unwrap().unwrap();
    assert_eq!(claimed.mode.as_deref(), Some("replace"));
}
```
Use whatever mock-creation helper the file already has (it creates mocks in `contract_versions_increment`); match `StoreError::NotFound`'s actual shape. `new_job` gets `mode: None`.

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test -p mb-store --test sqlite`
Expected: FAIL to compile (`set_contract_version_warnings`, `mode` unknown).

- [ ] **Step 3: Implement** the migrations, fields, columns and the method in both backends (an `UPDATE … WHERE mock_id = ? AND version = ?` then re-read; zero rows → `NotFound`), plus the `StubControl` forward and `mode: None` in `mb-admin` `create`.

- [ ] **Step 4: Run to verify they pass**

Run: `cargo test -p mb-store && scripts/test-postgres.sh` (Postgres when available) `&& cargo build --workspace --all-targets`
Expected: PASS; `upgrade.rs` still passes.

- [ ] **Step 5: Commit**

```bash
git add crates/mb-store crates/mockingbird/tests/observability.rs crates/mb-admin/src/imports/mod.rs
git commit -m "feat(store): contract-version warnings and import-job mode (migration 0650)"
```

---

### Task 3: Warnings on upload, version list, activate and drafts

**Files:**
- Modify: `crates/mb-admin/src/contracts.rs` (`UploadResult` `:66`, `ContractVersionSummary` `:77`, `From<&ContractVersion>` `:146`, `upload` `:278`)
- Modify: `crates/mb-admin/src/drafts.rs` (`put_draft` `:365`)
- Modify: `crates/mb-admin/src/imports/worker.rs` (`run_job` after `complete_import_job`, `:268`)
- Modify: `crates/mb-import/src/smart/assemble.rs` (new fn), `crates/mb-import/src/smart/mod.rs` (`run_import_with`, after `assemble`)
- Test: `crates/mb-admin/tests/api.rs`, `crates/mb-admin/tests/drafts.rs`, `crates/mb-import/tests/smart_import.rs`; update `crates/mb-admin/tests/api.rs:465` and `crates/mockingbird/tests/client.rs:285` exact-JSON expectations (add `"warnings": []`)

**Interfaces:**
- Consumes: Task 1 `bundle_warnings`, `Warning`; Task 2 `set_contract_version_warnings`, `ContractVersion.warnings`.
- Produces:
  ```rust
  // mb-admin contracts.rs
  #[derive(Serialize, ToSchema, Debug, Clone)]
  pub struct ContractWarningView { pub code: String, pub operation: Option<String>, pub message: String, pub size: Option<u64> }
  // UploadResult and ContractVersionSummary gain (always serialized, [] when none):
  pub warnings: Vec<ContractWarningView>,
  /// Compiles' warnings stored on `version`; the updated version.
  pub(crate) async fn store_warnings(st: &AdminState, mock_id: Uuid, version: i64, warnings: &[mb_import::Warning]) -> ApiResult<ContractVersion>;
  // mb-import smart/assemble.rs
  /// `schema_too_large` draft warnings (operation = target, e.g. "GET /big") for `bundle`.
  pub fn schema_warnings(bundle: &Bundle) -> Vec<DraftWarning>;
  ```
  `ContractVersionSummary::from` reads `v.warnings` (an unparseable value counts as none). Activate and approve return the summary, so they carry warnings.

- [ ] **Step 1: Write the failing tests**

```rust
// crates/mb-admin/tests/api.rs
#[tokio::test]
async fn huge_schema_warnings_on_upload_list_and_activate() {
    let h = harness().await; // existing harness of this file
    let r = h.upload("orders", "openapi/huge-response.json", None).await; // existing helper; JSON media type
    let up = r.expect(StatusCode::CREATED, "upload");
    assert_eq!(up["warnings"][0]["code"], "schema_too_large");
    assert_eq!(up["warnings"][0]["operation"], "getBig");
    let list = h.get("/api/v1/mocks/orders/contracts").await.json();
    assert_eq!(list[0]["warnings"][0]["operation"], "getBig");
    let small = h.upload("orders", "openapi/orders-3.0.yaml", None).await.json();
    assert_eq!(small["warnings"], json!([]));
    let active = h.post_empty_json("/api/v1/mocks/orders/contracts/1/activate").await.json();
    assert_eq!(active["warnings"][0]["code"], "schema_too_large");
}

// crates/mb-import/tests/smart_import.rs
#[test]
fn schema_warnings_name_the_operation_target() {
    let bundle = mb_import::compile(&fixture("openapi/huge-response.json")).unwrap();
    let ws = mb_import::smart::assemble::schema_warnings(&bundle);
    assert_eq!(ws.len(), 1);
    assert_eq!(ws[0].code, "schema_too_large");
    assert_eq!(ws[0].operation.as_deref(), Some("GET /big"));
}

// crates/mb-admin/tests/drafts.rs
#[tokio::test]
async fn put_draft_refreshes_schema_warnings() {
    let h = harness().await;
    let v = h.har_draft().await;
    let huge: Value = serde_json::from_slice(&fixture("openapi/huge-response.json")).unwrap();
    let view = h.call("editor", Method::PUT, &Harness::url(v, "/draft"), Some(json!({ "document": huge })))
        .await.expect(StatusCode::OK, "put huge");
    let codes: Vec<&str> = view["meta"]["warnings"].as_array().unwrap().iter()
        .map(|w| w["code"].as_str().unwrap()).collect();
    assert_eq!(codes.iter().filter(|c| **c == "schema_too_large").count(), 1, "{codes:?}");
    let list = h.call("viewer", Method::GET, &format!("{BASE}/pets/contracts"), None).await.json();
    assert_eq!(list[0]["warnings"][0]["operation"], "getBig");
}
```
Adapt helper names to the ones each file has (`api.rs` `Harness::upload(mock, contract, activate)` at `:148`, `post_empty_json`).

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test -p mb-admin --test api huge_schema && cargo test -p mb-admin --test drafts put_draft_refreshes && cargo test -p mb-import --features smart --test smart_import schema_warnings`
Expected: FAIL (`warnings` missing / fn not found).

- [ ] **Step 3: Implement.** `upload`: `compile_with_warnings` in the blocking task; after `add_contract_version`, `store_warnings`, then activate. `run_import_with`: append `schema_warnings(&bundle)` to `meta.warnings`. `run_job`: after a stored draft, `set_contract_version_warnings` with `bundle_warnings` (failure logs a `tracing::warn!`, never fails the job). `put_draft`: drop `schema_too_large` entries from `new_meta.warnings`, append fresh `schema_warnings(&bundle)`, update, then `store_warnings`. Then `npm run gen:api` in `crates/mb-console/web` and add `warnings: []` to console test fixtures typed as `ContractVersionSummary`/`UploadResult` (`src/test/contracts.test.tsx`, `src/test/mocks.test.tsx`) if typecheck asks.

- [ ] **Step 4: Run to verify they pass**

Run: `cargo test -p mb-admin && cargo test -p mb-import --features smart && cargo test -p mockingbird --test client && (cd crates/mb-console/web && npm run typecheck && npm run lint && npm test)`
Expected: PASS

- [ ] **Step 5: Commit**

```bash
git add crates/mb-admin crates/mb-import crates/mockingbird/tests/client.rs crates/mb-console/web
git commit -m "feat(admin): return schema-size warnings on uploads, versions and drafts"
```

---

### Task 4: Extraction prompt asks for required/nullable/ids; invented defaults dropped

**Files:**
- Modify: `crates/mb-import/src/smart/llm.rs` (`extraction_messages` `:176`, `build_operation` `:452`)
- Test: `crates/mb-import/tests/smart_llm.rs`

**Interfaces:**
- Consumes: `Chunk { text, .. }`.
- Produces: private `fn drop_invented_defaults(schema: &mut Value, source: &str) -> usize` (recursive over every object in the schema; a `default` is kept when its rendering occurs verbatim in `source`: a string default as the string itself, any other value as compact JSON, e.g. `20`, `true`, `["a"]`). `build_operation` applies it to every kept parameter, request and response schema, and pushes one `invented_default` warning per operation when anything was dropped: `format!("{target}: {n} default value(s) not stated in the documentation were dropped")`.
- Prompt copy appended to the system message, exactly (after "…for their parameters, request bodies and responses."): `Give every object schema a "required" list with the properties the documentation marks required. Make a property nullable ("type": ["<type>", "null"]) only when the documentation says it may be null. Include a "default" only when the documentation states that value. Use the documentation's own operation ids in "operation_id".`

- [ ] **Step 1: Write the failing tests**

```rust
#[tokio::test]
async fn extraction_prompt_asks_for_required_nullable_and_real_ids() {
    let provider = Arc::new(ScriptedProvider::new(vec![Ok(answer(json!([])))]));
    let _ = extract_chunk(&llm(provider.clone()), &chunk("GET /pets lists pets."), 1, "doc").await;
    let system = &provider.requests()[0].messages[0].content;
    assert!(system.contains(r#"Give every object schema a "required" list"#), "{system}");
    assert!(system.contains("only when the documentation says it may be null"), "{system}");
    assert!(system.contains(r#"Include a "default" only when the documentation states that value"#), "{system}");
    assert!(system.contains("own operation ids"), "{system}");
}

#[test]
fn defaults_not_in_the_source_are_dropped() {
    let c = chunk("GET /movies lists movies. Query `page` defaults to 1. Language is en-US by default.");
    let ops = json!([{"method": "GET", "path": "/movies", "confidence": 0.9,
        "parameters": [
            {"name": "page", "in": "query", "schema": {"type": "integer", "default": 1}},
            {"name": "region", "in": "query", "schema": {"type": "string", "default": "US-WEST"}},
            {"name": "language", "in": "query", "schema": {"type": "string", "default": "en-US"}}],
        "responses": [{"status": 200, "schema": {"type": "object",
            "properties": {"adult": {"type": "boolean", "default": false}}}}]}]);
    let (got, warnings) = parse_extraction(&answer(ops), &c).unwrap();
    let p = &got[0].parameters;
    assert_eq!(p[0].schema.as_ref().unwrap()["default"], 1);
    assert!(p[1].schema.as_ref().unwrap().get("default").is_none());
    assert_eq!(p[2].schema.as_ref().unwrap()["default"], "en-US");
    assert!(got[0].responses[0].schema.as_ref().unwrap()["properties"]["adult"].get("default").is_none());
    let w: Vec<_> = warnings.iter().filter(|w| w.code == "invented_default").collect();
    assert_eq!(w.len(), 1);
    assert!(w[0].message.starts_with("GET /movies: 2 default value(s)"), "{}", w[0].message);
}
```

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test -p mb-import --features smart --test smart_llm extraction_prompt_asks defaults_not_in`
Expected: FAIL (prompt lacks the copy; `region`/`adult` defaults kept).

- [ ] **Step 3: Implement** the prompt copy and `drop_invented_defaults` as specified.

- [ ] **Step 4: Run to verify they pass**

Run: `cargo test -p mb-import --features smart --test smart_llm`
Expected: PASS (the existing delimiter tests still pass).

- [ ] **Step 5: Commit**

```bash
git add crates/mb-import/src/smart/llm.rs crates/mb-import/tests/smart_llm.rs
git commit -m "feat(import): prompt for required/nullable/real ids; drop invented defaults"
```

---

### Task 5: Converted operation ids; required and nullable post-pass

**Files:**
- Modify: `crates/mb-import/src/smart/assemble.rs` (`assign_id` `:108`, `operation_object` `:292`, `media` `:222`)
- Create: `crates/mb-import/src/smart/complete.rs` (+ `pub mod complete;` in `smart/mod.rs`)
- Test: `crates/mb-import/tests/smart_import.rs` (update `assemble_unique_operation_ids` at `:195`: `"bad id!"` now becomes `"bad_id"`), new `crates/mb-import/tests/smart_complete.rs` (register in `crates/mb-import/Cargo.toml` with `required-features = ["smart"]`)
- Create: `fixtures/smart/draft-quality.md` (a TMDB-like page: one endpoint `GET /3/movie/{movie_id}` with operation id `movie-details`, a response example in which `tagline` is `null` in one of two examples and `homepage` appears in only one, and a property table saying `belongs_to_collection` "may be null")

**Interfaces:**
- Produces:
  ```rust
  // smart/assemble.rs (private)
  /// `id` made valid: every char outside [A-Za-z0-9_] becomes `_`, runs of `_` collapse,
  /// leading/trailing `_` are trimmed, a leading digit gets a `_` prefix, at most 64 chars;
  /// None when nothing is left. Camel case is kept.
  fn convert_id(id: &str) -> Option<String>;
  /// (assigned id, the docs' original id when it was converted)
  fn assign_id(op: &DraftOperation, used: &mut HashSet<String>) -> (String, Option<String>);
  // smart/complete.rs
  /// Adds `required` where missing and marks properties nullable, in place (see rules).
  pub fn complete_schema(schema: &mut Value, examples: &[&Value]);
  ```
  Rules of `complete_schema`, recursively (depth ≤ `MAX_SCHEMA_DEPTH`), for every schema with a `properties` object:
  1. No `required` key: if `examples` has at least one object → the sorted properties present (key exists, any value) in **every** object example; else the sorted properties whose `description` (lower-cased) contains `required` and contains neither `not required` nor `optional`; omitted when empty.
  2. A property is nullable when any object example has it as `null`, or its `description` (lower-cased) contains `nullable`, `may be null`, `can be null`, `or null` or `optional`. Making nullable: string `type` → `[type, "null"]`; array `type` → append `"null"` if absent; no `type` → `{"anyOf": [<schema>, {"type": "null"}]}` unless already null-accepting; an `enum` gets `null` appended.
  3. Recurse into each property with that key's non-null example values, and into `items` with the flattened array elements.
  `media` calls it on both given and inferred schemas with the kept examples. `operation_object` writes `"x-original-operation-id": <original>` when `assign_id` converted.

- [ ] **Step 1: Write the failing tests**

```rust
// crates/mb-import/tests/smart_complete.rs
#[test]
fn required_from_examples_and_nullable_from_nulls() {
    let mut s = json!({"type": "object", "properties": {
        "id": {"type": "integer"}, "tagline": {"type": "string"}, "homepage": {"type": "string"}}});
    let a = json!({"id": 1, "tagline": null, "homepage": "https://x"});
    let b = json!({"id": 2, "tagline": "t"});
    complete_schema(&mut s, &[&a, &b]);
    assert_eq!(s["required"], json!(["id", "tagline"]));
    assert_eq!(s["properties"]["tagline"]["type"], json!(["string", "null"]));
    assert_eq!(s["properties"]["homepage"]["type"], "string");
}

#[test]
fn without_examples_docs_decide() {
    let mut s = json!({"type": "object", "properties": {
        "name": {"type": "string", "description": "Required. The name."},
        "note": {"type": "string", "description": "Optional note"},
        "collection": {"$ref": "#/components/schemas/C", "description": "May be null."},
        "status": {"type": "string", "enum": ["a", "b"], "description": "nullable"}}});
    complete_schema(&mut s, &[]);
    assert_eq!(s["required"], json!(["name"]));
    assert_eq!(s["properties"]["note"]["type"], json!(["string", "null"]));
    assert_eq!(s["properties"]["collection"]["anyOf"][1], json!({"type": "null"}));
    assert_eq!(s["properties"]["status"]["enum"], json!(["a", "b", null]));
}

#[test]
fn existing_required_is_kept_and_items_recurse() {
    let mut s = json!({"type": "array", "items": {"type": "object", "required": ["id"],
        "properties": {"id": {"type": "integer"}, "sub": {"type": "object", "properties": {"x": {"type": "string"}}}}}});
    let e = json!([{"id": 1, "sub": {"x": null}}]);
    complete_schema(&mut s, &[&e]);
    assert_eq!(s["items"]["required"], json!(["id"]));
    assert_eq!(s["items"]["properties"]["sub"]["required"], json!(["x"]));
    assert_eq!(s["items"]["properties"]["sub"]["properties"]["x"]["type"], json!(["string", "null"]));
}

// crates/mb-import/tests/smart_import.rs
#[test]
fn docs_operation_ids_are_converted_not_replaced() {
    let ops = [with_id(op("GET", "/3/movie/{movie_id}"), "movie-details"),
               with_id(op("GET", "/3/tv"), "tv.list"), with_id(op("GET", "/x"), "getX")];
    let (doc, ids, _) = assemble_openapi("TMDB", &[], &ops);
    assert_eq!(ids, vec!["movie_details", "tv_list", "getX"]);
    let get = &doc["paths"]["/3/movie/{movie_id}"]["get"];
    assert_eq!(get["x-original-operation-id"], "movie-details");
    assert!(doc["paths"]["/x"]["get"].get("x-original-operation-id").is_none());
    compile_draft(&doc).unwrap();
}

#[tokio::test]
async fn markdown_draft_has_required_and_nullable() {
    // ScriptedProvider answers the draft-quality.md chunk with one GET /3/movie/{movie_id}
    // (operation_id "movie-details", confidence 0.9, two response examples as in the fixture,
    // and a response schema whose belongs_to_collection description says "May be null.").
    let draft = run_import(md_input(fixture("smart/draft-quality.md")), Some(&llm(provider, 4000)),
        &ImportOptions::default(), &NoopObserver).await.unwrap();
    let schema = &draft.document["paths"]["/3/movie/{movie_id}"]["get"]["responses"]["200"]
        ["content"]["application/json"]["schema"];
    assert_eq!(schema["required"], json!(["id", "tagline"]));
    assert_eq!(schema["properties"]["tagline"]["type"], json!(["string", "null"]));
    assert!(draft.meta.operations.contains_key("movie_details"));
}
```

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test -p mb-import --features smart --test smart_complete --test smart_import`
Expected: FAIL (`complete_schema` missing; ids `get_3_movie_movie_id`…).

- [ ] **Step 3: Implement** `convert_id`, the new `assign_id`, the `x-original-operation-id` write, `complete.rs`, and the `media` call; update the `"bad id!"` expectation to `"bad_id"`.

- [ ] **Step 4: Run to verify they pass**

Run: `cargo test -p mb-import --features smart`
Expected: PASS (re-accept the HAR snapshot with `cargo insta review` only if `required`/nullable changes are the sole diff).

- [ ] **Step 5: Commit**

```bash
git add crates/mb-import fixtures/smart/draft-quality.md
git commit -m "feat(import): keep docs operation ids; add required and nullable to draft schemas"
```

---

### Task 6: Drop low-confidence operations with their reason

**Files:**
- Modify: `crates/mb-import/src/smart/mod.rs` (`run_import_with`, between extraction and the `is_empty` check `:418`)
- Test: `crates/mb-import/tests/smart_import.rs`

**Interfaces:**
- Produces: `pub const MIN_CONFIDENCE: f32 = 0.4;` and private `fn drop_low_confidence(ops: Vec<DraftOperation>, warnings: &mut Vec<DraftWarning>) -> Vec<DraftOperation>`. Each dropped op gets one warning, code `low_confidence`, `operation` = target, message exactly `format!("{target} was left out: confidence {c:.2} is below 0.40 ({reason})")`, where `reason` is the messages of the existing warnings for that target joined with `"; "`, or `the documentation gives no response shape` when its responses have no schema or example, else `the model was unsure`. When every operation is dropped the import fails with `SmartError::NoOperations` quoting these warnings (existing path).

- [ ] **Step 1: Write the failing test**

```rust
#[tokio::test]
async fn low_confidence_operations_are_dropped_with_a_reason() {
    let answer = json!({"operations": [
        {"method": "GET", "path": "/pets", "confidence": 0.9, "evidence": "Returns all pets, newest first.",
         "responses": [{"status": 200, "example": [{"id": 1}]}]},
        {"method": "DELETE", "path": "/pets/{petId}", "confidence": 0.5},
        {"method": "GET", "path": "/owners", "confidence": 0.7,
         "responses": [{"status": 200, "example": []}]}]}).to_string();
    let provider = Arc::new(ScriptedProvider::new(vec![Ok(answer)]));
    let draft = run_import(md_input(fixture("smart/petstore.md")), Some(&llm(provider, 1000)),
        &ImportOptions::default(), &NoopObserver).await.unwrap();
    assert_eq!(path_operations(&draft.document), 1);
    let low: Vec<_> = draft.meta.warnings.iter().filter(|w| w.code == "low_confidence").collect();
    assert_eq!(low.len(), 2, "{:?}", draft.meta.warnings);
    let delete = low.iter().find(|w| w.operation.as_deref() == Some("DELETE /pets/{petId}")).unwrap();
    assert!(delete.message.contains("0.30 is below 0.40 (the documentation gives no response shape)"), "{}", delete.message);
    let owners = low.iter().find(|w| w.operation.as_deref() == Some("GET /owners")).unwrap();
    // `/owners` is not in petstore.md: 0.7 is halved to 0.35 with an `ungrounded_operation` warning.
    assert!(owners.message.contains("0.35 is below 0.40 (GET /owners is not grounded"), "{}", owners.message);
}
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test -p mb-import --features smart --test smart_import low_confidence`
Expected: FAIL (3 operations, no `low_confidence`).

- [ ] **Step 3: Implement** `MIN_CONFIDENCE` and `drop_low_confidence` (applied to every source kind after `merge_operations`).

- [ ] **Step 4: Run to verify it passes**

Run: `cargo test -p mb-import --features smart && cargo test -p mb-admin --test imports`
Expected: PASS

- [ ] **Step 5: Commit**

```bash
git add crates/mb-import/src/smart/mod.rs crates/mb-import/tests/smart_import.rs
git commit -m "feat(import): leave out operations below 0.4 confidence, with the reason"
```

---

### Task 7: Operations from embedded OpenAPI blocks

**Files:**
- Create: `crates/mb-import/src/smart/embedded.rs` (+ `pub mod embedded;`)
- Modify: `crates/mb-import/src/smart/mod.rs` (`Extracted` `:275`, `extract_document` `:285`, `assemble` `:489`)
- Create: `fixtures/smart/readme-oas.html`, `fixtures/smart/embedded-oas.md`, `fixtures/smart/embedded-broken.md`
- Test: new `crates/mb-import/tests/smart_embedded.rs` (register in `Cargo.toml`, `required-features = ["smart"]`)

**Interfaces:**
- Consumes: `text::{TextDoc, Block, BlockKind, is_endpoint_line}`, `crate::parse_document`, `paths::{normalize_path, path_key}`, `assemble::compile_draft`.
- Produces:
  ```rust
  pub struct EmbeddedOperation {
      pub method: String,        // upper case
      pub path: String,          // the block's path template
      pub operation: Value,      // the OpenAPI operation object, path-level parameters merged in
      pub excerpt: String,       // ≤ 300 chars of the block, for provenance
  }
  #[derive(Default)]
  pub struct Embedded {
      pub operations: Vec<EmbeddedOperation>,
      /// `components` of the blocks merged (first definition of a name wins).
      pub components: Map<String, Value>,
      pub servers: Vec<String>,
      pub warnings: Vec<DraftWarning>,
  }
  /// Finds OpenAPI 3.x documents in `bytes` (HTML scripts) and in `doc`'s code blocks;
  /// the code blocks used are removed from `doc` so the LLM never sees them.
  pub fn extract_embedded(kind: ImportKind, bytes: &[u8], doc: &mut TextDoc) -> Embedded;
  /// Whether every endpoint line left in `doc` names an embedded operation.
  pub fn covers_endpoints(embedded: &Embedded, doc: &TextDoc) -> bool;
  /// Inserts the operations into `document` (`paths`, `components`), ids via the
  /// same conversion as `assign_id`; returns each operation's final id, in order.
  pub fn insert_embedded(document: &mut Value, embedded: &Embedded, used: &mut HashSet<String>) -> Vec<String>;
  ```
- Detection markers (pinned; the implementer verifies them against a saved, trimmed TMDB ReadMe page `fixtures/smart/tmdb-readme.html`, ≤ 200 KB, saved by hand from `developer.themoviedb.org/reference/movie-details`; if the live markup differs, adapt the selectors and the synthetic fixture to it, and record the markers in the module doc):
  1. HTML: text of `script[type="application/json"]`, `script#__NEXT_DATA__`, `script#ssr-props`, and the value of any `data-initial-props` attribute, each parsed as JSON.
  2. Markdown/HTML code blocks (`BlockKind::Code`) that parse with `crate::parse_document` (JSON or YAML).
  3. In every parsed value, walk depth-first (depth ≤ 64, ≤ 200 000 nodes): an object with a string `openapi` starting `3.` and an object `paths` is a document; a string longer than 50 chars starting with `{` and containing `"openapi"` is parsed and walked (ReadMe stringifies `oasDefinition`). Keys such as `oasDefinition`, `oas`, `apiDefinition` need no special case beyond this walk. Duplicate documents (same `paths` keys) are collapsed.
  4. An object with `swagger: "2.0"` and `paths` is not converted: one `embedded_swagger2` warning (`a Swagger 2.0 block was found; its operations are read from the text instead`), and its code block stays in the text.
- Pipeline rules: block operations get `OperationMeta { confidence: 1.0, provenance: [Provenance { chunk: None, location: "embedded OpenAPI".into(), excerpt }], edited: false }`; an LLM operation with the same `path_key` as a block operation is dropped (block wins); when `covers_endpoints` holds and at least one block operation exists, no chunk is sent to the LLM (`chunk_count` 0) and the import succeeds without an LLM; `servers` from the blocks are used when the source gave none; a block operation the compiler implicates is removed with an `invalid_operation` warning, like LLM operations today.

- [ ] **Step 1: Create the fixtures**
  - `readme-oas.html`: a ReadMe-like page: `<h1>Details</h1>`, prose with the endpoint line `GET https://api.themoviedb.org/3/movie/{movie_id}`, and `<script id="ssr-props" type="application/json">{"props":{"oasDefinition": <OpenAPI 3.0.3 with servers https://api.themoviedb.org, GET /3/movie/{movie_id} (movie-details, 200 schema `$ref` #/components/schemas/Movie) and GET /3/movie/{movie_id}/credits (movie-credits)>}}</script>`.
  - `embedded-oas.md`: prose for `GET /pets` (no block) and a fenced ```yaml block with `openapi: 3.0.0` defining `POST /pets` (`createPet`, 201 response schema).
  - `embedded-broken.md`: a fenced ```json block `openapi 3.0.0` whose `GET /a` response `$ref`s `#/components/schemas/Missing` plus a valid `GET /b`, and a second fenced ```yaml block that does not parse (`openapi: [3`).

- [ ] **Step 2: Write the failing tests**

```rust
#[tokio::test]
async fn readme_page_imports_every_embedded_operation_without_llm() {
    let input = ImportInput::Bytes { kind: ImportKind::Html, name: Some("details.html".into()),
        bytes: fixture("smart/readme-oas.html") };
    let draft = run_import(input, None, &ImportOptions::default(), &NoopObserver).await.unwrap();
    assert_eq!(draft.meta.chunk_count, 0);
    let ids: Vec<&str> = draft.meta.operations.keys().map(String::as_str).collect();
    assert_eq!(ids, vec!["movie_credits", "movie_details"]);
    assert_eq!(draft.meta.operations["movie_details"].confidence, 1.0);
    assert_eq!(draft.meta.operations["movie_details"].provenance[0].location, "embedded OpenAPI");
    assert_eq!(draft.document["servers"][0]["url"], "https://api.themoviedb.org");
    assert!(draft.document["components"]["schemas"]["Movie"].is_object());
}

#[tokio::test]
async fn mixed_page_block_wins_and_llm_fills_the_rest() {
    // The LLM answers GET /pets and a (worse) POST /pets.
    let provider = Arc::new(ScriptedProvider::new(vec![Ok(json!({"operations": [
        {"method": "GET", "path": "/pets", "confidence": 0.9, "responses": [{"status": 200, "example": []}]},
        {"method": "POST", "path": "/pets", "confidence": 0.9, "responses": [{"status": 200}]}]}).to_string())]));
    let draft = run_import(md("embedded-oas.md"), Some(&llm(provider.clone())), &ImportOptions::default(), &NoopObserver)
        .await.unwrap();
    let user = &provider.requests()[0].messages[1].content;
    assert!(!user.contains("openapi: 3.0.0"), "the block reached the prompt");
    let post = &draft.document["paths"]["/pets"]["post"];
    assert_eq!(post["operationId"], "createPet");
    assert!(post["responses"]["201"].is_object());
    assert!(draft.document["paths"]["/pets"]["get"].is_object());
}

#[tokio::test]
async fn broken_embedded_block_is_skipped_not_fatal() {
    let provider = Arc::new(ScriptedProvider::new(vec![Ok(r#"{"operations": []}"#.into())]));
    let draft = run_import(md("embedded-broken.md"), Some(&llm(provider)), &ImportOptions::default(), &NoopObserver)
        .await.unwrap();
    assert!(draft.document["paths"]["/b"]["get"].is_object());
    assert!(draft.document["paths"].get("/a").is_none());
    assert!(draft.meta.warnings.iter().any(|w| w.code == "invalid_operation"
        && w.operation.as_deref() == Some("GET /a")));
}

#[test]
fn swagger2_block_is_left_to_the_llm() {
    let md = "# API\n\n```json\n{\"swagger\": \"2.0\", \"info\": {\"title\": \"t\", \"version\": \"1\"}, \"paths\": {\"/x\": {\"get\": {\"responses\": {\"200\": {\"description\": \"ok\"}}}}}}\n```\n";
    let mut doc = mb_import::smart::text::extract_markdown(md);
    let e = extract_embedded(ImportKind::Markdown, md.as_bytes(), &mut doc);
    assert!(e.operations.is_empty());
    assert_eq!(e.warnings[0].code, "embedded_swagger2");
    assert!(doc.blocks.iter().any(|b| b.text.contains("swagger")));
}
```
(`md(name)` builds an `ImportInput::Bytes` Markdown input from `fixtures/smart/<name>`; `llm` as in `smart_import.rs`.)

- [ ] **Step 3: Run to verify they fail**

Run: `cargo test -p mb-import --features smart --test smart_embedded`
Expected: FAIL to compile (`embedded` module missing).

- [ ] **Step 4: Implement** `embedded.rs` and wire it: `extract_document` calls `extract_embedded` on the `TextDoc` before chunking, skips chunking when covered (and then no longer requires an LLM: move the `LlmRequired` check to just before the first chunk), filters LLM ops by `path_key`; `assemble` calls `insert_embedded` after `assemble_openapi` and extends the meta.

- [ ] **Step 5: Run to verify they pass**

Run: `cargo test -p mb-import --features smart`
Expected: PASS (existing `doc_import_without_llm_fails` still fails for `petstore.md`, which has no block).

- [ ] **Step 6: Commit**

```bash
git add crates/mb-import fixtures/smart
git commit -m "feat(import): take operations from OpenAPI blocks embedded in docs pages"
```

---

### Task 8: One draft from several sources

**Files:**
- Modify: `crates/mb-import/src/smart/mod.rs` (`run_import`, `run_import_with` `:356`), `crates/mb-import/src/smart/model.rs` (`DraftMeta` `:197`)
- Test: `crates/mb-import/tests/smart_import.rs`

**Interfaces:**
- Produces:
  ```rust
  /// Most sources of one import.
  pub const MAX_IMPORT_SOURCES: usize = 100;
  /// Every source extracted under one token budget, operations merged (`merge_operations`),
  /// one draft assembled. A source that cannot be fetched or read is skipped with a
  /// `source_skipped` warning (`format!("{name} was skipped: {error}")`); when all fail,
  /// the first error is returned. `LlmRequired` and `Canceled` end the import at once.
  pub async fn run_import_sources(inputs: Vec<ImportInput>, llm: Option<&ExtractLlm>, opts: &ImportOptions,
      observer: &dyn ImportObserver, resolver: &dyn Resolver) -> Result<DraftResult, SmartError>;
  // run_import_with(input, ..) == run_import_sources(vec![input], ..); run_import unchanged.
  // DraftMeta gains:
  /// Names (file names or redacted URLs) of every source, when there was more than one.
  #[serde(default, skip_serializing_if = "Vec::is_empty")] pub sources: Vec<String>,
  /// The version this draft was merged into (Task 9).
  #[serde(default, skip_serializing_if = "Option::is_none")] pub merged_from: Option<i64>,
  ```
  Title, `source_kind`, `source_name`, `source_url` come from the first source that produced operations; `chunk_count`/`chunks_processed` are sums. More than `MAX_IMPORT_SOURCES` inputs → `SmartError::Unsupported("an import takes at most 100 sources")`.

- [ ] **Step 1: Write the failing test**

```rust
#[tokio::test]
async fn two_sources_make_one_draft() {
    let server = MockServer::start().await;
    Mock::given(method("GET")).and(path("/pets.md"))
        .respond_with(ResponseTemplate::new(200).insert_header("content-type", "text/markdown")
            .set_body_bytes(fixture("smart/petstore.md"))).mount(&server).await;
    let har = ImportInput::Bytes { kind: ImportKind::Har, name: Some("petstore.har".into()),
        bytes: fixture("smart/petstore.har") };
    let url = ImportInput::Url(format!("{}/pets.md", server.uri()));
    let missing = ImportInput::Url(format!("{}/missing.md", server.uri()));
    let provider = Arc::new(ScriptedProvider::new(vec![Ok(pets_answer())]));
    let draft = mb_import::smart::run_import_sources(vec![har, url, missing], Some(&llm(provider, 1000)),
        &loopback_options(), &NoopObserver, &mb_import::smart::fetch::SystemResolver).await.unwrap();
    assert_eq!(draft.meta.sources.len(), 3);
    assert_eq!(draft.meta.sources[0], "petstore.har");
    assert!(draft.document["paths"]["/api/pets/{petId}"]["get"].is_object()); // from the HAR
    assert!(draft.document["paths"]["/pets/{petId}"]["get"].is_object());     // from the docs
    assert!(draft.meta.warnings.iter().any(|w| w.code == "source_skipped" && w.message.contains("missing.md")));
}
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test -p mb-import --features smart --test smart_import two_sources`
Expected: FAIL to compile (`run_import_sources` missing).

- [ ] **Step 3: Implement** by splitting today's `run_import_with` body into a per-source `extract_source(input, …) -> Result<(SourceInfo, Extracted), SmartError>` and a shared tail (merge, `drop_low_confidence`, assemble, meta). Update `meta_for` in `smart_import.rs:580` and `refresh_meta` for the new fields.

- [ ] **Step 4: Run to verify it passes**

Run: `cargo test -p mb-import --features smart && cargo test -p mb-admin`
Expected: PASS

- [ ] **Step 5: Commit**

```bash
git add crates/mb-import
git commit -m "feat(import): build one draft from several sources"
```

---

### Task 9: Merge a draft into an existing contract version

**Files:**
- Modify: `crates/mb-import/src/smart/merge.rs` (new items after `merge_operations`)
- Test: `crates/mb-import/tests/smart_import.rs`

**Interfaces:**
- Consumes: `crate::{parse_document, detect_format, SourceFormat}`, `assemble::{compile_draft, schema_warnings}`, `paths::path_key`, `DraftResult`, `DraftMeta`.
- Produces:
  ```rust
  /// The contract version an import merges into.
  pub struct MergeBase { pub version: i64, pub source: Vec<u8>, pub meta: Option<DraftMeta> }
  /// `base`'s document with `incoming`'s operations added; an operation with the same
  /// method and path (by `path_key`) is replaced, nothing is removed.
  pub fn merge_drafts(base: &MergeBase, incoming: DraftResult) -> Result<DraftResult, SmartError>;
  ```
  Rules: `base.source` must parse and `detect_format` as `OpenApi3`, else `SmartError::Unsupported(format!("contract version {v} is not an OpenAPI 3 document, so the import cannot merge into it; import with mode replace"))`. Base `info` and `servers` kept, incoming servers appended when new. `components` sections merged per name, base wins; a differing same-name incoming component gives a `conflicting_component` warning. An incoming `operationId` equal to a kept base operation's id is suffixed `_2`, `_3`… and gets `x-original-operation-id` (unless it has one). Meta: base operations keep `base.meta`'s entry (else `confidence 1.0`, no provenance, `edited false`); incoming ones keep theirs under their final id; warnings = base warnings whose `operation` is not replaced (minus `schema_too_large`) + incoming warnings (minus `schema_too_large`) + `schema_warnings` of the merged bundle; `merged_from = Some(base.version)`; `sources`, `usage`, chunk counts from incoming. The result is compiled with `compile_draft` (`SmartError::Compile` on failure).

- [ ] **Step 1: Write the failing tests**

```rust
/// `run_import` of fixtures/smart/petstore.har with no LLM.
async fn har_draft() -> DraftResult
/// `run_import` of fixtures/smart/legacy-v2.0.postman_collection.json with no LLM.
async fn postman_draft() -> DraftResult

#[tokio::test]
async fn merge_adds_and_replaces_without_removing() {
    let base = har_draft().await;
    let base_ids: Vec<String> = base.meta.operations.keys().cloned().collect();
    let mb = MergeBase { version: 3, source: base.source.clone(), meta: Some(base.meta.clone()) };
    let merged = merge_drafts(&mb, postman_draft().await).unwrap();
    for id in &base_ids { assert!(merged.meta.operations.contains_key(id), "{id} lost"); }
    assert!(merged.document["paths"]["/v1/orders/{orderId}"]["get"].is_object());
    assert_eq!(merged.meta.merged_from, Some(3));
}

#[tokio::test]
async fn merging_a_draft_into_itself_is_idempotent() {
    let d = har_draft().await;
    let mb = MergeBase { version: 1, source: d.source.clone(), meta: Some(d.meta.clone()) };
    let merged = merge_drafts(&mb, har_draft().await).unwrap();
    assert_eq!(merged.meta.operations.keys().collect::<Vec<_>>(), d.meta.operations.keys().collect::<Vec<_>>());
    assert_eq!(merged.bundle.operations.len(), d.bundle.operations.len());
}

#[tokio::test]
async fn merge_into_yaml_openapi_works_and_graphql_is_refused() {
    let yaml = MergeBase { version: 1, source: fixture("openapi/orders-3.0.yaml"), meta: None };
    let merged = merge_drafts(&yaml, har_draft().await).unwrap();
    let orders_ops = mb_import::compile(&fixture("openapi/orders-3.0.yaml")).unwrap().operations.len();
    assert!(merged.bundle.operations.len() > orders_ops);
    assert!(merged.meta.operations.values().any(|m| m.confidence == 1.0 && m.provenance.is_empty()));
    let sdl = MergeBase { version: 2, source: b"type Query { a: Int }".to_vec(), meta: None };
    let err = merge_drafts(&sdl, har_draft().await).unwrap_err();
    assert!(err.to_string().contains("import with mode replace"), "{err}");
}

#[tokio::test]
async fn merge_renames_a_colliding_operation_id() {
    // base: GET /a with operationId "list"; incoming: GET /b with operationId "list".
    let (doc, _, _) = assemble_openapi("T", &[], &[with_id(op("GET", "/a"), "list")]);
    let (source, _) = compile_draft(&doc).unwrap();
    let (doc_b, _, _) = assemble_openapi("T", &[], &[with_id(op("GET", "/b"), "list")]);
    let (source_b, bundle_b) = compile_draft(&doc_b).unwrap();
    let incoming = DraftResult { document: doc_b, source: source_b, bundle: bundle_b, meta: meta_for(&["list"]) };
    let merged = merge_drafts(&MergeBase { version: 1, source, meta: None }, incoming).unwrap();
    assert_eq!(merged.document["paths"]["/b"]["get"]["operationId"], "list_2");
    assert_eq!(merged.document["paths"]["/b"]["get"]["x-original-operation-id"], "list");
}
```

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test -p mb-import --features smart --test smart_import merge_`
Expected: FAIL to compile (`merge_drafts` missing).

- [ ] **Step 3: Implement** `MergeBase` and `merge_drafts` per the rules (CPU-bound; callers run it on the blocking pool).

- [ ] **Step 4: Run to verify they pass**

Run: `cargo test -p mb-import --features smart`
Expected: PASS

- [ ] **Step 5: Commit**

```bash
git add crates/mb-import/src/smart/merge.rs crates/mb-import/tests/smart_import.rs
git commit -m "feat(import): merge an import into the existing contract"
```

---

### Task 10: Admin API `urls`, `documents` and `mode`; the worker merges

**Files:**
- Modify: `crates/mb-admin/src/imports/mod.rs` (`ImportKindSchema` `:97`, `CreateImport` `:172`, `ImportJobView` `:198`, `json_job` `:304`, `create` `:451`)
- Modify: `crates/mb-admin/src/imports/worker.rs` (`run_job` `:204`)
- Test: `crates/mb-admin/tests/imports.rs`

**Interfaces:**
- Consumes: Task 2 `NewImportJob.mode`/`ImportJob.mode`, Task 8 `run_import_sources`, Task 9 `MergeBase`, `merge_drafts`.
- Produces (wire shapes WS5 calls):
  ```rust
  #[derive(Serialize, Deserialize, ToSchema, Clone, Copy, Debug, PartialEq, Eq)]
  #[serde(rename_all = "snake_case")]
  pub enum ImportModeSchema { Merge, Replace }
  /// One text document of a `documents` import.
  #[derive(Deserialize, ToSchema, Debug)]
  pub struct ImportDocument { pub filename: Option<String>, pub kind: Option<ImportKindSchema>, pub text: String }
  // CreateImport gains (exactly one of url, urls, text, content_base64, documents):
  /// 1–20 http(s) URLs imported into one draft.
  pub urls: Option<Vec<String>>,
  /// 1–100 Markdown, HTML or text documents (kind from `kind` or the file name), at most 20 MiB in all.
  pub documents: Option<Vec<ImportDocument>>,
  /// `merge` (add new operations, replace same method+path, remove nothing) or `replace`.
  /// Absent: merge when the mock has a draft or an active version.
  pub mode: Option<ImportModeSchema>,
  // ImportKindSchema gains `Urls` ("urls") and `Documents` ("documents"); ImportJobView gains
  pub mode: Option<ImportModeSchema>,
  pub const MAX_IMPORT_URLS: usize = 20;
  ```
  Job storage: kind `urls`, input = JSON array of the validated (unredacted) URLs; `source_name` = first redacted URL plus `format!(" (+{n} more)")`. Kind `documents`, input = JSON array of `{"kind","name","text"}`; `source_name` = `format!("{n} documents")`. `mode` stored as `"merge"`/`"replace"`/NULL. Errors (400 problem+json): more than one of the five inputs; empty or over-long `urls`/`documents`; an invalid URL (`validate_url` message, prefixed `urls[i]: `); a document kind that is not markdown/html/text or undetectable. A `documents` import without an LLM is 422 like a text import.
  Worker: build `Vec<ImportInput>` from the job kind; `run_import_sources`; then mode `replace` → store as today; else find the base = newest version (from `list_contract_versions`) whose status is `Draft` or `Active`; none → store as today; `merge_drafts` on the blocking pool: `Unsupported` with mode NULL → store the unmerged draft with a `merge_skipped` warning (`format!("not merged into version {v}: {reason}")`); with mode `merge` → fail the job with that message.

- [ ] **Step 1: Write the failing tests**

```rust
#[tokio::test]
async fn second_import_merges_into_the_first_draft() {
    let h = harness().await;
    let id = h.upload_har("pets").await["id"].as_str().unwrap().to_string();
    assert!(h.run().await);
    let first = h.job("pets", &id).await["result_version"].as_i64().unwrap();
    let postman = String::from_utf8(fixture("smart/legacy-v2.0.postman_collection.json")).unwrap();
    let view = h.call("editor", Method::POST, &format!("{BASE}/pets/imports"),
        Some(json!({ "kind": "postman", "text": postman }))).await.expect(StatusCode::ACCEPTED, "postman");
    assert_eq!(view["mode"], Value::Null);
    assert!(h.run().await);
    let v = h.job("pets", view["id"].as_str().unwrap()).await["result_version"].as_i64().unwrap();
    let draft = h.call("viewer", Method::GET, &format!("{BASE}/pets/contracts/{v}/draft"), None).await.json();
    let targets: Vec<String> = draft["operations"].as_array().unwrap().iter()
        .map(|o| format!("{} {}", o["method"].as_str().unwrap(), o["path"].as_str().unwrap())).collect();
    assert!(targets.contains(&"GET /api/pets/{petId}".to_string()), "{targets:?}");
    assert!(targets.iter().any(|t| t.starts_with("GET /v1/orders")), "{targets:?}");
    assert_eq!(draft["meta"]["merged_from"], first);
}

#[tokio::test]
async fn replace_mode_does_not_merge() {
    // as above with "mode": "replace": the second draft has no /api/pets operations and no merged_from.
}

#[tokio::test]
async fn default_merge_over_graphql_falls_back_to_replace() {
    let h = harness().await;
    // upload fixtures/graphql/starwars.graphql to …/pets/contracts as application/graphql (becomes active)
    // then a HAR import with no mode
    // → job done; draft meta.warnings has merge_skipped; draft has only HAR operations.
    // with "mode": "merge" → job failed, error contains "import with mode replace".
}

#[tokio::test]
async fn urls_and_documents_validation() {
    let h = harness_with(Some(scripted(vec![]))).await;
    let uri = format!("{BASE}/pets/imports");
    let bad = [
        json!({ "urls": [] }),
        json!({ "urls": ["ftp://x"] }),
        json!({ "urls": ["https://a.example/x"], "url": "https://b.example/y" }),
        json!({ "documents": [{ "filename": "a.pdf", "text": "x" }] }),
        json!({ "urls": (0..21).map(|i| format!("https://a.example/{i}")).collect::<Vec<_>>() }),
    ];
    for body in bad {
        h.call("editor", Method::POST, &uri, Some(body.clone())).await.expect(StatusCode::BAD_REQUEST, &body.to_string());
    }
    let ok = h.call("editor", Method::POST, &uri, Some(json!({ "urls": ["https://a.example/x?sig=s3cret", "https://b.example/y"], "mode": "replace" })))
        .await.expect(StatusCode::ACCEPTED, "urls");
    assert_eq!(ok["kind"], "urls");
    assert_eq!(ok["mode"], "replace");
    assert_eq!(ok["source_name"], "https://a.example/x?sig=REDACTED (+1 more)");
    let docs = h.call("editor", Method::POST, &uri, Some(json!({ "documents": [
        { "filename": "a.md", "text": md_text() }, { "filename": "b.html", "text": "<h1>B</h1>" }] })))
        .await.expect(StatusCode::ACCEPTED, "documents");
    assert_eq!(docs["kind"], "documents");
    assert_eq!(docs["source_name"], "2 documents");
}

#[tokio::test]
async fn documents_import_reads_each_file() {
    // scripted LLM answers pets_answer() for a.md and {"operations": []} for b.md;
    // job done; draft meta.sources == ["a.md", "b.md"]; three operations.
}
```
Fill the bodies of the sketched tests with the same helpers (`upload_har`, `run`, `job`, `call`); the assertions in the comments are the required ones.

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test -p mb-admin --test imports`
Expected: FAIL (unknown fields rejected / no merge).

- [ ] **Step 3: Implement** the request handling, job encoding and worker flow above. Then `npm run gen:api` in `crates/mb-console/web` (no UI change; the console still sends `url`/`text`/multipart).

- [ ] **Step 4: Run to verify they pass**

Run: `cargo test -p mb-admin && (cd crates/mb-console/web && npm run typecheck && npm run lint && npm test)`
Expected: PASS

- [ ] **Step 5: Commit**

```bash
git add crates/mb-admin crates/mb-console/web/src/api/schema.d.ts crates/mb-console/web/openapi.json
git commit -m "feat(admin): imports take urls, documents and mode; merge into the existing contract"
```

---

### Task 11: Approve activates by default

**Files:**
- Modify: `crates/mb-admin/src/drafts.rs` (`approve` `:426`)
- Modify: `crates/mb-console/web/src/api/imports.ts:136-148` (`useApproveDraft` sends `{ activate: false }`)
- Test: `crates/mb-admin/tests/drafts.rs` (new test; update `approve_then_activate` `:438` to send `{"activate": false}`), `crates/mockingbird/tests/smart_import.rs:176-186` (send `{"activate": false}`), `crates/mb-console/web/src/test/draft.test.tsx:355` (assert the body)

**Interfaces:**
- Produces:
  ```rust
  /// What to do on approval.
  #[derive(Deserialize, ToSchema, Debug, Default)]
  pub struct ApproveRequest {
      /// Also activate the approved version (default true).
      pub activate: Option<bool>,
  }
  pub async fn approve(State(st): State<AdminState>, access: WsAccess, Path(VersionPath { mock, version }): Path<VersionPath>,
      _json: RequireJson, body: axum::body::Bytes) -> ApiResult<Json<ContractVersionSummary>>;
  ```
  `POST …/contracts/{version}/approve`, `Content-Type: application/json` (still required), body empty, `{}` or `{"activate": bool}`; malformed JSON → 400. `activate` absent or true: approve then `activate_contract_version`; 200 summary with `status: "active"` (and `warnings`); audits `contract.approve` then `contract.activate`. `false`: today's behaviour, `status: "approved"`. utoipa: `request_body = ApproveRequest` (optional), response description `Approved and, unless activate is false, activated`.

- [ ] **Step 1: Write the failing test**

```rust
#[tokio::test]
async fn approve_activates_by_default() {
    let h = harness().await;
    let v = h.har_draft().await;
    let r = h.call("editor", Method::POST, &Harness::url(v, "/approve"), Some(json!({})))
        .await.expect(StatusCode::OK, "approve");
    assert_eq!(r["status"], "active");
    let actions = h.actions().await;
    assert!(actions.contains(&"contract.approve".into()) && actions.contains(&"contract.activate".into()), "{actions:?}");
    let v2 = h.har_draft().await;
    let r = h.call("editor", Method::POST, &Harness::url(v2, "/approve"), Some(json!({ "activate": false })))
        .await.expect(StatusCode::OK, "approve only");
    assert_eq!(r["status"], "approved");
    let empty = Request::post(Harness::url(h.har_draft().await, "/approve"))
        .header(header::AUTHORIZATION, "Bearer editor").header(header::CONTENT_TYPE, "application/json")
        .body(Body::empty()).unwrap();
    assert_eq!(h.send(empty).await.json()["status"], "active");
    h.call("editor", Method::POST, &Harness::url(v2, "/approve"), Some(json!({ "activate": "yes" })))
        .await.expect(StatusCode::BAD_REQUEST, "bad body");
}
```
(`har_draft` may be called repeatedly; each call adds a new draft version.)

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test -p mb-admin --test drafts approve_activates_by_default`
Expected: FAIL (`status` is `approved`).

- [ ] **Step 3: Implement** `ApproveRequest` and the new `approve`; update the three existing tests and `useApproveDraft` (`body: { activate: false }`); `npm run gen:api`; assert in `draft.test.tsx` that the approve call's body is `{"activate":false}`.

- [ ] **Step 4: Run to verify it passes**

Run: `cargo test -p mb-admin --test drafts && cargo test -p mockingbird --test smart_import && (cd crates/mb-console/web && npm run typecheck && npm run lint && npm test)`
Expected: PASS

- [ ] **Step 5: Commit**

```bash
git add crates/mb-admin crates/mockingbird/tests/smart_import.rs crates/mb-console/web
git commit -m "feat(admin): approve activates the draft unless activate is false"
```

---

### Task 12: CLI `import`: `--no-activate`, `--mode`, several URLs, docs folders

**Files:**
- Modify: `crates/mockingbird/src/client.rs` (`ImportArgs` `:159-180`, `ACTIVATE_NEEDS_APPROVE` `:306`, `import` `:484`, `smart_import` `:582-680`, new helpers next to `document_body` `:516`)
- Modify: `crates/mockingbird/src/mcp.rs:1134` (fill the new fields: `more_urls: Vec::new(), no_activate: false, mode: None`; nothing else)
- Test: `crates/mockingbird/tests/client.rs`

**Interfaces:**
- Consumes: Task 10 request shapes, Task 11 `ApproveRequest`.
- Produces:
  ```rust
  #[derive(clap::ValueEnum, Clone, Copy, Debug, PartialEq, Eq)]
  pub enum ImportMode { Merge, Replace }
  // ImportArgs gains:
  /// More http(s) URLs imported into the same draft.
  #[arg(value_name = "URL")] pub more_urls: Vec<String>,
  /// With --approve: approve without activating.
  #[arg(long, conflicts_with = "activate")] pub no_activate: bool,
  /// Merge into the mock's draft or active contract, or replace it [default: merge when one exists].
  #[arg(long, value_enum)] pub mode: Option<ImportMode>,
  /// The documents of a folder holding only .md/.markdown/.html/.htm/.txt files (recursive,
  /// hidden entries skipped, sorted by relative path); None when it holds anything else or nothing.
  fn docs_folder(dir: &Path) -> anyhow::Result<Option<Vec<Value>>>; // [{"filename","kind","text"}]
  ```
  Behaviour: several sources must all be URLs (`--as` mock) → `{"urls": [...]}`, else usage error (exit 2) `several sources must all be http(s) URLs`. A docs folder → `{"documents": [...]}`; any other folder uploads as an archive (today). `--mode` → `"mode"` in the body. `--approve` posts `{"activate": !no_activate}` to `…/approve` and no longer calls `…/activate`; `--activate` stays accepted (redundant with `--approve`, still refused without it). Help text of `--activate` and `--approve` updated accordingly.

- [ ] **Step 1: Write the failing tests**

```rust
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn import_approve_activates_unless_no_activate() {
    let admin = admin_url().await;
    let c = smart_fixture("legacy-v2.0.postman_collection.json");
    let out = run(&admin, &["import", c.to_str().unwrap(), "--as", "a", "--approve"]).await;
    assert_eq!(out.code, 0, "stderr: {}", out.stderr);
    assert_eq!(out.json()["status"], "active");
    let out = run(&admin, &["import", c.to_str().unwrap(), "--as", "b", "--approve", "--no-activate", "--mode", "replace"]).await;
    assert_eq!(out.code, 0, "stderr: {}", out.stderr);
    assert_eq!(out.json()["status"], "approved");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn import_docs_folder_goes_to_imports() {
    let admin = admin_url().await;
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.md"), "# A\n\nGET /a returns a.").unwrap();
    std::fs::create_dir(dir.path().join("sub")).unwrap();
    std::fs::write(dir.path().join("sub/b.html"), "<h1>B</h1>").unwrap();
    let out = run(&admin, &["import", dir.path().to_str().unwrap(), "--as", "docs"]).await;
    assert_eq!(out.code, 1, "stdout: {}", out.stdout);
    assert!(out.stderr.contains("LLM profile"), "stderr: {}", out.stderr); // offline: documents need an LLM
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn several_sources_must_be_urls() {
    let admin = admin_url().await;
    let har = smart_fixture("petstore.har");
    let out = run(&admin, &["import", har.to_str().unwrap(), "https://a.example/x", "--as", "m"]).await;
    assert_eq!(out.code, 2, "stderr: {}", out.stderr);
    assert!(out.stderr.contains("several sources must all be http(s) URLs"), "{}", out.stderr);
}
```

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test -p mockingbird --test client import_approve_activates import_docs_folder several_sources`
Expected: FAIL (unknown flags; folder uploaded as archive).

- [ ] **Step 3: Implement** the args, `docs_folder`, the body building and the approve call; fill the new fields in `mcp.rs`.

- [ ] **Step 4: Run to verify they pass**

Run: `cargo test -p mockingbird --test client && cargo test -p mockingbird --test agent_toolkit`
Expected: PASS (the existing `import_postman_approve_activate` and `import_activate_without_approve_rejected` still pass).

- [ ] **Step 5: Commit**

```bash
git add crates/mockingbird/src/client.rs crates/mockingbird/src/mcp.rs crates/mockingbird/tests/client.rs
git commit -m "feat(cli): import --no-activate, --mode, several URLs and docs folders"
```

---

### Task 13: Documentation, gates and pull request

**Files:**
- Modify: `README.md` (Smart import section `:500-600`: merge/replace, `urls`/`documents`, approve activates, embedded OpenAPI, low-confidence and new warning codes; Admin API table rows `:430-431`, `:559-563`; CLI examples `:590-593`), `CHANGELOG.md` (`## [Unreleased]`)

- [ ] **Step 1: Write the docs.** README: the request fields and defaults exactly as in Task 10/11 Interfaces; the merge caveat (two imports running at once each merge into the same base; the later one wins). CHANGELOG `### Added`: embedded OpenAPI blocks; `urls`/`documents`/`mode` with merge by default; schema-size warnings on versions and `validate`; migration `0650_import_drafts` (`contract_versions.warnings`, `import_jobs.mode`). `### Changed`: approve activates unless `activate: false` (CLI `--no-activate`); drafts keep docs operation ids (`x-original-operation-id`), get `required`/nullable, drop undocumented defaults and operations below 0.4 confidence.

- [ ] **Step 2: Run every gate**

```bash
cargo fmt --all --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-features
cargo check -p mockingbird --no-default-features
(cd crates/mb-console/web && npm run check:api && npm run typecheck && npm run lint && npm test)
scripts/test-postgres.sh   # when Docker/Postgres is available
```
Expected: all pass, except the known pre-existing `tls::llm_client_trusts_outbound_ca` (macOS) and the flaky `graphql::rejections_are_recorded` (re-run once).

- [ ] **Step 3: Commit, push, open the PR (do not merge)**

```bash
git add README.md CHANGELOG.md
git commit -m "docs: smart import merging, embedded OpenAPI, approve activates, schema warnings

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>"
git push -u origin feat/ws3-import-drafts
gh pr create --base main --title "WS3: import and drafts" --body "<summary of 3.1–3.4, the Admin API shapes for WS5, test plan>

🤖 Generated with [Claude Code](https://claude.com/claude-code)"
```
Every commit message in this plan ends with the `Co-Authored-By` line.

---

## Produces for WS5 (MCP tools)

All under `/api/v1/workspaces/{ws}/mocks/{mock}`, `application/json`, errors `application/problem+json`:

- `POST …/imports` — `CreateImport` with exactly one of `url`, `urls` (1–20), `text` (+`kind`), `content_base64`, `documents` (1–100 of `{filename?, kind?, text}`), plus optional `filename`, `kind`, `mode: "merge" | "replace"` (absent = merge when a draft or active version exists). 202 → `ImportJobView` (now with `kind` `urls`/`documents` and `mode`). Poll `GET …/imports/{job}` until `status: done`, read `result_version`.
- `GET …/contracts/{version}/draft` — `DraftView`; `meta` gains `sources: [string]` and `merged_from: int | null`; `meta.warnings[]` `{code, message, operation?}` with `operation` = `"GET /path"` (join with `operations[].method/path` for per-operation warnings; codes include `low_confidence`, `invented_default`, `schema_too_large`, `merge_skipped`, `source_skipped`, `embedded_swagger2`).
- `PUT …/contracts/{version}/draft` — unchanged (`{document}`), so `edit_draft` applies its JSON merge patch to `document` and PUTs it.
- `POST …/contracts/{version}/approve` — body `{}` or `{"activate": bool}` (default true) → `ContractVersionSummary` with `status: "active"` (or `"approved"`).
- `ContractVersionSummary` (list, activate, approve) and `UploadResult` gain `warnings: [{code, operation, message, size}]` (operation = operation id).
- Library: `mb_import::compile_with_warnings(&[u8]) -> Result<(Bundle, Vec<mb_import::Warning>), ImportError>` for `validate_contract`.
