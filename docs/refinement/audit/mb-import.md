# mb-import audit (verified)

Adversarially verified against the source at b9d4606 (read-only; nothing built or run). 43 findings kept, 11 dropped (two of them merged into kept findings) (listed at the end).

Context for semver labels: `mockingbird-import` 0.1.0 is published only for mockingbird-cli, and its Cargo description says "no API stability guarantee". The workspace pins it `=0.1.0`. Breaking items still go to 0.2.

### MB-IMPORT-PERF-1: XSD group / attributeGroup expansion is exponential
- file: crates/mb-import/src/wsdl/xsd.rs:455 (group_ref), also :567 (attribute_group)
- dimension: perf | guideline: PERF:unbounded-expansion | severity: high | semver: none | effort: M
- evidence: group_ref re-expands the referenced group's particles inline on every reference, and attribute_group does the same. The only guard is MAX_DEPTH=64 (check_depth). A chain of groups where each holds two refs to the next one costs 2^levels particle visits, which pins a blocking thread from a WSDL of a few KB. This runs on contract upload and during reconcile. Correction: the complexContent extension base (:650) is a single base per type, so it is a linear chain and not a fan-out. It does not need memoizing.
- fix: Memoize each named group and attributeGroup expansion (keyed by QName, optional and in_choice) in a RefCell<HashMap> on Xsd, then replay it into the Obj. Or add a total work counter (for example 1e6 particles) that returns ImportError::Invalid("schema expansion too large").
- risk: Low. Output for valid schemas is unchanged. Set the cap generously and check the existing WSDL fixtures.

### MB-IMPORT-PERF-2: compile_draft deep-clones all components into every validator
- file: crates/mb-import/src/smart/assemble.rs:589
- dimension: perf | guideline: PERF:needless-clone-quadratic | severity: medium (was high: an admin draft PUT/assemble path, not request serving) | semver: none | effort: M
- evidence: For every schema location, with_defs(…, &defs) clones the whole defs map into $defs (526-531) and calls jsonschema::validator_for on it. The per-component loop (577-587) also clones the stubs map and the schema, once per component. The cost is O(locations x components size) on every draft PUT (mb-admin drafts.rs:399, inside spawn_blocking), every assemble and every merge. The GB-scale numbers in the original finding are estimates and were not measured.
- fix: Register the defs once as a shared resource or registry (jsonschema options with_resource) and compile each location against it without cloning. For the per-component check, validate all components together and fall back to per-component checks only when that fails.
- risk: Medium. Issue pointers and error texts are snapshot-tested, and $ref resolution through an external resource differs slightly.

### MB-IMPORT-PERF-3: bundle_warnings serializes each output's reachable-defs closure, and drafts compute it twice
- file: crates/mb-import/src/lib.rs:234
- dimension: perf | guideline: PERF:redundant-serialization | severity: medium | semver: none | effort: S
- evidence: For every output, bundle.with_reachable_defs(schema) clones the transitive def closure (mb-mcm lib.rs:391-405), and `.to_string().len()` serializes it only to measure its size. mb-admin drafts.rs:405-406 calls schema_warnings (which calls bundle_warnings) and then bundle_warnings again on the same bundle, on the async thread. schema_warnings (assemble.rs:630-636) also does a linear operations.find for each warning.
- fix: Count bytes with serde_json::to_writer into a counting io::Write instead of building a String, and cache sizes per def name. Have drafts.rs reuse the first result. Build a HashMap of id to operation in schema_warnings.
- risk: Low. The counting writer gives the same byte count as to_string().len().

### MB-IMPORT-PERF-4: sniff parses the whole document, then compile parses it again; download handler sniffs on the async thread
- file: crates/mb-import/src/lib.rs:137
- dimension: perf | guideline: PERF:double-parse | severity: medium | semver: none (an extra pub sniff_head would be additive) | effort: S
- evidence: sniff runs serde_json::from_slice::<Value> or serde_saphyr::from_slice over the whole input. compile (178-189) then calls parse_document again, so YAML is parsed twice. mb-admin contracts.rs:555/601 (source_extension, and again at :638) calls mb_import::sniff on the stored source in the async GET download handler with no spawn_blocking. imports/mod.rs:363 does the same on an error path.
- fix: Add a private sniff_parsed(bytes) -> (SourceFormat, Option<Value>) and have compile reuse the Value. Keep pub sniff as a wrapper. For the download filename, store the detected format with the version, or run the sniff in spawn_blocking.
- risk: Low.

### MB-IMPORT-PERF-5: detect_kind builds a full Value of up to 20 MiB on the caller's async thread
- file: crates/mb-import/src/smart/mod.rs:101 (json_kind)
- dimension: perf | guideline: PERF:blocking-on-async | severity: low | semver: none | effort: S
- evidence: json_kind runs serde_json::from_slice::<Value> over the whole input only to check /log/entries and /info. mb-admin imports/mod.rs:360/430 (document_job) and mockingbird client.rs:605 call it synchronously. The HAR and Postman importers then parse the bytes again.
- fix: Deserialize into a probe struct (with IgnoredAny for everything else) so no Value tree is built.
- risk: Low.

### MB-IMPORT-PERF-6: a new reqwest::Client with re-parsed PEM roots is built for every fetch hop
- file: crates/mb-import/src/smart/fetch.rs:359
- dimension: perf | guideline: PERF:per-call-rebuild | severity: low | semver: none | effort: S
- evidence: pinned_client builds a fresh Client for every hop, redirects included, and calls Certificate::from_pem_bundle(&policy.extra_roots_pem) each time (368-371).
- fix: Parse extra_roots_pem once into a Vec<Certificate> held next to the policy. Keep the client per hop, since the address pinning needs it.
- risk: Low. DNS-rebinding pinning stays per hop.

### MB-IMPORT-PERF-7: HAR path templating compares response shapes all-pairs
- file: crates/mb-import/src/smart/paths.rs:213
- dimension: perf | guideline: PERF:quadratic-scan | severity: low | semver: none | effort: S
- evidence: The Vec::contains dedup (209) is linear, and the all-pairs Shape::compatible check (213-216) does BTreeSet work per pair. It repeats per position and per fixpoint iteration. It is quadratic only in the number of distinct shapes per group, which is usually small.
- fix: Dedup through a set (derive Ord or Hash on Shape) and cap the number of distinct shapes compared.
- risk: Low. Keep the HAR snapshots green.

### MB-IMPORT-PERF-8: walk_refs allocates an escaped pointer String for every visited node
- file: crates/mb-import/src/schema.rs:157
- dimension: perf | guideline: PERF:needless-alloc | severity: low | semver: none | effort: S
- evidence: The pointer `at: String` is passed by value, so every object child that recurses runs `child()` (format plus two replace calls), and every array element runs format!. The pointer is used only when a ref is collected.
- fix: Keep a Vec<&str> segment stack and build the pointer String only when a $ref is pushed.
- risk: Very low.

### MB-IMPORT-PERF-9: schema_of clones up to 20 sample bodies only to borrow them again
- file: crates/mb-import/src/smart/infer.rs:32
- dimension: perf | guideline: PERF:needless-clone | severity: low | semver: none | effort: S
- evidence: schema_of clones each body into a Vec<Value>, and infer_schema (25-27) immediately collects &Value references and calls infer(&refs, 1).
- fix: Call `infer(&values[..values.len().min(MAX_SCHEMA_SAMPLES)], 1)` directly.
- risk: None.

### MB-IMPORT-PERF-12: interface implementors are found by scanning all types once per interface
- file: crates/mb-import/src/graphql.rs:496
- dimension: perf | guideline: PERF:quadratic-scan | severity: low | semver: none | effort: S
- evidence: def_schema, called once per type from compile_text (610-613), filters schema.types for implementors of every Interface. The cost is O(interfaces x types x implements).
- fix: Precompute a BTreeMap of interface to implementors once in compile_text and pass it in.
- risk: None. The output order is unchanged.

### MB-IMPORT-STYLE-1: public enums are exhaustive
- file: crates/mb-import/src/lib.rs:53
- dimension: api | guideline: Google:non-exhaustive-enums | severity: medium | semver: breaking-needs-0.2 | effort: S
- evidence: There is no #[non_exhaustive] anywhere in src. ImportError (lib.rs:53), SourceFormat (:29), SmartError (smart/mod.rs:49), FetchError (smart/fetch.rs:201), ImportKind (smart/model.rs), Phase (smart/mod.rs:171) and AllowEntry (fetch.rs:62) are pub and reachable. SourceFormat grows with each importer, and mb-admin contracts.rs:601-612 matches it exhaustively.
- fix: In 0.2, add #[non_exhaustive] to these enums and add `_ =>` arms in mb-admin and mockingbird in the same PR.
- risk: Low at runtime.

### MB-IMPORT-STYLE-2: public fallible APIs return String errors
- file: crates/mb-import/src/triggers.rs:13
- dimension: api | guideline: C-GOOD-ERR | severity: low | semver: breaking-needs-0.2 | effort: M
- evidence: validate_triggers -> Result<(), String> is re-exported at lib.rs:26. AllowEntry's FromStr has Err = String (fetch.rs:72), and FetchPolicy::from_allowlist returns Result<Self, String> (fetch.rs:138). triggers.rs:20 hardcodes "100ms" instead of formatting MIN_SCHEDULE_INTERVAL.
- fix: Add thiserror enums with Display text identical to the current strings, and switch to them in 0.2. Optionally add try_ variants in 0.1.x. Format the minimum interval from the constant.
- risk: Admin API problem+json texts must not change.

### MB-IMPORT-STYLE-3: SmartError::Fetch drops the typed FetchError, and NoOperations carries punctuation glue
- file: crates/mb-import/src/smart/mod.rs:66
- dimension: api | guideline: C-GOOD-ERR | severity: low | semver: breaking-needs-0.2 | effort: S
- evidence: From<FetchError> stringifies the error (66-70). NoOperations(String) is built with "" (607) or ": a; b" (637-639).
- fix: Use `Fetch(#[from] FetchError)` and `NoOperations { reasons: Vec<String> }`, with a Display that keeps the current text.
- risk: Display stays identical if it is formatted as described.

### MB-IMPORT-STYLE-4: nine smart submodules are pub only for integration tests
- file: crates/mb-import/src/smart/mod.rs:5
- dimension: api | guideline: C-HIDDEN | severity: medium | semver: breaking-needs-0.2 | effort: M
- evidence: chunk, complete, embedded, har, infer, llm, paths, postman and text have no users outside crates/mb-import/tests (grep across crates/). ExtractLlm is re-exported at smart::ExtractLlm, so llm could become private without affecting mb-admin. These modules expose TokenBudget's pub fields, ChunkResult, Chunk, TextDoc and Block as published API.
- fix: In 0.1.x, mark them #[doc(hidden)]. In 0.2, make them pub(crate) and move their tests into unit tests or behind a test-internals feature.
- risk: The integration tests that import them must move.

### MB-IMPORT-STYLE-5: report()'s doc comment is attached to compile_error()
- file: crates/mb-import/src/smart/mod.rs:219
- dimension: style | guideline: Google:doc-comment-syntax | severity: low | semver: none | effort: S
- evidence: Lines 219-220 document report, but they sit above line 221 (compile_error's doc) and compile_error itself. report at :231 has no doc.
- fix: Move lines 219-220 down to sit above `pub(crate) async fn report`.
- risk: None.

### MB-IMPORT-STYLE-6: missing rustdoc on public items and thin crate docs (merged with STYLE-13)
- file: crates/mb-import/src/lib.rs:29
- dimension: api | guideline: C-FAILURE / C-CRATE-DOC | severity: low | semver: none | effort: M
- evidence: SourceFormat (:29), ImportError (:53) and its variants, SmartError (smart/mod.rs:49), FetchError (fetch.rs:201), DraftWarning::new/for_operation and TokenBudget::new/add have no ///. The crate has no `# Errors` sections. The crate doc (lib.rs:1-3) is a single sentence, with no example, no mention of the `smart` feature and no stability note.
- fix: Document the error enums and the compile, sniff, compile_files, unpack_archive and read_source entry points with `# Errors` sections. Expand the //! docs with a `?` example, a Features section and the stability note. Enable missing_docs only after the docs land, since -D warnings would fail CI.
- risk: Docs only.

### MB-IMPORT-STYLE-7: public types missing Debug
- file: crates/mb-import/src/smart/llm.rs:37
- dimension: api | guideline: C-DEBUG | severity: low | semver: additive | effort: S
- evidence: ExtractLlm derives only Clone (it holds Arc<dyn Provider>). ChunkResult (llm.rs:212), SystemResolver (fetch.rs:181) and NoopObserver (mod.rs:200) derive nothing.
- fix: Derive Debug, Clone, Copy and Default on the two unit structs and Debug on ChunkResult. Write a manual Debug for ExtractLlm that omits the provider (or prints only its name), after checking that HopSettings holds nothing secret.
- risk: Additive.

### MB-IMPORT-STYLE-8: ImportKind::parse/as_str duplicate FromStr/Display
- file: crates/mb-import/src/smart/model.rs:41
- dimension: api | guideline: Google:std-traits | severity: low | semver: additive | effort: S
- evidence: `pub fn parse(&str) -> Option<Self>` and `as_str` are ad-hoc versions of FromStr and Display. Phase::as_str (smart/mod.rs:183) is the same pattern.
- fix: Implement FromStr and Display that delegate to these functions, and keep the existing functions.
- risk: Additive. If parse is deprecated, update the mb-admin callers in the same PR because of -D warnings.

### MB-IMPORT-STYLE-9: no #[must_use] on the consuming builder DraftWarning::for_operation
- file: crates/mb-import/src/smart/model.rs:170
- dimension: api | guideline: Google:must-use | severity: low | semver: additive | effort: S
- evidence: There are 0 #[must_use] attributes in src. `for_operation(mut self, …) -> Self` can be called as a statement and the result silently dropped.
- fix: Add #[must_use] to for_operation, and optionally to the pure queries (can_afford, needs_llm, detect_kind).
- risk: Can surface warnings at call sites, which -D warnings turns into errors. Fix them in the same PR.

### MB-IMPORT-STYLE-10: Builder::body has 7 parameters plus a bare bool, under allow(too_many_arguments)
- file: crates/mb-import/src/wsdl/mod.rs:500
- dimension: lint | guideline: Google:enum-over-bool | severity: low | semver: internal-only | effort: S
- evidence: `fn body(&self, xsd, service, op_name, rpc: bool, side, msg, binding_op)` is called twice in a row (585-594), and only side and msg differ between the two calls.
- fix: Add a private OpContext struct (with a SoapStyle enum in place of the bool), build it once, and remove the allow.
- risk: Private refactor, covered by the tests/wsdl.rs snapshots.

### MB-IMPORT-STYLE-11: dead clippy::expect_used allows; no lint enforces the "no expect()" rule
- file: crates/mb-import/tests/smart_complete.rs:2
- dimension: lint | guideline: StyleGuide:clippy-config | severity: low | semver: internal-only | effort: S
- evidence: The workspace denies only unwrap_used (Cargo.toml:126-127), and there is no clippy.toml. So the expect_used allows in smart_complete, smart_embedded, smart_har, smart_import, smart_llm and smart_postman do nothing. smart_complete, smart_embedded and smart_llm have 0 .expect calls. AGENTS.md forbids expect() outside tests, but nothing enforces it.
- fix: Add expect_used = "deny" to the workspace and a clippy.toml with allow-unwrap-in-tests and allow-expect-in-tests. Then drop the inner allows in src/smart/pdfscan.rs:391 and src/wsdl/xml.rs:310.
- risk: A workspace-wide lint change that needs a sweep of the other crates first.

### MB-IMPORT-STYLE-12: docs.rs will not render the `smart` module; crate metadata is incomplete
- file: crates/mb-import/Cargo.toml:1
- dimension: build-ci | guideline: C-METADATA | severity: low | semver: none | effort: S
- evidence: `#[cfg(feature = "smart")] pub mod smart;` (lib.rs:13-14) holds most of the API, and there is no [package.metadata.docs.rs]. There are no keywords or categories, while mb-ext/Cargo.toml:9-10 has them.
- fix: Add `[package.metadata.docs.rs] all-features = true` (plus the docsrs cfg and doc(cfg) on the mod) and add keywords and categories.
- risk: Metadata only. It takes effect on the next publish.

### MB-IMPORT-STUB-1: GraphQL validate() allows duplicate field names, and serve then panics
- file: crates/mb-import/src/graphql.rs:325
- dimension: stub | guideline: correctness | severity: high | semver: internal-only | effort: S | classification: finish
- evidence: validate checks only that field and argument types are known (325-333). merge extends field, value and member lists from `extend` blocks without de-duplicating (171-176). Duplicate types and duplicate schema definitions are rejected (218, 240), but duplicate fields are not. docs/improvements.md records octokit's schema compiling and then async-graphql panicking with "Field … already exists".
- fix: After the extension merge, reject a duplicate name per type with `invalid("/types/{T}/fields/{f}", "the field is defined more than once")`. Apply the same check to interface fields, input fields, enum values, union members and arguments.
- risk: Schemas that compile today get rejected, but those schemas already crash serve.

### MB-IMPORT-STUB-2: multi-file OpenAPI/Swagger/AsyncAPI with external $refs cannot be imported
- file: crates/mb-import/src/lib.rs:321
- dimension: stub | guideline: completeness | severity: high | semver: internal-only | effort: L (the message-only part is S) | classification: finish
- evidence: compile_files handles multiple files only for proto, WSDL and GraphQL. Any other multi-file archive gets "archive contains no recognised contract" (319-322). On a single file, check_refs (openapi3.rs:396-407) reports `./schemas/pet.yaml#/Pet` as an "unresolvable $ref". docs/protocols.md Known limits does not mention this.
- fix: Short term: for refs that do not start with `#`, say "external $ref is not supported; bundle the spec (e.g. redocly bundle)", and document the limit. Long term: add a bundling pass in compile_files that resolves relative-file refs against the uploaded files, with cycle and traversal guards. Never fetch URLs.
- risk: The bundler needs deterministic naming so source_digest stays stable. The message-only change carries no risk.

### MB-IMPORT-STUB-3: OpenAPI response ranges (2XX/4XX/5XX) are silently dropped
- file: crates/mb-import/src/openapi3.rs:294
- dimension: stub | guideline: completeness | severity: medium | semver: internal-only | effort: S | classification: finish
- evidence: `Err(_) => continue, // ranges like 2XX are not representable`, with no warning. docs/improvements.md cites Discord's spec, where 4XX does not count as declaring 404.
- fix: Expand a range into representative codes the operation does not already declare (explicit codes take precedence), and emit a warning (see STUB-7). StatusSelector::Range would be a breaking change in mb-mcm.
- risk: Scenario status checks become more permissive.

### MB-IMPORT-STUB-4: dangling local $refs fail the whole compile
- file: crates/mb-import/src/openapi3.rs:404
- dimension: stub | guideline: completeness | severity: medium | semver: internal-only | effort: S | classification: finish
- evidence: check_refs returns Invalid("unresolvable $ref referenced from …") for any ref that does not resolve. docs/improvements.md records Square's api.json failing this way and asks for "an open schema with a warning".
- fix: Stub a dangling `#/components/schemas/X` as a `true` def (the same approach as assemble.rs) and push a dangling_ref warning. Keep non-local refs as errors (STUB-2).
- risk: Lenient handling hides typos, so the warning must be visible in the upload response. improvements.md already states the preference.

### MB-IMPORT-STUB-5: Swagger basePath and OpenAPI servers[].url paths are ignored
- file: crates/mb-import/src/swagger2.rs:236
- dimension: stub | guideline: completeness | severity: medium | semver: none | effort: M | classification: ask-user
- evidence: Nothing in mb-import (outside asyncapi and smart) or in the other crates reads basePath or servers. Operations mount at the raw paths key. The only workaround is path_rewrite.add_prefix (docs/configuration.md:241).
- fix: Decide between (a) prefixing routes with the base path (opt-in per mock) and (b) documenting in docs/protocols.md that base paths are ignored, with the add_prefix recipe.
- risk: Option (a) changes the routes of existing mocks.

### MB-IMPORT-STUB-6: archives with incidental files (README, __MACOSX) are refused or mis-compiled
- file: crates/mb-import/src/lib.rs:319
- dimension: stub | guideline: completeness | severity: medium | semver: internal-only | effort: S | classification: finish
- evidence: Case 4 of compile_files accepts only `[one]`, so openapi.yaml plus README.md fails. unpack_archive (archive.rs Members::add) keeps `__MACOSX/._x.proto` AppleDouble entries. They match the `.proto` filter (lib.rs:277-280) and break the shared-dir strip (archive.rs:88-94). pack_dir skips hidden files (archive.rs:258), but unpack does not.
- fix: Drop `__MACOSX/` entries and dot-prefixed path components when unpacking. In case 4, keep only the files that sniff() as a contract, and error only when zero or several remain, naming the candidates.
- risk: Low. This matches pack_dir.

### MB-IMPORT-STUB-7: deterministic compilers have no warning channel for silent skips
- file: crates/mb-import/src/lib.rs:226
- dimension: stub | guideline: completeness | severity: medium | semver: additive | effort: M | classification: finish
- evidence: bundle_warnings emits only schema_too_large. Silent drops include response ranges (openapi3.rs:294), headers whose deref fails (`.ok()?` at :338), cookie params (only path, query and header are read, :483-485), unmappable security schemes (security.rs:44-50, 74-95), and xs:any (xsd.rs:449 `_ => {}`). The Warning type already reaches the Admin API through compile_with_warnings.
- fix: Thread a Vec<Warning> through an internal Ctx and add codes such as status_range_dropped, security_scheme_unsupported, cookie_param_ignored and xsd_wildcard_ignored. STUB-3 and STUB-8 build on this.
- risk: Existing contracts gain warnings. Make sure contract-upload warnings never block activation.

### MB-IMPORT-STUB-8: an unsupported scheme inside an AND security requirement is dropped, weakening it
- file: crates/mb-import/src/security.rs:48
- dimension: stub | guideline: correctness | severity: medium | semver: internal-only | effort: S | classification: finish
- evidence: map_scheme returns None for http digest and other schemes, mutualTLS, and apiKey with an unknown `in` (74-95). Those legs are dropped from the requirement. When every leg is dropped, the requirement disappears (51-53), and an operation whose only requirement it was becomes unauthenticated.
- fix: Emit a security_scheme_unsupported warning first (STUB-7). Then consider treating an unknown scheme as "any credential present" rather than dropping it. Document the supported scheme set.
- risk: Stricter enforcement could reject requests that pass today, so ship the warning first.

### MB-IMPORT-STUB-9: OpenAPI 3.0 `x-webhooks` is not read
- file: crates/mb-import/src/openapi3.rs:569
- dimension: stub | guideline: completeness | severity: low | semver: internal-only | effort: S | classification: finish
- evidence: compile_webhooks reads only `doc.get("webhooks")` (570). docs/improvements.md notes that GitHub's 3.0.3 spec uses x-webhooks.
- fix: Fall back to `x-webhooks`, with the pointer prefix to match, and add a fixture test.
- risk: Webhook bodies change for contracts that use x-webhooks.

### MB-IMPORT-STUB-10: WSDL 1.1 wsdl:import is unsupported and undocumented
- file: crates/mb-import/src/lib.rs:296
- dimension: stub | guideline: completeness | severity: low | semver: internal-only | effort: M | classification: finish
- evidence: wsdl/mod.rs `locations` follows only XSD import, include and redefine (179-186). More than one .wsdl is rejected (lib.rs:288-298). docs/protocols.md Known limits lists only WSDL 2.0 and external XSDs.
- fix: Follow wsdl:import through the upload map, with the file holding wsdl:service as the root. Until then, document the limit and name wsdl:import in the error.
- risk: Medium for the full fix, because namespace-dropping lookups may collide.

### MB-IMPORT-STUB-11: an unsupported AsyncAPI protocol gets a misleading "add channel bindings or servers" error
- file: crates/mb-import/src/asyncapi/mod.rs:434
- dimension: stub | guideline: error-quality | severity: low | semver: internal-only | effort: S | classification: keep-refusal
- evidence: Any protocol other than kafka, amqp, mqtt or ws hits `_ => Err(invalid(at, NO_PROTOCOL))`, even when the server declares e.g. nats. binding_protocol (bindings.rs:15-22) returns None for two known bindings, which falls back to the same message.
- fix: Keep the refusal, but name the cause: "server X uses protocol p; supported: kafka, amqp, mqtt, ws", and "channel has both kafka and ws bindings; keep one".
- risk: None.

### MB-IMPORT-STUB-12: embedded Swagger 2.0 blocks are not converted
- file: crates/mb-import/src/smart/embedded.rs:190
- dimension: stub | guideline: completeness | severity: low | semver: internal-only | effort: M | classification: finish
- evidence: A swagger: "2.0" block only yields an embedded_swagger2 warning (190-194), and its operations go to the LLM as text. This is documented in docs/import.md.
- fix: Convert Swagger 2.0 to OpenAPI 3.1 (definitions, body and formData params, produces, basePath) and feed the result through the embedded path.
- risk: Conversion gaps are contained by the per-operation invalid_operation fallback.

### MB-IMPORT-TESTS-1: 100 ms wall-clock stall budget is a flake source under parallel load
- file: crates/mb-import/tests/smart_import.rs:747
- dimension: tests | guideline: tests:flaky-timing | severity: medium | semver: none | effort: S
- evidence: longest_stall (714-744) asserts that the max gap of a 1 ms ticker on a current_thread runtime stays under MAX_STALL=100ms, in the 8 MiB HTML test (750) and the 1500-entry HAR test (783). The binary has 34 tests running in parallel on shared CI cores, and its own spawn_blocking work competes for the CPU.
- fix: Widen MAX_STALL to about 500 ms (a regression that blocks the runtime stalls for seconds), or count ticks during the import instead of taking the max gap. Optionally put these two tests in a serial nextest group.
- risk: Test-only. Detection is a little weaker.

### MB-IMPORT-TESTS-3: the 8 MiB HTML extraction test is likely slow in a debug build
- file: crates/mb-import/tests/smart_import.rs:752
- dimension: tests | guideline: tests:slow | severity: low (unmeasured) | semver: none | effort: S
- evidence: The test builds an 8 MiB HTML document that goes through scraper/html5ever at opt-level 0 (only rsa and num-bigint-dig have dev overrides, Cargo.toml:130-133). Its purpose, showing that extraction runs off the runtime thread, does not need 8 MiB.
- fix: Shrink the input to the smallest size that still stalls visibly when the spawn_blocking is reverted, calibrated once, or add dev opt-level overrides for html5ever and scraper.
- risk: An input that is too small could hide a regression. Calibrate it.

### MB-IMPORT-TESTS-4: deflate-bomb tests run unoptimized miniz_oxide over 64+ MiB (merged with TESTS-5)
- file: crates/mb-import/tests/archive.rs:91
- dimension: tests | guideline: tests:slow | severity: medium (unmeasured) | semver: none | effort: S
- evidence: archive.rs unpacked_size_limit (86-101) deflates 65 x 1 MiB through ZipWriter and then inflates it up to the 64 MiB cap. smart_text.rs pdf_flate_bomb_is_refused (642) and pdf_nested_flate_bomb_is_refused (651) inflate up to MAX_PDF_EXPANSION=64 MiB. The nested one also recompresses about 650 KB with Compression::best() (656-658). There are no dev opt-level overrides for miniz_oxide, zlib-rs or flate2. All three backends appear in Cargo.lock, so check which one flate2 and zip actually select.
- fix: Add `[profile.dev.package.miniz_oxide] opt-level = 3` (and the same for zlib-rs if it is in use) to the workspace Cargo.toml. Use Compression::fast() for the outer layer of the nested bomb.
- risk: None. This changes only dev build speed.

### MB-IMPORT-TESTS-6: the transport-error redaction test relies on port 9 being closed and never asserts the error kind
- file: crates/mb-import/tests/smart_fetch.rs:364
- dimension: tests | guideline: tests:weak-assertion | severity: low (was medium) | semver: none | effort: S
- evidence: The test fetches http://127.0.0.1:9/… and asserts only that "s3cret" is absent from the message. If something listens on port 9, a different error path runs and the test still passes.
- fix: Bind 127.0.0.1:0, read the port and drop the listener. Assert `matches!(err, FetchError::Transport(_))` before checking the redaction.
- risk: A small race on the freed port.

### MB-IMPORT-TESTS-7: unquoted_numeric_versions_are_detected asserts only is_ok()
- file: crates/mb-import/tests/swagger2.rs:90
- dimension: tests | guideline: tests:weak-assertion | severity: low | semver: none | effort: S
- evidence: Lines 92 and 94 only check compile(y).is_ok(). Misdetection between Swagger2 and OpenApi3 would pass.
- fix: Also assert `sniff(y) == Ok(SourceFormat::Swagger2)` / `Ok(SourceFormat::OpenApi3)`.
- risk: None.

### MB-IMPORT-TESTS-8: the undeclared-entity case asserts only is_err()
- file: crates/mb-import/tests/wsdl.rs:465
- dimension: tests | guideline: tests:weak-assertion | severity: low | semver: none | effort: S
- evidence: `assert!(compile(entity).is_err())`. Any failure passes, including a "no service" error after the entity was expanded.
- fix: Match ImportError::Invalid and assert that the message contains a stable fragment about the entity.
- risk: Coupling to quick-xml's wording, so use contains().

### MB-IMPORT-TESTS-9: the prompt test discards the extract_chunk result
- file: crates/mb-import/tests/smart_llm.rs:563
- dimension: tests | guideline: tests:weak-assertion | severity: low | semver: none | effort: S
- evidence: `let _ = extract_chunk(...).await;` followed by `provider.requests()[0]`.
- fix: Assert the result (Ok with empty operations) and `provider.requests().len() == 1` before indexing.
- risk: None.

### MB-IMPORT-TESTS-10: orders_snapshot_unchanged shares a snapshot file with openapi3::orders_30_snapshot
- file: crates/mb-import/tests/security.rs:102
- dimension: tests | guideline: tests:duplication | severity: low | semver: none | effort: S
- evidence: It asserts "openapi3__orders_30_snapshot" with prepend_module_to_snapshot=false, the same file that tests/openapi3.rs:99-101 owns. The only extra check is security.is_empty().
- fix: Move the security.is_empty() assertion into openapi3.rs::orders_30_snapshot and delete the copy.
- risk: None.

### MB-IMPORT-TESTS-11: 17 integration-test binaries could be consolidated into one
- file: crates/mb-import/Cargo.toml:77
- dimension: tests | guideline: tests:binary-count | severity: medium | semver: none | effort: M
- evidence: tests/ holds 17 files, and 9 [[test]] stanzas exist only for required-features=["smart"]. Blockers were checked: no set_var, OnceLock or statics (the `&'static str` in smart_fetch.rs is a field type), no #[path] or mod common. There are 14 snapshots in tests/snapshots. The workspace Cargo.toml notes that test binaries already filled the disk.
- fix: Create tests/it/main.rs with the shared allows, plain mods and `#[cfg(feature = "smart")]` smart mods, and one [[test]] entry. Move the snapshots and rename them to `it__<module>__<name>`, then review with cargo insta. Fix TESTS-10 and TESTS-1 first.
- risk: The tests share one process, which makes the stall tests more sensitive. Do not leave stale snapshot files behind.

## Dropped
- MB-IMPORT-PERF-10: one schema compile per LLM chunk is negligible next to the LLM round trip it follows.
- MB-IMPORT-PERF-11: the suffix probing is quadratic only in the number of definitions that share one local name, which is small in real WSDLs and specs.
- MB-IMPORT-STYLE-13: merged into STYLE-6.
- MB-IMPORT-STYLE-14: a subjective module split with large churn and no defect.
- MB-IMPORT-STYLE-15: nested use paths are the rustfmt 2024 default and the 80-column rule is not the Rust style (rustfmt uses 100). The &Option<String> parameter is a trivial private nit.
- MB-IMPORT-STUB-13: the WSDL 2.0 refusal is deliberate and documented, so there is nothing to change.
- MB-IMPORT-STUB-14: the nested-archive refusal is deliberate and documented, so there is nothing to change.
- MB-IMPORT-TESTS-2: saves about 20 ms, which the finding itself calls negligible.
- MB-IMPORT-TESTS-5: merged into TESTS-4.
- MB-IMPORT-TESTS-12: saves about 150 ms, and a 50 ms timeout adds load sensitivity.
- MB-IMPORT-TESTS-13: the proptest cost is unmeasured and fewer cases trades coverage for a small gain.
