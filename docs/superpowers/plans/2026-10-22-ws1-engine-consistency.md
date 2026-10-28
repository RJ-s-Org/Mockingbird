# WS1: Engine Consistency and Faker Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Mocks answer consistently across calls (stored GETs, a per-session response cache, list→detail seeding, unique ids, opt-in 404 for unknown ids), the faker produces client-safe values, more LLM errors are patched locally, and scenarios apply without request headers.

**Architecture:** The engine (`mb-engine`) gains new `Source` variants, a stored-answer check, list/identity helpers, a pattern- and name-aware faker and a broader local patcher. The per-call pipeline (`mb-protocols/src/call.rs`) decides, before generating, whether state, the cache or a 404 answers the call, and after generating seeds list items and caches the answer. The response cache lives in `mb-state` as a `StateStore` decorator (`CachingStateStore`) so resets, writes and session idleness clear it through the same calls that change session state; seeded items are stored with a new quiet `StateStore::insert_seeded`. Mock settings (`consistency`, `unknown_ids`, `default_scenario`) and per-operation scenario instructions flow file config → control store → Admin API → `MockRuntime`.

**Tech Stack:** Rust 1.97 workspace, `rand` 0.10.3, `rand_regex` **0.19.0** (depends on `rand ^0.10` and `regex-syntax ^0.8.9`; the lock has `regex-syntax 0.8.11`, so no duplicate), `regex-syntax` 0.8 (direct, for the ASCII-first parse), `jsonschema` 0.58, sqlx (SQLite/Postgres), axum, utoipa; console (Node 22) only for `npm run gen:api` and one label map.

**Spec:** `docs/superpowers/specs/2026-10-22-improvements-backlog-design.md` (section WS1, 1.1–1.9; Decisions table).

## Global Constraints

- Branch `feat/ws1-engine-consistency`, in its own worktree, cut from `main` (rebase on `main` once the spec PR is merged). One PR; it merges only with the user's OK.
- Consistency is **on by default**; `consistency: off` per mock disables the response cache and list→detail seeding. Stored GETs from state are **always on**.
- Unknown ids: opt-in per mock with `unknown_ids: 404`; default `generate` keeps generating.
- Response cache: per session, LRU, **500 entries** per session (`DEFAULT_CACHE_ENTRIES = 500`), keyed by operation id + canonical parameters (path params, sorted query params; for GraphQL the root field, its arguments and a hash of the narrowed selection schema); holds status, content type, body; cleared on any write to the same resource type in the session and on session reset; bypassed by `consistency: off`, scenario calls and forced-status calls.
- Faker: integers with no tight bounds draw from `0..=100`; bounds wider than ±2^31 are ignored; nullable arrays are never `null`; at the depth/node limit an array is `[]`; strings default to at most **64** chars; `pattern` via `rand_regex` with max **64** extra repeats per unbounded `*`/`+`/`{n,}`; add formats `ipv4`, `ipv6`, `hostname`, `uri-reference`; echo path/query/GraphQL-argument values into same-named fields via `FakerHints { echoes: Map<String, Value> }`.
- Runaway strings: system prompt adds "keep free-text strings under 200 characters unless the schema allows more"; a string with no declared `maxLength` longer than **2 000** chars is truncated to **200** (char boundary) and the answer is `Repaired`.
- Local patching covers `null` type errors plus `pattern`, `maxLength`, `minLength`, `enum`, `format`, `const`; patch only when every error is patchable, otherwise ask for a repair.
- `default_scenario: <name>` applies when no `X-Mockingbird-Scenario` header is sent; scenario `operations` entries may carry per-operation `instructions` that replace the scenario-wide ones for that operation.
- No `unwrap()`/`expect()` outside tests; Admin API errors are `application/problem+json`; secrets are references only; never print or write API keys (trials use `env:OPENROUTER_API_KEY`).
- A new serde field on a stored type is `#[serde(default, skip_serializing_if = …)]` so stored JSON and snapshots stay byte-identical.
- `mb-engine`, `mb-store`, `mb-state` stay free of axum; `mb-protocols` depends on neither `mb-store` nor `mb-auth`.
- New config keys go in `docs/configuration.md` **and** `docs/examples/mockingbird.full.yaml` (checked by `crates/mockingbird/tests/docs_reference.rs::every_config_key_is_documented`), in the WS1 part of those files.
- After the Admin API changes run `npm run gen:api` in `crates/mb-console/web` and commit `src/api/schema.d.ts`.
- Commits: `type(scope): summary`, ending with `Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>`.
- File ownership: do not edit `crates/mb-protocols/src/graphql/*`, `chaos.rs`, `http_rest/handler.rs` (WS2), `crates/mb-admin/src/requests.rs` (WS5) or the prefix checks in `mb-admin/src/mocks.rs`/`reconcile.rs` (WS4). Expected textual overlap with WS4/WS6 in `file_config.rs`, `docs/configuration.md`, `docs/examples/mockingbird.full.yaml`, and with WS4 in every `NewMock { … }` literal (WS4 adds `hosts`).
- WS6 (wave 2) adds a post-write hook in `call.rs` after `write_state`; keep `write_state` returning the `StateWrite`s described in Task 8 and do not inline it.

## Review Focus

1. **A partial resource stored by a create is read back** (the create body lacked required response fields): the stored GET must not serve an invalid body; it falls through to generate-and-overlay. Test: `stored_partial_resource_is_generated_and_overlaid` (Task 2).
2. **Seeding overwrites a richer created resource** with a list summary: seeding must keep anything already stored. Test: `insert_seeded_keeps_an_existing_resource` in the conformance suite (Task 5) and `listing_keeps_a_created_resource` (Task 8).
3. **Paginated lists evict each other** (page 2 seeding clears the cached page 1): seeded writes must not invalidate the cache. Test: `seeded_inserts_do_not_invalidate` (Task 6).
4. **`unknown_ids: 404` with a forced status or on a list/collection path**: forced status wins; operations without a path identity are never 404'd. Test: `unknown_ids_404_spares_lists_and_forced_status` (Task 3).
5. **A pattern the generator cannot honour** (lookaround, backreference, or a draw longer than `maxLength`): the faker must fall back, never panic on non-UTF-8 output. Test: `unsupported_or_too_long_patterns_fall_back` (Task 10).

---

### Task 1: `Source::State`/`Source::Cache` and the stored-answer check

**Files:**
- Modify: `crates/mb-engine/src/lib.rs:86-118` (`Source`), add `Engine::serve_stored` and `Engine::not_found` after `Engine::fallback` (`:450-480`)
- Modify: `crates/mb-evals/src/report.rs:276-280` (exhaustive `match source`)
- Modify: `crates/mb-console/web/src/lib/sources.ts` (`SOURCES`), `crates/mb-console/web/src/components/http.tsx:30` (`SOURCE_TONE`)
- Test: `crates/mb-engine/tests/generate.rs`

**Interfaces:**
- Consumes: nothing new.
- Produces:
  - `pub enum Source { Llm, Repaired, Fallback, State, Cache }`; `as_str`: `"state"`, `"cache"`; both rank `0` in `Source::worst`.
  - `impl Engine { pub fn serve_stored(&self, bundle: &Bundle, op: &Operation, stored: &Value, response_schema: Option<&Value>) -> Option<GeneratedResponse> }` — `Some` (status = success status, variant content type, body = `stored.clone()`, `Source::State`, zero usage, no fallback reason) only when the success variant has a schema (the narrowed one when given) and `stored` validates against it.
  - `impl Engine { pub fn not_found(&self, bundle: &Bundle, op: &Operation, call: &OperationCall, detail: &str) -> GeneratedResponse }` — the declared `404` variant faker-filled through `Engine::fallback` (forced status 404), else `content_type: Some("application/problem+json")`, body `{"type":"about:blank","title":"Not Found","status":404,"detail":detail}`; always `source: Source::State`.

- [ ] **Step 1: Write the failing tests** (append to `crates/mb-engine/tests/generate.rs`, using its `orders()`, `valid_order_at_o1()`, `offline()`, `inline()`, `call()` helpers)

```rust
#[test]
fn stored_resource_that_validates_is_served_as_state() {
    let bundle = orders();
    let op = bundle.operation("getOrder").unwrap();
    let out = offline().serve_stored(&bundle, op, &valid_order_at_o1(), None).unwrap();
    assert_eq!(out.status, 200);
    assert_eq!(out.source, Source::State);
    assert_eq!(out.source.to_string(), "state");
    assert_eq!(out.body, Some(valid_order_at_o1()));
    assert_eq!(Source::Cache.as_str(), "cache");
    assert_eq!(Source::State.worst(Source::Llm), Source::State);
    assert_eq!(Source::Cache.worst(Source::Fallback), Source::Fallback);
}

#[test]
fn partial_stored_resource_is_not_served() {
    let bundle = orders();
    let op = bundle.operation("getOrder").unwrap();
    let partial = json!({"id": "o1", "customerId": "c1"});
    assert!(offline().serve_stored(&bundle, op, &partial, None).is_none());
}

#[test]
fn not_found_uses_the_declared_404_else_problem_json() {
    let bundle = orders();
    let op = bundle.operation("getOrder").unwrap();
    let out = offline().not_found(&bundle, op, &call("getOrder"), "orders o1 does not exist");
    assert_eq!(out.status, 404);
    assert_eq!(out.source, Source::State);
    let body = out.body.unwrap();
    assert!(body["code"].is_i64() && body["message"].is_string(), "{body}");

    let bundle = inline(r#"
openapi: 3.1.0
info: {title: Pets, version: "1"}
paths:
  /pets/{petId}:
    get:
      operationId: getPet
      parameters: [{name: petId, in: path, required: true, schema: {type: string}}]
      responses:
        "200": {description: OK, content: {application/json: {schema: {type: object, properties: {id: {type: string}}}}}}
"#);
    let op = bundle.operation("getPet").unwrap();
    let out = offline().not_found(&bundle, op, &OperationCall::default(), "pets p1 does not exist");
    assert_eq!(out.content_type.as_deref(), Some("application/problem+json"));
    assert_eq!(out.body.unwrap()["detail"], "pets p1 does not exist");
}
```

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test -p mb-engine --test generate stored_resource partial_stored not_found_uses`
Expected: FAIL to compile (`no variant named State`, `no method named serve_stored`).

- [ ] **Step 3: Implement** the two variants (`rank` 0), `serve_stored` (reuse `self.validator` for the operation schema; `SchemaValidator::new(&bundle.with_reachable_defs(s))` for a narrowed one, as `generate` does at `lib.rs:294-297`) and `not_found`. Add `Source::State | Source::Cache => {}` to `mb-evals/src/report.rs` (the eval runner calls `Engine::generate`, which never returns them). In the console add `state: { label: "Session state", hint: "Served from the session's stored resource, without the LLM." }`, `cache: { label: "Cache", hint: "A repeat of an earlier answer in the same session." }` and tones `state: "info", cache: "neutral"`.

- [ ] **Step 4: Run to verify they pass**

Run: `cargo test -p mb-engine --test generate && cargo test -p mb-evals` then `cd crates/mb-console/web && npm run typecheck && npm test`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/mb-engine crates/mb-evals crates/mb-console/web/src/lib/sources.ts crates/mb-console/web/src/components/http.tsx
git commit -m "feat(engine): state and cache sources, stored-answer and not-found helpers"
```

---

### Task 2: Stored GETs from session state (1.1) and the runtime settings

**Files:**
- Modify: `crates/mb-protocols/src/lib.rs:202-310` (`MockRuntime` fields + `MockRuntime::new` defaults), add `UnknownIds`
- Modify: `crates/mb-protocols/src/call.rs:621-665` (`generate_narrowed`)
- Test: `crates/mb-protocols/tests/http_rest.rs`

**Interfaces:**
- Consumes: `Engine::serve_stored` (Task 1).
- Produces:
  - `#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)] pub enum UnknownIds { #[default] Generate, NotFound }` (re-exported from `mb_protocols`).
  - `MockRuntime` public fields: `pub consistency: bool` (default `true`), `pub unknown_ids: UnknownIds` (default `Generate`), `pub default_scenario: Option<String>` (default `None`).
  - `call.rs`: `fn plain(plan: &CallPlan<'_>) -> bool` — no scenario applies and no status is forced (chaos errors force one).

- [ ] **Step 1: Write the failing tests.** In `crates/mb-protocols/tests/http_rest.rs` first refactor the harness so tests can configure the runtime and the store:

```rust
fn harness_with(
    script: Option<Vec<Result<String, LlmError>>>,
    state: Arc<dyn StateStore>,
    configure: impl FnOnce(&mut MockRuntime),
) -> Harness { /* body of today's `harness`, with `state`, then `configure(&mut rt)` before `Arc::new` */ }

fn harness(script: Option<Vec<Result<String, LlmError>>>, header_overrides: bool) -> Harness {
    harness_with(script, memory_state(), |rt| rt.header_overrides = header_overrides)
}

fn memory_state() -> Arc<dyn StateStore> {
    Arc::new(MemoryStore::new(Duration::from_secs(3600)))
}
```

then add:

```rust
#[tokio::test]
async fn get_after_create_is_served_from_state() {
    let h = harness(Some(vec![Ok(order("o1", "c1").to_string())]), true);
    send(&h, post_json("/orders", r#"{"customerId":"c1","total":10}"#)).await;
    let resp = send(&h, get("/orders/o1")).await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(header_str(&resp, "x-mockingbird-source"), "state");
    assert_eq!(body_json(resp).await, order("o1", "c1"));
    assert_eq!(h.provider.as_ref().unwrap().requests().len(), 1, "no LLM call for the GET");
}

#[tokio::test]
async fn stored_partial_resource_is_generated_and_overlaid() {
    let h = offline();
    h.rt.state
        .upsert(&session("default"), StoredResource {
            resource_type: "orders".into(),
            id: "o1".into(),
            data: json!({"id": "o1", "customerId": "c9"}),
        })
        .await
        .unwrap();
    let resp = send(&h, get("/orders/o1")).await;
    assert_eq!(header_str(&resp, "x-mockingbird-source"), "fallback");
    let body = body_json(resp).await;
    assert_eq!(body["customerId"], "c9");
    assert!(body["total"].is_number(), "{body}");
}

#[tokio::test]
async fn scenario_and_forced_status_skip_the_stored_answer() {
    let h = offline();
    h.rt.state
        .upsert(&session("default"), StoredResource {
            resource_type: "orders".into(),
            id: "o1".into(),
            data: order("o1", "c1"),
        })
        .await
        .unwrap();
    let forced = Request::get("/orders/o1")
        .header("x-mockingbird-status", "404")
        .body(Body::empty())
        .unwrap();
    let resp = send(&h, forced).await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    assert_ne!(header_str(&resp, "x-mockingbird-source"), "state");
}
```

(`StateStore` joins the `mb_state` import.)

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test -p mb-protocols --test http_rest get_after_create stored_partial scenario_and_forced`
Expected: FAIL — `get_after_create_is_served_from_state` sees source `llm` (the script is exhausted, so the GET falls back: `fallback`).

- [ ] **Step 3: Implement.** Add the `MockRuntime` fields and `UnknownIds`. In `generate_narrowed`, after `read_context`: when `op.kind == OperationKind::Query`, `plain(plan)` and a target was read, return `rt.engine.serve_stored(&rt.bundle, op, &target.data, response_schema)` when it is `Some` (touch the session as the existing tail does; write nothing). Otherwise continue unchanged.

- [ ] **Step 4: Run to verify they pass, then the crate**

Run: `cargo test -p mb-protocols --all-features`
Expected: PASS. Existing tests that GET a resource they stored and assert `llm`/`fallback` now see `state`; update only those source assertions (behaviour, not intent, changed).

- [ ] **Step 5: Commit**

```bash
git add crates/mb-protocols
git commit -m "feat(protocols): serve stored resources to reads without the LLM"
```

---

### Task 3: Unknown ids → 404, opt-in (1.5)

**Files:**
- Modify: `crates/mb-protocols/src/call.rs` (`generate_narrowed`)
- Test: `crates/mb-protocols/tests/http_rest.rs`

**Interfaces:**
- Consumes: `UnknownIds`, `MockRuntime::unknown_ids` (Task 2); `Engine::not_found` (Task 1).
- Produces: behaviour only. A call is 404'd when `rt.unknown_ids == UnknownIds::NotFound`, `op.kind == OperationKind::Query`, `plan.forced_status.is_none()`, `op.identity` has a `path_param`, `target_id` is `Some` and no target was read. Detail text: `"{resource_type} {id} does not exist in session {session}"`. Nothing is written or cached.

- [ ] **Step 1: Write the failing tests**

```rust
#[tokio::test]
async fn unknown_ids_404_when_opted_in() {
    let h = harness_with(None, memory_state(), |rt| rt.unknown_ids = UnknownIds::NotFound);
    let resp = send(&h, get("/orders/nope")).await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    assert_eq!(header_str(&resp, "x-mockingbird-source"), "state");
    let body = body_json(resp).await;
    assert!(body["code"].is_i64() && body["message"].is_string(), "{body}");

    let created = body_json(send(&h, post_json("/orders", r#"{"customerId":"c1","total":10}"#)).await).await;
    let id = created["id"].as_str().unwrap();
    assert_eq!(send(&h, get(&format!("/orders/{id}"))).await.status(), StatusCode::OK);
}

#[tokio::test]
async fn unknown_ids_404_spares_lists_and_forced_status() {
    let h = harness_with(None, memory_state(), |rt| rt.unknown_ids = UnknownIds::NotFound);
    assert_eq!(send(&h, get("/orders")).await.status(), StatusCode::OK);
    let forced = Request::get("/orders/nope")
        .header("x-mockingbird-status", "200")
        .body(Body::empty())
        .unwrap();
    assert_eq!(send(&h, forced).await.status(), StatusCode::OK);
    // Default: keep generating.
    assert_eq!(send(&offline(), get("/orders/nope")).await.status(), StatusCode::OK);
}

#[tokio::test]
async fn unknown_id_without_declared_404_is_problem_json() {
    let src = r#"
openapi: 3.1.0
info: {title: Pets, version: "1"}
paths:
  /pets/{petId}:
    get:
      operationId: getPet
      parameters: [{name: petId, in: path, required: true, schema: {type: string}}]
      responses:
        "200": {description: OK, content: {application/json: {schema: {type: object, properties: {id: {type: string}}}}}}
"#;
    let bundle = Arc::new(compile(src.as_bytes()).unwrap());
    let mut rt = MockRuntime::new("inline", bundle, Arc::new(Engine::new(None, EngineConfig::default())), memory_state());
    rt.unknown_ids = UnknownIds::NotFound;
    let router = http_rest::router(Arc::new(rt));
    let resp = router.oneshot(get("/pets/p1")).await.unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    assert_eq!(header_str(&resp, "content-type"), "application/problem+json");
}
```

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test -p mb-protocols --test http_rest unknown_id`
Expected: FAIL (`200 != 404`).

- [ ] **Step 3: Implement** the check in `generate_narrowed`, before the stored-answer check of Task 2, returning `rt.engine.not_found(...)`.

- [ ] **Step 4: Run to verify they pass**

Run: `cargo test -p mb-protocols --test http_rest`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/mb-protocols
git commit -m "feat(protocols): opt-in 404 for ids the session does not hold"
```

---

### Task 4: Default scenario and per-operation instructions at runtime (1.9, data plane)

**Files:**
- Modify: `crates/mb-protocols/src/lib.rs:125-145` (`ScenarioRuntime`)
- Modify: `crates/mb-protocols/src/call.rs:394-406` (scenario lookup in `plan`)
- Test: `crates/mb-protocols/tests/runtime.rs`

**Interfaces:**
- Consumes: `MockRuntime::default_scenario` (Task 2).
- Produces:
  - `ScenarioRuntime` field `pub per_operation: HashMap<String, ScenarioRuntime>` (default empty).
  - `impl ScenarioRuntime { pub fn with_operation_instructions(self, instructions: BTreeMap<String, String>) -> Self }` — for each entry, a clone of `self` whose `spec.instructions` is that text, stored in `per_operation`.
  - `impl ScenarioRuntime { pub fn for_operation(&self, operation_id: &str) -> &ScenarioRuntime }` — the per-operation variant, else `self`.
  - `plan`: scenario name = header, else `rt.default_scenario`; an unknown header name stays `PlanError::UnknownScenario`; an unknown default is ignored (`tracing::debug!`). The applying scenario is `s.for_operation(&op.id)`, so every adapter (including GraphQL's `plan.scenario.map(|s| s.spec.clone())`) sees the per-operation instructions without call-site changes.

- [ ] **Step 1: Write the failing tests** (in `crates/mb-protocols/tests/runtime.rs`, using `runtime`, `memory`, `offline`, `shipped`, `get_with`, `body_json`; add `use std::collections::BTreeMap;`)

```rust
#[tokio::test]
async fn default_scenario_applies_without_a_header() {
    let mut rt = offline();
    rt.scenarios.insert("shipped".into(), shipped(&[]));
    rt.default_scenario = Some("shipped".into());
    let router = http_rest::router(Arc::new(rt));
    let resp = send(&router, get_with("/orders/o1", &[])).await;
    assert_eq!(body_json(resp).await["status"], "shipped");
}

#[tokio::test]
async fn unknown_default_scenario_is_ignored() {
    let mut rt = offline();
    rt.default_scenario = Some("missing".into());
    let router = http_rest::router(Arc::new(rt));
    assert_eq!(send(&router, get_with("/orders/o1", &[])).await.status(), StatusCode::OK);
}

#[tokio::test]
async fn per_operation_instructions_replace_the_scenario_ones() {
    let provider = Arc::new(ScriptedProvider::new(vec![Ok(order("o1").to_string())]));
    let mut rt = runtime(Some(provider.clone() as Arc<dyn Provider>), memory());
    let instructions = BTreeMap::from([("getOrder".to_string(), "This order is a gift.".to_string())]);
    rt.scenarios.insert("shipped".into(), shipped(&[]).with_operation_instructions(instructions));
    let router = http_rest::router(Arc::new(rt));
    send(&router, get_with("/orders/o1", &[("x-mockingbird-scenario", "shipped")])).await;
    let prompt: String = provider.requests()[0].messages.iter().map(|m| m.content.as_str()).collect();
    assert!(prompt.contains("This order is a gift."), "{prompt}");
    assert!(!prompt.contains("Every order has already shipped."), "{prompt}");
}
```

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test -p mb-protocols --test runtime default_scenario per_operation unknown_default`
Expected: FAIL to compile (`no method with_operation_instructions`), then `"pending" != "shipped"`.

- [ ] **Step 3: Implement** the field, the two methods and the `plan` change.

- [ ] **Step 4: Run to verify they pass**

Run: `cargo test -p mb-protocols --all-features`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/mb-protocols
git commit -m "feat(protocols): default scenario and per-operation scenario instructions"
```

---

### Task 5: Quiet, insert-if-absent seeding in every state store

**Files:**
- Modify: `crates/mb-state/src/lib.rs:95-140` (`StateStore`)
- Modify: `crates/mb-state/src/memory.rs:162-306`, `crates/mb-state/src/sql.rs:289-590`, `crates/mb-state/src/redis.rs:262-540`
- Modify: `crates/mockingbird/src/ha.rs:274-330` (`BusStateStore` delegates)
- Test: `crates/mb-state/tests/common/conformance.rs` (runs for memory, SQLite, and the gated Postgres/Redis suites)

**Interfaces:**
- Produces: provided trait method
  `async fn insert_seeded(&self, s: &SessionKey, r: StoredResource) -> Result<bool, StateError>` — stores `r` only when the session holds no resource with its type and id, creating/refreshing the session like `upsert`; publishes **no** change event; `Ok(true)` when stored. The default (`get`, then `upsert`) is for test doubles and does publish; `MemoryStore`, `SqlStateStore` (`INSERT … ON CONFLICT DO NOTHING`, session upsert as in `upsert`, no `publish`), `RedisStateStore` (`HSETNX`, then `ZADD` + `refresh` only when set) and `BusStateStore` (delegate) override it.

- [ ] **Step 1: Write the failing test** (add to `run_all_in` and the file)

```rust
async fn insert_seeded_keeps_an_existing_resource(st: &Arc<dyn StateStore>, ws: &str) {
    let s = key(ws, "seeding", "a");
    let mut changes = st.subscribe();
    assert!(st.insert_seeded(&s, res("order", "1", json!({"v": 1}))).await.expect("seed"));
    assert_eq!(st.get(&s, "order", "1").await.expect("get").map(|r| r.data), Some(json!({"v": 1})));
    assert!(!st.insert_seeded(&s, res("order", "1", json!({"v": 2}))).await.expect("seed again"));
    assert_eq!(st.get(&s, "order", "1").await.expect("get").map(|r| r.data), Some(json!({"v": 1})));
    assert!(
        matches!(changes.try_recv(), Err(broadcast::error::TryRecvError::Empty)),
        "seeding publishes nothing"
    );
    assert_eq!(st.sessions(ws, "seeding").await.expect("sessions").len(), 1);
}
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test -p mb-state --test memory memory_conformance && cargo test -p mb-state --test sql`
Expected: FAIL to compile (`no method insert_seeded`).

- [ ] **Step 3: Implement** the provided method and the four overrides.

- [ ] **Step 4: Run to verify it passes**

Run: `cargo test -p mb-state && scripts/test-postgres.sh && scripts/test-redis.sh` (the scripts when `initdb`/`redis-server` are available; otherwise note it in the PR), `cargo test -p mockingbird --test ha`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/mb-state crates/mockingbird/src/ha.rs
git commit -m "feat(state): quiet insert-if-absent for seeded resources"
```

---

### Task 6: Per-session response cache in `mb-state` (1.2, storage)

**Files:**
- Create: `crates/mb-state/src/cache.rs`
- Modify: `crates/mb-state/src/lib.rs` (`pub mod cache;` re-exports; provided `StateStore::response_cache`)
- Modify: `crates/mockingbird/src/config.rs:525` (wrap the shared state store)
- Test: create `crates/mb-state/tests/cache.rs`

**Interfaces:**
- Produces:
  - `pub const DEFAULT_CACHE_ENTRIES: usize = 500;`
  - `#[derive(Clone, Debug, PartialEq)] pub struct CachedResponse { pub status: u16, pub content_type: Option<String>, pub body: Option<Value> }`
  - `pub struct ResponseCache` — `pub fn new(capacity: usize, idle_ttl: Duration) -> Self`; `pub fn get(&self, s: &SessionKey, key: &str) -> Option<CachedResponse>` (bumps recency; a session idle for `idle_ttl` is dropped and misses); `pub fn put(&self, s: &SessionKey, key: String, resource_type: Option<&str>, value: CachedResponse)` (evicts the least recently used entry over `capacity`); `pub fn invalidate_type(&self, s: &SessionKey, resource_type: &str)`; `pub fn clear_session(&self, s: &SessionKey)`; `pub fn clear_mock(&self, workspace: &str, mock: &str)`; `pub fn len(&self, s: &SessionKey) -> usize`. Storage: `DashMap<SessionKey, SessionCache>` with a `u64` recency counter per entry (O(n) eviction at n = 500 is fine; no new crate); times from `tokio::time::Instant` (pausable in tests).
  - Provided `fn response_cache(&self) -> Option<&ResponseCache> { None }` on `StateStore`.
  - `pub struct CachingStateStore` — `pub fn wrap(inner: Arc<dyn StateStore>, idle_ttl: Duration, capacity: usize) -> Arc<dyn StateStore>`. Delegates every method; `upsert`/`delete` call `invalidate_type`; `reset`/`seed` call `clear_session`; `reset_mock` calls `clear_mock`; `insert_seeded` does **not** invalidate; `response_cache` returns `Some`. When a Tokio runtime is present it spawns a task over `inner.subscribe()` that calls `invalidate_type` for every change event (covers changes relayed from other nodes by `BusStateStore`) and sweeps idle sessions every `SWEEP_INTERVAL`, holding only a `Weak`.
  - `config.rs`: `let state = CachingStateStore::wrap(ha.shared_state(state), cfg.storage.session_ttl, DEFAULT_CACHE_ENTRIES);` — the same `Arc` reaches the registry and `AdminState`, so an Admin session reset clears the cache. Known limit (document in `docs/architecture.md`'s state section): a reset on another HA node does not clear this node's cache; idleness and the next write of the type do.

- [ ] **Step 1: Write the failing tests** (`crates/mb-state/tests/cache.rs`)

```rust
#![allow(clippy::unwrap_used)]
use std::sync::Arc;
use std::time::Duration;
use mb_state::{CachedResponse, CachingStateStore, MemoryStore, SessionKey, StateStore, StoredResource};
use serde_json::json;

fn key(session: &str) -> SessionKey {
    SessionKey { workspace: "default".into(), mock: "orders".into(), session: session.into() }
}
fn answer(n: i64) -> CachedResponse {
    CachedResponse { status: 200, content_type: None, body: Some(json!({"n": n})) }
}
fn order(id: &str) -> StoredResource {
    StoredResource { resource_type: "orders".into(), id: id.into(), data: json!({"id": id}) }
}
fn store() -> Arc<dyn StateStore> {
    CachingStateStore::wrap(Arc::new(MemoryStore::new(Duration::from_secs(3600))), Duration::from_secs(3600), 2)
}

#[tokio::test]
async fn least_recently_used_entry_is_evicted() {
    let st = store();
    let c = st.response_cache().unwrap();
    c.put(&key("a"), "k1".into(), Some("orders"), answer(1));
    c.put(&key("a"), "k2".into(), Some("orders"), answer(2));
    assert_eq!(c.get(&key("a"), "k1"), Some(answer(1)));
    c.put(&key("a"), "k3".into(), Some("orders"), answer(3));
    assert_eq!(c.get(&key("a"), "k2"), None);
    assert_eq!(c.len(&key("a")), 2);
    assert_eq!(c.get(&key("b"), "k1"), None, "sessions are separate");
}

#[tokio::test]
async fn writes_of_the_type_and_resets_invalidate() {
    let st = store();
    let c = st.response_cache().unwrap();
    c.put(&key("a"), "list".into(), Some("orders"), answer(1));
    c.put(&key("a"), "other".into(), Some("customers"), answer(2));
    st.upsert(&key("a"), order("o1")).await.unwrap();
    assert_eq!(c.get(&key("a"), "list"), None);
    assert_eq!(c.get(&key("a"), "other"), Some(answer(2)));
    st.reset(&key("a")).await.unwrap();
    assert_eq!(c.get(&key("a"), "other"), None);
}

#[tokio::test]
async fn seeded_inserts_do_not_invalidate() {
    let st = store();
    let c = st.response_cache().unwrap();
    c.put(&key("a"), "page1".into(), Some("orders"), answer(1));
    assert!(st.insert_seeded(&key("a"), order("o9")).await.unwrap());
    assert_eq!(c.get(&key("a"), "page1"), Some(answer(1)));
}

#[tokio::test]
async fn changes_made_below_the_wrapper_invalidate() {
    let inner: Arc<dyn StateStore> = Arc::new(MemoryStore::new(Duration::from_secs(3600)));
    let st = CachingStateStore::wrap(Arc::clone(&inner), Duration::from_secs(3600), 500);
    let c = st.response_cache().unwrap();
    c.put(&key("a"), "list".into(), Some("orders"), answer(1));
    inner.upsert(&key("a"), order("o1")).await.unwrap();
    tokio::time::timeout(Duration::from_secs(1), async {
        while c.get(&key("a"), "list").is_some() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
}

#[tokio::test(start_paused = true)]
async fn idle_sessions_expire() {
    let st = CachingStateStore::wrap(Arc::new(MemoryStore::new(Duration::from_secs(60))), Duration::from_secs(60), 500);
    let c = st.response_cache().unwrap();
    c.put(&key("a"), "k".into(), None, answer(1));
    tokio::time::advance(Duration::from_secs(61)).await;
    assert_eq!(c.get(&key("a"), "k"), None);
}
```

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test -p mb-state --test cache`
Expected: FAIL to compile (`unresolved import mb_state::CachedResponse`).

- [ ] **Step 3: Implement** `cache.rs`, the provided trait method and the `config.rs` wrap.

- [ ] **Step 4: Run to verify they pass**

Run: `cargo test -p mb-state && cargo test -p mockingbird --test e2e --test ha`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/mb-state crates/mockingbird/src/config.rs docs/architecture.md
git commit -m "feat(state): per-session LRU response cache cleared by writes and resets"
```

---

### Task 7: List helpers, unique list ids and fresh create ids in the engine (1.4)

**Files:**
- Create: `crates/mb-engine/src/consistency.rs` (`pub mod consistency;` in `lib.rs`)
- Modify: `crates/mb-engine/src/lib.rs:686-697` (`finish` becomes `Engine::finish(&self, validator, bundle, schema, op, call, ctx, response)`; call sites `:345`, `:441`, `:473`)
- Test: `crates/mb-engine/tests/generate.rs`

**Interfaces:**
- Produces (in `mb_engine::consistency`):
  - `pub const ENVELOPE_KEYS: [&str; 6] = ["data", "items", "results", "records", "entries", "nodes"];`
  - `pub fn list_items(body: &Value) -> Option<&Vec<Value>>` and `pub fn list_items_mut(body: &mut Value) -> Option<&mut Vec<Value>>` — the body when it is an array; else the first `ENVELOPE_KEYS` member that is an array; else the only top-level member that is an array of objects; else `None`.
  - `pub fn list_identity(bundle: &Bundle, op: &Operation) -> String` — `identity.field` of a `Query` operation in `bundle` with the same `resource_type` and an identity with a `path_param`; else `"id"`.
  - `impl Engine { pub fn fresh_identity(&self, bundle: &Bundle, op: &Operation, body: &Value) -> Option<Value> }` — `body` with `op.identity.field` (else `"id"`) replaced by a faker value for that property's schema (from the success schema, `$ref`s resolved), different from the current one and validating; `None` when no such value is found in 8 draws.
  - Private `Engine::unique_list_ids(&self, bundle, schema, op, validator, response) -> GeneratedResponse`, run by `finish` for 2xx `Query` bodies without a target: a later item whose identity equals an earlier one gets a fresh faker value for the item property's schema (up to 8 draws for an unused one); the result is kept only if the whole body validates.

- [ ] **Step 1: Write the failing tests**

```rust
#[tokio::test]
async fn duplicate_ids_in_a_list_are_made_unique() {
    let bundle = orders();
    let op = bundle.operation("listOrders").unwrap();
    let mut a2 = valid_order();
    a2["id"] = json!("a");
    let mut a1 = a2.clone();
    a1["total"] = json!(1);
    let mut b = valid_order();
    b["id"] = json!("b");
    let (_, engine) = scripted(vec![Ok(json!([a1, a2, b]).to_string())]);
    let mut c = call("listOrders");
    c.path_params.clear();
    let out = engine.generate(&bundle, op, &c, ctx()).await.unwrap();
    let ids: Vec<&str> = out.body.as_ref().unwrap().as_array().unwrap().iter().map(|o| o["id"].as_str().unwrap()).collect();
    assert_eq!(ids[0], "a");
    assert_eq!(ids[2], "b");
    assert!(ids[1] != "a" && ids[1] != "b", "{ids:?}");
}

#[test]
fn list_items_finds_arrays_and_envelopes() {
    use mb_engine::consistency::{list_identity, list_items};
    assert_eq!(list_items(&json!([{"id": 1}])).unwrap().len(), 1);
    assert_eq!(list_items(&json!({"object": "list", "data": [{"id": 1}, {"id": 2}]})).unwrap().len(), 2);
    assert_eq!(list_items(&json!({"page": 1, "results": [{"id": 1}]})).unwrap().len(), 1);
    assert!(list_items(&json!({"a": [{"id": 1}], "b": [{"id": 2}]})).is_none());
    let bundle = orders();
    assert_eq!(list_identity(&bundle, bundle.operation("listOrders").unwrap()), "id");
}

#[test]
fn fresh_identity_changes_only_the_id() {
    let bundle = orders();
    let op = bundle.operation("createOrder").unwrap();
    let fresh = offline().fresh_identity(&bundle, op, &valid_order()).unwrap();
    assert_ne!(fresh["id"], valid_order()["id"]);
    assert_eq!(fresh["customerId"], "c1");
    validator(&bundle, "Order").validate(&fresh).unwrap();
}
```

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test -p mb-engine --test generate duplicate_ids list_items_finds fresh_identity`
Expected: FAIL to compile (`unresolved module consistency`).

- [ ] **Step 3: Implement** `consistency.rs`, `fresh_identity`, `unique_list_ids`, and move `finish` onto `Engine` with the extra `bundle`/`schema` parameters.

- [ ] **Step 4: Run to verify they pass**

Run: `cargo test -p mb-engine`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/mb-engine
git commit -m "feat(engine): unique ids in generated lists and fresh create ids"
```

---

### Task 8: Cache, seeding and create-id uniqueness in the call pipeline (1.2, 1.3, 1.4)

**Files:**
- Modify: `crates/mb-protocols/src/call.rs` (`generate_narrowed` `:621-665`, `StateAccess` `:963-1065`, `write_state` `:1104-1145`, tests module)
- Test: `crates/mb-protocols/tests/http_rest.rs`, `crates/mb-protocols/src/call.rs` (`#[cfg(test)] mod tests`)

**Interfaces:**
- Consumes: `StateStore::insert_seeded` (Task 5); `StateStore::response_cache`, `CachedResponse`, `CachingStateStore`, `DEFAULT_CACHE_ENTRIES` (Task 6); `list_items`, `list_identity`, `Engine::fresh_identity` (Task 7); `MockRuntime::consistency`, `plain` (Task 2).
- Produces (for WS6's post-write hook):
  - `#[derive(Clone, Copy, Debug, PartialEq, Eq)] pub(crate) enum WriteOrigin { Call, Seeded }`
  - `#[derive(Clone, Debug, PartialEq)] pub(crate) struct StateWrite { pub origin: WriteOrigin, pub effect: Effect, pub resource_type: String, pub id: String, pub data: Option<Value> }` (`effect` is `Effect::Create` for seeded items; `data` is `None` for deletes)
  - `async fn write_state(state: &mut StateAccess<'_>, session: &SessionKey, op: &Operation, resource_type: &str, target_id: Option<String>, generated: &GeneratedResponse, request_body: Option<&Value>, origin: WriteOrigin) -> Vec<StateWrite>` — `Call`: today's create/update/delete behaviour, one `StateWrite` per store write that succeeded; `Seeded`: every `list_items` entry whose `list_identity` field is a string or number goes through `StateAccess::insert_seeded`, one `StateWrite` per item actually stored. `generate_narrowed` keeps the returned writes in a local `writes` so WS6 can add `for w in writes.iter().filter(|w| w.origin == WriteOrigin::Call)` right after it.
  - `StateAccess::insert_seeded(&mut self, s: &SessionKey, r: StoredResource) -> bool`
  - `fn cache_key(op: &Operation, call: &OperationCall, response_schema: Option<&Value>) -> String` — the JSON text of `[op.id, canonical(path_params), canonical(query), canonical(body) for Query ops else null, hex(DefaultHasher of canonical(response_schema)) or null]`; `fn canonical(value: &Value) -> Value` sorts object keys recursively (stable with or without serde_json's `preserve_order`).
  - Order in `generate_narrowed` for a `Query`: 404 (Task 3) → stored answer (Task 2) → cache lookup when `rt.consistency && plain(plan)` (hit: `Source::Cache`, zero usage; seed its list items too) → generate → for a 2xx answer with no target and `rt.consistency`: seed (`WriteOrigin::Seeded`) **then** `put` (seeding first, so a later list never clears a cached one). Cache `put` only when `plain(plan)`, status is 2xx, and the answer is not `Source::Fallback` with a `fallback_reason` from a failed LLM call (do not pin a transient failure; offline mocks — `OFFLINE_REASON` — do cache).
  - For `Command(Create)`: before `write_state`, when the generated identity is stored already in the session and the request body does not carry that identity, replace the body with `rt.engine.fresh_identity(...)` (up to 3 tries, each checked with `StateAccess::get`).

- [ ] **Step 1: Write the failing tests** (`http_rest.rs`, using `harness_with`/`memory_state` from Task 2; add `CachingStateStore`, `DEFAULT_CACHE_ENTRIES`, `ScenarioRuntime`, `ScenarioSpec` imports)

```rust
fn cached_state() -> Arc<dyn StateStore> {
    CachingStateStore::wrap(memory_state(), Duration::from_secs(3600), DEFAULT_CACHE_ENTRIES)
}

#[tokio::test]
async fn repeated_list_is_served_from_the_cache_until_a_write() {
    let h = harness_with(None, cached_state(), |_| {});
    let first = send(&h, get("/orders")).await;
    assert_eq!(header_str(&first, "x-mockingbird-source"), "fallback");
    let first = body_json(first).await;
    let second = send(&h, get("/orders")).await;
    assert_eq!(header_str(&second, "x-mockingbird-source"), "cache");
    assert_eq!(body_json(second).await, first);
    send(&h, post_json("/orders", r#"{"customerId":"c1","total":10}"#)).await;
    let third = send(&h, get("/orders")).await;
    assert_eq!(header_str(&third, "x-mockingbird-source"), "fallback");
}

#[tokio::test]
async fn cache_is_bypassed_when_off_or_under_a_scenario() {
    let h = harness_with(None, cached_state(), |rt| rt.consistency = false);
    send(&h, get("/orders")).await;
    assert_eq!(header_str(&send(&h, get("/orders")).await, "x-mockingbird-source"), "fallback");

    let h = harness_with(None, cached_state(), |rt| {
        rt.scenarios.insert("calm".into(), ScenarioRuntime {
            spec: ScenarioSpec { name: "calm".into(), ..ScenarioSpec::default() },
            ..ScenarioRuntime::default()
        });
    });
    let with_scenario = || Request::get("/orders").header("x-mockingbird-scenario", "calm").body(Body::empty()).unwrap();
    send(&h, with_scenario()).await;
    assert_eq!(header_str(&send(&h, with_scenario()).await, "x-mockingbird-source"), "fallback");
}

#[tokio::test]
async fn listed_items_are_served_by_id_from_state() {
    let h = offline();
    let mut changes = h.rt.state.subscribe();
    let list = body_json(send(&h, get("/orders")).await).await;
    let item = list[0].clone();
    assert!(changes.try_recv().is_err(), "seeding publishes no change");
    let resp = send(&h, get(&format!("/orders/{}", item["id"].as_str().unwrap()))).await;
    assert_eq!(header_str(&resp, "x-mockingbird-source"), "state");
    assert_eq!(body_json(resp).await, item);
}

#[tokio::test]
async fn listing_keeps_a_created_resource() {
    let h = harness(
        Some(vec![
            Ok(order("o1", "c1").to_string()),
            Ok(json!([order("o1", "someone-else")]).to_string()),
        ]),
        true,
    );
    send(&h, post_json("/orders", r#"{"customerId":"c1","total":10}"#)).await;
    send(&h, get("/orders")).await;
    let kept = h.rt.state.get(&session("default"), "orders", "o1").await.unwrap().unwrap();
    assert_eq!(kept.data["customerId"], "c1");
}

#[tokio::test]
async fn no_seeding_when_consistency_is_off() {
    let h = harness_with(None, memory_state(), |rt| rt.consistency = false);
    let list = body_json(send(&h, get("/orders")).await).await;
    let resp = send(&h, get(&format!("/orders/{}", list[0]["id"].as_str().unwrap()))).await;
    assert_eq!(header_str(&resp, "x-mockingbird-source"), "fallback");
}

#[tokio::test]
async fn create_never_reuses_a_stored_id() {
    let h = harness(Some(vec![Ok(order("o1", "c2").to_string())]), true);
    h.rt.state
        .upsert(&session("default"), StoredResource { resource_type: "orders".into(), id: "o1".into(), data: order("o1", "c1") })
        .await
        .unwrap();
    let body = body_json(send(&h, post_json("/orders", r#"{"customerId":"c2","total":10}"#)).await).await;
    let id = body["id"].as_str().unwrap();
    assert_ne!(id, "o1");
    let kept = h.rt.state.get(&session("default"), "orders", "o1").await.unwrap().unwrap();
    assert_eq!(kept.data["customerId"], "c1");
    assert!(h.rt.state.get(&session("default"), "orders", id).await.unwrap().is_some());
}
```

and in `call.rs`'s test module:

```rust
#[test]
fn cache_key_ignores_query_order_and_tracks_the_selection() {
    let rt = stream_runtime();
    let op = &rt.bundle.operations[0];
    let call = |q: Value| OperationCall { query: q.as_object().unwrap().clone(), ..OperationCall::default() };
    let a = call(json!({"a": 1, "b": 2}));
    let b = call(json!({"b": 2, "a": 1}));
    assert_eq!(cache_key(op, &a, None), cache_key(op, &b, None));
    assert_ne!(cache_key(op, &a, None), cache_key(op, &call(json!({"a": 2, "b": 2})), None));
    let narrow = json!({"type": "object", "properties": {"id": {"type": "string"}}});
    assert_ne!(cache_key(op, &a, None), cache_key(op, &a, Some(&narrow)));
}
```

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test -p mb-protocols --test http_rest cache listed listing_keeps no_seeding create_never && cargo test -p mb-protocols --lib cache_key`
Expected: FAIL (`"fallback" != "cache"`, `cache_key` not found, `"o1"` reused).

- [ ] **Step 3: Implement** as specified in Interfaces.

- [ ] **Step 4: Run to verify they pass**

Run: `cargo test -p mb-protocols --all-features && cargo test -p mockingbird --test e2e --test protocols_e2e`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/mb-protocols
git commit -m "feat(protocols): session response cache, list seeding and unique create ids"
```

---

### Task 9: Faker counts, non-null lists, string cap and new formats (1.6)

**Files:**
- Modify: `crates/mb-engine/src/faker.rs` (`gen_value` `:89-148` gains `field: Option<&str>`; `fake_object` `:150-185` passes the property name; `fake_string` `:628-702`; `bounds`/`fake_integer` `:729-784`; `flatten` branch pick `:551-566`)
- Test: `crates/mb-engine/tests/faker.rs`

**Interfaces:**
- Produces: `pub const UNCONSTRAINED_MAX_CHARS: usize = 64;` `pub const SMALL_INT_WINDOW: i64 = 100;` (in `faker.rs`). Integer window rule (replaces today's `0..1000` default; `fake_number` unchanged):

```text
lo, hi = declared bounds (exclusive ones applied as today), each optional
huge   = lo or hi missing, or hi - lo > 10_000, or lo <= -2^31, or hi >= 2^31 - 1
county = field name (lower-cased) ends with count|total|size|length|page|pages|limit|offset
if huge or (county and hi - lo > 100):
    base = max(lo or 0, 0); window = [base, min(hi or +inf, base + 100)]
    if hi < base: window = [hi - 100, hi]
else window = [lo, hi]          # then multipleOf as today
```

Nullable arrays: in `gen_value`, a `null` at the limit is returned only when the types do not include `"array"`; in `flatten`, `Pick::Random`/`Pick::Shallow` skip a branch whose only type is `"null"` when a sibling branch is an array (`type: array` or has `items`). Formats: `ipv4` (four `0..=255` octets), `ipv6` (eight 4-hex-digit groups), `hostname` (`<word>.example.com`), `uri-reference` (`/<word>/<word>`). Unconstrained strings are capped at `max(minLength, maxLength.unwrap_or(UNCONSTRAINED_MAX_CHARS))`.

- [ ] **Step 1: Write the failing tests**

```rust
fn draws(schema: Value, n: u64) -> Vec<Value> {
    let faker = Faker::default();
    (0..n).map(|seed| faker.fake(&schema, &mut StdRng::seed_from_u64(seed))).collect()
}

#[test]
fn counts_are_small_and_non_negative() {
    let graphql_int = json!({"type": "object", "required": ["n"], "properties": {"n": {"type": "integer", "minimum": -2147483648i64, "maximum": 2147483647}}});
    let named = json!({"type": "object", "required": ["total_count", "pageSize"], "properties": {
        "total_count": {"type": "integer", "minimum": 0, "maximum": 1000000}, "pageSize": {"type": "integer"}}});
    for v in draws(graphql_int, 200) {
        assert!((0..=100).contains(&v["n"].as_i64().unwrap()), "{v}");
    }
    for v in draws(named, 200) {
        assert!((0..=100).contains(&v["total_count"].as_i64().unwrap()), "{v}");
        assert!((0..=100).contains(&v["pageSize"].as_i64().unwrap()), "{v}");
    }
    for v in draws(json!({"type": "integer", "minimum": 5, "maximum": 10}), 100) {
        assert!((5..=10).contains(&v.as_i64().unwrap()));
    }
    for v in draws(json!({"type": "integer", "minimum": 1}), 100) {
        assert!((1..=101).contains(&v.as_i64().unwrap()));
    }
}

#[test]
fn nullable_lists_are_never_null() {
    for v in draws(json!({"anyOf": [{"type": "array", "items": {"type": "integer"}}, {"type": "null"}]}), 200) {
        assert!(v.is_array(), "{v}");
    }
    let at_limit = Faker::new(0).fake(&json!({"type": ["array", "null"], "items": {"type": "string"}}), &mut StdRng::seed_from_u64(1));
    assert_eq!(at_limit, json!([]));
}

#[test]
fn unconstrained_strings_stay_short_and_new_formats_parse() {
    for v in draws(json!({"type": "string", "minLength": 10}), 50) {
        let n = v.as_str().unwrap().chars().count();
        assert!((10..=64).contains(&n), "{v}");
    }
    let s = |format: &str| draws(json!({"type": "string", "format": format}), 1)[0].as_str().unwrap().to_string();
    s("ipv4").parse::<std::net::Ipv4Addr>().unwrap();
    s("ipv6").parse::<std::net::Ipv6Addr>().unwrap();
    assert!(s("hostname").ends_with(".example.com"));
    assert!(s("uri-reference").starts_with('/'));
}
```

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test -p mb-engine --test faker counts_are nullable_lists unconstrained_strings`
Expected: FAIL (`-2147483648` outside `0..=100`; `null` list; `ipv4` parse error).

- [ ] **Step 3: Implement** as specified.

- [ ] **Step 4: Run to verify they pass**

Run: `cargo test -p mb-engine`
Expected: PASS (including the proptest and `fakes_orders_order_valid` suites).

- [ ] **Step 5: Commit**

```bash
git add crates/mb-engine
git commit -m "feat(engine): small counts, non-null lists, short strings and more formats in the faker"
```

---

### Task 10: Pattern-aware strings with `rand_regex` (1.6)

**Files:**
- Modify: `Cargo.toml` (`[workspace.dependencies]`: `rand_regex = "0.19.0"`, `regex-syntax = "0.8"`), `crates/mb-engine/Cargo.toml`
- Modify: `crates/mb-engine/src/faker.rs` (`Ctx` `:43-47` gains a per-call cache of compiled patterns; `fake_string` uses it before `fixed_literal` `:710-720`)
- Test: `crates/mb-engine/tests/faker.rs`

**Interfaces:**
- Produces: `pub const PATTERN_MAX_REPEAT: u32 = 64;` and private `fn pattern_string<R: Rng>(ctx: &mut Ctx<'_>, pattern: &str, min: usize, max: Option<usize>, rng: &mut R) -> Option<String>`:
  1. strip one leading `^` and one trailing unescaped `$`;
  2. parse ASCII-first: `regex_syntax::ParserBuilder::new().unicode(false).utf8(true).build().parse(body)` → `rand_regex::Regex::with_hir(hir, PATTERN_MAX_REPEAT)`; on error `rand_regex::Regex::compile(body, PATTERN_MAX_REPEAT)`; on error → `None` (cached as unsupported);
  3. up to 8 draws of `Result<String, FromUtf8Error>` (never the panicking `String` impl), keeping the first that is UTF-8, has `min..=max` chars and matches `regex::Regex::new(pattern)` (the original pattern);
  4. `None` → today's `fixed_literal`, then the word path.
  Compiled pairs are cached in `Ctx` by pattern text for the duration of one `fake` call.

- [ ] **Step 1: Write the failing tests**

```rust
#[test]
fn strings_match_their_pattern() {
    let pattern = r"^/[A-Za-z0-9]{22}\.jpg$";
    let re = regex::Regex::new(pattern).unwrap();
    for v in draws(json!({"type": "string", "pattern": pattern}), 100) {
        let s = v.as_str().unwrap();
        assert!(re.is_match(s) && s.len() == 27, "{s}");
    }
    let re = regex::Regex::new(r"^[a-z]+-\d+$").unwrap();
    for v in draws(json!({"type": "string", "pattern": r"^[a-z]+-\d+$", "maxLength": 20}), 100) {
        let s = v.as_str().unwrap();
        assert!(re.is_match(s) && s.len() <= 20, "{s}");
    }
}

#[test]
fn unsupported_or_too_long_patterns_fall_back() {
    for v in draws(json!({"type": "string", "pattern": r"^(a)\1$"}), 20) {
        assert!(v.is_string());
    }
    for v in draws(json!({"type": "string", "pattern": "^x{100}$", "maxLength": 10}), 20) {
        assert!(v.as_str().unwrap().chars().count() <= 10);
    }
    assert_eq!(draws(json!({"type": "string", "pattern": "^v1$"}), 1)[0], json!("v1"));
}
```

(Add `regex` to `mb-engine`'s dev-dependencies if the test target lacks it; it is already a normal dependency.)

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test -p mb-engine --test faker strings_match unsupported_or`
Expected: FAIL (a random word does not match `^/[A-Za-z0-9]{22}\.jpg$`).

- [ ] **Step 3: Implement** as specified; confirm `cargo tree -p mb-engine -i regex-syntax` shows a single `regex-syntax 0.8.x` and `cargo tree -i rand@0.10.3` includes `rand_regex`.

- [ ] **Step 4: Run to verify they pass**

Run: `cargo test -p mb-engine`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add Cargo.toml Cargo.lock crates/mb-engine
git commit -m "feat(engine): generate strings that match their pattern"
```

---

### Task 11: Echo request values into same-named fields (1.6)

**Files:**
- Modify: `crates/mb-engine/src/faker.rs` (`Faker`, `FakerHints`), `crates/mb-engine/src/lib.rs` (`fake_value` `:503-526`, `fake_response` `:482-498` take `hints`; `generate`/`fallback` build them from the call), `crates/mb-engine/src/message.rs:49,68,99,102` and `crates/mb-engine/src/stream.rs:410` (pass `&FakerHints::default()`)
- Test: `crates/mb-engine/tests/faker.rs`, `crates/mb-engine/tests/generate.rs`

**Interfaces:**
- Produces:
  - `#[derive(Clone, Debug, Default, PartialEq)] pub struct FakerHints { pub echoes: Map<String, Value> }` (re-exported from `mb_engine`).
  - `impl FakerHints { pub fn from_call(op: &Operation, call: &OperationCall) -> Self }` — scalar path parameters, scalar query parameters, and for `OperationKind::Query` the request body's top-level scalars (GraphQL arguments); later sources do not overwrite earlier ones.
  - `impl Faker { pub fn with_hints(self, hints: FakerHints) -> Self }`.
  - Echo rule in `gen_value`: only at depth 1 (members of the root object), when the field name is an `echoes` key, the schema has no `const`/`enum`, and the value fits the chosen type (string as is; integer/number when the string parses; boolean from `"true"`/`"false"`).
  - `pub(crate) fn fake_value(&self, bundle: &Bundle, schema: &Value, validator: Option<&SchemaValidator>, hints: &FakerHints) -> Value`.

- [ ] **Step 1: Write the failing tests**

```rust
// faker.rs
#[test]
fn echoed_values_fill_same_named_root_fields() {
    let mut echoes = serde_json::Map::new();
    echoes.insert("login".into(), json!("octocat"));
    echoes.insert("page".into(), json!("3"));
    let faker = Faker::default().with_hints(mb_engine::FakerHints { echoes });
    let schema = json!({"type": "object", "required": ["login", "page", "owner"], "properties": {
        "login": {"type": "string"}, "page": {"type": "integer"},
        "owner": {"type": "object", "required": ["login"], "properties": {"login": {"type": "string"}}}}});
    let v = faker.fake(&schema, &mut StdRng::seed_from_u64(7));
    assert_eq!(v["login"], "octocat");
    assert_eq!(v["page"], 3);
    assert_ne!(v["owner"]["login"], "octocat", "only root fields echo");
}

// generate.rs
#[tokio::test]
async fn offline_answers_echo_path_parameters() {
    let bundle = inline(r#"
openapi: 3.1.0
info: {title: Users, version: "1"}
paths:
  /users/{login}:
    get:
      operationId: getUser
      parameters: [{name: login, in: path, required: true, schema: {type: string}}]
      responses:
        "200": {description: OK, content: {application/json: {schema: {type: object, required: [login], properties: {login: {type: string}}}}}}
"#);
    let op = bundle.operation("getUser").unwrap();
    let mut c = OperationCall::default();
    c.path_params.insert("login".into(), json!("octocat"));
    let out = offline().generate(&bundle, op, &c, ctx()).await.unwrap();
    assert_eq!(out.body.unwrap()["login"], "octocat");
}
```

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test -p mb-engine --test faker echoed_values && cargo test -p mb-engine --test generate offline_answers_echo`
Expected: FAIL to compile (`no FakerHints`), then a random word instead of `octocat`.

- [ ] **Step 3: Implement** as specified.

- [ ] **Step 4: Run to verify they pass**

Run: `cargo test -p mb-engine`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/mb-engine
git commit -m "feat(engine): echo path, query and argument values into faked fields"
```

---

### Task 12: Patch pattern, length, enum, format and const errors locally (1.8)

**Files:**
- Modify: `crates/mb-engine/src/validate.rs:26-80` (`patch_nulls` → `patch`) and its tests `:132-209`
- Modify: `crates/mb-engine/src/guards.rs:83-93` (`Checked::Patched` doc), `:171-189` (`Engine::patch_nulls` → `Engine::patch`)
- Test: `crates/mb-engine/src/validate.rs` (unit), `crates/mb-engine/tests/generate.rs`

**Interfaces:**
- Produces: `impl SchemaValidator { pub fn patch(&self, instance: &Value, fill: impl FnMut(&Value) -> Value) -> Option<Value> }` replacing `patch_nulls`. Patchable error kinds: `ValidationErrorKind::Type` on a `null` instance (today's drop-or-fill handling), and `Pattern`, `MaxLength`, `MinLength`, `Enum`, `Format`, `Constant` — whose instance is replaced by `fill(subschema)` (the schema object holding the failing keyword, with `$defs`, as `subschema` already computes). `None` when any other error is present, nothing is patchable, or the result does not validate. (2020-12 formats are annotations in this validator, so `Format` rarely fires; it is handled for completeness.)

- [ ] **Step 1: Write the failing tests**

```rust
// validate.rs tests module
#[test]
fn pattern_length_enum_and_const_errors_are_refilled() {
    let v = SchemaValidator::new(&json!({
        "type": "object",
        "properties": {
            "path": {"type": "string", "pattern": "^/x$"},
            "code": {"type": "string", "maxLength": 2},
            "kind": {"enum": ["a", "b"]},
            "v": {"const": 1}
        }
    }))
    .unwrap();
    let out = v
        .patch(&json!({"path": "/y", "code": "long", "kind": "c", "v": 2}), |s| {
            s.get("const").or_else(|| s.get("enum").and_then(|e| e.get(0))).cloned().unwrap_or_else(|| {
                if s.get("pattern").is_some() { json!("/x") } else { json!("ok") }
            })
        })
        .unwrap();
    assert_eq!(out, json!({"path": "/x", "code": "ok", "kind": "a", "v": 1}));
}
```

(rename the three existing tests' `patch_nulls` calls to `patch`; `other_errors_are_not_patched` keeps asserting `None` for `"title": 1`.)

```rust
// generate.rs
#[tokio::test]
async fn pattern_errors_are_patched_without_a_repair_call() {
    let bundle = inline(r#"
openapi: 3.1.0
info: {title: Images, version: "1"}
paths:
  /images/{id}:
    get:
      operationId: getImage
      parameters: [{name: id, in: path, required: true, schema: {type: string}}]
      responses:
        "200": {description: OK, content: {application/json: {schema: {type: object, required: [file_path], properties: {file_path: {type: string, pattern: "^/[A-Za-z0-9]{22}\\.jpg$"}}}}}}
"#);
    let op = bundle.operation("getImage").unwrap();
    let (provider, engine) = scripted(vec![Ok(json!({"file_path": "/poster.jpg"}).to_string())]);
    let out = engine.generate(&bundle, op, &OperationCall::default(), ctx()).await.unwrap();
    assert_eq!(provider.requests().len(), 1, "no repair call");
    assert_eq!(out.source, Source::Repaired);
    let path = out.body.unwrap()["file_path"].as_str().unwrap().to_string();
    assert!(regex::Regex::new(r"^/[A-Za-z0-9]{22}\.jpg$").unwrap().is_match(&path), "{path}");
}
```

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test -p mb-engine --lib validate && cargo test -p mb-engine --test generate pattern_errors_are_patched`
Expected: FAIL to compile (`no method patch`), then `2 != 1` provider requests.

- [ ] **Step 3: Implement** `patch` and switch `guards.rs` to it (the faker's pattern support from Task 10 produces the fill).

- [ ] **Step 4: Run to verify they pass**

Run: `cargo test -p mb-engine`
Expected: PASS, including `disallowed_nulls_are_patched_without_a_repair_call` and `nulls_with_other_errors_still_ask_for_a_repair`.

- [ ] **Step 5: Commit**

```bash
git add crates/mb-engine
git commit -m "feat(engine): patch pattern, length, enum and const errors without a repair call"
```

---

### Task 13: Runaway strings: prompt line and truncation guard (1.7)

**Files:**
- Modify: `crates/mb-engine/src/prompt.rs:25-28` (`ROLE_STATEMENT`)
- Modify: `crates/mb-engine/src/validate.rs` (add `truncate_runaway`), `crates/mb-engine/src/guards.rs:146-181` (`check_output`)
- Test: `crates/mb-engine/tests/generate.rs`

**Interfaces:**
- Produces:
  - `ROLE_STATEMENT` gains the sentence `Keep free-text strings under 200 characters unless the schema allows more.`
  - `pub const RUNAWAY_CHARS: usize = 2_000;` `pub const TRUNCATED_CHARS: usize = 200;` in `validate.rs`.
  - `impl SchemaValidator { pub fn truncate_runaway(&self, instance: &Value) -> Option<Value> }` — every string longer than `RUNAWAY_CHARS` chars whose schema location declares no `maxLength` is cut to its first `TRUNCATED_CHARS` chars; `Some` only when something was cut and the result validates. Schema location lookup (`fn declares_max_length(&self, pointer: &str) -> bool`): walk the instance pointer from the root schema, at each node expanding `$ref` (via `$defs`), `allOf`, `oneOf`, `anyOf` (bounded at 64 hops), then stepping into `properties[seg]` else an object `additionalProperties`, or `prefixItems[i]` else `items`; declared when any reached schema has `maxLength` (conservative: a branch that declares it protects the string).
  - `check_output`: `Valid(v)` → `Patched(t)` when `truncate_runaway(&v)` is `Some(t)`; the same pass runs on a `Patched` value.

- [ ] **Step 1: Write the failing tests**

```rust
#[tokio::test]
async fn prompt_asks_for_short_free_text() {
    let bundle = orders();
    let op = bundle.operation("getOrder").unwrap();
    let (provider, engine) = scripted(vec![Ok(valid_order_at_o1().to_string())]);
    engine.generate(&bundle, op, &call("getOrder"), ctx()).await.unwrap();
    let system = &provider.requests()[0].messages[0].content;
    assert!(system.contains("under 200 characters"), "{system}");
}

#[tokio::test]
async fn runaway_strings_are_truncated_unless_a_max_length_allows_them() {
    let bundle = inline(r#"
openapi: 3.1.0
info: {title: Places, version: "1"}
paths:
  /places/{id}:
    get:
      operationId: getPlace
      parameters: [{name: id, in: path, required: true, schema: {type: string}}]
      responses:
        "200": {description: OK, content: {application/json: {schema: {type: object, properties: {city: {type: string}, essay: {type: string, maxLength: 5000}}}}}}
"#);
    let op = bundle.operation("getPlace").unwrap();
    let answer = json!({"city": "é".repeat(3000), "essay": "x".repeat(3000)});
    let (provider, engine) = scripted(vec![Ok(answer.to_string())]);
    let out = engine.generate(&bundle, op, &OperationCall::default(), ctx()).await.unwrap();
    assert_eq!(provider.requests().len(), 1);
    assert_eq!(out.source, Source::Repaired);
    let body = out.body.unwrap();
    assert_eq!(body["city"].as_str().unwrap().chars().count(), 200);
    assert_eq!(body["essay"].as_str().unwrap().len(), 3000);
}
```

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test -p mb-engine --test generate prompt_asks_for_short_free_text runaway_strings`
Expected: FAIL (`system` lacks the sentence; `city` keeps 3000 chars, source `llm`).

- [ ] **Step 3: Implement** as specified.

- [ ] **Step 4: Run to verify they pass**

Run: `cargo test -p mb-engine`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/mb-engine
git commit -m "feat(engine): cap runaway free-text strings"
```

---

### Task 14: Store and Admin API: mock behaviour and per-operation instructions (1.5, 1.9, `consistency`)

**Files:**
- Modify: `crates/mb-store/src/model.rs:66-101` (`Mock`, `NewMock`), `:270-288` (`ScenarioRules`); add `Consistency`, `UnknownIds`, `MockBehavior`
- Create: `crates/mb-store/migrations/sqlite/0610_mock_behavior.sql`, `crates/mb-store/migrations/postgres/0610_mock_behavior.sql`
- Modify: `crates/mb-store/src/sqlite.rs:35-36,259-278,627-730` and the matching code in `crates/mb-store/src/postgres.rs` (`MOCK_COLUMNS`, row mapping, insert, update)
- Modify: every `NewMock { … }` literal (25 sites: `rg -l "NewMock \{" crates`) to add `behavior: MockBehavior::default()`, and the full `ScenarioRules { … }` literals (`rg -n "ScenarioRules \{" crates`)
- Modify: `crates/mb-admin/src/mocks.rs:112-210,286-336,377-421` (view, create, patch), `crates/mb-admin/src/scenarios.rs:25-100,223-310`
- Modify: `crates/mb-console/web/src/api/schema.d.ts` (generated)
- Test: `crates/mb-store/tests/common/conformance.rs`, `crates/mb-admin/tests/mocks.rs`, `crates/mb-admin/tests/scenarios.rs`

**Interfaces:**
- Produces (mb-store, re-exported):
  - `#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)] pub enum Consistency { #[default] On, Off }` — serializes `"on"`/`"off"`; deserializes those strings or a bool (`true` = on).
  - `#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)] pub enum UnknownIds { #[default] Generate, NotFound }` — serializes `"generate"`/`"404"`; deserializes `"generate"`, `"404"` or the integer `404` (YAML `unknown_ids: 404` is an integer).
  - `#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)] pub struct MockBehavior { #[serde(default)] pub consistency: Consistency, #[serde(default)] pub unknown_ids: UnknownIds, #[serde(default, skip_serializing_if = "Option::is_none")] pub default_scenario: Option<String> }` with `pub fn is_default(&self) -> bool`.
  - `Mock` and `NewMock`: `#[serde(default, skip_serializing_if = "MockBehavior::is_default")] pub behavior: MockBehavior`.
  - `ScenarioRules`: `#[serde(default, skip_serializing_if = "BTreeMap::is_empty")] pub operation_instructions: BTreeMap<String, String>`.
  - Migration (both dialects; Postgres type `JSONB`): `ALTER TABLE mocks ADD COLUMN behavior TEXT NOT NULL DEFAULT '{}';`. (Named `0610` to leave `0600` to WS4; if both land, they apply in order.)
- Produces (Admin API):
  - `MockView`: `consistency` (`"on"|"off"`), `unknown_ids` (`"generate"|"404"`), `default_scenario` (omitted when unset); `#[schema(value_type = String)]` on the enums.
  - `CreateMock`: same three fields, defaulted. `PatchMock`: `consistency: Option<Consistency>`, `unknown_ids: Option<UnknownIds>`, `#[serde(default, deserialize_with = "present")] default_scenario: Option<Option<String>>` (`null` clears). A `default_scenario` that names no scenario of the mock → 422 problem+json `Invalid mock` with detail `mock "<m>" has no scenario "<name>"`.
  - `PutScenario`/`ScenarioView`: `operation_instructions: BTreeMap<String, String>` (view omits it when empty). 422 when a key is not an operation of the active contract, or when `operations` is non-empty and the key is not in it.

- [ ] **Step 1: Write the failing tests**

```rust
// mb-store/tests/common/conformance.rs, added to run_all
async fn mock_behavior_round_trips(store: &Arc<dyn ControlStore>) {
    let mut mock = create(store, "behaving").await;
    assert_eq!(mock.behavior, MockBehavior::default());
    mock.behavior = MockBehavior {
        consistency: Consistency::Off,
        unknown_ids: UnknownIds::NotFound,
        default_scenario: Some("calm".into()),
    };
    store.update_mock(&mock).await.unwrap();
    let got = store.get_mock(DEFAULT_WORKSPACE, "behaving").await.unwrap().unwrap();
    assert_eq!(got.behavior, mock.behavior);
    let rules = ScenarioRules {
        operation_instructions: BTreeMap::from([("getOrder".into(), "A gift.".into())]),
        ..ScenarioRules::default()
    };
    store.put_scenario(&Scenario { mock_id: got.id, name: "calm".into(), instructions: None, rules: rules.clone(), source: ResourceSource::Api }).await.unwrap();
    assert_eq!(store.list_scenarios(got.id).await.unwrap()[0].rules, rules);
    assert_eq!(serde_json::to_value(MockBehavior::default()).unwrap(), json!({"consistency": "on", "unknown_ids": "generate"}));
    let parsed: MockBehavior = serde_json::from_value(json!({"consistency": false, "unknown_ids": 404})).unwrap();
    assert_eq!((parsed.consistency, parsed.unknown_ids), (Consistency::Off, UnknownIds::NotFound));
}
```

```rust
// mb-admin/tests/mocks.rs
#[tokio::test]
async fn patch_consistency_settings() {
    let h = harness().await;
    let (status, created) = h.send(Method::POST, "/api/v1/mocks", Some(json!({"name": "orders"}))).await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    assert_eq!((created["consistency"].as_str(), created["unknown_ids"].as_str()), (Some("on"), Some("generate")));
    assert!(created.get("default_scenario").is_none());
    let (status, body) = h.send(Method::PATCH, "/api/v1/mocks/orders", Some(json!({"default_scenario": "calm"}))).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    h.send(Method::PUT, "/api/v1/mocks/orders/scenarios/calm", Some(json!({}))).await;
    let (status, view) = h
        .send(Method::PATCH, "/api/v1/mocks/orders", Some(json!({"consistency": "off", "unknown_ids": "404", "default_scenario": "calm"})))
        .await;
    assert_eq!(status, StatusCode::OK, "{view}");
    assert_eq!((view["consistency"].as_str(), view["unknown_ids"].as_str(), view["default_scenario"].as_str()), (Some("off"), Some("404"), Some("calm")));
    let (_, view) = h.send(Method::PATCH, "/api/v1/mocks/orders", Some(json!({"default_scenario": null}))).await;
    assert!(view.get("default_scenario").is_none());
}
```

```rust
// mb-admin/tests/scenarios.rs (no active contract: only the operations-subset rule applies)
#[tokio::test]
async fn put_scenario_with_operation_instructions() {
    let h = harness().await;
    h.send(Method::POST, "/api/v1/mocks", Some(json!({"name": "orders"}))).await;
    let (status, view) = h
        .send(Method::PUT, "/api/v1/mocks/orders/scenarios/gift", Some(json!({
            "operations": ["getOrder"], "operation_instructions": {"getOrder": "A gift."}})))
        .await;
    assert_eq!(status, StatusCode::OK, "{view}");
    assert_eq!(view["operation_instructions"]["getOrder"], "A gift.");
    let (status, body) = h
        .send(Method::PUT, "/api/v1/mocks/orders/scenarios/gift", Some(json!({
            "operations": ["getOrder"], "operation_instructions": {"listOrders": "Nope."}})))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert!(body["type"].is_string(), "{body}");
}
```

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test -p mb-store --test sqlite && cargo test -p mb-admin --test mocks patch_consistency && cargo test -p mb-admin --test scenarios operation_instructions`
Expected: FAIL to compile (`no field behavior`, `cannot find type MockBehavior`).

- [ ] **Step 3: Implement** the types, migrations, both store backends, all literal sites, the Admin API fields and checks; then regenerate the client.

Run: `cd crates/mb-console/web && npm run gen:api && npm run check:api`
Expected: `src/api/schema.d.ts` changes; `check:api` passes.

- [ ] **Step 4: Run to verify they pass**

Run: `cargo test -p mb-store && cargo test -p mb-admin && scripts/test-postgres.sh` (when Postgres is available), `cd crates/mb-console/web && npm run typecheck && npm run lint && npm test`
Expected: PASS; store snapshots unchanged (default behaviour is not serialized).

- [ ] **Step 5: Commit**

```bash
git add crates/mb-store crates/mb-admin crates/mb-console/web/src/api/schema.d.ts crates/mockingbird crates/mb-admin/tests
git commit -m "feat(admin): mock consistency, unknown ids, default scenario and per-operation instructions"
```

---

### Task 15: Config file keys, reconcile and runtime wiring

**Files:**
- Modify: `crates/mockingbird/src/file_config.rs:905-945` (`FileMock`), `:1145-1163` (`FileScenario.operations`), `:1240-1265` (`validate`)
- Modify: `crates/mockingbird/src/reconcile.rs:505-540,588-610` (`NewMock`, wanted `Mock`, `ScenarioRules`)
- Modify: `crates/mockingbird/src/runtime.rs:315-336,545-560` (`build_runtime_with_budget`, `scenario_runtime`)
- Modify: `docs/configuration.md` (mocks table near `:223`, scenarios table near `:270`), `docs/examples/mockingbird.full.yaml` (mock `orders`, `:148-182`), `docs/scenarios.md` or the README scenario section if one describes header-only selection (`rg -l "X-Mockingbird-Scenario" docs README.md`)
- Test: `crates/mockingbird/tests/config.rs`, `crates/mockingbird/tests/registry.rs`

**Interfaces:**
- Consumes: `MockBehavior`, `Consistency`, `UnknownIds` (mb-store, Task 14); `MockRuntime::{consistency, unknown_ids, default_scenario}`, `mb_protocols::UnknownIds` (Task 2); `ScenarioRuntime::with_operation_instructions` (Task 4).
- Produces:
  - `FileMock`: `#[serde(default)] pub consistency: Consistency`, `#[serde(default)] pub unknown_ids: UnknownIds`, `#[serde(default)] pub default_scenario: Option<String>`.
  - `#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)] #[serde(untagged)] pub enum FileScenarioOperation { Id(String), Entry(FileScenarioOperationEntry) }`; `#[serde(deny_unknown_fields)] pub struct FileScenarioOperationEntry { pub id: String, #[serde(default)] pub instructions: Option<String> }`; `FileScenario.operations: Vec<FileScenarioOperation>`. Reconcile maps ids into `ScenarioRules.operations` and entries with instructions into `operation_instructions`.
  - `validate`: `mocks.<m>.default_scenario` must be a key of `mocks.<m>.scenarios` — error `mocks.<m>.default_scenario: no scenario named "<s>"`.
  - `build_runtime_with_budget`: `rt.consistency = mock.behavior.consistency == Consistency::On`, `rt.unknown_ids` mapped, `rt.default_scenario = mock.behavior.default_scenario.clone()`; `scenario_runtime` applies `with_operation_instructions(s.rules.operation_instructions)`.
  - Docs rows: `mocks.<name>.consistency` (`on`, `off`; default `on`), `mocks.<name>.unknown_ids` (`generate`, `404`; default `generate`), `mocks.<name>.default_scenario` (string; unset), `mocks.<name>.scenarios.<name>.operations[].id`, `mocks.<name>.scenarios.<name>.operations[].instructions`; the `operations` row says entries are an id or `{id, instructions}`. Example: add to mock `orders` `consistency: on`, `unknown_ids: generate`, `default_scenario: friendly`, and a scenario `friendly: {operations: [{id: getOrder, instructions: Orders belong to returning customers.}]}`.

- [ ] **Step 1: Write the failing tests**

```rust
// crates/mockingbird/tests/config.rs
const BEHAVIOR: &str = r#"
mocks:
  orders:
    contract: orders.yaml
    consistency: off
    unknown_ids: 404
    default_scenario: gift
    scenarios:
      gift:
        operations:
          - listOrders
          - {id: getOrder, instructions: This order is a gift.}
"#;

#[tokio::test]
async fn reconcile_applies_mock_behavior_and_operation_instructions() {
    let ws = Workspace::new();
    let cfg = ws.load(BEHAVIOR);
    let store = store().await;
    reconcile(store.as_ref(), no_state().as_ref(), &cfg).await.unwrap();
    let mock = store.get_mock(DEFAULT_WORKSPACE, "orders").await.unwrap().unwrap();
    assert_eq!(mock.behavior.consistency, mb_store::Consistency::Off);
    assert_eq!(mock.behavior.unknown_ids, mb_store::UnknownIds::NotFound);
    assert_eq!(mock.behavior.default_scenario.as_deref(), Some("gift"));
    let s = &store.list_scenarios(mock.id).await.unwrap()[0];
    assert_eq!(s.rules.operations, ["listOrders", "getOrder"]);
    assert_eq!(s.rules.operation_instructions.get("getOrder").map(String::as_str), Some("This order is a gift."));
    let again = reconcile(store.as_ref(), no_state().as_ref(), &cfg).await.unwrap();
    assert!(again.updated.is_empty(), "{again:?}");
}

#[test]
fn default_scenario_must_name_a_scenario() {
    let ws = Workspace::new();
    ws.write("mocks:\n  orders:\n    contract: orders.yaml\n    default_scenario: missing\n");
    let err = file_config::load(&ws.config_path()).and_then(|c| c.validate().map_err(anyhow::Error::msg)).unwrap_err();
    assert!(err.to_string().contains("mocks.orders.default_scenario"), "{err:#}");
}
```

```rust
// crates/mockingbird/tests/registry.rs
#[tokio::test]
async fn runtime_carries_mock_behavior() {
    let store = store().await;
    let mut mock = create_mock(&store, "orders", "/orders", None).await;
    activate(&store, &mock, &fixture_source()).await;
    mock.behavior = MockBehavior { consistency: Consistency::Off, unknown_ids: UnknownIds::NotFound, default_scenario: Some("gift".into()) };
    let mock = store.update_mock(&mock).await.unwrap();
    let rt = mockingbird::runtime::build_runtime(&deps(store.clone()), &mock).await.unwrap().unwrap();
    assert!(!rt.consistency);
    assert_eq!(rt.unknown_ids, mb_protocols::UnknownIds::NotFound);
    assert_eq!(rt.default_scenario.as_deref(), Some("gift"));
}
```

(If `FileConfig::validate` is already run inside `file_config::load`, assert on `load`'s error directly; keep the substring assertion.)

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test -p mockingbird --test config reconcile_applies_mock_behavior default_scenario_must && cargo test -p mockingbird --test registry runtime_carries`
Expected: FAIL (`unknown field consistency`).

- [ ] **Step 3: Implement** the config types, validation, reconcile mapping, runtime wiring, docs rows and example keys.

- [ ] **Step 4: Run to verify they pass**

Run: `cargo test -p mockingbird --test config --test registry --test docs_reference --test scenarios_e2e`
Expected: PASS (`every_config_key_is_documented` included).

- [ ] **Step 5: Commit**

```bash
git add crates/mockingbird docs/configuration.md docs/examples/mockingbird.full.yaml docs
git commit -m "feat(config): consistency, unknown_ids, default_scenario and per-operation scenario instructions"
```

---

### Task 16: Gates, changelog, pull request

**Files:**
- Modify: `CHANGELOG.md` (`## [Unreleased]` → `### Added` / `### Changed`)

- [ ] **Step 1: Changelog.** Under `### Added`: stored GETs served from session state (`X-Mockingbird-Source: state`); per-session response cache (`cache`, 500 entries, cleared by writes of the type and session resets; `consistency: off` disables it and seeding); list→detail seeding; `unknown_ids: 404`; `default_scenario` and per-operation scenario `instructions`; faker `pattern` support (`rand_regex`), `ipv4`/`ipv6`/`hostname`/`uri-reference`, echoed request values; migration `0610_mock_behavior` adds `mocks.behavior`. Under `### Changed`: faker integers default to `0..=100` (bounds wider than ±2^31 ignored), nullable arrays never `null`, unconstrained strings ≤ 64 chars; LLM strings over 2 000 chars without a `maxLength` are cut to 200 (`repaired`); `pattern`/`maxLength`/`minLength`/`enum`/`const` errors are patched without a repair call; generated lists get unique ids and creates never reuse a session id.

- [ ] **Step 2: Run every local gate**

```bash
cargo fmt --all --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-features
cargo check -p mockingbird --no-default-features
scripts/test-postgres.sh && scripts/test-redis.sh   # when initdb / redis-server are installed
cd crates/mb-console/web && npm ci && npm run typecheck && npm run lint && npm test && npm run build && npm run check:api
```

Expected: all pass, except the known pre-existing failures to ignore and mention in the PR: `tls::llm_client_trusts_outbound_ca` on macOS and the occasionally flaky `graphql::rejections_are_recorded`.

- [ ] **Step 3: Commit and push**

```bash
git add CHANGELOG.md
git commit -m "docs: changelog for engine consistency and faker improvements

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>"
git push -u origin feat/ws1-engine-consistency
```

- [ ] **Step 4: Open the PR (do not merge; the user approves each merge)**

```bash
gh pr create --base main --head feat/ws1-engine-consistency \
  --title "feat: engine consistency and faker (WS1)" \
  --body "$(cat <<'EOF'
Implements WS1 (1.1–1.9) of docs/superpowers/specs/2026-10-22-improvements-backlog-design.md.

- Stored GETs from session state (`state` source); per-session LRU response cache (`cache`, 500 entries) as an `mb-state` decorator; list→detail seeding via a quiet `StateStore::insert_seeded`; unique list ids and create ids; opt-in `unknown_ids: 404`.
- Faker: small non-negative counts, non-null lists, 64-char string cap, `pattern` via `rand_regex` 0.19, new formats, echoed request values.
- Local patching of pattern/length/enum/const errors; runaway-string truncation.
- `default_scenario` and per-operation scenario instructions (config, store, Admin API, console client regenerated). Migration `0610_mock_behavior`.
- `write_state` returns `StateWrite`s with `WriteOrigin::{Call, Seeded}` for WS6's post-write hook.
- Not covered: GraphQL queries answered by joint generation (several root fields, `graphql/batch.rs`, WS2-owned) skip state/cache/seeding.

Known pre-existing failures: `tls::llm_client_trusts_outbound_ca` (macOS), flaky `graphql::rejections_are_recorded`.

🤖 Generated with [Claude Code](https://claude.com/claude-code)
EOF
)"
```

Expected: the PR URL is printed; CI runs the same gates.
