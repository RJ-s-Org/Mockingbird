# Mockingbird P3 Console and Auth Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Put the P2 platform behind real identity and give it a UI. This phase adds:
- an embedded React/TanStack console;
- OIDC login (Rauthy by default), bearer JWTs and API keys, with per-workspace RBAC;
- multi-workspace control-plane resources;
- per-mock data-plane auth;
- an audit log with JSONL export and a webhook;
- a live request tail over SSE;
- a Compose stack that runs Rauthy.

**Architecture:**
- **New crates:**
  - `mb-auth` (no axum): roles, principals, claim mapping, API keys, server-side admin sessions, an OIDC relying party (`openidconnect`) and a bearer-JWT verifier (`jsonwebtoken`). Its `testing` feature provides a wiremock OIDC provider.
  - `mb-console`: rust-embed serving of the Vite build. The TypeScript source lives in `crates/mb-console/web/`.
- **Extended crates:**
  - `mb-store` gains workspaces, API keys, admin sessions and audit events.
  - `mb-mcm`/`mb-import` carry each operation's security requirements.
  - `mb-protocols` enforces data-plane auth.
  - `mb-admin` moves every resource under `/api/v1/workspaces/{ws}`, keeps the P2 paths as aliases, enforces RBAC through an extractor, and adds audit, API keys, the SSE tail and the login endpoints.
- **The binary** wires authentication, the host guard, the console and the background jobs.

**Tech Stack:** P2 stack, plus:
- Rust crates:
  - `openidconnect` 4.0, default features off; HTTP goes through our reqwest 0.13 client.
  - `jsonwebtoken` 11.1, feature `aws_lc_rs`.
  - `rust-embed` 8.13.
  - `hmac` 0.12, `subtle` 2, `base64` 0.22.
  - `rsa` 0.9 (`pem` feature), only for the `testing` provider.
  - `tokio-stream` 0.1 (`sync` feature) for SSE.
- Console:
  - Node 22 / npm 10.
  - react 19.3, @tanstack/react-router 1.170 (code-based route tree), @tanstack/react-query 5.104, @tanstack/react-table 9.3.
  - vite 8.3 with @vitejs/plugin-react 6, tailwindcss 4.3 via @tailwindcss/vite.
  - openapi-typescript 7.13, openapi-fetch 0.17.
  - vitest 5.0 with jsdom, @testing-library/react 16 and @testing-library/user-event 14.
  - eslint 10 with typescript-eslint 8 and eslint-plugin-react-hooks 7.
  - @playwright/test 1.64, @axe-core/playwright 4.14.
- Rauthy: `ghcr.io/sebadob/rauthy:0.37.1`.

**Spec:** `docs/superpowers/specs/2026-10-14-mockingbird-platform-design.md`. This plan covers the P3 row of §16 and implements §§4.4 (console), 8.1 (Workspace, ApiKey, AuditEvent, `data_plane_auth`), 11.1, 11.2, 11.3 (audit), 12 (Compose) and 14 (live request log). The controller's design decisions (`p3-decisions.md`) are binding and are restated, with exact values, in the Global Constraints and the tasks.

## Global Constraints

### Carried over and crate boundaries
- All P1 and P2 Global Constraints still hold except "workspace is the constant `default`". That includes: no `unwrap()`/`expect()` outside tests; `cargo clippy --workspace --all-targets --all-features -- -D warnings`; `cargo fmt`; problem+json admin errors; secrets only as `env:`/`file:` refs; Postgres tests gated by `MB_TEST_POSTGRES_URL` and run with `scripts/test-postgres.sh`.
- New crate names, exactly: `mb-auth`, `mb-console`.
  - `mb-auth` depends on `mb-store` and must not depend on axum. Extractors and middleware live in `mb-admin`.
  - `mb-store` and `mb-engine` stay axum-free.
  - `mb-protocols` must not depend on `mb-store` or `mb-auth`.

### Workspaces and roles
- Workspace names use the mock-name rule `^[a-z0-9][a-z0-9-]{0,62}$`. Workspace `default` always exists and cannot be deleted (409).
- Route prefixes are globally unique across workspaces (409 on collision). Mock and profile names are unique per workspace.
- Roles: `viewer < editor < admin < owner`, plus the global `sysadmin` flag. Wire format is snake_case.
- Permissions and their minimum role:

  | Permission | Minimum role | Covers |
  |---|---|---|
  | `Read` | viewer | read everything in the workspace except API keys |
  | `Edit` | editor | mocks, contracts, scenarios, session seed/reset |
  | `ManageProfiles` | admin | LLM profiles |
  | `ManageApiKeys` | admin | API keys, including listing them |
  | `ManageWorkspace` | owner | workspace PATCH/DELETE |

  Creating workspaces and reading the global audit log require `sysadmin`. A sysadmin has the effective role `owner` in every workspace.
- Group claim strings: `<prefix>:<workspace>:<role>` and `<prefix>:sysadmin`. Defaults: `roles_claim: groups`, `group_prefix: mb`. `auth.sysadmin_subjects` lists `sub` values that are always sysadmin.

### API keys and sessions
- API key token: `mbk_<key_id>_<secret>`.
  - `key_id`: 12 chars from `abcdefghijklmnopqrstuvwxyz234567`.
  - `secret`: 32 random bytes, base64url without padding (43 chars, may contain `_` and `-`).
  - Storage: only the hex sha256 of the 32 secret bytes. Compare in constant time (`subtle`).
  - Scopes: `admin` and `data-plane` (wire strings). Default: `[admin]`.
  - `last_used_at` is written at most once per 60 s.
- Admin session cookie `mb_session`: `HttpOnly; SameSite=Lax; Path=/; Max-Age=<ttl>`, plus `Secure` when `auth.external_url` is https.
  - The value is 32 random bytes, base64url without padding. The DB stores its hex sha256.
  - TTL comes from `auth.session_ttl` (default 12h) and is absolute. `last_seen` is updated at most once per 60 s.
- Login state cookie: `mb_login=<state>; HttpOnly; SameSite=Lax; Path=/auth; Max-Age=600` (plus `Secure` as above).

### Auth modes and admin endpoints
- `auth.mode: none | oidc`, default `none`.
  - `none`: every admin request is the Local principal (`subject "local"`, kind `local`, sysadmin). A non-loopback bind still needs `--insecure-no-auth`.
  - `oidc`: requires `auth.oidc` and `auth.external_url`, and may bind non-loopback without the flag.
- Public on the admin port in every mode: `/healthz`, `/readyz`, `/auth/login`, `/auth/callback`, `/api/openapi.json`. `/metrics` is public unless `auth.public_metrics: false`.
- Unauthenticated responses:
  - API: 401 problem with `WWW-Authenticate: Bearer realm="mockingbird"`.
  - Console navigation (GET with `Accept` containing `text/html`): 302 to `/auth/login?return_to=<path+query>`.
- An insufficient role gets a 403 problem. A principal with no role in a workspace gets 403 whether or not the workspace exists; a sysadmin gets 404 for a missing one.
- Bearer credentials: a token starting `mbk_` is an API key and must have scope `admin`. Anything else is a JWT, validated only in `oidc` mode:
  - `iss` must equal `auth.oidc.issuer`;
  - `aud` must contain `auth.oidc.audience`, default `client_id`;
  - `exp`/`nbf` are checked with 60 s leeway;
  - allowed algorithms: RS256, ES256, EdDSA;
  - JWKS is cached and refetched on an unknown `kid` at most once per 30 s.
- Admin API paths:
  - Canonical: `/api/v1/workspaces/{ws}/{mocks|llm-profiles|api-keys|audit}…`.
  - P2 aliases `/api/v1/mocks…` and `/api/v1/llm-profiles…` act on `default`, use the same handlers, and are marked `deprecated` in OpenAPI.
  - Also: `/api/v1/workspaces`, `/api/v1/workspaces/{ws}`, `/api/v1/me`, `/api/v1/audit` (sysadmin).
  - Auth endpoints: `GET /auth/login?return_to=`, `GET /auth/callback`, `POST /auth/logout` (Content-Type `application/json`).
  - `return_to` must start with `/`, must not start with `//` or `/\`, and must have no scheme; otherwise 400.

### Audit
- Actions:
  - `workspace.create|update|delete`
  - `mock.create|update|delete`
  - `contract.upload|activate`
  - `scenario.put|delete`
  - `session.seed|reset`
  - `profile.put|delete`
  - `api_key.create|revoke`
  - `auth.login|logout|login_failed`
- Targets: `workspace:<ws>`, `mock:<name>`, `contract:<mock>@<version>`, `scenario:<mock>/<name>`, `session:<mock>/<session>`, `profile:<name>`, `api_key:<key_id>`, `user:<subject>`.
- Each successful mutation writes exactly one event, with before/after taken from the API view types. An event never contains a secret or a token.
- Retention: `audit.retention_days` (default 365), pruned daily.
- Webhook:
  - Headers `X-Mockingbird-Signature: sha256=<hex HMAC-SHA256(secret, body)>` and `Content-Type: application/json`.
  - Queue of 1024; up to 3 retries with backoff of 1 s, 2 s, 4 s; 5 s timeout per attempt.
  - When the queue is full the event is dropped and `mb_audit_webhook_dropped_total` is incremented. Delivery never blocks a request.

### Data-plane auth
- `data_plane_auth: { mode: none | api_key | passthrough_validate, header: x-api-key }`.
- `api_key` mode:
  - The key goes in `header` or in `?api_key=`.
  - It must be a non-revoked, unexpired key of the mock's workspace (or a sysadmin key) with scope `data-plane`.
  - Failure: 401 problem with `WWW-Authenticate: ApiKey realm="mockingbird", header="<header>"`.
  - The key never reaches the engine and is never recorded.

### Live tail
- `GET /api/v1/workspaces/{ws}/mocks/{mock}/requests/stream?operation=&session=&source=`, Read permission.
- SSE `event: request` carries one JSON record. A lagging subscriber gets `event: lagged` with `{"dropped":N}`. Keep-alive every 15 s.

### Console
- Served at `/` on the admin port:
  - `/assets/*` with `Cache-Control: public, max-age=31536000, immutable`;
  - `index.html` with `Cache-Control: no-cache`;
  - every console response with `Content-Security-Policy: default-src 'self'; frame-ancestors 'none'` and `X-Content-Type-Options: nosniff`;
  - SPA fallback for any GET not under `/api/`, `/auth/` or the ops paths.
- `cargo build` never needs Node: `crates/mb-console/build.rs` writes a placeholder `dist/index.html` when `dist/` is missing. `crates/mb-console/dist/` is gitignored.
- TypeScript is pinned to `~5.9.3`, not 7.0: openapi-typescript 7.13 peers `typescript ^5.x` and typescript-eslint peers `<6.1.0`.
- Local Playwright:
  - Runs against the binary with `auth.mode: none`.
  - Uses the pre-installed Chromium: when `MB_E2E_CHROMIUM` is set it is passed as `launchOptions.executablePath`. Locally that is `/opt/pw-browsers/chromium-1194/chrome-linux/chrome`, because Playwright 1.64 expects a newer browser build than the one installed.
  - CI runs `npx playwright install --with-deps chromium` instead.
- Docker is not available locally: Compose, Rauthy and the Rauthy Playwright flow are verified only by the CI `e2e` job.

## Review Focus

1. **`return_to` open redirect.** `//evil.example`, `/\evil.example`, `https://evil.example` and `javascript:alert(1)` must be rejected with 400 by `/auth/login`. The callback only ever redirects to the validated stored value. Test: Task 10 `return_to_open_redirect_rejected`.
2. **A data-plane API key leaks into the prompt or the request log.** `?api_key=` and the configured header must be stripped before the engine sees the request, and must never appear in the `RequestRecord`. Test: Task 5 `data_plane_key_never_reaches_prompt_or_record`.
3. **The same mock name in two workspaces.** Session state, the request log and the live tail must stay attributed to the right workspace, and neither mock may see the other's state. Test: Task 6 `same_name_two_workspaces_isolated`.
4. **A key used on the wrong plane.**
   - A `data-plane`-only key on the Admin API → 401.
   - An `admin`-only key on the data plane → 401.
   - A revoked or expired key → 401 on both planes.

   Tests: Task 3 `data_plane_key_cannot_authenticate_admin`, Task 6 `admin_scoped_key_rejected_on_data_plane`.
5. **The admin session expires while the console is open.** The next API call returns 401, and the console must send the browser to `/auth/login?return_to=<current location>` (once, with no redirect loop) instead of rendering a broken page. Test: Task 12 `client redirects to login on 401`.

---

## File Structure

```
crates/mb-store/migrations/{sqlite,postgres}/0002_workspaces.sql    workspaces, mocks.data_plane_auth
crates/mb-store/migrations/{sqlite,postgres}/0003_auth_audit.sql    api_keys, admin_sessions, audit_events
crates/mb-store/src/{lib.rs,model.rs,sqlite.rs,postgres.rs}
crates/mb-store/tests/common/conformance.rs                          + workspace/key/session/audit cases
crates/mb-auth/src/{lib.rs,role.rs,principal.rs,mapping.rs,apikey.rs,session.rs,authenticator.rs,error.rs}
crates/mb-auth/src/{jwt.rs,oidc.rs,http.rs,testing.rs}
crates/mb-auth/tests/{core.rs,oidc.rs}
crates/mb-mcm/src/lib.rs                                             SecurityRequirement
crates/mb-import/src/{openapi3.rs,swagger2.rs,security.rs}
fixtures/openapi/secured-3.0.yaml, fixtures/swagger/secured-2.0.json
crates/mb-protocols/src/{lib.rs,record.rs,dp_auth.rs,http_rest/handler.rs}
crates/mockingbird/src/{registry.rs,runtime.rs,recorder.rs,dp_keys.rs,file_config.rs,reconcile.rs,config.rs,main.rs,host_guard.rs,client.rs,ops.rs,maintenance.rs}
crates/mb-admin/src/{lib.rs,access.rs,workspaces.rs,apikeys.rs,audit.rs,webhook.rs,live.rs,auth_routes.rs,openapi.rs,…existing}
crates/mb-admin/tests/{api.rs,rbac.rs,workspaces.rs,audit.rs,apikeys.rs,live.rs,auth_flow.rs}
crates/mb-console/{Cargo.toml,build.rs,src/lib.rs,tests/serve.rs}
crates/mb-console/web/{package.json,package-lock.json,tsconfig.json,vite.config.ts,eslint.config.js,index.html,playwright.config.ts,playwright.rauthy.config.ts}
crates/mb-console/web/src/{main.tsx,router.tsx,index.css,api/,auth/,components/,pages/,test/}
crates/mb-console/web/e2e/{mockingbird.e2e.yaml,shell.spec.ts,mocks.spec.ts,runtime.spec.ts,admin.spec.ts}
crates/mb-console/web/e2e-rauthy/full-flow.spec.ts
deploy/compose/{docker-compose.yml,mockingbird.yaml,rauthy-bootstrap.sh,README.md}
.github/workflows/ci.yml, Dockerfile, .dockerignore, .gitignore, deny.toml, README.md
```

---

### Task 1: Store: workspaces, global prefix lookup, data-plane auth column

**Files:**
- Create: `crates/mb-store/migrations/{sqlite,postgres}/0002_workspaces.sql`
- Modify: `crates/mb-store/src/{lib.rs,model.rs,sqlite.rs,postgres.rs}`, `crates/mb-store/tests/common/conformance.rs`, and every `NewMock { .. }` literal in the workspace (add `data_plane_auth: Default::default()`)

**Interfaces:**
- Produces (`model.rs`; derives as the existing model types):

```rust
pub struct Workspace { pub name: String, pub display_name: String, pub created_at: DateTime<Utc> }
#[derive(Default)] #[serde(rename_all = "snake_case")]
pub enum DataPlaneAuthMode { #[default] None, ApiKey, PassthroughValidate }
pub struct DataPlaneAuthConfig { #[serde(default)] pub mode: DataPlaneAuthMode,
    #[serde(default = "DataPlaneAuthConfig::default_header")] pub header: String /* "x-api-key" */ }
impl Default for DataPlaneAuthConfig { /* None, "x-api-key" */ }
// Mock and NewMock gain: #[serde(default)] pub data_plane_auth: DataPlaneAuthConfig
// ControlChange gains: Workspace { name: String }
```

- `ControlStore` additions:

```rust
async fn list_workspaces(&self) -> Result<Vec<Workspace>, StoreError>;                 // name asc
async fn get_workspace(&self, name: &str) -> Result<Option<Workspace>, StoreError>;
async fn create_workspace(&self, name: &str, display_name: &str) -> Result<Workspace, StoreError>; // Conflict if exists
async fn update_workspace(&self, name: &str, display_name: &str) -> Result<Workspace, StoreError>; // NotFound
async fn delete_workspace(&self, name: &str, force: bool) -> Result<bool, StoreError>;
async fn list_all_mocks(&self) -> Result<Vec<Mock>, StoreError>;                        // every workspace, (workspace, name) asc
```

- Free functions in `lib.rs`:
  - `pub fn validate_workspace_name(name: &str) -> Result<(), StoreError>` (the mock-name rule);
  - `pub fn validate_header_name(name: &str) -> Result<(), StoreError>` (a lowercase RFC 9110 token, 1–64 chars).

Rules:
- **Migration 0002:**
  - Creates `workspaces(name PK, display_name, created_at)`.
  - Inserts `default` (display name `Default`), plus every distinct `workspace` already present in `mocks` or `llm_profiles`.
  - Adds `mocks.data_plane_auth`: TEXT (SQLite) or JSON (Postgres), NOT NULL, default `{"mode":"none","header":"x-api-key"}`.
- `create_mock`, `put_profile` and (Task 2) `create_api_key` → `NotFound("workspace …")` when the workspace does not exist. Checked in code; no FK.
- `create_mock`/`update_mock` validate `data_plane_auth.header` with `validate_header_name` (`Invalid` otherwise).
- **`delete_workspace`:**
  - `default` → `Conflict("the default workspace cannot be deleted")`.
  - Non-empty (any mock or profile) without `force` → `Conflict`.
  - With `force`, deletes its mocks (cascading), profiles and (after Task 2) API keys in one transaction. Audit events are kept.
  - Publishes `Workspace{name}` plus `Mock{..}` for each deleted mock.
- `create_workspace`/`update_workspace` publish `ControlChange::Workspace`.

- [ ] **Step 1: Add failing conformance cases** to `run_all`:
  - `default_workspace_exists`
  - `workspace_crud_and_conflict` (create `team-a`; again → `Conflict`; update display name; list order)
  - `invalid_workspace_name_rejected` (`"Team A"` → `Invalid`)
  - `delete_default_workspace_conflicts`
  - `delete_nonempty_workspace_requires_force` (with a mock: `force=false` → `Conflict`; `force=true` → true, and the mock is gone)
  - `mock_in_missing_workspace_not_found`
  - `same_mock_name_in_two_workspaces` (both created; `list_all_mocks` returns both, sorted)
  - `data_plane_auth_round_trips` (`ApiKey` with header `x-key` persists; header `"Bad Header"` → `Invalid`)
  - `workspace_changes_are_published`
- [ ] **Step 2:** Run `cargo test -p mb-store --test sqlite`. Expected: FAIL (methods missing).
- [ ] **Step 3:** Implement both backends, both migrations and the model changes. Then update the `NewMock` literals across the workspace (`mb-admin`, `mockingbird`, tests).
- [ ] **Step 4:** Run `cargo test --workspace --all-features && scripts/test-postgres.sh -p mb-store`. Expected: all pass.
- [ ] **Step 5:** Commit: `feat(store): workspaces, global mock listing, data-plane auth settings`.

---

### Task 2: Store: API keys, admin sessions, audit events

**Files:**
- Create: `crates/mb-store/migrations/{sqlite,postgres}/0003_auth_audit.sql`
- Modify: `crates/mb-store/src/{lib.rs,model.rs,sqlite.rs,postgres.rs}`, `crates/mb-store/tests/common/conformance.rs`

**Interfaces:**
- Consumes: `Workspace`, `delete_workspace` (Task 1).
- Produces (model):

```rust
pub struct ApiKey { pub key_id: String, pub workspace: String, pub name: String, pub role: String /* viewer|editor|admin|owner */,
    pub scopes: Vec<String> /* "admin" | "data-plane" */, pub sysadmin: bool, pub secret_hash: String /* hex sha256 */,
    pub created_by: String, pub created_at: DateTime<Utc>, pub expires_at: Option<DateTime<Utc>>,
    pub last_used_at: Option<DateTime<Utc>>, pub revoked_at: Option<DateTime<Utc>> }
pub struct AdminSession { pub id_hash: String, pub principal: Value, pub created_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>, pub last_seen: DateTime<Utc> }
pub struct AuditEvent { pub id: i64, pub workspace: Option<String>, pub actor_subject: String, pub actor_kind: String,
    pub action: String, pub target: String, pub before: Option<Value>, pub after: Option<Value>,
    pub request_id: Option<String>, pub at: DateTime<Utc> }
pub struct NewAuditEvent { /* AuditEvent minus id */ }
pub struct AuditFilter { pub workspace: Option<String>, pub action: Option<String>, pub actor: Option<String>,
    #[serde(default = …)] pub limit: u32 /* default 100, clamp 1..=1000 */, pub before_id: Option<i64> }
impl AuditFilter { pub const DEFAULT_LIMIT: u32 = 100; pub const MAX_LIMIT: u32 = 1000; pub fn effective_limit(&self) -> u32; }
```

- `ControlStore` additions:

```rust
async fn create_api_key(&self, k: &ApiKey) -> Result<(), StoreError>;          // NotFound if workspace missing; Conflict on key_id
async fn get_api_key(&self, key_id: &str) -> Result<Option<ApiKey>, StoreError>;
async fn list_api_keys(&self, ws: &str) -> Result<Vec<ApiKey>, StoreError>;    // created_at desc, revoked included
async fn revoke_api_key(&self, ws: &str, key_id: &str) -> Result<bool, StoreError>; // sets revoked_at; false if absent/already revoked
async fn touch_api_key(&self, key_id: &str, at: DateTime<Utc>) -> Result<(), StoreError>;
async fn create_admin_session(&self, s: &AdminSession) -> Result<(), StoreError>;
async fn get_admin_session(&self, id_hash: &str) -> Result<Option<AdminSession>, StoreError>;
async fn touch_admin_session(&self, id_hash: &str, at: DateTime<Utc>) -> Result<(), StoreError>;
async fn delete_admin_session(&self, id_hash: &str) -> Result<bool, StoreError>;
async fn delete_expired_admin_sessions(&self, now: DateTime<Utc>) -> Result<u64, StoreError>;
async fn append_audit(&self, e: NewAuditEvent) -> Result<AuditEvent, StoreError>;
async fn list_audit(&self, f: &AuditFilter) -> Result<Vec<AuditEvent>, StoreError>; // id desc; workspace None = all; actor matches actor_subject
async fn prune_audit(&self, older_than: DateTime<Utc>) -> Result<u64, StoreError>;
```

- `delete_workspace(force=true)` now also deletes that workspace's `api_keys`.
- Index `audit_events(workspace, id)`.

- [ ] **Step 1: Add failing conformance cases:**
  - `api_key_create_get_list_revoke` (revoke twice → second false; the revoked key is still listed with `revoked_at`)
  - `api_key_in_missing_workspace_not_found`
  - `api_key_touch_updates_last_used`
  - `admin_session_lifecycle` (create, get, touch, delete)
  - `expired_admin_sessions_are_swept` (two sessions, one expired → sweep returns 1)
  - `audit_append_and_filter` (5 events across 2 workspaces and 2 actors; filter by workspace, action, actor; `before_id` paging; `None` workspace lists all)
  - `audit_prune_by_age`
  - `force_delete_workspace_removes_api_keys_keeps_audit`
- [ ] **Step 2:** Run `cargo test -p mb-store --test sqlite`. Expected: FAIL.
- [ ] **Step 3:** Implement both backends and migrations. Store `scopes` as a JSON array.
- [ ] **Step 4:** Run `cargo test -p mb-store && scripts/test-postgres.sh -p mb-store`. Expected: all pass.
- [ ] **Step 5:** Commit: `feat(store): API keys, admin sessions and audit events`.

---

### Task 3: `mb-auth` core: roles, principals, claim mapping, API keys, sessions

**Files:**
- Create: `crates/mb-auth/{Cargo.toml,src/lib.rs,src/error.rs,src/role.rs,src/principal.rs,src/mapping.rs,src/apikey.rs,src/session.rs,src/authenticator.rs}`, `crates/mb-auth/tests/core.rs`
- Modify: root `Cargo.toml` (member and workspace deps `mb-auth`, `subtle = "2"`, `base64 = "0.22"`, `hmac = "0.12"`)

**Interfaces:**
- Consumes: `ControlStore`, `ApiKey`, `AdminSession` (Task 2).
- Produces:

```rust
// role.rs  (Serialize/Deserialize snake_case, Copy, Ord)
pub enum Role { Viewer, Editor, Admin, Owner }
impl Role { pub fn as_str(self) -> &'static str; pub fn parse(s: &str) -> Option<Role>; pub fn allows(self, p: Permission) -> bool; }
pub enum Permission { Read, Edit, ManageProfiles, ManageApiKeys, ManageWorkspace }
impl Permission { pub fn min_role(self) -> Role; }
// principal.rs  (Serialize/Deserialize; stored as JSON in admin_sessions)
pub enum PrincipalKind { User, ApiKey, Local }                       // snake_case
pub struct Principal { pub subject: String, pub display_name: String, pub kind: PrincipalKind,
    pub sysadmin: bool, pub roles: BTreeMap<String, Role> }
impl Principal { pub fn local() -> Self;                              // "local", "Local user", Local, sysadmin
    pub fn role_in(&self, ws: &str) -> Option<Role>;                  // sysadmin → Some(Owner)
    pub fn can(&self, ws: &str, p: Permission) -> bool; }
// mapping.rs
pub struct RoleMapping { pub roles_claim: String, pub group_prefix: String, pub sysadmin_subjects: Vec<String> } // Default: groups, mb, []
impl RoleMapping { pub fn principal(&self, claims: &serde_json::Map<String, Value>) -> Result<Principal, AuthError>; }
// error.rs
#[derive(thiserror::Error, Debug)] pub enum AuthError { #[error("{0}")] Unauthenticated(String), #[error("{0}")] Forbidden(String),
    #[error("{0}")] Invalid(String), #[error("identity provider: {0}")] Upstream(String), #[error(transparent)] Store(#[from] StoreError) }
// apikey.rs
pub const API_KEY_PREFIX: &str = "mbk_";
#[serde(rename_all = "kebab-case")] pub enum KeyScope { Admin, DataPlane }
pub struct GeneratedKey { pub key_id: String, pub token: String, pub secret_hash: String }
pub fn generate_key() -> GeneratedKey;
pub fn parse_token(token: &str) -> Option<(String, [u8; 32])>;      // strip "mbk_", 12-char id, '_', 43-char secret decoding to 32 bytes
pub fn hash_secret(secret: &[u8; 32]) -> String;                     // hex sha256
pub struct NewKeyRequest { pub name: String, pub role: Role, pub scopes: Vec<KeyScope>, pub expires_at: Option<DateTime<Utc>>, pub sysadmin: bool }
#[derive(Clone)] pub struct ApiKeys { store: Arc<dyn ControlStore> }
impl ApiKeys { pub fn new(store: Arc<dyn ControlStore>) -> Self;
    pub async fn create(&self, creator: &Principal, ws: &str, req: NewKeyRequest) -> Result<(ApiKey, String), AuthError>; // (record, token)
    pub async fn verify(&self, token: &str, scope: KeyScope) -> Result<ApiKey, AuthError>;
    pub fn principal(key: &ApiKey) -> Principal; }                    // kind ApiKey, subject "api_key:<id>", display = name, roles {ws: role}
// session.rs
#[derive(Clone)] pub struct SessionManager { store: Arc<dyn ControlStore>, ttl: Duration, secure: bool }
impl SessionManager { pub fn new(store: Arc<dyn ControlStore>, ttl: Duration, secure: bool) -> Self;
    pub async fn create(&self, p: &Principal) -> Result<String, AuthError>;         // returns cookie value
    pub async fn lookup(&self, cookie_value: &str) -> Result<Option<Principal>, AuthError>;
    pub async fn destroy(&self, cookie_value: &str) -> Result<(), AuthError>;
    pub fn set_cookie(&self, value: &str) -> String;                                 // full Set-Cookie value per Global Constraints
    pub fn clear_cookie(&self) -> String;                                            // Max-Age=0
    pub fn spawn_sweeper(&self) -> tokio::task::JoinHandle<()>; }                   // delete_expired_admin_sessions hourly
pub const SESSION_COOKIE: &str = "mb_session";
// authenticator.rs
#[async_trait] pub trait Authenticate: Send + Sync {
    async fn authenticate(&self, headers: &http::HeaderMap) -> Result<Option<Principal>, AuthError>; }
pub struct LocalAuth;                                                                // always Ok(Some(Principal::local()))
#[async_trait] pub trait BearerVerifier: Send + Sync { async fn verify(&self, token: &str) -> Result<Principal, AuthError>; }
pub struct Authenticator { pub api_keys: ApiKeys, pub sessions: Option<SessionManager>, pub bearer: Option<Arc<dyn BearerVerifier>> }
```

Rules:
- **`RoleMapping::principal`:**
  - `sub` is required (`Unauthenticated` otherwise).
  - `display_name`: the first of `name`, `preferred_username`, `email`, `sub`.
  - The roles claim may be a string array, or a single string split on whitespace.
  - `<prefix>:<ws>:<role>` sets the highest role seen for that workspace. `<prefix>:sysadmin`, or `sub` ∈ `sysadmin_subjects`, sets `sysadmin`. Other strings are ignored.
- **`ApiKeys::create`:**
  - `role` must be ≤ `creator.role_in(ws)`, else `Forbidden`.
  - `sysadmin` requires `creator.sysadmin` (`Forbidden`).
  - Empty `scopes` → `[Admin]`.
  - `name` is 1–128 chars (`Invalid`).
  - `created_by = creator.subject`.
- **`ApiKeys::verify`:** returns `Unauthenticated` for any of:
  - an unparsable token;
  - an unknown id;
  - a hash mismatch (compared with `subtle::ConstantTimeEq`);
  - a revoked or expired key;
  - a missing scope.

  It touches `last_used_at` when that is `None` or older than 60 s.
- **`Authenticator::authenticate`:**
  - `Authorization: Bearer mbk_…` → `verify(.., Admin)` → `ApiKeys::principal`.
  - Any other `Bearer` → `self.bearer`, or `Unauthenticated("bearer tokens are not accepted")` when there is none.
  - Else the `mb_session` cookie → `sessions.lookup`. An unknown or expired cookie is `Ok(None)`, not an error.
  - Else `Ok(None)`.
- **`SessionManager::lookup`:** deletes an expired session and returns `None`. It touches `last_seen` at most once per 60 s.

- [ ] **Step 1: Write failing tests** (`tests/core.rs`, SQLite memory store):
  - `role_order_and_permissions` (the full matrix from Global Constraints)
  - `sysadmin_is_owner_everywhere`
  - `claims_map_to_roles` (`groups: ["mb:default:editor","mb:team-a:viewer","mb:team-a:admin","other:x"]` → `{default: Editor, team-a: Admin}`)
  - `space_separated_roles_claim`, `custom_prefix_and_claim` (`roles_claim: roles`, `group_prefix: mock`)
  - `sysadmin_from_group_and_subject`
  - `missing_sub_rejected`
  - `token_format_round_trips` (prefix, 12-char base32 id, 43-char secret; `parse_token` of mangled tokens → `None`)
  - `api_key_verify_ok_and_last_used_throttled` (two verifies within 60 s → one touch: `last_used_at` unchanged on the second)
  - `revoked_key_401`, `expired_key_401`, `wrong_secret_401`
  - `data_plane_key_cannot_authenticate_admin` (scopes `[data-plane]` → `Authenticator` returns `Unauthenticated`)
  - `key_role_capped_by_creator` (an editor creating an admin key → `Forbidden`)
  - `sysadmin_key_requires_sysadmin`
  - `session_create_lookup_destroy`
  - `expired_session_is_none_and_deleted` (ttl 1 s, sleep 1.2 s)
  - `session_ids_are_unique_and_hashed_at_rest` (100 creates → distinct; `get_admin_session(cookie)` → `None`, `get_admin_session(sha256(cookie))` → `Some`)
  - `set_cookie_attributes` (secure and non-secure variants match the Global Constraints string exactly)
  - `bearer_without_verifier_rejected`
- [ ] **Step 2:** Run `cargo test -p mb-auth`. Expected: FAIL (crate missing).
- [ ] **Step 3:** Implement. Draw random bytes from a CSPRNG (`rand::rng()` or `getrandom`).
- [ ] **Step 4:** Run `cargo test -p mb-auth`. Expected: all pass.
- [ ] **Step 5:** Commit: `feat(auth): roles, principals, claim mapping, API keys, admin sessions`.

---

### Task 4: `mb-auth` OIDC relying party, bearer JWT verification, test provider

**Files:**
- Create: `crates/mb-auth/src/{jwt.rs,oidc.rs,http.rs,testing.rs}`, `crates/mb-auth/tests/oidc.rs`
- Modify: `crates/mb-auth/{Cargo.toml,src/lib.rs}`, root `Cargo.toml`. Workspace deps:
  - `openidconnect = { version = "4.0", default-features = false }`
  - `jsonwebtoken = { version = "11.1", features = ["aws_lc_rs"] }`
  - `rsa = { version = "0.9", features = ["pem"] }`

  Feature `testing = ["dep:wiremock", "dep:rsa"]`.
- Modify: `deny.toml`. Add an `[advisories] ignore` entry for `RUSTSEC-2023-0071` with the reason "rsa Marvin timing attack affects private-key decryption; Mockingbird only verifies signatures (openidconnect) and signs in tests". Add it only if `cargo deny check advisories` reports it.

**Interfaces:**
- Consumes: `Principal`, `RoleMapping`, `AuthError`, `BearerVerifier` (Task 3).
- Produces:

```rust
// http.rs: openidconnect's async HTTP client over reqwest 0.13 (redirects disabled, 10 s timeout)
pub fn http_client() -> reqwest::Client;
pub async fn send(client: &reqwest::Client, req: openidconnect::HttpRequest) -> Result<openidconnect::HttpResponse, HttpError>;
#[derive(thiserror::Error, Debug)] #[error("{0}")] pub struct HttpError(pub String);
// jwt.rs
pub const ALLOWED_ALGS: [jsonwebtoken::Algorithm; 3] = [RS256, ES256, EdDSA];
pub struct JwtVerifier { /* issuer, audience, jwks_uri, mapping, http, cache: RwLock<(JwkSet, Option<Instant>)>, refetch_interval */ }
impl JwtVerifier { pub fn new(issuer: String, audience: String, jwks_uri: String, mapping: RoleMapping, http: reqwest::Client) -> Self;
    pub fn with_refetch_interval(self, d: Duration) -> Self; /* default 30 s */ }
#[async_trait] impl BearerVerifier for JwtVerifier { /* kind User; claims → mapping.principal */ }
// oidc.rs
pub struct OidcSettings { pub issuer: String, pub client_id: String, pub client_secret: Option<String> /* resolved value */,
    pub scopes: Vec<String>, pub audience: Option<String>, pub redirect_url: String, pub mapping: RoleMapping }
pub struct LoginRedirect { pub url: String, pub state: String }
pub struct CompletedLogin { pub principal: Principal, pub return_to: String }
pub struct OidcClient { /* openidconnect CoreClient, Arc<JwtVerifier>, pending: Mutex<HashMap<String, Pending>> */ }
impl OidcClient { pub async fn discover(settings: OidcSettings, http: reqwest::Client) -> Result<Self, AuthError>;
    pub fn begin_login(&self, return_to: &str) -> Result<LoginRedirect, AuthError>;      // validates return_to
    pub async fn complete_login(&self, state: &str, code: &str) -> Result<CompletedLogin, AuthError>;
    pub fn bearer(&self) -> Arc<JwtVerifier>; }                                         // jwks_uri from discovery
pub fn validate_return_to(s: &str) -> Result<String, AuthError>;                       // rules in Global Constraints → Invalid
// testing.rs (feature "testing")
pub struct MockOidcProvider { pub server: wiremock::MockServer, /* keys: Mutex<Vec<(kid, RsaPrivateKey)>> */ }
impl MockOidcProvider { pub async fn start() -> Self;                                   // discovery, JWKS, token, userinfo mounted
    pub fn issuer(&self) -> String; pub const CLIENT_ID: &'static str = "mockingbird"; pub const CLIENT_SECRET: &'static str = "test-secret";
    pub fn sign(&self, claims: serde_json::Value) -> String;                            // RS256 with the current kid
    pub fn sign_with(&self, header: jsonwebtoken::Header, key: &jsonwebtoken::EncodingKey, claims: Value) -> String;
    pub async fn rotate_key(&self) -> String;                                           // new kid; JWKS now serves only the new key
    pub async fn issue_code(&self, code: &str, claims: Value);                          // token endpoint answers id_token(claims) for this code
    pub fn claims(&self, sub: &str, groups: &[&str], nonce: Option<&str>) -> Value;     // iss, aud=CLIENT_ID, iat/exp now+300
    pub fn query_param(url: &str, name: &str) -> Option<String>; }
```

Rules:
- **`begin_login`:**
  - Authorization Code + PKCE S256, with a random `state` and nonce, and the configured scopes.
  - Stores `Pending{pkce_verifier, nonce, return_to, created}` keyed by `state`. Entries expire after 10 min; the map holds at most 10 000 entries and evicts the oldest.
- **`complete_login`:**
  - Removes the pending entry (one use only); an unknown or expired state → `Unauthenticated("unknown login state")`.
  - Exchanges the code, then verifies the ID token with openidconnect: signature, issuer, audience and nonce. A mismatch → `Unauthenticated`.
  - Maps the ID token's claims (standard plus additional, collected as a JSON map) through `RoleMapping`. When the roles claim is absent and the provider has a userinfo endpoint, the userinfo claims are merged in first.
- **`JwtVerifier`:**
  - `decode_header`; an algorithm outside `ALLOWED_ALGS`, or a missing `kid` → `Unauthenticated`.
  - The key comes from the cache. An unknown `kid` refetches JWKS only if the last fetch is older than `refetch_interval`.
  - `Validation` sets iss, aud, leeway 60 and `validate_nbf`.

- [ ] **Step 1: Write failing tests** (`tests/oidc.rs`, `--features testing`):
  - `discovery_and_login_round_trip`: `begin_login("/mocks")` → the URL has `code_challenge_method=S256`, `state` and `nonce`. `issue_code("c1", claims(.., nonce))`, then `complete_login(state, "c1")` → principal roles and `return_to == "/mocks"`.
  - `state_mismatch_rejected` (unknown state)
  - `state_is_single_use`
  - `nonce_mismatch_rejected` (ID token carries another nonce)
  - `return_to_validation` (accepts `/`, `/w/default/mocks?x=1`; rejects `//evil.example`, `/\evil.example`, `https://evil.example`, `javascript:alert(1)`, `""`)
  - `bearer_jwt_accepted`
  - `bearer_wrong_issuer_or_audience_401`
  - `expired_jwt_401` (exp now−120 s); `jwt_within_leeway_ok` (exp now−30 s)
  - `alg_none_and_hs256_rejected`
  - `jwks_rotation_refetches` (verify with kid1; `rotate_key`; a token with kid2 is accepted after the refetch)
  - `unknown_kid_refetch_throttled` (interval 30 s; two unknown-kid tokens → the JWKS mock receives exactly 2 requests: the initial fetch and one refetch)
- [ ] **Step 2:** Run `cargo test -p mb-auth --features testing --test oidc`. Expected: FAIL.
- [ ] **Step 3:** Implement. Generate the RSA key per provider with `rsa::RsaPrivateKey::new(&mut rsa::rand_core::OsRng, 2048)`. Expose n/e in JWKS as base64url, and sign through `EncodingKey::from_rsa_pem` (PKCS#8 PEM).
- [ ] **Step 4:** Run `cargo test -p mb-auth --all-features && cargo deny check`. Expected: all pass.
- [ ] **Step 5:** Commit: `feat(auth): OIDC login with PKCE, bearer JWT verification, mock provider`.

---

### Task 5: Contract security requirements and data-plane auth in the protocol layer

**Files:**
- Create: `crates/mb-import/src/security.rs`, `crates/mb-protocols/src/dp_auth.rs`, `fixtures/openapi/secured-3.0.yaml`, `fixtures/swagger/secured-2.0.json`
- Modify: `crates/mb-mcm/src/lib.rs`, `crates/mb-import/src/{lib.rs,openapi3.rs,swagger2.rs}`, `crates/mb-protocols/{Cargo.toml (async-trait, base64),src/lib.rs,src/record.rs,src/http_rest/handler.rs}`
- Test: `crates/mb-import/tests/security.rs`, `crates/mb-protocols/tests/data_plane_auth.rs`

**Interfaces:**
- Produces:

```rust
// mb-mcm (serde snake_case)
pub enum ApiKeyLocation { Header, Query, Cookie }
pub enum SecurityScheme { Bearer, Basic, ApiKey { location: ApiKeyLocation, name: String },
    OAuth2 { scopes: Vec<String> }, OpenIdConnect { scopes: Vec<String> } }
pub struct SecurityRequirement { pub schemes: Vec<SecurityScheme> }      // AND within; Operation.security is OR across
// Operation gains: #[serde(default, skip_serializing_if = "Vec::is_empty")] pub security: Vec<SecurityRequirement>
// mb-protocols/src/dp_auth.rs
pub enum DataPlaneAuthMode { None, ApiKey, PassthroughValidate }
pub struct DataPlaneAuth { pub mode: DataPlaneAuthMode, pub header: String }   // Default: None, "x-api-key"
#[async_trait] pub trait DataPlaneKeys: Send + Sync { async fn verify(&self, workspace: &str, key: &str) -> bool; }
pub fn passthrough_satisfied(security: &[SecurityRequirement], headers: &HeaderMap, query: &[(String, String)]) -> bool;
// MockRuntime gains: pub workspace: String ("default" in new()), pub data_plane_auth: DataPlaneAuth,
//                    pub data_plane_keys: Option<Arc<dyn DataPlaneKeys>>
// RequestRecord gains: pub workspace: String
```

Rules:
- **Importers:**
  - The operation-level `security` overrides the document-level one. `security: []` means none.
  - An empty requirement object `{}` makes the operation's auth optional: `passthrough_satisfied` → true.
  - Scheme mapping:
    - OAS3 `http` + `bearer` → `Bearer`; `http` + `basic` → `Basic`.
    - Swagger 2 `basic` → `Basic`.
    - `apiKey` → `ApiKey{in,name}`.
    - `oauth2` → `OAuth2{scopes}`; `openIdConnect` → `OpenIdConnect{scopes}`.
    - Unknown schemes are skipped, with the requirement kept only if it has other schemes.
- **`passthrough_satisfied` checks:**
  - `Bearer`/`OAuth2`/`OpenIdConnect` → `Authorization: Bearer <non-empty>`. OAuth scopes are not checked: the token is opaque to us.
  - `Basic` → `Authorization: Basic <b64>` that decodes (standard alphabet) to UTF-8 containing `:`.
  - `ApiKey` → the named header (case-insensitive), query parameter, or cookie in `Cookie`, with a non-empty value.
  - No requirements → true.
- **Handler, after quota admission (spec §7 order: tenant → quota → data-plane auth) and before overrides:**
  - **`ApiKey` mode.** The key is taken from `header` or the `api_key` query parameter. No key, no `data_plane_keys`, or `verify == false` → 401 problem `Unauthorized`, `WWW-Authenticate: ApiKey realm="mockingbird", header="<header>"`.
  - **Key stripping.** The `api_key` parameter is removed from the query map handed to the engine and from `recorded_query`. `header` is treated as sensitive for this runtime (not forwarded, not recorded).
  - **`PassthroughValidate` mode.** Unsatisfied → 401 problem `Unauthorized`. `WWW-Authenticate` names the first requirement's first scheme: `Bearer`, `Basic realm="mockingbird"`, or `ApiKey realm="mockingbird", <in>="<name>"`.
  - The session `SessionKey.workspace` and `RequestRecord.workspace` use `rt.workspace` (remove the `WORKSPACE` const).

- [ ] **Step 1: Write failing tests:**
  - `crates/mb-import/tests/security.rs`:
    - `oas3_security_compiled`: the fixture's ops cover global bearer, an op-level apiKey-in-query override, `security: []`, `{}`-optional, basic, an oauth2 with scopes, and a two-scheme AND requirement.
    - `swagger2_security_compiled`
    - `orders_snapshot_unchanged`: the existing insta snapshot passes untouched.
  - `crates/mb-protocols/tests/data_plane_auth.rs` (offline engine, a stub `DataPlaneKeys` accepting `good` for workspace `default`):
    - `api_key_header_accepted`
    - `api_key_query_accepted`
    - `missing_or_bad_key_401_with_www_authenticate`
    - `key_from_other_workspace_401` (stub keyed per workspace; runtime workspace `team-a`)
    - `data_plane_key_never_reaches_prompt_or_record` (scripted provider capturing the prompt plus a collecting recorder; `?api_key=good&x=1` → the prompt and the record's query contain `x` but not `good`; with header `x-key: good`, the recorded headers lack `x-key`)
    - `passthrough_bearer_basic_apikey` (each satisfied/unsatisfied case from the fixture, including malformed base64 → 401)
    - `passthrough_optional_and_unsecured_ops_pass`
    - `record_carries_workspace`
    - `none_mode_unchanged` (existing behaviour)
- [ ] **Step 2:** Run `cargo test -p mb-import --test security && cargo test -p mb-protocols --test data_plane_auth`. Expected: FAIL.
- [ ] **Step 3:** Implement.
- [ ] **Step 4:** Run `cargo test --workspace --all-features`. Expected: all pass (existing `RequestRecord` literals updated).
- [ ] **Step 5:** Commit: `feat(protocols): contract security requirements and per-mock data-plane auth`.

---

### Task 6: Multi-workspace runtime, registry, recorder and data-plane keys

**Files:**
- Create: `crates/mockingbird/src/dp_keys.rs`
- Modify: `crates/mockingbird/{Cargo.toml (mb-auth),src/lib.rs,src/registry.rs,src/runtime.rs,src/recorder.rs,src/ops.rs}`
- Test: `crates/mockingbird/tests/registry.rs` (existing, adapted), `crates/mockingbird/tests/workspaces.rs`

**Interfaces:**
- Consumes: `list_all_mocks`, `ControlChange::Workspace`, `Mock.data_plane_auth` (Task 1); `ApiKeys::verify`, `KeyScope` (Task 3); `DataPlaneKeys`, `DataPlaneAuth`, `MockRuntime.workspace`, `RequestRecord.workspace` (Task 5).
- Produces:

```rust
// dp_keys.rs
pub struct StoreDataPlaneKeys { keys: mb_auth::ApiKeys }
impl StoreDataPlaneKeys { pub fn new(store: Arc<dyn ControlStore>) -> Arc<Self>; }
#[async_trait] impl DataPlaneKeys for StoreDataPlaneKeys  // verify(token, DataPlane) and (key.workspace == ws || key.sysadmin)
// runtime.rs: RuntimeDeps gains `pub data_plane_keys: Option<Arc<dyn DataPlaneKeys>>`
// registry.rs
impl MockRegistry {
    pub async fn reload_mock(&self, deps: &RuntimeDeps, workspace: &str, name: &str);
    pub fn mounted(&self) -> Vec<(String, String, String)>;     // (workspace, name, prefix), longest prefix first
    pub fn failed_mocks(&self) -> Vec<(String, String)>;         // (label, error)
    pub fn degraded_mocks(&self) -> Vec<String>;                 // labels
}
pub fn mock_label(workspace: &str, name: &str) -> String;        // "name" for default, else "ws/name"
```

Rules:
- `reload_all` uses `list_all_mocks`. Mounted entries and the `failed` map are keyed by `(workspace, name)`. The watcher handles `Mock{workspace,name}` for every workspace, and `Workspace{..}`/`Profile{..}`/lagged by reloading everything.
- `build_runtime`:
  - sets `rt.workspace = mock.workspace` and maps `mock.data_plane_auth` to `mb_protocols::DataPlaneAuth`;
  - sets `rt.data_plane_keys = deps.data_plane_keys`;
  - loads profiles from `mock.workspace`.
  - The default profile is looked up in the mock's workspace first, then in `default`.
- `StoreRecorder` resolves mock ids by `(workspace, name)` and evicts the cache entry on `Mock{workspace,name}`.
- `/readyz` reasons use `mock_label`.

- [ ] **Step 1: Write failing tests** (`tests/workspaces.rs`; SQLite memory, offline):
  - `same_name_two_workspaces_isolated`: mock `orders` in `default` at `/a` and in `team-a` at `/b`, same fixture contract. `POST /a/orders` then `GET /b/orders` → the team-a list does not contain the created id. The request log of each mock has only its own entry, and live records carry the right `workspace`.
  - `profile_resolved_in_mock_workspace` (a profile with the same name in two workspaces pointing at two wiremock servers → each mock calls its own)
  - `default_profile_falls_back_to_default_workspace`
  - `api_key_mode_with_store_key`: a key with scopes `[data-plane]` in `team-a` → 200, then 401 once it is revoked.
  - `admin_scoped_key_rejected_on_data_plane`
  - `expired_data_plane_key_401`
  - `workspace_force_delete_unmounts`
  - Adapt `tests/registry.rs` to the new signatures.
- [ ] **Step 2:** Run `cargo test -p mockingbird --test workspaces --test registry`. Expected: FAIL.
- [ ] **Step 3:** Implement. In `build_app_with` set `data_plane_keys: Some(StoreDataPlaneKeys::new(store.clone()))`.
- [ ] **Step 4:** Run `cargo test --workspace --all-features`. Expected: all pass.
- [ ] **Step 5:** Commit: `feat(runtime): mocks from every workspace; data-plane API keys`.

---

### Task 7: Admin API: workspace scoping, RBAC, workspace endpoints, P2 aliases

**Files:**
- Create: `crates/mb-admin/src/{access.rs,workspaces.rs}`, `crates/mb-admin/tests/{rbac.rs,workspaces.rs}`
- Modify: `crates/mb-admin/{Cargo.toml (mb-auth, async-trait),src/lib.rs,src/openapi.rs,src/problem.rs,src/mocks.rs,src/contracts.rs,src/scenarios.rs,src/profiles.rs,src/sessions.rs,src/requests.rs}`, `crates/mb-admin/tests/api.rs` (harness via `AdminState::new`), `crates/mockingbird/src/config.rs` (construct with `AdminState::new`)

**Interfaces:**
- Consumes: `Principal`, `Permission`, `Role`, `Authenticate`, `LocalAuth`, `AuthError` (Task 3); workspace store methods and `DataPlaneAuthConfig` (Task 1).
- Produces:

```rust
// lib.rs
#[derive(Clone)] pub struct AdminState { pub store, pub state, pub api_secret_refs, pub auth: Arc<dyn mb_auth::Authenticate> }
impl AdminState { pub fn new(store: Arc<dyn ControlStore>, state: Arc<dyn StateStore>) -> Self; } // LocalAuth, default secret refs
pub fn openapi_document() -> utoipa::openapi::OpenApi;      // ApiDoc + deprecated aliases (used by /api/openapi.json and `mockingbird openapi`)
// access.rs
pub async fn authenticate(State(st): State<AdminState>, req: Request, next: Next) -> Response; // inserts Extension<Principal> or 401
pub struct CurrentPrincipal(pub Principal);                  // extractor from extensions; 401 if absent
pub struct WsAccess { pub principal: Principal, pub ws: String, pub request_id: String }
impl WsAccess { pub fn require(&self, p: Permission) -> ApiResult<()>; }   // 403 problem
impl FromRequestParts<AdminState> for WsAccess              // ws from RawPathParams "ws" or "default"; role check; workspace exists
impl From<AuthError> for Problem                             // Unauthenticated 401 (+WWW-Authenticate), Forbidden 403, Invalid 422/400, Upstream 502, Store per StoreError
// workspaces.rs
pub struct WorkspaceView { pub name: String, pub display_name: String, pub created_at: DateTime<Utc>, pub role: Option<Role> }
pub struct CreateWorkspace { pub name: String, pub display_name: Option<String> }  // display_name defaults to name
pub struct PatchWorkspace { pub display_name: String }
pub struct DeleteParams { pub force: Option<bool> }
```

Endpoints and permissions (canonical paths; every P2 path keeps working as an alias for `default`):

| Method & path | Permission |
|---|---|
| `GET /api/v1/workspaces` | any principal; lists only workspaces where `role_in` is `Some` |
| `POST /api/v1/workspaces` | sysadmin → 201; 409 exists; 422 invalid name |
| `GET /api/v1/workspaces/{ws}` | Read |
| `PATCH /api/v1/workspaces/{ws}` | ManageWorkspace |
| `DELETE /api/v1/workspaces/{ws}?force=` | ManageWorkspace; 409 default/non-empty; with force, `state.reset_mock` for each mock first |
| `GET …/workspaces/{ws}/mocks…`, contracts, scenarios, sessions, requests (reads) | Read |
| mock POST/PATCH/DELETE, contract upload/activate, scenario PUT/DELETE, session PUT/DELETE | Edit |
| `GET …/llm-profiles…` | Read |
| `PUT/DELETE …/llm-profiles/{name}` | ManageProfiles |

Rules:
- **Routing.** `router()` builds one resource router and nests it at both `/api/v1/workspaces/{ws}` and `/api/v1`.
  - Handlers take `WsAccess` plus named path structs (`#[derive(Deserialize)] struct MockPath { mock: String }`, etc.), replacing tuple `Path`s so both nestings deserialize.
  - Every store call uses `access.ws`.
- **Middleware.** The `authenticate` middleware is a `route_layer` on the `/api/v1` routers only. `/api/openapi.json` stays public.
  - Unknown `/api/*` paths → 404 problem via an explicit `/api/{*rest}` route.
  - Remove the router-wide `.fallback`, so the binary can set the console as the fallback service (Task 12). Keep `method_not_allowed_fallback`.
- **Mocks.**
  - `CreateMock`, `PatchMock` and `MockView` gain `data_plane_auth` (`DataPlaneAuthConfig` with `ToSchema`).
  - `ensure_prefix_free` uses `list_all_mocks`. A holder in another workspace → 409 whose detail names `<ws>/<mock>`.
  - Profile references are checked in `access.ws`.
- **OpenAPI.**
  - `#[utoipa::path]` uses the canonical `/api/v1/workspaces/{ws}/…` paths, with a `ws` path param.
  - `openapi_document()` copies each canonical `mocks`/`llm-profiles` path to its alias: the `ws` parameter is removed and every operation is marked `deprecated: true`, with operation ids suffixed `_default`.
  - The `/api/openapi.json` handler serves `openapi_document()`.

- [ ] **Step 1: Write failing tests:**
  - `tests/workspaces.rs`:
    - `workspace_crud`
    - `delete_default_409`, `delete_nonempty_requires_force`
    - `same_mock_name_two_workspaces_ok`
    - `prefix_collision_across_workspaces_409`
    - `alias_routes_act_on_default` (create via `/api/v1/mocks`, read via `/api/v1/workspaces/default/mocks/x` and vice versa)
    - `openapi_has_canonical_and_deprecated_alias_paths`
    - `list_shows_only_visible_workspaces`
  - `tests/rbac.rs` (a test `Authenticate` mapping `Authorization: Bearer <name>` to fixed principals: viewer/editor/admin/owner of `default`, an editor of `team-a`, a sysadmin; no header → `None`):
    - `missing_credentials_401_with_www_authenticate`
    - `viewer_can_read_cannot_mutate` (every mutating endpoint → 403)
    - `editor_cannot_touch_profiles` (PUT/DELETE profile → 403; GET → 200)
    - `admin_can_manage_profiles`
    - `owner_can_patch_and_delete_workspace_admin_cannot`
    - `cross_workspace_access_403` (team-a editor → `default` resources 403; a nonexistent workspace → 403)
    - `sysadmin_missing_workspace_404`
    - `only_sysadmin_creates_workspaces`
    - `openapi_json_is_public`
  - Existing `tests/api.rs` passes unchanged except for the harness constructor.
- [ ] **Step 2:** Run `cargo test -p mb-admin`. Expected: FAIL.
- [ ] **Step 3:** Implement.
- [ ] **Step 4:** Run `cargo test --workspace --all-features`. Expected: all pass.
- [ ] **Step 5:** Commit: `feat(admin): workspace-scoped API with RBAC; P2 paths as deprecated aliases`.

---

### Task 8: Audit log, webhook sink and API key endpoints

**Files:**
- Create: `crates/mb-admin/src/{audit.rs,webhook.rs,apikeys.rs}`, `crates/mb-admin/tests/{audit.rs,apikeys.rs}`
- Modify: `crates/mb-admin/{Cargo.toml (reqwest, hmac, sha2, metrics),src/lib.rs,src/openapi.rs}`, and every mutating handler in `mocks.rs`, `contracts.rs`, `scenarios.rs`, `profiles.rs`, `sessions.rs`, `workspaces.rs`

**Interfaces:**
- Consumes: `WsAccess`, `AdminState::new` (Task 7); `append_audit`, `list_audit`, `prune_audit`, `AuditFilter`, `NewAuditEvent` and the API key store methods (Task 2); `ApiKeys`, `NewKeyRequest`, `KeyScope`, `Role` (Task 3).
- Produces:

```rust
// audit.rs
#[derive(Clone)] pub struct Auditor { store: Arc<dyn ControlStore>, webhook: Option<WebhookSink> }
impl Auditor { pub fn new(store: Arc<dyn ControlStore>, webhook: Option<WebhookSink>) -> Self;
    pub async fn record(&self, e: NewAuditEvent); }   // append; on success hand to webhook; on error warn + mb_audit_write_errors_total
impl WsAccess { pub async fn audit(&self, auditor: &Auditor, action: &str, target: String, before: Option<Value>, after: Option<Value>); }
pub fn spawn_retention(store: Arc<dyn ControlStore>, retention_days: u32) -> tokio::task::JoinHandle<()>; // prune at start, then every 24 h
pub struct AuditEventView { /* AuditEvent fields */ }
// webhook.rs
#[derive(Clone)] pub struct WebhookSink { tx: mpsc::Sender<AuditEvent> }
impl WebhookSink { pub fn spawn(url: String, secret: Option<String>, http: reqwest::Client) -> Self;
    pub fn spawn_with(url: String, secret: Option<String>, http: reqwest::Client, backoff_base: Duration) -> Self;
    pub fn send(&self, e: AuditEvent); }              // try_send; Full → drop + mb_audit_webhook_dropped_total
pub fn signature(secret: &[u8], body: &[u8]) -> String;   // "sha256=<hex>"
// apikeys.rs
pub struct CreateApiKey { pub name: String, #[serde(default = viewer)] pub role: Role, #[serde(default)] pub scopes: Vec<KeyScope>,
    pub expires_at: Option<DateTime<Utc>>, #[serde(default)] pub sysadmin: bool }
pub struct ApiKeyView { pub key_id, pub name, pub role, pub scopes, pub sysadmin, pub created_by, pub created_at,
    pub expires_at, pub last_used_at, pub revoked_at }      // never secret_hash
pub struct CreatedApiKey { pub key: ApiKeyView, pub token: String }
// AdminState gains: pub audit: Auditor (AdminState::new → Auditor::new(store, None))
```

Endpoints:

| Method & path | Permission | Behaviour |
|---|---|---|
| `GET /api/v1/workspaces/{ws}/audit?action&actor&limit&before&format` | Read | `AuditEventView[]`; `format=jsonl` → `application/jsonl`, one event per line, every match streamed (pages of 1000) |
| `GET /api/v1/audit?…` | sysadmin | the same over every workspace |
| `GET /api/v1/workspaces/{ws}/api-keys` | ManageApiKeys | `ApiKeyView[]` |
| `POST /api/v1/workspaces/{ws}/api-keys` | ManageApiKeys | 201 `CreatedApiKey` (the token is shown only here); 403 role above creator's / sysadmin by non-sysadmin; 422 bad name |
| `DELETE /api/v1/workspaces/{ws}/api-keys/{key_id}` | ManageApiKeys | 204; 404 unknown or already revoked |

Rules:
- Every mutating handler calls `access.audit(...)` exactly once after it succeeds, with the action and target strings from Global Constraints. before/after are the serialized API views (`MockView`, `ScenarioView`, `LlmProfileView`, `WorkspaceView`, `ApiKeyView`, contract `UploadResult`). Session seed records `after = {"resources": N}`, not the data.
- `api_key.create`'s `after` is the `ApiKeyView` without the token.
- `request_id` = the `x-request-id` header when 1–128 visible ASCII chars, else a new uuid v4 (set by `WsAccess`).
- The webhook body is the `AuditEventView` JSON. Non-2xx and transport errors are retried, then `mb_audit_webhook_failed_total` is incremented.

- [ ] **Step 1: Write failing tests:**
  - `tests/audit.rs`:
    - `each_mutation_writes_exactly_one_event`: one table-driven test; per endpoint, count events before/after, and check action, target, actor and whether before/after are present.
    - `failed_mutation_writes_no_event` (422 upload)
    - `audit_never_contains_secrets` (profile PUT with `secret_ref: env:X` where X=`sk-123`, plus an API key create → no event text contains `sk-123` or the token)
    - `audit_filters_and_paging`
    - `audit_jsonl_export` (content type, line count = events)
    - `global_audit_requires_sysadmin`
    - `viewer_reads_workspace_audit`
    - `webhook_hmac_verified` (wiremock receiver; recompute `signature(secret, body)` and compare to the header)
    - `webhook_retries_then_gives_up` (500 ×4 → exactly 4 deliveries, `backoff_base` 10 ms)
    - `webhook_failure_does_not_affect_requests` (receiver delays 10 s → a mutation still returns within 1 s)
    - `webhook_queue_full_drops` (`send` 2000 events to a stalled sink → returns immediately)
    - `retention_prunes_old_events`
  - `tests/apikeys.rs`:
    - `create_list_revoke`
    - `token_shown_once` (the list never contains `token` or `secret_hash`)
    - `editor_cannot_manage_keys`
    - `role_above_creator_403`
    - `sysadmin_key_requires_sysadmin`
    - `revoked_key_401_on_admin_api`: an `Authenticator` with `ApiKeys` as `st.auth`, then a request with the token → 200; revoke; then 401.
- [ ] **Step 2:** Run `cargo test -p mb-admin --test audit --test apikeys`. Expected: FAIL.
- [ ] **Step 3:** Implement.
- [ ] **Step 4:** Run `cargo test --workspace --all-features`. Expected: all pass.
- [ ] **Step 5:** Commit: `feat(admin): audit log with JSONL export and signed webhook; API key endpoints`.

---

### Task 9: Live request tail over SSE

**Files:**
- Create: `crates/mb-admin/src/live.rs`, `crates/mb-admin/tests/live.rs`
- Modify: `crates/mb-admin/{Cargo.toml (mb-protocols, tokio-stream),src/lib.rs,src/openapi.rs}`, `crates/mockingbird/src/config.rs` (pass `recorder.live` into `AdminState`)

**Interfaces:**
- Consumes: `RequestRecord.workspace` (Task 5); `StoreRecorder::live()` (P2); `WsAccess` (Task 7).
- Produces:

```rust
pub type LiveSubscribe = Arc<dyn Fn() -> broadcast::Receiver<mb_protocols::RequestRecord> + Send + Sync>;
// AdminState gains: pub live: Option<LiveSubscribe>   (None in AdminState::new)
pub struct LiveRequestView { pub workspace: String, pub mock: String, pub operation_id: Option<String>, pub session: String,
    pub method: String, pub path: String, pub status: u16, pub source: Option<String>, pub latency_ms: u64,
    pub prompt_tokens: u32, pub completion_tokens: u32, pub request: Value, pub response: Option<Value>, pub at: DateTime<Utc> }
pub struct StreamQuery { pub operation: Option<String>, pub session: Option<String>, pub source: Option<String> }
```

Rules:
- `GET /api/v1/workspaces/{ws}/mocks/{mock}/requests/stream` (and its alias) requires Read.
  - An unknown mock → 404. `live` is `None` → 503 problem.
  - Response: `axum::response::Sse` with `KeepAlive::new().interval(15s)`.
  - Records are filtered by `workspace == ws && mock == mock` plus the query filters. `at` is set when the record is forwarded.
  - `RecvError::Lagged(n)` → `event: lagged`, data `{"dropped":n}`. The stream continues.
- OpenAPI documents the endpoint with `content_type = "text/event-stream"` and `LiveRequestView` as the schema.

- [ ] **Step 1: Write failing tests** (`tests/live.rs`; a broadcast channel as `LiveSubscribe`; read the body stream with `http_body_util::BodyExt::frame` and a 2 s timeout):
  - `stream_emits_matching_records` (records for another mock and another workspace are skipped)
  - `stream_filters_by_operation_session_source`
  - `lagged_subscriber_gets_lagged_event` (channel capacity 4, send 10 before reading → `event: lagged` with `dropped` > 0, then later records)
  - `stream_requires_read` (403 for a team-a-only principal)
  - `stream_unknown_mock_404`
  - `stream_without_live_feed_503`
- [ ] **Step 2:** Run `cargo test -p mb-admin --test live`. Expected: FAIL.
- [ ] **Step 3:** Implement. In `build_app_with` set `live: Some(Arc::new(move || recorder.live()))`.
- [ ] **Step 4:** Run `cargo test --workspace --all-features`. Expected: all pass.
- [ ] **Step 5:** Commit: `feat(admin): live request tail over SSE`.

---

### Task 10: Login, logout, `/api/v1/me` and the console gate

**Files:**
- Create: `crates/mb-admin/src/auth_routes.rs`, `crates/mb-admin/tests/auth_flow.rs`
- Modify: `crates/mb-admin/{Cargo.toml (dev: mb-auth features=["testing"], wiremock),src/lib.rs,src/openapi.rs}`

**Interfaces:**
- Consumes: `OidcClient`, `CompletedLogin`, `validate_return_to`, `MockOidcProvider` (Task 4); `SessionManager`, `Authenticator`, `SESSION_COOKIE` (Task 3); `Auditor` (Task 8); `CurrentPrincipal` (Task 7).
- Produces:

```rust
#[derive(Clone)] pub struct WebAuth { pub oidc: Arc<mb_auth::OidcClient>, pub sessions: mb_auth::SessionManager, pub secure_cookies: bool }
// AdminState gains: pub web_auth: Option<WebAuth>   (None = auth.mode none)
pub fn auth_router(st: AdminState) -> axum::Router;   // /auth/login, /auth/callback, /auth/logout; merged by router()
pub async fn console_gate(State(st): State<AdminState>, req: Request, next: Next) -> Response; // for the console and private /metrics
pub struct MeView { pub subject: String, pub display_name: String, pub kind: PrincipalKind, pub sysadmin: bool, pub workspaces: Vec<MeWorkspace> }
pub struct MeWorkspace { pub name: String, pub role: Role }
pub const LOGIN_STATE_COOKIE: &str = "mb_login";
```

Behaviour:
- **`GET /auth/login?return_to=`** (default `/`).
  - `validate_return_to` fails → 400 problem.
  - `web_auth` is `None` → 302 to `return_to`.
  - Otherwise `begin_login`, `Set-Cookie: mb_login=<state>…`, and 302 to the IdP URL.
- **`GET /auth/callback?code&state` (or `?error`).** These cases → 400 problem `Login failed`, plus an `auth.login_failed` audit event (`actor_subject "anonymous"`, target `user:unknown`):
  - `error` present;
  - a missing `mb_login` cookie, or one ≠ `state`;
  - `complete_login` errors.

  On success:
  - destroy any session in the incoming `mb_session` cookie (fixation);
  - `sessions.create`;
  - `Set-Cookie` the new session and clear `mb_login`;
  - audit `auth.login` (workspace `None`, target `user:<sub>`, after `{display_name, sysadmin, roles}`);
  - 302 to `return_to`.
- **`POST /auth/logout`** (`RequireJson`) → destroy the session, clear the cookie, audit `auth.logout`, 204. Without a session → 204, no event.
- **`GET /api/v1/me`** (any principal) → `MeView`. `workspaces`: every existing workspace where `role_in` is `Some`, with that role, sorted by name.
- **`console_gate`:**
  - Runs `st.auth.authenticate`. A principal → continue.
  - Otherwise: a GET whose `Accept` includes `text/html` → 302 `/auth/login?return_to=<percent-encoded path+query>`; anything else → 401 problem.
  - With `web_auth` `None`, the Local principal always passes.
- `/api/openapi.json` documents `/api/v1/me` (the auth routes are not part of the JSON API and are not documented).

- [ ] **Step 1: Write failing tests** (`tests/auth_flow.rs`; `MockOidcProvider`; `AdminState` with `web_auth` set and `auth` = `Authenticator{api_keys, sessions: Some, bearer: Some(oidc.bearer())}`):
  - `login_redirects_to_idp_with_pkce_and_sets_state_cookie`
  - `full_login_sets_session_and_me_reflects_roles` (groups `mb:default:editor` → `/api/v1/me` lists `default` editor; then a mutation as editor works)
  - `return_to_open_redirect_rejected` (the four bad values from Review Focus → 400)
  - `callback_state_mismatch_rejected_and_audited`
  - `callback_without_state_cookie_rejected`
  - `nonce_mismatch_rejected`
  - `session_fixation_new_id_per_login` (log in twice with the first cookie attached → a different cookie; the first is invalid afterwards)
  - `logout_invalidates_session`
  - `logout_requires_json_content_type` (415)
  - `missing_invalid_expired_token_401` (no auth; garbage bearer; expired JWT)
  - `bearer_jwt_principal_has_mapped_roles`
  - `console_gate_redirects_html_and_401s_api_clients`
  - `secure_cookie_when_https_external_url`
  - `mode_none_login_redirects_back`
- [ ] **Step 2:** Run `cargo test -p mb-admin --test auth_flow`. Expected: FAIL.
- [ ] **Step 3:** Implement.
- [ ] **Step 4:** Run `cargo test --workspace --all-features`. Expected: all pass.
- [ ] **Step 5:** Commit: `feat(admin): OIDC login, logout, sessions and /api/v1/me`.

---

### Task 11: Configuration, workspaces as code, and `serve` with auth

**Files:**
- Create: `crates/mockingbird/src/maintenance.rs`, `crates/mockingbird/tests/auth_serve.rs`
- Modify:
  - `crates/mockingbird/src/{file_config.rs,reconcile.rs,config.rs,main.rs,host_guard.rs,client.rs,ops.rs}`
  - `crates/mockingbird/Cargo.toml` (dev: `mb-auth` with `testing`)
  - `docs/examples/mockingbird.yaml` (every new key)
  - `crates/mockingbird/tests/{config.rs,client.rs,e2e.rs}`

**Interfaces:**
- Consumes: everything from Tasks 1–10.
- Produces:

```rust
// file_config.rs (deny_unknown_fields everywhere)
pub struct FileConfig { /* P2 */ pub auth: AuthConfig, pub audit: AuditConfig }
pub enum AuthMode { None, Oidc }                                   // snake_case, default None
pub struct AuthConfig { pub mode: AuthMode, pub external_url: String /* "http://127.0.0.1:9090" */,
    #[serde(with = "humantime_serde")] pub session_ttl: Duration /* 12h */, pub sysadmin_subjects: Vec<String>,
    pub public_metrics: bool /* true */, pub oidc: Option<OidcConfig> }
pub struct OidcConfig { pub issuer: String, pub client_id: String, pub client_secret: Option<String> /* secret ref */,
    pub scopes: Vec<String> /* [openid, profile, email, groups] */, pub roles_claim: String /* groups */,
    pub group_prefix: String /* mb */, pub audience: Option<String> }
pub struct AuditConfig { pub retention_days: u32 /* 365 */, pub webhook_url: Option<String>, pub webhook_secret: Option<String> /* secret ref */ }
// FileMock gains: #[serde(default = "default_ws")] pub workspace: String, #[serde(default)] pub data_plane_auth: DataPlaneAuthConfig
// FileProfile gains: #[serde(default = "default_ws")] pub workspace: String
impl ServerConfig { pub fn check_bind(&self, auth: &AuthConfig) -> Result<(), String>; }
// host_guard.rs
#[derive(Clone)] pub enum HostPolicy { Loopback, Exact(String) /* host[:port] of external_url, lowercased */ }
pub async fn host_guard(State(p): State<HostPolicy>, req: Request, next: Next) -> Response; // ops paths exempt
// maintenance.rs
pub struct Maintenance { _handles: Vec<JoinHandle<()>> }       // session sweeper + audit retention; aborted on drop
// config.rs: Command gains `Openapi` (prints mb_admin::openapi_document() as pretty JSON);
//            Cli gains `#[arg(long, global = true, env = "MB_ADMIN_TOKEN")] pub token: Option<String>`
// client.rs: pub async fn run(admin_url: &str, token: Option<&str>, cmd: ClientCommand) -> Result<Option<Value>, ClientError>
```

Rules:
- **Validation (`check_bind` and `resolve_config`):**
  - `oidc` mode needs `auth.oidc`, and an `external_url` with an http/https scheme and a host (startup error otherwise).
  - `none` mode keeps the P2 rule (non-loopback needs `--insecure-no-auth`).
  - `oidc` mode accepts any host.
- **Reconcile:**
  - Validate that every `workspace` named by a file mock or profile matches the name rule. Create missing workspaces (display name = name) before applying mocks and profiles.
  - Mocks and profiles are keyed by `(workspace, name)`.
  - A prefix collision with any mock in any workspace → `Err` (nothing applied).
  - `source=file` deletion is scoped per workspace over `list_all_mocks`.
  - `data_plane_auth` is applied to file mocks.
  - Workspaces are never deleted by reconcile.
- **`build_app_with`, `auth.mode`:**
  - `none` → `AdminState.auth = LocalAuth`, `web_auth = None`.
  - `oidc`:
    1. Resolve `client_secret` through `ext.secrets`.
    2. `OidcClient::discover` with `redirect_url = external_url + "/auth/callback"`, audience defaulting to `client_id`.
    3. `SessionManager::new(store, session_ttl, external_url starts with "https://")`.
    4. `auth = Authenticator{ api_keys, sessions: Some, bearer: Some(oidc.bearer()) }` and `web_auth = Some(..)`.

    A discovery failure is a startup error that names the issuer.
- **`build_app_with`, audit:** `AdminState.audit = Auditor::new(store, webhook)`, where the webhook is built from `audit.webhook_url` with its secret resolved through `ext.secrets`.
- **`build_app_with`, maintenance:** `Maintenance` starts the session sweeper (oidc only) and `spawn_retention(store, retention_days)`. `App` gains `pub maintenance: Maintenance`.
- **Admin router assembly:** `mb_admin::router(st)` merged with the ops router, where `/metrics` is wrapped in `console_gate` when `public_metrics == false`. The host guard layer is the outermost layer:
  - `Loopback` in `none` mode on a loopback bind;
  - `Exact(external host)` in `oidc` mode;
  - none otherwise.

  `/healthz`, `/readyz` and `/metrics` are exempt from `Exact`, so probes that send pod IPs as `Host` still work.
- **Client:** sends `Authorization: Bearer <token>` when `--token`/`MB_ADMIN_TOKEN` is set.

- [ ] **Step 1: Write failing tests:**
  - `tests/config.rs`:
    - `loads_full_example_config` (updated example)
    - `auth_defaults`
    - `oidc_mode_requires_oidc_section_and_external_url`
    - `none_mode_public_bind_requires_flag`, `oidc_mode_public_bind_allowed`
    - `reconcile_creates_file_workspaces`
    - `reconcile_same_name_two_workspaces`
    - `reconcile_prefix_collision_across_workspaces_errors`
    - `reconcile_applies_data_plane_auth`
  - `tests/auth_serve.rs` (`MockOidcProvider`; `build_app` with an `oidc` config and `client_secret: env:<unique var>`):
    - `oidc_mode_api_requires_auth` (no credentials → 401; provider-signed JWT with `mb:default:viewer` → GET 200, POST 403)
    - `api_key_token_from_cli` (create a key over the API as a sysadmin JWT; then `mockingbird --token <tok> mocks list` against the served app → exit 0; without `--token` → exit 1 with `Unauthorized` on stderr)
    - `host_guard_allows_external_host_only` (`Host: evil` → 421; external host → 401/200; `/healthz` with `Host: 10.0.0.5` → 200)
    - `metrics_private_when_configured`
    - `discovery_failure_is_startup_error`
    - `openapi_subcommand_prints_document` (`assert_cmd`; stdout parses; it contains `/api/v1/workspaces/{ws}/mocks` and the deprecated `/api/v1/mocks`)
  - P2 e2e and client tests pass unchanged (`none` mode).
- [ ] **Step 2:** Run `cargo test -p mockingbird`. Expected: FAIL.
- [ ] **Step 3:** Implement.
- [ ] **Step 4:** Run `cargo fmt --all --check && cargo clippy --workspace --all-targets --all-features -- -D warnings && cargo test --workspace --all-features && scripts/test-postgres.sh`. Expected: all pass.
- [ ] **Step 5:** Commit: `feat(cli): auth and audit configuration, workspaces as code, authenticated serve`.

---

### Task 12: Console crate, web scaffold and auth shell

**Files:**
- Create:
  - `crates/mb-console/{Cargo.toml,build.rs,src/lib.rs,tests/serve.rs}`
  - `crates/mb-console/web/{package.json,package-lock.json,tsconfig.json,vite.config.ts,eslint.config.js,index.html,playwright.config.ts}`
  - `web/src/{main.tsx,router.tsx,index.css}`
  - `web/src/api/{client.ts,schema.d.ts,queries.ts}`
  - `web/src/auth/{me.ts,permissions.ts}`
  - `web/src/components/{Layout.tsx,WorkspaceSwitcher.tsx,ProblemAlert.tsx,DataTable.tsx,Drawer.tsx,ConfirmDialog.tsx,SourceBadge.tsx}`
  - `web/src/pages/{NotFound.tsx,Home.tsx}`
  - `web/src/test/{setup.ts,client.test.ts,permissions.test.ts,shell.test.tsx}`
  - `web/e2e/{mockingbird.e2e.yaml,shell.spec.ts}`
- Modify: root `Cargo.toml` (member, `rust-embed = "8.13"`, `mb-console`), `.gitignore` (`crates/mb-console/dist/`, `crates/mb-console/web/node_modules/`, `web/openapi.json`, `web/test-results/`, `web/playwright-report/`), `crates/mockingbird/{Cargo.toml,src/config.rs}` (console as admin fallback, behind `console_gate`)

**Interfaces:**
- Consumes: `openapi_document()`, `mockingbird openapi` (Tasks 7, 11); `/api/v1/me`, `MeView`, `console_gate` (Task 10); `/api/v1/workspaces` (Task 7).
- Produces:

```rust
// mb-console
pub fn router() -> axum::Router;          // GET/HEAD any path: embedded file, else index.html; headers per Global Constraints
pub fn is_placeholder() -> bool;          // true when built without a real console
```

```ts
// src/api/client.ts
export const api: Client<paths>;                             // openapi-fetch, baseUrl "", 401 middleware below
export function loginUrl(location: Location): string;        // "/auth/login?return_to=" + encodeURIComponent(pathname+search)
export class ApiError extends Error { status: number; problem?: { title: string; detail: string; status: number } }
export function unwrap<T>(r: { data?: T; error?: unknown; response: Response }): T; // throws ApiError
// src/auth/permissions.ts
export type Role = "viewer" | "editor" | "admin" | "owner";
export type Permission = "read" | "edit" | "manageProfiles" | "manageApiKeys" | "manageWorkspace";
export function can(me: Me, ws: string, p: Permission): boolean;   // mirrors Rust matrix; sysadmin → true
// src/auth/me.ts
export function useMe(): UseQueryResult<Me>;                  // GET /api/v1/me
// src/router.tsx: code-based tree
//   /                          → redirect to /w/<last-used or first>/mocks
//   /w/$ws                     → Layout (switcher, nav: Mocks, LLM profiles, API keys, Audit, Status; user menu with Sign out)
//   /w/$ws/mocks, /w/$ws/mocks/$mock/{overview,contracts,scenarios,sessions,requests}, /w/$ws/llm-profiles, /w/$ws/api-keys, /w/$ws/audit, /status
```

Rules:
- **`build.rs`:** when `dist/index.html` is missing, it creates `dist/` and writes a minimal HTML page containing exactly `Console not built — run npm --prefix crates/mb-console/web run build`. It emits `cargo:rerun-if-changed=dist`. `RustEmbed` folder: `dist/`.
- **`vite.config.ts`:** `build.outDir: "../dist"`, `emptyOutDir: true`, React and Tailwind plugins; dev server proxy for `/api` and `/auth` to `http://127.0.0.1:9090`.
- **npm scripts:**
  - `dev`, `build` (`tsc -b && vite build`)
  - `typecheck` (`tsc -b --noEmit`), `lint` (`eslint .`)
  - `test` (`vitest run`), `e2e` (`playwright test`)
  - `gen:api`: `cargo run -q -p mockingbird -- openapi > openapi.json && openapi-typescript openapi.json -o src/api/schema.d.ts`
  - `check:api`: `npm run gen:api && git diff --exit-code src/api/schema.d.ts`
- **401 middleware:** on a 401 from any `/api/` call, set `window.location.href = loginUrl(window.location)` once (a module-level flag prevents loops), then throw `ApiError`. Mutations send `Content-Type: application/json`.
- **Sign out:** `POST /auth/logout` with a JSON body `{}`, then navigate to `/auth/login?return_to=/`. Hidden for `kind === "local"`.
- **Workspace switcher:** lists `/api/v1/workspaces` and remembers the last workspace in `localStorage` (wrapped in try/catch). A sysadmin sees "New workspace" (name + display name dialog → POST).
- **Accessibility:** every form control has a `<label>`; dialogs and drawers trap focus and close on Escape; visible focus rings; Tailwind colors meet AA contrast.
- **`playwright.config.ts`:**
  - `webServer.command`: `cargo run -q --manifest-path ../../../Cargo.toml -p mockingbird -- serve -c e2e/mockingbird.e2e.yaml --offline`
  - `url: http://127.0.0.1:19090/readyz`, `timeout: 600_000`, `reuseExistingServer: !process.env.CI`
  - `use.baseURL: http://127.0.0.1:19090`
  - `launchOptions.executablePath: process.env.MB_E2E_CHROMIUM`
- **`mockingbird.e2e.yaml`:** `server: {port: 18080, admin_port: 19090}`, `storage.url: sqlite::memory:`, `auth.mode: none`, and one file mock `files` (workspace `default`, contract `../../../../fixtures/openapi/orders-3.0.yaml`, prefix `/files`), so read-only UI can be tested.

- [ ] **Step 1: Write failing tests:**
  - `crates/mb-console/tests/serve.rs`:
    - `index_served_with_no_cache_and_csp`
    - `spa_fallback_returns_index` (`/w/default/mocks`)
    - `hashed_asset_immutable_cache` (place a test file under `dist/assets/`, or skip when `is_placeholder()`)
    - `placeholder_mentions_build_command` (only when `is_placeholder()`)
  - Vitest:
    - `client redirects to login on 401` (mock `fetch` → 401; assert `window.location.href === "/auth/login?return_to=%2Fw%2Fdefault%2Fmocks%3Fq%3D1"` and that a second 401 does not reassign)
    - `permissions mirror the server matrix` (table of role × permission, plus sysadmin)
    - `shell renders workspace switcher and hides sign-out for local user`
  - `e2e/shell.spec.ts`: loads `/`, lands on `/w/default/mocks`, the switcher shows `default`, and an axe scan of the shell has no `serious`/`critical` violations.
- [ ] **Step 2:** Run `cargo test -p mb-console && npm --prefix crates/mb-console/web ci && npm --prefix crates/mb-console/web test`. Expected: FAIL.
- [ ] **Step 3:** Implement.
  - Generate `schema.d.ts` with `npm run gen:api` and commit it.
  - Pin `typescript` `~5.9.3` (Global Constraints).
  - Wire `mb_console::router()` as `admin.fallback_service(...)` in `build_app_with`, layered with `console_gate`.
- [ ] **Step 4:** Run:
  - `npm --prefix crates/mb-console/web run typecheck && npm --prefix crates/mb-console/web run lint && npm --prefix crates/mb-console/web test && npm --prefix crates/mb-console/web run build && npm --prefix crates/mb-console/web run check:api`
  - `cargo test -p mb-console`
  - `MB_E2E_CHROMIUM=/opt/pw-browsers/chromium-1194/chrome-linux/chrome npm --prefix crates/mb-console/web run e2e -- e2e/shell.spec.ts`

  Expected: all pass.
- [ ] **Step 5:** Commit: `feat(console): embedded React console scaffold with auth shell`.

---

### Task 13: Console: mocks and contracts

**Files:**
- Create:
  - `web/src/pages/mocks/{MocksList.tsx,CreateMockDialog.tsx,MockLayout.tsx,Overview.tsx,Contracts.tsx,OperationsTable.tsx}`
  - `web/src/test/{mocks.test.tsx,contracts.test.tsx}`
  - `web/e2e/mocks.spec.ts`
- Modify: `web/src/router.tsx`, `web/src/api/queries.ts`

**Interfaces:**
- Consumes: `api`, `unwrap`, `ApiError`, `can`, `useMe`, `DataTable`, `ConfirmDialog`, `ProblemAlert`, `SourceBadge`, routes (Task 12); mock/contract endpoints with `data_plane_auth` (Task 7).
- Produces: query hooks in `queries.ts`, keyed `["ws", ws, …]`:
  - `useMocks(ws)`, `useMock(ws, mock)`, `useCreateMock(ws)`, `usePatchMock(ws, mock)`, `useDeleteMock(ws)`
  - `useContracts(ws, mock)`, `useContract(ws, mock, version)`, `useUploadContract(ws, mock)`, `useActivate(ws, mock)`
  - `useProfiles(ws)` (read-only here)

Behaviour:
- **Mocks list:**
  - Columns: name (link), prefix, active version (or "—"), source badge (`file`/`api`), data-plane auth mode.
  - "New mock" (Edit) opens a dialog with name, prefix (placeholder `/<name>`), persona, profile select and header overrides. A 409/422 problem is shown inline.
- **Overview tab:** a settings form for persona, profile, prefix, header overrides, data-plane auth mode and header. Save sends a PATCH with only the changed fields. Delete uses a confirm dialog. Everything is disabled with the text "Managed by config file" for `source === "file"` or without Edit.
- **Contracts tab:**
  - Upload a file input (`.json`, `.yaml`, `.yml`). Content-Type is chosen by extension. An "Activate after upload" checkbox maps to `?activate=true`. A 422 shows the importer detail.
  - Versions table: version, status, digest (first 12 chars), created_by, created_at, operations count. Row actions: Activate (label "Roll back" for an older version than the active one), View operations (an `OperationsTable` drawer), and Download source (an `<a href>` to `/source` with a `download` attribute).

- [ ] **Step 1: Write failing tests:**
  - Vitest with a mocked `fetch`:
    - `mocks list shows source badge and active version`
    - `create mock dialog posts and shows 409 problem`
    - `file-managed mock is read-only`
    - `viewer sees no mutating actions`
    - `upload sends yaml content type and activate flag`
    - `rollback activates older version`
  - `e2e/mocks.spec.ts`: create mock `pets` → upload `fixtures/openapi/orders-3.0.yaml` → v1 is active → upload again with activate → v2 active → roll back to v1 → the `files` mock shows read-only → axe scan.
- [ ] **Step 2:** Run `npm --prefix crates/mb-console/web test`. Expected: FAIL.
- [ ] **Step 3:** Implement.
- [ ] **Step 4:** Run typecheck, lint, test and build (as in Task 12), then `MB_E2E_CHROMIUM=… npm --prefix crates/mb-console/web run e2e -- e2e/mocks.spec.ts`. Expected: all pass.
- [ ] **Step 5:** Commit: `feat(console): mocks and contract versions`.

---

### Task 14: Console: scenarios, sessions and live requests

**Files:**
- Create:
  - `web/src/pages/mocks/{Scenarios.tsx,ScenarioEditor.tsx,Sessions.tsx,LiveRequests.tsx,RequestDrawer.tsx}`
  - `web/src/api/sse.ts`
  - `web/src/test/{scenarios.test.tsx,sessions.test.tsx,live.test.tsx}`
  - `web/e2e/runtime.spec.ts`
- Modify: `web/src/router.tsx`, `web/src/api/queries.ts`

**Interfaces:**
- Consumes: Task 12/13 components and hooks; scenario/session endpoints (Task 7); the SSE stream and `LiveRequestView` (Task 9).
- Produces:

```ts
// src/api/sse.ts
export type LiveEvent = { kind: "request"; record: LiveRequestView } | { kind: "lagged"; dropped: number };
export function subscribeRequests(ws: string, mock: string, filters: { operation?: string; session?: string; source?: string },
    onEvent: (e: LiveEvent) => void, onError: () => void): () => void;   // EventSource; returns close()
```

Behaviour:
- **Scenarios:**
  - The list shows name, status, latency, operation count and source.
  - Editor fields: instructions (textarea), status, latency_ms, `set` (a JSON textarea validated with `JSON.parse` that must be an object; the error is shown under the field and blocks Save), and operations (a multi-select from the active contract's operations).
  - PUT on save; delete with confirm. Read-only rules as in Task 13.
- **Sessions:**
  - The list shows session, resources and last touched.
  - Inspect opens a drawer with pretty JSON of the export.
  - Reset (Edit, with confirm).
  - Export downloads the export as `<mock>-<session>.json` via a Blob.
  - Seed (Edit) takes a file input with a JSON array and PUTs it. Invalid JSON → inline error.
- **Live requests:**
  - Filter inputs (operation select, session, source select `llm|repaired|fallback`); changing a filter re-subscribes.
  - A Pause/Resume toggle: while paused, incoming events are counted, not shown.
  - The table keeps the newest 500 rows: time, method, path, status, source, latency, session.
  - A `lagged` event shows a dismissible banner "N requests were dropped".
  - Clicking a row opens `RequestDrawer` with the request/response JSON.
  - The table is keyboard navigable (rows are buttons).

- [ ] **Step 1: Write failing tests:**
  - Vitest (a stub `EventSource` class on `globalThis`):
    - `scenario editor rejects invalid set JSON`
    - `scenario operations come from active contract`
    - `session reset and seed call API`
    - `live table prepends events and caps at 500`
    - `pause stops rendering new rows`
    - `lagged banner shows dropped count`
    - `filters re-subscribe with query`
  - `e2e/runtime.spec.ts`:
    1. Create mock `live` with the orders fixture.
    2. Open the Live requests tab.
    3. Playwright `request.post("http://127.0.0.1:18080/live/orders", {headers: {"X-Mockingbird-Session": "s1"}, data: {...}})`.
    4. The row appears within 5 s; open the drawer.
    5. The Sessions tab shows `s1` with ≥ 1 resource; reset → gone.
    6. Create scenario `slow` with latency 100 and `set {"status":"shipped"}`.
    7. Axe scan.
- [ ] **Step 2:** Run `npm --prefix crates/mb-console/web test`. Expected: FAIL.
- [ ] **Step 3:** Implement.
- [ ] **Step 4:** Run typecheck, lint, test and build, then `MB_E2E_CHROMIUM=… npm --prefix crates/mb-console/web run e2e -- e2e/runtime.spec.ts`. Expected: all pass.
- [ ] **Step 5:** Commit: `feat(console): scenarios, sessions and live request tail`.

---

### Task 15: Console: LLM profiles, API keys, audit log and status

**Files:**
- Create:
  - `web/src/pages/{Profiles.tsx,ProfileEditor.tsx,ApiKeys.tsx,CreateKeyDialog.tsx,Audit.tsx,Status.tsx}`
  - `web/src/test/{profiles.test.tsx,apikeys.test.tsx,audit.test.tsx,status.test.tsx}`
  - `web/e2e/admin.spec.ts`
- Modify: `web/src/router.tsx`, `web/src/api/queries.ts`

**Interfaces:**
- Consumes: Task 12 components; profile endpoints (Task 7); API key and audit endpoints, `CreatedApiKey`, `AuditEventView` (Task 8); `/readyz`, `/metrics` (P2 ops).
- Produces: hooks `useProfiles`, `usePutProfile`, `useDeleteProfile`, `useApiKeys`, `useCreateApiKey`, `useRevokeApiKey`, `useAudit(ws, filters)`, `useGlobalAudit(filters)`, `useReadiness()`, `useMetricsSummary()`.

Behaviour:
- **LLM profiles:**
  - Table: name, model, base_url, secret_ref, fallback, source.
  - Editor: base_url, model, `secret_ref` (text; helper text "env:NAME or file:/path — the key itself is never stored"), json_schema, temperature, timeout_ms, max_tokens, fallback_profile.
  - The response's `warnings[]` are shown after save.
  - Mutations need ManageProfiles; viewers see a read-only table. The form has no raw key field.
- **API keys** (the page needs ManageApiKeys; otherwise it shows a 403 explanation):
  - Table: name, key_id, role, scopes, created_by, created_at, expires_at, last_used_at, status (active/revoked/expired).
  - Create dialog: name, role (only roles ≤ my role), scopes checkboxes `admin`/`data-plane`, optional expiry date, sysadmin checkbox (sysadmin only).
  - The result dialog shows the token once with a Copy button and the warning "This token will not be shown again"; closing clears it from state.
  - Revoke with confirm.
- **Audit:**
  - Table: time, actor, action, target, and an expandable before/after JSON diff (side-by-side pretty JSON).
  - Filters: action, actor. "Load more" uses `before=<last id>`.
  - "Export JSONL" is an `<a href>` to the same query with `format=jsonl` and the `download` attribute.
  - A sysadmin sees an "All workspaces" toggle that uses `/api/v1/audit`.
- **Status** (`/status`):
  - `/readyz` status and reasons.
  - A metrics summary parsed from `/metrics` text: total `mb_requests_total`, its split by `source` label, and `mb_request_log_dropped_total`.
  - A 401/403 on `/metrics` shows "Metrics are private".

- [ ] **Step 1: Write failing tests:**
  - Vitest:
    - `profile form has no raw key field and shows warnings`
    - `create key shows token once and clears on close`
    - `role options capped at my role`
    - `audit filters and load more use before id`
    - `jsonl export link carries filters`
    - `status parses metrics text`
  - `e2e/admin.spec.ts`:
    1. Create profile `fast` with `secret_ref env:MB_LLM_API_KEY`.
    2. Create a data-plane key → the token is shown.
    3. Create mock `guarded`, upload the fixture, and set its data-plane auth to `api_key`.
    4. Playwright request to `/guarded/orders` without a key → 401; with `x-api-key: <token>` → 200.
    5. Revoke the key → 401.
    6. The audit page lists `api_key.create`, `api_key.revoke`, `mock.update`.
    7. The status page shows `ready`.
    8. Axe scan.
- [ ] **Step 2:** Run `npm --prefix crates/mb-console/web test`. Expected: FAIL.
- [ ] **Step 3:** Implement.
- [ ] **Step 4:** Run typecheck, lint, test and build, then `MB_E2E_CHROMIUM=… npm --prefix crates/mb-console/web run e2e`. Expected: the full local suite passes.
- [ ] **Step 5:** Commit: `feat(console): LLM profiles, API keys, audit log and status pages`.

---

### Task 16: CI, image, Compose with Rauthy, and docs

**Files:**
- Create:
  - `deploy/compose/{docker-compose.yml,mockingbird.yaml,rauthy-bootstrap.sh,README.md}`
  - `crates/mb-console/web/playwright.rauthy.config.ts`
  - `crates/mb-console/web/e2e-rauthy/full-flow.spec.ts`
- Modify: `.github/workflows/ci.yml`, `Dockerfile`, `.dockerignore` (exclude `node_modules`, `dist`), `README.md`

**Interfaces:**
- Consumes: the whole product.
- Produces:
  - CI jobs `check` (console built first), `console`, `deny`, `image`, `e2e`.
  - Compose services `postgres`, `rauthy`, `rauthy-bootstrap`, `mockingbird`.

Rules:
- **CI `console` job** (ubuntu, Node 22 with npm cache, Rust 1.97):
  - `npm ci`, `typecheck`, `lint`, `test`, `build`, `check:api`;
  - `npx playwright install --with-deps chromium && npm run e2e` (local auth-none suite).
- **CI `check` job:** before the cargo steps, `actions/setup-node@v4` (22), `npm --prefix crates/mb-console/web ci && npm --prefix crates/mb-console/web run build`. Then `cargo test --workspace --all-features` (was without `--all-features`).
- **Dockerfile:**
  - New first stage `node:22-bookworm AS console`: copy `crates/mb-console/web/package*.json`, `npm ci`, copy the web source, `npm run build` (output `/src/crates/mb-console/dist`).
  - The builder copies that `dist` into `crates/mb-console/dist` before the final `cargo build`.
- **Compose** (`deploy/compose/docker-compose.yml`):
  - **`postgres:16`:** healthcheck `pg_isready`, volume.
  - **`rauthy` (`ghcr.io/sebadob/rauthy:0.37.1`, embedded Hiqlite):**
    - Env: `LISTEN_SCHEME=http`, `LISTEN_PORT_HTTP=8081`, `PUB_URL=localhost:8081`, `COOKIE_MODE=danger-insecure`, `HQL_NODE_ID=1`, `HQL_NODES="1 localhost:8100 localhost:8200"`, `HQL_SECRET_RAFT`/`HQL_SECRET_API` (dev values ≥ 16 chars), `ENC_KEYS`/`ENC_KEY_ACTIVE` (one dev key), `BOOTSTRAP_ADMIN_EMAIL=admin@localhost.test`, `BOOTSTRAP_ADMIN_PASSWORD_PLAIN`, `BOOTSTRAP_API_KEY` (base64 JSON: name `bootstrap`, access Clients/Groups/Users/Roles with read/create/update/delete), `BOOTSTRAP_API_KEY_SECRET` (≥ 64 alphanumerics).
    - Volume `/app/data`.
    - Publishes `8081`, `8080` and `9090`. Mockingbird shares Rauthy's network namespace, so `localhost:8081` is the same issuer for the browser and the server.
    - Mark in the file: `# verify against Rauthy 0.37 docs: issuer is http://localhost:8081/auth/v1 with LISTEN_SCHEME=http`.
  - **`rauthy-bootstrap`** (`curlimages/curl`, `network_mode: "service:rauthy"`, runs `rauthy-bootstrap.sh`, volume `bootstrap:/bootstrap`). The script waits for Rauthy, then with `Authorization: API-Key bootstrap$<secret>`:
    - creates groups `mb:sysadmin` and `mb:default:admin`;
    - creates the confidential client `mockingbird` with redirect `http://localhost:9090/auth/callback`, post-logout `http://localhost:9090/`, scopes including `groups`;
    - generates its secret and writes it to `/bootstrap/client_secret`;
    - creates the user `demo@localhost.test` with password `Demo-Passw0rd-123!` in both groups;
    - is idempotent (409 counts as success).

    Each endpoint path and body carries the comment `# verify against Rauthy docs (Swagger UI at /auth/v1/docs)`: client create/secret, group create, user create and password set. If an endpoint cannot be confirmed, the script prints the manual steps and `deploy/compose/README.md` documents them.
  - **`mockingbird`:**
    - `build: ../..`, `network_mode: "service:rauthy"`, `depends_on`: postgres healthy and rauthy-bootstrap `service_completed_successfully`.
    - Command `serve -c /etc/mockingbird/mockingbird.yaml --host 0.0.0.0`.
    - Mounts `mockingbird.yaml` and the `bootstrap` volume (read-only).
  - **`mockingbird.yaml`:**
    - `storage.url: postgres://postgres:postgres@postgres:5432/postgres`
    - `auth.mode: oidc`, `external_url: http://localhost:9090`
    - `oidc: {issuer: http://localhost:8081/auth/v1, client_id: mockingbird, client_secret: file:/bootstrap/client_secret}`
    - `llm.default_profile` omitted; started with `--offline` in Compose so no LLM key is needed.
- **CI `e2e` job** (ubuntu-latest has Docker):
  - `docker compose -f deploy/compose/docker-compose.yml up -d --build --wait`, then poll `http://localhost:9090/readyz` for up to 180 s.
  - `npm ci`, `npx playwright install --with-deps chromium`, `npx playwright test -c playwright.rauthy.config.ts`.
  - On failure, `docker compose logs`.
- **`full-flow.spec.ts`:**
  1. Visit `/` → redirected to Rauthy; log in as the demo user. Rauthy form selectors are marked `// verify against Rauthy 0.37 login UI` and use `getByLabel(/e-?mail/i)`, `getByLabel(/password/i)` and `getByRole("button", {name: /login|sign in/i})`.
  2. Land on `/w/default/mocks` and create mock `e2e`.
  3. Upload `fixtures/openapi/orders-3.0.yaml`.
  4. Call `http://localhost:8080/e2e/orders` → 200.
  5. See the request in Live requests.
  6. Reset the session.
  7. The audit page shows `auth.login`, `mock.create`, `contract.upload`, `session.reset`.
- **README:** a "Console and auth" section covering:
  - auth modes, and the config keys with an example;
  - roles and the group convention `mb:<ws>:<role>`;
  - API keys (admin vs data-plane) and `--token`;
  - workspaces and the deprecated P2 paths;
  - the audit log and webhook signature verification snippet;
  - the Compose quickstart (`docker compose -f deploy/compose/docker-compose.yml up`, demo credentials, dev-only secrets warning);
  - console development (`npm run dev`).

- [ ] **Step 1:** Write `full-flow.spec.ts` and `playwright.rauthy.config.ts` (`baseURL http://localhost:9090`, no `webServer`). Run `npx tsc -p crates/mb-console/web --noEmit`. Expected: compiles. It cannot run locally (no Docker); say so in the report.
- [ ] **Step 2:** Write the Compose files, the bootstrap script, the Dockerfile stage and the CI jobs. Validate locally:
  - `bash -n deploy/compose/rauthy-bootstrap.sh`
  - `python3 -c "import yaml,sys; yaml.safe_load(open('deploy/compose/docker-compose.yml')); yaml.safe_load(open('.github/workflows/ci.yml'))"`
  - `docker compose -f deploy/compose/docker-compose.yml config -q` (only if the docker CLI can run without a daemon)

  Expected: no errors.
- [ ] **Step 3:** Update the README.
- [ ] **Step 4:** Run:
  - `cargo fmt --all --check && cargo clippy --workspace --all-targets --all-features -- -D warnings && cargo test --workspace --all-features && scripts/test-postgres.sh && cargo deny check`
  - `npm --prefix crates/mb-console/web run typecheck && npm --prefix crates/mb-console/web run lint && npm --prefix crates/mb-console/web test && npm --prefix crates/mb-console/web run check:api`
  - `MB_E2E_CHROMIUM=/opt/pw-browsers/chromium-1194/chrome-linux/chrome npm --prefix crates/mb-console/web run e2e`

  Expected: all pass. The Compose, Rauthy and image jobs are verified only in CI; report that.
- [ ] **Step 5:** Commit: `ci: console and Rauthy e2e jobs; feat(deploy): Compose stack with Rauthy; docs`.
