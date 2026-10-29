# mb-protocols audit (verified)

I checked 66 findings against the source (read-only, no builds). I kept 64 and dropped 2: STUB-14 and STUB-19 describe deliberate, documented refusals and need no change. I lowered several severities and corrected some line numbers and fix texts. The crate's Cargo description says it has "no API stability guarantee", so API-shape findings are rated low unless they cause wrong behaviour.

## Performance

### MB-PROTOCOLS-PERF-1: form/query decoding is O(k^2) in distinct keys, before auth
- File: crates/mb-protocols/src/http_rest/form.rs:75
- Dimension: perf | Guideline: PERF:quadratic-on-request-path | Severity: high | Semver: internal-only | Effort: S | Classification: n/a
- Evidence: `insert` finds each key with `entries.iter().position(..)`. An `a[]` key becomes `entries.len().to_string()`, so the lookup always misses after a full scan. `serve` runs `form::decode` on the query at handler.rs:337 and on form/multipart bodies at handler.rs:781/785. Both happen in step 2, before admission, rate limiting and data-plane auth (step 3, handler.rs:379). A body can be up to 1 MiB (`DEFAULT_MAX_REQUEST_BYTES`).
- Fix: Keep a `HashMap<String, usize>` index for each Map node, or use an IndexMap, and give `a[]` appends a counter. Cap the number of pairs (for example 1,000) and return 400 above the cap.
- Risk: Low. Insertion order and last-value-wins must stay the same. The cap rejects pathological requests that are accepted today.

### MB-PROTOCOLS-PERF-2: group() for recorded query/form is O(k^2) in keys
- File: crates/mb-protocols/src/http_rest/handler.rs:684
- Dimension: perf | Guideline: PERF:quadratic-on-request-path | Severity: medium | Semver: internal-only | Effort: S | Classification: n/a
- Evidence: `group` does a linear `out.iter_mut().find` for each pair. It runs through `string_params` from `recorded_query` (handler.rs:990) and from `recorded_body` for undecoded form bodies (handler.rs:1033), but only when a recorder is set. `report()` is awaited before the response returns.
- Fix: Group through a `HashMap<String, usize>` index into the output Vec, keeping first-seen order. Share the pair cap from PERF-1.
- Risk: Low. Output order is unchanged.

### MB-PROTOCOLS-PERF-3: every stream listener wakes on every state write and refetches each match
- File: crates/mb-protocols/src/call.rs:1175
- Dimension: perf | Guideline: PERF:broadcast-fanout | Severity: medium | Semver: additive | Effort: L | Classification: n/a
- Evidence: `change_feed` subscribes to `rt.state.subscribe()` (one broadcast of capacity `CHANGE_CHANNEL_CAPACITY` per store). It drops events from other sessions and types only after it wakes (call.rs:1183-1189). It then calls `state.get` for each match (call.rs:1191), and `Lagged` is silently `continue`d. events/triggers.rs follows the same pattern.
- Fix: Add a scoped subscription to `StateStore` as a default trait method in mb-state, and/or carry the upserted data (`Arc`) in `StateChanged` so feeds do not need to refetch.
- Risk: Medium. This touches every mb-state store and changes lag semantics. A default trait method is additive.

### MB-PROTOCOLS-PERF-4: narrowed GraphQL schema rebuilt, hashed and compiled into a validator per root field
- File: crates/mb-protocols/src/graphql/resolve.rs:392
- Dimension: perf | Guideline: PERF:per-request-recompilation | Severity: medium | Semver: additive | Effort: M | Classification: n/a
- Evidence: `select::narrow` builds a new schema for every root field. `call::cache_key` (call.rs:1055) canonicalizes and hashes that schema. mb-engine compiles a fresh `SchemaValidator` from it each time, at lib.rs:333-335 (commented "not cached"), lib.rs:453 and lib.rs:591. subscription.rs:676-677 does the same once per subscription.
- Fix: Add a bounded LRU keyed by (source_digest, op id, selection hash) that holds the narrowed schema, its digest and an `Arc<SchemaValidator>`. Pass the cached validator into the engine through a new mb-engine method.
- Risk: Low to medium. The cache must be bounded because selections are client-controlled, and keyed by digest so a contract reload invalidates it.

### MB-PROTOCOLS-PERF-5: a read by path field (e.g. issue number) lists up to 500 resources on every get miss
- File: crates/mb-protocols/src/call.rs:1428
- Dimension: perf | Guideline: PERF:N+1-store-query | Severity: medium | Semver: internal-only | Effort: M | Classification: n/a
- Evidence: In `read_context`, when `state.get` misses and the identity has a `path_field`, it calls `state.list(session, t, MAX_SCAN = 500)` and scans the result with `.find`. Resources are stored under `id`, so a read by number misses every time.
- Fix: Store a secondary `(type, field=value) -> id` alias when writing, and keep it in step on delete and session reset. Keep the scan as a fallback for older data.
- Risk: Medium. Keeping the alias consistent takes care.

### MB-PROTOCOLS-PERF-6: each create walks all operations twice and may list 500 resources
- File: crates/mb-protocols/src/call.rs:963
- Dimension: perf | Guideline: PERF:recompute-static-per-request | Severity: low | Semver: internal-only | Effort: M | Classification: n/a
- Evidence: `next_numbers` (call.rs:941) walks `bundle.operations` to collect path fields. Only when some exist does it call `state.list(.., 500)`. `store_wrapped` (call.rs:1016) walks every Query operation's success schema through `list_members` on every successful create, then calls `listed.contains` for each body member. The bundle is fixed per runtime, so this is static work redone per request.
- Fix: Precompute a `HashMap<resource_type, Vec<path_field>>` and a `HashSet` of listed plurals when the runtime is built. Optionally keep a per-(session, type, field) high-water mark.
- Risk: Low for the precompute. Medium for the high-water mark.

### MB-PROTOCOLS-PERF-7: each gRPC call and stream deep-clones the Operation and the scenario
- File: crates/mb-protocols/src/grpc/service.rs:758
- Dimension: perf | Guideline: PERF:hot-path-clone | Severity: low | Semver: internal-only | Effort: M | Classification: n/a
- Evidence: `OwnedPlan::new` (service.rs:756-768) does `plan.op.clone()` and `plan.scenario.cloned()`, and service.rs:310 calls it for every call. sse.rs:57, subscription.rs:600 and change_feed (call.rs:1170) clone the Operation again.
- Fix: Hold `Arc<MockRuntime>` plus an operation index (and a scenario name or Arc), and borrow from them in `plan()`. Do not make `Bundle.operations` Arc'd, because that is breaking for mb-mcm.
- Risk: Low. This is a lifetime refactor.

### MB-PROTOCOLS-PERF-8: every produced event message compiles fresh validators for key, headers and params
- File: crates/mb-protocols/src/events/produce.rs:197
- Dimension: perf | Guideline: PERF:per-request-recompilation | Severity: medium | Semver: additive | Effort: S | Classification: n/a
- Evidence: `engine.fake` is called for the key schema (produce.rs:197), the headers schema (produce.rs:202), address params (address.rs:206) and the fallback payload (produce.rs:176). Each call runs `SchemaValidator::new(&bundle.with_reachable_defs(schema))` (mb-engine message.rs:115). By contrast, `channel_validator` (message.rs:132) caches the message validator.
- Fix: Add an mb-engine fake method that uses a cached validator keyed like `channel_validator` (`<digest>/channel:<id>/key|headers|param:<n>`).
- Risk: Low. The faker output is unchanged.

### MB-PROTOCOLS-PERF-9: GraphQL query text is parsed 2-3 times per request
- File: crates/mb-protocols/src/graphql/mod.rs:528
- Dimension: perf | Guideline: PERF:redundant-parse | Severity: low | Semver: internal-only | Effort: S | Classification: n/a
- Evidence: `oversize` parses the query (mod.rs:528), `is_mutation` parses it again for GET (mod.rs:448), and async-graphql parses it once more on execute. async-graphql 7.2.1 has `Request::parsed_query(&mut self)` (request.rs:114), which caches the parsed document.
- Fix: Call `request.parsed_query()` once and pass `&ExecutableDocument` to `oversize`/`is_mutation`. On a parse error, still let the executor report it.
- Risk: Low.

### MB-PROTOCOLS-PERF-10: each outgoing subscription frame is fully parsed into a Value to read type and id
- File: crates/mb-protocols/src/graphql/subscription.rs:342
- Dimension: perf | Guideline: PERF:hot-path-alloc | Severity: low | Semver: internal-only | Effort: S | Classification: n/a
- Evidence: `operation_message` does `serde_json::from_str::<Value>` over the whole frame, payload included. It is called for every server frame at subscription.rs:277.
- Fix: Deserialize into a borrowed struct `{ #[serde(rename = "type")] ty: &str, id: Option<&str> }` that ignores the payload.
- Risk: Low.

### MB-PROTOCOLS-PERF-11: operation lookup is a linear scan by id on every call
- File: crates/mb-protocols/src/http_rest/handler.rs:303
- Dimension: perf | Guideline: PERF:linear-lookup-hot-path | Severity: low | Semver: internal-only | Effort: S | Classification: n/a
- Evidence: `RouteTable::find` clones the String op id (routing.rs:71), then `Bundle::operation` scans with `operations.iter().find` (mb-mcm lib.rs:357). The same lookup runs per call in gRPC, GraphQL resolve, batch and subscription.
- Fix: Add an internal index-returning lookup, for example a `pub(crate) fn find_index`, and keep the `pub fn find` signature because RouteTable is public. Resolve indexes once at router build in gRPC and GraphQL.
- Risk: Low. Indexes are stable for each MockRuntime.

### MB-PROTOCOLS-PERF-12: bidi streams clone up to 20 earlier bodies for every inbound message
- File: crates/mb-protocols/src/grpc/service.rs:663
- Dimension: perf | Guideline: PERF:hot-path-clone | Severity: low | Semver: internal-only | Effort: S | Classification: n/a
- Evidence: Each message runs `self.earlier.iter().cloned().collect()` (up to REQUEST_STREAM_ITEMS = 20) and `body.clone()`.
- Fix: Store the earlier bodies as `Arc<Value>` and pass a shared slice to `open_stream`/`one_item`.
- Risk: Low. Only an internal signature changes.

### MB-PROTOCOLS-PERF-13: consumed payload is deep-cloned and record headers built even without a recorder
- File: crates/mb-protocols/src/events/consume.rs:197
- Dimension: perf | Guideline: PERF:hot-path-clone | Severity: low | Semver: internal-only | Effort: S | Classification: n/a
- Evidence: A valid message produces `(200, payload.clone(), .., Some(payload))`, and the first copy is used only in the RequestRecord (consume.rs:233). The `headers` Map (consume.rs:158) is built for every message.
- Fix: Build the record body and headers only when `self.rt.recorder.is_some()`.
- Risk: Low.

### MB-PROTOCOLS-PERF-14: webhook delivery log never forgets deleted mocks
- File: crates/mb-protocols/src/webhooks/log.rs:62
- Dimension: perf | Guideline: PERF:unbounded-long-lived-state | Severity: low | Semver: additive | Effort: S | Classification: n/a
- Evidence: `by_mock: Mutex<HashMap<(String, String), VecDeque<..>>>` is capped at 200 records per key, but no method removes a key. The log is shared process-wide (`RuntimeDeps.webhook_log`, mockingbird/src/runtime.rs:98).
- Fix: Add `pub fn forget(&self, workspace, mock)` and call it when a mock is deleted or unpublished.
- Risk: Low. The new public method is additive.

## Style, lint and API

### MB-PROTOCOLS-STYLE-1: StateWrite is dead code hidden by #[allow(dead_code)] with a misleading comment
- File: crates/mb-protocols/src/call.rs:1769
- Dimension: lint | Guideline: Lint:allow-dead-code | Severity: low | Semver: internal-only | Effort: S | Classification: n/a
- Evidence: The attribute reads `#[allow(dead_code)] // read by post-write hooks`, but nothing reads any field. `writes` (call.rs:800) is only `drop(writes)`'d at call.rs:845, and `write_state` clones each item's data into it.
- Fix: Remove StateWrite, have `write_state` return `()`, and delete the binding and the drop.
- Risk: Low. The type is crate-private.

### MB-PROTOCOLS-STYLE-2: RouteError prints its source twice, exposes matchit::InsertError, has no docs
- File: crates/mb-protocols/src/http_rest/routing.rs:8
- Dimension: api | Guideline: C-GOOD-ERR / C-STABLE | Severity: low | Semver: breaking-needs-0.2 | Effort: S | Classification: n/a
- Evidence: `#[error("... {method:?} {path}: {source}")]` interpolates a field named `source`, and thiserror also makes that field the chained `source()`, so chain reporters print it twice. RouteError is re-exported (http_rest/mod.rs:19) with a public `matchit::InsertError` field, and it uses Debug in a Display message.
- Fix: Now: drop `: {source}` from the message and add docs. In 0.2: box the source or hold a String, and format the method with Display.
- Risk: Changing the message is non-breaking. Changing the field type is breaking.

### MB-PROTOCOLS-STYLE-3: public APIs use String as an error type
- File: crates/mb-protocols/src/soap/xml_json.rs:104
- Dimension: api | Guideline: C-GOOD-ERR | Severity: low | Semver: breaking-needs-0.2 | Effort: M | Classification: n/a
- Evidence: Three public items use String errors: `envelope_body_to_json -> Result<Value, String>` (re-exported at soap/mod.rs:40), `LeaseProvider::try_acquire -> Result<bool, String>` (events/triggers.rs:26), and `ResolvedTarget.secret: Result<Option<SecretBytes>, String>` (webhooks/deliver.rs:49).
- Fix: Add thiserror types (EnvelopeError, LeaseError, UnresolvedSecret) in 0.2.
- Risk: Breaking for the mockingbird crate's LeaseProvider impl, which is in this workspace.

### MB-PROTOCOLS-STYLE-4: error enums flatten typed causes into Strings
- File: crates/mb-protocols/src/events/runtime.rs:132
- Dimension: api | Guideline: C-GOOD-ERR | Severity: low | Semver: breaking-needs-0.2 | Effort: M | Classification: n/a
- Evidence: `PublishError::Broker(String)` is built from `e.to_string()` (produce.rs:283). All BrokerError variants hold String, as do `GrpcError::Descriptors(String)` and `GraphqlError::Schema(String)`, so none exposes `source()`. BrokerError, PublishError and BrokerKind have no docs.
- Fix: Use `Broker(#[source] BrokerError)`. This needs `PartialEq` on BrokerError, because PublishError derives it. Keep upstream errors as `#[source]`.
- Risk: Breaking variant payloads. Check Admin API messages built from `to_string()`.

### MB-PROTOCOLS-STYLE-5: 13 public types do not implement Debug
- File: crates/mb-protocols/src/lib.rs:246
- Dimension: api | Guideline: C-DEBUG / C-COMMON-TRAITS | Severity: low | Semver: additive | Effort: M | Classification: n/a
- Evidence: None of these has a derive or impl of Debug: MockRuntime, GrpcMock, GrpcServer, RouteTable, ChannelIo, EventDeps, ChannelRuntime, Brokers, MemoryBroker, AlwaysLeader, MqttClient, AmqpClient, KafkaClient. (EventDeps derives only Clone, and MemoryBroker only Default.)
- Fix: Derive Debug where possible. Elsewhere write manual impls with `finish_non_exhaustive()` that skip dyn objects and secrets (use `display_url()` for broker URLs). Then enable `missing_debug_implementations`.
- Risk: Low. Manual impls must not print secrets.

### MB-PROTOCOLS-STYLE-6: no # Errors / # Panics sections in the crate
- File: crates/mb-protocols/src/webhooks/deliver.rs:159
- Dimension: api | Guideline: C-FAILURE / Google:panic-docs | Severity: low | Semver: none | Effort: S | Classification: n/a
- Evidence: There are 0 such headings in src. `WebhookDispatcher::send` (tokio::spawn) and `ChannelRuntime::start` (events/runtime.rs:521) say "Must run inside a Tokio runtime" only in prose. Fallible public functions such as the `try_router`s, `GrpcMock::new`, `RouteTable::new` and `Brokers::client` have no Errors section.
- Fix: Add `# Panics` and `# Errors` sections. Optionally enable clippy `missing_errors_doc` and `missing_panics_doc`.
- Risk: None.

### MB-PROTOCOLS-STYLE-7: undocumented public items; missing_docs not enabled
- File: crates/mb-protocols/src/lib.rs:192
- Dimension: api | Guideline: Google:public-docs | Severity: low | Semver: none | Effort: M | Classification: n/a
- Evidence: Examples: `StateHealth::new`, `Brokers::new`, `Brokers::config`, `MemoryBroker::new`, `RouteError`, `PublishError`, `DeliveryLog`, `DispatchSettings`, and most SaslConfig/BrokerConfig fields. grep finds no `missing_docs` lint.
- Fix: Write the docs, then enable `missing_docs = "warn"` for library crates.
- Risk: None.

### MB-PROTOCOLS-STYLE-8: crate docs are four lines with no example; Cargo metadata missing
- File: crates/mb-protocols/src/lib.rs:1
- Dimension: api | Guideline: C-CRATE-DOC / C-EXAMPLE / C-METADATA | Severity: low | Semver: none | Effort: S | Classification: n/a
- Evidence: The `//!` docs list only the features, and src has no code fences. crates/mb-protocols/Cargo.toml has no keywords or categories, while mb-ext and mockingbird set them.
- Fix: Expand the crate doc with a `no_run` example (MockRuntime to router), and add keywords and categories.
- Risk: None. Feature-gate the example.

### MB-PROTOCOLS-STYLE-9: no #[non_exhaustive] on public enums or option/record structs
- File: crates/mb-protocols/src/lib.rs:73
- Dimension: api | Guideline: Google:non-exhaustive-enums / C-STRUCT-PRIVATE | Severity: low | Semver: breaking-needs-0.2 | Effort: M | Classification: n/a
- Evidence: There are 0 `#[non_exhaustive]` attributes. Adding a variant to an error enum, or a field to ProtocolSettings, ScenarioRuntime, DispatchSettings, EventSetup, ChannelConfig, RequestRecord or DeliveryRecord, is breaking. mockingbird builds ProtocolSettings (file_config.rs:766) and RequestRecord (recorder.rs:280) with struct literals.
- Fix: In 0.2, add `#[non_exhaustive]` to the error enums, BrokerKind and DeliveryStatus. For the structs, add constructors or builders first: a non_exhaustive struct cannot be built with a literal outside the crate, even with `..Default::default()`. Then migrate the mockingbird call sites.
- Risk: Breaking.

### MB-PROTOCOLS-STYLE-10: enums with as_str() have no Display/FromStr
- File: crates/mb-protocols/src/events/broker.rs:158
- Dimension: api | Guideline: C-COMMON-TRAITS | Severity: low | Semver: additive | Effort: S | Classification: n/a
- Evidence: BrokerKind, RequestKind and DeliveryStatus have `as_str` but no Display or FromStr. mockingbird/src/file_config.rs:930-935 hand-parses BrokerKind.
- Fix: Add Display (delegating to `as_str`) and FromStr with a small error type, and switch file_config to `parse`.
- Risk: Low.

### MB-PROTOCOLS-STYLE-11: BrokerInfo.status is a free-form String with three states
- File: crates/mb-protocols/src/events/broker.rs:238
- Dimension: api | Guideline: C-CUSTOM-TYPE | Severity: low | Semver: breaking-needs-0.2 | Effort: S | Classification: n/a
- Evidence: The field is documented as "`ok`, `not connected` or `error: <e>`" and is filled at broker.rs:433-437.
- Fix: Use `enum BrokerStatus { Ok, NotConnected, Error(String) }`, with a Display impl that produces today's strings.
- Risk: Breaking for String readers. The wire format is kept if Display matches.

### MB-PROTOCOLS-STYLE-12: retry_delay is a free function whose receiver is DispatchSettings
- File: crates/mb-protocols/src/webhooks/deliver.rs:74
- Dimension: api | Guideline: C-METHOD | Severity: low | Semver: additive | Effort: S | Classification: n/a
- Evidence: `pub fn retry_delay(settings: &DispatchSettings, retry: u32)` reads only `backoff_base`. It is used at deliver.rs:230 and tests/webhooks.rs:203.
- Fix: Add a method `DispatchSettings::retry_delay`, keep the free function as a deprecated wrapper, and remove it in 0.2.
- Risk: Low.

### MB-PROTOCOLS-STYLE-13: new() constructors return Arc<Self>; MemoryBroker::new disagrees with Default
- File: crates/mb-protocols/src/events/memory.rs:48
- Dimension: api | Guideline: C-CTOR / C-CALLER-CONTROL | Severity: low | Semver: additive | Effort: S | Classification: n/a
- Evidence: MemoryBroker derives Default (which returns Self), but `new()` returns `Arc<Self>`. `GrpcServer::new` (grpc/server.rs:133) and `Brokers::new`/`empty` (broker.rs:267/278) also return Arc.
- Fix: Add Self-returning constructors, or rename the Arc versions to `new_shared` and deprecate the old names. Document where Arc is required.
- Risk: Additive if done by deprecation.

### MB-PROTOCOLS-STYLE-14: two #[allow(clippy::too_many_arguments)] functions
- File: crates/mb-protocols/src/call.rs:1783
- Dimension: lint | Guideline: Lint:too_many_arguments | Severity: low | Semver: internal-only | Effort: S | Classification: n/a
- Evidence: `write_state` (call.rs:1784) and `report` (http_rest/handler.rs:587) each take 8 parameters. Both are private.
- Fix: Group the parameters into a `WriteRequest<'a>` struct and a `RequestParts<'a>` struct (after STYLE-1), then remove the allows.
- Risk: Low.

### MB-PROTOCOLS-STYLE-15: 11 src test-module unwrap allows could be replaced by clippy.toml
- File: crates/mb-protocols/src/graphql/select.rs:334
- Dimension: lint | Guideline: Lint:allow-unwrap-in-tests | Severity: low | Semver: internal-only | Effort: S | Classification: n/a
- Evidence: There is no clippy.toml at the workspace root, and 11 `#[cfg(test)]` modules repeat `allow(clippy::unwrap_used)`: chaos, call, grpc/service, soap/xml_json, soap/envelope, graphql/state, graphql/select, events/amqp, events/broker, webhooks/event, webhooks/sign.
- Fix: Add a root clippy.toml with `allow-unwrap-in-tests = true` and `allow-expect-in-tests = true`, then delete the src allows. Keep the file-level allows in tests/*.rs.
- Risk: Low. The setting applies to the whole workspace.

### MB-PROTOCOLS-STYLE-16: graphql/mod.rs (778 lines) and soap/mod.rs (863 lines) hold the whole handler
- File: crates/mb-protocols/src/soap/mod.rs:82
- Dimension: style | Guideline: Google:no-api-in-nonleaf | Severity: low | Semver: internal-only | Effort: M | Classification: n/a
- Evidence: Both non-leaf modules contain the router, the request pipeline and helpers. By contrast grpc/mod.rs (29 lines) and webhooks/mod.rs (18 lines) are only declarations.
- Fix: Move the bodies into leaf modules (soap/handler.rs, graphql/handler.rs) and re-export from mod.rs.
- Risk: Low, but concurrent edits to these files will conflict.

### MB-PROTOCOLS-STYLE-17: lock poisoning silently recovered at about 16 sites without a stated policy
- File: crates/mb-protocols/src/webhooks/log.rs:70
- Dimension: style | Guideline: Google:no-poison-internals | Severity: low | Semver: internal-only | Effort: S | Classification: n/a
- Evidence: `.unwrap_or_else(|p| p.into_inner())` appears in chaos, limits, grpc/service, grpc/server (x4), graphql/batch, graphql/resolve, graphql/subscription, events/runtime, events/mqtt, events/memory, events/amqp, events/broker and webhooks/log. Only log.rs explains why.
- Fix: Add one `lock_unpoisoned` helper that carries the justification, and use it everywhere.
- Risk: Low. Behaviour is unchanged.

## Stubs and gaps

### MB-PROTOCOLS-STUB-1: SDL directive definitions are dropped, so queries using custom executable directives fail validation
- File: crates/mb-protocols/src/graphql/schema.rs:168
- Dimension: stub | Guideline: STUB:silent-drop | Severity: high | Semver: none | Effort: M | Classification: finish
- Evidence: `TypeSystemDefinition::Directive(_) => {}`. In async-graphql 7.2.1, dynamic `SchemaBuilder::finish` starts with empty `directives`, registers only the built-in ones, and has no API for custom directives. KnownDirectives therefore reports `Unknown directive "x"`. docs/protocols.md:9 names Shopify's Storefront schema as a supported import, and Storefront queries often use `@inContext`.
- Fix: Keep the SDL-declared executable directive names in GraphqlState. Before execution, remove their uses from the document by AST position, along with any variables used only there. Add a Storefront-style `@inContext` test.
- Risk: The splicing must handle directive arguments and variables, and the unused-variable rule.

### MB-PROTOCOLS-STUB-2: ?wsdl serves the raw WSDL; uploaded XSDs stay unresolved schemaLocation references
- File: crates/mb-protocols/src/soap/mod.rs:288
- Dimension: stub | Guideline: STUB:docs-promise | Severity: medium | Semver: none | Effort: M | Classification: finish
- Evidence: `serve_wsdl` only rewrites addresses in `st.wsdl`. mb-import stores the WSDL text with only the BOM stripped (mb-import/src/wsdl/mod.rs:133-134), even though mb-mcm lib.rs:64 says "with imported schemas inlined". docs/protocols.md:116 has users upload the XSDs, but no route serves them.
- Fix: Inline the referenced uploaded XSDs into wsdl:types in mb-import, or serve `?xsd=<name>` and rewrite schemaLocation.
- Risk: The contract digest changes on re-import. Namespace prefixes must be carried over.

### MB-PROTOCOLS-STUB-3: docs promise 4 MiB SOAP and gRPC requests; code caps both at max_request_bytes (1 MiB)
- File: crates/mb-protocols/src/soap/mod.rs:455
- Dimension: stub | Guideline: STUB:docs-promise | Severity: medium | Semver: none | Effort: S | Classification: keep-refusal
- Evidence: docs/protocols.md:110 says "gRPC messages 4 MiB ... SOAP bodies 4 MiB". SOAP reads with `rt.limits.max_request_bytes`, and gRPC uses `limit.min(max_request_bytes)` (grpc/service.rs:183). `DEFAULT_MAX_REQUEST_BYTES` is 1 MiB (limits.rs:13).
- Fix: Correct the docs: requests are capped by `limits.max_request_bytes`, and `grpc.max_message_bytes` caps responses and is an upper bound on requests.
- Risk: Docs only.

### MB-PROTOCOLS-STUB-4: gRPC compression not enabled; gzip clients are refused
- File: crates/mb-protocols/src/grpc/service.rs:185
- Dimension: stub | Guideline: STUB:unsupported | Severity: medium | Semver: none | Effort: S | Classification: finish
- Evidence: The builder chain is `Grpc::new(codec).max_decoding_message_size(..).max_encoding_message_size(..)`, with no `accept_compressed`/`send_compressed`. The workspace has `tonic = "0.14.6"` without the gzip feature. Docs do not mention compression.
- Fix: Enable tonic `gzip` (and optionally `zstd`) and add `accept_compressed`/`send_compressed`. Add a gzip client test.
- Risk: Adds a dependency feature and changes Cargo.lock. Uncompressed clients are unaffected.

### MB-PROTOCOLS-STUB-5: REST operations that declare only non-JSON responses get JSON
- File: crates/mb-protocols/src/http_rest/handler.rs:1082
- Dimension: stub | Guideline: STUB:silent-fallback | Severity: low | Semver: none | Effort: M | Classification: ask-user
- Evidence: `respond` uses the declared content type only if `is_json_media`, otherwise application/json. mb-import openapi3.rs:313-315 keeps a non-JSON media type with no schema. A CSV or XML endpoint therefore returns generated JSON (honestly labelled as JSON), and this is undocumented.
- Fix: Product decision. At minimum, add a Known-limits line and an import warning. Optionally render XML or text, or return 406/501.
- Risk: Changes what existing mocks return.

### MB-PROTOCOLS-STUB-6: MQTT shared subscriptions ignore the per-mock group; all mocks share $share/<prefix>
- File: crates/mb-protocols/src/events/mqtt.rs:337
- Dimension: stub | Guideline: STUB:param-ignored | Severity: medium | Semver: none | Effort: S | Classification: finish
- Evidence: `share_group: cfg.client_id_prefix.clone()`, and `subscribe_shared` takes `_group` but ignores it (mqtt.rs:425-431). consume.rs:56 computes `consumer_group(prefix, workspace, mock)` for exactly this purpose. With shared subscriptions on (non-local HA bus), two different mocks consuming the same topic split its messages.
- Fix: Use `shared_filter(group, &filter)` and drop `share_group`. consumer_group output contains no '/', '+' or '#'.
- Risk: During a deploy, old and new share groups briefly coexist.

### MB-PROTOCOLS-STUB-7: stale Known-limits lines in docs/events.md
- File: docs/events.md:152
- Dimension: stub | Guideline: STUB:docs-stale | Severity: low | Semver: none | Effort: S | Classification: finish
- Evidence: Line 152 says MQTT subscriptions are never shared, but `subscribe_shared` uses `$share/..` with a non-local HA bus. Line 156 says the Channels tab "comes in P6", but `crates/mb-console/web/src/pages/mocks/Channels.tsx` exists and is routed (router.tsx:180, MockLayout.tsx:17).
- Fix: Reword line 152 (shared only with a non-local HA bus), delete line 156, and mention the tab in docs/console.md.
- Risk: Docs only.

### MB-PROTOCOLS-STUB-8: chaos settings have no effect on AsyncAPI channels
- File: crates/mb-protocols/src/events/produce.rs:106
- Dimension: stub | Guideline: STUB:config-unused | Severity: low | Semver: none | Effort: M | Classification: ask-user
- Evidence: `grep chaos src/events` finds nothing. docs/concepts.md:38 presents chaos as per-mock, with no protocol exceptions.
- Fix: Either apply error_rate and latency to publishes and replies, or document that chaos covers request/response protocols only and warn on channel-only mocks.
- Risk: Applying chaos changes trigger output for existing configs.

### MB-PROTOCOLS-STUB-9: batched GraphQL requests (JSON array body) are refused with a serde error
- File: crates/mb-protocols/src/graphql/mod.rs:442
- Dimension: stub | Guideline: STUB:unsupported | Severity: low | Semver: none | Effort: M | Classification: ask-user
- Evidence: `parse_post` only does `serde_json::from_slice::<Request>`, so an array body gets a 400 with a serde message. The docs do not mention batching.
- Fix: Parse `async_graphql::BatchRequest` and admit each entry, with a bound on batch length. Or return a clear "batched requests are not supported" 400 and document it.
- Risk: Each entry needs its own admission and metering.

### MB-PROTOCOLS-STUB-10: event messages are JSON but labelled with the contract's contentType
- File: crates/mb-protocols/src/events/produce.rs:276
- Dimension: stub | Guideline: STUB:silent-fallback | Severity: low | Semver: none | Effort: S | Classification: keep-refusal
- Evidence: `content_type: channel.content_type.clone()` is sent as the MQTT and AMQP content type, while payloads are always JSON (docs/events.md:151).
- Fix: Send the declared type only when it is JSON media, otherwise `application/json`. Extend the Known-limits line.
- Risk: Consumers matching on the declared type will see application/json instead.

### MB-PROTOCOLS-STUB-11: binary WebSocket frames on event channels are silently discarded
- File: crates/mb-protocols/src/ws.rs:173
- Dimension: stub | Guideline: STUB:silent-drop | Severity: low | Semver: none | Effort: S | Classification: finish
- Evidence: `Message::Binary(_) | Ping | Pong => {}`. By contrast, non-JSON broker payloads are recorded as 400 (consume.rs:177-181).
- Fix: Pass binary frames on event channels to the consumer so they are recorded (400 when not JSON). Otherwise, at least log them and document the limit.
- Risk: Low. Adds request-log entries.

### MB-PROTOCOLS-STUB-12: unsupported SASL mechanisms are refused only at connect (AMQP/MQTT) or passed to librdkafka
- File: crates/mb-protocols/src/events/amqp.rs:71
- Dimension: stub | Guideline: STUB:late-refusal | Severity: low | Semver: none | Effort: S | Classification: keep-refusal
- Evidence: amqp.rs:71 and mqtt.rs:199 reject non-PLAIN only when connecting. kafka.rs:51 passes any mechanism through. file_config does not validate the mechanism.
- Fix: Validate per kind in mockingbird/src/file_config.rs at load, and say per kind in configuration.md. Keep the runtime checks.
- Risk: Configs that used to start in a degraded state now fail at startup.

### MB-PROTOCOLS-STUB-13: passthrough_validate is a silent no-op for GraphQL, gRPC, SOAP and AsyncAPI
- File: crates/mb-protocols/src/dp_auth.rs:66
- Dimension: stub | Guideline: STUB:silent-noop | Severity: low | Semver: none | Effort: S | Classification: keep-refusal
- Evidence: `passthrough_satisfied` returns true when `security.is_empty()`. The GraphQL, proto and WSDL importers always emit `security: Vec::new()`. docs/console.md:62 notes this, but configuration.md:249 does not.
- Fix: Warn (log, and /readyz or mock status) when passthrough_validate is set on a bundle with no security requirements. Copy the caveat into configuration.md.
- Risk: Log and docs only.

### MB-PROTOCOLS-STUB-15: MQTT client is MQTT 5 only; undocumented
- File: crates/mb-protocols/src/events/mqtt.rs:14
- Dimension: stub | Guideline: STUB:undocumented-limit | Severity: low | Semver: none | Effort: S | Classification: ask-user
- Evidence: Only `rumqttc::v5` types are used, and docs/events.md never says brokers must support MQTT 5.
- Fix: Add a Known-limits line and map a protocol-version CONNACK refusal to a clear message. Whether to support 3.1.1 is a product decision.
- Risk: Docs only, unless 3.1.1 support is built.

### MB-PROTOCOLS-STUB-16: one broker per mock; multi-protocol AsyncAPI documents are partly inactive
- File: crates/mb-protocols/src/events/runtime.rs:464
- Dimension: stub | Guideline: STUB:undocumented-limit | Severity: low | Semver: none | Effort: M | Classification: ask-user
- Evidence: `resolve_broker` auto-picks a broker only when one broker serves every channel (`[only] =>`). Otherwise it errors, or marks the other protocols' channels inactive. AsyncAPI `servers` are not used.
- Fix: Short term, add a Known-limits line. A per-protocol broker map is a product decision.
- Risk: A broker map changes the config schema and docs.

### MB-PROTOCOLS-STUB-17: MTOM/XOP SOAP requests get 415; undocumented
- File: crates/mb-protocols/src/soap/mod.rs:725
- Dimension: stub | Guideline: STUB:undocumented-limit | Severity: low | Semver: none | Effort: S | Classification: keep-refusal
- Evidence: `version_and_action` accepts only `text/xml` and `application/soap+xml` (`_ => None`). The SOAP Known limits in docs/protocols.md:114-117 do not mention MTOM.
- Fix: Add a Known-limits line and name MTOM in the fault text.
- Risk: Docs and message only.

### MB-PROTOCOLS-STUB-18: webhook targets and deliveries have no console UI
- File: crates/mb-admin/src/webhook_deliveries.rs:86
- Dimension: api | Guideline: STUB:admin-route-unexposed | Severity: low | Semver: none | Effort: M | Classification: finish
- Evidence: A case-insensitive grep for "webhook" in crates/mb-console/web/src (excluding schema.d.ts) finds nothing, while MCP exposes list_webhook_deliveries.
- Fix: Add a Webhooks tab with a target editor (secret refs only) and a deliveries table.
- Risk: Console only.

## Tests

### MB-PROTOCOLS-TESTS-1: 2 s real sleep plus 100 ms settle in the hop-limit test
- File: crates/mb-protocols/tests/events_triggers.rs:384
- Dimension: tests | Guideline: PERF:test-fixed-sleep | Severity: medium | Semver: none | Effort: S | Classification: n/a
- Evidence: `state_effect_and_trigger_on_one_topic_stay_bounded` is an unpaused `#[tokio::test]`. It sleeps 100 ms at :370, then 2 s at :384, then asserts 3 publishes. MemoryBroker has `wait_published` and `subscribed_groups`.
- Fix: Poll `subscribed_groups()` until it is non-empty, call `wait_published(3, 5s)`, then run a short absence window (e.g. `wait_published(4, 150ms)`).
- Risk: Low.

### MB-PROTOCOLS-TESTS-2: settle() is a fixed 50 ms guess, used 9 times
- File: crates/mb-protocols/tests/events_runtime.rs:108
- Dimension: tests | Guideline: PERF:test-fixed-sleep | Severity: medium | Semver: none | Effort: S | Classification: n/a
- Evidence: `inject()` delivers only to existing subscribers (memory.rs:92), so a slow runner can silently lose the injected message.
- Fix: Poll `subscribed_groups().len() >= expected` with a 5 s deadline.
- Risk: Low.

### MB-PROTOCOLS-TESTS-3: tautological sleep and assert in the publish-only test; absence sleeps without a sentinel
- File: crates/mb-protocols/tests/events_runtime.rs:619
- Dimension: tests | Guideline: tests:tautology | Severity: low | Semver: none | Effort: S | Classification: n/a
- Evidence: `inject` never records to `published`, and publish-only mode has no consumers, so the assert after the 200 ms sleep at :619-620 cannot fail. :429 and :455 are 100 ms absence checks.
- Fix: Delete :618-620. At :429 and :455, inject a valid sentinel and wait for it instead.
- Risk: Low.

### MB-PROTOCOLS-TESTS-4: negative-assertion sleeps where a later event can be a sentinel
- File: crates/mb-protocols/tests/events_triggers.rs:272
- Dimension: tests | Guideline: PERF:test-fixed-sleep | Severity: low | Semver: none | Effort: S | Classification: n/a
- Evidence: The test sleeps 200 ms at :272 before a delete that is awaited with `wait_published(1)`. There is another 200 ms sleep at :257.
- Fix: Drop :272 (the later len()==1 plus key assert covers it). At :257, use a sentinel upsert.
- Risk: Low. This relies on the watcher handling changes in order.

### MB-PROTOCOLS-TESTS-5: 200 ms absence sleeps in webhook filter tests
- File: crates/mb-protocols/tests/webhooks.rs:454
- Dimension: tests | Guideline: PERF:test-fixed-sleep | Severity: low | Semver: none | Effort: S | Classification: n/a
- Evidence: The sleep at :454 is followed by a PUT whose delivery is awaited, plus `len()==1`. :416 has no sentinel.
- Fix: Delete :454-455. At :416, send a valid request as a sentinel and wait for it.
- Risk: Low.

### MB-PROTOCOLS-TESTS-6: Usage::settled costs at least 200 ms per call (4 sites)
- File: crates/mb-protocols/tests/grpc.rs:684
- Dimension: tests | Guideline: PERF:test-fixed-sleep | Severity: low | Semver: none | Effort: S | Classification: n/a
- Evidence: It loops on 100 ms sleeps until the count is stable twice. It is called at :789, :821, :856 and :902.
- Fix: Wait for n events with a deadline, then run one short (about 50 ms) absence window.
- Risk: Low.

### MB-PROTOCOLS-TESTS-7: fixed 3 s sleep before a publish-until-received loop
- File: crates/mb-protocols/tests/events_kafka.rs:50
- Dimension: tests | Guideline: PERF:test-fixed-sleep | Severity: medium | Semver: none | Effort: S | Classification: n/a
- Evidence: The loop at :61-68 already republishes every second for up to 30 s.
- Fix: Delete :49-50.
- Risk: Low.

### MB-PROTOCOLS-TESTS-8: fixed 200-500 ms MQTT subscribe settles
- File: crates/mb-protocols/tests/events_mqtt.rs:151
- Dimension: tests | Guideline: PERF:test-fixed-sleep | Severity: low | Semver: none | Effort: M | Classification: n/a
- Evidence: There are sleeps at :151, :218, :421 and :457. `test_client` (:116-131) drops SubAck.
- Fix: Forward SubAck from `test_client` and await it at :457. At :218 the runtime consumer also has to subscribe, so pair the SubAck with a publish-retry loop. Use a publish-until-received loop at :151, and on `other` at :421. Keep the 500 ms at :430, which is a bounding check.
- Risk: Low.

### MB-PROTOCOLS-TESTS-9: start_broker picks a port by bind-then-drop
- File: crates/mb-protocols/tests/events_mqtt.rs:26
- Dimension: tests | Guideline: tests:port-race | Severity: low | Semver: none | Effort: S | Classification: n/a
- Evidence: It binds 127.0.0.1:0, drops the listener, and lets rumqttd bind the port later on a thread. `let _ = broker.start()` swallows a bind failure, and the connect loop would then attach to whatever is listening on that port.
- Fix: Send the `broker.start()` error back over a channel and fail the test on it. Track the ports handed out in a process-wide set.
- Risk: Low. Test only.

### MB-PROTOCOLS-TESTS-10: duplicated poll-until helpers
- File: crates/mb-protocols/tests/data_plane_auth_protocols.rs:111
- Dimension: tests | Guideline: tests:dup-helper | Severity: low | Semver: none | Effort: M | Classification: n/a
- Evidence: `Records::wait` is copied verbatim in graphql.rs:773 and returns partial results on timeout. Other helpers: `wait_for` in webhooks.rs:108, `Capture::wait` in events_runtime.rs, and inline loops in sse.rs and graphql_ws.rs.
- Fix: Add a shared `poll_until` that panics with context, and an `absent_for` helper.
- Risk: Low. Check callers that assert on partial lengths.

### MB-PROTOCOLS-TESTS-11: consolidate 20 integration-test binaries into tests/it
- File: crates/mb-protocols/tests/events_broker.rs:289
- Dimension: tests | Guideline: tests:binary-consolidation | Severity: medium | Semver: none | Effort: M | Classification: n/a
- Evidence: There are 20 test files. Eight `include_bytes!("../../../fixtures/..")` sites would break one directory deeper. Cargo.toml:93-103 has 3 `[[test]]` blocks with required-features. spans.rs:72 uses the thread-local `set_default`.
- Fix: Create tests/it/main.rs with feature-gated `mod` lines. Use `concat!(env!("CARGO_MANIFEST_DIR"), ...)` for the includes, drop the `[[test]]` blocks, and put one unwrap allow in main.rs.
- Risk: Medium. `--test <name>` filters change. Fix TESTS-9 first.

### MB-PROTOCOLS-TESTS-12: 150 ms real sleep because StateHealth uses std::time::Instant
- File: crates/mb-protocols/tests/runtime.rs:464
- Dimension: tests | Guideline: PERF:test-fixed-sleep | Severity: low | Semver: internal-only | Effort: S | Classification: n/a
- Evidence: src/lib.rs:9 imports std `Instant` for `last_attempt`, so `start_paused` cannot advance it.
- Fix: Either shrink the interval and sleep, or switch the private field to `tokio::time::Instant` and pause the test.
- Risk: Low. Check the other paused runtime.rs tests.

### MB-PROTOCOLS-TESTS-13: 50 ms sleep orders a WS stop before the next start (race)
- File: crates/mb-protocols/tests/graphql.rs:1109
- Dimension: tests | Guideline: PERF:test-fixed-sleep | Severity: low | Semver: none | Effort: S | Classification: n/a
- Evidence: The slot is freed only when the outgoing `complete` frame passes through the async output stream (subscription.rs:283-290), so the server may not have processed the stop within 50 ms.
- Fix: Retry start with fresh ids until it is accepted, within 2 s.
- Risk: Low.

### MB-PROTOCOLS-TESTS-14: disconnect-stops-generation tests use a fixed 100+200 ms pair
- File: crates/mb-protocols/tests/sse.rs:226
- Dimension: tests | Guideline: PERF:test-fixed-sleep | Severity: low | Semver: none | Effort: S | Classification: n/a
- Evidence: sse.rs:226-230 and graphql_ws.rs:264-267 are absence checks with an upper bound.
- Fix: Poll until `chunks_yielded` is stable, then run one 100 ms absence window. Keep the upper bound.
- Risk: Low.

### MB-PROTOCOLS-TESTS-15: soap_wsdl_open_without_data_plane_auth duplicates soap.rs and checks status only
- File: crates/mb-protocols/tests/data_plane_auth_protocols.rs:974
- Dimension: tests | Guideline: tests:duplicate | Severity: low | Semver: none | Effort: S | Classification: n/a
- Evidence: It builds a runtime with no auth and asserts only 200. tests/soap.rs already covers an unauthenticated WSDL GET and checks the body.
- Fix: Delete it, or set mode None explicitly and assert the body and content type.
- Risk: None.

### MB-PROTOCOLS-TESTS-16: type ascription in new_runtime_defaults asserts nothing at runtime
- File: crates/mb-protocols/tests/runtime.rs:671
- Dimension: tests | Guideline: tests:tautology | Severity: low | Semver: none | Effort: S | Classification: n/a
- Evidence: `let _: &HashMap<String, ScenarioRuntime> = &rt.scenarios;` only pins the type, and the line above already asserts `is_empty()`.
- Fix: Remove it, or move it to an explicit API-shape test with a comment.
- Risk: None.

## Dropped

- MB-PROTOCOLS-STUB-14: the MQTT-over-TLS refusal is deliberate, enforced at load and connect, and documented. There is no defect to fix.
- MB-PROTOCOLS-STUB-19: ignoring OpenAPI callback URL expressions is a documented refusal (docs/webhooks.md:34), and following them would open an SSRF vector. There is nothing to change.
