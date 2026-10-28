# WS4: Routing, Proxy and CA Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Mocks can claim host names (`hosts:`) and share route prefixes across hosts, and a local intercepting HTTP proxy (`mockingbird proxy`, or `serve` with `proxy.enabled`) with its own CA lets unmodified apps reach those mocks over HTTPS. The proxy also serves placeholder images, and config reloads are visible in the logs and in `/readyz`.

**Architecture:**
- **Store (`mb-store`).** `Mock`/`NewMock` gain `hosts: Vec<String>` (migration `0600_mock_hosts`). Two helpers live next to `validate_route_prefix`: `validate_hosts` and `routes_collide`. The registry, the Admin API and the reconciler all use `routes_collide`, so every place agrees on what counts as a duplicate route.
- **Routing (`registry.rs`).**
  - Dispatch reads the request host: the URI authority (absolute-form or HTTP/2), else `Host`. It keeps the mocks whose `hosts` contain that host, plus the mocks with no `hosts`.
  - It then picks the longest prefix. At equal length a host-specific mock wins.
  - A mock with `hosts` is reachable only with one of those hosts.
- **Proxy (new `crates/mockingbird/src/proxy/`).**
  - `ca.rs` holds the rcgen local CA and a per-host leaf `rustls::ServerConfig` cache.
  - `images.rs` builds a minimal PNG with `flate2` (already in the tree; `flate2::Crc` provides the CRC-32).
  - `server.rs` is a hyper HTTP/1.1 proxy listener. It handles plain absolute-form requests and `CONNECT`:
    - **Mapped host** (claimed by a mock or listed in `proxy.images`): the upgraded stream is TLS-terminated with `tokio-rustls`. The decrypted stream is then served by `hyper_util::server::conn::auto` with the **same data-plane `axum::Router`**. Requests keep their `Host`/`:authority`, so they dispatch by host.
    - **Unmapped host:** `pass` tunnels the bytes with `copy_bidirectional`; `block` answers a 502 problem+json.
  - `env.rs` computes the variables to export. Both the CLI and WS5's MCP call it.
  - `standalone.rs` runs the proxy outside `serve`. It forwards to a running server's data plane over HTTP, keeping `Host`, and learns the claimed hosts from a new ops endpoint `GET /proxy/hosts`.
- **Serve.** `serve` binds the proxy listener at `server.host:proxy.port` when `proxy.enabled` (or `--proxy`) is set on a role that serves the data plane.
- **Reload (`watch.rs`/`ops.rs`).** A `ReloadStatus` shared between the watcher and `/readyz` reports `config_reload: <error>`. A mock whose rebuild failed is reported as `stale` when its previous version is still mounted, and `failed` otherwise.

**Tech Stack:** Rust 2024 workspace, axum 0.8.9, hyper 1.12.0, hyper-util 0.1.21, tokio-rustls 0.26.6, rustls 0.23.46 (aws-lc-rs), reqwest 0.13.5. New `[workspace.dependencies]` pins. All but `rcgen` are already in `Cargo.lock`; `rcgen` 0.14.10 is the version `fixtures/tls/README.md` already used:
- `rcgen = { version = "0.14.10", default-features = false, features = ["aws_lc_rs", "pem", "x509-parser"] }` (x509-parser 0.18.1 is in the tree; `Issuer::from_ca_cert_pem` needs `pem` + `x509-parser`)
- `time = "0.3.55"` (validity dates for rcgen)
- `hyper = { version = "1.12.0", features = ["server", "http1", "http2"] }`
- `hyper-util = { version = "0.1.21", features = ["server-auto", "tokio", "http1", "http2", "service"] }`
- `tokio-rustls = { version = "0.26.6", default-features = false, features = ["tls12"] }` (the provider is passed explicitly)
- `rustls-native-certs = "0.8.5"`, `webpki-root-certs = "1.0.10"` (system roots for the passthrough bundle, Mozilla roots as fallback)
- `flate2` (workspace, 1.1.10) and `http-body-util` (workspace) added to `crates/mockingbird/Cargo.toml` `[dependencies]`

**Spec:** `docs/superpowers/specs/2026-10-22-improvements-backlog-design.md`, section **WS4** (also the Decisions table and the Risks section).

## Global Constraints

- Branch `feat/ws4-routing-proxy`; one PR; merge only with the user's OK.
- `proxy.unmapped: pass | block`, default `pass` (CONNECT tunnel, not intercepted; logged). Every unmapped host is logged once.
- Local CA at `~/.mockingbird/ca/{ca.pem,ca.key}`, key mode `0600` (directory `0700`), generated with `rcgen` on first use. Leaf certs are cached in memory. No system trust store is touched and no OS settings are changed.
- `mockingbird proxy env` variables: `HTTPS_PROXY`, `HTTP_PROXY`, `NO_PROXY=localhost,127.0.0.1`, `NODE_EXTRA_CA_CERTS`, `SSL_CERT_FILE`/`REQUESTS_CA_BUNDLE` (system roots plus the local CA), `NODE_USE_ENV_PROXY=1`.
- Default proxy port `8888`.
- Placeholder image: a solid colour from a hash of the path; size from a `w123`-style path segment, else 300×450. No LLM.
- `block` answers 502 problem+json naming the host.
- No `unwrap()`/`expect()` outside tests (`clippy::unwrap_used = "deny"`). Admin API errors are `application/problem+json`. Never print or write API keys.
- New config keys go in `docs/configuration.md` **and** `docs/examples/mockingbird.full.yaml` (`tests/docs_reference.rs` checks both). Put WS4 keys in their own `## proxy` section and a `mocks.<name>.hosts` row, to keep merges with WS1/WS6 textual.
- After the Admin API changes, run `npm run gen:api` in `crates/mb-console/web`.
- File ownership: `crates/mockingbird/src/mcp.rs` and `daemon.rs` belong to WS5. This plan only exposes library functions and `serve` flags that WS5 wraps.
- Every commit ends with `Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>`.

## Review Focus

1. **A mock with `hosts` stops answering on `localhost:8080`.** This is the intended behaviour: such a mock is reachable only through its hosts, via the proxy or an explicit `Host` header. A user would still expect the 404 to say so. Task 2 pins the 404 detail, which names the host.
2. **Host spelling:** `API.Example.test:443`, a trailing dot, or `[::1]:8080` must match the configured `api.example.test`/`::1`. Task 2 pins it with a `normalize_host` unit test.
3. **Two processes create the CA at once** (`serve` with the proxy, plus `mockingbird proxy env` from an agent). Neither may overwrite the other's key, or every leaf issued earlier stops chaining. Task 6 writes the key with `create_new` and reloads on `AlreadyExists`, and its test races two `load_or_create` calls.
4. **The proxy's own forwarding loops through `HTTPS_PROXY`.** Run `serve` from a shell that exported `mockingbird proxy env`, and an unmapped plain request is forwarded through reqwest, which would read `HTTP_PROXY` and send it back to the proxy. Task 8 builds the forwarding client with `.no_proxy()` and tests it by setting a bogus proxy on the client builder.
5. **Strict TLS clients** (Python 3.13 sets `VERIFY_X509_STRICT`; Go) reject leaves without an Authority Key Identifier, and CAs without critical `basicConstraints` and `keyUsage`. Task 6 sets those extensions and checks them in its test.

---

### Task 1: Store: mock hosts and route collisions

**Files:**
- Create: `crates/mb-store/migrations/sqlite/0600_mock_hosts.sql`, `crates/mb-store/migrations/postgres/0600_mock_hosts.sql`
- Modify: `crates/mb-store/src/model.rs:66-101` (`Mock`, `NewMock`), `crates/mb-store/src/lib.rs:488-507` (next to `validate_route_prefix`), `crates/mb-store/src/sqlite.rs` (`MOCK_COLUMNS` :36, `mock_from_row` :259, `create_mock` :627, `update_mock` :685), `crates/mb-store/src/postgres.rs` (`MOCK_COLUMNS`/`MOCK_SELECT` :36-41, `mock_from_row` :191, `create_mock` :641, `update_mock` :694)
- Modify (add `hosts: vec![]` to every `NewMock { .. }` literal): `crates/mb-admin/tests/{imports,mocks,channels,api}.rs`, `crates/mockingbird/tests/{observability,registry,events_workspaces,config,limits,workspaces,import_llm}.rs`, `crates/mockingbird/src/maintenance.rs`, `crates/mb-store/tests/sqlite.rs`, `crates/mb-store/tests/common/conformance.rs`. `mb-admin/src/mocks.rs` and `reconcile.rs` get real values in Tasks 3 and 4; until then they use `hosts: vec![]` and `body.hosts`/`p.mock.hosts`.
- Test: `crates/mb-store/src/lib.rs` (`#[cfg(test)] mod tests`), `crates/mb-store/tests/common/conformance.rs`

**Interfaces:**
- Produces:
  - `pub struct Mock { …, #[serde(default)] pub hosts: Vec<String>, … }` and `pub struct NewMock { …, #[serde(default)] pub hosts: Vec<String> }`
  - `pub fn validate_hosts(hosts: &[String]) -> Result<(), StoreError>` (re-exported from `mb_store`)
  - `pub fn routes_collide(a_prefix: &str, a_hosts: &[String], b_prefix: &str, b_hosts: &[String]) -> bool`

- [ ] **Step 1: Write the failing tests**

In `crates/mb-store/src/lib.rs` tests:

```rust
#[test]
fn hosts_are_validated() {
    let ok = |h: &[&str]| validate_hosts(&h.iter().map(|s| s.to_string()).collect::<Vec<_>>());
    assert!(ok(&[]).is_ok());
    assert!(ok(&["api.github.com", "image.tmdb.org", "localhost", "127.0.0.1"]).is_ok());
    for bad in [&["API.github.com"][..], &["api.github.com:443"], &["-a.test"], &["a..test"], &[""], &["a.test", "a.test"], &["*.github.com"]] {
        assert!(matches!(ok(bad), Err(StoreError::Invalid(_))), "{bad:?}");
    }
}

#[test]
fn routes_collide_by_prefix_and_host_set() {
    let h = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
    assert!(routes_collide("/", &h(&[]), "/", &h(&[])));
    assert!(routes_collide("/", &h(&["a.test"]), "/", &h(&["b.test", "a.test"])));
    assert!(!routes_collide("/", &h(&["a.test"]), "/", &h(&["b.test"])));
    assert!(!routes_collide("/", &h(&["a.test"]), "/", &h(&[])), "host-specific and hostless coexist");
    assert!(!routes_collide("/", &h(&[]), "/graphql", &h(&[])));
}
```

In `conformance.rs`, add `mock_hosts_round_trip(&store).await;` to `run_all`:

```rust
async fn mock_hosts_round_trip(store: &Arc<dyn ControlStore>) {
    let mut new = new_mock("hosted");
    new.route_prefix = Some("/".into());
    new.hosts = vec!["api.github.com".into()];
    let m = store.create_mock(DEFAULT_WORKSPACE, new).await.expect("create");
    assert_eq!(m.hosts, ["api.github.com"]);
    let got = store.get_mock(DEFAULT_WORKSPACE, "hosted").await.expect("get").expect("some");
    assert_eq!(got.hosts, ["api.github.com"]);
    let updated = store.update_mock(&Mock { hosts: vec![], ..got.clone() }).await.expect("update");
    assert!(updated.hosts.is_empty());
    let bad = store.update_mock(&Mock { hosts: vec!["Bad:1".into()], ..got }).await;
    assert!(matches!(bad, Err(StoreError::Invalid(_))));
    assert!(store.delete_mock(DEFAULT_WORKSPACE, "hosted").await.expect("delete"));
}
```

- [ ] **Step 2: Run them and confirm they fail**

Run: `cargo test -p mb-store --all-features hosts_ routes_collide sqlite_memory_conformance`
Expected: compile FAIL, with "cannot find function `validate_hosts`" and "struct `NewMock` has no field named `hosts`".

- [ ] **Step 3: Implement**

- Migrations:
  - sqlite: `ALTER TABLE mocks ADD COLUMN hosts TEXT NOT NULL DEFAULT '[]';`
  - postgres: `ALTER TABLE mocks ADD COLUMN hosts JSONB NOT NULL DEFAULT '[]';`
  - Add a header comment naming WS4.
- The column is appended last in `MOCK_COLUMNS`. Postgres `MOCK_SELECT` adds `hosts::text AS hosts`. Rows parse it with `serde_json::from_str`.
- `create_mock` and `update_mock` call `validate_hosts` after `validate_route_prefix`.
- `validate_hosts` rule: every entry is 1–253 bytes. Labels separated by `.` are non-empty, made of `[a-z0-9-]`, and do not start or end with `-`. No duplicates. Messages:
  - `host {h:?} must be a lowercase DNS name or IPv4 address without a port`
  - `host {h:?} is listed twice`
- `routes_collide`: the prefixes are equal, and either both host sets are empty or the sets intersect.

- [ ] **Step 4: Run them and confirm they pass**

Run: `cargo test -p mb-store --all-features` and `cargo build --workspace --all-targets --all-features`
Expected: PASS. The build shows that every `NewMock` literal now compiles.

- [ ] **Step 5: Commit**

```bash
git add crates/mb-store crates/mb-admin crates/mockingbird
git commit -m "feat(store): mock hosts and host-aware route collisions"
```

---

### Task 2: Host-aware dispatch in the registry

**Files:**
- Modify: `crates/mockingbird/src/registry.rs`:
  - `Served::Http` (:63) and `Mounted::prefix`
  - `reload_all_locked` collision check (:161-171)
  - `reload_mock` collision and `released` logic (:212-270); a changed host set also counts as released
  - `publish` sort (:449-454)
  - `dispatch` (:519-547)
  - `build_mounted` (:621-627)
- Test: `crates/mockingbird/tests/registry.rs`; unit tests in `registry.rs` `mod tests`

**Interfaces:**
- Consumes: `mb_store::routes_collide`, `Mock::hosts` (Task 1)
- Produces:
  - `pub fn normalize_host(raw: &str) -> Option<String>`: lowercases, strips `:port`, `[` `]` and one trailing `.`; `None` for empty input
  - `pub fn request_host(req: &axum::extract::Request) -> Option<String>`: `req.uri().host()`, else the `Host` header, through `normalize_host`
  - `impl MockRegistry { pub fn claims_host(&self, host: &str) -> bool; pub fn claimed_hosts(&self) -> Vec<String> /* sorted, deduped */ }`

- [ ] **Step 1: Write the failing tests**

Unit test in `registry.rs`:

```rust
#[test]
fn hosts_are_normalized() {
    assert_eq!(normalize_host("API.Example.TEST:443").as_deref(), Some("api.example.test"));
    assert_eq!(normalize_host("api.example.test.").as_deref(), Some("api.example.test"));
    assert_eq!(normalize_host("[::1]:8080").as_deref(), Some("::1"));
    assert_eq!(normalize_host("127.0.0.1").as_deref(), Some("127.0.0.1"));
    assert_eq!(normalize_host(""), None);
}
```

In `tests/registry.rs`:
- Add a helper `create_hosted(store, name, prefix, hosts: &[&str]) -> Mock`, mirroring `create_mock` with `hosts` set.
- Add a helper `call(router: &axum::Router, method: &str, host: &str, path: &str, body: Option<Value>) -> (u16, Value)`. It uses `tower::ServiceExt::oneshot` on `registry.router()` with a `host` header.

```rust
#[tokio::test]
async fn mocks_share_a_prefix_on_different_hosts() {
    let store = store().await;
    let a = create_hosted(&store, "a", "/", &["a.test"]).await;
    activate(&store, &a, &fixture_v2_source()).await; // has GET /ping
    let b = create_hosted(&store, "b", "/", &["b.test"]).await;
    activate(&store, &b, &fixture_source()).await;
    let registry = MockRegistry::new();
    registry.reload_all(&deps(store.clone())).await.unwrap();
    let r = registry.router();
    assert_eq!(call(&r, "GET", "a.test", "/ping", None).await.0, 200);
    assert_eq!(call(&r, "GET", "B.TEST:443", "/orders", None).await.0, 200);
    assert_ne!(call(&r, "GET", "b.test", "/ping", None).await.0, 200);
    let (status, body) = call(&r, "GET", "localhost:8080", "/orders", None).await;
    assert_eq!(status, 404);
    assert!(body["detail"].as_str().unwrap().contains("localhost"), "{body}");
    assert_eq!(registry.claimed_hosts(), ["a.test", "b.test"]);
    assert!(registry.claims_host("a.test") && !registry.claims_host("c.test"));
}

#[tokio::test]
async fn rest_at_root_and_graphql_below_on_one_host() {
    let store = store().await;
    let rest = create_hosted(&store, "rest", "/", &["api.github.test"]).await;
    activate(&store, &rest, &fixture_source()).await;
    let gql = create_hosted(&store, "gql", "/graphql", &["api.github.test"]).await;
    let sdl = std::fs::read_to_string(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/graphql/github.graphql")).unwrap();
    activate(&store, &gql, &sdl).await;
    let registry = MockRegistry::new();
    registry.reload_all(&deps(store.clone())).await.unwrap();
    let r = registry.router();
    assert_eq!(call(&r, "GET", "api.github.test", "/orders", None).await.0, 200);
    let (status, body) = call(&r, "POST", "api.github.test", "/graphql", Some(json!({"query": "{ __typename }"}))).await;
    assert_eq!(status, 200);
    assert!(body.get("data").is_some(), "{body}");
}

#[tokio::test]
async fn host_specific_mock_wins_over_hostless_at_same_prefix() {
    let store = store().await;
    let any = create_mock(&store, "any", "/", None).await;
    activate(&store, &any, &fixture_source()).await;
    let x = create_hosted(&store, "x", "/", &["x.test"]).await;
    activate(&store, &x, &fixture_v2_source()).await;
    let registry = MockRegistry::new();
    registry.reload_all(&deps(store.clone())).await.unwrap();
    let r = registry.router();
    assert_eq!(call(&r, "GET", "x.test", "/ping", None).await.0, 200);
    assert_ne!(call(&r, "GET", "localhost", "/ping", None).await.0, 200);
    assert_eq!(call(&r, "GET", "localhost", "/orders", None).await.0, 200);
}

#[tokio::test]
async fn overlapping_host_sets_collide() {
    let store = store().await;
    let a = create_hosted(&store, "a", "/", &["a.test"]).await;
    activate(&store, &a, &fixture_source()).await;
    let b = create_hosted(&store, "b", "/", &["b.test", "a.test"]).await;
    activate(&store, &b, &fixture_source()).await;
    let registry = MockRegistry::new();
    registry.reload_all(&deps(store.clone())).await.unwrap();
    assert_eq!(registry.mounted(), [("default".to_string(), "a".to_string(), "/".to_string())]);
    registry.reload_mock(&deps(store.clone()), DEFAULT_WORKSPACE, "b").await;
    assert_eq!(registry.mounted().len(), 1, "a sorts first and keeps the route");
}
```

- [ ] **Step 2: Run them and confirm they fail**

Run: `cargo test -p mockingbird --all-features --test registry` and `cargo test -p mockingbird --lib registry::tests`
Expected: compile FAIL, with "cannot find function `normalize_host`" and "no method named `claimed_hosts`".

- [ ] **Step 3: Implement**

- `Served::Http { prefix: String, hosts: Vec<String>, router: Router }`, filled from `mock.hosts`.
- `publish` sorts by prefix length (descending), then `hosts.is_empty()` (host-specific first), then key.
- `dispatch`:
  1. `let host = request_host(&req);`
  2. Take the first mounted HTTP mock where `strip_prefix` matches and (`hosts.is_empty()` or `host` is in `hosts`).
  3. The 404 detail is `no mock is mounted at a prefix of {path} for host {host}` (or the existing text when there is no host).
- Both collision sites call `mb_store::routes_collide`. The existing log messages stay.
- In `reload_mock`, `released` is also true when the old host set differs from the new one.

- [ ] **Step 4: Run them and confirm they pass**

Run: `cargo test -p mockingbird --all-features --test registry --test workspaces --test events_workspaces` and `cargo test -p mockingbird --lib registry::tests`
Expected: PASS. The existing prefix tests are unchanged.

- [ ] **Step 5: Commit**

```bash
git add crates/mockingbird/src/registry.rs crates/mockingbird/tests/registry.rs
git commit -m "feat(registry): dispatch by host, then longest prefix"
```

---

### Task 3: Admin API: `hosts` on mocks

**Files:**
- Modify: `crates/mb-admin/src/mocks.rs`:
  - `MockView` (:111), `MockView::new` (:136)
  - `CreateMock` (:156), `PatchMock` (:184)
  - replace `ensure_prefix_free` (:233-247) with `ensure_route_free`
  - `create` (:296-321), `patch` (:381-391)
- Modify: `crates/mb-console/web/src/api/schema.d.ts` (generated), `crates/mb-console/web/src/test/harness.tsx:100` (`mockView` gets `hosts: []`)
- Test: `crates/mb-admin/tests/mocks.rs`

**Interfaces:**
- Consumes: `mb_store::{validate_hosts, routes_collide}` (Task 1)
- Produces:
  - `MockView.hosts: Vec<String>`
  - `CreateMock.hosts: Vec<String>` (`#[serde(default)]`)
  - `PatchMock.hosts: Option<Vec<String>>`: replaces the list, and `[]` clears it
  - `async fn ensure_route_free(st: &AdminState, prefix: &str, hosts: &[String], except: Option<(&str, &str)>) -> ApiResult<()>`

- [ ] **Step 1: Write the failing tests**

```rust
#[tokio::test]
async fn hosts_let_mocks_share_a_prefix() {
    let h = harness().await;
    let create = |name: &str, hosts: Value| json!({"name": name, "route_prefix": "/", "hosts": hosts});
    let (s, body) = h.send(Method::POST, "/api/v1/mocks", Some(create("a", json!(["a.test"])))).await;
    assert_eq!(s, StatusCode::CREATED, "{body}");
    assert_eq!(body["hosts"], json!(["a.test"]));
    let (s, _) = h.send(Method::POST, "/api/v1/mocks", Some(create("b", json!(["b.test"])))).await;
    assert_eq!(s, StatusCode::CREATED);
    let (s, body) = h.send(Method::POST, "/api/v1/mocks", Some(create("c", json!(["c.test", "b.test"])))).await;
    assert_eq!(s, StatusCode::CONFLICT);
    assert!(body["detail"].as_str().unwrap().contains("\"b.test\""), "{body}");
    assert!(body["detail"].as_str().unwrap().contains("default/b"), "{body}");
    let (s, body) = h.send(Method::POST, "/api/v1/mocks", Some(create("d", json!(["D.test:80"])))).await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
}

#[tokio::test]
async fn patch_hosts_replaces_and_checks_collisions() {
    let h = harness().await;
    for (name, host) in [("a", "a.test"), ("b", "b.test")] {
        let body = json!({"name": name, "route_prefix": "/", "hosts": [host]});
        assert_eq!(h.send(Method::POST, "/api/v1/mocks", Some(body)).await.0, StatusCode::CREATED);
    }
    let (s, _) = h.send(Method::PATCH, "/api/v1/mocks/b", Some(json!({"hosts": ["a.test"]}))).await;
    assert_eq!(s, StatusCode::CONFLICT);
    let (s, body) = h.send(Method::PATCH, "/api/v1/mocks/b", Some(json!({"hosts": ["c.test"]}))).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(body["hosts"], json!(["c.test"]));
    let (s, body) = h.send(Method::PATCH, "/api/v1/mocks/b", Some(json!({"hosts": []}))).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(body["hosts"], json!([]));
}
```

- [ ] **Step 2: Run them and confirm they fail**

Run: `cargo test -p mb-admin --all-features --test mocks hosts`
Expected: FAIL. `body["hosts"]` is `null`, and the third create returns 201 instead of 409.

- [ ] **Step 3: Implement**

- `ensure_route_free` finds a holder with `routes_collide` (ignoring `except`).
- Detail without hosts: keep exactly `route prefix {prefix:?} is already used by mock {ws}/{name}`.
- Detail with a shared host: `route prefix {prefix:?} on host {host:?} is already used by mock {ws}/{name}`, naming the first common host.
- `create` and `patch` call `validate_hosts` first (its `StoreError::Invalid` maps to 422). `patch` checks the final `(prefix, hosts)` pair whenever either one changed.
- Update the utoipa doc strings: 409 "another mock has its route prefix on an overlapping host set"; 422 lists hosts.

- [ ] **Step 4: Run them, then regenerate the console client**

Run: `cargo test -p mb-admin --all-features`, then in `crates/mb-console/web`: `npm run gen:api && npm run typecheck && npm run lint && npm test`
Expected: all PASS. `schema.d.ts` gains `hosts` on `MockView`, `CreateMock` and `PatchMock`.

- [ ] **Step 5: Commit**

```bash
git add crates/mb-admin crates/mb-console/web/src
git commit -m "feat(admin): hosts on mocks; route conflicts compare prefix and host set"
```

---

### Task 4: File config and reconcile: `mocks.<name>.hosts`

**Files:**
- Modify: `crates/mockingbird/src/file_config.rs`: `FileMock` (:913-947), `FileMock::new` (:1066), `validate_file` per-mock loop (:1239)
- Modify: `crates/mockingbird/src/reconcile.rs`:
  - the prefix map (:301-315)
  - the API-mock check (:445-458)
  - `apply_mock` `NewMock` (:509) and `wanted` (:526)
- Modify: `docs/configuration.md` (`## mocks` table: row `mocks.<name>.hosts`), `docs/examples/mockingbird.full.yaml` (orders mock: `hosts: [orders.example.test]`)
- Test: `crates/mockingbird/tests/config.rs`

**Interfaces:**
- Consumes: `mb_store::{validate_hosts, routes_collide}`
- Produces: `FileMock.hosts: Vec<String>` (`#[serde(default)]`)

- [ ] **Step 1: Write the failing tests** (in `tests/config.rs`, using its existing in-memory store and state helpers, and `mockingbird::reconcile::reconcile`)

```rust
const TWO_HOSTED: &str = "mocks:\n  rest:\n    contract: orders.yaml\n    route_prefix: /\n    hosts: [api.github.test]\n  other:\n    contract: orders.yaml\n    route_prefix: /\n    hosts: [HOSTS2]\n";

#[tokio::test]
async fn file_mocks_share_a_prefix_on_distinct_hosts() {
    let ws = Workspace::new();
    ws.write(&TWO_HOSTED.replace("HOSTS2", "uploads.github.test"));
    let (store, state) = (store().await, no_state());
    let cfg = file_config::load(&ws.config_path()).unwrap();
    reconcile(store.as_ref(), state.as_ref(), &cfg).await.unwrap();
    let rest = store.get_mock(DEFAULT_WORKSPACE, "rest").await.unwrap().unwrap();
    assert_eq!(rest.hosts, ["api.github.test"]);
}

#[tokio::test]
async fn file_mocks_with_overlapping_hosts_are_rejected() {
    let ws = Workspace::new();
    ws.write(&TWO_HOSTED.replace("HOSTS2", "api.github.test"));
    let (store, state) = (store().await, no_state());
    let cfg = file_config::load(&ws.config_path()).unwrap();
    let err = reconcile(store.as_ref(), state.as_ref(), &cfg).await.unwrap_err();
    assert!(format!("{err:#}").contains("share the route prefix \"/\""), "{err:#}");
}

#[test]
fn invalid_mock_host_is_rejected_at_load() {
    let ws = Workspace::new();
    ws.write(&TWO_HOSTED.replace("HOSTS2", "Bad.Host:1"));
    let err = file_config::load(&ws.config_path()).unwrap_err();
    assert!(format!("{err:#}").contains("mocks.other.hosts"), "{err:#}");
}
```

(`Workspace::new()` already copies the orders fixture to `orders.yaml`; `store()` and `no_state()` are the file's existing helpers.)

- [ ] **Step 2: Run them and confirm they fail**

Run: `cargo test -p mockingbird --all-features --test config hosts`
Expected: FAIL with "unknown field `hosts`".

- [ ] **Step 3: Implement**

- `validate_file` adds `mb_store::validate_hosts(&mock.hosts).map_err(|e| format!("{prefix}hosts: {e}"))?`.
- In `reconcile`, `prefixes` becomes a `Vec<(String /*prefix*/, Vec<String> /*hosts*/, String /*label*/)>`, checked with `routes_collide`. The message stays `mocks {other:?} and {label:?} share the route prefix {prefix:?}`. The API-mock check compares `(api_mock.route_prefix, api_mock.hosts)` the same way.
- `apply_mock` passes `p.mock.hosts.clone()` in `NewMock` and in `wanted`.
- Docs row: ``| `mocks.<name>.hosts` | list of strings | `[]` | Lowercase host names, no port | | Hosts the mock answers for (by `Host`, e.g. through `mockingbird proxy`). With hosts, two mocks may share a route prefix; a mock with hosts is not reached under other hosts. |``

- [ ] **Step 4: Run them and confirm they pass**

Run: `cargo test -p mockingbird --all-features --test config --test docs_reference`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/mockingbird/src/file_config.rs crates/mockingbird/src/reconcile.rs crates/mockingbird/tests/config.rs docs/configuration.md docs/examples/mockingbird.full.yaml
git commit -m "feat(config): mocks.<name>.hosts"
```

---

### Task 5: Hot reload summary, `config_reload` and stale mocks in `/readyz`

**Files:**
- Modify: `crates/mockingbird/src/watch.rs` (:115-203), `crates/mockingbird/src/reconcile.rs` (`ReconcileReport` :32-50), `crates/mockingbird/src/ops.rs` (`OpsState` :24, `readyz` :98-133)
- Modify: `crates/mockingbird/src/registry.rs` (next to `failed_mocks` :341), `crates/mockingbird/src/config.rs` (`App` :191, `OpsState {` :687, `watch::spawn_with_hook` :762)
- Modify (add `reload: Default::default()`): `crates/mockingbird/tests/observability.rs` (:504, :942, :1028)
- Test: `crates/mockingbird/tests/config.rs`, `crates/mockingbird/tests/registry.rs`

**Interfaces:**
- Produces:
  - `#[derive(Clone, Default)] pub struct ReloadStatus` in `watch.rs`, with `pub fn failed(&self, error: String)`, `pub fn succeeded(&self)` and `pub fn last_error(&self) -> Option<String>`
  - `pub fn spawn_with_status(config_path: PathBuf, store: Arc<dyn ControlStore>, state: Arc<dyn StateStore>, prepare: Prepare, applied: Option<Applied>, status: ReloadStatus) -> anyhow::Result<WatchGuard>`; `spawn_with_hook` delegates with `ReloadStatus::default()`
  - `impl ReconcileReport { pub fn summary(&self) -> String }`, giving `"{added} added, {removed} removed, {changed} changed"`; `changed` counts the distinct mocks in `updated` ∪ `new_versions` that are not in `created`
  - `OpsState.reload: ReloadStatus`; `App.reload: ReloadStatus`
  - `impl MockRegistry { pub fn mock_issues(&self) -> Vec<String> }`, sorted, giving `mock <label>: stale, serving the previous version: <error>` when the mock is still mounted and its rebuild failed, else `mock <label>: failed: <error>` (gRPC service collisions are `failed`)

- [ ] **Step 1: Write the failing tests**

In `tests/config.rs`:

```rust
#[test]
fn reconcile_report_summary_counts_mocks() {
    let report = ReconcileReport {
        created: vec!["a".into()],
        updated: vec!["b".into()],
        deleted: vec!["c".into(), "d".into()],
        new_versions: vec![("a".into(), 1), ("b".into(), 2), ("e".into(), 3)],
        warnings: vec![],
    };
    assert_eq!(report.summary(), "1 added, 2 removed, 2 changed");
}

#[tokio::test]
async fn readyz_degraded_after_failed_reload_until_fixed() {
    let ws = Workspace::new();
    ws.write(BASIC);
    let app = start(config_args(&ws, true)).await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    ws.write("mocks: [this is: not: valid\n");
    let ready = wait_for_readyz(&app.admin, |b| b["status"] == "degraded").await;
    assert!(ready["reasons"].as_array().unwrap().iter().any(|r| r.as_str().unwrap().starts_with("config_reload: ")), "{ready}");
    ws.write(BASIC);
    wait_for_readyz(&app.admin, |b| b["status"] == "ready").await;
}
```

`wait_for_readyz(admin, pred)` is a new local helper that polls `GET {admin}/readyz` every 100 ms for up to 5 s.

In `tests/registry.rs`, extend `failed_rebuild_keeps_serving_previous_version` after `let failed = …`:

```rust
let issues = registry.mock_issues();
assert!(issues[0].starts_with("mock c: stale, serving the previous version: "), "{issues:?}");
```

Add a new test there:

```rust
#[tokio::test]
async fn failed_first_build_is_reported_failed() {
    let store = store().await;
    let mut q = profile("q", "http://127.0.0.1:9".into(), None);
    q.secret_ref = Some("env:MB_TEST_REGISTRY_NOT_SET_7A1C".into());
    store.put_profile(&q).await.unwrap();
    let d = create_mock(&store, "d", "/d", Some("q")).await;
    activate(&store, &d, &fixture_source()).await;
    let mut deps = deps(store.clone());
    deps.offline = false;
    let registry = MockRegistry::new();
    registry.reload_all(&deps).await.unwrap();
    assert!(registry.mock_issues()[0].starts_with("mock d: failed: "));
}
```

- [ ] **Step 2: Run them and confirm they fail**

Run: `cargo test -p mockingbird --all-features --test config summary readyz_degraded --test registry failed_`
Expected: compile FAIL, with "no method named `summary`" and "no method named `mock_issues`".

- [ ] **Step 3: Implement**

- In the watch task:
  - On success, call `status.succeeded()` and log at info `config reloaded: {summary}` (fields `config`, `added`, `removed`, `changed`). Keep the debug line when the report is empty.
  - On failure, call `status.failed(format!("{error:#}"))` and keep the existing error log.
- In `readyz`:
  - Replace the `failed_mocks` loop with `reasons.extend(st.registry.mock_issues())`.
  - Push `format!("config_reload: {e}")` when `st.reload.last_error()` is `Some`.
- In `build_app_with`:
  - Create one `ReloadStatus`.
  - Pass it to `OpsState` and to `watch::spawn_with_status` (in place of `spawn_with_hook`).
  - Store it in `App.reload`.
- Update the `/readyz` doc comment in `ops.rs` with `config_reload: <error>` and the stale/failed wording.

- [ ] **Step 4: Run them and confirm they pass**

Run: `cargo test -p mockingbird --all-features --test config --test registry --test observability --test e2e`
Expected: PASS. `e2e.rs:343` still matches `mock c: `.

- [ ] **Step 5: Commit**

```bash
git add crates/mockingbird
git commit -m "feat(ops): config_reload readiness check, reload summary, stale mocks"
```

---

### Task 6: Local CA (`proxy/ca.rs`)

**Files:**
- Create: `crates/mockingbird/src/proxy/mod.rs` (`pub mod ca;`, the rest added later), `crates/mockingbird/src/proxy/ca.rs`
- Modify: `crates/mockingbird/src/lib.rs` (`pub mod proxy;`), `crates/mockingbird/src/tls.rs` (extract the helper from `server_config` :30-61), `Cargo.toml` and `crates/mockingbird/Cargo.toml` (the dependency pins in the header)
- Test: unit tests in `proxy/ca.rs` (`#[cfg(test)]`, `#[allow(clippy::unwrap_used)]`)

**Interfaces:**
- Produces:
  - `crate::tls::server_config_from(certs: Vec<CertificateDer<'static>>, key: PrivateKeyDer<'static>) -> anyhow::Result<rustls::ServerConfig>`: aws-lc-rs provider, safe defaults, ALPN `h2` then `http/1.1`; `server_config` uses it
  - `pub const CA_COMMON_NAME: &str = "Mockingbird Local CA";`
  - `pub fn default_ca_dir() -> anyhow::Result<PathBuf>`: `MB_CA_DIR` if set and non-empty, else `$HOME` (or `USERPROFILE`) `/.mockingbird/ca`
  - `pub struct LocalCa` (Send + Sync), with:
    - `pub fn load_or_create(dir: &Path) -> anyhow::Result<LocalCa>`
    - `pub fn cert_path(&self) -> PathBuf` (`dir/ca.pem`)
    - `pub fn key_path(&self) -> PathBuf` (`dir/ca.key`)
    - `pub fn cert_pem(&self) -> &str`
    - `pub fn server_config(&self, host: &str) -> anyhow::Result<Arc<rustls::ServerConfig>>` (cached per host)
    - `pub fn write_bundle(&self) -> anyhow::Result<PathBuf>`: writes `dir/bundle.pem` (system roots via `rustls_native_certs::load_native_certs().certs`, falling back to `webpki_root_certs::TLS_SERVER_ROOT_CERTS` when that is empty, then `ca.pem`), rewritten on each call and returned
  - `pub fn ca_cert_path() -> anyhow::Result<PathBuf>`: `LocalCa::load_or_create(&default_ca_dir()?)?.cert_path()`

- [ ] **Step 1: Write the failing tests**

```rust
#[test]
fn creates_once_with_private_key_and_reloads() {
    let dir = tempfile::tempdir().unwrap();
    let ca = LocalCa::load_or_create(dir.path()).unwrap();
    assert!(ca.cert_pem().starts_with("-----BEGIN CERTIFICATE-----"));
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(std::fs::metadata(ca.key_path()).unwrap().permissions().mode() & 0o777, 0o600);
        assert_eq!(std::fs::metadata(dir.path()).unwrap().permissions().mode() & 0o777, 0o700);
    }
    let again = LocalCa::load_or_create(dir.path()).unwrap();
    assert_eq!(again.cert_pem(), ca.cert_pem());
}

#[test]
fn concurrent_creation_yields_one_ca() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().to_path_buf();
    let handles: Vec<_> = (0..4).map(|_| { let p = p.clone(); std::thread::spawn(move || LocalCa::load_or_create(&p).unwrap().cert_pem().to_owned()) }).collect();
    let pems: std::collections::BTreeSet<String> = handles.into_iter().map(|h| h.join().unwrap()).collect();
    assert_eq!(pems.len(), 1);
}

#[tokio::test]
async fn leaf_chains_to_the_ca_with_strict_extensions() {
    let dir = tempfile::tempdir().unwrap();
    let ca = LocalCa::load_or_create(dir.path()).unwrap();
    let server = tokio_rustls::TlsAcceptor::from(ca.server_config("api.example.test").unwrap());
    let mut roots = rustls::RootCertStore::empty();
    roots.add(CertificateDer::from_pem_slice(ca.cert_pem().as_bytes()).unwrap()).unwrap();
    let client = rustls::ClientConfig::builder_with_provider(Arc::new(rustls::crypto::aws_lc_rs::default_provider()))
        .with_safe_default_protocol_versions().unwrap()
        .with_root_certificates(roots).with_no_client_auth();
    let (c, s) = tokio::io::duplex(64 * 1024);
    let accept = tokio::spawn(async move { server.accept(s).await.map(|_| ()) });
    let name = rustls::pki_types::ServerName::try_from("api.example.test").unwrap();
    let tls = tokio_rustls::TlsConnector::from(Arc::new(client)).connect(name, c).await.unwrap();
    accept.await.unwrap().unwrap();
    let leaf = &tls.get_ref().1.peer_certificates().unwrap()[0];
    let (_, cert) = x509_parser::parse_x509_certificate(leaf.as_ref()).unwrap();
    assert!(cert.extensions().iter().any(|e| e.oid == x509_parser::oid_registry::OID_X509_EXT_AUTHORITY_KEY_IDENTIFIER));
    assert!(Arc::ptr_eq(&ca.server_config("api.example.test").unwrap(), &ca.server_config("api.example.test").unwrap()));
}

#[test]
fn bundle_holds_roots_and_the_local_ca() {
    let dir = tempfile::tempdir().unwrap();
    let ca = LocalCa::load_or_create(dir.path()).unwrap();
    let text = std::fs::read_to_string(ca.write_bundle().unwrap()).unwrap();
    assert!(text.matches("BEGIN CERTIFICATE").count() > 10);
    assert!(text.trim_end().ends_with(ca.cert_pem().trim_end()));
}
```

(`x509-parser` is reachable through rcgen's feature. If it is not nameable from the crate, add `x509-parser = "0.18.1"` to `[dev-dependencies]`.)

- [ ] **Step 2: Run them and confirm they fail**

Run: `cargo test -p mockingbird --all-features --lib proxy::ca`
Expected: compile FAIL, with "unresolved module `proxy`".

- [ ] **Step 3: Implement**

- **Creation order:**
  1. `create_dir_all(dir)`, then set it to `0700` (Unix).
  2. If `ca.key` exists, load it.
  3. Otherwise generate a P-256 `KeyPair` and write it with `OpenOptions::new().write(true).create_new(true)` (Unix `.mode(0o600)`).
  4. On `ErrorKind::AlreadyExists`, re-read instead: another process won.
  5. Write `ca.pem` (atomically: a temp file in `dir`, then `rename`) only after the key file was created by this process. A reader that finds the key but not yet `ca.pem` retries for up to 2 s (20 × 100 ms).
- **CA params:**
  - DN: CN `CA_COMMON_NAME`, O `Mockingbird`.
  - `IsCa::Ca(BasicConstraints::Constrained(0))`.
  - Key usages: `KeyCertSign`, `CrlSign`, `DigitalSignature`.
  - Validity: now − 1 day to + 10 years.
  - Load with `Issuer::from_ca_cert_pem(&pem, key)` (type `Issuer<'static, KeyPair>`).
- **Leaf params:**
  - `CertificateParams::new(vec![host])` (an IP string becomes an IP SAN).
  - CN = host.
  - EKU `ServerAuth`.
  - `use_authority_key_identifier_extension = true`.
  - Validity: now − 1 day to + 397 days. Dates come from `time::OffsetDateTime::now_utc()`.
  - Fresh P-256 key.
  - The chain is `[leaf, ca]`.
  - `server_config_from` builds it; cache it in a `Mutex<HashMap<String, Arc<ServerConfig>>>`.
- **Bundle PEM:** base64 (workspace `base64`) with 64-column lines.

- [ ] **Step 4: Run them and confirm they pass**

Run: `cargo test -p mockingbird --all-features --lib proxy::ca` and `cargo test -p mockingbird --all-features --test tls`
Expected: PASS. `tls::llm_client_trusts_outbound_ca` may fail on macOS; that failure predates this work.

- [ ] **Step 5: Commit**

```bash
git add Cargo.toml Cargo.lock crates/mockingbird
git commit -m "feat(proxy): local CA with per-host leaf certificates"
```

---

### Task 7: Placeholder PNG (`proxy/images.rs`)

**Files:**
- Create: `crates/mockingbird/src/proxy/images.rs`
- Modify: `crates/mockingbird/src/proxy/mod.rs` (`pub mod images;`)
- Test: unit tests in `images.rs`

**Interfaces:**
- Produces:
  - `pub const DEFAULT_IMAGE_SIZE: (u32, u32) = (300, 450);`
  - `pub fn image_size(path: &str) -> (u32, u32)`: from the first segment that matches `w<digits>` with width in `1..=2000`: `(w, w * 3 / 2)`; else `DEFAULT_IMAGE_SIZE`
  - `pub fn color_for(path: &str) -> [u8; 3]`: the first 3 bytes of `sha2::Sha256(path)`
  - `pub fn placeholder_png(path: &str) -> Vec<u8>`: an 8-bit RGB PNG (colour type 2) with IHDR, one zlib IDAT (filter byte 0 per row) and IEND; CRC from `flate2::Crc`
  - `pub fn placeholder_response(method: &axum::http::Method, path: &str) -> axum::response::Response`: for GET, 200 with `content-type: image/png`, `cache-control: public, max-age=86400` and the PNG; HEAD answers the same with no body; any other method gets a 405 problem+json (`mb_protocols::http_rest::problem`)

- [ ] **Step 1: Write the failing tests**

```rust
#[test]
fn size_comes_from_the_w_segment() {
    assert_eq!(image_size("/t/p/w92/abc.jpg"), (92, 138));
    assert_eq!(image_size("/t/p/original/abc.jpg"), (300, 450));
    assert_eq!(image_size("/t/p/w99999/abc.jpg"), (300, 450));
}

#[test]
fn png_is_well_formed_and_solid() {
    let png = placeholder_png("/t/p/w4/x.jpg");
    assert_eq!(&png[..8], b"\x89PNG\r\n\x1a\n");
    assert_eq!(&png[12..16], b"IHDR");
    assert_eq!(u32::from_be_bytes(png[16..20].try_into().unwrap()), 4);
    assert_eq!(u32::from_be_bytes(png[20..24].try_into().unwrap()), 6);
    let mut crc = flate2::Crc::new();
    crc.update(&png[12..29]);
    assert_eq!(crc.sum(), u32::from_be_bytes(png[29..33].try_into().unwrap()));
    let idat_len = u32::from_be_bytes(png[33..37].try_into().unwrap()) as usize;
    assert_eq!(&png[37..41], b"IDAT");
    let mut raw = Vec::new();
    std::io::Read::read_to_end(&mut flate2::read::ZlibDecoder::new(&png[41..41 + idat_len]), &mut raw).unwrap();
    assert_eq!(raw.len(), 6 * (1 + 4 * 3));
    assert_eq!(&raw[1..4], &color_for("/t/p/w4/x.jpg"));
    assert!(png.ends_with(b"IEND\xaeB`\x82"));
    assert_ne!(color_for("/a.jpg"), color_for("/b.jpg"));
}
```

- [ ] **Step 2: Run them and confirm they fail**

Run: `cargo test -p mockingbird --all-features --lib proxy::images`
Expected: compile FAIL, with "cannot find function `placeholder_png`".

- [ ] **Step 3: Implement** the functions above in `images.rs` (`flate2::write::ZlibEncoder`, `Compression::default()`).

- [ ] **Step 4: Run them and confirm they pass**

Run: `cargo test -p mockingbird --all-features --lib proxy::images`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/mockingbird/src/proxy
git commit -m "feat(proxy): generated placeholder PNG images"
```

---

### Task 8: Proxy server: CONNECT intercept, passthrough, block, plain HTTP

**Files:**
- Create: `crates/mockingbird/src/proxy/server.rs`, `crates/mockingbird/src/proxy/forward.rs`
- Modify: `crates/mockingbird/src/proxy/mod.rs`, `crates/mockingbird/src/file_config.rs` (only the `Unmapped` enum here; the config section comes in Task 9)
- Test: `crates/mockingbird/tests/proxy.rs` (new)

**Interfaces:**
- Consumes:
  - `LocalCa::server_config` (Task 6)
  - `placeholder_response` (Task 7)
  - `registry::{normalize_host, request_host}` and `MockRegistry::claims_host` (Task 2)
- Produces:
  - In `file_config.rs`: `#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Default, clap::ValueEnum)] #[serde(rename_all = "lowercase")] pub enum Unmapped { #[default] Pass, Block }`
  - In `proxy/mod.rs`:
    - `pub const DEFAULT_PROXY_PORT: u16 = 8888;`
    - `pub trait HostClaims: Send + Sync + 'static { fn claims(&self, host: &str) -> bool; }`, with `impl HostClaims for MockRegistry` (delegates to `claims_host`)
    - `pub struct ProxyState`, with `pub fn new(claims: Arc<dyn HostClaims>, upstream: axum::Router, ca: Arc<LocalCa>, unmapped: Unmapped, images: Vec<String>) -> Arc<ProxyState>` and `pub fn first_sighting(&self, host: &str) -> bool` (true once per host; used to log unmapped hosts once at info, `proxy: unmapped host {host} ({pass|block})`)
    - `pub async fn serve(listener: tokio::net::TcpListener, state: Arc<ProxyState>, stop: tokio::sync::watch::Receiver<bool>) -> std::io::Result<()>`
  - In `forward.rs`:
    - `pub fn forward_client() -> anyhow::Result<reqwest::Client>`: `.no_proxy()`, no redirects
    - `pub async fn forward(client: &reqwest::Client, target: reqwest::Url, host_header: Option<&str>, req: axum::extract::Request) -> axum::response::Response`: streams both bodies, drops hop-by-hop headers (`connection`, `proxy-connection`, `proxy-authorization`, `keep-alive`, `te`, `trailer`, `transfer-encoding`, `upgrade`), and answers 502 problem+json on a connection error

- [ ] **Step 1: Write the failing tests** (`tests/proxy.rs`, `#![allow(clippy::unwrap_used)]`)

Helpers:
- `hosted_registry(host) -> Arc<MockRegistry>`: an in-memory SQLite store and one mock `orders` at `/` with `hosts: [host]` on `fixtures/openapi/orders-3.0.yaml`, using `deps` copied from `tests/registry.rs`.
- `start_proxy(registry, unmapped, images) -> (String /*http://addr*/, TempDir /*CA dir*/, watch::Sender<bool>)`: binds `127.0.0.1:0` and spawns `proxy::serve` with `ProxyState::new(registry.clone(), registry.router(), Arc::new(LocalCa::load_or_create(dir)?), …)`.
- `client(proxy_url, ca_pem: &[u8]) -> reqwest::Client`: `.proxy(reqwest::Proxy::all(proxy_url))`, `.tls_certs_only([Certificate::from_pem(ca_pem)])`.

```rust
#[tokio::test]
async fn intercepts_https_for_a_mapped_host() {
    let registry = hosted_registry("api.orders.test").await;
    let (proxy, ca_dir, _stop) = start_proxy(registry, Unmapped::Pass, vec![]).await;
    let ca = std::fs::read(ca_dir.path().join("ca.pem")).unwrap();
    let r = client(&proxy, &ca).get("https://api.orders.test/orders").send().await.unwrap();
    assert_eq!(r.status(), 200);
    assert!(r.headers().contains_key("x-mockingbird-source"));
}

#[tokio::test]
async fn plain_http_to_a_mapped_host_is_served() {
    let registry = hosted_registry("api.orders.test").await;
    let (proxy, ca_dir, _stop) = start_proxy(registry, Unmapped::Block, vec![]).await;
    let ca = std::fs::read(ca_dir.path().join("ca.pem")).unwrap();
    let r = client(&proxy, &ca).get("http://api.orders.test/orders").send().await.unwrap();
    assert_eq!(r.status(), 200);
}

#[tokio::test]
async fn unmapped_connect_is_tunnelled_untouched() {
    // A TLS server with the fixture certificate (localhost), answering "upstream".
    let upstream = fixture_tls_server().await; // axum_server + RustlsConfig::from_pem_file(fixtures/tls/server.pem, server-key.pem)
    let registry = hosted_registry("api.orders.test").await;
    let (proxy, _ca, _stop) = start_proxy(registry, Unmapped::Pass, vec![]).await;
    let fixture_ca = std::fs::read(fixture("tls/ca.pem")).unwrap();
    let r = client(&proxy, &fixture_ca).get(format!("https://localhost:{}/", upstream.port())).send().await.unwrap();
    assert_eq!(r.text().await.unwrap(), "upstream");
}

#[tokio::test]
async fn block_mode_answers_502_naming_the_host() {
    let registry = hosted_registry("api.orders.test").await;
    let (proxy, ca_dir, _stop) = start_proxy(registry, Unmapped::Block, vec![]).await;
    let ca = std::fs::read(ca_dir.path().join("ca.pem")).unwrap();
    let c = client(&proxy, &ca);
    let r = c.get("http://elsewhere.test/x").send().await.unwrap();
    assert_eq!(r.status(), 502);
    assert_eq!(r.headers()["content-type"], "application/problem+json");
    assert!(r.text().await.unwrap().contains("elsewhere.test"));
    assert!(c.get("https://elsewhere.test/x").send().await.is_err());
}

#[tokio::test]
async fn image_hosts_get_placeholder_pngs() {
    let registry = hosted_registry("api.orders.test").await;
    let (proxy, ca_dir, _stop) = start_proxy(registry, Unmapped::Block, vec!["image.tmdb.test".into()]).await;
    let ca = std::fs::read(ca_dir.path().join("ca.pem")).unwrap();
    let r = client(&proxy, &ca).get("https://image.tmdb.test/t/p/w92/a.jpg").send().await.unwrap();
    assert_eq!(r.headers()["content-type"], "image/png");
    let body = r.bytes().await.unwrap();
    assert_eq!(u32::from_be_bytes(body[16..20].try_into().unwrap()), 92);
}

#[test]
fn unmapped_hosts_are_logged_once() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let registry = rt.block_on(hosted_registry("a.test"));
    let dir = tempfile::tempdir().unwrap();
    let ca = Arc::new(LocalCa::load_or_create(dir.path()).unwrap());
    let st = ProxyState::new(registry.clone(), registry.router(), ca, Unmapped::Pass, vec![]);
    assert!(st.first_sighting("x.test"));
    assert!(!st.first_sighting("x.test"));
}

#[tokio::test]
async fn forwarding_ignores_proxy_environment() {
    // A bogus proxy configured for the process must not be used by the forwarder.
    let upstream = wiremock::MockServer::start().await;
    wiremock::Mock::given(wiremock::matchers::method("GET")).respond_with(wiremock::ResponseTemplate::new(204)).mount(&upstream).await;
    let registry = hosted_registry("api.orders.test").await;
    let (proxy, ca_dir, _stop) = start_proxy(registry, Unmapped::Pass, vec![]).await;
    let ca = std::fs::read(ca_dir.path().join("ca.pem")).unwrap();
    let r = client(&proxy, &ca).get(format!("{}/ping", upstream.uri())).send().await.unwrap();
    assert_eq!(r.status(), 204);
}
```

- [ ] **Step 2: Run them and confirm they fail**

Run: `cargo test -p mockingbird --all-features --test proxy`
Expected: compile FAIL, with "unresolved import `mockingbird::proxy::serve`".

- [ ] **Step 3: Implement `serve`**

`serve` runs an accept loop: `tokio::select!` on `listener.accept()` and `stop`. For each connection it spawns `hyper::server::conn::http1::Builder::new().serve_connection(TokioIo::new(tcp), service_fn(handle)).with_upgrades()`. `handle` follows this sequence:

```text
handle(req):
  if req.method() == CONNECT:
    host = normalize_host(req.uri().authority()?.host()); port = authority.port_u16().unwrap_or(443)
    if host is None → 400 problem
    mapped = state.images.contains(host) || state.claims.claims(host)
    if !mapped:
        if state.first_sighting(host) { info!(host, mode, "proxy: unmapped host") }
        if unmapped == Block → return 502 problem("Host not mocked", "proxy.unmapped is block: {host} is not served by any mock")
    spawn(async move {
        let upgraded = TokioIo::new(hyper::upgrade::on(req).await?);
        if mapped:
            let tls = TlsAcceptor::from(state.ca.server_config(&host)?).accept(upgraded).await?;
            let svc = service_fn(move |r: Request<Incoming>| {
                let mut r = r.map(axum::body::Body::new);
                if r.headers().get(HOST).is_none() && r.uri().authority().is_none() { r.headers_mut().insert(HOST, host) }
                if state.images.contains(&host) { ready(placeholder_response(r.method(), r.uri().path())) }
                else { state.upstream.clone().oneshot(r) }   // Infallible
            });
            hyper_util::server::conn::auto::Builder::new(TokioExecutor::new())
                .serve_connection_with_upgrades(TokioIo::new(tls), svc).await   // h2 or http/1.1 by ALPN
        else:
            let mut upstream = TcpStream::connect((host.as_str(), port)).await?;
            tokio::io::copy_bidirectional(&mut upgraded, &mut upstream).await
      // every error here is logged at debug and closes the connection
    })
    return 200 with an empty body
  else (absolute-form request):
    host = request_host(&req)
    if images.contains(host) → placeholder_response
    if claims(host)        → upstream.oneshot(req)        // dispatch reads the URI authority
    log the first sighting; Block → 502 as above; Pass → forward(&client, req.uri() as Url, None, req)
```

The unmapped `Pass` forward uses `forward_client()`, built once in `ProxyState::new`. Building it can fail, so `new` returns `Arc<ProxyState>` after mapping a builder error to a client with defaults. `reqwest::Client::builder().no_proxy().build()` only fails on TLS backend init; log and fall back to `reqwest::Client::new()`, which is infallible.

- [ ] **Step 4: Run them and confirm they pass**

Run: `cargo test -p mockingbird --all-features --test proxy`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/mockingbird
git commit -m "feat(proxy): intercepting HTTP proxy for mapped hosts, passthrough or block for the rest"
```

---

### Task 9: `proxy:` config, `serve` starts the proxy, `GET /proxy/hosts`

**Files:**
- Modify: `crates/mockingbird/src/file_config.rs` (`FileConfig` :40 adds `pub proxy: ProxyConfig`; `validate_file` :1196)
- Modify: `crates/mockingbird/src/config.rs`:
  - `ServeArgs` :118
  - `apply_flags` :311
  - `App` :191 adds `pub proxy: Option<Arc<ProxyState>>`
  - `build_app_with` :~650 builds it from `data` and `registry`
- Modify: `crates/mockingbird/src/server.rs` (`Listeners` :50 adds `pub proxy: Option<TcpListener>`; `run_with` binds `server.host:proxy.port` when `app.proxy` is `Some` and the role binds data; `serve_app` joins `proxy::serve`)
- Modify: `crates/mockingbird/src/ops.rs` (route `GET /proxy/hosts`)
- Modify: `crates/mockingbird/tests/{roles,tls,ha_cluster}.rs` (the `Listeners { .. }` literals get `proxy: None`)
- Modify: `docs/configuration.md` (new `## proxy` section after `## outbound`), `docs/examples/mockingbird.full.yaml` (`proxy:` block)
- Test: `crates/mockingbird/tests/proxy.rs`

**Interfaces:**
- Consumes: `ProxyState::new`, `proxy::serve`, `LocalCa::load_or_create`, `default_ca_dir` (Tasks 6 and 8)
- Produces:
  - `#[serde(deny_unknown_fields, default)] pub struct ProxyConfig { pub enabled: bool /*false*/, pub port: u16 /*8888*/, pub unmapped: Unmapped /*pass*/, pub images: Vec<String> /*[]*/, pub ca_dir: Option<PathBuf> /*unset → default_ca_dir()*/ }`
  - `ProxyConfig::effective_ca_dir(&self) -> anyhow::Result<PathBuf>`
  - `ServeArgs { …, #[arg(long)] pub proxy: bool, #[arg(long)] pub proxy_port: Option<u16> }`: `--proxy` sets `proxy.enabled = true`, and `--proxy-port` sets `proxy.port` (WS5's `start_server(proxy: true)` passes `--proxy`)
  - `GET /proxy/hosts` on the ops router returns `200 {"hosts": [...]}`, built from `registry.claimed_hosts()`. Image hosts are not included.

- [ ] **Step 1: Write the failing tests**

```rust
#[tokio::test]
async fn serve_starts_the_proxy_from_config() {
    let dir = tempfile::tempdir().unwrap();
    let ca_dir = dir.path().join("ca");
    let orders = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/openapi/orders-3.0.yaml");
    let cfg = dir.path().join("mockingbird.yaml");
    std::fs::write(&cfg, format!(
        "storage:\n  url: sqlite::memory:\nproxy:\n  enabled: true\n  unmapped: block\n  ca_dir: {}\nmocks:\n  orders:\n    contract: {}\n    route_prefix: /\n    hosts: [api.orders.test]\n",
        ca_dir.display(), orders.display())).unwrap();
    let app = build_app(&ServeArgs { config: Some(cfg), offline: true, ..ServeArgs::default() }, None).await.unwrap();
    assert!(app.proxy.is_some());
    let (data, admin, proxy) = (ephemeral().await, ephemeral().await, ephemeral().await);
    let (admin_url, proxy_url) = (format!("http://{}", admin.local_addr().unwrap()), format!("http://{}", proxy.local_addr().unwrap()));
    let (_stop, stop_rx) = watch::channel(false);
    tokio::spawn(serve_app(app, Listeners { data: Some(data), admin: Some(admin), grpc: None, proxy: Some(proxy) }, stop_rx));
    let ca = std::fs::read(ca_dir.join("ca.pem")).unwrap();
    let r = client(&proxy_url, &ca).get("https://api.orders.test/orders").send().await.unwrap();
    assert_eq!(r.status(), 200);
    let hosts: Value = reqwest::get(format!("{admin_url}/proxy/hosts")).await.unwrap().json().await.unwrap();
    assert_eq!(hosts, json!({"hosts": ["api.orders.test"]}));
}

#[test]
fn proxy_flags_override_config() {
    let mut cfg = FileConfig::default();
    apply_flags(&mut cfg, &ServeArgs { proxy: true, proxy_port: Some(9999), ..ServeArgs::default() });
    assert!(cfg.proxy.enabled);
    assert_eq!(cfg.proxy.port, 9999);
    assert_eq!(FileConfig::default().proxy.port, 8888);
    assert_eq!(FileConfig::default().proxy.unmapped, Unmapped::Pass);
}
```

Also add a `tests/config.rs` test: `proxy.images: [Bad:1]` fails `load` with `proxy.images`.

- [ ] **Step 2: Run them and confirm they fail**

Run: `cargo test -p mockingbird --all-features --test proxy serve_starts proxy_flags --test config proxy`
Expected: compile FAIL, with "no field `proxy` on type `App`".

- [ ] **Step 3: Implement**

- `validate_file`: `mb_store::validate_hosts(&self.proxy.images)` mapped to `proxy.images: {e}`.
- `build_app_with`, for `role != Control && cfg.proxy.enabled`:
  1. `LocalCa::load_or_create(&cfg.proxy.effective_ca_dir()?)` (errors carry `proxy.ca_dir` context).
  2. `ProxyState::new(registry.clone(), data.clone(), ca, cfg.proxy.unmapped, cfg.proxy.images.clone())`.
- `run_with` logs `proxy on http://{host}:{port} (CA {path})`. `serve_app` adds the proxy future to the `join!` and reports its errors with the prefix `proxy: `.
- Docs `## proxy` table: rows `proxy`, `proxy.enabled` (`--proxy`), `proxy.port` (`8888`, `--proxy-port`), `proxy.unmapped` (`pass`/`block`), `proxy.images` and `proxy.ca_dir` (default `~/.mockingbird/ca`, env `MB_CA_DIR`). Add `MB_CA_DIR`, `--proxy` and `--proxy-port` to "Environment variables and flags".

- [ ] **Step 4: Run them and confirm they pass**

Run: `cargo test -p mockingbird --all-features --test proxy --test config --test docs_reference --test roles --test tls --test ha_cluster`
Expected: PASS (except the known macOS `tls::llm_client_trusts_outbound_ca`).

- [ ] **Step 5: Commit**

```bash
git add crates/mockingbird docs/configuration.md docs/examples/mockingbird.full.yaml
git commit -m "feat(serve): proxy section; serve starts the proxy; /proxy/hosts"
```

---

### Task 10: CLI: `ca path`, `proxy env`, standalone `proxy`; proxy docs

**Files:**
- Create: `crates/mockingbird/src/proxy/env.rs`, `crates/mockingbird/src/proxy/standalone.rs`, `docs/proxy.md`
- Modify: `crates/mockingbird/src/config.rs` (`Command` :84 adds `Proxy(crate::proxy::ProxyArgs)` and `#[command(subcommand)] Ca(crate::proxy::CaCommand)`), `crates/mockingbird/src/main.rs` (:40-62 dispatch)
- Modify: `docs/configuration.md` (link to `proxy.md` from `## proxy`)
- Test: unit tests in `proxy/env.rs`; `crates/mockingbird/tests/proxy.rs` (assert_cmd and standalone)

**Interfaces:**
- Consumes: `LocalCa`, `default_ca_dir`, `write_bundle` (Task 6); `ProxyState`, `serve`, `forward` (Task 8); `GET /proxy/hosts` (Task 9)
- Produces (**WS5 calls these from `mcp.rs`**):
  - `pub struct ProxyEnvOptions { pub host: String, pub port: u16, pub ca_dir: PathBuf }`
  - `pub fn options_for(config: Option<&Path>, port: Option<u16>) -> anyhow::Result<ProxyEnvOptions>`:
    - port: the flag, else `proxy.port` from the config, else 8888
    - host: `server.host`, except that `0.0.0.0`/`::` become `127.0.0.1`
    - ca_dir: `proxy.ca_dir`, else `default_ca_dir()`
  - `#[derive(Serialize, Debug, Clone, PartialEq)] pub struct ProxyEnv { pub vars: BTreeMap<String, String>, pub ca_path: PathBuf, pub bundle_path: PathBuf }`
  - `pub fn proxy_env(opts: &ProxyEnvOptions) -> anyhow::Result<ProxyEnv>`: creates the CA if missing and writes the bundle
  - `impl ProxyEnv { pub fn to_shell(&self) -> String }`: one `export KEY='value'` line per variable, sorted
  - `#[derive(Subcommand)] pub enum CaCommand { /// Print the local CA certificate path (created if missing). Path }`
  - `#[derive(clap::Args)] #[command(args_conflicts_with_subcommands = true)] pub struct ProxyArgs { #[command(subcommand)] pub cmd: Option<ProxyCommand>, #[command(flatten)] pub run: ProxyRunArgs }`
  - `pub enum ProxyCommand { Env(ProxyEnvArgs) }`
  - `ProxyEnvArgs { -c/--config, --port, --json }`
  - `ProxyRunArgs { -c/--config, --host: Option<IpAddr>, --port: Option<u16>, --data-url: Option<String>, --unmapped: Option<Unmapped>, --image-host: Vec<String> }`
  - `pub struct StandaloneOptions { pub data_url: reqwest::Url, pub admin_url: String, pub ca_dir: PathBuf, pub unmapped: Unmapped, pub images: Vec<String> }`
  - `pub async fn run_standalone(listener: TcpListener, opts: StandaloneOptions, stop: watch::Receiver<bool>) -> anyhow::Result<()>`:
    - claims: a `RemoteClaims`, refreshed from `GET {admin_url}/proxy/hosts` at start (failing with `no Mockingbird server answers at {admin_url}`) and then every 2 s
    - upstream: an axum fallback that calls `forward(&client, data_url.join(path_and_query), Some(host), req)`
  - `pub async fn run(args: ProxyArgs, admin_url: &str) -> anyhow::Result<()>`: for `Env`, prints `to_shell()` or pretty JSON of `ProxyEnv`; with no subcommand, binds and runs `run_standalone` until `server::shutdown_signal()`
  - The exact variable set from `proxy_env`, all values strings:
    - `HTTPS_PROXY`, `HTTP_PROXY`, `https_proxy`, `http_proxy` = `http://{host}:{port}`
    - `NO_PROXY`, `no_proxy` = `localhost,127.0.0.1`
    - `NODE_EXTRA_CA_CERTS` = ca.pem
    - `SSL_CERT_FILE`, `REQUESTS_CA_BUNDLE` = bundle.pem
    - `NODE_USE_ENV_PROXY` = `1`

    The lowercase copies are there because curl and several Python and Go tools read only lowercase `http_proxy`.

- [ ] **Step 1: Write the failing tests**

Unit test in `env.rs`:

```rust
#[test]
fn env_names_proxy_ca_and_bundle() {
    let dir = tempfile::tempdir().unwrap();
    let env = proxy_env(&ProxyEnvOptions { host: "127.0.0.1".into(), port: 9999, ca_dir: dir.path().into() }).unwrap();
    let v = |k: &str| env.vars[k].clone();
    assert_eq!(v("HTTPS_PROXY"), "http://127.0.0.1:9999");
    assert_eq!(v("http_proxy"), "http://127.0.0.1:9999");
    assert_eq!(v("NO_PROXY"), "localhost,127.0.0.1");
    assert_eq!(v("NODE_USE_ENV_PROXY"), "1");
    assert_eq!(PathBuf::from(v("NODE_EXTRA_CA_CERTS")), dir.path().join("ca.pem"));
    assert_eq!(PathBuf::from(v("SSL_CERT_FILE")), dir.path().join("bundle.pem"));
    assert_eq!(v("REQUESTS_CA_BUNDLE"), v("SSL_CERT_FILE"));
    assert_eq!(env.vars.len(), 11);
    assert!(env.to_shell().contains("export HTTPS_PROXY='http://127.0.0.1:9999'\n"));
}
```

In `tests/proxy.rs`:

```rust
#[test]
fn ca_path_prints_the_certificate_path() {
    let dir = tempfile::tempdir().unwrap();
    let out = assert_cmd::Command::cargo_bin("mockingbird").unwrap()
        .env("MB_CA_DIR", dir.path()).args(["ca", "path"]).assert().success().get_output().stdout.clone();
    let path = dir.path().join("ca.pem");
    assert_eq!(String::from_utf8(out).unwrap(), format!("{}\n", path.display()));
    assert!(path.exists());
}

#[test]
fn proxy_env_prints_exports_and_json() {
    let dir = tempfile::tempdir().unwrap();
    let cmd = |args: &[&str]| assert_cmd::Command::cargo_bin("mockingbird").unwrap()
        .env("MB_CA_DIR", dir.path()).args(args).assert().success().get_output().stdout.clone();
    let text = String::from_utf8(cmd(&["proxy", "env", "--port", "9999"])).unwrap();
    assert!(text.contains("export NODE_USE_ENV_PROXY='1'"), "{text}");
    let json: Value = serde_json::from_slice(&cmd(&["proxy", "env", "--json"])).unwrap();
    assert_eq!(json["vars"]["HTTPS_PROXY"], "http://127.0.0.1:8888");
}

#[tokio::test]
async fn standalone_proxy_forwards_to_a_running_server() {
    // App from the Task 9 config but with proxy disabled; serve data + admin on ephemeral ports.
    let (data_url, admin_url, _stop) = serve_hosted_app("api.orders.test").await;
    let ca_dir = tempfile::tempdir().unwrap();
    let listener = ephemeral().await;
    let proxy_url = format!("http://{}", listener.local_addr().unwrap());
    let (_tx, rx) = watch::channel(false);
    tokio::spawn(run_standalone(listener, StandaloneOptions {
        data_url: data_url.parse().unwrap(), admin_url, ca_dir: ca_dir.path().into(),
        unmapped: Unmapped::Block, images: vec![],
    }, rx));
    tokio::time::sleep(Duration::from_millis(100)).await;
    let ca = std::fs::read(ca_dir.path().join("ca.pem")).unwrap();
    let r = client(&proxy_url, &ca).get("https://api.orders.test/orders").send().await.unwrap();
    assert_eq!(r.status(), 200, "Host survives forwarding, so the hosted mock answers");
}
```

- [ ] **Step 2: Run them and confirm they fail**

Run: `cargo test -p mockingbird --all-features --lib proxy::env` and `cargo test -p mockingbird --all-features --test proxy ca_path proxy_env standalone`
Expected: FAIL. The functions do not exist, and the binary answers `unrecognized subcommand 'ca'`.

- [ ] **Step 3: Implement** the interfaces above. `ca path` prints `ca_cert_path()?.display()` followed by a newline. Keep `main.rs` changes to two match arms.

  `docs/proxy.md` gets three sections:
  - **Setup:** `eval "$(mockingbird proxy env)"`, or `mockingbird serve -c … --proxy`.
  - **How it works:**
    - Mapped hosts are intercepted.
    - Unmapped hosts are passed through or blocked.
    - The CA lives at `~/.mockingbird/ca` and is never installed system-wide.
  - **Per runtime:**
    - Node `fetch`: `NODE_USE_ENV_PROXY=1` needs Node **≥ 24.5.0 or ≥ 22.21.0**. The implementer re-checks this against the Node.js changelog / `nodejs.org/en/learn/http/enterprise-network-configuration` and records the verified minimum here and in the PR. On older Node, `node --use-env-proxy` behaves the same where it is supported; otherwise use `global-agent`.
    - Node `axios`/`got`: use `global-agent` (`GLOBAL_AGENT_HTTP_PROXY`).
    - Python `requests`/`httpx`: `HTTPS_PROXY` and `REQUESTS_CA_BUNDLE`/`SSL_CERT_FILE`.
    - curl: lowercase `http_proxy`/`https_proxy` and `SSL_CERT_FILE` (OpenSSL builds).
    - Go: `HTTPS_PROXY` and `SSL_CERT_FILE`.

- [ ] **Step 4: Run them and confirm they pass**

Run: `cargo test -p mockingbird --all-features --lib proxy` and `cargo test -p mockingbird --all-features --test proxy --test docs_reference`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/mockingbird docs/proxy.md docs/configuration.md
git commit -m "feat(cli): mockingbird ca path, proxy env and standalone proxy"
```

---

### Task 11: Gates, changelog, PR

**Files:**
- Modify: `CHANGELOG.md` (`## [Unreleased]` → `### Added`/`### Changed`)

- [ ] **Step 1: CHANGELOG entries**

Under `### Added`:
- Mocks take `hosts:` (config, Admin API, console client). Dispatch filters by `Host`, then takes the longest prefix, so mocks on different hosts may share a route prefix (GitHub REST at `/` and GraphQL at `/graphql` on `api.github.com`). Migration `0600_mock_hosts` adds `mocks.hosts`.
- `mockingbird proxy` (and `serve --proxy` / `proxy.enabled`):
  - an HTTP proxy that intercepts HTTPS to mapped hosts with a local CA (`~/.mockingbird/ca`, `MB_CA_DIR`, `proxy.ca_dir`);
  - `proxy.unmapped: pass | block` (default `pass`);
  - placeholder PNGs for `proxy.images`;
  - `mockingbird ca path` and `mockingbird proxy env`;
  - `GET /proxy/hosts` on the ops port.

Under `### Changed`:
- `/readyz` reports `config_reload: <error>` after a failed config reload until the next good one, and words a failed mock rebuild as `stale, serving the previous version` or `failed`.
- Config reloads log `config reloaded: N added, N removed, N changed`.

- [ ] **Step 2: Run every local gate**

```bash
cargo fmt --all --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-features
cd crates/mb-console/web && npm run check:api && npm run typecheck && npm run lint && npm test
```

Expected: all PASS. Known exceptions that predate this work: `tls::llm_client_trusts_outbound_ca` on macOS, and the flaky `graphql::rejections_are_recorded` (re-run once). Any other failure gets fixed before the PR.

- [ ] **Step 3: Commit, push, open the PR (do not merge)**

```bash
git add CHANGELOG.md
git commit -m "docs: changelog for hosts, proxy and reload readiness

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>"
git push -u origin feat/ws4-routing-proxy
gh pr create --base main --title "WS4: hosts, mockingbird proxy with local CA, placeholder images, reload readiness" --body "$(cat <<'EOF'
Implements WS4 of docs/superpowers/specs/2026-10-22-improvements-backlog-design.md (plan: docs/superpowers/plans/2026-10-22-ws4-routing-proxy.md).

- 4.1 `hosts:` on mocks; host-then-longest-prefix dispatch; duplicate checks compare (prefix, host set) in registry, Admin API and reconcile.
- 4.2 `mockingbird proxy` / `serve --proxy`: CONNECT intercept for mapped hosts (rcgen local CA), `proxy.unmapped: pass|block`, `ca path`, `proxy env`, `/proxy/hosts`.
- 4.3 placeholder PNGs for `proxy.images`.
- 4.4 reload summary log, `config_reload` and stale mocks in `/readyz`.

Verified Node minimum for NODE_USE_ENV_PROXY: <fill in>.
For WS5: `proxy::env::{options_for, proxy_env, ProxyEnv}`, `proxy::ca::ca_cert_path`, `serve --proxy/--proxy-port`.

🤖 Generated with [Claude Code](https://claude.com/claude-code)
EOF
)"
```

Expected: the PR URL is printed. Leave it unmerged until the user approves.

---

## Notes for neighbouring workstreams

- **WS1** also adds mock store fields. It should name its migration `0610_…` or later; this plan takes `0600_mock_hosts`. Both add fields to `Mock`/`NewMock` and `MockView`/`CreateMock`/`PatchMock`, so expect textual conflicts in `model.rs`, the `*.rs` store column lists, `mb-admin/src/mocks.rs` and every `NewMock { .. }` literal in the tests.
- **WS5:**
  - `start_server(proxy: true)` adds `--proxy` (and optionally `--proxy-port`) to the `serve` command in `daemon.rs::start`.
  - MCP `proxy_env` returns `proxy::env::proxy_env(&proxy::env::options_for(config_path, None)?)?` as JSON.
  - The CA path comes from `proxy::ca::ca_cert_path()`.
