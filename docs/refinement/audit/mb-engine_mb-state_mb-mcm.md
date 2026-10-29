# Audit: mb-engine, mb-state, mb-mcm (verified)

Each finding below was checked against the cited source by reading the code (no builds). The crates are published as `mockingbird-engine`, `mockingbird-state` and `mockingbird-mcm` 0.1.0. Their Cargo descriptions say "Internal crate ... no API stability guarantee", and in-workspace dependents pin `=0.1.0`, so API-only findings are rated low unless they also affect behaviour.

Kept: 56. Dropped or merged: 3 (MB-ENGINE-PERF-13 merged into PERF-4, MB-ENGINE-STYLE-17 and MB-ENGINE-TESTS-12). The optional `memory_conformance` pause suggestion inside TESTS-1 was also removed, because those tests order by wall-clock `Utc` timestamps.

## Performance

### MB-ENGINE-PERF-1: Engine.shapes cache never evicts non-selection keys or entries from old contract digests
- file: crates/mb-engine/src/lib.rs:900
- dimension: perf; guideline: PERF:unbounded-cache; severity: medium (was high); semver: internal-only; effort: S; classification: n/a
- evidence: `if selection.is_none() || self.shapes.len() < MAX_SELECTION_SHAPES` inserts every non-selection key without checking the size. The key is `(digest/op.id, status, None, requested)`, where `requested` is the sorted subset of the schema's member names that appear in the request (lib.rs:886-891). Growth is therefore bounded by 2^M per (op, status), not truly unbounded, but M comes from the contract and the subset is chosen by the client. Keys carry `source_digest`, so every contract re-upload leaves its old entries behind. `member_names` (lib.rs:878) has the same digest-retention issue.
- fix: Count every insert against a cap, or replace both DashMaps with a bounded LRU (quick_cache or moka, about 4096 entries). Optionally drop entries whose digest differs from the current bundle the first time a new digest is seen.
- risk: Low. It is a memo of `budget::shape`, so a miss only costs a recompute.

### MB-ENGINE-PERF-2: Each GraphQL request compiles a fresh validator for its narrowed schema
- file: crates/mb-engine/src/lib.rs:335
- dimension: perf; guideline: PERF:per-request-recompile; severity: high; semver: internal-only; effort: M; classification: n/a
- evidence: `Some(schema) => Arc::new(SchemaValidator::new(&bundle.with_reachable_defs(schema))?)` runs whenever `ctx.response_schema` is set, and the code comments that the validator is not cached. `SchemaValidator::new` also clones the whole schema (validate.rs:33). `shaped()` already hashes `schema.to_string()` for the same narrowed schema (lib.rs:866-870). The same pattern appears at lib.rs:453 (joint) and lib.rs:591 (serve_stored).
- fix: Add a bounded cache keyed by `(digest/op.id, status, hash of narrowed schema)`. Compute the hash once per `generate()` and share it with `shaped()`. On a hit, compare the stored schema with the narrowed one (SchemaValidator keeps `schema`) so a hash collision cannot pick the wrong validator.
- risk: Low. Validators are pure functions of the schema.

### MB-ENGINE-PERF-3: The faker regex cache lives for only one fake call
- file: crates/mb-engine/src/faker.rs:156
- dimension: perf; guideline: PERF:per-request-recompile; severity: medium (was high: only the offline and fallback paths pay it); semver: internal-only; effort: M; classification: n/a
- evidence: `fake_with_costs` builds `Ctx { patterns: HashMap::new(), .. }`, so every call starts with an empty cache. `fake_value` calls it up to FALLBACK_ATTEMPTS=12 times (lib.rs:836-845), and `compile_pattern` (faker.rs:936) builds a `regex::Regex` plus a rand_regex generator each time. `echo_as` (faker.rs:327) runs `regex::Regex::new(p)` on every echoed field.
- fix: Keep a bounded, engine-level cache (`DashMap<String, Option<Arc<Pattern>>>` with a size cap, or an LRU) and pass a reference into `Ctx`. Have `echo_as` reuse the cached `check` regex. `fake_with_costs` is public, so keep its signature and add an internal variant that takes the cache.
- risk: Low. Compiled regexes are immutable and Send + Sync.

### MB-ENGINE-PERF-4: fake_value rebuilds the reachable-defs schema and Costs on every call (includes former PERF-13)
- file: crates/mb-engine/src/lib.rs:831
- dimension: perf; guideline: PERF:hot-path-alloc; severity: medium; semver: internal-only; effort: M; classification: n/a
- evidence: Every call runs `bundle.with_reachable_defs(schema)` (a transitive closure plus a deep clone) and `Costs::compute(&schema)`. Both depend only on (digest, schema). Callers include every offline or fallback response, every stream item, `Engine::fake` for each event message, and `fake_field` inside `unique_list_ids` (lib.rs:750, up to IDENTITY_DRAWS=8 per duplicate id) and `fresh_identity` (lib.rs:777). In those last two the id schema is small, so most of the cost is in whole-response fakes.
- fix: Memoise `(Arc<Value> schema_with_defs, Arc<Costs>)` per (digest/op.id, status) next to `validators`. Build the faker setup once per `unique_list_ids` or `fresh_identity` call instead of once per draw.
- risk: Low. Memory cost is about one schema copy per op.

### MB-ENGINE-PERF-5: Engine::fake compiles a validator for every produced event message
- file: crates/mb-engine/src/message.rs:115
- dimension: perf; guideline: PERF:per-request-recompile; severity: medium; semver: internal-only; effort: S; classification: n/a
- evidence: `pub fn fake` calls `SchemaValidator::new(&bundle.with_reachable_defs(schema))` on every call. It is called per message from mb-protocols events/produce.rs:176 (with `&channel.message_schema`), produce.rs:197 and 202, and address.rs:206. The cached `channel_validator` (message.rs:131) is not used.
- fix: Inside `fake`, reuse a validator cache keyed by a schema hash (shared with PERF-2), or add an additive `fake_for_channel` that uses `channel_validator`, and switch produce.rs:176 to it.
- risk: Low.

### MB-ENGINE-PERF-6: Persona Template::fix compiles a regex for each object and template
- file: crates/mb-engine/src/persona.rs:345
- dimension: perf; guideline: PERF:per-request-recompile; severity: medium; semver: internal-only; effort: S; classification: n/a
- evidence: `fix` builds an anchored pattern string and runs `Regex::new(&pattern)` on every call. `fix_object` (persona.rs:257-309) calls it for each template whose field is present, and for id-like keys that match a template's leading literal. A list of N items therefore costs about N times T compiles per response, but only when a persona declares formats.
- fix: Replace the regex with a hand-written matcher over the parts: `strip_prefix` each Literal and Field value, then check that exactly n chars of each Random part are in `class`, and that the input is fully consumed.
- risk: Low. The anchored, exact-count semantics must stay the same. The persona unit tests cover them.

### MB-ENGINE-PERF-7: Boolean validity checks format up to 10 error strings
- file: crates/mb-engine/src/validate.rs:335
- dimension: perf; guideline: PERF:hot-path-alloc; severity: medium; semver: additive; effort: S; classification: n/a
- evidence: `validate` collects `iter_errors(..).take(MAX_ERRORS).map(describe)`. About 19 non-test call sites only call `.is_ok()`, `.is_err()` or `.ok()` on it: guards.rs:265 and 285, lib.rs:292, 464, 604, 759, 784, 840, 1151, 1247, 1256, 1306, 1325 and 1327, persona.rs:234, validate.rs:110 and 276, and stream.rs:427. `validate` is a method of the publicly reachable `mb_engine::validate::SchemaValidator`.
- fix: Add `pub fn is_valid(&self, v: &Value) -> bool { self.inner.is_valid(v) }`, which jsonschema 0.60 provides and which short-circuits. Switch the boolean-only callers to it.
- risk: Very low. Same result, and a new public method.

### MB-ENGINE-PERF-8: MemoryStore::list sorts every resource in the session under a write shard lock
- file: crates/mb-state/src/memory.rs:50
- dimension: perf; guideline: PERF:o-n-scan-hot-path; severity: medium; semver: internal-only; effort: M; classification: n/a
- evidence: `sorted()` filters every `(type, id)` in the session and sorts all matches before `list` takes `limit`. `Inner::live` (memory.rs:80) uses `sessions.get_mut`, so the read paths (get, list and export at memory.rs:169, 185 and 293) take an exclusive shard lock.
- fix: Use `sessions.get` for the reads, and handle expiry by dropping the read guard before `remove_if`, as the code already does. Use `select_nth_unstable_by` or a bounded heap for the top `limit`, or keep a per-type `BTreeMap<seq, id>` index.
- risk: Switching to `get` and using partial selection are low risk. The index needs every insert, delete, seed and reset path to keep it in sync; the conformance tests cover ordering.

### MB-ENGINE-PERF-9: RedisStateStore::list fetches the whole order zset for each list call
- file: crates/mb-state/src/redis.rs:289
- dimension: perf; guideline: PERF:n-plus-one-store; severity: medium; semver: internal-only; effort: M; classification: n/a
- evidence: `ZREVRANGE order 0 -1` returns the session's members of every type. The client then filters by the `type<SEP>` prefix and applies `take(limit)` (redis.rs:289-299).
- fix: Smaller change: page `ZREVRANGE` in chunks (for example 4 x limit) until `limit` matches are found. Larger change: keep a per-type order zset, with a read fallback to the combined set for one TTL window during rolling upgrades.
- risk: Medium for the key-layout change, because HA nodes on mixed versions must agree on it. Chunked paging is low risk.

### MB-ENGINE-PERF-10: The overlay() fallback clones and revalidates the whole body for each request field
- file: crates/mb-engine/src/lib.rs:1251
- dimension: perf; guideline: PERF:o-n2-hot-path; severity: low; semver: internal-only; effort: S; classification: n/a
- evidence: After the merged body fails validation, the loop at lib.rs:1251-1259 clones `best` and runs a full `validate` for each field.
- fix: Insert in place, run `is_valid` (PERF-7), and remove the field on failure, so no clone is needed. Keep the per-field acceptance semantics.
- risk: Low.

### MB-ENGINE-PERF-11: declared_properties() walks the schema again on every create or update
- file: crates/mb-engine/src/validate.rs:195
- dimension: perf; guideline: PERF:hot-path-alloc; severity: low; semver: additive; effort: S; classification: n/a
- evidence: `overlay_request` (lib.rs:1223) calls `validator.declared_properties()`, which builds a new `HashSet<String>` each time from the validator's fixed schema.
- fix: Cache the result in a `OnceLock<Option<HashSet<String>>>` field. Add a borrowing accessor and keep the owned method.
- risk: Very low.

### MB-ENGINE-PERF-12: ResponseCache eviction scans every entry of the session on each put at capacity
- file: crates/mb-state/src/cache.rs:151
- dimension: perf; guideline: PERF:o-n-scan-hot-path; severity: low; semver: internal-only; effort: S; classification: n/a
- evidence: `while cache.entries.len() > self.capacity { .. iter().min_by_key(|(_, e)| e.used) .. }` (cache.rs:151-160) is O(C) per put, with C up to 500 by default, and runs under the shard lock.
- fix: Keep a `BTreeMap<used, key>` recency index or use `hashlink::LinkedHashMap`, and update it in `invalidate_type`. Optionally store `Arc<CachedResponse>` so `get` clones cheaply.
- risk: Low.

### MB-ENGINE-PERF-14: LineSplitter::push scans the whole pending buffer for a newline on every delta
- file: crates/mb-engine/src/ndjson.rs:17
- dimension: perf; guideline: PERF:o-n2-hot-path; severity: low; semver: internal-only; effort: S; classification: n/a
- evidence: `self.buffer.rfind('\n')` runs after each append. Complete lines are split off, so the buffer holds only the unfinished line, and the cost is O(line length) per delta, which is quadratic per line.
- fix: Search only `delta`. If it contains no newline, return early. Otherwise split at `old_len + i + 1`.
- risk: Very low. The splitter tests cover delta boundaries.

### MB-ENGINE-PERF-15: patch and fill_required deep-clone the $defs map for each error
- file: crates/mb-engine/src/validate.rs:225
- dimension: perf; guideline: PERF:hot-path-alloc; severity: low; semver: internal-only; effort: M; classification: n/a
- evidence: `at()` (validate.rs:225-231) and `subschema()` (from 235) clone the node and insert `defs.clone()` for every patched or filled error.
- fix: Fake from a subschema node while resolving refs against the root, using `self.schema` as `root_with_defs`. This needs an internal faker entry point.
- risk: Low to medium.

### MB-ENGINE-PERF-16: Bundle::operation is a linear scan on every request
- file: crates/mb-mcm/src/lib.rs:357
- dimension: perf; guideline: PERF:o-n-scan-hot-path; severity: low; semver: none (callers change; mb-mcm API unchanged); effort: S; classification: n/a
- evidence: `self.operations.iter().find(|o| o.id == id)`. `channel()` at lib.rs:361 is linear too. Bundle has all-pub fields and is not `#[non_exhaustive]` (lib.rs:11), so adding an index field would be breaking.
- fix: Build a `HashMap<String, usize>` index in the runtime that owns `Arc<Bundle>`, or have route resolution return the operation index directly.
- risk: Low.

## API and style

### MB-ENGINE-STYLE-1: StateError is a single stringly-typed variant that drops error sources
- file: crates/mb-state/src/lib.rs:35
- dimension: api; guideline: C-GOOD-ERR; severity: medium; semver: breaking-needs-0.2; effort: M; classification: n/a
- evidence: `pub enum StateError { #[error("state backend: {0}")] Backend(String) }` has 28 construction sites in mb-state/src, all built with string formatting. `EngineError::Schema(String)` (mb-engine lib.rs:47) is built the same way.
- fix: In 0.2, mark the enum `#[non_exhaustive]` and add typed variants with `#[source]` (Redis, Sql, Codec, Timeout, UnsupportedUrl), keeping the Display prefixes. Until then, there is no additive fix that keeps the source chain.
- risk: Breaks exhaustive matches and external construction of `Backend`. The Display text must stay stable.

### MB-ENGINE-STYLE-2: 17 public types have no Debug, and the URL-carrying types need redaction
- file: crates/mb-engine/src/lib.rs:239
- dimension: api; guideline: C-DEBUG; severity: low (was medium); semver: additive; effort: S; classification: n/a
- evidence: None of these has a derive or a manual impl: Engine (lib.rs:239), JointPart (69), EngineGuards (guards.rs:84, which derives only Clone and Default), SchemaValidator (validate.rs:22), Faker (faker.rs:48), Costs (faker.rs:360), ResponseCache (cache.rs:81), CachingStateStore (cache.rs:240), LeaderBackend (leader.rs:32), Leadership (leader.rs:80), MemoryStore (memory.rs:131), RedisStateStore (redis.rs:154), SqlStateStore (sql.rs:154), LocalBus, MemoryBusHub, PgBus and RedisBus (bus.rs:202, 235, 428, 589). There is no `impl Debug` anywhere in the three crates.
- fix: Derive Debug where every field allows it. Hand-write `finish_non_exhaustive` impls for the others. `LeaderBackend::Postgres/Redis(String)` must print a redacted URL. Then enable `missing_debug_implementations`.
- risk: Additive. A naive derive on a URL-carrying type would leak credentials.

### MB-ENGINE-STYLE-3: #[non_exhaustive] is applied inconsistently to enums likely to grow
- file: crates/mb-mcm/src/lib.rs:175
- dimension: api; guideline: Google:non-exhaustive-enums; severity: low (was medium; the crates are internal and pinned); semver: breaking-needs-0.2; effort: M; classification: n/a
- evidence: Only Protocol (lib.rs:85) and Binding (lib.rs:112) are marked. OperationKind (175), HttpMethod (160), SecurityScheme (263), ChannelBinding (422), Trigger (485), StatusSelector (303), mb-engine Source (lib.rs:93) and EngineError (lib.rs:45), StateError, and BusEvent (bus.rs:36) are exhaustive.
- fix: In 0.2, mark the error enums and the extensible model enums `#[non_exhaustive]` and add wildcard arms in the workspace. Batch this with STYLE-1, STUB-4, STUB-5, STUB-6 and STUB-11.
- risk: Breaking for 0.1 consumers.

### MB-ENGINE-STYLE-4: Public option structs are all-pub without #[non_exhaustive]
- file: crates/mb-engine/src/lib.rs:158
- dimension: api; guideline: Google:non-exhaustive-structs; severity: low; semver: breaking-needs-0.2; effort: S; classification: n/a
- evidence: EngineConfig (lib.rs:158, which implements Default at 223), StreamConfig (172), StreamContext (193), GenerationContext (78), EngineGuards (guards.rs:84) and ScrubberConfig (pii.rs:96) all have only public fields.
- fix: In 0.2, mark these `#[non_exhaustive]` and keep the `Default` impls. Leave mb-mcm data types exhaustive.
- risk: Breaks external struct literals.

### MB-ENGINE-STYLE-5: Copy enums leave out Hash, and OperationKind leaves out Copy, Eq and Hash
- file: crates/mb-mcm/src/lib.rs:173
- dimension: api; guideline: C-COMMON-TRAITS; severity: low; semver: additive; effort: S; classification: n/a
- evidence: `#[derive(.., Clone, Copy, PartialEq, Eq)]` without Hash on Protocol (83), GraphqlOperationType (93), SoapVersion (101), HttpMethod (158), Effect (219), ApiKeyLocation (252), Direction (409), StateChangeKind (473), StatusSelector (301), mb-engine Source (lib.rs:92), and mb-state ChangeKind and ChangeOrigin. OperationKind (173) derives only `Clone, PartialEq`, although its only payload is the `Copy + Eq` Effect. pii.rs BuiltinPattern already derives Hash.
- fix: Add `Hash` to these enums, and `Copy, Eq, Hash` to OperationKind.
- risk: Additive.

### MB-ENGINE-STYLE-6: The string-form accessor has inconsistent names and Display is mostly missing
- file: crates/mb-mcm/src/lib.rs:205
- dimension: api; guideline: C-WORD-ORDER; severity: low; semver: additive; effort: S; classification: n/a
- evidence: The same idea is spelled `Source::as_str` (which has Display), `BuiltinPattern::as_str` (pii.rs:47, no Display), `OperationKind::label` (mcm lib.rs:205), and `ChannelBinding::protocol() -> &'static str` (lib.rs:447). `Bundle::protocol()` (lib.rs:351) returns the `Protocol` enum instead. HttpMethod has only a private `method_name`.
- fix: Add `as_str` and Display to each type. Keep the old names, deprecated, and migrate the workspace callers in the same PR so `-D warnings` keeps passing.
- risk: Additive.

### MB-ENGINE-STYLE-7: No fallible public function documents its errors
- file: crates/mb-engine/src/lib.rs:300
- dimension: api; guideline: C-FAILURE; severity: low; semver: none; effort: S; classification: n/a
- evidence: There is no `# Errors` section in any of the three crates. `Engine::generate` (lib.rs:300) has no doc comment at all.
- fix: Add `# Errors` sections to `generate`, `SchemaValidator::new`, `Scrubber::new`, `mb_state::connect`, the store and bus `connect` functions, and `Leadership::start`.
- risk: Docs only.

### MB-ENGINE-STYLE-8: Many top-level public items are undocumented, and missing_docs is off
- file: crates/mb-state/src/lib.rs:35
- dimension: lint; guideline: Google:public-docs; severity: low; semver: none; effort: M; classification: n/a
- evidence: StateError, SessionKey, StoredResource, SessionInfo and ChangeKind (mb-state lib.rs:35-64), EngineError (mb-engine lib.rs:45), and many mb-mcm types have no docs. The workspace sets only `[workspace.lints.clippy] unwrap_used = "deny"` (Cargo.toml:126-127).
- fix: Write the docs first, then enable `missing_docs = "warn"` for these crates.
- risk: Turning on the lint before the docs exist fails CI.

### MB-ENGINE-STYLE-9: Leadership releases its Redis lease only by a spawn in Drop
- file: crates/mb-state/src/leader.rs:160
- dimension: api; guideline: C-DTOR-BLOCK; severity: low; semver: additive; effort: S; classification: n/a
- evidence: Drop calls `rt.spawn(async move { lease.release().await })` only when `Handle::try_current()` succeeds. During runtime shutdown the spawned task may never run, and followers then wait out the lease TTL. The public API has no awaitable release (leader.rs:91, 101, 151, 155).
- fix: Add `pub async fn release(&self)` that takes the lease from a Mutex/Option and awaits it with a timeout. Leadership is held in an `Arc` (mockingbird/src/config.rs:629), so `release(self)` would need `Arc::try_unwrap`; use `&self`. Call it on graceful shutdown.
- risk: Additive.

### MB-ENGINE-STYLE-10: LeaderBackend::Manual is doc(hidden) and called test-only, but production code uses it
- file: crates/mb-state/src/leader.rs:41
- dimension: api; guideline: C-HIDDEN; severity: low; semver: none (corrected from internal-only: un-hiding is docs only); effort: S; classification: n/a
- evidence: The doc comment says "tests flip it by hand" and the variant is `#[doc(hidden)]`, yet mockingbird/src/config.rs:625 builds `LeaderBackend::Manual(watch::channel(false).1)` for `Role::Data`.
- fix: Remove `#[doc(hidden)]` and document the data-node use. Moving the test-only hooks (LeaderTimings, start_with, the bus `connect_with_*`) behind a `testing` feature is a separate 0.2 change.
- risk: None for the un-hide.

### MB-ENGINE-STYLE-11: Six src test modules allow unwrap one by one instead of using allow-unwrap-in-tests
- file: crates/mb-engine/src/compact.rs:215
- dimension: lint; guideline: Style:attr-outer-preferred; severity: low; semver: none; effort: S; classification: n/a
- evidence: `#[allow(clippy::unwrap_used)]` appears at compact.rs:215, encoded.rs:217, budget.rs:527, persona.rs:386 and validate.rs:428, plus an inner `#![allow]` at headers.rs:105. The repo has no clippy.toml.
- fix: Add a root `clippy.toml` with `allow-unwrap-in-tests = true` and remove these attributes. Keep the integration-test `#![allow]`s until you confirm the setting covers helper functions.
- risk: Workspace-wide, but it relaxes test code only.

### MB-ENGINE-STYLE-12: Two private too_many_arguments allows could become parameter structs
- file: crates/mb-engine/src/headers.rs:43
- dimension: style; guideline: StyleGuide:too_many_arguments; severity: low; semver: internal-only; effort: S; classification: n/a
- evidence: `#[allow(clippy::too_many_arguments)]` appears at headers.rs:43 (`header_value`, which takes limit, remaining and window) and lib.rs:653 (`Engine::finish`).
- fix: Add a `RateLimit` struct and a private `Target<'a>` holding the validator, bundle, schema and op, then drop the allows.
- risk: Private refactor.

### MB-ENGINE-STYLE-13: Constructors named new return Arc<Self>
- file: crates/mb-engine/src/guards.rs:30
- dimension: api; guideline: C-CTOR; severity: low; semver: additive; effort: S; classification: n/a
- evidence: `GenerationLimiter::new(max) -> Arc<Self>` (guards.rs:30), `LocalBus::new -> Arc<Self>` (bus.rs:208) and `MemoryBusHub::new() -> Arc<Self>` (bus.rs:240).
- fix: Additive now: add `new_shared` and deprecate `new` (migrating the workspace callers). In 0.2, make `new` return `Self` and add `Default` for MemoryBusHub.
- risk: Changing the return type would be breaking.

### MB-ENGINE-STYLE-14: lib.rs files hold most of the API
- file: crates/mb-engine/src/lib.rs:239
- dimension: style; guideline: Google:no-api-in-nonleaf; severity: low; semver: none; effort: M; classification: n/a
- evidence: mb-engine/src/lib.rs is 1332 lines and defines Engine with its full impl next to 16 module declarations. mb-state/src/lib.rs defines the StateStore trait, its value types and `connect`.
- fix: Move these into `engine.rs`, `error.rs` and `store.rs`, and re-export them, so public paths do not change.
- risk: Merge conflicts with in-flight PRs.

### MB-ENGINE-STYLE-15: Bus mutexes ignore poison at six sites with no stated invariant
- file: crates/mb-state/src/bus.rs:250
- dimension: lint; guideline: Google:no-poison-internals; severity: low; semver: internal-only; effort: S; classification: n/a
- evidence: `.unwrap_or_else(PoisonError::into_inner)` appears at bus.rs:250, 262, 353, 359, 626 and 645.
- fix: Add one private `lock()` helper with a comment explaining why poison is safe to ignore (each critical section is a single assignment or push).
- risk: Behaviour-neutral.

### MB-ENGINE-STYLE-16: Public functions return &Vec<T> or clone a Vec where a slice or iterator would do
- file: crates/mb-engine/src/consistency.rs:31
- dimension: api; guideline: Google:return-iterators; severity: low; semver: additive; effort: S; classification: n/a
- evidence: `pub fn list_items(body: &Value) -> Option<&Vec<Value>>` (consistency.rs:31), and `Channel::effective_triggers(&self) -> Vec<Trigger>`, which clones `self.triggers` (mb-mcm lib.rs:587).
- fix: Add slice and iterator variants now, and change the return types in 0.2.
- risk: Additive if the methods are added rather than changed.

### MB-ENGINE-STYLE-18: Crate docs are one line, with no examples and sparse Cargo metadata
- file: crates/mb-engine/Cargo.toml:1
- dimension: api; guideline: C-CRATE-DOC; severity: low; semver: none; effort: S; classification: n/a
- evidence: `[workspace.package]` (Cargo.toml:5-10) has no keywords, categories, readme or homepage, and the crates have no README. The descriptions already say "Internal crate ... no API stability guarantee".
- fix: Expand the `//!` docs to state the internal status, link to mockingbird-cli and mb-ext, and add a small `no_run` example plus the missing metadata.
- risk: Docs and metadata only.

### MB-ENGINE-STYLE-19: Consuming builders and pure accessors lack #[must_use]
- file: crates/mb-engine/src/guards.rs:104
- dimension: api; guideline: Google:must-use; severity: low; semver: additive; effort: S; classification: n/a
- evidence: There is no `#[must_use]` in mb-engine or mb-mcm. `Engine::with_guards(mut self, ..) -> Self` (guards.rs:104) and `Faker::with_hints(mut self, ..) -> Self` (faker.rs:120) discard the configuration when their result is ignored.
- fix: Add `#[must_use]` to the consuming builders, and to pure functions such as `Source::worst`, `Engine::conform_item` and `Bundle::with_defs`.
- risk: Additive. It can only add warnings at call sites that misuse these functions.

## Stubs and doc-promised behaviour

### MB-ENGINE-STUB-1: The output validator never asserts `format`, so the Format repair arm is dead code
- file: crates/mb-engine/src/validate.rs:29
- dimension: stub; guideline: StubAudit:doc-promises-behaviour; severity: medium (was high: it is a quality gap, not a crash); semver: none; effort: S; classification: finish
- evidence: `jsonschema::draft202012::new(schema_with_defs)` is built without `should_validate_formats`. In jsonschema 0.60.2 (the version in Cargo.lock), `formats_are_assertions_by_default` (compiler.rs:66-67) is true only for Draft 4, 6 and 7. So `ValidationErrorKind::Format` at validate.rs:60 can never fire, even though the doc comment at validate.rs:40 lists `format` among the patched keywords. The workspace never calls `should_validate_formats`.
- fix: Add a format-asserting constructor (`draft202012::options().should_validate_formats(true).build(..)`) and use it for generated-output validators. Decide separately for request validation in mb-protocols. Add tests that check invalid `date-time` and `uuid` values get patched.
- risk: Faker formats must pass the asserting validator, or fallbacks start failing. Expect more `repaired` answers. Run mb-evals before and after.

### MB-ENGINE-STUB-2: After 12 attempts the faker silently returns a schema-invalid body
- file: crates/mb-engine/src/lib.rs:846
- dimension: stub; guideline: StubAudit:silent-fallback; severity: medium (was high); semver: internal-only; effort: S; classification: finish
- evidence: After FALLBACK_ATTEMPTS the code calls `tracing::warn!("faker output does not validate against the schema")` and returns the first invalid candidate. README.md:16 promises "falls back to a schema-valid faker", docs/concepts.md:23 describes the fallback as "a schema-valid answer", and the testing skill (line 10) says "always matches the schema". STUB-3's unsupported keywords make this path reachable.
- fix: Return a validity flag internally. When the value is invalid, set or extend `fallback_reason` and add a counter metric. Soften the docs to list the known gaps.
- risk: Low.

### MB-ENGINE-STUB-3: The faker ignores minProperties, additionalProperties and patternProperties schemas, prefixItems, contains, not and if/then/else
- file: crates/mb-engine/src/faker.rs:233
- dimension: stub; guideline: StubAudit:incomplete-implementation; severity: medium; semver: none; effort: M; classification: finish
- evidence: Searching faker.rs for these keywords as schema keys finds nothing; the matches are only Rust `contains` calls. `fake_object` reads only properties and required, and `fake_array` reads items, minItems, maxItems and uniqueItems. The importers emit `additionalProperties: <schema>` for maps (asyncapi/avro.rs) and pass `prefixItems` and `patternProperties` through.
- fix: Generate map entries for additionalProperties and patternProperties, and honour min/maxProperties. Generate prefixItems positionally and satisfy `contains`. Leave `not` and `if/then/else` to the retry loop and document them. Add proptest cases.
- risk: Offline output changes for the affected schemas. The node budget must bound map sizes.

### MB-ENGINE-STUB-4: StatusSelector cannot represent 2XX or 4XX, and the importer drops those responses silently
- file: crates/mb-mcm/src/lib.rs:303 (the drop happens at crates/mb-import/src/openapi3.rs:294)
- dimension: stub; guideline: StubAudit:silent-fallback; severity: medium; semver: internal-only; effort: S; classification: finish
- evidence: `enum StatusSelector { Code(u16), Default }`, and openapi3.rs:294 has `Err(_) => continue, // ranges like 2XX are not representable` with no warning. An operation whose only success response is `2XX` hits `EngineError::NoOutput` (mb-engine lib.rs:314).
- fix: In the importer, map `NXX` to a concrete code when that code is not already declared, and emit a warning. A `Range` variant would be a 0.2 change, since the enum is exhaustive.
- risk: Low for the importer mapping.

### MB-ENGINE-STUB-5: HttpMethod has no TRACE, and OpenAPI trace operations are dropped silently
- file: crates/mb-mcm/src/lib.rs:160
- dimension: stub; guideline: StubAudit:silent-fallback; severity: low; semver: breaking-needs-0.2 (the variant); internal-only for the warning; effort: S; classification: ask-user
- evidence: HttpMethod has 7 variants. mb-import openapi3.rs `METHODS` has 7 entries and no `trace`, while smart/embedded.rs:48 and smart/merge.rs:209 do list `trace`.
- fix: Emit an `unsupported_method` import warning now. Add `Trace` together with `#[non_exhaustive]` in 0.2.
- risk: Low for the warning.

### MB-ENGINE-STUB-6: Session reset, seed and mock reset clear only the local node's response cache
- file: crates/mb-state/src/cache.rs:330
- dimension: stub; guideline: StubAudit:doc-promises-behaviour; severity: medium; semver: breaking-needs-0.2 (enum route); none for the Resync route; effort: M; classification: finish
- evidence: `CachingStateStore::reset` and `reset_mock` (cache.rs:330-340) clear only the local cache. `BusStateStore` (mockingbird/src/ha.rs:306-326) only delegates reset, reset_mock and seed without publishing anything. The cache watcher (cache.rs:266-286) clears on `StateChanged` or lag, and BusEvent has no session-reset variant. docs/configuration.md (the `consistency` row) says the cache is "cleared ... by a session reset".
- fix: For 0.1, publish `BusEvent::Resync` after reset, reset_mock and seed, and make sure the remote handling clears the response cache. In 0.2, add a session-scoped bus event. Add a two-node MemoryBusHub test.
- risk: Resync clears every cache on every node, which costs repeat generations.

### MB-ENGINE-STUB-7: Bodyless responses are labelled Source::Llm even offline
- file: crates/mb-engine/src/lib.rs:327
- dimension: stub; guideline: StubAudit:silent-fallback; severity: low; semver: none; effort: S; classification: finish
- evidence: The no-schema branch (lib.rs:322-331) always returns `source: Source::Llm` with zero usage. This is already recorded in docs/improvements.md:96 (an offline bodyless response labelled `x-mockingbird-source: llm`) and, for the single-value case, at improvements.md:47.
- fix: When `self.provider.is_none()`, return `Source::Fallback` with `OFFLINE_REASON`. A `Contract` variant would be a 0.2 change.
- risk: Source-based filters and eval rates shift slightly.

### MB-ENGINE-STUB-8: A scenario `set` is silently ignored when the patched body would be invalid
- file: crates/mb-engine/src/lib.rs:1120
- dimension: stub; guideline: StubAudit:silent-fallback; severity: low; semver: none; effort: M; classification: finish
- evidence: On validation failure the code logs `warn!(.., "scenario set produces an invalid body; ignoring it")` (lib.rs:1120-1129) and returns the unpatched response. Non-object bodies return early at lib.rs:1108. The Admin API checks only that `set` is an object (mb-admin/src/scenarios.rs:287).
- fix: Validate `set` against the targeted operations at write time and return a warning. At runtime, add a response header or request-log field when the patch is dropped.
- risk: Stricter write-time checks could reject existing configs, so start with warnings.

### MB-ENGINE-STUB-9: The testing skill promises a deterministic offline mode, but the faker is unseeded
- file: plugins/mockingbird/skills/mockingbird-testing/SKILL.md:3
- dimension: stub; guideline: StubAudit:doc-promises-behaviour; severity: medium; semver: additive; effort: M; classification: ask-user
- evidence: The description says "deterministic offline mode", and line 10 says offline faker data "always matches the schema". `fake_value` uses `rand::rng()` (lib.rs:835), and no faker seed option exists anywhere in crates/.
- fix: Either add a faker seed derived from the mock, operation, session and request, or reword the skill and the CI docs. This is a product decision.
- risk: The seed option touches every faker call site. Rewording is docs only.

### MB-ENGINE-STUB-10: The documented Redis requirements are not checked at connect
- file: crates/mb-state/src/redis.rs:165
- dimension: stub; guideline: StubAudit:documented-refusal; severity: low; semver: none; effort: S; classification: keep-refusal
- evidence: docs/configuration.md:47 requires `maxmemory-policy noeviction` and no Cluster. mb-state never mentions cluster or maxmemory.
- fix: Run `INFO cluster` at connect and refuse when `cluster_enabled:1`. Warn when `CONFIG GET maxmemory-policy` is not `noeviction`. Treat command errors as unknown.
- risk: Managed Redis may block INFO or CONFIG, so never refuse when the check fails.

### MB-ENGINE-STUB-11: Event messages that fall back to the faker lose the reason
- file: crates/mb-engine/src/message.rs:77
- dimension: stub; guideline: StubAudit:silent-fallback; severity: low; semver: breaking-needs-0.2 (field); additive for the metric; effort: S; classification: finish
- evidence: The reason is only logged at message.rs:77. `GeneratedMessage` (message.rs:28) has payload, source and usage but no reason field, and the struct is exhaustive.
- fix: Add a `mb_message_fallback_total{reason}` metric now. Add a `fallback_reason` field in 0.2.
- risk: Adding the field is breaking.

### MB-ENGINE-STUB-12: The session_ttl docs say "last write", but any state-using request refreshes it
- file: docs/configuration.md:48
- dimension: stub; guideline: StubAudit:doc-promises-behaviour; severity: low; semver: none; effort: S; classification: finish
- evidence: The docs say "expires this long after its last write", but mb-protocols/src/call.rs calls `state.touch` on read paths (call.rs:629, 719, 730 and 767).
- fix: Reword to "after it was last used".
- risk: None.

## Tests

### MB-ENGINE-TESTS-1: memory_ttl_expiry burns about 1.9 s of real time
- file: crates/mb-state/tests/memory.rs:84
- dimension: tests; guideline: PERF:test-paused-clock; severity: medium; semver: none; effort: S; classification: n/a
- evidence: `ttl_expiry_in` sleeps 700, 600 and 600 ms with `tokio::time::sleep` (conformance.rs:453-458). MemoryStore's expiry runs on `tokio::time::Instant` (memory.rs:10, 19 and 74-75), and `expired_session_is_invisible` (tests/memory.rs:97) already uses `start_paused`.
- fix: Mark `memory_ttl_expiry` with `#[tokio::test(start_paused = true)]`. Do not pause `memory_conformance` or `connect_memory_url`: `sessions_lists_with_counts` orders by the `Utc` `last_touched` (conformance.rs:358-359), which paused tokio time does not advance.
- risk: Low.

### MB-ENGINE-TESTS-2: ttl_expiry hard-codes about 1.9 s of sleeps for SQL and Redis
- file: crates/mb-state/tests/common/conformance.rs:453
- dimension: tests; guideline: PERF:test-sleep-budget; severity: low (was medium); semver: none; effort: S; classification: n/a
- evidence: The fixed 700, 600 and 600 ms sleeps run against a 1 s TTL. The SQL and Redis stores use wall-clock timestamps.
- fix: Pass the TTL in, scale the sleeps to it, and use a shorter TTL (about 250 ms) for sqlite.
- risk: A shorter TTL risks flakes on a loaded runner, so keep proportional margins.

### MB-ENGINE-TESTS-3: assert_silent waits a real 300 ms in the in-process bus tests
- file: crates/mb-state/tests/bus.rs:58
- dimension: tests; guideline: PERF:test-paused-clock; severity: low; semver: none; effort: S; classification: n/a
- evidence: `timeout(300ms, rx.recv())` is used by `memory_hub_delivers_to_others_not_self` (bus.rs:63) and `local_bus_is_silent` (bus.rs:78). Both are in-process and use no sockets.
- fix: Add `start_paused = true` to these two tests.
- risk: Low.

### MB-ENGINE-TESTS-4: saturated_limiter_falls_back_within_timeout asserts a 300 ms wall-clock bound
- file: crates/mb-engine/tests/guards.rs:205
- dimension: tests; guideline: PERF:test-paused-clock; severity: medium; semver: none; effort: S; classification: n/a
- evidence: The test measures with `Instant::now()` and asserts `elapsed < 300ms` after a 200 ms queue timeout (guards.rs:202-209). SlowProvider sleeps with `tokio::time::sleep` (guards.rs:152).
- fix: Use `start_paused = true` with `tokio::time::Instant`, and assert a 200-300 ms window.
- risk: Low.

### MB-ENGINE-TESTS-5: dropping_stream_stops_provider sleeps a real 200 ms
- file: crates/mb-engine/tests/stream.rs:457
- dimension: tests; guideline: PERF:test-paused-clock; severity: low; semver: none; effort: S; classification: n/a
- evidence: The test calls `tokio::time::sleep(200ms)` after the drop. Sibling tests at stream.rs:362 and 393 already use `start_paused`.
- fix: Add `start_paused = true`.
- risk: Low.

### MB-ENGINE-TESTS-6: Faker tests assert wall-clock performance bounds
- file: crates/mb-engine/tests/faker.rs:461
- dimension: tests; guideline: TEST:no-wallclock-asserts; severity: medium; semver: none; effort: M; classification: n/a
- evidence: `costs_computed_once_per_response` asserts `elapsed < 300ms` in a debug build (faker.rs:457-464). Other tests in the file use 1-5 s bounds.
- fix: Assert a countable property through a `#[doc(hidden)]` counter, or relax the bound to 1 s and document it as a blow-up guard.
- risk: A relaxed bound weakens detection of small regressions.

### MB-ENGINE-TESTS-7: forced_status_uses_error_variant checks only that keys are present
- file: crates/mb-engine/tests/generate.rs:203
- dimension: tests; guideline: TEST:assert-body-not-shape; severity: low; semver: none; effort: S; classification: n/a
- evidence: The test checks only `body.get("code").is_some() && body.get("message").is_some()`.
- fix: `assert_eq!(body, json!({"code": 404, "message": "order not found"}))`.
- risk: None.

### MB-ENGINE-TESTS-8: Consolidate the integration-test binaries into tests/it per crate
- file: crates/mb-engine/tests/generate.rs:1
- dimension: tests; guideline: PERF:test-binary-consolidation; severity: low (was medium); semver: none; effort: M; classification: n/a
- evidence: mb-engine has 8 test binaries and mb-state has 6 (plus `common/`). Each one links the dependency graph separately.
- fix: Move the files into `tests/it/main.rs` modules and declare `mod common;` once for mb-state. Fix TESTS-4 and TESTS-6 first, because tests sharing a process contend for CPU.
- risk: Tests sharing a process can expose timing assumptions, and CI test filters must change to `--test it`.

### MB-ENGINE-TESTS-9: p4_bundle_without_channels_round_trips duplicates the p2 round-trip test
- file: crates/mb-mcm/tests/model.rs:380
- dimension: tests; guideline: TEST:no-duplicate-tests; severity: low; semver: none; effort: S; classification: n/a
- evidence: Both tests parse `P2_ORDERS` and assert a byte-equal round trip (model.rs:225 and 384). The p4 test's extra assertions follow from that.
- fix: Fold `channels.is_empty()` into the p2 test, or give p4 its own fixture.
- risk: None.

### MB-ENGINE-TESTS-10: connect_memory_url reruns the full conformance suite
- file: crates/mb-state/tests/memory.rs:89
- dimension: tests; guideline: TEST:no-duplicate-tests; severity: low; semver: none; effort: S; classification: n/a
- evidence: `mb_state::connect("memory")` returns a plain `Arc::new(MemoryStore::new(ttl))` (mb-state lib.rs:164-166), the same store that `memory_conformance` already tests.
- fix: Assert only the routing: connect, then ping, then one round trip.
- risk: Low.

### MB-ENGINE-TESTS-11: The elapsed assertion after wait_for repeats its timeout
- file: crates/mb-state/tests/leader.rs:120
- dimension: tests; guideline: TEST:no-tautology; severity: low; semver: none; effort: S; classification: n/a
- evidence: `wait_for(.., RENEW*2 + 300ms)` already panics at that bound, and the next line asserts the same bound.
- fix: Delete the line, or tighten it to a stricter bound.
- risk: None.
