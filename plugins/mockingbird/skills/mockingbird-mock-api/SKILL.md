---
name: mockingbird-mock-api
description: Create and drive Mockingbird mocks - import an OpenAPI/Swagger, GraphQL, protobuf/gRPC, WSDL/SOAP or AsyncAPI contract, or turn API docs, a HAR recording or a Postman collection into one; call the mock; add scenarios (forced errors, latency, fields), per-session state and fixtures, chaos, and event channels. Use when the user asks to mock, fake or stub an API or a specific endpoint behaviour.
---

# Mock an API

Prerequisite: a running server (`server_status`; start one with `start_server` or `mockingbird server start -c mockingbird.yaml`). Without a config file, `mockingbird server start --offline api.yaml` serves a contract directly at `/`.

## 1. Get the contract in

| You have | Do |
| --- | --- |
| OpenAPI 3.x / Swagger 2.0, GraphQL SDL or introspection JSON, `.proto` or descriptor set, WSDL 1.1, AsyncAPI 2/3 | `import_contract {source: "./api.yaml", mock: "orders"}`; CLI `mockingbird import api.yaml --as orders` |
| A directory of `.proto`/`.graphql`/WSDL+XSD files | pass the directory (CLI and local MCP); it is packed as tar.gz |
| API documentation (Markdown, HTML, PDF, text, or a URL to it), a HAR recording, a Postman collection | same call; it becomes a **draft** reviewed before it serves. Pass `approve: true, activate: true` to take it when the import reports no warnings; otherwise show the user the `warnings` |
| Only an idea of the API | write a small OpenAPI 3.1 file first, then import it |
| A contract that only exists in the conversation (remote agents, ChatGPT) | `import_contract {content: "<the file text>", filename: "api.yaml", mock: "orders"}` |

`validate_contract {path}` compiles a contract locally and lists its operation ids, without a server.

The first version is activated automatically; later imports need `activate: true` (or `activate_contract_version`). The mock is created if missing, mounted at `/<mock>` (`create_mock` / `update_mock` set `route_prefix`, `persona`, `llm_profile`, `chaos`, `limits`). In `mockingbird.yaml`, mocks are declared under `mocks.<name>.contract` instead, and are read-only through the API.

## 2. Call it

`call_mock {method: "POST", path: "/orders/orders", body: {...}, session: "alice"}` (or curl `localhost:8080/...`). The result includes status, body and the `x-mockingbird-*` headers:

- `X-Mockingbird-Source`: `llm`, `repaired` (LLM output fixed to fit the schema) or `fallback` (faker).
- `X-Mockingbird-Validation`: on a request that failed validation, why. The reasons are also in the error body's message field and in the request log (`request.validation`).
- Request overrides (on by default per mock): `X-Mockingbird-Session: <id>` (separate state), `X-Mockingbird-Status: 404` (force a declared status, or any 5xx), `X-Mockingbird-Latency: 250ms`, `X-Mockingbird-Scenario: <name>`, `X-Mockingbird-Stream-Items: 3`, `X-Mockingbird-Chaos: off`. `call_mock` has fields for each.
- GraphQL is served at `<prefix>/graphql`; gRPC on `127.0.0.1:50051` with reflection (`grpcurl -plaintext 127.0.0.1:50051 list`); SOAP at the WSDL's address path (`?wsdl` returns the WSDL).

State is per session: a `POST` creates a resource that a later `GET` of its id returns. Unknown ids are 404s when the contract declares one.

## 3. Shape behaviour

**Persona** (style of generated data): `update_mock {name, patch: {persona: "Terse, realistic EU retail data"}}`.

**Scenarios** are named override sets, selected per request with `X-Mockingbird-Scenario`:

```json
put_scenario {"mock": "orders", "name": "out-of-stock", "scenario": {
  "status": 409, "instructions": "Explain that the item is out of stock.",
  "operations": ["createOrder"], "latency_ms": 300, "set": {"code": "OUT_OF_STOCK"}
}}
```

Fields: `instructions`, `status` (must be declared by the covered operations unless 5xx, else 422), `latency_ms`, `set` (fields forced into the body), `operations` (operation ids; empty = all), `stream_items`, `stream_item_interval_ms`, `chaos`.

**Chaos** (fault injection) on the whole mock: `update_mock {name, patch: {chaos: {error_rate: 0.1, latency_jitter: {min_ms: 50, max_ms: 400}, drop_stream_rate: 0.05}}}`; `null` clears it.

**Fixtures**: `seed_session {mock, session, fixtures: [{resource_type, id, data}]}` loads known records; `export_session` returns them in the same form; `reset_session` clears a session.

**Which LLM**: `update_mock {name, patch: {llm_profile: "openrouter"}}`; profiles are managed with `list_llm_profiles`/`put_llm_profile` or the `mockingbird-setup` skill.

## 4. Inspect

- `list_requests {mock, limit, session?, operation?, source?}`: the recorded calls with redacted bodies, the operation matched, the source and latency. Use it to explain a surprising response or to assert in tests.
- `get_mock`, `list_contract_versions`, `server_logs`, `doctor`.
- The web console at http://127.0.0.1:9090/ shows the same, live.

## Events (AsyncAPI)

Mocks of AsyncAPI contracts publish generated messages to Kafka, AMQP, MQTT or WebSocket channels. `list_channels {mock}` shows each channel's triggers and broker; `publish_event {mock, channel, count, context}` publishes now. Brokers are declared in `mockingbird.yaml` (`brokers:`); WebSocket channels need none.

## Troubleshooting

- 404 from the data plane: wrong prefix. `get_mock` shows `route_prefix`; with one contract and no config it is `/`.
- `fallback` everywhere: the LLM fails or is slower than the profile timeout; run `test_llm`, check `server_logs`, or accept faker data.
- 409 on a write: the resource comes from `mockingbird.yaml`; edit the file.
- 422 `Scenario status not declared`: add the status to the contract or pick a declared one.
