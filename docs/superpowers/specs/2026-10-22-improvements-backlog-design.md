# Improvements backlog: design

Date: 2026-10-22. Source: [`docs/improvements.md`](../../improvements.md), audited against `main` at `3d058b2`.

## Goal

Close every open item in `docs/improvements.md` (all phases, the smaller items and the key setup idea) in one coordinated pass. Success means:

- the three field-trial apps (Stripe `accept-a-payment`, `github-readme-stats`, `nuxt/movies`) run against Mockingbird **with no source edits**, through `mockingbird proxy`, on the full upstream contracts, with OpenRouter `deepseek/deepseek-v4.1-flash`;
- every item in `improvements.md` is either in **Done** with its PR, or explicitly re-scoped with a reason;
- CI gates pass on every PR (`fmt`, `clippy -D warnings`, `test`, console checks when touched).

## Decisions

| Question | Decision |
|---|---|
| Scope | Everything, including proxy mode, key setup page and webhooks. |
| Execution | Workstreams by file ownership, one branch and PR each, run as parallel sessions in their own worktrees, in waves. Each PR merges only with the user's OK. |
| Proxy, unmapped hosts | `proxy.unmapped: pass \| block`, default `pass` (CONNECT tunnel, not intercepted; logged). |
| Consistency | On by default; `consistency: off` per mock disables the response cache and list→detail seeding. Stored GETs from state are always on. |
| Unknown ids | Opt-in per mock: `unknown_ids: 404`. Default keeps generating. |
| Key store | A `0600` file at `~/.mockingbird/secrets/<name>` (directory `0700`), referenced with the existing `${file:…}`. The server refuses a secrets file with wider permissions. No OS keychain. |

## Already done (doc is stale)

Four Phase 2 items shipped in #32 and move to **Done** in workstream 7: `$ref` inside `x-*` (`3a3471c`), GraphQL implementation covariance (`a7f3213`), form/multipart bodies and bracket keys (`4bf28e6`, `085cee7`, `bae24bc`), validation failures surfaced (`e26d15f`).

## Waves

```
Wave 1 (parallel):  WS1 engine   WS2 protocols   WS3 import   WS4 routing/proxy
Wave 2 (parallel):  WS5 agent/MCP (needs WS3, WS4)   WS6 webhooks (needs WS1)
Wave 3 (here):      WS7 trials, measurements, improvements.md
```

Merge order inside a wave: whichever is green first; later PRs rebase. Expected textual overlap only in `crates/mockingbird/src/file_config.rs`, `docs/configuration.md` and `docs/examples/mockingbird.full.yaml` (new config keys from WS1, WS4, WS6); each workstream adds its keys in its own section of those files.

---

## WS1: Engine consistency and faker

Files: `crates/mb-engine/src/*`, `crates/mb-protocols/src/call.rs`, `crates/mb-protocols/src/lib.rs` (scenarios), the mock's config/store fields for `consistency`, `unknown_ids`, `default_scenario`.

### 1.1 Stored GETs from session state
`call.rs` `read_context` already loads the target with `state.get`. When the operation is a read (`OperationKind::Query`) and a target exists and the stored value validates against the response schema, return it without calling the LLM. New `Source::State` (shows in `X-Mockingbird-Source` and the request log). If the stored value does not validate (partial resource from a create), keep today's path: generate and overlay the target.

### 1.2 Response cache
Per session, keyed by `(operation id, canonical parameters)` where canonical parameters are path params, sorted query params and, for GraphQL, the root field plus its arguments and the narrowed selection hash. Holds the final response (status, body). Cleared on any write to the same resource type in the session and on session reset. Lives in session state (`mb-state`) so it follows existing session lifetimes; bounded per session (LRU, default 500 entries). `consistency: off` bypasses it. Scenario and forced-status calls bypass it.

### 1.3 List → detail seeding
When a list response is produced (LLM or cache), store each item that carries an identity field as a resource in session state (same path as writes: `write_state` with a "seeded" flag so it is not treated as created). A later GET by that id is then served by 1.1 (if it validates against the detail schema) or used as the generation target (detail schema bigger than the list item: generate and overlay, as today for targets). Off with `consistency: off`.

### 1.4 Server-side unique ids
Identity fields in generated lists are de-duplicated: when two items share an id, the later one gets a fresh value matching the field's schema (faker). Creates already get ids from `force_identity`; also ensure a create never reuses an id already in the session.

### 1.5 Unknown ids → 404 (opt-in)
`unknown_ids: 404` on a mock: a read with a path identity that is not in session state (created or seeded) returns the operation's declared 404 response (faker-filled from its schema, or problem+json if none is declared). Default `generate`.

### 1.6 Faker output
In `faker.rs`:
- Counts: for integer fields with no explicit tight bounds, draw from `0..=100` (names ending in `count`, `total`, `size`, `length`, `page`… and any integer with `minimum <= 0` and a huge range). GraphQL `Int` keeps its i32 bounds in the schema; the faker ignores bounds wider than ±2^31 when picking a range and prefers the small non-negative window.
- Lists: never return `null` for a nullable array; at the depth/node limit return `[]`.
- Echo: fields whose name matches a path parameter, query parameter or GraphQL argument (`login`, `id`, `owner`, `name`, `slug`…) take that value. Passed in through a new `FakerHints { echoes: Map<String, Value> }` built from the call.
- `pattern`: generate matching strings with the `rand_regex` crate (bounded repetition, max 64 chars per unbounded `*`/`+`); fall back to today's literal handling when the regex is unsupported.
- `format`: already broad; add `ipv4`, `ipv6`, `hostname`, `uri-reference`.

### 1.7 Default `maxLength`
- Prompt: the compact schema shown to the model carries no change; the system prompt adds "keep free-text strings under 200 characters unless the schema allows more".
- Guard: after validation, a string with no declared `maxLength` longer than 2 000 characters is truncated to 200 (at a char boundary) and the answer is marked `Repaired`. Faker default cap for unconstrained strings: 64.

### 1.8 Patch more errors locally
Extend `SchemaValidator::patch_nulls` into `patch` that also handles `pattern`, `maxLength`, `minLength`, `enum`, `format` and `const` errors by regenerating just that value with the faker (now pattern-aware). Same rule as today: patch only when every error is patchable; otherwise ask for a repair. This removes the TMDB `file_path` repairs.

### 1.9 Scenarios without headers
- Mock-level `default_scenario: <name>` applies when no `x-mockingbird-scenario` header is sent.
- A scenario's `operations` entries may carry per-operation `instructions` that replace the scenario-wide instructions for that operation.
Config, store, Admin API (`scenarios`, mock update) and console client regenerated.

### Tests
Engine: cache hit/miss/invalidation, seeding then detail served from state, duplicate-id fix, 404 opt-in, faker counts/lists/echo/pattern, truncation guard, patching pattern errors. Protocols: REST GET after POST served with `Source::State`; default scenario applied without header.

---

## WS2: Protocol fixes

Files: `crates/mb-protocols/src/graphql/{select,resolve,batch}.rs`, `crates/mb-protocols/src/chaos.rs`, `crates/mb-protocols/src/http_rest/handler.rs`.

### 2.1 GraphQL aliases with different arguments
`Selection::of` keys children by **response key** (alias or name) and records the field name. `narrow` emits one property per response key (schema from the field name, description noting its arguments). `resolve.rs` `nested`/`project` read the value by response key. The prompt therefore asks for `{"small": …, "large": …}` separately. Aliases with identical arguments still share (dedupe by `(name, args)`, then copy).

### 2.2 Chaos errors in the contract's error shape
`chaos.rs` `error_status` falls back to `StatusSelector::Default` when no 5xx/429 is declared; the response body is faker-filled from that schema with the message-like field set (reuse the validation-error filler from `handler.rs`). problem+json only when the operation declares no error response at all.

### Tests
GraphQL: two aliases of `repositories(first: 1)` / `(first: 5)` produce different values; identical-argument aliases share one. Chaos: Stripe-like operation with only `default` returns the `default` shape.

---

## WS3: Import and drafts

Files: `crates/mb-import/src/smart/*`, `crates/mb-import/src/lib.rs`, `crates/mb-admin/src/{drafts,contracts,imports/*}.rs`, console client (`npm run gen:api`), CLI `import` in `crates/mockingbird/src/client.rs` (only the import command).

### 3.1 Draft quality
- `required`: the extraction prompt asks for `required` on every object; `assemble.rs` adds it when missing: properties shown in every example are required; when there is no example, properties the docs mark required; otherwise none.
- Nullable: a property is nullable when any example shows `null`, or the docs text calls it optional/nullable/"may be null". Applies to LLM-written schemas too (`assemble.rs` post-pass), not only inferred ones.
- operationId: keep the docs' id; ids that fail `valid_id` are converted (hyphens/dots → `_`, camel-case preserved) rather than replaced; the original goes in `x-original-operation-id`.
- `default`: drop any `default` not present verbatim in the source text.
- Low confidence: operations below 0.4 confidence are dropped from the draft and listed in the draft's warnings with their reason.

### 3.2 Embedded OpenAPI blocks
`smart/text.rs`/`fetch.rs` look for OpenAPI/Swagger JSON or YAML in fenced code blocks, `<script type="application/json">` blobs and ReadMe's `oas` data. When found, the operation is taken from the block (formal import path) and the LLM is not asked to re-derive it. Mixed pages: block operations win; the LLM only fills operations without a block.

### 3.3 Merging, inputs, approve
- `CreateImport` accepts `urls: [..]` and `mode: merge | replace` (default `merge` when a draft or active contract exists). Merge adds new operations and replaces operations with the same method+path; nothing is removed. A docs folder (`.md`, `.html`, `.txt`) uploads as docs text, one source per file.
- `approve` takes `activate` (default `true`): approve creates the version and activates it. CLI `--no-activate` keeps today's behaviour.

### 3.4 Huge-schema warnings
`compile` returns `(Bundle, Vec<Warning>)`. Warning when an operation's compiled response schema (or the prompt-compacted form) exceeds `PROMPT_SCHEMA_BUDGET`: names the operation, size, and that the prompt will be trimmed. Warnings stored on the contract version, returned by upload/activate, the version list, and the CLI `import`/`validate` output.

### Tests
Import fixtures: a docs page with examples showing `null` and full objects (required + nullable), hyphenated ids, invented defaults, a ReadMe page with embedded OAS, two imports merging, URL list. Admin API: approve activates; warnings on a big schema.

---

## WS4: Routing, proxy and CA

Files: `crates/mockingbird/src/{registry,tls,watch,ops,file_config}.rs`, new `crates/mockingbird/src/proxy/*`, `crates/mb-admin/src/mocks.rs` (prefix check), `crates/mockingbird/src/reconcile.rs` (prefix check).

### 4.1 Hosts and shared prefixes
Mocks get optional `hosts: [api.github.com]`. Dispatch: filter mocks whose `hosts` contains the request `Host` (or mocks with no `hosts`), then longest `route_prefix`. Two mocks may share a prefix if their host sets differ; a mock may use prefix `/`. So GitHub REST at `/` and GraphQL at `/graphql` on `api.github.com` coexist. Duplicate checks (`registry.rs`, `mocks.rs` `ensure_prefix_free`, `reconcile.rs`) compare `(host set, prefix)`.

### 4.2 `mockingbird proxy`
- Command `mockingbird proxy [--port 8888]`, also started by `serve` when `proxy.enabled: true`.
- HTTP proxy: plain requests and `CONNECT`. A CONNECT to a host claimed by a mock is intercepted: TLS terminated with a leaf certificate for that host signed by the local CA, then dispatched like any request (host-matched). Other hosts: `proxy.unmapped: pass` tunnels bytes to the real host; `block` returns 502 problem+json naming the host. Every unmapped host is logged once.
- Local CA: generated with `rcgen` on first use at `~/.mockingbird/ca/{ca.pem,ca.key}` (key `0600`), leaf certs cached in memory. `mockingbird ca path` prints the cert path; `mockingbird proxy env` prints the variables to export: `HTTPS_PROXY`, `HTTP_PROXY`, `NO_PROXY=localhost,127.0.0.1`, `NODE_EXTRA_CA_CERTS`, `SSL_CERT_FILE`/`REQUESTS_CA_BUNDLE` (a bundle of system roots plus the local CA, for passthrough to keep working), `NODE_USE_ENV_PROXY=1`.
- No system trust-store install (no OS settings changed); the env vars are the mechanism.

### 4.3 Placeholder images
`proxy.images: [image.tmdb.org]`: requests to those hosts return a generated PNG (solid colour from a hash of the path, size from a `w123`-style path segment or 300×450). No LLM.

### 4.4 Hot reload and `/readyz`
Config reloads log at info with a summary (`mocks added/removed/changed`) and at error on failure; a failed config reload sets a `config_reload` check in `/readyz` (degraded) until the next successful one. A mock whose new version failed but whose old version is still mounted reports `stale` rather than `failed`.

### Tests
Dispatch with hosts and shared prefixes; proxy integration test: a client with the CA trusts an intercepted HTTPS request to a mapped host, a passthrough CONNECT to a local TLS server, `block` mode 502; PNG placeholder; readyz after a bad config reload.

---

## WS5: Agent and MCP ergonomics (wave 2)

Files: `crates/mockingbird/src/{mcp,agents,daemon,telemetry,onboard}.rs`, `crates/mb-admin/src/requests.rs`, new `crates/mockingbird/src/keys/*`, `plugins/mockingbird/skills/*` (embedded), `crates/mb-ext/src/secrets.rs` (permission check).

### 5.1 Draft tools
MCP `get_draft` (summary: operations with method, path, confidence, warnings; full schema per operation on request), `edit_draft` (JSON merge patch on one operation or the whole draft), `approve_draft` (activates by default, WS3). `import_contract` takes `urls` and `mode`.

### 5.2 Proxy tools
MCP `proxy_env` (the variables to set, with the CA path) and `start_server` gaining `proxy: true`. Skills updated: the mock-api and testing skills recommend proxy mode instead of editing source.

### 5.3 Output sizes
- `validate_contract`: counts by kind and the first 20 operations, `operations_truncated: n`.
- `list_requests`: no bodies by default (method, path, status, source, fallback reason, sizes); `include_bodies: true` or `get_request` for one entry. Admin API gets `?fields=summary`.
- Logs: `telemetry.rs` disables ANSI when writing to a file; `server_logs` strips any remaining escape codes.
- `server_logs`/`stop_server`/`server_status` resolve the state dir from `server.json` found by walking up from the config path the server was started with (recorded at `start_server`), not the MCP process's working folder.

### 5.4 `agent install` key passthrough
For agents that start MCP servers without a login shell, `agent install` adds `env: {"OPENROUTER_API_KEY": "${OPENROUTER_API_KEY}"}`-style passthrough (variable **name** only, using each agent's interpolation syntax) for the variables the default profile references. Agents without interpolation get nothing (the key setup page covers them).

### 5.5 Key setup page
- `mockingbird keys setup` (and MCP `open_key_setup`) starts a one-shot local page on `127.0.0.1:<random>` with a random token in the URL, opens the browser, and returns the URL. The form posts the key directly to that local server, which writes `~/.mockingbird/secrets/<name>` (`0600`, directory `0700`) and updates the profile to reference `${file:~/.mockingbird/secrets/<name>}`. The MCP tool waits (bounded, 10 min) and returns only `{saved: name}`, never the value.
- `${file:…}` reads refuse files readable by group/others (Unix); on Windows the file is created with an owner-only ACL.
- `doctor` reports keys found in the store.

### Tests
MCP tool snapshot tests for summaries; `agent install` output per agent; key page: POST writes a 0600 file and rejects a wrong token; `${file:}` refuses 0644.

---

## WS6: Webhooks (wave 2)

Files: new `crates/mb-protocols/src/webhooks/*`, `crates/mb-protocols/src/call.rs` (post-write hook only, after WS1 merged), `crates/mb-import/src/openapi3.rs` (webhooks/callbacks), `crates/mb-admin/src/webhook.rs` (reuse signer), config.

- Mock config `webhooks: [{url, events: [operation ids or "*"], secret: <secret ref>, signing: stripe | hmac-sha256 | none, delay_ms}]`.
- After a successful write operation, an event is built (OpenAPI `webhooks`/`callbacks` payload schema when the contract defines one, generated by the engine; otherwise a generic envelope `{type: "<resource>.<created|updated|deleted>", data: {object: <response>}}`, which matches Stripe's shape) and POSTed with retries (3, backoff).
- Signing: `stripe` → `Stripe-Signature: t=<ts>,v1=<hex hmac_sha256(secret, "<ts>.<body>")>`; `hmac-sha256` → `X-Mockingbird-Signature: sha256=<hex>`.
- `mockingbird webhooks sign --secret-ref … --body file` helper prints the header for tests.
- Admin API/MCP: `list_webhook_deliveries` (status, attempts) per mock.

### Tests
Delivery to a local receiver; Stripe signature verifies with Stripe's published algorithm (test vector); retry on 500; OpenAPI 3.1 `webhooks` payload used when present.

---

## WS7: Trials and documentation (wave 3, this session)

1. Rebuild and install; restart the three trial mocks with `hosts:` and proxy enabled; revert the trial apps' source edits (`GITHUB_*_URL`, `TMDB_API_URL`, Stripe base URL) and run them with `mockingbird proxy env` exported.
2. Measure per trial: fallbacks, repairs, `Source::State`/cache hits, prompt tokens, page latency; Stripe webhook delivery with signature verified by the Stripe SDK.
3. Update `improvements.md`: move items to **Done** with PRs and the measurements table; note anything re-scoped.

## Risks

- **Proxy compatibility:** Node's built-in `fetch` honours `HTTPS_PROXY` only with `NODE_USE_ENV_PROXY=1`, which only recent Node releases support (WS4 records the minimum version it verifies); older Node versions and apps using `axios`/`got` need `global-agent` or similar. Mitigation: document per runtime; trial apps verified in WS7.
- **Cache staleness:** invalidation by resource type is coarse; acceptable for mocks, and `consistency: off` is the escape hatch.
- **Merge conflicts:** config files across WS1/WS4/WS6; small and textual.
