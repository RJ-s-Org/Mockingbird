---
name: mockingbird-mock-api
description: Create and drive Mockingbird mocks - import an OpenAPI/Swagger, GraphQL, protobuf/gRPC, WSDL/SOAP or AsyncAPI contract, or turn API docs, a HAR recording or a Postman collection into one; call the mock; add scenarios (forced errors, latency, fields), per-session state and fixtures, chaos, and event channels. Use when the user asks to mock, fake or stub an API or a specific endpoint behaviour.
---

# Mock an API

Prerequisite: a running server (`server_status`; start one with `start_server` or `mockingbird server start -c mockingbird.yaml`). Without a config file, `mockingbird server start --offline api.yaml` serves a contract directly at `/`.

## 1. Get the contract in

| You have | Do |
| --- | --- |
| An app that calls a well-known public API (`api.stripe.com`, `api.themoviedb.org`, `api.github.com`, ...) | Check the catalog before writing anything: `list_examples` (CLI `mockingbird examples list`) gives each example's `hosts`; `init_example {name: "tmdb", dir: "mocks"}` (CLI `mockingbird examples init tmdb --dir mocks`) writes its contract and a `mockingbird.yaml` that serves it. Adapt that config to the app (ports, LLM profile, and `hosts`/`route_prefix`/`path_rewrite` when the app calls the API through another host or base path) and add to the contract only the operations the app calls that it lacks. When the app is a real client of that API (its SDK, its official CLI), pass `full: true` (CLI `--full`) where `list_examples` shows `full_spec: true` (it downloads the upstream contract; work only through the world gaps of operations the app calls): the example's hand-written subset lacks fields and arguments real clients send, and a GraphQL server refuses a whole query that names one field its schema lacks |
| OpenAPI 3.x / Swagger 2.0, GraphQL SDL or introspection JSON, `.proto` or descriptor set, WSDL 1.1, AsyncAPI 2/3 | `import_contract {source: "./api.yaml", mock: "orders"}`; CLI `mockingbird import api.yaml --as orders` |
| A directory of `.proto`/`.graphql`/WSDL+XSD files | pass the directory (CLI and local MCP); it is packed as tar.gz |
| API documentation (Markdown, HTML, PDF, text, or a URL to it), a HAR recording, a Postman collection | same call (several documentation pages: `urls: [...]`); it becomes a **draft** reviewed before it serves. Pass `approve: true` to take it when the import reports no warnings; otherwise review it: `get_draft {mock}` lists operations with confidence and warnings, `get_draft {mock, operation}` shows one definition, `edit_draft {mock, operation, patch}` fixes it with a JSON merge patch, `approve_draft {mock}` serves it. A later import merges into the draft or active contract (`mode: "replace"` starts over) |
| Only an idea of the API | write a small OpenAPI 3.1 file first, then import it |
| A contract that only exists in the conversation (remote agents, ChatGPT) | `import_contract {content: "<the file text>", filename: "api.yaml", mock: "orders"}` |

`validate_contract {path}` compiles a contract locally and lists its operation ids, without a server.

The first version is activated automatically; later imports need `activate: true` (or `activate_contract_version`). The mock is created if missing, mounted at `/<mock>` (`create_mock` / `update_mock` set `route_prefix`, `persona`, `llm_profile`, `chaos`, `limits`). In `mockingbird.yaml`, mocks are declared under `mocks.<name>.contract` instead, and are read-only through the API.

### Review the world description

After every import (and every `approve_draft`), call `get_world {mock, version}` and read its `gaps` first; pass `version`, the version number the import or approval answered, unless that version is now active (the world tools default to the active version, so without it you would read and change an older one): types with no identity, operations with no target, answers whose record could not be found, skipped routes, refused entries, and the behaviour the contract signals that the description does not state (list parameters the executor cannot read, and the advisory checks for actions that change nothing, unique members and soft deletes). The `import_contract` answer already carries `next_steps` and the first `world_gaps`. Fix them from the contract and its documentation with `patch_world {mock, version, patch, author: "agent"}`, as the `mockingbird-world` skill describes, then check a create read back with `call_mock`. A new contract version carries the previous description forward; entries that no longer fit show up as gaps.

The same review applies to mocks a config declares and `start_server {config}` starts without an import: its answer carries `world_review`, one entry per mock whose active description has gaps, each with that mock's `next_steps` and first `world_gaps`; work through them the same way (`get_world {mock}` lists them all). When two mocks of the config answer for the same entry of `hosts` and serve one API (REST beside GraphQL, for example), give them the same `mocks.<name>.shares: <group>` so a record created through one is read through the other, and describe a type whose name differs between the contracts with `types.<type>.same_as` (the `mockingbird-world` skill); the `start_server` answer's `next_steps` names such mocks. The CLI's `mockingbird server start` does not list gaps: after it, call `get_world {mock}` (CLI `mockingbird world show <mock>`) for each mock yourself.

## 2. Call it

`call_mock {mock: "orders", method: "POST", path: "/orders/orders", body: {...}, session: "alice"}` (or curl `localhost:8080/...`). The result includes status, body and the `x-mockingbird-*` headers. A mock with `hosts` (proxy mode, often `route_prefix: /`) answers only requests for those hosts: `call_mock` sends the mock's first host as `Host` when you name the `mock`, and with curl send it yourself (`curl -s localhost:8080/v1/items -H 'Host: api.example.com'`), or the data plane answers 404 "No mock mounted at this path":

- `X-Mockingbird-Source`: `llm`, `patched` (LLM output fixed locally to fit the schema), `repaired` (fixed by a second LLM call) or `fallback` (faker).
- `X-Mockingbird-Validation`: on a request that failed validation, why. The reasons are also in the error body's message field and in the request log (`request.validation`).
- Request overrides (on by default per mock): `X-Mockingbird-Session: <id>` (separate state), `X-Mockingbird-Status: 404` (force a declared status, or any 5xx), `X-Mockingbird-Latency: 250ms`, `X-Mockingbird-Scenario: <name>`, `X-Mockingbird-Stream-Items: 3`, `X-Mockingbird-Chaos: off`. `call_mock` has fields for each.
- GraphQL is served at `<prefix>/graphql`; gRPC on `127.0.0.1:50051` with reflection (`grpcurl -plaintext 127.0.0.1:50051 list`); SOAP at the WSDL's address path (`?wsdl` returns the WSDL).

### Run an app unchanged (proxy mode)

To point a real app at mocks without editing its code or base URLs:

1. Give the mock the host the app calls: `hosts: [api.example.com]` under `mocks.<name>` in `mockingbird.yaml` (or `update_mock`). A mock bound to hosts is mounted at `/` unless it names a `route_prefix`, since the proxy forwards the app's own paths; set one only when the API lives below a path on its host. An SDK or CLI often calls one vendor on several hosts (`api.`, `verify.`, `lookups.`): find each one the app's flows reach in its code or its SDK's base URLs, and give each a mock (a catalog example may cover only some).
2. Start the server with the proxy: `start_server {config: "mockingbird.yaml", proxy: true}` (CLI `mockingbird server start --proxy -c mockingbird.yaml`).
3. Export the variables `proxy_env` returns (its `export` field) in the shell that runs the app (CLI `eval "$(mockingbird proxy env)"`). They route HTTP(S) through the proxy and make the app trust Mockingbird's local CA; calls to the mocked hosts are answered by the mock, others pass through. After a self-test, with the server still running, `mockingbird verify --no-unmapped` fails when a host no mock claims was reached (`mockingbird report` lists them under `unmapped_hosts`, with their `mode`): a host of the vendor's there reached the real API. Mock it, or set `proxy: {unmapped: block}` so it answers 502 instead, and tell the user which hosts you left out.

Or in one step: `mockingbird run -c mockingbird.yaml -- <command>` starts the server and proxy, runs the command with those variables, and stops the server afterwards. Give the user this form for running their app or tests, rather than an `eval` of `mockingbird proxy env`, which silently sets nothing when `mockingbird` is not on `PATH`. Node.js needs no extra package: the variables include a `NODE_OPTIONS` preload that routes every agent (stripe-node's, got's) and `fetch`. Editing the app's base URL to `http://127.0.0.1:8080/<mock>` is the fallback for clients that ignore proxy variables (bare `urllib3`, Go and system Ruby on macOS for HTTPS). If the app calls the API under another host or base path, set `path_rewrite: {add_prefix: /3}` (or `strip_prefix`) on the mock rather than copying the contract.

State is per session: a `POST` creates a resource that a later `GET` of its id returns. A read of an id the session does not hold (never created or listed) is generated by default (`unknown_ids: generate`). A deleted id already answers 404. Set `update_mock {name, patch: {unknown_ids: "404"}}` (or `mocks.<name>.unknown_ids: 404` in `mockingbird.yaml`) so that a mistyped or made-up id fails as the real API's does, with the operation's declared 404, unless the app reads ids it did not create through the mock: ids from its config or environment (a price id), the signed-in account, ids arriving in webhooks or from another mock. Keep `generate` for those. This holds for a path parameter named like an id (`{id}`, `{orderId}`); for one named otherwise (`{session}`, `{username}`), also give the read a `target` in the world description, or it stays generated: `operations: {getSession: {target: {path_param: session}}}`.

### Run the app's own flows before you report

A smoke call of one read only shows that the host is routed. Before you tell the user the mock is ready, run the app's own tests, or its main commands, through the mock (`mockingbird run -c mockingbird.yaml -- <command>`). Include a write followed by reads of what it wrote: create, then list and view it, then update or close it and read it again. Fix what fails, either in the contract (missing operations or fields) or in the world description (a write that does not change what later reads show). Then report which flows you ran and which still fail.

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

- `list_requests {mock, limit, session?, operation?, source?}`: the recorded calls (method, path, status, operation matched, source, fallback reason, latency, body sizes). `get_request {mock, id}` (or `include_bodies: true`) adds the redacted bodies. Use it to explain a surprising response or to assert in tests.
- `get_mock`, `list_contract_versions`, `server_logs`, `doctor`.
- The web console at http://127.0.0.1:9090/ shows the same, live.

## Events (AsyncAPI)

Mocks of AsyncAPI contracts publish generated messages to Kafka, AMQP, MQTT or WebSocket channels. `list_channels {mock}` shows each channel's triggers and broker; `publish_event {mock, channel, count, context}` publishes now. Brokers are declared in `mockingbird.yaml` (`brokers:`); WebSocket channels need none.

## Troubleshooting

- 404 from the data plane: wrong prefix. `get_mock` shows `route_prefix`; with one contract and no config it is `/`.
- 400 on a request the real API accepts (`X-Mockingbird-Validation` names the member: a field required that the API does not require, `null` or a number refused where it takes one): the contract is stricter than the API, often after a documentation import. `edit_contract {mock, operation, patch}` merge-patches that operation of the active contract (`{"requestBody": {"content": {"application/json": {"schema": {"required": []}}}}}`) and activates the result as a new version; review its world gaps as after any import. A mock declared in `mockingbird.yaml` takes the edit in its contract file instead.
- `fallback` everywhere: the LLM fails or is slower than the profile timeout; run `test_llm`, check `server_logs`, or accept faker data.
- `X-Mockingbird-Fallback-Reason: LLM request timed out`: a served answer waits at most `limits.generation_timeout` (20s by default) on the model. For a slow model or large answers (lists of 20 records), raise it at the top level of `mockingbird.yaml` (`limits: {generation_timeout: 60s}`) and restart the server: the `limits` section is not reloaded.
- 409 on a write: the resource comes from `mockingbird.yaml`; edit the file.
- 422 `Scenario status not declared`: add the status to the contract or pick a declared one.
