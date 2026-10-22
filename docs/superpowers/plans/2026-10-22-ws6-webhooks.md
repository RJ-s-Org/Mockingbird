# WS6: Webhooks Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** After a successful write operation a mock POSTs a signed webhook event (the contract's OpenAPI `webhooks`/`callbacks` payload when defined, else a Stripe-shaped envelope) to each configured URL, retries failures, and lists deliveries through the Admin API, the CLI and MCP.

**Architecture:** A new `mb_protocols::webhooks` module owns signing, event naming, the dispatcher (spawned, bounded, retrying deliveries) and an in-memory delivery log shared by the process. `call.rs` `generate_narrowed` calls one hook after its state write, for 2xx `Command` operations only. Targets are stored on the mock (`mb-store`, migration), set from `mockingbird.yaml` or the Admin API, and resolved (secret references) when `mockingbird::runtime` builds the `MockRuntime`. OpenAPI `webhooks`/`callbacks` compile into a new `Bundle.webhooks` list.

**Tech Stack:** Rust 1.97 workspace; `hmac` + `sha2` (already workspace deps), `reqwest` via `mb_llm::http_client`, `tokio`, `axum` + `utoipa` (Admin API), `clap` (CLI), `wiremock` (test receivers); console client regenerated with `npm run gen:api` (Node 22).

**Spec:** `docs/superpowers/specs/2026-10-22-improvements-backlog-design.md` (section **WS6: Webhooks (wave 2)**).

**Branch:** `feat/ws6-webhooks`, cut from `main` **after WS1 has merged**.

## Global Constraints

- No `unwrap()`/`expect()` outside tests.
- Admin API errors are RFC 9457 `application/problem+json` (`crate::problem::Problem`).
- Secrets are only ever references (`env:`, `file:`, `vault:`); a webhook secret value is never stored, logged, printed, returned by the API or written to a file. A `Debug` of anything holding a resolved secret prints `"<redacted>"`.
- A new serde field on a stored type is `#[serde(default, skip_serializing_if = …)]` so stored JSON and snapshots stay byte-identical.
- `mb-protocols` depends on neither `mb-store` nor `mb-auth`; `mb-engine`, `mb-store`, `mb-state` stay free of axum.
- New config keys go in `docs/configuration.md` **and** `docs/examples/mockingbird.full.yaml` (`crates/mockingbird/tests/docs_reference.rs` checks both), in a WS6-owned section (`mocks.<name>.webhooks…`, `server.api_webhook_secret_refs`) so WS1/WS4 edits to those files stay textually separate.
- After any Admin API change: `npm run gen:api` in `crates/mb-console/web`, commit `src/api/schema.d.ts`.
- Spec values: config `webhooks: [{url, events: [operation ids or "*"], secret: <secret ref>, signing: stripe | hmac-sha256 | none, delay_ms}]`; envelope `{type: "<resource>.<created|updated|deleted>", data: {object: <response>}}`; `Stripe-Signature: t=<ts>,v1=<hex hmac_sha256(secret, "<ts>.<body>")>`; `X-Mockingbird-Signature: sha256=<hex>`; retries: 3.
- Commits: `type(scope): summary`, ending with `Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>`.
- Never commit or print a real key; tests use literal throwaway secrets such as `whsec_test_secret` set through a test-only env var.

### Decisions this plan pins (the spec leaves them open)

| Question | Decision |
|---|---|
| Signer location | `mb-protocols` (it cannot depend on `mb-admin`; `mb-admin` already depends on `mb-protocols`). `mb_admin::webhook::signature` delegates to it. `mb-ext` (semver-stable) is not touched. |
| Delivery state | In-memory ring per `(workspace, mock)`, **200** records, in a process-wide `Arc<DeliveryLog>` held by `RuntimeDeps` and handed to the Admin API as an `axum::Extension`. Not in `mb-state`: deliveries are diagnostics, not session data. On split `role: control`/`data` deployments a control node lists none (documented). |
| Retry/backoff | 1 attempt + **3 retries**, sleeping **1 s, 2 s, 4 s** (base 1 s, doubling; same as the audit webhook). Retry on any non-2xx or transport error. Attempt timeout **10 s**. |
| Concurrency bound | At most **256** pending deliveries per mock (a `Semaphore`); beyond that a delivery is recorded `dropped` and `mb_webhook_dropped_total` increments. |
| Limits | At most **16** targets per mock; `delay_ms` ≤ **60 000**; `url` is absolute `http`/`https`. |
| Event type | `<singular resource>.<verb>`: verb from `Effect` (`Create`→`created`, `Update`/`Other`→`updated`, `Delete`→`deleted`); resource is the last static path segment directly followed by a `{param}` when the path ends in a param (`/v1/customers/{customer}` → `customers`), else `op.resource_type`, else `op.id`; singularised (`ies`→`y`; `sses`/`xes`/`ches`/`shes`→drop `es`; else drop one trailing `s` unless it ends `ss`). `/v1/payment_intents` POST → `payment_intent.created`. |
| `events` filter | An entry matches when it equals `"*"`, the operation id, or the event type (so Stripe users can write `customer.created`). Default `["*"]`. |
| Envelope | `{"id": "evt_<24 alnum>", "object": "event", "type": …, "created": <unix s>, "data": {"object": <response body>}}`. Response without a body: `{"id": <target id>, "deleted": true}` for deletes, `{}` otherwise. Still Stripe's shape. |
| Contract payload | `Bundle.webhooks` entry chosen as: a callback of the operation; else a `webhooks` entry whose name equals (ASCII case-insensitive) the event type or the operation id. Generated with `Engine::generate_message` on a synthetic publish `Channel` (id `webhook:<name>`, binding `WebSocket { path: "webhook:<name>" }`), so LLM + faker fallback come for free without touching `mb-engine`. Callback URL expressions are ignored: events go to configured targets only. |
| Default signing | `hmac-sha256` when `secret` is set, else `none`. `stripe`/`hmac-sha256` without `secret` is invalid. Each attempt is re-signed (Stripe `t` is the attempt's time). |
| Secret refs via Admin API | Must match the new `server.api_webhook_secret_refs` (default `["env:*_WEBHOOK_SECRET", "env:MB_WEBHOOK_*"]`), separate from the LLM-profile globs. File-config targets are not restricted (as for profiles). |
| Unresolvable secret | The mock still builds; that target's deliveries are recorded `failed` with `error: "secret reference <ref> could not be resolved"` and are never sent unsigned. |
| Delivery log URL | Recorded without userinfo and query (`https://u:p@h/x?token=1` → `https://h/x`). |
| Headers sent | `content-type: application/json`, `user-agent: Mockingbird-Webhooks/<CARGO_PKG_VERSION>`, `x-mockingbird-event-id`, `x-mockingbird-event-type`, plus the signature header. |
| Migration | `0700_mock_webhooks.sql` (sqlite `TEXT`, postgres `JSONB`), leaving `0600` for WS1. Task 0 re-checks. |

## Review Focus

1. **Slow or dead receiver** — the data-plane response must not wait for delivery: a POST answers while the receiver sleeps 3 s (test in Task 6).
2. **Secret env var unset** — the mock keeps serving; deliveries show `failed` naming the reference, never the value, and nothing is sent unsigned (test in Task 7).
3. **Non-write or unsuccessful calls** — GETs, WS1 list seeding writes, 4xx, forced/chaos 5xx fire nothing (tests in Task 6).
4. **Credentials in the target URL** — the delivery log and Admin API never show userinfo or query (test in Task 5).
5. **Delete with 204 and no body** — the envelope still carries `data.object = {"id": <id>, "deleted": true}` (test in Task 6).

---

### Task 0: Rebase onto WS1 and confirm the hook point

**Files:** none changed (read-only check).

**Interfaces:**
- Consumes (from WS1, assumed): `crates/mb-protocols/src/call.rs` still has `pub(crate) async fn generate_narrowed(rt: &MockRuntime, plan: &CallPlan<'_>, call: &OperationCall, response_schema: Option<&Value>) -> Result<GeneratedResponse, EngineError>`, which calls `write_state(...)` for a 2xx with a resource type and then `state.touch(session)`. WS1 may add a `seeded`/origin argument to `write_state` and call it from list seeding; WS6 does **not** hook inside `write_state`.

- [ ] **Step 1:** `git fetch && git checkout -b feat/ws6-webhooks origin/main`; confirm `git log origin/main --oneline | grep -i ws1` shows the WS1 merge. If WS1 has not merged, stop and report.
- [ ] **Step 2:** If `docs/superpowers/plans/2026-10-22-ws1-engine-consistency.md` exists, read its `Produces` for `write_state`/`generate_narrowed`; confirm unary `Command` calls still reach the end of `generate_narrowed` with `generated: GeneratedResponse`, `target_id: Option<String>` and `session: &SessionKey` in scope, and that response-cache hits and `Source::State` apply to `OperationKind::Query` only. If a cache hit can short-circuit a `Command`, put the Task 6 hook on that path too.
- [ ] **Step 3:** `ls crates/mb-store/migrations/sqlite`; if `0700_*` exists, use the next free hundred in Task 4 and in the CHANGELOG.
- [ ] **Step 4:** Run `cargo test -p mb-protocols` → PASS (baseline).

---

### Task 1: Signatures

**Files:**
- Create: `crates/mb-protocols/src/webhooks/mod.rs` (module root, `pub mod sign;` etc.), `crates/mb-protocols/src/webhooks/sign.rs`
- Modify: `crates/mb-protocols/src/lib.rs` (`pub mod webhooks;`), `crates/mb-protocols/Cargo.toml` (add `hmac.workspace = true`, `sha2.workspace = true`, move `reqwest.workspace = true` to `[dependencies]`, add `wiremock.workspace = true` to dev-deps), `crates/mb-admin/src/webhook.rs:90-106` (`signature` delegates)
- Test: `crates/mb-protocols/src/webhooks/sign.rs` (`#[cfg(test)] mod tests`)

**Interfaces:**
- Produces:
  - `pub enum Signing { Stripe, HmacSha256, None }` (`Clone, Copy, Debug, PartialEq, Eq`)
  - `pub const STRIPE_SIGNATURE_HEADER: &str = "stripe-signature";`
  - `pub const MOCKINGBIRD_SIGNATURE_HEADER: &str = "x-mockingbird-signature";`
  - `pub fn hmac_sha256_hex(secret: &[u8], message: &[u8]) -> String` (lowercase hex)
  - `pub fn stripe_signature(secret: &[u8], timestamp: i64, body: &[u8]) -> String` → `t=<ts>,v1=<hex hmac(secret, "<ts>.<body>")>`
  - `pub fn mockingbird_signature(secret: &[u8], body: &[u8]) -> String` → `sha256=<hex>`
  - `pub fn signature_header(signing: Signing, secret: &[u8], timestamp: i64, body: &[u8]) -> Option<(&'static str, String)>` (`None` for `Signing::None`)

- [ ] **Step 1: Write the failing tests**

```rust
#[test]
fn hmac_matches_rfc4231_case_2() {
    assert_eq!(
        hmac_sha256_hex(b"Jefe", b"what do ya want for nothing?"),
        "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
    );
}

const BODY: &[u8] = br#"{"id":"evt_1","type":"customer.created"}"#;

#[test]
fn stripe_signature_test_vector() {
    assert_eq!(
        stripe_signature(b"whsec_test_secret", 1_700_000_000, BODY),
        "t=1700000000,v1=2146c3ca883db3d5b33f638513d4322fcf955eeb8624ee63937a742c023fe30b"
    );
}

/// Stripe's documented verification: split on ',', take t= and v1=,
/// recompute HMAC-SHA256(secret, "<t>.<payload>") and compare.
#[test]
fn stripe_signature_verifies_with_stripes_algorithm() {
    let header = stripe_signature(b"whsec_test_secret", 1_700_000_123, BODY);
    let mut t = None;
    let mut v1 = None;
    for part in header.split(',') {
        match part.split_once('=').unwrap() {
            ("t", v) => t = Some(v.to_string()),
            ("v1", v) => v1 = Some(v.to_string()),
            _ => {}
        }
    }
    let signed = [t.unwrap().as_bytes(), b".", BODY].concat();
    assert_eq!(v1.unwrap(), hmac_sha256_hex(b"whsec_test_secret", &signed));
}

#[test]
fn mockingbird_signature_test_vector() {
    assert_eq!(
        mockingbird_signature(b"whsec_test_secret", BODY),
        "sha256=fd41b171babb49da71ea3fab7b7da430fd8fc07d35a82924971cc9d6d758d58a"
    );
}

#[test]
fn signature_header_per_scheme() {
    assert_eq!(signature_header(Signing::None, b"k", 1, b"{}"), None);
    assert_eq!(signature_header(Signing::Stripe, b"k", 1, b"{}").unwrap().0, "stripe-signature");
    assert_eq!(signature_header(Signing::HmacSha256, b"k", 1, b"{}").unwrap().0, "x-mockingbird-signature");
}
```

- [ ] **Step 2:** Run `cargo test -p mb-protocols webhooks::sign` → FAIL (module missing).
- [ ] **Step 3:** Implement the functions in `sign.rs` (`Hmac::<Sha256>::new_from_slice`, an `Err` arm returning an empty string as `mb_admin::webhook::signature` does today; no `expect`). Replace the body of `mb_admin::webhook::signature` with `mb_protocols::webhooks::sign::mockingbird_signature(secret, body)`; drop the now-unused `hmac`/`sha2` imports there (keep the Cargo deps if still used elsewhere in `mb-admin`).
- [ ] **Step 4:** Run `cargo test -p mb-protocols webhooks::sign && cargo test -p mb-admin --test audit` → PASS (the audit webhook tests still verify `sha256=` signatures).
- [ ] **Step 5:** Commit `feat(protocols): webhook signatures (stripe, hmac-sha256)`.

---

### Task 2: Event type and envelope

**Files:**
- Create: `crates/mb-protocols/src/webhooks/event.rs`
- Test: same file, `mod tests`

**Interfaces:**
- Consumes: `mb_mcm::{Operation, OperationKind, Effect, Binding}`.
- Produces:
  - `pub fn event_type(op: &Operation) -> Option<String>` — `None` unless `op.kind` is `OperationKind::Command(_)`.
  - `pub fn singular(word: &str) -> String`
  - `pub fn new_event_id() -> String` (`evt_` + 24 ASCII alphanumerics from `rand`)
  - `pub fn envelope(id: &str, event_type: &str, created: i64, object: Value) -> Value`
  - `pub fn envelope_object(op: &Operation, body: Option<&Value>, target_id: Option<&str>) -> Value`

- [ ] **Step 1: Write the failing tests** (build operations with a small helper `op(method, path, kind, resource_type)` in the test module)

```rust
#[test]
fn event_types_follow_resource_and_effect() {
    assert_eq!(event_type(&op("post", "/v1/customers", Effect::Create, Some("customers"))).as_deref(), Some("customer.created"));
    assert_eq!(event_type(&op("post", "/v1/payment_intents", Effect::Create, Some("payment_intents"))).as_deref(), Some("payment_intent.created"));
    // Stripe updates are POST /x/{id}: Effect::Other.
    assert_eq!(event_type(&op("post", "/v1/customers/{customer}", Effect::Other, Some("customers"))).as_deref(), Some("customer.updated"));
    assert_eq!(event_type(&op("put", "/orders/{orderId}", Effect::Update, Some("orders"))).as_deref(), Some("order.updated"));
    assert_eq!(event_type(&op("delete", "/orders/{orderId}", Effect::Delete, Some("orders"))).as_deref(), Some("order.deleted"));
    // Action sub-path: last static segment followed by a param.
    assert_eq!(event_type(&op("post", "/v1/payment_intents/{intent}/confirm", Effect::Create, Some("confirm"))).as_deref(), Some("confirm.created"));
}

#[test]
fn queries_have_no_event_type() {
    let mut get = op("get", "/orders", Effect::Create, Some("orders"));
    get.kind = OperationKind::Query;
    assert_eq!(event_type(&get), None);
}

#[test]
fn singularises() {
    for (plural, one) in [("orders", "order"), ("categories", "category"), ("addresses", "address"),
                          ("boxes", "box"), ("branches", "branch"), ("status", "status"), ("class", "class"), ("data", "data")] {
        assert_eq!(singular(plural), one, "{plural}");
    }
}

#[test]
fn envelope_is_stripe_shaped() {
    let e = envelope("evt_1", "order.created", 1_700_000_000, json!({"id": "o1"}));
    assert_eq!(e, json!({"id": "evt_1", "object": "event", "type": "order.created",
                         "created": 1_700_000_000, "data": {"object": {"id": "o1"}}}));
    let id = new_event_id();
    assert!(id.starts_with("evt_") && id.len() == 28 && id[4..].chars().all(|c| c.is_ascii_alphanumeric()));
}

#[test]
fn bodyless_delete_object() {
    let del = op("delete", "/orders/{orderId}", Effect::Delete, Some("orders"));
    assert_eq!(envelope_object(&del, None, Some("o1")), json!({"id": "o1", "deleted": true}));
    let upd = op("put", "/orders/{orderId}", Effect::Update, Some("orders"));
    assert_eq!(envelope_object(&upd, None, Some("o1")), json!({}));
    assert_eq!(envelope_object(&upd, Some(&json!({"id": "o1"})), Some("o1")), json!({"id": "o1"}));
}
```

Note: the `confirm` case pins that the "last static segment followed by a param" rule applies only when the path **ends** in a param; otherwise `op.resource_type` is used. Non-HTTP bindings use `op.resource_type`, else `op.id`.

- [ ] **Step 2:** `cargo test -p mb-protocols webhooks::event` → FAIL.
- [ ] **Step 3:** Implement in `event.rs` per the Decisions table. `"status"`, `"class"` and words ending `ss`/`us` stay; `"data"` (no trailing `s`) stays.
- [ ] **Step 4:** `cargo test -p mb-protocols webhooks::event` → PASS.
- [ ] **Step 5:** Commit `feat(protocols): webhook event types and envelope`.

---

### Task 3: OpenAPI `webhooks` and `callbacks` in the bundle

**Files:**
- Modify: `crates/mb-mcm/src/lib.rs` (new `WebhookDef`, `Bundle.webhooks`), every `Bundle { … }` literal the compiler lists (≈24 files: `crates/mb-import/src/{openapi3,swagger2,graphql}.rs`, `crates/mb-import/src/{asyncapi/v2,asyncapi/v3,proto/mod,wsdl/mod}.rs`, tests in `mb-import`, `mb-engine`, `mb-protocols`, `mb-store`, `mb-mcm`) gain `webhooks: Vec::new()`
- Modify: `crates/mb-import/src/openapi3.rs` (`compile` collects them; new `fn compile_webhooks(doc: &Value, defs: &Map<String, Value>, operations: &[Operation]) -> Result<Vec<WebhookDef>, ImportError>` reading top-level `webhooks` and each operation's `callbacks`)
- Create: `fixtures/openapi/webhooks-3.1.yaml` — `Pets API` with `POST /pets` (`createPet`, 201 `Pet`), a top-level `webhooks: {pet.created: {post: {requestBody: {content: {application/json: {schema: {$ref: '#/components/schemas/PetEvent'}}}}}}}` where `PetEvent` is `{type: object, required: [event, pet], properties: {event: {const: pet.created}, pet: {$ref: '#/components/schemas/Pet'}}}`, and on `createPet` a `callbacks: {onAdopted: {'{$request.body#/callbackUrl}': {post: {requestBody: … schema {type: object, required: [adopted], properties: {adopted: {const: true}}}}}}}`
- Test: `crates/mb-import/tests/openapi3.rs`

**Interfaces:**
- Produces (in `mb_mcm`):

```rust
/// An outbound event the contract describes: OpenAPI 3.1 `webhooks` or an
/// operation's `callbacks`.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(rename_all = "snake_case")]
pub struct WebhookDef {
    /// The `webhooks` key, or `<operationId>.<callbackName>`.
    pub name: String,
    /// For a callback, the operation that declares it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub operation: Option<String>,
    /// JSON Schema 2020-12 of the request body (refs as in `defs`).
    pub payload_schema: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
}
// Bundle gains:
#[serde(default, skip_serializing_if = "Vec::is_empty")]
pub webhooks: Vec<WebhookDef>,
// and
impl Bundle { pub fn webhook_for(&self, op_id: &str, event_type: &str) -> Option<&WebhookDef>; }
```

`webhook_for`: first def with `operation == Some(op_id)`; else first def whose `name` equals `event_type` or `op_id`, ASCII case-insensitive.

- [ ] **Step 1: Write the failing tests**

```rust
#[test]
fn compiles_31_webhooks_and_callbacks() {
    let b = compile(&fixture("webhooks-3.1.yaml")).unwrap();
    assert_eq!(b.webhooks.len(), 2);
    let hook = b.webhooks.iter().find(|w| w.name == "pet.created").unwrap();
    assert_eq!(hook.operation, None);
    assert_eq!(hook.payload_schema, json!({"$ref": "#/$defs/PetEvent"}));
    let cb = b.webhooks.iter().find(|w| w.name == "createPet.onAdopted").unwrap();
    assert_eq!(cb.operation.as_deref(), Some("createPet"));
    assert_eq!(cb.payload_schema["properties"]["adopted"]["const"], json!(true));
    // Callback first, then name match.
    assert_eq!(b.webhook_for("createPet", "pet.created").unwrap().name, "createPet.onAdopted");
    assert_eq!(b.webhook_for("other", "PET.CREATED").unwrap().name, "pet.created");
    assert!(b.webhook_for("other", "pet.deleted").is_none());
}

#[test]
fn contracts_without_webhooks_serialise_unchanged() {
    let b = orders30();
    assert!(b.webhooks.is_empty());
    assert!(serde_json::to_value(&b).unwrap().get("webhooks").is_none());
}

#[test]
fn webhook_payload_refs_are_checked() {
    // webhooks-3.1.yaml with PetEvent's $ref pointing at a missing schema
    let src = std::str::from_utf8(&fixture("webhooks-3.1.yaml")).unwrap()
        .replace("#/components/schemas/PetEvent", "#/components/schemas/Missing");
    assert!(matches!(compile(src.as_bytes()), Err(ImportError::Invalid { .. })));
}
```

(Use the `$ref` form `normalize_schema` produces for component refs; read one existing operation's compiled schema to confirm `#/$defs/<name>` before pinning it.)

- [ ] **Step 2:** `cargo test -p mb-import --test openapi3` → FAIL (no field `webhooks`).
- [ ] **Step 3:** Add `WebhookDef`, the field and `webhook_for`; add `webhooks: Vec::new()` to every literal `cargo build --workspace --all-targets` reports. In `openapi3::compile`, after operations, build defs with `compile_body` on each method of each `webhooks` path item (key order; one def per path item, the first method with a JSON body) and of each callback expression (first expression, first method); check refs with `check_refs`. Swagger 2 has neither.
- [ ] **Step 4:** `cargo test -p mb-import && cargo test -p mb-mcm && cargo build --workspace --all-targets` → PASS.
- [ ] **Step 5:** Commit `feat(import): compile OpenAPI webhooks and callbacks`.

---

### Task 4: Stored webhook targets

**Files:**
- Modify: `crates/mb-store/src/model.rs` (types; `Mock.webhooks`, `NewMock.webhooks`), `crates/mb-store/src/lib.rs:473-481` (`validate_mock_settings` also validates webhooks), `crates/mb-store/src/sqlite.rs` (`MOCK_COLUMNS`, `mock_from_row`, `create_mock`, `update_mock`), `crates/mb-store/src/postgres.rs` (same)
- Create: `crates/mb-store/migrations/sqlite/0700_mock_webhooks.sql` (`ALTER TABLE mocks ADD COLUMN webhooks TEXT;`), `crates/mb-store/migrations/postgres/0700_mock_webhooks.sql` (`ALTER TABLE mocks ADD COLUMN webhooks JSONB;`)
- Modify: every `Mock { … }` / `NewMock { … }` literal the compiler lists (add `webhooks: Vec::new()`)
- Test: `crates/mb-store/tests/common/conformance.rs` (add to `run_all`), `crates/mb-store/src/lib.rs` unit tests

**Interfaces:**
- Produces (in `mb_store`, re-exported from the crate root):

```rust
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub enum WebhookSigning {
    #[serde(rename = "stripe")] Stripe,
    #[serde(rename = "hmac-sha256")] HmacSha256,
    #[serde(rename = "none")] None,
}
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct WebhookTarget {
    pub url: String,
    /// Operation ids, event types or "*"; default ["*"].
    #[serde(default = "WebhookTarget::all_events")]
    pub events: Vec<String>,
    /// A secret reference (env:, file:, vault:), never a value.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub secret: Option<String>,
    /// Default: hmac-sha256 with a secret, none without.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signing: Option<WebhookSigning>,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub delay_ms: u64,
}
impl WebhookTarget {
    pub const MAX_TARGETS: usize = 16;
    pub const MAX_DELAY_MS: u64 = 60_000;
    pub fn all_events() -> Vec<String>;            // vec!["*"]
    pub fn effective_signing(&self) -> WebhookSigning;
    pub fn validate(&self) -> Result<(), String>;   // messages prefixed "webhooks[i]." by the caller
}
pub fn validate_webhooks(targets: &[WebhookTarget]) -> Result<(), String>;
// Mock and NewMock gain:
#[serde(default, skip_serializing_if = "Vec::is_empty")]
pub webhooks: Vec<WebhookTarget>,
```

- [ ] **Step 1: Write the failing tests**

```rust
// crates/mb-store/src/lib.rs tests
#[test]
fn webhook_target_validation() {
    let ok = WebhookTarget { url: "https://example.test/hook".into(), events: vec!["*".into()],
        secret: Some("env:STRIPE_WEBHOOK_SECRET".into()), signing: Some(WebhookSigning::Stripe), delay_ms: 0 };
    assert_eq!(validate_webhooks(std::slice::from_ref(&ok)), Ok(()));
    let bad = |f: &dyn Fn(&mut WebhookTarget)| { let mut t = ok.clone(); f(&mut t); validate_webhooks(&[t]).unwrap_err() };
    assert!(bad(&|t| t.url = "ftp://x/y".into()).contains("webhooks[0].url"));
    assert!(bad(&|t| t.url = "/relative".into()).contains("webhooks[0].url"));
    assert!(bad(&|t| t.events.clear()).contains("webhooks[0].events"));
    assert!(bad(&|t| t.secret = Some("whsec_plainvalue".into())).contains("secret reference"));
    // The rejection never echoes a pasted value.
    assert!(!bad(&|t| t.secret = Some("whsec_plainvalue".into())).contains("whsec_plainvalue"));
    assert!(bad(&|t| t.secret = None).contains("webhooks[0].signing"));
    assert!(bad(&|t| t.delay_ms = 60_001).contains("webhooks[0].delay_ms"));
    assert!(validate_webhooks(&vec![ok.clone(); 17]).unwrap_err().contains("at most 16"));
    assert_eq!(WebhookTarget { secret: None, signing: None, ..ok.clone() }.effective_signing(), WebhookSigning::None);
    assert_eq!(WebhookTarget { signing: None, ..ok }.effective_signing(), WebhookSigning::HmacSha256);
}

// conformance.rs
async fn mock_webhooks_round_trip(store: &Arc<dyn ControlStore>) {
    let mut nm = new_mock("hooks");
    nm.webhooks = vec![WebhookTarget { url: "http://127.0.0.1:4242/hook".into(),
        events: vec!["createOrder".into()], secret: Some("env:SHOP_WEBHOOK_SECRET".into()),
        signing: Some(WebhookSigning::Stripe), delay_ms: 250 }];
    let m = store.create_mock(DEFAULT_WORKSPACE, nm.clone()).await.unwrap();
    assert_eq!(store.get_mock(DEFAULT_WORKSPACE, "hooks").await.unwrap().unwrap().webhooks, nm.webhooks);
    let mut cleared = m.clone();
    cleared.webhooks.clear();
    assert!(store.update_mock(&cleared).await.unwrap().webhooks.is_empty());
    // A mock without webhooks serialises without the key.
    assert!(serde_json::to_value(&store.get_mock(DEFAULT_WORKSPACE, "hooks").await.unwrap().unwrap())
        .unwrap().get("webhooks").is_none());
    let mut bad = new_mock("hooks-bad");
    bad.webhooks = vec![WebhookTarget { url: "nope".into(), ..nm.webhooks[0].clone() }];
    assert!(matches!(store.create_mock(DEFAULT_WORKSPACE, bad).await, Err(StoreError::Invalid(_))));
}
```

- [ ] **Step 2:** `cargo test -p mb-store` → FAIL (types missing).
- [ ] **Step 3:** Implement. Secret-reference check: `env:`/`file:`/`vault:` prefix with a non-empty, non-whitespace remainder; message `"webhooks[i].secret must be a secret reference (env:NAME, file:/path or vault:path#key), not a value"`. `validate_mock_settings(chaos, limits, webhooks)` gains the slice. NULL column reads as `Vec::new()`.
- [ ] **Step 4:** `cargo test -p mb-store && scripts/test-postgres.sh` → PASS (also `cargo test -p mb-store --test upgrade`).
- [ ] **Step 5:** Commit `feat(store): webhook targets on mocks (migration 0700_mock_webhooks)`.

---

### Task 5: Dispatcher and delivery log

**Files:**
- Create: `crates/mb-protocols/src/webhooks/log.rs`, `crates/mb-protocols/src/webhooks/deliver.rs`
- Test: `crates/mb-protocols/tests/webhooks.rs` (wiremock receivers), unit tests in `log.rs`

**Interfaces:**
- Consumes: Task 1 `Signing`, `signature_header`.
- Produces:

```rust
// log.rs
pub const DELIVERY_LOG_CAPACITY: usize = 200;
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)] #[serde(rename_all = "snake_case")]
pub enum DeliveryStatus { Pending, Delivered, Failed, Dropped }
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct DeliveryRecord {
    pub id: String,             // "whd_" + 16 alnum, one per (event, target)
    pub event_id: String,
    pub event_type: String,
    pub operation: String,
    pub session: String,
    pub url: String,            // redacted_url(target url)
    pub status: DeliveryStatus,
    pub attempts: u32,
    pub response_status: Option<u16>,
    pub error: Option<String>,
    pub created_at: std::time::SystemTime,
    pub finished_at: Option<std::time::SystemTime>,
}
#[derive(Default, Debug)]
pub struct DeliveryLog { /* Mutex<HashMap<(String, String), VecDeque<DeliveryRecord>>> */ }
pub type SharedDeliveryLog = Arc<DeliveryLog>;
impl DeliveryLog {
    pub fn push(&self, workspace: &str, mock: &str, record: DeliveryRecord);
    pub fn update(&self, workspace: &str, mock: &str, id: &str, f: impl FnOnce(&mut DeliveryRecord));
    /// Newest first, at most `limit`.
    pub fn list(&self, workspace: &str, mock: &str, limit: usize) -> Vec<DeliveryRecord>;
}
pub fn redacted_url(url: &str) -> String;

// deliver.rs
pub struct ResolvedTarget {
    pub url: String,
    pub events: Vec<String>,
    pub signing: Signing,
    /// Ok(secret bytes) or Err(reason without the value); Debug prints "<redacted>".
    pub secret: Result<Option<SecretBytes>, String>,
    pub delay: Duration,
}
pub struct SecretBytes(/* Arc<[u8]> */);           // Debug → "<redacted>"
impl SecretBytes { pub fn new(value: String) -> Self; }
#[derive(Clone, Debug)]
pub struct DispatchSettings { pub backoff_base: Duration, pub max_pending: usize, pub attempt_timeout: Duration }
impl Default for DispatchSettings { /* 1 s, 256, 10 s */ }
pub const MAX_RETRIES: u32 = 3;
pub struct WebhookDispatcher { /* workspace, mock, targets, http, log, settings, pending: Arc<Semaphore> */ }
pub struct OutboundEvent { pub event_id: String, pub event_type: String, pub operation: String, pub session: String, pub body: Vec<u8> }
impl WebhookDispatcher {
    pub fn new(workspace: String, mock: String, targets: Vec<ResolvedTarget>,
               http: reqwest::Client, log: SharedDeliveryLog, settings: DispatchSettings) -> Self;
    /// Targets whose `events` match `operation` or `event_type` (or "*").
    pub fn matching(&self, operation: &str, event_type: &str) -> Vec<usize>;
    /// Records one Pending delivery per listed target and spawns its delivery;
    /// never awaits the network. Must run inside a Tokio runtime.
    pub fn send(self: &Arc<Self>, event: OutboundEvent, targets: &[usize]);
}
```

Delivery loop per target: sleep `delay`; if `secret` is `Err(reason)` record `Failed`, `attempts: 0`, `error: Some(reason)` and return; up to `1 + MAX_RETRIES` attempts, sleeping `backoff_base * 2^(n-1)` before retry `n`; each attempt re-signs with the current unix time; 2xx → `Delivered`. `try_acquire_owned` on the semaphore before spawning; none free → `Dropped` + `metrics::counter!("mb_webhook_dropped_total")`; final failure → `metrics::counter!("mb_webhook_failed_total")`; success → `mb_webhook_delivered_total`.

- [ ] **Step 1: Write the failing tests** (`crates/mb-protocols/tests/webhooks.rs`; helper `dispatcher(server_uri, targets, settings) -> (Arc<WebhookDispatcher>, SharedDeliveryLog)` and `wait_for(log, pred)` polling every 10 ms up to 5 s)

```rust
fn fast() -> DispatchSettings {
    DispatchSettings { backoff_base: Duration::from_millis(10), ..Default::default() }
}

#[tokio::test]
async fn delivers_signed_body_once() {
    let server = MockServer::start().await;
    Mock::given(method("POST")).and(path("/hook")).respond_with(ResponseTemplate::new(200))
        .expect(1).mount(&server).await;
    let (d, log) = dispatcher(&format!("{}/hook", server.uri()),
        Signing::Stripe, Some("whsec_test_secret"), fast());
    d.send(event("evt_1", br#"{"a":1}"#), &[0]);
    let r = wait_for(&log, |r| r.status == DeliveryStatus::Delivered).await;
    assert_eq!(r.attempts, 1);
    assert_eq!(r.response_status, Some(200));
    let req = &server.received_requests().await.unwrap()[0];
    assert_eq!(req.body, br#"{"a":1}"#);
    assert_eq!(req.headers["content-type"], "application/json");
    assert_eq!(req.headers["x-mockingbird-event-id"], "evt_1");
    let sig = req.headers["stripe-signature"].to_str().unwrap();
    let t: i64 = sig.split(',').next().unwrap().strip_prefix("t=").unwrap().parse().unwrap();
    assert_eq!(sig, stripe_signature(b"whsec_test_secret", t, br#"{"a":1}"#));
}

#[tokio::test]
async fn retries_on_500_then_succeeds() {
    let server = MockServer::start().await;
    Mock::given(method("POST")).respond_with(ResponseTemplate::new(500)).up_to_n_times(2).mount(&server).await;
    Mock::given(method("POST")).respond_with(ResponseTemplate::new(204)).mount(&server).await;
    let (d, log) = dispatcher(&server.uri(), Signing::None, None, fast());
    d.send(event("evt_2", b"{}"), &[0]);
    let r = wait_for(&log, |r| r.status == DeliveryStatus::Delivered).await;
    assert_eq!(r.attempts, 3);
}

#[tokio::test]
async fn gives_up_after_three_retries() {
    let server = MockServer::start().await;
    Mock::given(method("POST")).respond_with(ResponseTemplate::new(503)).expect(4).mount(&server).await;
    let (d, log) = dispatcher(&server.uri(), Signing::None, None, fast());
    d.send(event("evt_3", b"{}"), &[0]);
    let r = wait_for(&log, |r| r.status == DeliveryStatus::Failed).await;
    assert_eq!((r.attempts, r.response_status), (4, Some(503)));
}

#[test]
fn default_settings_are_pinned() {
    let s = DispatchSettings::default();
    assert_eq!((s.backoff_base, s.max_pending, s.attempt_timeout),
               (Duration::from_secs(1), 256, Duration::from_secs(10)));
    let delays: Vec<_> = (1..=MAX_RETRIES).map(|n| retry_delay(&s, n)).collect();
    assert_eq!(delays, [1, 2, 4].map(Duration::from_secs));
}

#[tokio::test]
async fn unresolved_secret_is_never_sent() {
    let server = MockServer::start().await;
    Mock::given(method("POST")).respond_with(ResponseTemplate::new(200)).expect(0).mount(&server).await;
    let (d, log) = dispatcher_with_secret(&server.uri(),
        Err("secret reference env:MISSING_WEBHOOK_SECRET could not be resolved".into()), fast());
    d.send(event("evt_4", b"{}"), &[0]);
    let r = wait_for(&log, |r| r.status == DeliveryStatus::Failed).await;
    assert_eq!(r.attempts, 0);
    assert!(r.error.unwrap().contains("env:MISSING_WEBHOOK_SECRET"));
}

#[tokio::test]
async fn drops_beyond_max_pending() {
    let server = MockServer::start().await;
    Mock::given(method("POST")).respond_with(ResponseTemplate::new(200).set_delay(Duration::from_secs(1)))
        .mount(&server).await;
    let (d, log) = dispatcher(&server.uri(), Signing::None, None,
        DispatchSettings { max_pending: 1, ..fast() });
    d.send(event("evt_5", b"{}"), &[0]);
    d.send(event("evt_6", b"{}"), &[0]);
    let all = log.list("default", "m", 10);
    assert_eq!(all[0].event_id, "evt_6");
    assert_eq!(all[0].status, DeliveryStatus::Dropped);
    assert_eq!(all[1].status, DeliveryStatus::Pending);
}

#[test]
fn log_is_bounded_newest_first_and_redacted() {
    let log = DeliveryLog::default();
    for i in 0..205 { log.push("default", "m", record(&format!("whd_{i}"))); }
    let all = log.list("default", "m", 500);
    assert_eq!(all.len(), 200);
    assert_eq!(all[0].id, "whd_204");
    assert!(log.list("default", "other", 10).is_empty());
    assert_eq!(redacted_url("https://user:pass@hooks.test:8443/a/b?token=x#f"), "https://hooks.test:8443/a/b");
    assert_eq!(redacted_url("not a url"), "<invalid url>");
    assert_eq!(format!("{:?}", SecretBytes::new("whsec_test_secret".into())), "<redacted>");
}
```

`retry_delay(settings: &DispatchSettings, retry: u32) -> Duration` is `pub` in `deliver.rs` (`backoff_base * 2^(retry-1)`, saturating). The test helper's dispatcher uses workspace `default`, mock `m`.

- [ ] **Step 2:** `cargo test -p mb-protocols --test webhooks` → FAIL.
- [ ] **Step 3:** Implement `log.rs` and `deliver.rs` with the signatures above (`reqwest::Url` for `redacted_url`; `std::sync::Mutex`, lock poisoning handled with `into_inner` on the error, no `unwrap`).
- [ ] **Step 4:** `cargo test -p mb-protocols --test webhooks` → PASS.
- [ ] **Step 5:** Commit `feat(protocols): webhook dispatcher with retries and delivery log`.

---

### Task 6: Post-write hook in `call.rs`

**Files:**
- Create: `crates/mb-protocols/src/webhooks/emit.rs`
- Modify: `crates/mb-protocols/src/lib.rs:221-264,297-326` (`MockRuntime.webhooks`, initialised `None` in `new`), `crates/mb-protocols/src/call.rs` (`generate_narrowed`, after the `write_state`/`touch` block)
- Test: `crates/mb-protocols/tests/webhooks.rs` (REST end-to-end on `fixtures/openapi/orders-3.0.yaml` and `webhooks-3.1.yaml`, offline engine as in `tests/call.rs`)

**Interfaces:**
- Consumes: Task 2 `event_type`, `envelope`, `envelope_object`, `new_event_id`; Task 3 `Bundle::webhook_for`; Task 5 `WebhookDispatcher::{matching, send}`, `OutboundEvent`; `mb_engine::{Engine::generate_message, MessageContext}`.
- Produces:
  - `MockRuntime { pub webhooks: Option<Arc<WebhookDispatcher>>, … }`
  - `pub(crate) fn emit(rt: &MockRuntime, op: &Operation, call: &OperationCall, generated: &GeneratedResponse, target_id: Option<&str>, session: &SessionKey)` in `emit.rs` — returns at once when `rt.webhooks` is `None`, the status is not 2xx, `event_type(op)` is `None` or no target matches; otherwise spawns one task that builds the body (contract payload via `generate_message` on the synthetic channel from the Decisions table, `MessageContext { trigger: json!({"kind": "webhook", "operation": op.id, "type": event_type}), context: Some(json!({"request": call.body, "response": generated.body})), received: None, related: vec![] }`; else the envelope), serialises it once and calls `send`.
  - Hook site in `generate_narrowed`: `crate::webhooks::emit::emit(rt, op, call, &generated, target_id.as_deref(), session);` placed after the state block, outside `write_state` (so WS1 list seeding never fires events). `target_id` must be cloned before it is moved into `write_state`.

- [ ] **Step 1: Write the failing tests** (helper `rest(fixture, targets) -> (Router, SharedDeliveryLog)` building a `MockRuntime` with `webhooks: Some(Arc::new(WebhookDispatcher::new(…, fast())))`)

```rust
#[tokio::test]
async fn create_posts_envelope() {
    let server = receiver(200).await;
    let (router, log) = rest("orders-3.0.yaml", vec![target(&server, &["*"])]);
    let resp = router.oneshot(post_json("/orders", json!({"item": "x", "quantity": 1}))).await.unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    let created: Value = body_json(resp).await;
    wait_for(&log, |r| r.status == DeliveryStatus::Delivered).await;
    let sent: Value = serde_json::from_slice(&server.received_requests().await.unwrap()[0].body).unwrap();
    assert_eq!(sent["type"], "order.created");
    assert_eq!(sent["object"], "event");
    assert_eq!(sent["data"]["object"], created);
    assert!(sent["id"].as_str().unwrap().starts_with("evt_"));
}

#[tokio::test]
async fn reads_errors_and_forced_failures_fire_nothing() {
    let server = receiver(200).await;
    let (router, log) = rest("orders-3.0.yaml", vec![target(&server, &["*"])]);
    router.clone().oneshot(get("/orders")).await.unwrap();
    router.clone().oneshot(get("/orders/o1")).await.unwrap();
    router.clone().oneshot(post_json("/orders", json!({"bad": true}))).await.unwrap();   // 4xx validation
    router.clone().oneshot(post_json_with("/orders", json!({"item": "x", "quantity": 1}),
        "x-mockingbird-status", "500")).await.unwrap();
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(log.list("default", "orders", 10).is_empty());
    assert!(server.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn delete_without_body_carries_id() {
    let server = receiver(200).await;
    let (router, log) = rest("orders-3.0.yaml", vec![target(&server, &["deleteOrder"])]);
    let resp = router.oneshot(delete("/orders/o42")).await.unwrap();
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);
    wait_for(&log, |r| r.status == DeliveryStatus::Delivered).await;
    let sent: Value = serde_json::from_slice(&server.received_requests().await.unwrap()[0].body).unwrap();
    assert_eq!(sent["type"], "order.deleted");
    assert_eq!(sent["data"]["object"], json!({"id": "o42", "deleted": true}));
}

#[tokio::test]
async fn events_filter_by_operation_or_type() {
    let server = receiver(200).await;
    let (router, log) = rest("orders-3.0.yaml",
        vec![target(&server, &["order.updated"]), target(&server, &["deleteOrder"])]);
    router.oneshot(post_json("/orders", json!({"item": "x", "quantity": 1}))).await.unwrap();
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(log.list("default", "orders", 10).is_empty());
}

#[tokio::test]
async fn openapi_webhook_payload_used_when_present() {
    let server = receiver(200).await;
    let (router, log) = rest("webhooks-3.1.yaml", vec![target(&server, &["*"])]);
    router.oneshot(post_json("/pets", json!({"name": "Rex"}))).await.unwrap();
    wait_for(&log, |r| r.status == DeliveryStatus::Delivered).await;
    let sent: Value = serde_json::from_slice(&server.received_requests().await.unwrap()[0].body).unwrap();
    // createPet declares a callback, which wins over the `pet.created` webhook.
    assert_eq!(sent["adopted"], json!(true));
    assert!(sent.get("data").is_none());
}

#[tokio::test]
async fn slow_receiver_does_not_delay_the_response() {
    let server = MockServer::start().await;
    Mock::given(method("POST")).respond_with(ResponseTemplate::new(200).set_delay(Duration::from_secs(3)))
        .mount(&server).await;
    let (router, _log) = rest("orders-3.0.yaml", vec![target(&server, &["*"])]);
    let started = std::time::Instant::now();
    router.oneshot(post_json("/orders", json!({"item": "x", "quantity": 1}))).await.unwrap();
    assert!(started.elapsed() < Duration::from_secs(1));
}
```

(Check `orders-3.0.yaml` for `createOrder`'s required body fields and adjust the JSON in `post_json` to a valid one before running.)

- [ ] **Step 2:** `cargo test -p mb-protocols --test webhooks` → FAIL (`webhooks` field missing).
- [ ] **Step 3:** Implement `emit.rs`, the field and the hook. The created timestamp is unix seconds from `SystemTime::now()`.
- [ ] **Step 4:** `cargo test -p mb-protocols && cargo check -p mockingbird --no-default-features` → PASS.
- [ ] **Step 5:** Commit `feat(protocols): send webhooks after successful writes`.

---

### Task 7: Config file, runtime wiring and docs

**Files:**
- Modify: `crates/mockingbird/src/file_config.rs` (`FileMock.webhooks: Vec<FileWebhook>`, `FileMock::new`, validation alongside `chaos` at ~1244), `crates/mockingbird/src/reconcile.rs:~515,~532` (`webhooks: p.mock.webhooks.iter().map(FileWebhook::to_store).collect()`), `crates/mockingbird/src/runtime.rs` (`RuntimeDeps.webhook_log: SharedDeliveryLog`; `build_runtime_with_budget` builds the dispatcher), every `RuntimeDeps { … }` literal (14, mostly in `crates/mockingbird/tests/`; add `webhook_log: Default::default()`), `crates/mockingbird/src/config.rs` (create the log once, put it in `RuntimeDeps`)
- Modify: `docs/configuration.md` (new `### mocks.<name>.webhooks` section after `mocks.<name>.chaos`), `docs/examples/mockingbird.full.yaml` (a `webhooks:` list on the example mock with every key)
- Create: `docs/webhooks.md` (configuration, event types, envelope vs contract payload, signatures and how to verify, retries, `webhooks sign`, delivery listing, split-role limitation); link it from `README.md`'s docs list
- Test: `crates/mockingbird/src/file_config.rs` tests, `crates/mockingbird/tests/webhooks_e2e.rs`, existing `crates/mockingbird/tests/docs_reference.rs`

**Interfaces:**
- Consumes: Task 4 `mb_store::{WebhookTarget, WebhookSigning, validate_webhooks}`; Task 5 `ResolvedTarget`, `SecretBytes`, `DispatchSettings`, `WebhookDispatcher::new`, `SharedDeliveryLog`; `mb_llm::http_client(&deps.http, None)`; `deps.ext.secrets.resolve`.
- Produces:
  - `pub struct FileWebhook { pub url: String, #[serde(default = "WebhookTarget::all_events")] pub events: Vec<String>, #[serde(default)] pub secret: Option<String>, #[serde(default)] pub signing: Option<WebhookSigning>, #[serde(default)] pub delay_ms: u64 }` (`deny_unknown_fields`) with `pub fn to_store(&self) -> WebhookTarget`
  - `pub(crate) async fn webhook_dispatcher(deps: &RuntimeDeps, mock: &Mock) -> anyhow::Result<Option<Arc<WebhookDispatcher>>>` in `runtime.rs` — `None` when `mock.webhooks` is empty; a failed `resolve` becomes `Err("secret reference <ref> could not be resolved")` on that target (logged at `warn` with the mock and reference, never the value); a failed client build is an error.
  - `RuntimeDeps { pub webhook_log: SharedDeliveryLog, … }`

- [ ] **Step 1: Write the failing tests**

```rust
// file_config.rs tests
#[test]
fn webhooks_parse_and_validate() {
    let cfg: FileConfig = serde_yaml::from_str(r#"
mocks:
  shop:
    contract: shop.yaml
    webhooks:
      - url: http://localhost:4242/webhook
        events: [createCustomer, "customer.updated"]
        secret: env:SHOP_WEBHOOK_SECRET
        signing: stripe
        delay_ms: 100
"#).unwrap();
    let w = &cfg.mocks["shop"].webhooks[0];
    assert_eq!(w.to_store().effective_signing(), WebhookSigning::Stripe);
    assert_eq!(cfg.validate(), Ok(()));
    let bad: FileConfig = serde_yaml::from_str(
        "mocks:\n  shop:\n    contract: s.yaml\n    webhooks: [{url: http://x/h, signing: stripe}]\n").unwrap();
    assert!(bad.validate().unwrap_err().contains("mocks.shop.webhooks[0].signing"));
    assert!(serde_yaml::from_str::<FileConfig>(
        "mocks:\n  shop:\n    contract: s.yaml\n    webhooks: [{url: http://x/h, sekret: env:A}]\n").is_err());
}

// tests/webhooks_e2e.rs — serve a config whose mock (orders-3.0.yaml) has
// webhooks to a wiremock receiver, signing stripe, secret env:WS6_TEST_WEBHOOK_SECRET
// set to "whsec_test_secret" for the test process.
#[tokio::test]
async fn served_mock_delivers_stripe_signed_event() {
    // POST /orders on the data plane → receiver gets one request whose
    // stripe-signature equals stripe_signature(b"whsec_test_secret", t, body)
    // and whose JSON "type" is "order.created".
}

#[tokio::test]
async fn unset_secret_keeps_serving_and_records_failure() {
    // secret env:WS6_UNSET_WEBHOOK_SECRET (never set): POST /orders → 201;
    // deps.webhook_log lists one Failed record, attempts 0, error contains
    // "env:WS6_UNSET_WEBHOOK_SECRET"; the receiver got nothing.
}
```

(Build the server with the same helpers `crates/mockingbird/tests/registry.rs` uses: `deps(store)` + `build_runtime`.)

- [ ] **Step 2:** `cargo test -p mockingbird webhooks` → FAIL.
- [ ] **Step 3:** Implement; then document every key (`mocks.<name>.webhooks`, `…webhooks[]`, `…webhooks[].url|events|secret|signing|delay_ms`) in `docs/configuration.md` using the existing table columns, and set each in the full example.
- [ ] **Step 4:** `cargo test -p mockingbird --test webhooks_e2e --test docs_reference && cargo test -p mockingbird --lib file_config` → PASS.
- [ ] **Step 5:** Commit `feat(serve): configure mock webhooks in mockingbird.yaml`.

---

### Task 8: Admin API (targets on mocks, delivery listing)

**Files:**
- Modify: `crates/mb-admin/src/mocks.rs` (`MockView`, `CreateMock`, `PatchMock` gain `webhooks`; `create`/`patch` check refs), `crates/mb-admin/src/lib.rs` (`AdminState.api_webhook_secret_refs: Arc<Vec<String>>`, `pub const DEFAULT_API_WEBHOOK_SECRET_REFS: [&str; 2] = ["env:*_WEBHOOK_SECRET", "env:MB_WEBHOOK_*"]`, merge `webhook_deliveries::routes()` next to `channels::routes()`), `crates/mb-admin/src/openapi.rs` (merge `WebhookDeliveriesDoc`, schema `WebhookTargetSchema`), `crates/mb-admin/src/profiles.rs` (make `accepted_secret_refs` reusable; it already is `pub`)
- Create: `crates/mb-admin/src/webhook_deliveries.rs`
- Modify: `crates/mockingbird/src/file_config.rs` (`server.api_webhook_secret_refs`, default from the constant), `crates/mockingbird/src/config.rs:~657` (pass it; `.layer(axum::Extension::<SharedDeliveryLog>(webhook_log.clone()))` next to the `SharedChannelControl` layer), `docs/configuration.md` + `docs/examples/mockingbird.full.yaml` (`server.api_webhook_secret_refs`)
- Modify: `crates/mb-console/web/src/api/schema.d.ts` (regenerated only)
- Test: `crates/mb-admin/tests/mocks.rs`, new `crates/mb-admin/tests/webhook_deliveries.rs`

**Interfaces:**
- Consumes: Task 4 `WebhookTarget`, `validate_webhooks`; Task 5 `SharedDeliveryLog`, `DeliveryRecord`, `DeliveryStatus`.
- Produces:
  - `GET /api/v1/workspaces/{ws}/mocks/{mock}/webhooks/deliveries?limit=` (alias `/api/v1/mocks/{mock}/webhooks/deliveries`), `Permission::Read`, `limit` 1..=200 default 50 (422 problem+json outside), 404 for an unknown mock, `[]` without the extension. OpenAPI operation id `webhooks_list_deliveries`.
  - `#[derive(Serialize, ToSchema)] pub struct WebhookDeliveryView { id, event_id, event_type, operation, session, url: String, status: String /* pending|delivered|failed|dropped */, attempts: u32, response_status: Option<u16>, error: Option<String>, created_at: DateTime<Utc>, finished_at: Option<DateTime<Utc>> }`
  - `PatchMock.webhooks: Option<Vec<WebhookTarget>>` (replaces the list; `[]` clears); `CreateMock.webhooks: Vec<WebhookTarget>` (`#[serde(default)]`); `MockView.webhooks` (`skip_serializing_if = "Vec::is_empty"`).
  - `fn check_webhook_secret_refs(targets: &[WebhookTarget], allowed: &[String]) -> ApiResult<()>` in `mocks.rs`: 422 titled `Secret reference not allowed` naming `webhooks[i].secret` and `accepted_secret_refs(allowed)`, never echoing the value; `validate_webhooks` errors map to 422 `Invalid webhooks`.

- [ ] **Step 1: Write the failing tests**

```rust
// tests/mocks.rs
#[tokio::test]
async fn webhooks_round_trip_through_the_api() {
    let app = app().await;
    let (status, body) = call(&app, "POST", "/api/v1/workspaces/default/mocks", json!({
        "name": "shop", "webhooks": [{"url": "https://hooks.test/stripe", "events": ["*"],
        "secret": "env:SHOP_WEBHOOK_SECRET", "signing": "stripe", "delay_ms": 10}]})).await;
    assert_eq!(status, 201);
    assert_eq!(body["webhooks"][0]["signing"], "stripe");
    let (status, body) = call(&app, "PATCH", "/api/v1/workspaces/default/mocks/shop", json!({"webhooks": []})).await;
    assert_eq!(status, 200);
    assert!(body.get("webhooks").is_none());
}

#[tokio::test]
async fn webhook_secret_refs_are_restricted() {
    let app = app().await;
    for (secret, title) in [("env:OPENROUTER_API_KEY", "Secret reference not allowed"),
                            ("whsec_pasted_value", "Invalid webhooks")] {
        let (status, body) = call(&app, "POST", "/api/v1/workspaces/default/mocks", json!({
            "name": "shop", "webhooks": [{"url": "https://hooks.test/h", "secret": secret}]})).await;
        assert_eq!(status, 422);
        assert_eq!(body["title"], title);
        assert!(!body.to_string().contains("whsec_pasted_value"));
    }
}

// tests/webhook_deliveries.rs
#[tokio::test]
async fn lists_deliveries_newest_first() {
    // AdminState + Extension(log) with two records pushed for (default, shop)
    // (one Delivered/200, one Failed/503, url "https://h.test/x").
    // GET …/mocks/shop/webhooks/deliveries?limit=1 → 200, one item, status "failed",
    // attempts 4, response_status 503, created_at RFC 3339.
    // GET …/mocks/nope/webhooks/deliveries → 404 problem+json.
    // GET …?limit=0 → 422 problem+json.
    // A viewer-role key may read it (rbac).
}
```

- [ ] **Step 2:** `cargo test -p mb-admin --test mocks --test webhook_deliveries` → FAIL.
- [ ] **Step 3:** Implement; then `cargo run -p mockingbird -- openapi > /dev/null` sanity check and `cd crates/mb-console/web && npm run gen:api && npm run check:api && npm run typecheck`.
- [ ] **Step 4:** `cargo test -p mb-admin && cargo test -p mockingbird --test docs_reference` → PASS.
- [ ] **Step 5:** Commit `feat(admin): mock webhooks and webhook delivery listing` (include `schema.d.ts`).

---

### Task 9: CLI `mockingbird webhooks`

**Files:**
- Modify: `crates/mockingbird/src/client.rs` (`ClientCommand::Webhooks(WebhooksCommand)`, module doc list)
- Test: `crates/mockingbird/tests/client.rs`

**Interfaces:**
- Consumes: Task 1 `signature_header`, `Signing`; Task 8 route; `mb_ext::DefaultSecrets::from_env()` + `SecretStore::resolve`.
- Produces:

```rust
#[derive(Subcommand, Debug)]
pub enum WebhooksCommand {
    /// Print the signature header a mock would send for a body (for tests).
    Sign {
        /// env:NAME, file:/path or vault:path#key.
        #[arg(long)] secret_ref: String,
        /// The exact body bytes (`-` reads stdin).
        #[arg(long)] body: PathBuf,
        #[arg(long, value_enum, default_value = "hmac-sha256")] signing: SignArg, // stripe | hmac-sha256
        /// Unix seconds for Stripe's `t` [default: now].
        #[arg(long)] timestamp: Option<i64>,
    },
    /// List a mock's recent webhook deliveries, newest first.
    Deliveries { mock: String, #[arg(long, default_value_t = 50)] limit: u32 },
}
```

`Sign` runs locally (no Admin API call) and returns `{"header": "<name>", "value": "<signature>"}`; a resolve failure is an error naming the reference only.

- [ ] **Step 1: Write the failing tests**

```rust
#[tokio::test]
async fn webhooks_sign_prints_stripe_header() {
    let dir = tempfile::tempdir().unwrap();
    let body = dir.path().join("body.json");
    std::fs::write(&body, br#"{"id":"evt_1","type":"customer.created"}"#).unwrap();
    let out = run_with("http://127.0.0.1:9", &["webhooks", "sign", "--secret-ref", "env:WS6_CLI_WEBHOOK_SECRET",
        "--body", body.to_str().unwrap(), "--signing", "stripe", "--timestamp", "1700000000"],
        &[("WS6_CLI_WEBHOOK_SECRET", "whsec_test_secret")], None).await;
    assert!(out.status.success());
    let v: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v, json!({"header": "stripe-signature",
        "value": "t=1700000000,v1=2146c3ca883db3d5b33f638513d4322fcf955eeb8624ee63937a742c023fe30b"}));
    assert!(!String::from_utf8_lossy(&out.stdout).contains("whsec_test_secret"));
}

#[tokio::test]
async fn webhooks_sign_reports_missing_secret_by_reference() {
    // env:WS6_UNSET_WEBHOOK_SECRET → non-zero exit, stderr names the reference.
}

#[tokio::test]
async fn webhooks_deliveries_lists_json() {
    // against admin_url(): create mock "shop", run `webhooks deliveries shop` → stdout "[]".
}
```

- [ ] **Step 2:** `cargo test -p mockingbird --test client webhooks` → FAIL.
- [ ] **Step 3:** Implement.
- [ ] **Step 4:** `cargo test -p mockingbird --test client` → PASS.
- [ ] **Step 5:** Commit `feat(cli): webhooks sign and webhooks deliveries`.

---

### Task 10: MCP tool `list_webhook_deliveries` (rebase onto WS5 if it merged first)

WS5 owns `crates/mockingbird/src/mcp.rs` in this wave. Before starting: `git fetch && git rebase origin/main`; if WS5 has merged, follow its output-size conventions (summary fields only).

**Files:**
- Modify: `crates/mockingbird/src/mcp.rs` (`tools()` entry after `list_requests`; dispatch arm next to `"list_requests"`)
- Test: `crates/mockingbird/src/mcp.rs` tests (`tools_are_unique_and_described` covers description/uniqueness) plus one call test in the same module or `crates/mockingbird/tests/agent_toolkit.rs`

**Interfaces:**
- Consumes: Task 8 route.
- Produces: tool `list_webhook_deliveries`, `read_only: true`, schema `{mock (required), limit: "1-200; default 20."}`, calls `GET mocks/<mock>/webhooks/deliveries?limit=`.

- [ ] **Step 1: Write the failing test**

```rust
#[tokio::test]
async fn list_webhook_deliveries_tool() {
    let tool = tools().into_iter().find(|t| t.name == "list_webhook_deliveries").unwrap();
    assert!(tool.read_only && !tool.destructive);
    assert_eq!(tool.schema["required"], json!(["mock"]));
    // Against a test admin server: calling it for an existing mock returns
    // a result whose JSON is [] and is_error false.
}
```

- [ ] **Step 2:** `cargo test -p mockingbird --lib mcp` → FAIL.
- [ ] **Step 3:** Implement; mention the tool in `INSTRUCTIONS` only if WS5 has not restructured it.
- [ ] **Step 4:** `cargo test -p mockingbird --lib mcp && cargo test -p mockingbird --test agent_toolkit` → PASS.
- [ ] **Step 5:** Commit `feat(mcp): list_webhook_deliveries tool`.

---

### Task 11: Gates, changelog, PR

**Files:**
- Modify: `CHANGELOG.md` (`## [Unreleased]` → `### Added`)

- [ ] **Step 1:** Add one entry: "Webhooks: mocks POST an event after each successful write to the URLs in `mocks.<name>.webhooks` (or the Admin API), signed Stripe-style (`Stripe-Signature`) or with `X-Mockingbird-Signature`, retried 3 times (1 s, 2 s, 4 s); the payload is the contract's OpenAPI `webhooks`/`callbacks` body when defined, else a Stripe-shaped `<resource>.<created|updated|deleted>` event. Deliveries: `GET …/mocks/{mock}/webhooks/deliveries`, `mockingbird webhooks deliveries`, MCP `list_webhook_deliveries`; `mockingbird webhooks sign` prints a signature for tests. `server.api_webhook_secret_refs` limits secret references set through the API. Migration `0700_mock_webhooks` adds `mocks.webhooks`."
- [ ] **Step 2:** Run every gate:

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-features
cargo check -p mockingbird --no-default-features
scripts/test-postgres.sh
cd crates/mb-console/web && npm ci && npm run typecheck && npm run lint && npm test && npm run check:api
```

Expected: all pass, except the known pre-existing `tls::llm_client_trusts_outbound_ca` on macOS and the flaky `graphql::rejections_are_recorded` (re-run once).
- [ ] **Step 3:** Commit `docs: changelog for webhooks`; `git push -u origin feat/ws6-webhooks`.
- [ ] **Step 4:** `gh pr create --base main --title "feat: webhooks (WS6)"` with a body summarising the decisions table, the test list, the known failures and "MCP tool: rebased onto WS5" if applicable, ending with `🤖 Generated with [Claude Code](https://claude.com/claude-code)`. Do **not** merge; report the PR URL.
