# Mockingbird Platform Design

- **Date:** 2026-10-14
- **Status:** Approved design, pending spec review
- **Scope:** Core open-source repository (`mockingbird`). The managed-service repository (`mockingbird-cloud`) is out of scope except for the extension contract it builds against.

## 1. Purpose

Mockingbird is a lightweight, Rust-native mock endpoint platform. It reads API contracts (specifications, documentation, recorded traffic) and serves mock endpoints whose responses are synthetic data generated per request by a fast, inexpensive LLM (for example DeepSeek 4.1 Flash). Teams use it to build integrations against APIs that are unavailable, unstable, or costly in sandboxes and lower environments.

### 1.1 Users and deployment models

All three are first-class:

| Model | Description |
|---|---|
| Local | A developer runs a single binary or container against a spec file. |
| Private cloud / bare metal | A team self-hosts a shared instance (Compose or Helm) in any cloud or on-premises. |
| Managed | The maintainers operate Mockingbird as a hosted service. Built from the core plus the private `mockingbird-cloud` repository. |

### 1.2 Success criteria

1. `mockingbird serve openapi.yaml` with only `MB_LLM_API_KEY` set serves a working mock in under five minutes from download.
2. Every response a consumer receives validates against the contract's response schema.
3. Generated responses reach first byte in ≤ 1 s p50 with a fast-tier model. When the LLM is slow or unavailable, the mock stays up via the deterministic fallback.
4. Writes are reflected in subsequent reads within the same session (stateful behaviour).
5. REST, GraphQL, gRPC, SOAP, SSE, WebSocket, Kafka, AMQP, and MQTT are supported through one engine.
6. The same container image runs in all three deployment models.
7. Self-hosting enterprises get SSO, RBAC, audit, HA, and signed artifacts without a commercial licence.

### 1.3 Non-goals

- Business-logic emulation beyond contract shape and session state.
- Load testing at production scale. Mockingbird targets lower-environment traffic.
- Proxy/record-and-replay against live upstreams as a serving mode. Recorded traffic is an *import* source only.
- Hard multi-tenancy, billing, signup, and operator-funded LLM spend controls. These belong to `mockingbird-cloud`.

## 2. Key decisions

| Decision | Choice |
|---|---|
| Generation mode | Fresh LLM generation on every request, plus session-scoped state so writes affect later reads. No response replay cache. |
| Contract inputs | Structured specs compile deterministically to MCM. Docs, HAR, and Postman importers use the LLM to produce a draft that requires approval. |
| Protocols | Request/response, streaming, and event-driven (brokers) |
| Management surface | Config-as-code + CLI + Admin REST API + embedded web console, with RBAC/SSO |
| Identity | Rauthy as the default OIDC provider. Any standards-compliant OIDC issuer works. |
| Architecture | Modular monolith: one Cargo workspace, one binary, `--role all\|control\|data` |
| Repository split | Open core (`mockingbird`) plus private `mockingbird-cloud`, joined by compile-time extension traits |
| License (core) | `MIT OR Apache-2.0` |
| Console frontend | React + TanStack (Router, Query, Table) + Vite, TypeScript, embedded in the binary |

## 3. Repository strategy

### 3.1 Boundary rule

A feature belongs in the core if a self-hosting organisation needs it, including enterprise features such as SSO, RBAC, audit, and HA. A feature belongs in `mockingbird-cloud` only if it exists because the maintainers operate the service for others.

| `mockingbird` (public, MIT OR Apache-2.0) | `mockingbird-cloud` (private, proprietary) |
|---|---|
| MCM, importers, engine, LLM providers, state, all protocol adapters | Hard tenant isolation and tenant provisioning |
| Admin API, console, CLI | Signup, billing, usage metering |
| OIDC/Rauthy auth, RBAC, data-plane API keys, audit log | Quotas, abuse controls, operator-funded LLM keys with spend caps |
| Workspaces (soft isolation within one install) | Per-tenant secret vault integration, fleet operations |
| OTel, Prometheus, Docker image, Compose, Helm | Infrastructure-as-code (Terraform/Argo), SLA dashboards |

### 3.2 Extension contract (`mb-ext`)

`mockingbird-cloud` depends on core crates and builds its own binary, supplying its own implementations of the traits below. There is no dynamic plugin loading. Each trait ships a default implementation used by the open-source binary.

| Trait | Responsibility | Core default |
|---|---|---|
| `TenantResolver` | Map an inbound request or principal to a tenant and workspace | Single tenant; workspace from route/config |
| `QuotaGuard` | Admit or reject a call before generation | Allow all (per-mock rate limits still apply) |
| `UsageSink` | Record tokens, calls, and latency per tenant/mock | Prometheus counters |
| `SecretStore` | Resolve secret references | Env, file, Vault, AWS/GCP/Azure KMS |
| `ProviderPolicy` | Choose the LLM profile/key permitted for a call | Use the mock's configured profile |
| `AdminHooks` | Register extra admin routes and console pages | None |

`mb-ext` follows semantic versioning. Reaching 1.0 is part of Phase 7.

## 4. Architecture

### 4.1 Workspace layout

```
mockingbird/
├─ crates/
│  ├─ mb-mcm/        Mockingbird Contract Model: types, versioning, per-operation JSON Schema
│  ├─ mb-import/     Deterministic compilers: openapi3, swagger2, graphql, proto, asyncapi, wsdl
│  │                 LLM-assisted draft importers: docs (md/html/pdf/url), har, postman
│  ├─ mb-llm/        Provider trait, OpenAI-compatible client, streaming, structured output
│  ├─ mb-engine/     Prompt builder, generation, validate → repair → fallback, schema faker
│  ├─ mb-state/      Session-scoped resource store and event bus
│  ├─ mb-protocols/  ProtocolAdapter implementations behind Cargo features:
│  │                 http-rest, graphql, grpc, soap, sse, websocket, kafka, amqp, mqtt
│  ├─ mb-ext/        Extension traits and defaults (§3.2)
│  ├─ mb-auth/       OIDC relying party, RBAC, data-plane API keys
│  ├─ mb-admin/      Admin REST API (OpenAPI-described), audit log
│  ├─ mb-console/    Embedded console assets (rust-embed) + console/ TypeScript source
│  └─ mockingbird/   Binary: CLI, config loading, role wiring
├─ deploy/           Dockerfile, compose/, helm/
└─ docs/
```

### 4.2 Unit boundaries

- `mb-mcm` has no internal dependencies. All other crates exchange MCM types.
- Protocol adapters contain no generation logic. Each implements `ProtocolAdapter`: bind listeners, decode traffic into `OperationCall`, encode `GeneratedResponse` or stream items back onto the wire. Adding a protocol adds an adapter only.
- `mb-engine` has no protocol or transport knowledge. Its interface is `generate(OperationCall, Ctx) -> GeneratedResponse | Stream<Item>`, and it is testable with a scripted provider.
- `mb-import` runs only in the control plane. Data-plane processes load compiled MCM bundles and never parse source specs on the request path.
- `mb-ext` is the only API surface `mockingbird-cloud` relies on.

### 4.3 Roles

| Role | Runs |
|---|---|
| `all` (default) | Everything in one process |
| `control` | Admin API, console, importers, contract compilation, schedulers (leader-elected) |
| `data` | Protocol adapters, engine, state access; pulls active MCM bundles from the control-plane database and reloads on change notifications |

### 4.4 Technology choices

| Concern | Choice |
|---|---|
| Runtime / HTTP / SSE / WebSocket | `tokio`, `axum` |
| gRPC | `tonic` + `prost-reflect` (dynamic descriptors, no codegen) |
| GraphQL | `async-graphql` dynamic schema |
| SOAP/WSDL | `quick-xml` with an in-repo WSDL/XSD → MCM compiler |
| Brokers | `rdkafka`, `lapin`, `rumqttc` (feature-gated) |
| Persistence | `sqlx` (SQLite, Postgres); `redis` for state/bus in HA |
| Validation | `jsonschema` |
| Observability | `tracing`, OpenTelemetry OTLP, Prometheus exporter |
| Console | React, TanStack Router/Query/Table, Vite, TypeScript, client generated from the Admin API OpenAPI document |

## 5. Mockingbird Contract Model (MCM)

MCM is the single internal representation that every importer produces and the runtime consumes.

- **Bundle:** `{ id, version, source_digest, protocol_bindings[], operations[], channels[], schemas{} }`
- **Operation:** `{ id, protocol, binding (method+path | graphql field | grpc service/method | soap action), kind (query | command | stream-server | stream-client | stream-bidi), input_schema, output_schemas{status/variant → schema}, error_schemas{}, identity_hint, resource_type, semantic_hints[] }`
- **Channel** (event-driven): `{ id, broker binding (topic/exchange/routing key/mqtt topic), direction (publish | subscribe), message_schema, triggers[] }`

All schemas are normalised to JSON Schema 2020-12. Protobuf and XSD types map to JSON Schema with protocol annotations, so the engine validates uniformly and adapters re-encode to the wire format.

`identity_hint` names the field that identifies a resource. It is taken from the contract where one is explicit (path parameter, `id` field, gRPC request field), detected heuristically otherwise (`id`, `<resource>Id`), and can be overridden in the console.

`kind` is derived from the contract: HTTP method, GraphQL query/mutation/subscription, gRPC streaming flags, SOAP operation naming. Where it is ambiguous (for example a gRPC unary call named `Create*`), naming heuristics set the default and an override is available.

## 6. Contract import

### 6.1 Deterministic compilers

OpenAPI 3.0/3.1, Swagger 2.0, GraphQL SDL and introspection JSON, `.proto` files and `FileDescriptorSet`, AsyncAPI 2.x/3.x, WSDL 1.1/2.0. They produce MCM directly. A compile failure is reported with source locations and never replaces the active contract version.

### 6.2 LLM-assisted importers

Inputs: Markdown, HTML, PDF, URLs (fetched through an SSRF allowlist), HAR files, Postman collections.

1. Extract text/examples and chunk them.
2. The LLM proposes operations and JSON Schemas as structured output.
3. Merge and deduplicate, then validate that the result is a well-formed MCM.
4. Persist as a `draft` ContractVersion.
5. A user reviews and edits the draft in the console (operation list, schema editor, sample generation preview) and approves it. Only `approved` versions can be activated.

## 7. Request flow

1. **Decode.** The adapter matches the inbound call to an MCM operation and validates input against `input_schema`. Invalid input returns the contract's declared 4xx/error shape, built deterministically without an LLM call.
2. **Admission.** `TenantResolver` → `QuotaGuard` → per-mock rate limit → data-plane auth check.
3. **State lookup.** Resolve the session (`X-Mockingbird-Session` header, cookie, or gRPC metadata; default session per mock otherwise). Load related resources: the target resource for reads/updates/deletes, and resources referenced by ID fields in the request.
4. **Prompt build.** A static system preamble per operation, ordered first so provider prompt caching applies, followed by the operation summary, response schema, semantic hints, persona (locale, realism, style), active scenario instructions, the request (as delimited, quoted data), and related state. Target size is 1–3K tokens.
5. **Generate.** Call the LLM using JSON Schema structured output where the provider supports it, JSON mode otherwise, streamed. The default timeout is 3 s, configurable per mock.
6. **Validate → repair → fallback.** Validate against the selected output schema. On failure, make one repair call that includes the validation errors. On a second failure or timeout, the schema faker produces a valid response. The response is tagged with `X-Mockingbird-Source: llm | repaired | fallback` (or the protocol equivalent: gRPC metadata, GraphQL `extensions`).
7. **State write.** For commands: upsert the resulting resource keyed by `identity_hint` (create/update), or remove it (delete). Publish `StateChanged` on the bus.
8. **Encode.** The adapter serialises to JSON, protobuf, a GraphQL envelope, or a SOAP envelope.

### 7.1 Overrides

Overrides can be set by request headers (or gRPC metadata) or by console rules.

| Override | Effect |
|---|---|
| `X-Mockingbird-Status: <code>` | Return the contract's response/error variant for that status |
| `X-Mockingbird-Latency: <duration>` | Add delay before responding |
| `X-Mockingbird-Scenario: <name>` | Apply a saved scenario (instructions and/or rules) |
| `X-Mockingbird-Session: <id>` | Select the state session |
| Per-mock chaos settings | Configured error rate, latency jitter, and dropped streams |

Header overrides can be disabled per mock.

### 7.2 Streaming

For server, client, and bidirectional gRPC streams, GraphQL subscriptions, SSE, and WebSocket channels, the LLM produces newline-delimited JSON items. Each item is validated as it completes and emitted immediately. Invalid items are repaired or replaced by faker output. Subscriptions and SSE streams also emit when a related `StateChanged` event occurs.

### 7.3 Event-driven channels

Producers publish generated messages to Kafka topics, AMQP exchanges, or MQTT topics on triggers:

- `schedule` (cron or rate)
- `on_state_change` (for example `POST /orders` → `order.created`)
- `manual` (Admin API or console)

Consumers subscribe to declared channels, record received messages for inspection, validate them against the schema, and can fire reply messages or state changes. AsyncAPI WebSocket channels use the same mechanism over the WebSocket adapter.

## 8. Data model and state

### 8.1 Control-plane entities

| Entity | Key fields |
|---|---|
| Workspace | id, name, members/roles |
| Mock | id, workspace, name, bindings (route prefix, ports, broker connections), persona, llm_profile, chaos, data_plane_auth, header_overrides_enabled, source (`file` \| `api`) |
| ContractVersion | id, mock, source blobs, compiled MCM bundle, status (`draft` \| `approved` \| `active` \| `archived`), created_by |
| Scenario | id, mock, name, instructions, rules (status/latency/field constraints) |
| LLMProfile | id, provider, base_url, model, secret_ref, temperature, timeout, max_tokens, fallback_profile |
| ApiKey | id, scope (admin or mock), hashed secret, expiry |
| AuditEvent | actor, action, target, before/after, timestamp |
| RequestLog | mock, operation, session, status, source, latency, tokens, redacted request/response; ring buffer with configurable retention |

Only one ContractVersion per mock is `active`. Activating another version is atomic and acts as rollback.

### 8.2 Runtime state

- Documents keyed by `(workspace, mock, session, resource_type, resource_id)`, stored as JSON.
- Session TTL defaults to 24 h. Sessions can be listed, inspected, reset, exported as fixtures, and seeded from fixtures.
- Event bus: in-process broadcast (single node), Redis pub/sub, or Postgres `LISTEN/NOTIFY` (multi-node).

### 8.3 Storage backends

| Profile | Control plane | Runtime state | Bus |
|---|---|---|---|
| Local | SQLite (file or memory) | SQLite or memory | In-process |
| Private / HA | Postgres | Postgres or Redis | Redis or Postgres NOTIFY |

Migrations use `sqlx`, run at startup under an advisory lock, and are maintained for both SQLite and Postgres.

## 9. Configuration and CLI

The precedence order is `mockingbird.yaml` < `MB_*` environment variables < CLI flags.

`mockingbird.yaml` covers server/ports, storage, auth, LLM profiles, observability, and an optional `mocks:` block declaring mocks as code. File-declared mocks are reconciled at startup and on change (`--watch`), and are read-only in the console.

```
mockingbird serve [-c mockingbird.yaml] [--role all|control|data] [--watch] [contract…]
mockingbird import <file|url> --as <mock> [--approve]
mockingbird mocks | sessions | scenarios | profiles <subcommand>
mockingbird export-fixtures --mock <m> --session <s> > fixtures.json
mockingbird validate <contract>
```

Zero-config mode: `mockingbird serve spec.yaml` uses SQLite in memory, auth off, binds `127.0.0.1`, and reads the LLM from `MB_LLM_BASE_URL` / `MB_LLM_MODEL` / `MB_LLM_API_KEY`. The defaults are DeepSeek's OpenAI-compatible endpoint and its fast model.

Secrets are stored only as `SecretStore` references. Raw secrets are never written to the database.

## 10. LLM layer

- `Provider` trait: `complete(Request) -> Response`, `stream(Request) -> Stream<Chunk>`, plus capability flags (`json_schema`, `json_mode`, `prompt_cache`).
- Built-in provider: OpenAI-compatible Chat Completions. It covers DeepSeek, OpenAI, Groq, Together, OpenRouter, vLLM, Ollama, and LM Studio. Additional native providers can be added behind the trait.
- Per-mock `LLMProfile`, with a fallback profile used on provider errors before the faker fallback.
- Token usage, latency, and outcome are recorded through `UsageSink` and OTel.

## 11. Auth and security

### 11.1 Admin plane (console, Admin API, CLI)

- OIDC Authorization Code + PKCE for the console, with a server-side session in a cookie (`HttpOnly`, `Secure`, `SameSite=Lax`).
- Bearer JWTs validated against the issuer's JWKS for CLI/CI (Rauthy client credentials), plus Mockingbird admin API keys.
- RBAC roles per workspace: `owner`, `admin`, `editor`, `viewer`, plus global `sysadmin`. Roles are mapped from a configurable claim (Rauthy groups or roles).
- Rauthy is the default and is shipped in Compose and as an optional Helm subchart. Any OIDC issuer is supported via discovery.
- Auth is disabled by default only in local mode, and only when bound to loopback. Binding to a non-loopback address without auth requires `--insecure-no-auth`.

### 11.2 Data plane (mock endpoints)

Configured per mock:

- `none`
- `api-key` (header or query parameter)
- `passthrough-validate`: requires the caller to present credentials in the form the contract declares (bearer, basic, API key, OAuth scopes), without verifying them against a real issuer

Admin and data planes listen on separate ports by default (`9090`, `8080`).

### 11.3 Controls

- Audit log for all admin mutations, exportable as JSONL or by webhook.
- Prompt-injection containment: request content enters prompts only as delimited data, the LLM has no tool access, and all output is schema-validated before release.
- Synthetic-only data: the persona preamble forbids real personal data. An optional output PII scrubber applies configurable patterns.
- SSRF allowlist for URL imports and for any outbound fetch.
- Per-mock rate limits, token budgets, request/response size caps, and field redaction in request logs.
- TLS via rustls or an upstream ingress. Custom CA bundles and outbound proxy support cover private LLM endpoints.

## 12. Deployment and distribution

- Release binaries: Linux amd64/arm64 (musl, static), macOS arm64/x86_64, Windows x86_64.
- OCI images (multi-arch, distroless):
  - `mockingbird:<v>`: all features
  - `mockingbird:<v>-slim`: HTTP/GraphQL/gRPC/SOAP/SSE/WebSocket, no broker clients
- Supply chain: cosign signatures, CycloneDX SBOM, SLSA provenance, Trivy scan in CI.
- `deploy/compose/`: Mockingbird, Rauthy, Postgres, optional Redis, Kafka/RabbitMQ/Mosquitto profiles for local event testing.
- `deploy/helm/`: Deployments per role, HPA for `data`, ingress (HTTP and gRPC), PDB, NetworkPolicy, ServiceMonitor, external Postgres/Redis values, optional Rauthy subchart.
- Scheduler leader election via a Postgres advisory lock (SQLite deployments are single-node).

## 13. Error handling and health

- Consumer-facing errors always use the contract's declared error shape: REST error bodies, GraphQL `errors[]`, gRPC `Status`, SOAP Fault.
- When the platform degrades:
  - LLM failure → fallback profile → faker
  - State store unavailable → stateless generation with `/readyz` reporting degraded
  - Broker unavailable → producer retries with backoff, surfaced in the console
- Admin API errors use RFC 9457 `application/problem+json`.
- `/healthz` (liveness), `/readyz` (database reachable, bundles loaded), `/metrics` (Prometheus).
- Graceful shutdown: stop accepting, drain in-flight requests and streams, flush producers, close broker connections.

## 14. Observability

- One trace per call with spans for decode, admission, state, prompt, llm, validate, repair, and encode, exported via OTLP.
- Metrics: requests, latency histograms, source ratio (`llm`/`repaired`/`fallback`), tokens, broker messages, state sizes.
- Live request log streamed to the console (SSE), filterable by mock, operation, session, and source.

## 15. Testing strategy

| Layer | Approach |
|---|---|
| Unit | Per crate. `mb-engine` uses a scripted `MockProvider` (deterministic, offline). |
| Importer golden tests | Corpus: Petstore, Stripe/GitHub OpenAPI subsets, GitHub GraphQL SDL, gRPC example protos, AsyncAPI Streetlights, sample WSDL. Snapshot MCM via `insta`. |
| Property tests | `proptest`: faker output validates for generated schemas, and MCM round-trips. |
| Adapter conformance | Real clients against a live server (`reqwest`, dynamic `tonic` client, GraphQL client, SOAP client). Brokers via `testcontainers` (Kafka, RabbitMQ, Mosquitto). |
| End-to-end | Compose stack (Rauthy, Postgres, Redis). Playwright runs login → import → approve → call → inspect state → reset. |
| LLM evals | Nightly, real provider, over the corpus. Tracks schema-valid rate, repair rate, p50/p95 latency, tokens per call. |
| CI gates | `cargo fmt`, `clippy -D warnings`, `cargo deny`, test matrix (SQLite/Postgres), console lint/typecheck/test, image build and scan. |

## 16. Delivery phases

The implementation plan executes these phases in order. Each phase ends merged, tested, and releasable.

| Phase | Scope |
|---|---|
| P1 Core loop | Workspace scaffold, `mb-mcm`, OpenAPI 3/Swagger 2 compiler, `mb-llm` OpenAI-compatible provider, `mb-engine` (prompt, validate/repair/fallback, faker), REST adapter, in-memory state with sessions, `serve <spec>` CLI |
| P2 Platform | SQLite/Postgres storage and migrations, control-plane entities, Admin API, scenarios and overrides, config-as-code with `--watch`, OTel/Prometheus, Docker image |
| P3 Console and auth | React/TanStack console, OIDC (Rauthy) login, RBAC, API keys, audit log, request log streaming, Compose stack |
| P4 Protocols | GraphQL (incl. subscriptions), gRPC (incl. all streaming kinds), SOAP/WSDL, SSE, WebSocket |
| P5 Events | AsyncAPI compiler, Kafka/AMQP/MQTT producers and consumers, triggers, console channel views |
| P6 Smart importers | Docs/PDF/URL, HAR, Postman draft importers, draft review and approval UI |
| P7 Enterprise hardening | Helm chart, role split and HA (Redis bus, leader election), chaos controls, PII scrubber, signed releases/SBOM/provenance, nightly LLM evals, `mb-ext` 1.0 |

## 17. Open items for later specs

- `mockingbird-cloud` design (tenancy, metering, billing, quotas, operations), built against `mb-ext` 1.0.
- Native (non-OpenAI-compatible) LLM providers, if evals show a need.
