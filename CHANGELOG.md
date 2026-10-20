# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).
The `mb-ext` crate follows semantic versioning on its own from 1.0.

## [Unreleased]

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
