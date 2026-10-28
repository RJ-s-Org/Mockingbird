# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).
The `mb-ext` crate follows semantic versioning on its own from 1.0.

## [Unreleased]

### Added

- Output budget (`limits.output_budget_tokens`, default 1500): a response whose estimated answer is larger is written shorter by the LLM (unrequested optional sub-objects left out, arrays capped, deep required objects filled by the faker). GitHub `search/commits` p50 29.8 s → 9.4 s, TMDB movie detail 51 s → 6.2 s ([performance](docs/performance.md#latency-against-a-real-llm)).
- Formats a persona declares (`field = "<id>_secret_<24 random chars>"` templates, `(pi_, cus_) followed by 24 random alphanumeric chars` id prefixes) are checked and patched after generation.
- `scripts/trial-bench.py`: latency bench over the field-trial contracts against a real provider.
- A Terraform and OpenTofu provider (`integrations/terraform-provider`) for workspaces, mocks (contract from inline content, a file or a URL, re-uploaded when its sha256 changes), contract versions, scenarios, LLM profiles (secret references only), API keys, channels and event settings, with `mockingbird_mock` and `mockingbird_workspace` data sources and import on every resource; Pulumi through its Terraform bridge, with TypeScript and Python examples (`integrations/pulumi`). See [docs/iac.md](docs/iac.md).
- `X-Mockingbird-Fallback-Reason` on every `fallback` response says why the LLM answer was not used (GraphQL: `extensions.mockingbird.fallbackReasons`); the request log records it. Migration `0500_llm_diagnostics` adds `request_log.fallback_reason` and `llm_profiles.extra_body`.
- LLM profiles take `extra_body`, JSON merged into every provider request (OpenRouter `provider` routing, `reasoning` settings); `--extra-body` on `llm configure` and `init`.
- `mockingbird mcp` imports the variables it lacks from the login shell at startup (`MB_NO_SHELL_ENV=1` skips it), so keys exported in `~/.zshenv` reach a server started by a desktop agent; `doctor` reports `shell_env.imported`.
- `mockingbird init` (MCP `init_config`) takes `--port`, `--admin-port` and `--grpc-port`.
- A request that fails validation reports why: the `X-Mockingbird-Validation` response header, the declared error shape's message-like field (`message`, `detail`, … or nested, like Stripe's `error.message`), and `request.validation` in the request log.
- Reads of a resource the session holds are answered from session state without the LLM (`X-Mockingbird-Source: state`) when the stored value matches the response schema.
- Per-session response cache: a session repeating a read gets the same answer (`cache`, 500 answers per session), cleared by any write of the resource type and by a session reset. `consistency: off` on a mock disables it and list seeding.
- List→detail seeding: the items of a list answer are stored in the session, so a later read by id returns the same item.
- `unknown_ids: 404` on a mock answers reads of ids the session does not hold with the operation's 404 (problem+json when none is declared).
- `default_scenario` on a mock applies a scenario when a request names none; a scenario's `operations` entries may be `{id, instructions}` to give one operation its own instructions (config, Admin API and console client).
- Faker: strings honour `pattern` (via `rand_regex`), formats `ipv4`, `ipv6`, `hostname` and `uri-reference`, and path, query and GraphQL argument values are echoed into same-named fields.
- Migration `0610_mock_behavior` adds `mocks.behavior`.
- Smart import takes operations from OpenAPI 3.x blocks embedded in documentation (fenced code blocks, JSON scripts, ReadMe page data) as written; a page fully covered by its blocks needs no LLM.
- Imports take `urls` (up to 20) and `documents` (up to 100) for one draft from several sources, and `mode: merge | replace`; by default an import merges into the mock's newest draft or active version (adds operations, replaces those with the same method and path, removes nothing). CLI: several URLs, docs folders, `--mode`.
- Schema-size warnings (`schema_too_large`) on uploads, the version list, activation, approval, drafts and `mockingbird validate`, for response schemas over the 64 KiB prompt budget; `mb_import::compile_with_warnings`.
- Migration `0650_import_drafts` adds `contract_versions.warnings` and `import_jobs.mode`.
- Mocks take `hosts:` (config, Admin API, console client). Dispatch filters by `Host`, then takes the longest prefix, so mocks on different hosts may share a route prefix (GitHub REST at `/` and GraphQL at `/graphql` on `api.github.com`). A GraphQL mock also answers at its own mount point (`POST /graphql` for a mock at `/graphql`). Migration `0600_mock_hosts` adds `mocks.hosts`.
- `mockingbird proxy` (and `serve --proxy` / `proxy.enabled`): an HTTP proxy that intercepts HTTPS to mapped hosts with a local CA (`~/.mockingbird/ca`, `MB_CA_DIR`, `proxy.ca_dir`; never installed system-wide); `proxy.unmapped: pass | block` (default `pass`); placeholder PNGs for `proxy.images`; `mockingbird ca path` and `mockingbird proxy env`; `GET /proxy/hosts` on the ops port. See [docs/proxy.md](docs/proxy.md).
- MCP draft tools: `get_draft` (operations with confidence and warnings, one operation's definition on request), `edit_draft` (JSON merge patch on one operation or the whole draft; a patch that breaks it changes nothing) and `approve_draft` (activates by default). `import_contract` takes `urls` and `mode`.
- Request log: Admin API `GET …/mocks/{mock}/requests/{id}` and `?fields=summary` (entries without bodies, with `request_bytes` / `response_bytes`); MCP `get_request`.
- MCP `proxy_env` (the proxy variables and CA path, with `export` lines) and `start_server {proxy: true}`; `mockingbird server start --proxy`.
- `mockingbird agent install` passes the default profile's key variables (names only) through to the agent's MCP config in each agent's syntax; `--config`, `--pass-env`.
- `mockingbird keys setup` and MCP `open_key_setup`: a one-shot local page that stores an LLM key in `~/.mockingbird/secrets/<name>` (`0600`) and points the profile at it, returning only the name and profile (the URL when the user takes longer than 45 s). `doctor` reports the store under `key_store`.
- `mb-ext`: `check_secret_file` and `in_key_store` (additive).
- Webhooks: mocks POST an event after each successful write to the URLs in `mocks.<name>.webhooks` (or the Admin API), signed Stripe-style (`Stripe-Signature`) or with `X-Mockingbird-Signature`, retried 3 times (1 s, 2 s, 4 s); the payload is the contract's OpenAPI `webhooks`/`callbacks` body when defined, else a Stripe-shaped `<resource>.<created|updated|deleted>` event. Deliveries: `GET …/mocks/{mock}/webhooks/deliveries`, `mockingbird webhooks deliveries`, MCP `list_webhook_deliveries`; `mockingbird webhooks sign` prints a signature for tests. `server.api_webhook_secret_refs` limits secret references set through the API. Migration `0700_mock_webhooks` adds `mocks.webhooks`. See [docs/webhooks.md](docs/webhooks.md).

### Changed

- Requests to OpenRouter send `reasoning: {enabled: false}` unless the profile's `extra_body` sets `reasoning`; a model that rejects it is asked again without it.
- A single object answered for a top-level array schema is taken as the list's one item instead of costing a repair; a faker list standing in for a failed LLM call no longer seeds session state.
- A fallback answer to a read is cached under consistency: the identical repeat is answered at once and still reported as `fallback`.
- GraphQL queries with several root fields read state, the response cache and stored resources per field, seed and cache per field, and are recorded as one request (operations comma-separated, `request.args` keyed by response key); each field is still metered.
- MCP `list_requests` returns summaries (method, path, status, source, fallback reason, sizes) unless `include_bodies: true`; `validate_contract` returns counts by protocol and the first 20 operations and channels (`operations_truncated`, `channels_truncated`).
- Background-server logs carry no ANSI codes (colour only on a terminal; `server_logs` strips any left), and the MCP server tools find the server's state next to the config `start_server` used (or the `config` argument), not in the MCP process's folder.
- The mock-api and testing skills recommend proxy mode over editing an app's base URL.
- A `file:` secret reference into `~/.mockingbird/secrets` is refused when the file is readable by group or others; other `file:` paths (container mounts) are unchanged.
- `mockingbird mcp`'s login-shell import also fills a variable that is set but empty (an agent's `${NAME:-}` passthrough) when the shell has a value.
- `server start` drops `HTTP(S)_PROXY` variables that point at the server's own proxy from the server's environment.
- Faker integers without tight bounds (and count-like fields) are drawn from 0..=100 (id-like fields such as `id`, `owner_id`, `userId` from 1..=1 000 000), ignoring bounds wider than ±2^31; nullable arrays are never `null`; strings without a `maxLength` are at most 64 characters.
- An LLM string over 2 000 characters where the schema sets no `maxLength` is cut to 200 (`repaired`); the prompt asks for free text under 200 characters.
- `pattern`, `maxLength`, `minLength`, `enum`, `format` and `const` errors in an LLM answer are patched locally without a repair call.
- Generated lists get unique item ids, and a create never reuses an id the session already holds.
- LLM profiles default to a 60 s timeout and 16 384 output tokens (were 3 s and 2048): a large response from a remote model takes 30 s or more.
- The OpenRouter preset uses `deepseek/deepseek-v4.1-flash`.
- `mockingbird llm test` with no flags tests the default profile of `./mockingbird.yaml`.
- MCP `start_server` refuses a config whose default profile's key the server cannot see, unless `offline: true`.
- Documentation import extracts at temperature 0 in JSON-object mode.
- The MCP "is a server running?" hint appears only when the Admin API is unreachable.
- GraphQL root fields are generated from the query's selection set: the prompt, validation and fallback use only the selected fields (fragments merged, each field's arguments noted); creates and updates are still generated whole, since they become stored state. GitHub's `viewer { login … }` went from ~347k to ~1k prompt tokens.
- The prompt (and the JSON Schema response format) carries the response schema as compact JSON fitted to 64 KiB: past that, descriptions are cut to 160 characters and the definitions furthest from the root become short stubs. Validation still uses the full schema. Stripe's `payment_intent` went from ~357k to ~23k prompt tokens.
- The prompt asks for arrays of 3 to 5 items unless the request names a count.
- The Admin API request log (and MCP `list_requests`) includes `fallback_reason`.
- `POST …/contracts/{version}/approve` also activates the version unless the body is `{"activate": false}`; `mockingbird import --approve` activates unless `--no-activate`. The console still approves and activates in two steps.
- Drafts keep the documentation's operation ids (converted when invalid, the original in `x-original-operation-id`), get `required` and nullable properties from the examples and the text, drop `default` values the documentation does not state, and leave out operations below 0.4 confidence with a `low_confidence` warning.
- `/readyz` reports `config_reload: <error>` after a failed config reload until the next good one, and words a failed mock rebuild as `stale, serving the previous version` or `failed`.
- Config reloads log `config reloaded: N added, N removed, N changed`.
- The root fields of a GraphQL query are generated in one LLM call, as one object keyed by response key, so they agree with each other (GitHub's `user(login:)` and `organization(login:)` no longer both answer). A nullable field may be left out of that answer; one that stays invalid after the repair is answered field by field as before. Mutations, subscriptions and queries with root-level fragments or directives are unchanged.
- An LLM answer whose only errors are nulls the schema does not allow is patched instead of sent back for a repair call: an optional property is dropped, a required one gets a faker value. The response's source is `repaired`. Real APIs return null where their docs never say so (TMDB's `belongs_to_collection`), and models copy them; this saved a 20–40 s repair call on 4 of 8 TMDB detail pages.
- An answer wrapped like a GraphQL response (`{"data": {"user": {...}}}`) is unwrapped like an array envelope.

### Fixed

- Provider replies with the answer only in `reasoning`, errors inside a 200, or output cut at `max_tokens` are handled (with one retry for transient failures) instead of falling back without a reason.
- A JSON array the model wrapped in a one-key object (JSON-object mode) is unwrapped when the inner value matches the schema.
- Real-world contracts load: a `$ref` inside a vendor extension (Stripe's `x-expansionResources`, GitHub's `x-github-breaking-changes`) no longer fails compilation, and an unresolvable `$ref` names its referrer and the JSON pointer where it sits.
- GraphQL schemas with covariant interface fields serve (GitHub's `User.email: String!` implementing `ProfileOwner.email: String`): implementations are checked with the spec's rules instead of async-graphql's inverted check.
- OpenAPI 3 `application/x-www-form-urlencoded` and `multipart/form-data` request bodies are compiled, decoded and validated; the engine no longer sees `"body": null` for them, nor for any non-empty body the contract does not describe. Bracket notation (`a[b][c]=1`, `a[0]=x`, `a[]=y`) nests in form bodies and query strings, so stripe-node requests validate against Stripe's schema. The request log shows the body as decoded. Swagger 2 `type: file` parameters are binary strings.
- GraphQL aliases of one field with different arguments (`small: repositories(first: 1)`, `large: repositories(first: 5)`) get separate generated values; aliases with identical arguments share one.
- Chaos errors on an operation that declares only a `default` error response (Stripe) answer HTTP 503 in that shape, with its message-like field set, instead of problem+json.

## [0.1.0-rc.1] - 2026-10-20

### Added

- Agent toolkit: every capability is reachable from coding agents (see [docs/agents.md](docs/agents.md)).
  - `mockingbird mcp`: an MCP server (stdio, or streamable HTTP with bearer auth, origin checks, `--read-only` and host tools hidden unless `--allow-local-tools`) with 33 tools covering setup, LLM onboarding, the background server and the Admin API, plus the skills as resources and prompts.
  - Onboarding commands that print JSON: `mockingbird init`, `mockingbird llm providers|configure|test` with presets for OpenRouter, Google AI Studio, Cloudflare AI Gateway and Workers AI, OpenAI, Anthropic, DeepSeek, Groq, Mistral, Together, Azure OpenAI, Ollama, LM Studio and any OpenAI-compatible endpoint; `mockingbird server start|stop|status|logs`; `mockingbird doctor`.
  - `mockingbird agent install <agent>` for Claude Code, Codex, Cursor, Gemini CLI, Kiro, VS Code, Windsurf and Claude Desktop, and `agent print|skills`.
  - Five agent skills, the Claude Code / Codex / Cursor plugin and marketplaces, a Gemini CLI extension, a Kiro power, and `install.sh` / `install.ps1`.
- LLM profiles take extra provider request headers (`llm.profiles.<name>.headers`, `headers` in the Admin API) whose values may embed `${env:NAME}` secret references, for Cloudflare AI Gateway's `cf-aig-authorization`, Azure OpenAI's `api-key` and similar. Migration `0400_profile_headers` adds the column.
- P1 Core loop: the Cargo workspace, the Mockingbird Contract Model (`mb-mcm`), the OpenAPI 3 and Swagger 2 compilers, the OpenAI-compatible `mb-llm` provider, the `mb-engine` generation pipeline (prompt, validate, repair, faker fallback), the REST adapter, in-memory session state and the `mockingbird serve <spec>` CLI.
- P2 Platform: SQLite and Postgres storage with migrations, the control-plane entities, the Admin API, scenarios and override headers, config-as-code (`mockingbird.yaml`) with `--watch`, OpenTelemetry tracing, Prometheus metrics and the Docker image.
- P3 Console and auth: the embedded React/TanStack console, OIDC login (Rauthy), RBAC, admin and data-plane API keys, the audit log, request log streaming and the Compose stack.
- P4 Protocols: GraphQL (including subscriptions), gRPC (all four call kinds, with reflection), SOAP 1.1/1.2 from WSDL, server-sent events and WebSocket.
- P5 Events: the AsyncAPI 2.x/3.0 compiler, Kafka, AMQP and MQTT producers and consumers, WebSocket channels, schedule, cron, state-change and manual triggers, and the channel Admin API and CLI.
- P6 Smart import: draft contracts from documentation (Markdown, HTML, PDF, plain text or a URL, extracted by an LLM within a per-job token budget) and, deterministically, from HAR recordings and Postman collections; SSRF-guarded URL fetching; the import job and draft Admin API (review, edit, preview, approve) with quota admission and usage reporting; the console's Import tab and draft review page; and `mockingbird import <FILE|URL>`.
- P7 Enterprise hardening and readiness:
  - **High availability:** `--role all|control|data` (`server.role`, `MB_ROLE`); a cross-node change bus over Postgres `LISTEN/NOTIFY` or Redis pub/sub (`bus.url`, `MB_BUS_URL`) carrying ids only, with `Resync` on reconnect; leader election (Postgres advisory lock or Redis lease) gating reconcile, `--watch` and maintenance; `/readyz` reports `role` and `leader`.
  - **Redis session state** (`storage.state_url: redis://…`, `rediss://`).
  - **Limits:** a global generation semaphore, request and response size caps (`limits:`), and per-mock rate limits and daily token budgets shared across replicas (migration `0300_hardening`: `mocks.limits`, `token_usage`, whose rows the leader prunes after 90 days).
  - **Chaos:** per-mock and per-scenario error injection in the contract's error shape, latency jitter and dropped streams (`mocks.chaos`), bypassed by `X-Mockingbird-Chaos: off`.
  - **PII scrubber** for generated output (`pii:`), with six builtin patterns and custom ones.
  - **TLS termination** with certificate reload (`server.tls`), outbound CA bundle and proxy settings (`outbound:`), and `server.trust_forwarded_headers`.
  - **Protocol caps:** GraphQL root-field, subscription and connection limits, and a `connection_init` timeout that frees the slot of a WebSocket that never initialises (`protocols.graphql.connection_init_timeout`); queries and mutations over the GraphQL WebSocket transport; per-item metering of client and bidi gRPC streams; logged rejections; `?wsdl` addresses behind a proxy.
  - **Observability:** per-stage trace spans (`mb.decode` … `mb.encode`) and the metrics `mb_generation_in_flight`, `mb_generation_queue_timeouts_total`, `mb_token_budget_*`, `mb_rate_limited_total`, `mb_pii_replacements_total`, `mb_chaos_injected_total`, `mb_bus_*`, `mb_leader` and `mb_state_sessions`.
  - **Delivery:** the Helm chart (all-in-one or split, HPA, PDB, NetworkPolicy, ServiceMonitor), Compose profiles `ha`, `kafka`, `amqp` and `mqtt`, and a release pipeline with multi-arch images (full and slim) signed with cosign, CycloneDX SBOM, SLSA provenance and binaries for five targets.
  - **`mb-ext` 1.0:** a frozen, documented API with constructors, builders and `#[non_exhaustive]` core types, `vault:` secret references, a worked extension example (`examples/custom-ext`) and a semver gate.
  - **Console and CLI:** the Channels tab, the chaos and limits editor, and `--workspace`, `scenarios` and `profiles` in the CLI.
  - **Quality:** nightly LLM evals and an offline throughput bench (`mb-evals`), image scanning, a cross-node HA test and a Compose end-to-end flow in CI.
  - **Documents:** community files, the example gallery, and the operator docs in `docs/` (architecture, configuration reference, deployment guides, runbook, upgrade notes, performance).

### Changed

- The repository moved to `RJ-s-Org/Mockingbird`: images are `ghcr.io/rj-s-org/mockingbird`, the chart is `oci://ghcr.io/rj-s-org/charts/mockingbird`, and the installers download from the new repository.
- Console: a refined look in light and dark mode. Colours come from semantic tokens defined once per theme; pages share one set of buttons, fields, cards, notices and badges; the header carries the product mark (and a favicon) and stays compact on phones; forms are grouped into titled cards, with Delete mock in its own card; HTTP methods, status codes, response sources and statuses (keys, contract versions, readiness, draft confidence) are shown as colour-coded badges.

[Unreleased]: https://github.com/rj-s-org/mockingbird/compare/v0.1.0-rc.1...HEAD
[0.1.0-rc.1]: https://github.com/rj-s-org/mockingbird/releases/tag/v0.1.0-rc.1
