# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).
The `mb-ext` crate follows semantic versioning on its own from 1.0.

## [Unreleased]

## [0.1.0] - Unreleased

### Added

- P1 Core loop: the Cargo workspace, the Mockingbird Contract Model (`mb-mcm`), the OpenAPI 3 and Swagger 2 compilers, the OpenAI-compatible `mb-llm` provider, the `mb-engine` generation pipeline (prompt, validate, repair, faker fallback), the REST adapter, in-memory session state and the `mockingbird serve <spec>` CLI.
- P2 Platform: SQLite and Postgres storage with migrations, the control-plane entities, the Admin API, scenarios and override headers, config-as-code (`mockingbird.yaml`) with `--watch`, OpenTelemetry tracing, Prometheus metrics and the Docker image.
- P3 Console and auth: the embedded React/TanStack console, OIDC login (Rauthy), RBAC, admin and data-plane API keys, the audit log, request log streaming and the Compose stack.
- P4 Protocols: GraphQL (including subscriptions), gRPC (all four call kinds, with reflection), SOAP 1.1/1.2 from WSDL, server-sent events and WebSocket.
- P5 Events: the AsyncAPI 2.x/3.0 compiler, Kafka, AMQP and MQTT producers and consumers, WebSocket channels, schedule, cron, state-change and manual triggers, and the channel Admin API and CLI.
- P6 Smart import: draft contracts from documentation (Markdown, HTML, PDF, plain text or a URL, extracted by an LLM within a per-job token budget) and, deterministically, from HAR recordings and Postman collections; SSRF-guarded URL fetching; the import job and draft Admin API (review, edit, preview, approve) with quota admission and usage reporting; the console's Import tab and draft review page; and `mockingbird import <FILE|URL>`.
- P7 Enterprise hardening: the `control`/`data` role split with a Postgres or Redis bus and leader election, the Redis state backend, per-mock chaos and limits, the PII scrubber, TLS termination and outbound CA/proxy settings, the Helm chart, signed releases with SBOM and provenance, nightly LLM evals, `mb-ext` 1.0, and the community files and example gallery.

[Unreleased]: https://github.com/rjs-git/mockingbird/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/rjs-git/mockingbird/releases/tag/v0.1.0
