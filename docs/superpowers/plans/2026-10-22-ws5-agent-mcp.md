# WS5: Agent and MCP Ergonomics Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make Mockingbird's MCP server and `agent install` usable by coding agents without hand-holding: draft and proxy tools, small tool outputs, logs and server state that resolve correctly, key passthrough in agent configs, and a local key setup page that stores the key in a private file the agent never sees.

**Architecture:** New MCP tools in `crates/mockingbird/src/mcp.rs` call the Admin API (drafts, request log) or local helpers (proxy env, key page). The Admin API request log gains a summary projection and a single-entry route. A new `crates/mockingbird/src/keys/` module owns the private key store (`~/.mockingbird/secrets/<name>`, `0600` in a `0700` directory) and the one-shot axum page on `127.0.0.1:<random>`. `mb-ext`'s `EnvFileSecrets` refuses key-store files readable by group or others.

**Tech Stack:** Rust 1.97 workspace; axum 0.8 (already in tree; `Form` extractor from its default features); `json-patch` 4 (`json_patch::merge`, RFC 7396; already a workspace dependency); `uuid` v4 for the page token; `std::process::Command` for the browser (`open` / `xdg-open` / `cmd /C start`) and Windows `icacls`; wiremock (existing dev-dependency) as a fake Admin API in MCP unit tests.

**Spec:** `docs/superpowers/specs/2026-10-22-improvements-backlog-design.md` (WS5, sections 5.1–5.5). Runs in wave 2, after WS3 (import and drafts) and WS4 (routing, proxy, CA) are merged on `main`.

## Global Constraints

- Branch `feat/ws5-agent-mcp`, cut from `main` after WS3 and WS4 merged; one PR; do not merge it.
- No `unwrap()`/`expect()` outside tests (`clippy::unwrap_used = "deny"`); test modules in `src/` carry `#[allow(clippy::unwrap_used, clippy::expect_used)]`.
- Admin API errors are RFC 9457 `application/problem+json` (`mb_admin::problem::Problem`).
- Secrets are references only. A key value is never written to a config file, an agent config, a command line, a log line, a tool result or an HTTP response; `open_key_setup` returns only `{"saved": name}`.
- The key store is `~/.mockingbird/secrets/<name>`: file `0600`, directory `0700`. No OS keychain.
- After the Admin API change run `npm run gen:api` in `crates/mb-console/web` and commit `src/api/schema.d.ts`.
- Skills under `plugins/mockingbird/skills` are embedded by `crates/mockingbird/src/agents.rs` (`SKILLS`); no new skill directories, only edits. `plugins/mockingbird/bin/install.sh` stays a copy of `install.sh` (untouched here).
- `mb-ext` follows semver from 1.0: the one new public function is additive; say so in the PR.
- Tool results keep the existing shape conventions: objects for structured results, arrays where the existing tool returned an array (`list_requests`).
- Commits `type(scope): summary`, ending `Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>`.
- Known pre-existing failures, not caused by this branch: `tls::llm_client_trusts_outbound_ca` on macOS, flaky `graphql::rejections_are_recorded`.

## Review Focus

1. **A web page in the user's browser posting to the key page** (CSRF / DNS rebinding): only a request whose path carries the token, whose `Host` is `127.0.0.1:<port>` and whose `Origin` (when sent) is `http://127.0.0.1:<port>` may save; anything else writes nothing. Test `foreign_host_or_origin_is_refused` in Task 9.
2. **The key leaking through a side channel**: the saved key must not appear in the HTTP response, the MCP result, the config file or the agent config. Tests `post_with_token_writes_a_private_file` (Task 9) and `open_key_setup_never_returns_the_key` (Task 9) assert the key string is absent.
3. **A second submission after the key was saved** (double click, replay): the page is one-shot; a second POST gets `410 Gone` and the stored file is unchanged. Test `second_post_is_gone` in Task 9.
4. **`edit_draft` with a patch that breaks the document**: the Admin API's 422 is surfaced as a tool error naming the problem and the draft is not changed. Test `edit_draft_surfaces_an_invalid_patch` in Task 4.
5. **A secret name that is a path** (`../../.ssh/id`, `a/b`, `.hidden`): rejected before anything is written. Test `secret_names_are_plain` in Task 8.

---

### Task 0: Verify the WS3/WS4 interfaces on `main` and pin the names

The WS3 and WS4 plans were not on disk when this plan was written; the names below are the spec's. This task confirms or corrects them; later tasks use whatever this task records.

**Files:**
- Modify: `docs/superpowers/plans/2026-10-22-ws5-agent-mcp.md` (this file, the "Resolved WS3/WS4 names" list below) — only if a name differs.

**Interfaces:**
- Consumes (assumed, from the spec):
  - WS3: `POST /api/v1/workspaces/{ws}/mocks/{mock}/imports` body `CreateImport` accepts `urls: [string]` and `mode: "merge" | "replace"`; `POST …/contracts/{version}/approve` accepts `{"activate": bool}` (default `true`) and returns the version summary; the CLI `client::ImportArgs` gains `urls: Vec<String>` and `mode: Option<ImportMode>`; `onboard::contract_summary` may already carry WS3 `warnings`.
  - WS4 (from its plan, `2026-10-22-ws4-routing-proxy.md`): `proxy::env::options_for(config: Option<&Path>, port: Option<u16>) -> anyhow::Result<ProxyEnvOptions>` and `proxy::env::proxy_env(&ProxyEnvOptions) -> anyhow::Result<ProxyEnv>`, where `ProxyEnv { vars: BTreeMap<String, String>, ca_path, bundle_path }` is `Serialize` (the MCP `proxy_env` tool returns it as JSON); `proxy::ca::ca_cert_path()`; `proxy::DEFAULT_PROXY_PORT = 8888`; `ServeArgs.proxy` (`--proxy`) and `ServeArgs.proxy_port` (`--proxy-port`), which `start_server(proxy: true)` passes through `daemon.rs start`.
- Produces: the "Resolved WS3/WS4 names" list, read by Tasks 4 and 5.

- [ ] **Step 1: Create the branch**

```bash
git fetch origin && git switch -c feat/ws5-agent-mcp origin/main
```

- [ ] **Step 2: Check each assumed name**

Run:
```bash
grep -n "urls\|mode" crates/mb-admin/src/imports/*.rs crates/mb-admin/src/imports.rs 2>/dev/null | head
grep -n "activate" crates/mb-admin/src/drafts.rs
grep -n "pub struct ImportArgs" -A30 crates/mockingbird/src/client.rs
grep -rn "pub fn\|pub async fn" crates/mockingbird/src/proxy/ | head -40
grep -n "proxy" crates/mockingbird/src/config.rs crates/mockingbird/src/file_config.rs | head
grep -n "warnings" crates/mockingbird/src/onboard.rs
```
Expected: each assumed name found, or its real name noted.

- [ ] **Step 3: Record the real names**

Fill in, under this task, a list "Resolved WS3/WS4 names" with one line per assumption above (`assumed → actual`). If any WS4 name above differs on main, record the actual one and use it in Task 5. No commit when nothing changed; otherwise:

```bash
git add docs/superpowers/plans/2026-10-22-ws5-agent-mcp.md
git commit -m "docs: pin the WS3/WS4 names WS5 uses"
```

---

### Task 1: Request log summary and single entry in the Admin API

**Files:**
- Modify: `crates/mb-admin/src/requests.rs` (query field, summary view, `get_one` handler)
- Modify: `crates/mb-admin/src/lib.rs:249` (route)
- Modify: `crates/mb-admin/src/openapi.rs:56,112` (path and schema)
- Modify: `crates/mb-console/web/src/api/schema.d.ts` (regenerated)
- Test: `crates/mb-admin/tests/api.rs` (next to `request_log_kind_query`; and the documented-paths list at ~1689)

**Interfaces:**
- Produces:
  - `RequestsQuery.fields: Option<String>`: absent → full entries; `"summary"` → summaries; anything else → 400 problem `unknown fields value {v:?}; use summary`.
  - `#[derive(Serialize, ToSchema, Debug)] pub struct RequestLogSummaryView { pub id: i64, pub operation_id: Option<String>, pub session: String, pub method: String, pub path: String, pub status: u16, pub source: Option<String>, pub fallback_reason: Option<String>, pub latency_ms: u64, pub prompt_tokens: u32, pub completion_tokens: u32, pub at: DateTime<Utc>, pub kind: String, pub request_bytes: u64, pub response_bytes: u64 }` — the byte counts are `serde_json::to_vec(..).len()` of the stored request / response (`0` when the response is `None`).
  - `impl From<RequestLogEntry> for RequestLogSummaryView`.
  - `pub async fn list(...) -> ApiResult<axum::response::Response>` (JSON array of either view; the OpenAPI body stays `Vec<RequestLogView>` with the description "with `fields=summary`, `RequestLogSummaryView` items", so the console client type does not change).
  - `#[derive(Deserialize)] pub struct RequestPath { pub mock: String, pub id: i64 }`
  - `pub async fn get_one(State(st): State<AdminState>, access: WsAccess, Path(RequestPath { mock, id }): Path<RequestPath>) -> ApiResult<Json<RequestLogView>>` at `GET /api/v1/workspaces/{ws}/mocks/{mock}/requests/{id}` (alias `/api/v1/mocks/{mock}/requests/{id}` comes from `openapi.rs`'s alias generation). Looks the entry up with `list_request_log` and `RequestLogFilter { before_id: id.checked_add(1), limit: 1, .. }`, and answers 404 problem `no request {id} in mock {mock:?}` when the returned id differs. No store change.

- [ ] **Step 1: Write the failing tests** in `crates/mb-admin/tests/api.rs`, seeding entries the same way `request_log_*` tests do:

```rust
#[tokio::test]
async fn request_log_summary_and_single_entry() {
    let h = harness().await;
    // seed mock "orders" and two entries exactly as the request_log test above does;
    // ids[1] has request {"query": {}} and response {"ok": true}
    let r = h.get("/api/v1/mocks/orders/requests?fields=summary").await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.text);
    let first = &r.json()[0];
    assert!(first.get("request").is_none() && first.get("response").is_none(), "{first}");
    assert_eq!(first["request_bytes"], json!({ "query": {} }).to_string().len());
    assert_eq!(first["response_bytes"], json!({ "ok": true }).to_string().len());
    assert_eq!(first["fallback_reason"], Value::Null);
    h.get("/api/v1/mocks/orders/requests?fields=bodies")
        .await
        .assert_problem(StatusCode::BAD_REQUEST);

    let one = h.get(&format!("/api/v1/mocks/orders/requests/{}", ids[0])).await;
    assert_eq!(one.status, StatusCode::OK, "{}", one.text);
    assert_eq!(one.json()["id"], ids[0]);
    assert_eq!(one.json()["response"], json!({ "ok": true }));
    h.get("/api/v1/mocks/orders/requests/999999")
        .await
        .assert_problem(StatusCode::NOT_FOUND);
    // "stream" stays the SSE route, not an id
    assert_ne!(h.get("/api/v1/mocks/orders/requests/stream").await.status, StatusCode::BAD_REQUEST);
}
```

Add `("/api/v1/mocks/{mock}/requests/{id}", &["get"])` to the documented-paths list (~line 1689).

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test -p mb-admin --test api request_log_summary_and_single_entry openapi`
Expected: FAIL (`request_bytes` is null; `/requests/{id}` 404 from the router; path missing from the document).

- [ ] **Step 3: Implement** the types and handlers above in `requests.rs`; route `.route("/mocks/{mock}/requests/{id}", get(requests::get_one))` in `lib.rs` next to line 249; add `requests::get_one` to `paths(...)` and `requests::RequestLogSummaryView` to the schemas in `openapi.rs`.

- [ ] **Step 4: Run to verify they pass**

Run: `cargo test -p mb-admin`
Expected: PASS.

- [ ] **Step 5: Regenerate the console client**

Run: `cd crates/mb-console/web && npm run gen:api && npm run typecheck && npm run check:api`
Expected: `schema.d.ts` gains `RequestLogSummaryView` and the `{id}` path; typecheck passes.

- [ ] **Step 6: Commit**

```bash
git add crates/mb-admin crates/mb-console/web/src/api/schema.d.ts
git commit -m "feat(admin): request log summaries and single entries"
```

---

### Task 2: Small MCP outputs: `list_requests`, `get_request`, `validate_contract`

**Files:**
- Modify: `crates/mockingbird/src/mcp.rs` (tool definitions ~:233-611, dispatch in `McpServer::call` ~:850-1010)
- Modify: `crates/mockingbird/src/onboard.rs:441` (`contract_summary`)
- Test: `crates/mockingbird/src/mcp.rs` `mod tests`; `crates/mockingbird/src/onboard.rs` `mod tests`; `crates/mockingbird/tests/agent_toolkit.rs` (`mcp_drives_a_background_server`)

**Interfaces:**
- Consumes: Task 1 (`?fields=summary`, `GET …/requests/{id}`).
- Produces:
  - Tool `list_requests` gains `include_bodies: boolean` (default false); without it the call sends `fields=summary`. Result: the Admin API's array unchanged.
  - Tool `get_request { mock: string, id: integer }` (read-only, not local) → `GET mocks/{mock}/requests/{id}`.
  - `pub const SUMMARY_OPERATIONS: usize = 20;` in `onboard.rs`.
  - `pub fn contract_summary(path: &Path) -> anyhow::Result<Value>` now returns `{ "title", "version", "counts": { "<kind label>": n, …, "channels": n }, "operations": [first 20 {id, target, kind}], "operations_truncated": n, "channels": [first 20], "channels_truncated": n }` (`n` = how many were left out, `0` when none; keep any WS3 `warnings` key Task 0 found).

- [ ] **Step 1: Write the failing tests**

In `onboard.rs` tests, with a generated OpenAPI document of 25 `GET /r{i}` operations written to a tempdir:
```rust
#[test]
fn contract_summary_caps_the_operation_list() {
    let s = contract_summary(&path).unwrap();
    assert_eq!(s["operations"].as_array().unwrap().len(), SUMMARY_OPERATIONS);
    assert_eq!(s["operations_truncated"], 5);
    assert_eq!(s["counts"]["http"], 25);
    assert_eq!(s["channels_truncated"], 0);
}
```
In `mcp.rs` tests (wiremock as the Admin API; `McpServer { admin_url: mock.uri(), .. server(false) }`):
```rust
#[tokio::test]
async fn list_requests_asks_for_summaries_unless_bodies_are_wanted() {
    // Mock GET /api/v1/workspaces/default/mocks/m/requests with query_param("fields","summary") → [{"id":1}]
    // and GET …/requests/7 → {"id":7,"response":{"ok":true}}
    let r = call_tool(&s, "list_requests", json!({"mock":"m"})).await;
    assert_eq!(r, json!([{"id":1}]));
    let r = call_tool(&s, "get_request", json!({"mock":"m","id":7})).await;
    assert_eq!(r["response"]["ok"], true);
    // include_bodies: no fields parameter (wiremock matcher `query_param_is_missing("fields")`)
    let r = call_tool(&s, "list_requests", json!({"mock":"m","include_bodies":true})).await;
    assert_eq!(r, json!([{"id":1,"request":{}}]));
}
```
`call_tool` is a test helper in `mod tests`: sends `tools/call`, asserts `isError == false`, parses `content[0].text`.

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test -p mockingbird --lib contract_summary_caps list_requests_asks`
Expected: FAIL (`operations` has 25 items; `get_request` is an unknown tool).

- [ ] **Step 3: Implement** the tool schema changes (`include_bodies: b("Return request and response bodies; default false (sizes only).")`, `get_request` with `id: n("Request log id (list_requests).")`) and `contract_summary` above. Update the `list_requests` description to "…newest first: method, path, status, source, fallback reason, sizes; bodies with include_bodies or get_request."

- [ ] **Step 4: Update the e2e assertion** in `agent_toolkit.rs` `mcp_drives_a_background_server`: after `list_requests` assert `log[0].get("response").is_none()` and `log[0]["response_bytes"].as_u64().unwrap() > 0`; add `let one = mcp.tool("get_request", json!({"mock":"orders","id": log[0]["id"]})); assert!(one["response"].is_object());`.

- [ ] **Step 5: Run to verify they pass**

Run: `cargo test -p mockingbird --lib && cargo test -p mockingbird --test agent_toolkit`
Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add crates/mockingbird/src/mcp.rs crates/mockingbird/src/onboard.rs crates/mockingbird/tests/agent_toolkit.rs
git commit -m "feat(mcp): request summaries, get_request and capped contract summaries"
```

---

### Task 3: Clean logs and the right state directory

**Files:**
- Modify: `crates/mockingbird/src/telemetry.rs:56` (fmt layer)
- Modify: `crates/mockingbird/src/daemon.rs` (`logs` ~:376, new helpers)
- Modify: `crates/mockingbird/src/mcp.rs` (`McpServer` fields, `state()`, `data_plane()` ~:1157, `start_server`/`server_status`/`server_logs`/`stop_server` schemas and dispatch)
- Test: `daemon.rs` `mod tests` (new), `mcp.rs` `mod tests`, `crates/mockingbird/tests/agent_toolkit.rs`

**Interfaces:**
- Produces:
  - `telemetry::init_with`: `tracing_subscriber::fmt::layer().with_writer(std::io::stderr).with_ansi(std::io::IsTerminal::is_terminal(&std::io::stderr()))`.
  - `pub fn strip_ansi(text: &str) -> String` in `daemon.rs`: removes CSI sequences (`ESC [` parameter bytes `0x30–0x3F`, intermediate `0x20–0x2F`, final `0x40–0x7E`) and OSC sequences (`ESC ]` … terminated by BEL or `ESC \`); any other lone `ESC` is dropped. `logs` returns `strip_ansi(&tail(..))`.
  - `pub fn resolve_state_dir(state_dir: &Path, config: Option<&Path>, cwd: &Path) -> PathBuf` in `daemon.rs`: an absolute `state_dir` is returned as is; otherwise walk the ancestors of the config file's directory (made absolute against `cwd`), then the ancestors of `cwd`, returning the first `<ancestor>/<state_dir>` that contains `server.json`; with none found, `<config dir>/<state_dir>` when a config is given, else `cwd.join(state_dir)`.
  - `McpServer` gains `started_config: std::sync::Mutex<Option<PathBuf>>` (absolute config path recorded by a successful `start_server`).
  - `fn state_for(&self, config: Option<&Path>) -> daemon::StateArgs` in `mcp.rs`: `resolve_state_dir(&self.state_dir, config.or(recorded), &current_dir)`; used by `start_server`, `server_status`, `server_logs`, `stop_server`, `doctor` and `data_plane`.
  - Tools `server_status`, `server_logs`, `stop_server` gain optional `config: s("Config file the server was started with; default: the one start_server used.")`.

- [ ] **Step 1: Write the failing tests**

```rust
// daemon.rs
#[test]
fn strip_ansi_removes_escape_sequences() {
    assert_eq!(strip_ansi("\x1b[2m2026\x1b[0m \x1b[32m INFO\x1b[0m ready"), "2026  INFO ready");
    assert_eq!(strip_ansi("a\x1b]8;;http://x\x07link\x1b]8;;\x1b\\b"), "alinkb");
    assert_eq!(strip_ansi("plain"), "plain");
}

#[test]
fn state_dir_is_found_next_to_the_config() {
    let root = tempfile::tempdir().unwrap();
    let project = root.path().join("app/sub");
    std::fs::create_dir_all(root.path().join("app/.mockingbird")).unwrap();
    std::fs::write(root.path().join("app/.mockingbird/server.json"), "{}").unwrap();
    std::fs::create_dir_all(&project).unwrap();
    let elsewhere = tempfile::tempdir().unwrap();
    let rel = Path::new(".mockingbird");
    assert_eq!(resolve_state_dir(rel, Some(&project.join("mockingbird.yaml")), elsewhere.path()),
               root.path().join("app/.mockingbird"));
    let fresh = tempfile::tempdir().unwrap();
    assert_eq!(resolve_state_dir(rel, Some(&fresh.path().join("m.yaml")), elsewhere.path()),
               fresh.path().join(".mockingbird"));
    assert_eq!(resolve_state_dir(Path::new("/abs/state"), None, elsewhere.path()), PathBuf::from("/abs/state"));
}
```
In `agent_toolkit.rs`, a new test:
```rust
#[test]
fn server_tools_find_the_server_from_another_folder() {
    // project dir P with `init --offline --contract orders.yaml`; MCP started with current_dir = other tempdir O
    // start_server {config: "<P>/mockingbird.yaml", ports…} → ready
    // server_logs {} (same MCP process) → log non-empty and !log.contains('\x1b')
    // drop the MCP client, start a new one in O:
    // server_status {config: "<P>/mockingbird.yaml"} → running == true
    // stop_server {config: "<P>/mockingbird.yaml"} → stopped == true
    // assert P/.mockingbird/server.json exists and O/.mockingbird does not
}
```

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test -p mockingbird --lib strip_ansi state_dir_is_found && cargo test -p mockingbird --test agent_toolkit server_tools_find`
Expected: FAIL (functions missing; state written under O).

- [ ] **Step 3: Implement** the items listed in Produces. `start_server` passes `state_for(config)` into `daemon::StartArgs.state` and stores the absolute config path in `started_config` after `daemon::start` succeeds.

- [ ] **Step 4: Run to verify they pass**

Run: `cargo test -p mockingbird --lib && cargo test -p mockingbird --test agent_toolkit`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/mockingbird/src/{telemetry,daemon,mcp}.rs crates/mockingbird/tests/agent_toolkit.rs
git commit -m "fix(mcp): plain-text logs and the started server's state directory"
```

---

### Task 4: Draft MCP tools and `import_contract` urls/mode

**Files:**
- Modify: `crates/mockingbird/src/mcp.rs` (tools, dispatch, new methods), `crates/mockingbird/Cargo.toml` (`json-patch.workspace = true`)
- Test: `crates/mockingbird/src/mcp.rs` `mod tests` (wiremock Admin API)

**Interfaces:**
- Consumes: Admin API `GET …/mocks/{mock}/contracts` (version list with `version`, `status`), `GET/PUT …/contracts/{version}/draft` (`DraftView` in `crates/mb-admin/src/drafts.rs:97`, `PutDraft { document }`), `POST …/contracts/{version}/approve` with WS3's `activate` (Task 0); WS3 `client::ImportArgs` `urls`/`mode` (Task 0).
- Produces:
  - `async fn draft_version(&self, mock: &str, version: Option<u64>) -> Result<u64, ToolError>`: the given version, else the highest version whose `status == "draft"`; none → `ToolError("mock {mock:?} has no draft; import documentation, HAR or Postman with import_contract first")`.
  - Tool `get_draft { mock, version?, operation? }` (read-only, not local). Without `operation`: `{ "version", "status", "source_kind", "operations": [{ "id", "method", "path", "confidence", "warnings": [message…] }], "warnings": [{code, message} not tied to an operation], "diff" }` — an operation's warnings are the `meta.warnings` whose `operation` equals `"<METHOD> <path>"`. With `operation: <id>`: `{ "version", "id", "method", "path", "confidence", "warnings", "definition": document.paths[path][lowercase method] }`; unknown id → tool error listing up to 20 known ids.
  - Tool `edit_draft { mock, version?, operation?, patch: object }`: GET the draft, apply `json_patch::merge` to `document` (whole draft) or to `document.paths[path][method]` (when `operation` is given), PUT `{document}`; returns the `get_draft` summary of the PUT response. A 422 becomes `ToolError("the patched draft does not compile: <pointer>: <message>; …")`.
  - Tool `approve_draft { mock, version?, activate?: boolean }` (default `activate: true`) → POST approve; returns the Admin API result.
  - `fn draft_summary(view: &Value, operation: Option<&str>) -> Result<Value, ToolError>` (pure; shared by the three tools).
  - Tool `import_contract` gains `urls: { type: array, items: string, description: "Documentation pages to import together (http(s))." }` and `mode: { enum: ["merge","replace"], description: "With an existing draft or active contract: merge (default) adds and replaces operations by method+path; replace starts over." }`, passed to WS3's `client::ImportArgs`. `source`, `content`, `content_base64` and `urls` are mutually exclusive (existing "give exactly one of …" message gains `urls`).

- [ ] **Step 1: Write the failing tests** (wiremock fixtures: a version list `[{"version":1,"status":"active"},{"version":2,"status":"draft"}]` and a `DraftView` JSON with two operations `listPets GET /pets 0.9` and `getPet GET /pets/{id} 0.5`, a warning `{code:"invalid_schema", message:"x", operation:"GET /pets/{id}"}` and one with `operation: null`):

```rust
#[tokio::test]
async fn get_draft_summarises_and_expands_one_operation() {
    let r = call_tool(&s, "get_draft", json!({"mock":"pets"})).await;
    assert_eq!(r["version"], 2);
    assert_eq!(r["operations"][1], json!({"id":"getPet","method":"GET","path":"/pets/{id}","confidence":0.5,"warnings":["x"]}));
    assert_eq!(r["warnings"].as_array().unwrap().len(), 1);
    assert!(r.get("document").is_none());
    let one = call_tool(&s, "get_draft", json!({"mock":"pets","operation":"getPet"})).await;
    assert!(one["definition"]["responses"].is_object());
}

#[tokio::test]
async fn edit_draft_merges_into_one_operation() {
    // PUT …/contracts/2/draft matcher: body_partial_json({"document":{"paths":{"/pets/{id}":{"get":{"summary":"Fetch"}}}}})
    let r = call_tool(&s, "edit_draft", json!({"mock":"pets","operation":"getPet","patch":{"summary":"Fetch"}})).await;
    assert_eq!(r["version"], 2);
}

#[tokio::test]
async fn edit_draft_surfaces_an_invalid_patch() {
    // PUT answers 422 {"type":"about:blank","title":"Invalid draft","status":422,"detail":"…","errors":[{"pointer":"/paths/~1pets/get/responses","message":"required"}]}
    let text = call_tool_error(&s, "edit_draft", json!({"mock":"pets","operation":"listPets","patch":{"responses":null}})).await;
    assert!(text.contains("/paths/~1pets/get/responses") && text.contains("required"), "{text}");
}

#[tokio::test]
async fn approve_draft_activates_by_default() {
    // POST …/contracts/2/approve matcher: body_json({"activate": true}) → {"version":2,"status":"active"}
    let r = call_tool(&s, "approve_draft", json!({"mock":"pets"})).await;
    assert_eq!(r["status"], "active");
}

#[tokio::test]
async fn draft_tools_without_a_draft_say_so() {
    // version list with only an active version
    let text = call_tool_error(&s, "get_draft", json!({"mock":"pets"})).await;
    assert!(text.contains("has no draft"), "{text}");
}
```
Also extend `remote_clients_get_no_local_tools`: `get_draft`, `edit_draft`, `approve_draft`, `get_request` are listed for remote clients.

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test -p mockingbird --lib draft`
Expected: FAIL (unknown tools).

- [ ] **Step 3: Implement** the tools and helpers listed in Produces; descriptions name the workflow: "Review a draft import: operations with confidence and warnings; pass operation for its full definition."

- [ ] **Step 4: Run to verify they pass**

Run: `cargo test -p mockingbird --lib`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/mockingbird/Cargo.toml crates/mockingbird/src/mcp.rs Cargo.lock
git commit -m "feat(mcp): get_draft, edit_draft, approve_draft and multi-URL imports"
```

---

### Task 5: Proxy MCP tools and skills that recommend proxy mode

**Files:**
- Modify: `crates/mockingbird/src/mcp.rs` (tool `proxy_env`, `start_server.proxy`, `LOCAL_TOOLS`, `INSTRUCTIONS`)
- Modify: `crates/mockingbird/src/daemon.rs` (`StartArgs.proxy`, `start`)
- Modify (only if Task 0 says so): nothing in WS4's files (use its `proxy::env` API)
- Modify: `plugins/mockingbird/skills/mockingbird-mock-api/SKILL.md`, `plugins/mockingbird/skills/mockingbird-testing/SKILL.md`, `plugins/mockingbird/skills/mockingbird/SKILL.md`, `docs/agents.md` (tool table ~:42)
- Test: `crates/mockingbird/src/mcp.rs` `mod tests`, `crates/mockingbird/tests/agent_toolkit.rs`

**Interfaces:**
- Consumes: WS4 proxy env function and serve flag as resolved in Task 0.
- Produces:
  - `daemon::StartArgs.proxy: bool` (`#[arg(long)]`, "Also run the HTTP(S) proxy (`proxy.enabled`)"); when set, the child gets the serve flag from Task 0, and `server.json` / the start result gain `"proxy": "http://127.0.0.1:<proxy port>"`.
  - Tool `proxy_env { config?: string, port?: integer }` (read-only, local) → the WS4 JSON `{ "variables": {…}, "ca_path": "…" }` plus `"export": "<one `export NAME=value` line per variable>"`.
  - Tool `start_server` gains `proxy: b("Also start the HTTP(S) proxy; the result carries proxy_env.")`; with it the result has `proxy` and `proxy_env`.
  - `LOCAL_TOOLS` gains `"proxy_env"`.
  - `INSTRUCTIONS` gains the sentence: "To run an app unchanged against mocks, start the server with `proxy: true` and export the variables from `proxy_env` in the app's shell instead of editing its base URLs."

- [ ] **Step 1: Write the failing tests**

```rust
// mcp.rs
#[tokio::test]
async fn proxy_env_is_local_and_lists_the_variables() {
    let names: Vec<&str> = McpServer { local_tools: false, ..server(false) }.tools().iter().map(|t| t.name).collect();
    assert!(!names.contains(&"proxy_env"));
    let r = call_tool(&server(false), "proxy_env", json!({"port": 8888})).await;
    assert_eq!(r["variables"]["HTTPS_PROXY"], "http://127.0.0.1:8888");
    assert_eq!(r["variables"]["NO_PROXY"], "localhost,127.0.0.1");
    assert!(r["ca_path"].as_str().unwrap().ends_with("ca.pem"));
    assert!(r["export"].as_str().unwrap().contains("export NODE_USE_ENV_PROXY=1"));
}
```
(Run with `HOME` pointed at a tempdir inside the test if the WS4 function creates the CA on first use; use the same guard WS4's tests use.)

```rust
// agent_toolkit.rs, inside mcp_drives_a_background_server or a sibling test with its own ports
let started = mcp.tool("start_server", json!({ "config": "mockingbird.yaml", "proxy": true, "port": port, "admin_port": admin, "grpc_port": grpc }));
assert!(started["proxy"].as_str().unwrap().starts_with("http://127.0.0.1:"));
assert!(started["proxy_env"]["variables"]["HTTPS_PROXY"].is_string());
```
And in `every_skill_directory_is_embedded` (or a new `skills_recommend_proxy_mode`): the mock-api and testing skills contain `proxy_env` and `mockingbird proxy env`.

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test -p mockingbird --lib proxy_env && cargo test -p mockingbird --test agent_toolkit`
Expected: FAIL.

- [ ] **Step 3: Implement** the Produces items. Skills: in `mockingbird-mock-api` add a section "Run an app unchanged (proxy mode)" — give the mock `hosts: [api.example.com]`, `start_server {proxy: true}` (CLI `mockingbird server start --proxy -c mockingbird.yaml`), then export `proxy_env`'s variables (CLI `eval "$(mockingbird proxy env)"`) where the app runs; editing the app's base URL is the fallback for runtimes that ignore proxy variables (Node `fetch` before `NODE_USE_ENV_PROXY` support, axios/got without an agent). In `mockingbird-testing`, replace "Point the client's base URL at the mock" with proxy mode first and the base URL as the fallback. In the `mockingbird` entry skill's tool list add `get_draft`, `edit_draft`, `approve_draft`, `get_request`, `proxy_env`. In `docs/agents.md` add the new tools to the table rows (Drafts row: `get_draft`, `edit_draft`, `approve_draft`; Traffic row: `get_request`; Server row: `proxy_env`).

- [ ] **Step 4: Run to verify they pass**

Run: `cargo test -p mockingbird --lib && cargo test -p mockingbird --test agent_toolkit`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/mockingbird plugins/mockingbird/skills docs/agents.md
git commit -m "feat(mcp): proxy_env, start_server proxy and proxy-first skills"
```

---

### Task 6: `agent install` key passthrough

**Files:**
- Modify: `crates/mockingbird/src/agents.rs` (`InstallArgs` ~:113, `server_entry` ~:147, `entry_for` ~:214, `codex_toml` ~:221, `install_codex`, `install` ~:346, `print`)
- Modify: `crates/mockingbird/src/daemon.rs` (shared default-profile lookup)
- Test: `crates/mockingbird/src/agents.rs` `mod tests`, `crates/mockingbird/tests/agent_toolkit.rs`

**Interfaces:**
- Produces:
  - `pub fn default_profile_env_names(config: &Path) -> Vec<String>` in `daemon.rs`: the variable names of the default profile's `env:` references (`api_key` and `${env:…}` in headers), sorted, deduplicated; a missing or invalid config → empty. `missing_default_secrets` and this function share `fn default_profile(cfg: &FileConfig) -> Option<&FileProfile>`.
  - `InstallArgs` gains `#[arg(short = 'c', long, default_value = "mockingbird.yaml")] pub config: PathBuf` and `#[arg(long = "pass-env", value_name = "NAME")] pub pass_env: Vec<String>` (added to the config's names; each must be a valid variable name, else `bail!("--pass-env {name:?} is not a variable name")`).
  - `#[derive(Clone, Copy, Debug, PartialEq, Eq)] pub enum EnvSyntax { Braced, EnvPrefixed, CodexEnvVars, Unsupported }` and `pub fn env_syntax(client: Client, scope: Scope) -> EnvSyntax`, pinned:

    | Client | Syntax | Written |
    |---|---|---|
    | ClaudeCode, project (`.mcp.json`) | `Braced` | `"env": {"NAME": "${NAME}"}` |
    | ClaudeCode, user (printed `claude mcp add`) | `Unsupported` | nothing (a shell would expand the value into `~/.claude.json`) |
    | ClaudeDesktop | `Unsupported` | nothing |
    | Codex | `CodexEnvVars` | `env_vars = ["NAME", …]` in the TOML table |
    | Cursor | `EnvPrefixed` | `"env": {"NAME": "${env:NAME}"}` |
    | Gemini | `Braced` | `"env": {"NAME": "${NAME}"}` |
    | Kiro | `Unsupported` | nothing |
    | Vscode | `EnvPrefixed` | `"env": {"NAME": "${env:NAME}"}` |
    | Windsurf | `EnvPrefixed` | `"env": {"NAME": "${env:NAME}"}` |

    The implementer verifies each row against the agent's current MCP configuration docs before merging (Claude Code `.mcp.json` env expansion; Codex `config.toml` `env_vars`; Cursor and Windsurf `${env:NAME}` interpolation; VS Code `mcp.json` variables; Gemini CLI `$VAR`/`${VAR}` in `env`; Kiro and Claude Desktop: none) and moves a row to `Unsupported` when the docs do not confirm it; the PR lists the sources.
  - `fn entry_for(client: Client, command: &str, env: &[String], scope: Scope) -> Value` (no `env` key when `env` is empty or the syntax is not JSON-based); `fn codex_toml(command: &str, env: &[String]) -> String` (adds `env_vars = [..]` with `toml_string` quoting when non-empty); `print` passes `&[]`.
  - `install`'s output gains `"passthrough": [names]` when any were written, or `"passthrough_unsupported": "<client> cannot read variables in its MCP config; run `mockingbird keys setup` to store the key in a file instead"` when names exist but the syntax is `Unsupported`.

- [ ] **Step 1: Write the failing tests**

```rust
// agents.rs
#[test]
fn passthrough_uses_each_agents_syntax() {
    let env = vec!["OPENROUTER_API_KEY".to_string()];
    assert_eq!(entry_for(Client::Cursor, "/mb", &env, Scope::Project)["env"]["OPENROUTER_API_KEY"], "${env:OPENROUTER_API_KEY}");
    assert_eq!(entry_for(Client::ClaudeCode, "/mb", &env, Scope::Project)["env"]["OPENROUTER_API_KEY"], "${OPENROUTER_API_KEY}");
    assert!(entry_for(Client::ClaudeDesktop, "/mb", &env, Scope::User).get("env").is_none());
    assert!(entry_for(Client::Cursor, "/mb", &[], Scope::Project).get("env").is_none());
    assert!(codex_toml("/mb", &env).contains("env_vars = [\"OPENROUTER_API_KEY\"]"));
}
```
```rust
// agent_toolkit.rs
#[test]
fn agent_install_passes_key_names_not_values() {
    let dir = tempdir();
    // mockingbird.yaml from `init --provider openrouter` (api_key: env:OPENROUTER_API_KEY)
    let run = |args: &[&str]| { /* bin() in dir, env HOME=dir, OPENROUTER_API_KEY=sk-or-secret-value, MB_NO_SHELL_ENV=1 */ };
    run(&["agent", "install", "cursor", "--no-skills"]);
    let cursor: Value = serde_json::from_str(&read(&dir.path().join(".cursor/mcp.json"))).unwrap();
    assert_eq!(cursor["mcpServers"]["mockingbird"]["env"]["OPENROUTER_API_KEY"], "${env:OPENROUTER_API_KEY}");
    run(&["agent", "install", "codex", "--no-skills"]);
    assert!(read(&dir.path().join(".codex/config.toml")).contains("env_vars = [\"OPENROUTER_API_KEY\"]"));
    let out = run(&["agent", "install", "claude-desktop", "--no-skills"]);
    assert!(out["passthrough_unsupported"].as_str().unwrap().contains("keys setup"));
    for f in [".cursor/mcp.json", ".codex/config.toml", ".mcp.json"] {
        let p = dir.path().join(f);
        if p.exists() { assert!(!read(&p).contains("sk-or-secret-value"), "{f}"); }
    }
    // no config → no env
    let bare = tempdir();
    // run agent install cursor --no-skills in `bare`; assert mcpServers.mockingbird has no "env"
}
```

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test -p mockingbird --lib passthrough && cargo test -p mockingbird --test agent_toolkit agent_install_passes`
Expected: FAIL (signature mismatch / no `env`).

- [ ] **Step 3: Implement** the Produces items. `install` computes `names = default_profile_env_names(&args.config) ∪ args.pass_env`.

- [ ] **Step 4: Run to verify they pass**

Run: `cargo test -p mockingbird --lib && cargo test -p mockingbird --test agent_toolkit`
Expected: PASS (including the existing `agent_install_writes_mcp_config_and_skills`).

- [ ] **Step 5: Commit**

```bash
git add crates/mockingbird/src/agents.rs crates/mockingbird/src/daemon.rs crates/mockingbird/tests/agent_toolkit.rs
git commit -m "feat(agents): pass provider key variables through agent MCP configs"
```

---

### Task 7: `file:` references refuse a key-store file others can read

**Files:**
- Modify: `crates/mb-ext/src/secrets.rs:19-46`
- Modify: `crates/mb-ext/src/lib.rs` (re-export)
- Modify: `crates/mockingbird/src/providers.rs` (`resolve` ~:458)
- Modify: `docs/configuration.md:9` (one sentence on the rule)
- Test: `crates/mb-ext/tests/defaults.rs`, `crates/mockingbird/src/providers.rs` `mod tests`

**Interfaces:**
- Produces (additive `mb-ext` API):
  - `pub fn in_key_store(path: &Path) -> bool`: the file's parent directory is named `secrets` and its parent is named `.mockingbird`.
  - `pub fn check_secret_file(path: &Path) -> Result<(), ExtError>`: on Unix, for a path `in_key_store`, `metadata.permissions().mode() & 0o077 != 0` → `Err(ExtError::Other(format!("secret file {} is readable by other users (mode {:o}); run chmod 600 {}", …)))`; all other paths and all non-Unix platforms → `Ok(())` (Windows relies on the owner-only ACL written in Task 8).
  - `EnvFileSecrets::resolve` calls `check_secret_file` before reading a `file:` path. The error message never contains the file's contents.
  - `providers::resolve` (local `doctor`/`llm test`/`start_server` checks) calls `mb_ext::check_secret_file` too, so a refused file shows as `set: false`.
- Decision recorded: the refusal applies to key-store files only. Docker, Compose (`file:/bootstrap/client_secret`) and Kubernetes mounts are commonly `0444`/`0644` and keep working.

- [ ] **Step 1: Write the failing tests** (`#[cfg(unix)]`)

```rust
// mb-ext/tests/defaults.rs
#[cfg(unix)]
#[tokio::test]
async fn key_store_files_must_be_private() {
    use std::os::unix::fs::PermissionsExt;
    let home = tempfile::tempdir().expect("tempdir");
    let store = home.path().join(".mockingbird/secrets");
    std::fs::create_dir_all(&store).expect("mkdir");
    let key = store.join("openrouter");
    std::fs::write(&key, "sk-test\n").expect("write");
    std::fs::set_permissions(&key, std::fs::Permissions::from_mode(0o644)).expect("chmod");
    let r = EnvFileSecrets.resolve(&format!("file:{}", key.display())).await;
    let err = r.expect_err("0644 must be refused").to_string();
    assert!(err.contains("chmod 600") && !err.contains("sk-test"), "{err}");
    std::fs::set_permissions(&key, std::fs::Permissions::from_mode(0o600)).expect("chmod");
    assert_eq!(EnvFileSecrets.resolve(&format!("file:{}", key.display())).await.expect("0600"), "sk-test");
    let shared = home.path().join("mounted");
    std::fs::write(&shared, "v").expect("write");
    std::fs::set_permissions(&shared, std::fs::Permissions::from_mode(0o644)).expect("chmod");
    assert_eq!(EnvFileSecrets.resolve(&format!("file:{}", shared.display())).await.expect("outside the store"), "v");
}
```
In `providers.rs` tests: a profile with `api_key: Some(format!("file:{}", key))` for a `0640` key-store file → `secret_status(&p)[0]["set"] == false`.

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test -p mb-ext --test defaults key_store && cargo test -p mockingbird --lib providers`
Expected: FAIL (0644 accepted).

- [ ] **Step 3: Implement** the Produces items; add to `docs/configuration.md` line 9: "A `file:` reference into the key store (`~/.mockingbird/secrets/`) is refused when the file is readable by group or others."

- [ ] **Step 4: Run to verify they pass**

Run: `cargo test -p mb-ext && cargo test -p mockingbird --lib`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/mb-ext crates/mockingbird/src/providers.rs docs/configuration.md
git commit -m "feat(ext): refuse key-store secret files that others can read"
```

---

### Task 8: The key store and the profile update

**Files:**
- Create: `crates/mockingbird/src/keys/mod.rs`, `crates/mockingbird/src/keys/store.rs`
- Modify: `crates/mockingbird/src/lib.rs` (`pub mod keys;`)
- Modify: `crates/mockingbird/src/onboard.rs` (profile update, sharing `configure_file`'s rewrite)
- Modify: `crates/mockingbird/src/daemon.rs` (`doctor` ~:396)
- Test: `keys/store.rs` `mod tests`, `onboard.rs` `mod tests`, `crates/mockingbird/tests/agent_toolkit.rs`

**Interfaces:**
- Consumes: `mb_ext::check_secret_file` (Task 7).
- Produces:
  - `pub fn default_store_dir() -> anyhow::Result<PathBuf>` → `<HOME or USERPROFILE>/.mockingbird/secrets`.
  - `pub fn valid_name(name: &str) -> bool`: 1–64 characters of `[A-Za-z0-9._-]`, not starting with `.`.
  - `pub fn write_secret(dir: &Path, name: &str, value: &str) -> anyhow::Result<PathBuf>`: rejects invalid names (`bail!("key names are 1-64 characters of [A-Za-z0-9._-], not starting with a dot")`) and empty values; creates `dir` and sets it to `0700` (Unix) even if it existed; writes `value.trim()` to `<dir>/.<name>.tmp-<uuid>` opened with `OpenOptionsExt::mode(0o600)`, then `rename`s it to `<dir>/<name>`; on Windows runs `icacls <file> /inheritance:r /grant:r <USERNAME>:F` (and the same on the directory with `(OI)(CI)F`) and fails when it exits non-zero. Returns the absolute file path. Never logs the value.
  - `pub fn list(dir: &Path) -> Vec<serde_json::Value>`: `[{ "name", "file", "private": bool }]` for regular files not starting with `.`, sorted by name (`private` = `check_secret_file` is `Ok`); never reads contents.
  - `pub fn file_reference(path: &Path) -> String` → `format!("file:{}", path.display())` (absolute path; see the conflict note below).
  - `pub fn resolve_profile(config: &Path, profile: Option<&str>) -> anyhow::Result<String>` in `onboard.rs`: the given profile (must exist), else `llm.default_profile`, else the only profile; otherwise `bail!("{} has no LLM profile to update; create one with `mockingbird llm configure` (MCP configure_llm)", ..)`.
  - `pub fn set_profile_api_key(config: &Path, profile: &str, reference: &str) -> anyhow::Result<Value>` in `onboard.rs`: sets `llm.profiles.<profile>.api_key = reference` using `configure_file`'s rewrite (leading comment block kept, `.yaml.bak` when other comments existed, original restored if the result does not load). Factor the shared part out as `fn rewrite_config(path: &Path, edit: impl FnOnce(&mut Map<String, Value>) -> anyhow::Result<()>) -> anyhow::Result<Option<PathBuf>>` (returns the backup path). Returns `{ "config", "profile", "backup" }`.
  - `doctor` output gains `"key_store": { "dir": …, "keys": list(dir) }`; each key with `private: false` adds the problem `key file {file} is readable by other users: chmod 600 {file}`. The "not set" problem text for a profile secret gains "…, or store the key with `mockingbird keys setup` (MCP open_key_setup)".
- Conflict noted: the spec writes the reference as `${file:~/.mockingbird/secrets/<name>}`, but `api_key` takes a bare `file:/path` reference (`${…}` is only for header templates) and nothing expands `~`. The profile therefore gets `file:<absolute path>`.

- [ ] **Step 1: Write the failing tests**

```rust
// keys/store.rs
#[test]
fn secret_names_are_plain() {
    for bad in ["", "../../.ssh/id", "a/b", "a\\b", ".hidden", &"x".repeat(65)] {
        assert!(!valid_name(bad), "{bad:?}");
    }
    assert!(valid_name("openrouter") && valid_name("OPENROUTER_API_KEY") && valid_name("a.b-c"));
    let dir = tempfile::tempdir().unwrap();
    assert!(write_secret(dir.path(), "../x", "v").is_err());
    assert!(std::fs::read_dir(dir.path()).unwrap().next().is_none());
}

#[cfg(unix)]
#[test]
fn written_secrets_are_private() {
    use std::os::unix::fs::PermissionsExt;
    let home = tempfile::tempdir().unwrap();
    let dir = home.path().join(".mockingbird/secrets");
    let path = write_secret(&dir, "openrouter", "  sk-abc \n").unwrap();
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "sk-abc");
    assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
    assert_eq!(std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777, 0o700);
    let listed = list(&dir);
    assert_eq!(listed, vec![json!({"name":"openrouter","file":path,"private":true})]);
    assert!(!serde_json::to_string(&listed).unwrap().contains("sk-abc"));
    write_secret(&dir, "openrouter", "sk-new").unwrap();
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "sk-new");
}
```
```rust
// onboard.rs
#[test]
fn set_profile_api_key_points_the_profile_at_the_file() {
    // config written by `init` with provider openrouter (profile "openrouter", default)
    assert_eq!(resolve_profile(&cfg, None).unwrap(), "openrouter");
    let out = set_profile_api_key(&cfg, "openrouter", "file:/home/u/.mockingbird/secrets/openrouter").unwrap();
    assert_eq!(out["profile"], "openrouter");
    let loaded = file_config::load(&cfg).unwrap();
    assert_eq!(loaded.llm.profiles["openrouter"].api_key.as_deref(), Some("file:/home/u/.mockingbird/secrets/openrouter"));
    assert!(resolve_profile(&cfg, Some("nope")).is_err());
    let before = std::fs::read_to_string(&cfg).unwrap();
    assert!(set_profile_api_key(&cfg, "nope", "file:/x").is_err());
    assert_eq!(std::fs::read_to_string(&cfg).unwrap(), before);
}
```
In `agent_toolkit.rs`: `doctor_reports_the_key_store` — with `HOME=<tempdir>` and a `0600` file written by `write_secret`, `doctor` JSON has `key_store.keys[0].name == "openrouter"` and `private == true`; after `chmod 0644`, `problems` contains `chmod 600`.

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test -p mockingbird --lib keys:: set_profile_api_key && cargo test -p mockingbird --test agent_toolkit doctor_reports_the_key_store`
Expected: FAIL (module missing).

- [ ] **Step 3: Implement** the Produces items. `set_profile_api_key` on a missing profile returns `Err` before writing.

- [ ] **Step 4: Run to verify they pass**

Run: `cargo test -p mockingbird --lib && cargo test -p mockingbird --test agent_toolkit`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/mockingbird/src/{keys,lib.rs,onboard.rs,daemon.rs} crates/mockingbird/tests/agent_toolkit.rs
git commit -m "feat(keys): private key store, profile reference and doctor report"
```

---

### Task 9: The key setup page, `mockingbird keys setup` and MCP `open_key_setup`

**Files:**
- Create: `crates/mockingbird/src/keys/page.rs`
- Modify: `crates/mockingbird/src/keys/mod.rs` (`KeysCommand`, `setup`)
- Modify: `crates/mockingbird/src/config.rs:84` (`Command::Keys`)
- Modify: `crates/mockingbird/src/main.rs` (dispatch)
- Modify: `crates/mockingbird/src/mcp.rs` (tool `open_key_setup`, `LOCAL_TOOLS`, `INSTRUCTIONS`)
- Modify: `plugins/mockingbird/skills/mockingbird-setup/SKILL.md` (~:73-77)
- Test: `keys/page.rs` `mod tests`, `crates/mockingbird/tests/agent_toolkit.rs`

**Interfaces:**
- Consumes: Task 8 (`write_secret`, `valid_name`, `file_reference`, `resolve_profile`, `set_profile_api_key`, `default_store_dir`).
- Produces:
  - `pub const PAGE_TIMEOUT: Duration = Duration::from_secs(600);`
  - `pub struct PageOptions { pub name: String, pub store_dir: PathBuf, pub profile: Option<ProfileTarget>, pub timeout: Duration }`, `pub struct ProfileTarget { pub config: PathBuf, pub profile: String }`.
  - `pub struct SavedKey { pub name: String, pub file: PathBuf, pub profile: Option<String>, pub config: Option<PathBuf> }` (no field holds the value; `Debug` derived).
  - `pub struct KeyPage { pub url: String, … }` with `pub async fn start(opts: PageOptions) -> anyhow::Result<KeyPage>` and `pub async fn wait(self) -> anyhow::Result<SavedKey>`; dropping a `KeyPage` (or `wait` returning) shuts the server down. `wait` past `timeout` → `Err("no key was saved within 10m; run it again")` (formatted with `humantime`).
  - `pub fn detach(page: KeyPage)`: spawns a task that waits (bounded by the timeout) and logs `saved key {name}` or the timeout at info; used when the caller does not wait.
  - `pub fn open_browser(url: &str) -> bool`: macOS `open <url>`, Windows `cmd /C start "" <url>`, other Unix `xdg-open <url>`; `true` when the command exits 0.
  - Server: binds `127.0.0.1:0`; token = `format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple())`; `url = http://127.0.0.1:<port>/<token>`. Routes `GET /{token}` (form) and `POST /{token}` (`axum::Form<KeyForm>`, `#[derive(Deserialize)] struct KeyForm { key: String }` with no `Debug`), `DefaultBodyLimit::max(16 * 1024)`. Checks, in order, for every request: path token compared with `mcp::same`-style constant-time equality (wrong → 404); `Host` header equals `127.0.0.1:<port>` (else 403); on POST, `Origin` absent or equal to `http://127.0.0.1:<port>` (else 403); after a save, POST → 410. Empty or >4096-character key → 400 with the form again. Every response carries `Cache-Control: no-store`, `Referrer-Policy: no-referrer`, `Content-Security-Policy: default-src 'none'; style-src 'unsafe-inline'; form-action 'self'`. The form: title "Mockingbird key setup", label "API key for <name>", `<input type="password" name="key" autocomplete="off" required>`, button "Save"; success page "Saved <name>. You can close this tab." — neither ever contains the submitted value.
  - On a valid POST: `write_secret`, then `set_profile_api_key(config, profile, &file_reference(&file))` when a target is set, then 200 and the done signal. A failure answers 500 with the error message (which never includes the value) and the page stays open for another try.
  - CLI: `Command::Keys(crate::keys::KeysCommand)`; `KeysCommand::Setup(SetupArgs)` with `SetupArgs { name: Option<String> /* positional; default: the profile name */, #[arg(short='c', long, default_value="mockingbird.yaml")] config: PathBuf, #[arg(long)] profile: Option<String>, #[arg(long)] no_open: bool, #[arg(long, default_value="10m")] timeout: humantime::Duration }`. When the config does not exist, the key is stored without a profile update and `name` is required (`bail!("name the key: mockingbird keys setup <name>")`). Prints `Open <url> to enter the key` on stderr, waits, prints `{ "saved", "file", "profile", "config" }` on stdout.
  - `pub async fn setup(args: &SetupArgs) -> anyhow::Result<Value>` in `keys/mod.rs`.
  - MCP tool `open_key_setup { name?, config?, profile?, open?: boolean }` (local; not read-only). Builds `PageOptions` like the CLI. When `open` (default true) and `open_browser` succeeds: waits (bounded by `PAGE_TIMEOUT`) and returns exactly `{ "saved": name }`. When `open: false` or the browser cannot be opened: `detach`es the page and returns `{ "url": url, "saved": null, "detail": "Ask the user to open this URL on this machine and paste the key there; the page stays open for 10 minutes. doctor lists the key once it is saved." }`.
  - `LOCAL_TOOLS` gains `"open_key_setup"`; `INSTRUCTIONS`' sentence "Never pass API keys as arguments…" gains "; if the key is not in this process's environment, `open_key_setup` lets the user type it into a local page that stores it in a private file".
  - Setup skill (~:73-77): after the `export` instruction add "If the agent's MCP server cannot see the variable (desktop agents), run `open_key_setup` (CLI `mockingbird keys setup`): the user types the key into a local page, Mockingbird stores it in `~/.mockingbird/secrets/<name>` (`0600`) and points the profile at it. The tool returns only the key's name."

- [ ] **Step 1: Write the failing tests** (`keys/page.rs`, `#[tokio::test]`, a temp `HOME`-like dir and a config written by `onboard::init` with provider `openrouter`)

```rust
#[tokio::test]
async fn post_with_token_writes_a_private_file() {
    let page = start(opts(&dir, Some(target))).await.unwrap();
    let r = reqwest::Client::new().post(&page.url).form(&[("key", "sk-or-v1-secret")]).send().await.unwrap();
    assert_eq!(r.status(), 200);
    let body = r.text().await.unwrap();
    assert!(body.contains("Saved openrouter") && !body.contains("sk-or-v1-secret"));
    let saved = page.wait().await.unwrap();
    assert_eq!(saved.name, "openrouter");
    assert_eq!(std::fs::read_to_string(&saved.file).unwrap(), "sk-or-v1-secret");
    #[cfg(unix)] { use std::os::unix::fs::PermissionsExt;
        assert_eq!(std::fs::metadata(&saved.file).unwrap().permissions().mode() & 0o777, 0o600); }
    let config = std::fs::read_to_string(&cfg).unwrap();
    assert!(config.contains(&format!("file:{}", saved.file.display())) && !config.contains("sk-or-v1-secret"));
}

#[tokio::test]
async fn wrong_token_is_refused() {
    let page = start(opts(&dir, None)).await.unwrap();
    let base = page.url.rsplit_once('/').unwrap().0;
    let r = reqwest::Client::new().post(format!("{base}/{}", "0".repeat(64))).form(&[("key", "k")]).send().await.unwrap();
    assert_eq!(r.status(), 404);
    assert!(!dir.join("openrouter").exists());
}

#[tokio::test]
async fn foreign_host_or_origin_is_refused() {
    let page = start(opts(&dir, None)).await.unwrap();
    let c = reqwest::Client::new();
    let r = c.post(&page.url).header("host", "evil.example").form(&[("key", "k")]).send().await.unwrap();
    assert_eq!(r.status(), 403);
    let r = c.post(&page.url).header("origin", "https://evil.example").form(&[("key", "k")]).send().await.unwrap();
    assert_eq!(r.status(), 403);
    assert!(!dir.join("openrouter").exists());
}

#[tokio::test]
async fn second_post_is_gone() {
    let page = start(opts(&dir, None)).await.unwrap();
    let c = reqwest::Client::new();
    assert_eq!(c.post(&page.url).form(&[("key", "first")]).send().await.unwrap().status(), 200);
    assert_eq!(c.post(&page.url).form(&[("key", "second")]).send().await.unwrap().status(), 410);
    assert_eq!(std::fs::read_to_string(dir.join("openrouter")).unwrap(), "first");
}

#[tokio::test]
async fn empty_key_and_timeout() {
    let page = start(PageOptions { timeout: Duration::from_millis(200), ..opts(&dir, None) }).await.unwrap();
    assert_eq!(reqwest::Client::new().post(&page.url).form(&[("key", "  ")]).send().await.unwrap().status(), 400);
    let err = page.wait().await.unwrap_err().to_string();
    assert!(err.contains("no key was saved within"), "{err}");
}
```
```rust
// agent_toolkit.rs
#[test]
fn open_key_setup_never_returns_the_key() {
    // dir with `init --provider openrouter`; MCP started with HOME=<dir>/home, MB_NO_SHELL_ENV=1
    let r = mcp.tool("open_key_setup", json!({ "open": false }));
    let url = r["url"].as_str().unwrap().to_string();
    assert!(url.starts_with("http://127.0.0.1:") && r["saved"].is_null());
    // POST key=sk-or-v1-from-browser to url with a small tokio runtime + reqwest; expect 200
    // poll `doctor` (≤ 5 s) until key_store.keys contains "openrouter"
    let doctor = mcp.tool("doctor", json!({}));
    assert!(!doctor.to_string().contains("sk-or-v1-from-browser"));
    assert!(!r.to_string().contains("sk-or-v1-from-browser"));
    assert!(!read(&dir.path().join("mockingbird.yaml")).contains("sk-or-v1-from-browser"));
    // the default profile now resolves: doctor's config.profiles[0].secrets[0].set == true
}
```

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test -p mockingbird --lib keys::page && cargo test -p mockingbird --test agent_toolkit open_key_setup`
Expected: FAIL (module and tool missing).

- [ ] **Step 3: Implement** `keys/page.rs`, `KeysCommand`/`setup`, the `Command::Keys` variant (help: "Store an LLM key in a private file through a local page, and point the profile at it."), the `main.rs` arm `Command::Keys(KeysCommand::Setup(args)) => print_json(keys::setup(&args).await)`, the MCP tool, and the skill text above.

- [ ] **Step 4: Run to verify they pass**

Run: `cargo test -p mockingbird --lib && cargo test -p mockingbird --test agent_toolkit`
Expected: PASS.

- [ ] **Step 5: Manual check** (macOS): `cargo run -- keys setup demo --no-open -c /tmp/none.yaml` prints the URL; open it, save a dummy value, the command prints `{"saved":"demo",…}` and `ls -l ~/.mockingbird/secrets/demo` shows `-rw-------`. Delete the dummy file afterwards with `rm ~/.mockingbird/secrets/demo`.

- [ ] **Step 6: Commit**

```bash
git add crates/mockingbird plugins/mockingbird/skills/mockingbird-setup/SKILL.md
git commit -m "feat(keys): local key setup page, keys setup and open_key_setup"
```

---

### Task 10: Documentation, changelog, gates and the pull request

**Files:**
- Modify: `CHANGELOG.md` (`## [Unreleased]`)
- Modify: `docs/agents.md` (key passthrough table and the key setup page; tool rows from Tasks 2, 4, 5, 9)
- Modify: `README.md` only if it lists MCP tools or `agent install` flags (check with `grep -n "agent install\|open_key_setup\|list_requests" README.md`)

**Interfaces:**
- Consumes: everything above.

- [ ] **Step 1: Write the docs**

`docs/agents.md`: the passthrough table from Task 6 (agent → what `agent install` writes), `--config`/`--pass-env`, and a "Key setup page" section (`mockingbird keys setup`, MCP `open_key_setup`, the store path and modes, the `file:` refusal rule, `doctor`'s `key_store`). CHANGELOG `### Added`: draft MCP tools (`get_draft`, `edit_draft`, `approve_draft`), `import_contract` `urls`/`mode`, `get_request` and Admin API `GET …/requests/{id}` and `?fields=summary`, `proxy_env` and `start_server` `proxy`, `agent install` key passthrough, `mockingbird keys setup` / `open_key_setup`, `doctor` `key_store`. `### Changed`: `list_requests` returns summaries unless `include_bodies`; `validate_contract` lists the first 20 operations with counts; background-server logs carry no ANSI codes; server tools find the state next to the started config; skills recommend proxy mode; `file:` references into `~/.mockingbird/secrets` must be `0600` (`mb-ext`: new `check_secret_file`, `in_key_store`, additive).

- [ ] **Step 2: Run the local gates**

Run:
```bash
cargo fmt --all --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-features
cargo check -p mockingbird --no-default-features
(cd crates/mb-console/web && npm run typecheck && npm run lint && npm test && npm run check:api)
```
Expected: all pass, except the known `tls::llm_client_trusts_outbound_ca` (macOS) and flaky `graphql::rejections_are_recorded`; rerun the latter once.

- [ ] **Step 3: Commit**

```bash
git add CHANGELOG.md docs/agents.md README.md
git commit -m "docs: WS5 agent and MCP ergonomics"
```

- [ ] **Step 4: Push and open the PR** (do not merge)

```bash
git push -u origin feat/ws5-agent-mcp
gh pr create --base main --title "WS5: agent and MCP ergonomics" --body "$(cat <<'EOF'
Implements WS5 of docs/superpowers/specs/2026-10-22-improvements-backlog-design.md (5.1-5.5).

- Draft tools, request-log summaries, capped contract summaries, plain logs, state dir next to the config
- proxy_env / start_server proxy, proxy-first skills
- agent install key passthrough (sources for each agent's interpolation syntax: …)
- keys setup page and private key store; file: refusal for key-store files

mb-ext: additive (check_secret_file, in_key_store).

🤖 Generated with [Claude Code](https://claude.com/claude-code)
EOF
)"
```
Expected: the PR URL; CI starts.
