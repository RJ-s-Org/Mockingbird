# Mockingbird P4 Protocols Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Serve GraphQL (with subscriptions), gRPC (all four call kinds, reflection, health), SOAP/WSDL and SSE mocks from the same engine, with NDJSON streaming generation, deterministic GraphQL/proto/WSDL compilers and multi-file archive uploads.

**Architecture:**
- **`mb-mcm`** gains the GraphQL, gRPC and SOAP bindings, three streaming kinds, an `input_field` identity hint and a `protocol_payload` carrying the SDL, descriptor set or WSDL the adapters need on the wire.
- **`mb-llm` / `mb-engine`** gain text-delta streaming and `Engine::generate_stream`: NDJSON items, each validated as it completes, repaired once or replaced by faker output.
- **`mb-import`** gains GraphQL (SDL and introspection), protobuf (`.proto` via `protox`, `FileDescriptorSet`) and WSDL 1.1/XSD compilers, plus zip/tar.gz archives.
- **`mb-protocols`**:
  - REST's request pipeline (overrides, admission, state, recording) moves into a shared `call` module.
  - Adapters behind features: `sse` (REST `text/event-stream`), `graphql` (async-graphql dynamic schema; subscriptions over the reusable `ws` module), `grpc` (tonic + prost-reflect on its own h2c listener, with reflection and health) and `soap`.
- **`mockingbird`** mounts each mock by its contract's protocol family. gRPC mocks share one `GrpcServer` on `server.grpc_port`. Protocol knobs live in a new `protocols:` config section.

**Tech Stack:** P2 stack, plus these exact pins (all `[workspace.dependencies]`):
- `futures = "0.3.34"`, `tokio-stream = "0.1.19"`, `base64 = "0.23.1"`, `eventsource-stream = "0.2.3"`
- `async-graphql = { version = "7.2.1", default-features = false, features = ["dynamic-schema"] }`
- `async-graphql-parser = "7.2.1"` (8.x is still a release candidate)
- `tonic = "0.14.6"` (default features), `tonic-reflection = "0.14.6"`, `tonic-health = "0.14.6"`
- `prost = "0.14.4"`, `prost-types = "0.14.4"`, `prost-reflect = { version = "0.16.5", features = ["serde"] }`, `protox = "0.10.0"`
- `quick-xml = "0.42.0"`, `zip = { version = "9.0.3", default-features = false, features = ["deflate"] }`, `tar = "0.4.46"`, `flate2 = "1.1.10"`
- dev: `tokio-tungstenite = "0.29"`, matching the `tungstenite` 0.29 that axum 0.8.9 uses, so there is no duplicate.
- Feature changes:
  - `reqwest` adds `"stream"`;
  - `axum` gets `"ws"` in mb-protocols (feature `websocket`), and `"http2"` in mb-protocols (feature `grpc`) and in `mockingbird`.

**Spec:** `docs/superpowers/specs/2026-10-14-mockingbird-platform-design.md`. This plan covers the P4 row of §16 and implements §§4.2, 4.4, 5, 6.1 (GraphQL, proto, WSDL), 7 (per protocol) and 7.2. The controller's decisions brief (`p4-decisions.md`) is folded into the tasks below, and the plan is self-contained.

## Global Constraints

- All P1/P2 Global Constraints still hold:
  - no `unwrap()`/`expect()` outside tests;
  - `cargo clippy --workspace --all-targets --all-features -- -D warnings` and `cargo fmt --all --check` clean;
  - Admin API errors are `application/problem+json`;
  - workspace id `"default"`;
  - Postgres tests read `MB_TEST_POSTGRES_URL` and run through `scripts/test-postgres.sh`;
  - importer golden tests use `insta` YAML snapshots.
- Docker is not available locally. CI verifies the image.
- Crate boundaries:
  - `mb-mcm` depends only on serde, serde_json and base64.
  - `mb-engine` must not depend on axum, tonic, async-graphql or quick-xml.
  - `mb-import` must not depend on axum, tonic or mb-protocols.
  - Data-plane code never parses source specs. Adapters read only `Bundle` and `Bundle::protocol_payload`.
- `mb-protocols` features, exactly:
  - `http-rest`, `sse = ["http-rest"]`, `websocket`, `graphql` (implies `websocket`), `grpc`, `soap`;
  - `default = ["http-rest", "sse", "graphql", "grpc", "soap", "websocket"]`.
  - `axum`, `humantime`, `thiserror` and `futures` become non-optional, because the shared `call` module needs them.
  - `cargo check -p mb-protocols --no-default-features --features <f>` must pass for each single feature.
- **No control-store schema change in P4.** Add no migration. New per-scenario settings ride in the `scenarios.rules` JSON column. A parallel branch (P3) adds migrations, auth and multi-workspace routes.
- Edits that must stay small and additive (P3 touches the same files):
  - `crates/mb-admin/src/lib.rs`: none.
  - `crates/mockingbird/src/config.rs`: one flag, one `App` field, one bind helper.
  - New admin behaviour lives in `contracts.rs` and `scenarios.rs`.
- Every new serde field on a stored type is `#[serde(default, skip_serializing_if = "Option::is_none")]` (or `default` for bools/vecs). P1–P3 bundles and rules then load unchanged, and the existing OpenAPI snapshots stay byte-identical.
- Override names:
  - HTTP headers `X-Mockingbird-{Session,Status,Latency,Scenario,Source}`, plus the new `X-Mockingbird-Stream-Items` (integer 1–10000).
  - gRPC metadata uses the same names lowercased.
  - GraphQL reports sources under response `extensions.mockingbird`.
- Limits (defaults are configurable where noted):

| Limit | Value |
|---|---|
| GraphQL query depth (cfg) | 15 |
| GraphQL query complexity (cfg) | 1000 |
| GraphQL request body | 1 MiB |
| gRPC message, decode and encode (cfg) | 4 MiB |
| SOAP request body | 4 MiB |
| Archive upload | 20 MiB |
| Archive member files | 500 |
| Archive unpacked total | 64 MiB |
| Stream items, default (cfg) | 5 |
| Stream items, max (cfg) | 50 |
| Stream item interval (cfg) | 0 |
| SSE keepalive (cfg) | 15 s |
| WebSocket ping (cfg) | 30 s |
| `<request_stream>` items in a prompt | last 20, each truncated at 8 KiB |

- gRPC listener:
  - `server.grpc_port` (default `50051`), `MB_GRPC_PORT`, `--grpc-port`, on the same host as the other ports, with the same loopback / `--insecure-no-auth` rule;
  - h2c only;
  - a busy default port is skipped with a warning, like the admin port.
- Operation id formats:
  - GraphQL: `Query.<field>` / `Mutation.<field>` / `Subscription.<field>`.
  - gRPC: `<Service>.<Method>`, or `<package>.<Service>.<Method>` when the short form collides.
  - SOAP: `<operation>`, or `<Service>.<operation>` on collision.
- Streaming:
  - Generated stream items are never written to state.
  - Engine streams are polled by the response body or the subscription and are never spawned onto detached tasks. Dropping the consumer stops generation.
- Every adapter call runs inside `tracing::info_span!("mock.request", mock, operation, session, status, source)`, as REST does (spec §14).
- Importers are deterministic:
  - iterate sorted (`BTreeMap`/sorted `Vec`), never `HashMap` order;
  - the same bytes give the same `Bundle`;
  - `source_digest` is the hex sha256 of the uploaded bytes (for an archive, of the archive).
- Faker formats added in Task 3 and emitted by importers next to their patterns:
  - `int64` → `^-?[0-9]+$`
  - `uint64` → `^[0-9]+$`
  - `byte` (base64)
  - `time`
  - `duration` (ISO 8601)
  - `proto-duration` → `^-?[0-9]+(\.[0-9]+)?s$`
  - `hex` → `^[0-9A-Fa-f]*$`

## Review Focus

1. **A huge or deeply nested GraphQL query.**
   - Expected: depth > 15 or complexity > 1000 returns GraphQL `errors[]` and makes no engine call.
   - Test: Task 7 `query_depth_and_complexity_limits`.
2. **An oversized gRPC message.**
   - Expected: a request over 4 MiB fails with tonic's `OUT_OF_RANGE` before any decode or generation.
   - Test: Task 10 `oversized_message_rejected`.
3. **XML bombs.**
   - Expected: a `<!DOCTYPE …>` in a SOAP request gives a Client/Sender fault naming DOCTYPE, with no engine call. In an uploaded WSDL or XSD it is `ImportError::Invalid`. quick-xml never expands entities.
   - Tests: Task 12 `doctype_request_rejected`, Task 11 `doctype_rejected`.
4. **Archive path traversal (zip-slip) and archive bombs.**
   - Expected: absolute paths, `..`, backslashes, links, more than 500 files, or more than 64 MiB unpacked are each rejected with an error naming the entry. Nothing is written to disk.
   - Tests: Task 13 `zip_slip_rejected`, `symlink_entry_rejected`, `too_many_files_rejected`, `unpacked_size_limit`, admin `upload_zip_slip_422`.
5. **An unknown gRPC service or method.**
   - Expected: `UNIMPLEMENTED`, not a hang or a 404 body.
   - Test: Task 10 `unknown_service_is_unimplemented`.

---

## File Structure

```
crates/mb-mcm/src/lib.rs                         + bindings, kinds, Identity.input_field, ProtocolPayload
crates/mb-llm/src/{lib.rs,openai.rs,fallback.rs,scripted.rs}   + Provider::stream
crates/mb-engine/src/{lib.rs,prompt.rs,faker.rs,stream.rs,ndjson.rs}
crates/mb-import/src/{lib.rs,heuristics.rs,openapi3.rs,swagger2.rs,graphql.rs,archive.rs}
crates/mb-import/src/proto/{mod.rs,schema.rs}
crates/mb-import/src/wsdl/{mod.rs,xml.rs,xsd.rs}
crates/mb-protocols/src/{lib.rs,call.rs,ws.rs}
crates/mb-protocols/src/http_rest/{handler.rs,sse.rs}
crates/mb-protocols/src/graphql/{mod.rs,schema.rs,resolve.rs,subscription.rs}
crates/mb-protocols/src/grpc/{mod.rs,codec.rs,service.rs,server.rs,status.rs}
crates/mb-protocols/src/soap/{mod.rs,envelope.rs,xml_json.rs}
crates/mb-store/src/model.rs                     ScenarioRules stream fields (JSON only)
crates/mb-admin/src/{contracts.rs,scenarios.rs}
crates/mockingbird/src/{registry.rs,runtime.rs,config.rs,file_config.rs,reconcile.rs,client.rs,main.rs}
fixtures/openapi/events-3.1.yaml
fixtures/graphql/{github.graphql,starwars.graphql,starwars-introspection.json}
fixtures/proto/{helloworld.proto,route_guide.proto,multi/orders/v1/orders.proto,multi/common/v1/money.proto}
fixtures/wsdl/{calculator.wsdl,accounts-rpc.wsdl,external/service.wsdl,external/types.xsd}
```

---

### Task 1: MCM protocol model

**Files:**
- Modify:
  - `crates/mb-mcm/src/lib.rs` and `Cargo.toml` (add `base64`);
  - every exhaustive match and struct literal the change breaks:
    - `crates/mb-import/src/{openapi3.rs,swagger2.rs,heuristics.rs}`
    - `crates/mb-engine/src/{lib.rs,prompt.rs}`
    - `crates/mb-admin/src/contracts.rs`
    - `crates/mb-protocols/src/http_rest/handler.rs`
    - `crates/mb-store/tests/common/conformance.rs`
- Test: `crates/mb-mcm/tests/model.rs`, `crates/mb-engine/tests/generate.rs`, `crates/mb-admin/tests/api.rs`

**Interfaces:**
- Produces (serde `rename_all = "snake_case"` as for existing types):

```rust
pub enum Protocol { Http, Graphql, Grpc, Soap }                       // stays #[non_exhaustive]
pub enum GraphqlOperationType { Query, Mutation, Subscription }
pub enum SoapVersion { Soap11, Soap12 }                               // serde "soap11" / "soap12"
pub enum Binding {                                                    // stays #[non_exhaustive]
    Http { method: HttpMethod, path: String },
    Graphql { operation_type: GraphqlOperationType, field: String },
    Grpc { package: String, service: String, method: String },        // service = short name
    Soap { service: String, port: String, operation: String, soap_action: Option<String>,
           version: SoapVersion, address_path: String },
}
pub enum OperationKind { Query, Command(Effect), StreamServer, StreamClient, StreamBidi }
pub struct Identity { pub path_param: Option<String>, pub field: String,
    /// Top-level request-body field carrying the target id (non-HTTP protocols).
    #[serde(default, skip_serializing_if = "Option::is_none")] pub input_field: Option<String> }
pub enum ProtocolPayload {                                            // externally tagged
    Grpc { #[serde(with = "base64_bytes")] file_descriptor_set: Vec<u8> },   // standard base64 with padding
    Graphql { sdl: String },
    Soap { wsdl: String },
}
// Bundle gains: #[serde(default, skip_serializing_if = "Option::is_none")] pub protocol_payload: Option<ProtocolPayload>
impl Bundle { pub fn protocol(&self) -> Protocol }                    // first operation's; Http when empty
impl Binding { pub fn grpc_path(&self) -> Option<String> }            // "/pkg.Service/Method"; "/Service/Method" for empty package
impl OperationKind { pub fn streams_response(&self) -> bool; pub fn streams_request(&self) -> bool }
impl Operation { pub fn stream_item_schema(&self) -> Option<&Value>  // success_output().schema for the three stream kinds
                 pub fn target(&self) -> String }                     // see OperationSummary.target below; used by admin and `validate`
impl OperationKind { pub fn label(&self) -> &'static str }            // query|create|update|delete|other|stream-server|stream-client|stream-bidi
```

- Doc comment on `OperationKind`: for the streaming kinds, the success `OutputVariant`'s schema is the schema of ONE stream item (or of the single response, for `StreamClient`).
- `mb-admin` `OperationSummary` gains two fields:
  - `protocol: String` (`http|graphql|grpc|soap`);
  - `target: String`:
    - HTTP: `"GET /orders/{id}"`
    - GraphQL: `"query user"`
    - gRPC: `grpc_path()`
    - SOAP: `"<service>/<port> <operation>"`
- `OperationSummary.kind` comes from `OperationKind::label` and adds `stream-server`, `stream-client` and `stream-bidi`. `target` comes from `Operation::target`. `method`/`path` stay empty for non-HTTP.
- Engine changes:
  - `reflect`/`reassert_request` treat the stream kinds like `Query` without a target;
  - `force_identity` takes the id from `call.path_params[path_param]`, else `call.body[input_field]`.
- `prompt::system_message` writes one binding line per protocol:
  - `GraphQL: <query|mutation|subscription> <field>`
  - `gRPC: <grpc_path> (<unary|server streaming|client streaming|bidi streaming>)`
  - `SOAP: <service>/<port> <operation>`
  - The HTTP line is unchanged.

- [ ] **Step 1: Write failing tests:**
  - `p2_bundle_without_payload_deserializes`: an orders bundle's JSON with no `protocol_payload` and no `input_field` → `None`, and it re-serializes byte-identically.
  - `grpc_payload_round_trips_base64`: bytes `[0,1,2,255]` ↔ `{"grpc":{"file_descriptor_set":"AAEC/w=="}}`.
  - `new_bindings_and_kinds_round_trip`: `{"graphql":{"operation_type":"subscription","field":"reviewAdded"}}` and `"stream_bidi"`.
  - `bundle_protocol_is_first_operations`.
  - `grpc_path_formats`.
  - `force_identity_uses_input_field` (generate.rs: a query op with `Identity{path_param: None, field: "id", input_field: Some("id")}` and body `{"id":"42"}` → offline response `id == "42"`).
  - `prompt_names_grpc_binding`.
  - `operation_summaries_report_protocol_and_target` (admin, orders contract: `protocol == "http"`, `target == "GET /orders/{orderId}"`).
- [ ] **Step 2:** Run `cargo test --workspace --all-features`. Expected: FAIL (compile errors in the new tests).
- [ ] **Step 3:** Implement. The existing `openapi3__orders_30_snapshot` must pass without `cargo insta accept`.
- [ ] **Step 4:** Run `cargo test --workspace --all-features`. Expected: all pass.
- [ ] **Step 5:** Commit: `feat(mcm): GraphQL, gRPC and SOAP bindings, streaming kinds, protocol payload`.

---

### Task 2: LLM text streaming

**Files:**
- Modify: `crates/mb-llm/src/{lib.rs,openai.rs,fallback.rs,scripted.rs}`, `crates/mb-llm/Cargo.toml` (futures, eventsource-stream, reqwest `stream`), root `Cargo.toml` (new workspace deps from the Tech Stack)
- Test: `crates/mb-llm/tests/{openai.rs,fallback.rs,scripted.rs}` (`scripted` needs `required-features = ["testing"]`)

**Interfaces:**
- Produces:

```rust
pub type TextStream = futures::stream::BoxStream<'static, Result<String, LlmError>>;
// Provider gains (default: complete() once, yielding its content as one delta):
async fn stream(&self, req: ChatRequest) -> Result<TextStream, LlmError>;
// ScriptedProvider gains:
pub fn with_streams(self, streams: Vec<Result<Vec<Result<String, LlmError>>, LlmError>>) -> Self;
pub fn with_chunk_delay(self, delay: Duration) -> Self;   // sleep before each streamed chunk
pub fn chunks_yielded(&self) -> usize;                    // chunks handed to consumers so far
pub fn stream_requests(&self) -> Vec<ChatRequest>;
```

Rules:
- **OpenAiCompatible::stream:**
  - sends the P1 body plus `"stream": true`;
  - `req.timeout` bounds only the arrival of the response head;
  - a non-2xx status → `Err(LlmError::Http{..})` from `stream()` itself;
  - parses the SSE `data:` events, yielding `choices[0].delta.content` when it is a non-empty string;
  - `[DONE]` ends the stream;
  - a mid-stream transport or decode error yields one `Err` and ends.
- **FallbackProvider::stream:** opens the primary with the primary's `HopSettings`. When `stream()` errs, or the first item is an `Err`, it warns and returns the secondary's stream. After the first `Ok` delta it never switches.
- **ScriptedProvider::stream:** when `with_streams` was used, each call pops one script entry; an exhausted script gives `Err(Transport("script exhausted"))`. Otherwise it uses the trait default. `requests()` keeps recording only `complete` calls.

- [ ] **Step 1: Write failing tests:**
  - `openai_stream_yields_deltas` (wiremock `text/event-stream` body with two content deltas, a role-only delta and `[DONE]` → `["{\"a\"", ":1}\n"]`; the request body has `"stream": true`)
  - `openai_stream_http_error_before_first_delta` (500 → `Err(Http{status: 500, ..})`)
  - `default_stream_yields_complete_once`
  - `fallback_stream_uses_secondary_when_primary_fails_first`
  - `fallback_stream_does_not_switch_after_first_delta` (primary `["a", Err]` → `"a"` then `Err`; the secondary's `stream_requests()` is empty)
  - `scripted_stream_counts_chunks` (paused clock, delay 10 ms, take 2 of 5 → `chunks_yielded() == 2`)
- [ ] **Step 2:** Run `cargo test -p mb-llm --all-features`. Expected: FAIL.
- [ ] **Step 3:** Implement.
- [ ] **Step 4:** Run `cargo test -p mb-llm --all-features`. Expected: all pass.
- [ ] **Step 5:** Commit: `feat(llm): streaming text deltas with fallback and scripted streams`.

---

### Task 3: Engine streaming (NDJSON)

**Files:**
- Create: `crates/mb-engine/src/{stream.rs,ndjson.rs}`
- Modify: `crates/mb-engine/src/{lib.rs,prompt.rs,faker.rs}`, `Cargo.toml` (futures)
- Test: `crates/mb-engine/tests/stream.rs`, `crates/mb-engine/tests/faker.rs`

**Interfaces:**
- Consumes: `Provider::stream`, `TextStream`, `ScriptedProvider::{with_streams, with_chunk_delay, chunks_yielded}` (Task 2); `Operation::stream_item_schema` (Task 1).
- Produces:

```rust
#[derive(Clone, Debug)] pub struct StreamConfig { pub default_items: usize /*5*/, pub max_items: usize /*50*/, pub item_interval: Duration /*0*/ }
// EngineConfig gains `pub stream: StreamConfig` (Default as above)
#[derive(Clone, Debug, Default)] pub struct StreamContext { pub related: Vec<StoredResource>, pub target: Option<StoredResource>,
    pub scenario: Option<ScenarioSpec>, pub request_stream: Vec<Value>, pub item_interval: Option<Duration> }
#[derive(Clone, Debug, PartialEq)] pub struct StreamItem { pub value: Value, pub source: Source }
impl Source { pub fn worst(self, other: Source) -> Source }           // Fallback > Repaired > Llm
impl Engine {
    pub fn generate_stream(self: &Arc<Self>, bundle: Arc<Bundle>, op: &Operation, call: OperationCall,
        ctx: StreamContext, max_items: Option<usize>) -> Result<BoxStream<'static, StreamItem>, EngineError>;
    pub fn conform_item(&self, bundle: &Bundle, op: &Operation, value: &Value) -> bool;
    pub fn config(&self) -> &EngineConfig;
}
// prompt.rs
pub const MAX_REQUEST_STREAM: usize = 20;
pub fn build_stream_messages(bundle: &Bundle, op: &Operation, item_schema: &Value, items: usize,
    call: &OperationCall, ctx: &StreamContext, persona: Option<&str>) -> Vec<ChatMessage>;
// ndjson.rs
pub struct LineSplitter;
impl LineSplitter { pub fn new() -> Self; pub fn push(&mut self, delta: &str) -> Vec<String>; pub fn finish(self) -> Option<String> }
```

Rules:
- **Item count and errors:**
  - `N = max_items.unwrap_or(stream.default_items).clamp(1, stream.max_items)`.
  - `Err(EngineError::NoOutput)` when the op has no item schema.
- **Offline engine:** N faker items (`Source::Fallback`), each validated the way `fake_response` does.
- **With a provider** (one `provider.stream` call, opened lazily on first poll, `ResponseFormat::Text`):
  - Every complete non-blank line from `LineSplitter` is one candidate. Lines starting with three backticks are skipped, and a trailing `,` is stripped. Then `extract_json` + validate.
  - Valid → `Llm`.
  - Invalid → one `provider.complete` repair. It uses `JsonSchema` of the item and messages with system + `"Fix this stream item. Errors: …\n<item>…</item>\nReturn only the corrected JSON."`, bounded by `total_timeout`. Valid → `Repaired`, else a faker item → `Fallback`.
  - The stream ends after N items; extra lines are ignored. A provider stream that ends early, errors, or times out → the remaining items come from the faker.
- **Timeouts:**
  - the first item within `provider.total_timeout(config.timeout)` of opening;
  - each later item within `config.timeout` of the previous one.
- **Pacing and post-processing:**
  - `ctx.item_interval.unwrap_or(stream.item_interval)` is slept before every item except the first;
  - a scenario `set` patch applies to object items when the result still validates;
  - no `reflect`.
- **Prompt:**
  - The system message stays static per operation: the stream role statement (one JSON value per line; no array, prose or fences; no real personal data), the binding lines, then `Stream item JSON Schema:`.
  - The user message is the unary user message, plus `<request_stream>` (the last 20 items, each `data()`-escaped and truncated at `MAX_BODY_BYTES`) when non-empty, plus a final line `Generate {N} items.`
- **Usage** is not tracked for streams.
- **Faker formats** (strings): `int64`, `uint64`, `byte`, `time`, `duration`, `proto-duration`, `hex`, as in Global Constraints.

- [ ] **Step 1: Write failing tests** (`stream_bundle()` builds a `StreamServer` op whose item is `{id: string, n: integer}`, both required):
  - `ndjson_split_across_arbitrary_boundaries` (proptest: any split of a 3-line text yields the same 3 lines)
  - `stream_yields_n_llm_items` (chunks `["{\"id\":\"a\"", ",\"n\":1}\n{\"id\":\"b\",\"n\":2}\n", "{\"id\":\"c\",\"n\":3}"]`, max 3 → 3 `Llm` items in order)
  - `invalid_line_is_repaired`
  - `unrepairable_line_falls_back_to_faker`
  - `provider_error_mid_stream_fills_with_faker` (`[line, Err]`, N=4 → 1 `Llm` + 3 `Fallback`, all valid)
  - `offline_engine_yields_faker_items` (5)
  - `max_items_caps_requested_count` (500 → 50)
  - `stream_prompt_includes_request_stream_and_count` (25 values → only the last 20, plus `Generate 3 items.`)
  - `idle_timeout_falls_back` (paused clock)
  - `item_interval_paces_items` (100 ms, 3 items → ≥ 200 ms)
  - `dropping_stream_stops_provider` (chunk delay 20 ms, 50 lines; take 1, drop; after 200 ms `chunks_yielded() ≤ 2`)
  - `faker_formats_match_patterns` (faker.rs: each new format validates against its pattern)
- [ ] **Step 2:** Run `cargo test -p mb-engine`. Expected: FAIL.
- [ ] **Step 3:** Implement.
- [ ] **Step 4:** Run `cargo test -p mb-engine`. Expected: all pass.
- [ ] **Step 5:** Commit: `feat(engine): NDJSON stream generation with per-item repair and faker fill`.

---

### Task 4: Shared call pipeline in `mb-protocols`

**Files:**
- Create: `crates/mb-protocols/src/call.rs`
- Modify: `crates/mb-protocols/src/{lib.rs,http_rest/handler.rs}`, `crates/mb-protocols/Cargo.toml` (features per Global Constraints; `futures`)
- Test: `crates/mb-protocols/tests/call.rs`, plus unit tests in `call.rs`

**Interfaces:**
- Consumes: `Engine::generate_stream`, `StreamContext`, `StreamItem`, `Source::worst`, `Engine::conform_item` (Task 3); `Identity.input_field` (Task 1).
- Produces:

```rust
// lib.rs (pub)
#[derive(Clone, Debug)] pub struct ProtocolSettings { pub graphql_path: String /*"/graphql"*/, pub graphql_introspection: bool /*true*/,
    pub graphql_max_depth: usize /*15*/, pub graphql_max_complexity: usize /*1000*/, pub sse_keepalive: Duration /*15s*/,
    pub ws_keepalive: Duration /*30s*/, pub grpc_max_message_bytes: usize /*4 MiB*/ }      // impl Default
// MockRuntime gains `pub protocols: ProtocolSettings` (default in MockRuntime::new)
// ScenarioRuntime gains `pub stream_items: Option<usize>, pub stream_item_interval: Option<Duration>`
#[derive(Clone, Debug)] pub struct MountPrefix(pub String);  // request extension: the mount prefix the dispatcher stripped
pub const STREAM_ITEMS_HEADER: &str = "x-mockingbird-stream-items";
// call.rs (pub(crate))
pub(crate) struct Overrides { pub session: String, pub status: Option<u16>, pub latency: Option<Duration>,
    pub scenario: Option<String>, pub stream_items: Option<usize> }
impl Overrides { pub(crate) fn parse(headers: &HeaderMap, enabled: bool) -> Result<Self, String> }
pub(crate) struct CallPlan<'a> { pub op: &'a Operation, pub session: SessionKey, pub scenario: Option<&'a ScenarioRuntime>,
    pub forced_status: Option<u16>, pub delay: Option<Duration>, pub stream_items: Option<usize>, pub tenancy: Tenancy }
pub(crate) enum PlanError { Rejected(Rejection), BadOverride(String), UnknownScenario(String) }
pub(crate) async fn plan<'a>(rt: &'a MockRuntime, op: &'a Operation, headers: &HeaderMap) -> Result<CallPlan<'a>, PlanError>;
pub(crate) fn target_id(op: &Operation, call: &OperationCall) -> Option<String>;
pub(crate) async fn generate_once(rt: &MockRuntime, plan: &CallPlan<'_>, call: &OperationCall) -> Result<GeneratedResponse, EngineError>;
pub(crate) async fn open_stream(rt: &Arc<MockRuntime>, plan: &CallPlan<'_>, call: OperationCall, request_stream: Vec<Value>)
    -> Result<BoxStream<'static, StreamItem>, EngineError>;
pub(crate) fn change_feed(rt: &Arc<MockRuntime>, session: SessionKey, op: &Operation) -> BoxStream<'static, Value>;
pub(crate) struct Report { pub operation_id: Option<String>, pub session: String, pub method: String, pub path: String,
    pub status: u16, pub source: Option<Source>, pub usage: Usage, pub latency: Duration, pub tenancy: Option<Tenancy>,
    pub admission_ran: bool, pub request: Value, pub request_body_bytes: Option<usize>, pub response: Option<Value> }
pub(crate) async fn report(rt: &MockRuntime, headers: &HeaderMap, r: Report);
pub(crate) fn forwarded_headers(headers: &HeaderMap) -> Map<String, Value>;
pub(crate) fn recorded_headers(headers: &HeaderMap) -> Map<String, Value>;
pub(crate) struct StateAccess<'a>;   // moved unchanged from handler.rs, with related()/write_state()
```

Rules:
- This is a move, not a rewrite. REST behaviour stays byte-identical, and every existing `mb-protocols`, `mockingbird` and `mb-admin` test passes unmodified.
- **`plan`:**
  - admission (`tenants.resolve` → `quota.admit`), then `Overrides::parse`, then scenario lookup;
  - precedence as P2: header status/latency beat the scenario's;
  - `stream_items` = header > `scenario.stream_items`.
- **`target_id`:** `identity.path_param` in `call.path_params`; else `identity.input_field` in `call.body`; else, for update/delete on HTTP, the last path parameter (the P2 rule).
- **`generate_once`:** steps 5–7 of the P2 handler: target/related read, `engine.generate`, state write for 2xx, `touch`.
- **`open_stream`:**
  - reads target/related exactly like `generate_once`, then calls `engine.generate_stream`;
  - `ctx.scenario` is the scenario spec;
  - `ctx.item_interval = scenario.stream_item_interval`;
  - `max_items = plan.stream_items`;
  - never writes state.
- **`change_feed`:**
  - subscribes to `rt.state.subscribe()`;
  - for `Upserted` events in `session` whose `resource_type == op.resource_type`, it `get`s the resource and yields its data when `engine.conform_item` accepts it;
  - lagged → skips;
  - an empty stream when the op has no `resource_type`.

- [ ] **Step 1: Write failing tests:**
  - `target_id_from_input_field` (unit)
  - `stream_items_header_validated` (REST GET with `X-Mockingbird-Stream-Items: 0` and `abc` → 400 problem `Invalid override`)
  - `change_feed_emits_upserts_for_resource_type` (MemoryStore: an upsert in the same session yields the data within 1 s; another session or type yields nothing)
  - `open_stream_applies_scenario_stream_settings` (a scenario with `stream_items: 2` → 2 items)
- [ ] **Step 2:** Run `cargo test -p mb-protocols`. Expected: FAIL.
- [ ] **Step 3:** Implement. Move code out of `handler.rs` and have the handler call `call::*`.
- [ ] **Step 4:** Run `cargo test --workspace --all-features`, plus `cargo check -p mb-protocols --no-default-features --features http-rest`. Expected: all pass.
- [ ] **Step 5:** Commit: `refactor(protocols): shared call pipeline for all adapters`.

---

### Task 5: SSE (REST `text/event-stream`)

**Files:**
- Create: `crates/mb-protocols/src/http_rest/sse.rs`, `fixtures/openapi/events-3.1.yaml`
- Modify: `crates/mb-import/src/{openapi3.rs,swagger2.rs}`, `crates/mb-protocols/src/http_rest/{handler.rs,mod.rs}`
- Test: `crates/mb-import/tests/openapi3.rs`, `crates/mb-protocols/tests/sse.rs` (dev-dep `reqwest` with `stream`)

**Interfaces:**
- Consumes: `call::{plan, open_stream, change_feed, report, Report}`, `ProtocolSettings.sse_keepalive` (Task 4).
- Produces:
  - REST ops of kind `StreamServer` answer `text/event-stream`;
  - `pub(crate) fn sse_response(rt: Arc<MockRuntime>, plan: CallPlan<'_>, call: OperationCall, headers: HeaderMap) -> Response`.

Rules:
- **Import (OpenAPI 3 and Swagger 2 `produces`):**
  - A 2xx response with a `text/event-stream` media type gives an `OutputVariant` with `content_type "text/event-stream"`, and the op's kind becomes `StreamServer`.
  - The item schema is the media type's `x-mockingbird-item-schema` if present; else the schema's `items` when it is an array; else the schema.
  - `x-mockingbird-sse-event` on the media type is copied into the item schema as `"x-mockingbird-sse-event": "<name>"`.
- **Serving:**
  - Status 200 with `content-type: text/event-stream` and `cache-control: no-cache`.
  - Each item is an event: `event: <x-mockingbird-sse-event or "message">`, `id: <1-based seq>`, `data: <compact JSON>`.
  - Items from `open_stream` come first, then `change_feed` items are interleaved as they arrive.
  - The stream stays open until the client disconnects.
  - The keepalive comment `: keepalive` is sent every `sse_keepalive`.
  - Latency is slept before the first event.
- **A forced non-2xx status:** the unary path (`generate_once` → the declared variant or a P2 problem).
- **Reporting:** one `Report` when the body drops: status 200, `source` = worst over items, `response {"items": n}`.
- **Without the `sse` feature:** 501 problem `Streaming not enabled`.

- [ ] **Step 1: Write failing tests:**
  - `openapi_event_stream_compiles_to_stream_server` (`GET /ticks` → `StreamServer`, item `$ref` Tick, event name `tick`; `POST /ticks` creates a Tick)
  - `sse_streams_n_events` (offline, `X-Mockingbird-Stream-Items: 3` → 3 `event: tick` events whose data validates)
  - `sse_keepalive_comment_sent` (`sse_keepalive` 100 ms)
  - `sse_disconnect_stops_generation` (scripted: 50 lines, chunk delay 20 ms; read 2 events and drop; `chunks_yielded()` stays ≤ 5 and unchanged over a further 200 ms)
  - `sse_emits_state_changes` (after 2 items, a `POST /ticks` in the same session → an event carrying the created tick)
  - `sse_forced_status_returns_error_variant`
- [ ] **Step 2:** Run `cargo test -p mb-import -p mb-protocols`. Expected: FAIL.
- [ ] **Step 3:** Implement with `axum::response::sse::{Sse, Event, KeepAlive}`.
- [ ] **Step 4:** Run `cargo test --workspace --all-features`. Expected: all pass.
- [ ] **Step 5:** Commit: `feat(sse): stream REST text/event-stream operations`.

---

### Task 6: GraphQL importer

**Files:**
- Create: `crates/mb-import/src/graphql.rs`, `fixtures/graphql/{github.graphql,starwars.graphql,starwars-introspection.json}`
- Modify: `crates/mb-import/src/{lib.rs,heuristics.rs}`, `Cargo.toml` (`async-graphql-parser`)
- Test: `crates/mb-import/tests/graphql.rs` + snapshots

**Interfaces:**
- Produces:

```rust
pub enum SourceFormat { OpenApi3, Swagger2, GraphqlSdl, GraphqlIntrospection, Proto, FileDescriptorSet, Wsdl11, Archive }
pub fn sniff(bytes: &[u8]) -> Result<SourceFormat, ImportError>;   // compile() dispatches on it; later tasks fill the remaining arms
// heuristics.rs
pub(crate) fn kind_from_name(name: &str, default: OperationKind) -> OperationKind;
pub(crate) fn resource_from_name(name: &str) -> Option<String>;
```

- **`kind_from_name`:** the prefix is matched case-insensitively and must be followed by an uppercase letter, `_` or the end:
  - `get|list|find|search|query|fetch|read` → `Query`
  - `create|add|insert|new|register` → `Create`
  - `update|set|modify|edit|patch|replace` → `Update`
  - `delete|remove|cancel|destroy` → `Delete`
- **`resource_from_name`:** strips that verb prefix, lowercases, drops one trailing `s` (not `ss`); `None` if empty.

Rules:
- **Fixtures:**
  - `github.graphql` (GitHub-like subset):
    - `interface Node`, `scalar DateTime`, `enum IssueState`, types `User`, `Repository`, `Issue`;
    - `union SearchResult = Repository | Issue | User`, `input CreateIssueInput`;
    - `Query { viewer, repository(owner,name), issue(id: ID!), search(query, first) }`;
    - `Mutation { createIssue(input), updateIssue(id, input), deleteIssue(id) }`;
    - `Subscription { issueAdded(repositoryId) }`;
    - recursion via `User.repositories` / `Repository.owner`.
  - Star Wars schema, as introspection JSON and as equivalent SDL: Character interface, Human/Droid, Episode, `Query.hero/human/droid`, `Mutation.createReview`, `Subscription.reviewAdded`.
- **Detection:**
  - JSON with `/__schema` or `/data/__schema` → introspection;
  - non-JSON text containing `type Query`, `schema {`, `type Mutation` or `type Subscription` → SDL.
  - Introspection is rendered to SDL (types sorted by name; built-in scalars and `__*` types omitted) and compiled as SDL. The payload is always `ProtocolPayload::Graphql { sdl }`.
- **Operations:** one per root field, honouring `schema { query: X }` renames:
  - `id`: `Query.<f>` etc.;
  - `Binding::Graphql`;
  - kind:
    - Query → `Query`
    - Mutation → `kind_from_name(field, Command(Other))`
    - Subscription → `StreamServer`
  - `summary` = the field description;
  - `semantic_hints` = the return type's description;
  - `input.body` = `{type: object, properties: {<arg>: schema}, required: [non-null args without default], additionalProperties: false}`, with `body_required` = any required;
  - `outputs = [Code(200), Some("application/json"), field type schema]`.
- **Types:**
  - object/input types → `$defs/<Name>` with all fields, `required` = the non-null fields, and `additionalProperties: false`;
  - every object def has `"__typename": {"const": "<Name>"}`, required;
  - interface → `oneOf` of its implementors' refs (sorted); union → `oneOf` of member refs;
  - `Int` → integer within i32 bounds;
  - `Float` → number;
  - `String`/`ID` → string;
  - `Boolean` → boolean;
  - a custom scalar → `{"description": "GraphQL scalar <Name>"}`;
  - enum → string `enum`;
  - list → array;
  - a nullable `T` → `{"anyOf": [T, {"type": "null"}]}`;
  - `extend type` merged; directives ignored.
- **`resource_type`:** the lowercased name of the output's named type when it is an object type, else `resource_from_name(field)`.
- **`identity`:**
  - `field: "id"` when the output object has `id`;
  - `input_field`: the argument `id` or `<resource>Id` when present.
- **Bundle:** title = the schema description or `"GraphQL API"`; id = its slug; version `"0"`.
- **Errors:**
  - parse → `Parse("line L, column C: …")`;
  - no query type → `Invalid{pointer: "/schema", ..}`;
  - an unknown type → `Invalid{pointer: "/types/<Type>/fields/<field>", ..}`.

- [ ] **Step 1: Write failing tests:**
  - `github_sdl_snapshot`
  - `starwars_introspection_snapshot`
  - `introspection_and_equivalent_sdl_compile_identically` (operations and defs equal)
  - `root_fields_become_operations` (kinds: `Query.repository` Query, `Mutation.createIssue` Create, `Mutation.updateIssue` Update, `Mutation.deleteIssue` Delete, `Subscription.issueAdded` StreamServer; `Query.issue` identity `input_field "id"`)
  - `scalars_enums_lists_nonnull_map` (exact JSON for an inline SDL)
  - `recursive_types_use_defs`
  - `invalid_sdl_reports_location`
  - `sniff_detects_sdl_and_introspection`
  - `heuristics_kind_and_resource_from_name` (`getUser` Query/user, `Add` Create/None, `settle` Query default, `listOrders` → `order`)
- [ ] **Step 2:** Run `cargo test -p mb-import`. Expected: FAIL.
- [ ] **Step 3:** Implement; review and accept the snapshots.
- [ ] **Step 4:** Run `cargo test -p mb-import`. Expected: all pass.
- [ ] **Step 5:** Commit: `feat(import): GraphQL SDL and introspection compiler`.

---

### Task 7: GraphQL adapter (queries and mutations)

**Files:**
- Create: `crates/mb-protocols/src/graphql/{mod.rs,schema.rs,resolve.rs}`
- Modify: `crates/mb-protocols/src/lib.rs`, `Cargo.toml` (feature `graphql`)
- Test: `crates/mb-protocols/tests/graphql.rs`

**Interfaces:**
- Consumes: `call::{plan, generate_once, report, Report, PlanError}`, `ProtocolSettings` (Task 4); `ProtocolPayload::Graphql` (Task 1); the importer's op-id, `__typename` and nullability conventions (Task 6).
- Produces:

```rust
pub mod graphql {
    pub fn try_router(rt: Arc<MockRuntime>) -> Result<axum::Router, GraphqlError>;
    #[derive(Debug, thiserror::Error)] pub enum GraphqlError {
        #[error("bundle has no GraphQL schema")] MissingSdl, #[error("invalid GraphQL schema: {0}")] Schema(String) }
    pub(crate) fn build_schema(rt: &Arc<MockRuntime>) -> Result<async_graphql::dynamic::Schema, GraphqlError>;
    pub(crate) fn error_code(status: u16) -> &'static str;
}
```

Rules:
- **HTTP surface:**
  - `rt.protocols.graphql_path` takes `POST` (`application/json` `{query, variables, operationName}`, or raw `application/graphql`) and `GET` (`?query&variables&operationName`);
  - a mutation over GET → 405 problem;
  - other paths → 404 problem;
  - a body over 1 MiB → 413 problem;
  - an unparseable HTTP body → 400 problem.
- **Responses:**
  - GraphQL parse and validation errors → 200 `{errors}`;
  - results → 200 `application/json` `{data, errors?, extensions?}`.
- **Schema construction:**
  - a dynamic schema from the payload SDL;
  - `limit_depth(graphql_max_depth)`, `limit_complexity(graphql_max_complexity)`;
  - `disable_introspection()` unless `graphql_introspection`.
- **Root resolvers:**
  - `plan(rt, op, headers)`, with the request headers passed in through request data;
  - `call = OperationCall{operation_id, body: Some(args JSON object), ..Default}`;
  - errors map as follows, all without calling the engine:

| Condition | Result |
|---|---|
| forced non-2xx status | a field error with `extensions {code: error_code(s), status: s}` |
| `PlanError::Rejected` | code by its status, plus `retryAfter` seconds |
| `BadOverride`/`UnknownScenario` | `BAD_USER_INPUT` |

  - otherwise `generate_once`.
- **`error_code` table:**

| HTTP status | `extensions.code` |
|---|---|
| 400 | `BAD_USER_INPUT` |
| 401 | `UNAUTHENTICATED` |
| 403 | `FORBIDDEN` |
| 404 | `NOT_FOUND` |
| 409 | `CONFLICT` |
| 429 | `RATE_LIMITED` |
| other 4xx | `BAD_REQUEST` |
| 5xx and anything else | `INTERNAL_SERVER_ERROR` |

- **Nested resolvers** project from the parent `serde_json::Value` by field name (`FieldValue::owned_any(Value)`; `ctx.parent_value.try_downcast_ref::<Value>()`):
  - an enum → `Value::Enum`;
  - an interface or union → `.with_type(<__typename>)`;
  - a missing field → null.
  - Arguments on nested fields are ignored. Aliases and fragments are handled by async-graphql.
- **Sources and latency:**
  - `extensions.mockingbird = {"sources": {<response key>: "llm"|"repaired"|"fallback"}}`;
  - header `X-Mockingbird-Source` = the worst over the generated fields;
  - the largest `plan.delay` among the root fields is slept once, after reporting, before responding.
- **Reporting:** one `Report` per root field:
  - method `POST`/`GET`, path = `graphql_path`;
  - status 200, or the mapped status for an error;
  - `request {variables, args, headers}`, `response` = the field value.
- Mutations persist through `generate_once`, and root query fields resolve concurrently (async-graphql's default).

- [ ] **Step 1: Write failing tests** (real server on `127.0.0.1:0`, reqwest, `fixtures/graphql/github.graphql`):
  - `query_with_variables_fragments_aliases` (aliased `a`/`b` repository fields with a fragment → each object has exactly the selected keys)
  - `one_engine_call_per_root_field` (scripted; 2 root fields → 2 provider requests)
  - `mutation_then_query_reflects_state` (`createIssue` → `issue(id)` in the same session returns the same title)
  - `introspection_returns_schema`
  - `introspection_disabled_by_setting`
  - `forced_status_maps_to_error_code` (404 → `NOT_FOUND`, `data.repository` null, no provider calls)
  - `query_depth_and_complexity_limits` (16-deep nesting → an error mentioning depth; over 1000 complexity through aliases → an error; no provider calls)
  - `sources_in_extensions_and_header`
  - `get_query_supported_and_get_mutation_rejected`
  - `invalid_query_returns_graphql_errors` (200 with `errors[0].message`)
- [ ] **Step 2:** Run `cargo test -p mb-protocols --test graphql`. Expected: FAIL.
- [ ] **Step 3:** Implement.
- [ ] **Step 4:** Run `cargo test --workspace --all-features`, plus `cargo check -p mb-protocols --no-default-features --features graphql`. Expected: all pass.
- [ ] **Step 5:** Commit: `feat(graphql): dynamic-schema GraphQL adapter`.

---

### Task 8: WebSocket infrastructure and GraphQL subscriptions

**Files:**
- Create: `crates/mb-protocols/src/ws.rs`, `crates/mb-protocols/src/graphql/subscription.rs`
- Modify: `crates/mb-protocols/src/graphql/{mod.rs,schema.rs}`, `Cargo.toml` (feature `websocket = ["axum/ws"]`; dev-dep `tokio-tungstenite`)
- Test: `crates/mb-protocols/tests/{ws.rs,graphql_ws.rs}`

**Interfaces:**
- Consumes: `graphql::build_schema`, `graphql::error_code` (Task 7); `call::{plan, open_stream, change_feed, report}` (Task 4).
- Produces (reused for AsyncAPI channels in P5):

```rust
pub mod ws {
    pub enum Outbound { Text(String), Close { code: u16, reason: String } }
    pub struct ChannelIo { pub protocol: Option<String>, pub incoming: BoxStream<'static, String>,
        pub outgoing: tokio::sync::mpsc::Sender<Outbound> }
    pub struct ChannelConfig { pub subprotocols: Vec<&'static str>, pub keepalive: Duration, pub max_message_bytes: usize }
    pub fn serve_channel<H, F>(upgrade: axum::extract::WebSocketUpgrade, cfg: ChannelConfig, handler: H) -> axum::response::Response
    where H: FnOnce(ChannelIo) -> F + Send + 'static, F: Future<Output = ()> + Send + 'static;
}
```

Rules:
- **`serve_channel`:**
  - picks the first client-offered subprotocol found in `cfg.subprotocols`;
  - text frames go to `incoming`; binary is ignored; an oversized frame → close 1009;
  - pings every `keepalive`;
  - `incoming` ends on close;
  - the socket closes when the handler returns or `outgoing` drops.
- **GraphQL WebSocket handshake:**
  - A `GET` with `Upgrade: websocket` at `graphql_path` → `serve_channel` with `["graphql-transport-ws", "graphql-ws"]`, `ws_keepalive` and a 1 MiB frame limit.
  - `async_graphql::http::WebSocket::new(schema, incoming, protocol)` output maps to `Outbound`.
  - Override headers come from the upgrade request, overlaid by the string-valued `connection_init` payload keys (e.g. `{"x-mockingbird-session": "s1"}`).
- **Subscription root fields** (`SubscriptionField`):
  - `plan` → `open_stream` items, then `change_feed` values, each projected like a query result;
  - the stream runs until the client sends `complete` or closes;
  - a forced non-2xx status → one error, then complete;
  - one `Report` when the stream drops (`response {"items": n}`).

- [ ] **Step 1: Write failing tests:**
  - `serve_channel_negotiates_and_pings` (ws.rs: echo handler, keepalive 100 ms → the client sees a Ping and the negotiated protocol)
  - `oversized_frame_closes_1009`
  - `subscription_transport_ws_receives_n_items` (`connection_init` payload `{"x-mockingbird-stream-items": "3"}` → ack, 3 `next` messages each with only `id` and `title`)
  - `legacy_graphql_ws_protocol_accepted` (`start` → `data` messages)
  - `subscription_emits_state_change` (after the items, `createIssue` over HTTP in the same session → one more `next`)
  - `complete_stops_engine_stream` (scripted chunk delay; `complete` after 1 item; `chunks_yielded()` stops growing)
  - `introspection_query_over_http_lists_subscription_type`
- [ ] **Step 2:** Run `cargo test -p mb-protocols --test ws --test graphql_ws`. Expected: FAIL.
- [ ] **Step 3:** Implement.
- [ ] **Step 4:** Run `cargo test --workspace --all-features`, plus `cargo check -p mb-protocols --no-default-features --features websocket`. Expected: all pass.
- [ ] **Step 5:** Commit: `feat(graphql): subscriptions over graphql-transport-ws and graphql-ws`.

---

### Task 9: Protobuf importer

**Files:**
- Create: `crates/mb-import/src/proto/{mod.rs,schema.rs}`, `fixtures/proto/{helloworld.proto,route_guide.proto}`, and under `fixtures/proto/multi/`:
  - `orders/v1/orders.proto`, which imports `common/v1/money.proto` and `google/protobuf/timestamp.proto`;
  - `common/v1/money.proto`.
- Modify: `crates/mb-import/src/lib.rs`, `Cargo.toml` (protox, prost, prost-types, prost-reflect)
- Test: `crates/mb-import/tests/proto.rs` + snapshots

**Interfaces:**
- Consumes: `kind_from_name`, `resource_from_name`, `sniff`, `SourceFormat` (Task 6).
- Produces:

```rust
#[derive(Clone, Debug, PartialEq)] pub struct SourceFile { pub path: String, pub bytes: Vec<u8> }   // '/'-separated relative path
pub fn compile_files(files: &[SourceFile]) -> Result<Bundle, ImportError>;
// this task: any .proto → compile all .proto files; exactly one file → compile(bytes); else Unsupported. Tasks 11 and 13 extend it.
```

Rules:
- **Sources and compilation:**
  - A single text `.proto` passed to `compile()` (sniff: `syntax = "proto`, or both `service ` and `message `) is named `main.proto`.
  - A binary `FileDescriptorSet` is the last sniff resort: it decodes with ≥ 1 file and ≥ 1 service.
  - protox `Compiler::with_file_resolver(ChainFileResolver[in-memory upload set, GoogleFileResolver])` with `include_imports(true)` and `include_source_info(true)`.
- **Errors:**
  - an unresolved import → `Invalid{pointer: "<file>", message: "import \"<x>\" not found; upload a .zip or .tar.gz containing it"}`;
  - other protox errors → `Parse("<file>:<line>:<col>: …")`.
- **Payload:** `Grpc { file_descriptor_set }` = the encoded set including imports.
- **Operations:** one per method of each service in the non-import files, services sorted by full name, methods in declaration order:
  - `Binding::Grpc`;
  - kind:
    - client+server streaming → `StreamBidi`
    - client streaming → `StreamClient`
    - server streaming → `StreamServer`
    - unary → `kind_from_name(method, Query)`
  - `input.body = {"$ref": "#/$defs/<request full name>"}` with `body_required: true`;
  - `outputs = [Code(200), Some("application/grpc"), $ref response]`;
  - `summary` = the leading comment;
  - `resource_type = resource_from_name(method)`;
  - identity: `field "id"` when the response (item) message has `id`, and `input_field` = the request field JSON name `id` or `<resource>Id`.
- **Message and enum defs:**
  - `$defs` keyed by full name (`routeguide.Point`);
  - properties use the field's JSON name (prost-reflect's default serialization);
  - `additionalProperties: false`, no `required` (proto3);
  - a oneof keeps its fields as optional properties and adds `"x-oneof": {"<oneof>": [json names]}` on the message; synthetic proto3-`optional` oneofs are ignored;
  - an enum → a string `enum` of value names;
  - `repeated` → array;
  - `map<K,V>` → `{type: object, additionalProperties: V}`.
- **Scalars:**

| Proto type | JSON Schema |
|---|---|
| int32, sint32, sfixed32 | integer within i32 bounds |
| uint32, fixed32 | integer within u32 bounds |
| int64, sint64, sfixed64 | `{type: string, format: int64, pattern: "^-?[0-9]+$"}` |
| uint64, fixed64 | `format: uint64`, `pattern: "^[0-9]+$"` |
| float, double | number |
| bool | boolean |
| string | string |
| bytes | `{type: string, format: byte, contentEncoding: base64}` |

- **Well-known types:**

| Type | JSON Schema |
|---|---|
| Timestamp | string `date-time` |
| Duration | `{type: string, format: proto-duration, pattern: …}` |
| Empty | `{type: object, additionalProperties: false}` |
| Struct | object |
| Value | `{}` |
| ListValue | array |
| NullValue | null |
| FieldMask | string |
| Any | object with required string `@type` |
| wrappers | `anyOf [mapped scalar, null]` |

- **Bundle:** title = the package (else the first service); version `"0"`.

- [ ] **Step 1: Write failing tests:**
  - `route_guide_snapshot`
  - `helloworld_snapshot`
  - `streaming_flags_map_to_kinds` (GetFeature Query, ListFeatures StreamServer, RecordRoute StreamClient, RouteChat StreamBidi; `Greeter.SayHello` Query)
  - `scalar_and_wkt_mapping` (exact schemas, including `x-oneof`)
  - `multi_file_imports_resolve` (`compile_files` over `fixtures/proto/multi` → defs contain `common.v1.Money`)
  - `missing_import_is_invalid`
  - `file_descriptor_set_compiles_same_as_source` (protox-encoded route_guide set → equal operations and defs)
  - `prost_reflect_json_validates_against_schema` (populated `routeguide.Feature` and an int64/bytes/enum/Timestamp/map message serialized by prost-reflect serde → validate against `bundle.with_reachable_defs`)
- [ ] **Step 2:** Run `cargo test -p mb-import --test proto`. Expected: FAIL.
- [ ] **Step 3:** Implement.
- [ ] **Step 4:** Run `cargo test -p mb-import`. Expected: all pass.
- [ ] **Step 5:** Commit: `feat(import): protobuf compiler for .proto and FileDescriptorSet`.

---

### Task 10: gRPC adapter, reflection and health

**Files:**
- Create: `crates/mb-protocols/src/grpc/{mod.rs,codec.rs,service.rs,server.rs,status.rs}`
- Modify: `crates/mb-protocols/src/lib.rs`, `Cargo.toml` (feature `grpc`: tonic, tonic-reflection, tonic-health, prost, prost-types, prost-reflect, tokio-stream, `axum/http2`)
- Test: `crates/mb-protocols/tests/grpc.rs` (fixtures from Task 9)

**Interfaces:**
- Consumes: `call::{plan, generate_once, open_stream, report, Report, PlanError}`, `ProtocolSettings.grpc_max_message_bytes` (Task 4); `ProtocolPayload::Grpc`, `Binding::grpc_path` (Task 1); `x-oneof` (Task 9).
- Produces:

```rust
pub mod grpc {
    pub struct DynamicCodec { /* encode + decode MessageDescriptor */ }             // tonic::codec::Codec<Encode = DynamicMessage, Decode = DynamicMessage>
    impl DynamicCodec { pub fn new(encode: MessageDescriptor, decode: MessageDescriptor) -> Self }
    // server: new(output, input); client (tests): new(input, output)
    pub struct GrpcMock;
    impl GrpcMock { pub fn new(rt: Arc<MockRuntime>) -> Result<Self, GrpcError>; pub fn name(&self) -> &str;
        pub fn services(&self) -> Vec<String>; /* full names, sorted */ pub fn descriptor_set(&self) -> &[u8]; }
    #[derive(Debug, thiserror::Error)] pub enum GrpcError {
        #[error("bundle has no protobuf descriptors")] MissingDescriptors,
        #[error("invalid descriptors: {0}")] Descriptors(String),
        #[error("service {service} is already served by mock {holder}")] ServiceCollision { service: String, holder: String } }
    pub struct GrpcServer;
    impl GrpcServer { pub fn new() -> Arc<Self>;
        pub async fn publish(&self, mocks: Vec<Arc<GrpcMock>>) -> Result<(), GrpcError>;   // atomic swap; collision → Err, nothing changed
        pub fn router(self: &Arc<Self>) -> axum::Router;
        pub fn services(&self) -> Vec<(String, String)>; }                                  // (service, mock), sorted
    pub fn grpc_code_for_status(status: u16) -> tonic::Code;
}
```

Rules:
- **`router()` dispatch** by request path:
  - `grpc.health.v1.Health/*` → tonic-health;
  - `grpc.reflection.v1.ServerReflection/*` and `grpc.reflection.v1alpha.ServerReflection/*` → tonic-reflection;
  - `/<service>/<method>` of a published mock → the mock;
  - anything else → `UNIMPLEMENTED`;
  - a non-`application/grpc*` content type → 415.
- **`publish`:**
  - rebuilds reflection from the union of the mocks' descriptor sets (deduplicated by file name, first wins) plus `tonic_health::pb::FILE_DESCRIPTOR_SET`;
  - health:
    - `""` and each service → `SERVING`;
    - services no longer published → `NOT_SERVING`.
- **Every call:**
  - request metadata is the HTTP headers;
  - `plan(rt, op, headers)` gives the session from `x-mockingbird-session`;
  - a request message → JSON via prost-reflect serde with default options;
  - decode and encode limits = `grpc_max_message_bytes`.
- **Response encoding:**
  - JSON → `DynamicMessage::deserialize`, after keeping only the first set member of each `x-oneof` group (declared order);
  - a failure → the engine fallback for the success variant, then retry;
  - a second failure → `INTERNAL`.
- **Per call kind:**
  - Unary: `generate_once`.
  - Server streaming: `open_stream`; one message per item; then `OK`.
  - Client streaming: collect every message; `call.body` = the last one; `request_stream` = the last 20; `open_stream` with `stream_items = Some(1)`; the first item is the single response.
  - Bidi: per inbound message `i`, `open_stream` with `stream_items = Some(1)`, `call.body = msg_i` and `request_stream` = up to 20 earlier messages. Replies are sent in order. `OK` after the client half-closes and the last reply is sent.
- **Errors, all without an engine call:**
  - a forced non-2xx status → `Status::new(grpc_code_for_status(s), "forced by x-mockingbird-status: <s>")`;
  - `Rejected` → the code for its status;
  - `BadOverride`/`UnknownScenario` → `INVALID_ARGUMENT`.
- **Metadata and latency:**
  - `x-mockingbird-source` goes in the unary response headers and in the trailers of every successful call (streams: the worst over items);
  - latency is slept before the response (unary) or the first message.
- **Status table:**

| HTTP status | gRPC code |
|---|---|
| 400 | INVALID_ARGUMENT |
| 401 | UNAUTHENTICATED |
| 403 | PERMISSION_DENIED |
| 404 | NOT_FOUND |
| 409 | ALREADY_EXISTS |
| 412 | FAILED_PRECONDITION |
| 416 | OUT_OF_RANGE |
| 429 | RESOURCE_EXHAUSTED |
| 499 | CANCELLED |
| 501 | UNIMPLEMENTED |
| 503 | UNAVAILABLE |
| 504 | DEADLINE_EXCEEDED |
| other 4xx | INVALID_ARGUMENT |
| anything else | INTERNAL |

- **Reporting:** one `Report` per call:
  - method `POST`, path = `grpc_path`;
  - status 200 for OK, else the forced/plan HTTP status (`500` for INTERNAL);
  - response = the JSON (unary) or `{"items": n}`.

- [ ] **Step 1: Write failing tests** (`axum::serve` with http2 on `127.0.0.1:0`; a tonic `Channel` + `tonic::client::Grpc` with `DynamicCodec`):
  - `unary_get_feature` (offline → a valid `routeguide.Feature`; trailer `x-mockingbird-source: fallback`)
  - `server_streaming_list_features_yields_n` (metadata `x-mockingbird-stream-items: 4` → 4 messages, then OK)
  - `client_streaming_record_route` (3 Points → 1 RouteSummary; the scripted stream request's user message has `<request_stream>` with 3 items)
  - `bidi_route_chat_replies_per_message`
  - `forced_status_maps_code` (`x-mockingbird-status: 404` → `Code::NotFound`, no provider call)
  - `unknown_service_is_unimplemented`
  - `oversized_message_rejected` (5 MiB bytes field → `Code::OutOfRange`)
  - `reflection_lists_services` (v1 and v1alpha `ServerReflectionClient` → `routeguide.RouteGuide`, `grpc.health.v1.Health`)
  - `health_reports_serving` (`""` and the service → SERVING; unknown → `NotFound`)
  - `publish_rejects_duplicate_services`
  - `grpc_code_table` (unit)
- [ ] **Step 2:** Run `cargo test -p mb-protocols --test grpc`. Expected: FAIL.
- [ ] **Step 3:** Implement. The server side uses `tonic::server::Grpc::new(codec)` with `unary`/`server_streaming`/`client_streaming`/`streaming` and `.max_{de,en}coding_message_size`.
- [ ] **Step 4:** Run `cargo test --workspace --all-features`, plus `cargo check -p mb-protocols --no-default-features --features grpc`. Expected: all pass.
- [ ] **Step 5:** Commit: `feat(grpc): dynamic gRPC adapter with reflection and health`.

---

### Task 11: WSDL 1.1 / XSD importer

**Files:**
- Create: `crates/mb-import/src/wsdl/{mod.rs,xml.rs,xsd.rs}` and these fixtures:
  - `fixtures/wsdl/calculator.wsdl`: document/literal; Add/Subtract/Divide; SOAP 1.1 + 1.2 ports at the same address `/calculator.asmx`; a `DivideByZero` fault.
  - `fixtures/wsdl/accounts-rpc.wsdl`: rpc/literal; GetAccount/CreateAccount/DeleteAccount; `Account` with an `id` attribute, an unbounded `tags`, a `status` enumeration and a `dateTime`.
  - `fixtures/wsdl/external/{service.wsdl,types.xsd}`.
- Modify: `crates/mb-import/src/lib.rs` (sniff; `compile_files`: a single `.wsdl` plus its `.xsd` files), `Cargo.toml` (quick-xml)
- Test: `crates/mb-import/tests/wsdl.rs` + snapshots

**Interfaces:**
- Consumes: `SourceFile`, `compile_files` (Task 9); `kind_from_name`, `resource_from_name` (Task 6).
- Produces:
  - `SourceFormat::Wsdl11`;
  - the XML annotations Task 12 relies on, emitted on every object schema:
    - `"x-xml-root": {"name", "namespace"}`: on input body and output schemas only;
    - `"x-xml-order": [names]`;
    - `"x-xml-namespace": "<target ns>"`;
    - `"x-xml-qualified": bool` (`elementFormDefault`);
    - `"x-xml-choice": [names]`: on choice groups.
  - Attribute properties are named `@<name>`; simple content is `#text`.

Rules:
- **Detection and safety:**
  - a root `definitions` in `http://schemas.xmlsoap.org/wsdl/` → WSDL 1.1;
  - a root `description` in `http://www.w3.org/ns/wsdl` → `Unsupported("WSDL 2.0 is not supported; convert to WSDL 1.1")`;
  - a DOCTYPE anywhere → `Invalid{pointer: "/", message: "DOCTYPE is not allowed"}`;
  - no entity expansion.
- **`xml.rs`:** a minimal namespace-resolving DOM (`Element { name: QName, attrs, children, text }`).
- **Ports:**
  - Per `wsdl:service`, the first port bound to SOAP 1.1 (`…/wsdl/soap/`) or SOAP 1.2 (`…/wsdl/soap12/`) gives `port` and `address_path` (the path of `soap:address@location`, default `/<service>`).
  - `version` = `Soap11` if the service has any SOAP 1.1 port, else `Soap12`.
  - `soap_action` comes from that binding's `soap:operation@soapAction` (empty → `None`).
  - Style: `soap:operation@style`, else `soap:binding@style`, else document.
- **Operations:** one per portType operation of the chosen binding:
  - `kind_from_name(op, Query)`;
  - `resource_type = resource_from_name(op)`;
  - identity: `field "id"` (or `"@id"`) when the output has it; `input_field` likewise in the input.
- **Message shapes:**
  - document/literal: body = the schema of the element of the single input part (several parts → `Invalid`), with `x-xml-root` = that element; the output likewise.
  - rpc/literal: wrapper `<operation>` / `<operation>Response` in `soap:body@namespace`, with one property per part in part order.
  - `body_required: true`.
- **Outputs:**
  - `Code(200)`, `"text/xml"`;
  - the first `wsdl:fault` → `Code(500)`, `"text/xml"`, with the fault element's schema.
- **XSD → JSON Schema:**
  - Named complexTypes and top-level elements become `$defs/<local>` (`<local>_2` … on collision).
  - Groups:
    - `sequence`/`all` → properties plus `x-xml-order`;
    - `choice` → optional properties plus `x-xml-choice`.
  - Occurrence:
    - `minOccurs="0"` → not required;
    - `maxOccurs > 1` or `unbounded` → array with `minItems`/`maxItems`;
    - `nillable` → `anyOf [T, null]`.
  - Attributes → `@name` (required if `use="required"`).
  - `simpleContent` → `#text` plus attributes.
  - `complexContent extension` → the base properties first, then its own (flattened).
  - simpleType restrictions → `enum`/`pattern`/`length`/`minLength`/`maxLength`/`min|maxInclusive`/`min|maxExclusive`; `list`/`union` → string.
  - Built-ins:

| XSD types | JSON Schema |
|---|---|
| string, normalizedString, token, anyURI, QName, NCName, ID, language | string |
| integer family (integer, long, int, short, byte, unsigned*, positive/negative/nonNegative/nonPositive) | integer, with bounds for int/short/byte/unsigned*/positive/nonNegative |
| decimal, float, double | number |
| boolean | boolean |
| date | string, `format: date` |
| dateTime | string, `format: date-time` |
| time | string, `format: time` |
| duration | string, `format: duration` |
| base64Binary | string, `format: byte` |
| hexBinary | string, `format: hex` with its pattern |
| anyType | `{}` |

- **Imports:**
  - `xs:import`/`xs:include` `schemaLocation` resolves relative to the importing file within the upload set;
  - missing → `Invalid{pointer: "<file>", message: "schemaLocation \"<loc>\" is not in the upload; external schemas are not fetched"}`;
  - imports without `schemaLocation` (SOAP encoding, XSD) are ignored.
- **Payload and bundle:** `Soap { wsdl }` = the WSDL text; title = `definitions@name`, else the first service; version `"0"`.

- [ ] **Step 1: Write failing tests:**
  - `calculator_doc_literal_snapshot`
  - `accounts_rpc_literal_snapshot`
  - `soap_bindings_versions_and_actions` (Add: `Soap11`, `Some("http://tempuri.org/Add")`, `/calculator.asmx`)
  - `xsd_mapping_exact` (inline: order, `minOccurs 0`, unbounded → array, `@currency`, enumeration, `date-time`, `#text`)
  - `fault_becomes_500_variant`
  - `wsdl20_unsupported`
  - `doctype_rejected`
  - `external_import_requires_upload` (single file → `Invalid` naming `types.xsd`; `compile_files` with both → ok)
- [ ] **Step 2:** Run `cargo test -p mb-import --test wsdl`. Expected: FAIL.
- [ ] **Step 3:** Implement.
- [ ] **Step 4:** Run `cargo test -p mb-import`. Expected: all pass.
- [ ] **Step 5:** Commit: `feat(import): WSDL 1.1 and XSD compiler`.

---

### Task 12: SOAP adapter

**Files:**
- Create: `crates/mb-protocols/src/soap/{mod.rs,envelope.rs,xml_json.rs}`
- Modify: `crates/mb-protocols/src/lib.rs`, `Cargo.toml` (feature `soap`: quick-xml)
- Test: `crates/mb-protocols/tests/soap.rs`

**Interfaces:**
- Consumes: `call::{plan, generate_once, report, Report, PlanError}`, `MountPrefix` (Task 4); `Binding::Soap`, `ProtocolPayload::Soap` (Task 1); the `x-xml-*` annotations (Task 11).
- Produces:

```rust
pub mod soap {
    pub fn try_router(rt: Arc<MockRuntime>) -> Result<axum::Router, SoapError>;
    #[derive(Debug, thiserror::Error)] pub enum SoapError { #[error("bundle has no WSDL")] MissingWsdl,
        #[error("contract has no SOAP operations")] NoOperations }
    /// The first child of the envelope's Body, converted per `schema_with_defs`.
    pub fn envelope_body_to_json(envelope: &str, schema_with_defs: &Value) -> Result<Value, String>;
    pub(crate) fn json_to_xml(value: &Value, schema_with_defs: &Value) -> Result<String, String>;   // uses x-xml-root
}
```

Rules:
- **Routes:** per distinct `address_path`:
  - `POST` → dispatch;
  - `GET ?wsdl` (or `?WSDL`) → the stored WSDL with every `soap:address`/`soap12:address` location set to `http://<Host header><MountPrefix><address_path>`, `text/xml; charset=utf-8`.
- **Request checks:**
  - version from the `Content-Type`: `text/xml` → 1.1, `application/soap+xml` → 1.2, else a 415 Client fault (1.1 shape);
  - the response uses the request's version;
  - a body over 4 MiB → 413;
  - a DOCTYPE → a Client/Sender fault `DOCTYPE is not allowed`, with no engine call.
- **Dispatch:**
  - 1.1: the `SOAPAction` header (quotes stripped); 1.2: the `action=` content-type parameter. Either is matched against `soap_action`.
  - Else the first Body child's QName against the op's input `x-xml-root`.
  - Else a Client fault `unknown operation`.
- **Decode and plan:**
  - The Body child → JSON (`@attr`, `#text`; arrays, numbers and booleans by schema), validated against `input.body`. Invalid → a Client fault listing the errors, with no engine call.
  - `plan` reads overrides from the HTTP headers; the SOAP `Header` element is ignored.
- **Responses:**
  - Success: `generate_once` → `json_to_xml`, with element order from `x-xml-order` (unlisted keys appended in sorted order), `@` → attributes, arrays → repeated elements, `null` → `xsi:nil="true"`, and namespaces/qualification from the annotations.
  - A forced non-2xx status:
    - op declares `Code(500)` → `generate_once` with forced 500 → a Fault with `<detail>` (1.1) / `<env:Detail>` (1.2);
    - otherwise a Fault without detail.
- **Fault shapes:**
  - 1.1: `faultcode` `soap:Client` (4xx) / `soap:Server` (5xx), plus `faultstring`; HTTP 500.
  - 1.2: `env:Code/env:Value` `env:Sender` (HTTP 400) / `env:Receiver` (HTTP 500), plus `env:Reason/env:Text xml:lang="en"`.
- **Headers and reporting:**
  - `X-Mockingbird-Source` on every generated response;
  - one `Report` per call (`request {headers, body: decoded JSON | "[undecodable]"}`).

- [ ] **Step 1: Write failing tests** (reqwest, raw XML; calculator and accounts fixtures):
  - `soap11_add_round_trip` (SOAPAction `"http://tempuri.org/Add"` → 200 `text/xml`; `envelope_body_to_json` of the response validates against the output schema)
  - `soap12_dispatch_by_action_param`
  - `dispatch_by_body_qname_without_action`
  - `invalid_input_returns_client_fault_without_llm`
  - `forced_500_returns_fault_with_detail` (1.1: HTTP 500, `soap:Server`, the detail validates against the fault schema; 1.2: `env:Receiver`)
  - `doctype_request_rejected` (a billion-laughs envelope → a fault containing `DOCTYPE`; no provider call)
  - `wsdl_served_with_rewritten_address` (`MountPrefix("/calc")`, `Host: 127.0.0.1:1234` → `location="http://127.0.0.1:1234/calc/calculator.asmx"`)
  - `rpc_literal_state_round_trip` (CreateAccount, then GetAccount by its id in the same session → same name)
- [ ] **Step 2:** Run `cargo test -p mb-protocols --test soap`. Expected: FAIL.
- [ ] **Step 3:** Implement.
- [ ] **Step 4:** Run `cargo test --workspace --all-features`, plus `cargo check -p mb-protocols --no-default-features --features soap`. Expected: all pass.
- [ ] **Step 5:** Commit: `feat(soap): SOAP 1.1/1.2 adapter with faults and ?wsdl`.

---

### Task 13: Archives, multi-file uploads, admin upload and CLI

**Files:**
- Create: `crates/mb-import/src/archive.rs`
- Modify:
  - `crates/mb-import/src/lib.rs`, `Cargo.toml` (zip, tar, flate2)
  - `crates/mb-admin/src/contracts.rs`
  - `crates/mockingbird/src/{client.rs,main.rs,reconcile.rs}`
- Test: `crates/mb-import/tests/archive.rs`, `crates/mb-admin/tests/api.rs`, `crates/mockingbird/tests/{client.rs,config.rs}`

**Interfaces:**
- Consumes: `SourceFile`, `compile_files` (Tasks 9, 11); `OperationSummary.protocol/target` (Task 1).
- Produces:

```rust
pub const MAX_ARCHIVE_BYTES: usize = 20 * 1024 * 1024;
pub const MAX_ARCHIVE_FILES: usize = 500;
pub const MAX_ARCHIVE_UNPACKED_BYTES: u64 = 64 * 1024 * 1024;
pub fn unpack_archive(bytes: &[u8]) -> Result<Vec<SourceFile>, ImportError>;   // zip (PK\x03\x04) or gzip'd tar (1f 8b); sorted by path
pub fn pack_dir(dir: &Path) -> Result<Vec<u8>, ImportError>;                  // deterministic tar.gz
pub fn read_source(path: &Path) -> Result<Vec<u8>, ImportError>;              // a file's bytes, or pack_dir for a directory
// mb-admin contracts.rs: MAX_CONTRACT_BYTES becomes 20 MiB; CONTRACT_MEDIA_TYPES gains the types below
```

Rules:
- **`unpack_archive`:**
  - Entries are rejected with `Invalid{pointer: "<entry>", ..}` when they are absolute, contain `..`, `\`, NUL or a drive letter, or are a link or device. Directories are skipped and a leading `./` is stripped.
  - More than 500 files → `Invalid`.
  - Bytes actually read (through a limiting reader) above 64 MiB → `Invalid`.
  - When every entry shares one top-level directory, it is stripped.
  - Nothing touches the filesystem.
- **`pack_dir`:** sorted relative paths; hidden entries skipped; mtime 0, mode 0644, uid/gid 0; gzip header mtime 0. Packing twice gives identical bytes, so reconcile's digest is stable.
- **`compile` and `compile_files`:**
  - `compile(bytes)`: sniffed archive → `unpack_archive` → `compile_files`; `source_digest` = sha256 of the archive bytes.
  - `compile_files` dispatch:
    1. any `.proto` → proto;
    2. else exactly one `.wsdl` (+ `.xsd`) → WSDL (several `.wsdl` → `Invalid`);
    3. else `.graphql|.graphqls|.gql` → their SDL joined by `\n` in path order;
    4. else exactly one file → `compile(bytes)`;
    5. else `Unsupported("archive contains no recognised contract")`.
- **Admin:**
  - New media types: `application/graphql`, `text/x-protobuf`, `application/x-protobuf`, `application/xml`, `text/xml`, `application/wsdl+xml`, `application/zip`, `application/gzip`, `application/octet-stream`. Still none that is CORS-safelisted: not `text/plain`.
  - The utoipa docs list them, with "at most 20 MiB".
  - `GET …/source` content type sniffs: zip → `application/zip`; gzip → `application/gzip`; `<` → `application/xml`; `{` → `application/json`; GraphQL or proto text → `text/plain; charset=utf-8`; non-UTF-8 → `application/octet-stream`; else `application/yaml`.
- **CLI `import <path>`:**
  - uses `read_source`;
  - media type by extension:
    - `.graphql/.graphqls/.gql` → `application/graphql`
    - `.proto` → `text/x-protobuf`
    - `.pb/.desc` → `application/x-protobuf`
    - `.wsdl/.xsd/.xml` → `application/xml`
    - `.zip` → `application/zip`
    - `.tgz/.tar.gz` and directories → `application/gzip`
    - else the P2 rule.
- **CLI `validate`:**
  - uses `read_source`;
  - prints `{id:<32} {op.target()}`, plus ` [<kind.label()>]` for the stream kinds (Task 1 helpers).
- **Reconcile** reads contracts with `read_source`, so `mocks.<m>.contract` may be a directory.

- [ ] **Step 1: Write failing tests:**
  - `zip_slip_rejected` (`../evil.proto`, `/abs.proto`, `a\\b.proto` → each `Invalid` naming the entry)
  - `symlink_entry_rejected` (tar)
  - `too_many_files_rejected` (501)
  - `unpacked_size_limit` (a zip of 65 MiB of zeros)
  - `multi_file_proto_zip_compiles` (zip built in the test from `fixtures/proto/multi`)
  - `tar_gz_wsdl_with_xsd_compiles`
  - `pack_dir_is_deterministic`
  - `single_top_level_dir_stripped`
  - Admin:
    - `upload_graphql_proto_wsdl_and_archive` (each → 201 with an operations count)
    - `upload_zip_slip_422`
    - `upload_over_20_mib_413`
    - `grpc_contract_detail_lists_protocol_target_kind` (`grpc`, `/routeguide.RouteGuide/ListFeatures`, `stream-server`)
  - CLI:
    - `import_directory_uploads_archive`
    - `validate_handles_proto_and_wsdl` (stdout contains `/routeguide.RouteGuide/ListFeatures [stream-server]`)
  - Config: `file_config_contract_directory` (reconcile twice → one version)
- [ ] **Step 2:** Run `cargo test -p mb-import -p mb-admin -p mockingbird`. Expected: FAIL.
- [ ] **Step 3:** Implement.
- [ ] **Step 4:** Run `cargo test --workspace --all-features`. Expected: all pass.
- [ ] **Step 5:** Commit: `feat(import): zip/tar.gz contract archives; admin and CLI multi-format upload`.

---

### Task 14: Registry, runtime, config and the gRPC listener

**Files:**
- Modify:
  - `crates/mockingbird/src/{registry.rs,runtime.rs,file_config.rs,reconcile.rs,main.rs}`
  - `crates/mockingbird/src/config.rs` (additive only)
  - `crates/mockingbird/Cargo.toml` (`axum` `http2`)
  - `crates/mb-store/src/model.rs` (ScenarioRules)
  - `crates/mb-admin/src/scenarios.rs`
  - `docs/examples/mockingbird.yaml`
- Test: `crates/mockingbird/tests/{registry.rs,config.rs,protocols_e2e.rs}`, `crates/mb-admin/tests/api.rs`

**Interfaces:**
- Consumes:
  - `graphql::try_router` (Task 7);
  - `soap::try_router` (Task 12);
  - `GrpcMock`, `GrpcServer`, `GrpcError::ServiceCollision` (Task 10);
  - `http_rest::try_router`, `MountPrefix`, `ProtocolSettings`, `ScenarioRuntime.stream_*` (Task 4);
  - `StreamConfig` (Task 3);
  - `Bundle::protocol` (Task 1);
  - `read_source` (Task 13).
- Produces:

```rust
// mb-store model.rs: ScenarioRules gains (JSON column; no migration)
#[serde(default, skip_serializing_if = "Option::is_none")] pub stream_items: Option<u32>,
#[serde(default, skip_serializing_if = "Option::is_none")] pub stream_item_interval_ms: Option<u64>,
// mb-admin scenarios.rs: ScenarioView and PutScenario gain the same two optional fields
// file_config.rs
ServerConfig { .., pub grpc_port: u16 /*50051*/, #[serde(skip)] pub grpc_port_explicit: bool }
FileConfig { .., pub protocols: ProtocolsConfig }
#[serde(deny_unknown_fields, default)] pub struct ProtocolsConfig { pub graphql: GraphqlConfig, pub stream: StreamFileConfig, pub sse: SseConfig, pub grpc: GrpcConfig }
pub struct GraphqlConfig { pub path: String /*"/graphql"*/, pub introspection: bool /*true*/, pub max_depth: usize /*15*/, pub max_complexity: usize /*1000*/ }
pub struct StreamFileConfig { pub default_items: usize /*5*/, pub max_items: usize /*50*/, #[serde(with = "humantime_serde")] pub item_interval: Duration /*0s*/ }
pub struct SseConfig { #[serde(with = "humantime_serde")] pub keepalive: Duration /*15s*/ }
pub struct GrpcConfig { pub max_message_bytes: usize /*4194304*/ }
impl ProtocolsConfig { pub fn settings(&self) -> (ProtocolSettings, StreamConfig) }
FileScenario { .., pub stream_items: Option<u32>, #[serde(with = "humantime_serde")] pub stream_item_interval: Option<Duration> }
// apply_env_from: MB_GRPC_PORT → server.grpc_port (+ grpc_port_explicit)
// config.rs (additive)
ServeArgs { .., /// gRPC port [default: 50051]. #[arg(long)] pub grpc_port: Option<u16> }   // apply_flags sets port + explicit
App { .., pub grpc: axum::Router }
pub async fn bind_grpc(addr: SocketAddr, explicit: bool) -> anyhow::Result<Option<TcpListener>>;   // same policy as bind_admin
// runtime.rs
RuntimeDeps { .., pub protocols: ProtocolSettings, pub stream: StreamConfig }   // EngineConfig.stream = deps.stream; rt.protocols = deps.protocols
// registry.rs
impl MockRegistry { pub fn grpc(&self) -> Arc<GrpcServer>; }
```

Rules:
- **Mounting** by `bundle.protocol()`:
  - Http → `http_rest::try_router`;
  - Graphql → `graphql::try_router`;
  - Soap → `soap::try_router`;
  - Grpc → `GrpcMock::new`.
  - A gRPC mock holds no prefix; its `route_prefix` is ignored for dispatch.
- **gRPC set:**
  - After every publish of the mounted set, the registry calls `grpc.publish(...)`.
  - A gRPC service name already held by a mock that sorts first by name → the later mock is skipped and recorded in `failed_mocks` with the `ServiceCollision` text, mirroring the prefix rule.
  - Deleting a mock removes its services.
- **Dispatch** inserts `MountPrefix(prefix)` into request extensions before forwarding.
- **`scenario_runtime`** maps `stream_items` and `stream_item_interval_ms`.
- **Reconcile** maps `FileScenario.stream_*`.
- **The `protocols:` section** is read at startup (documented like `api_secret_refs`).
- **`build_app_with`:** `App.grpc = registry.grpc().router()`; logs `serving <mock> gRPC <service> at <host>:<grpc_port>` for each published service.
- **`main`:** binds `bind_grpc(SocketAddr(host, grpc_port), grpc_port_explicit)` next to the admin listener and serves `app.grpc` with `axum::serve` (h2c) and the same graceful shutdown.
- Update every `RuntimeDeps { .. }` literal in the tests.

- [ ] **Step 1: Write failing tests:**
  - registry:
    - `graphql_mock_served_at_prefix` (`/gh/graphql`)
    - `soap_wsdl_uses_mount_prefix`
    - `grpc_mock_published_to_grpc_server`
    - `grpc_service_collision_skips_later_mock`
    - `grpc_mock_removed_on_delete`
  - config:
    - `grpc_port_precedence` (file 6000 < `MB_GRPC_PORT` 6001 < `--grpc-port` 6002)
    - `busy_default_grpc_port_is_skipped`
    - `protocols_section_defaults_and_overrides`
    - `scenario_stream_settings_reach_runtime` (a file scenario `stream_items: 2` → SSE with `X-Mockingbird-Scenario` yields 2 events)
    - the updated `loads_full_example_config`
  - admin: `scenario_stream_fields_round_trip`
  - `protocols_e2e.rs`: `build_app` offline with a config of four mocks (events, github, route_guide, calculator); serve `data` and `grpc` on ephemeral ports. One real call each: a GraphQL query, a tonic unary GetFeature, a SOAP 1.1 Add, and 2 SSE events.
- [ ] **Step 2:** Run `cargo test -p mockingbird -p mb-admin -p mb-store`. Expected: FAIL.
- [ ] **Step 3:** Implement.
- [ ] **Step 4:** Run `cargo test --workspace --all-features && scripts/test-postgres.sh`. Expected: all pass. A scenario with stream fields round-trips on Postgres too.
- [ ] **Step 5:** Commit: `feat(runtime): mount GraphQL, gRPC, SOAP and SSE mocks; gRPC listener`.

---

### Task 15: Documentation, image and final verification

**Files:**
- Modify: `README.md`, `Dockerfile` (`EXPOSE 8080 9090 50051`), `.github/workflows/ci.yml` (`cargo clippy`/`cargo test` with `--all-features`)
- Test: none new. This task verifies the workspace.

**Interfaces:**
- Consumes: everything above. Produces nothing new.

README content:
- **A Protocols section:** a table of format → detection → how it is served:
  - REST/SSE at the prefix;
  - GraphQL at `<prefix>/graphql` (HTTP + WS);
  - gRPC on `:50051` with reflection and health;
  - SOAP at `<prefix><address path>` with `?wsdl`.
- **Examples:**
  - `curl` GraphQL;
  - `grpcurl -plaintext 127.0.0.1:50051 list` and a unary call;
  - a SOAP 1.1 `curl`;
  - `curl -N` SSE.
- **Streaming:**
  - `X-Mockingbird-Stream-Items`;
  - scenario `stream_items`/`stream_item_interval`;
  - the `protocols:` section;
  - the open-stream semantics: SSE and subscriptions stay open for state changes, while gRPC server streams complete.
- **Override mapping per protocol:** the gRPC status table and the GraphQL `extensions.code` table.
- **Archive uploads:** limits and the `import --as m dir/` command.
- **Known limits:**
  - WSDL 2.0 is unsupported;
  - external XSDs are never fetched;
  - complex XSD patterns may yield faker output that does not validate (logged);
  - stream token usage is not metered.

- [ ] **Step 1:** Write the README sections and the Dockerfile/CI edits.
- [ ] **Step 2:** For each `f` in `http-rest sse websocket graphql grpc soap`, run `cargo check -p mb-protocols --no-default-features --features $f`. Expected: all succeed.
- [ ] **Step 3:** Run `cargo fmt --all --check && cargo clippy --workspace --all-targets --all-features -- -D warnings && cargo test --workspace --all-features && scripts/test-postgres.sh && cargo deny check`. Expected: all pass, with no new license or ban errors. Docker is unavailable locally, so state that the image is verified in CI.
- [ ] **Step 4:** Commit: `docs: P4 protocols guide; expose gRPC port in image`.
