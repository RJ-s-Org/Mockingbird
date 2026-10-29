# Refinement plan

Source: the verified audit in [`audit/`](audit/), 319 findings that survived an
adversarial pass (Haiku inventory, Opus judgement, Opus verification; 44 dropped).
Finding IDs below (`MB-ENGINE-PERF-2`, `CI-2` …) point at entries there, each with
file:line, evidence, fix and risk. Numbers are measured against
[`baseline.md`](baseline.md).

## Rules for every PR

- One concern per PR, a disjoint file set from every PR in the same wave, branched
  from `main`, merged with `--merge` once the required checks pass.
- Local gates: `cargo fmt --all --check`, clippy with `-D warnings` on all targets
  and features, tests of the touched crates and their dependents (CI runs the whole
  workspace), and the console checks when `crates/mb-console/web` is touched.
- Behaviour changes are test-first. Pure refactors keep the existing tests green.
  A test-harness PR states the test count before and after (nextest `--list`), and
  the counts must match.
- Each PR states its measured effect against the baseline.
- Published APIs stay semver-compatible: additive changes only (new impls, new
  methods, `#[must_use]`, deprecations). Anything breaking is listed under
  [0.2.0](#deferred-to-020) and not done.
- No new `allow` attributes and no new panics in library code.

## Wave 0: foundations (land first; they speed up every later PR)

| PR | Content | Files |
|---|---|---|
| R0 | This plan, the baseline and the audit | `docs/refinement/**` |
| R1 CI | CI-2 nextest (`ci` profile in `.config/nextest.toml`, doctests through `cargo test --doc`, a serial group for the cross-node binaries); CI-1 drop the custom-ext step; CI-3 move `cargo doc` and the slim check into `protocol-features`; CI-7 cancel only PR runs; CI-14 the integrations path filter; CONTRIBUTING and `docs/ci.md` mention nextest | `.github/workflows/{ci,examples,integrations,iac,npm}.yml`, `.config/nextest.toml`, `CONTRIBUTING.md`, `docs/ci.md` |
| R2 lints | `clippy.toml` with `allow-unwrap-in-tests` and `allow-expect-in-tests`; deny `clippy::expect_used` in the workspace; delete the 143 per-file and per-module `unwrap_used`/`expect_used` allows; replace the 28 `expect(` calls in library code with errors (or `unwrap_or_else` on infallible constants with a comment) | `Cargo.toml`, `clippy.toml`, the allow sites |

R1 and R2 touch disjoint files and run in parallel.

## Wave 1: one integration-test binary per crate

One PR per crate group, in parallel (each touches only its own `tests/` and the
`[[test]]` sections of its own `Cargo.toml`). Each moves `tests/*.rs` into modules
of `tests/it/main.rs`, merges the duplicated helpers into `tests/it/support.rs`
with one bounded `wait_until` poll helper, and resolves the blockers the audit
lists: process-global state (`set_var`, a global tracing subscriber) moves into
its own binary or gets a serial guard; `required-features` tests become
`#[cfg(feature = …)]` modules. Test counts must match.

| PR | Crates | Binaries now | Findings |
|---|---|---|---|
| T1 | `mockingbird` | 31 | MOCKINGBIRD-TESTS-3/4/5/9, CI-11; plus the macOS-only `tls` failure (generate the server certificate at test time with `rcgen` rather than a 100-year fixture) |
| T2 | `mb-protocols` | 20 | MB-PROTOCOLS-TESTS-9/10/11 |
| T3 | `mb-import` | 17 | MB-IMPORT-TESTS-10/11 |
| T4 | `mb-admin`, `mb-auth` | 18 | MB-ADMIN-TESTS-10/11/12/13 |
| T5 | `mb-engine`, `mb-state`, `mb-mcm` | 15 | MB-ENGINE-TESTS-8 |
| T6 | `mb-store`, `mb-llm`, `mb-ext`, `mb-evals`, `mb-console`, `custom-ext` | 12 | MB-STORE-TESTS-10/11/12 |

Expected: integration-test link units from 113 to about 20, several GB less
`target/`, and a lower clean test build.

## Wave 2: faster and more accurate tests

Same crate split as Wave 1, run after it on the same files. Replace sleeps with
event-driven waits, the `wait_until` helper or `tokio::time::pause` (only where no
real socket or cross-thread timer depends on the wall clock). Tighten weak
assertions. Cut the slow tests.

| PR | Findings |
|---|---|
| S-T1 | `mockingbird`: the `agent_toolkit` binary (58 s, about 3 s a test: find out why and cut it), MOCKINGBIRD-TESTS-1/2/6/7/8/10/11/12 |
| S-T2 | `mb-protocols`: MB-PROTOCOLS-TESTS-1–8, 12–16 (`events_kafka` connect timeout 10 s) |
| S-T3 | `mb-import`: MB-IMPORT-TESTS-1/3/4/6–9 |
| S-T4 | `mb-admin`, `mb-auth`: MB-ADMIN-TESTS-1–5/8/9; MB-ADMIN-TESTS-3 caches the test OIDC RSA key (`mb-auth/src/testing.rs`, internal) |
| S-T5 | `mb-engine`, `mb-state`, `mb-mcm`: MB-ENGINE-TESTS-1–7/9–11 |
| S-T6 | others: MB-STORE-TESTS-1–5/9/13/14 |

## Wave 3: correctness and security fixes (high severity first)

Small PRs with disjoint files, test-first.

| PR | Findings | Files (main) |
|---|---|---|
| B1 secrets in `Debug` | MB-ADMIN-STYLE-1, MB-STORE-STYLE-1 | `mb-auth/src/oidc.rs`, `apikey.rs`; `mb-llm/src/openai.rs` |
| B2 console profile editor erases `extra_body` | MB-STORE-STUB-2 | `crates/mb-console/web/src/pages/ProfileEditor.tsx` |
| B3 GraphQL duplicate fields panic at serve time | MB-IMPORT-STUB-1 | `mb-import/src/graphql.rs` |
| B4 an unsupported scheme in an AND security requirement weakens auth | MB-IMPORT-STUB-8 | `mb-import/src/security.rs` |
| B5 `vault:` references | MOCKINGBIRD-STUB-1, MB-STORE-STUB-1/6/7 | `mockingbird/src/reconcile.rs`, `mb-admin/src/profiles.rs`, `mb-store/src/model.rs`, `mb-ext/src/vault.rs` |
| B6 exponential XSD group expansion | MB-IMPORT-PERF-1 | `mb-import/src/wsdl/xsd.rs` |
| B7 O(k²) form and query decoding before auth | MB-PROTOCOLS-PERF-1/2 | `mb-protocols/src/http_rest/{form,handler}.rs` |
| B8 `http_client` fallback loses the no-redirect policy | MB-ADMIN-STUB-11 | `mb-auth/src/http.rs` |
| B9 streaming drops provider errors; fallback never fails over | MB-STORE-STUB-3 | `mb-llm/src/{openai,fallback}.rs` |

## Wave 4: hot-path performance

One PR per crate, disjoint, each with a micro-benchmark or a scaling test showing
the before and after.

| PR | Findings |
|---|---|
| P1 `mb-engine` | MB-ENGINE-PERF-2 (validator per GraphQL request), 3, 4, 5, 6, 7, 1 (cache eviction), 10, 11, 15 |
| P2 `mb-state` | MB-ENGINE-PERF-8, 9, 12 |
| P3 `mb-protocols` | MB-PROTOCOLS-PERF-4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14 (PERF-3 stream wake-ups is L: design first) |
| P4 `mockingbird` | MOCKINGBIRD-PERF-1, 2, 3, 4, 8, 10, 11, 13, 14 |
| P5 data-plane API-key lookup cache | MB-ADMIN-PERF-1, MOCKINGBIRD-PERF-6 (bounded TTL cache invalidated on revoke; Opus design) |
| P6 `mb-admin` | MB-ADMIN-PERF-2, 4, 5, 6, 7, 8, 9, 10 |
| P7 `mb-import` | MB-IMPORT-PERF-2–9, 12 |
| P8 `mb-store` | MB-STORE-PERF-1, 2, 7 |
| P9 small crates | MB-STORE-PERF-3, 4, 5, 8, 9, MB-ENGINE-PERF-16 |

## Wave 5: finish the stubs

Classified *finish* by the audit; one PR per crate area.

| PR | Findings |
|---|---|
| F1 `mb-protocols` GraphQL | MB-PROTOCOLS-STUB-1 (SDL directives) |
| F2 `mb-protocols` other | STUB-2 (`?wsdl` XSDs), 4 (gRPC compression), 6 (MQTT shared group), 7, 11 (binary WS frames) |
| F3 `mb-import` | MB-IMPORT-STUB-3, 4, 6, 7, 9, 10, 12 |
| F4 `mb-engine`, `mb-mcm` | MB-ENGINE-STUB-1, 2, 3, 4, 7, 8, 12 |
| F5 `mockingbird` | MOCKINGBIRD-STUB-2, 3, 6, 7, 8, 9, 13 |
| F6 `mb-admin` | MB-ADMIN-STUB-4, 5, 6 (Admin API: `npm run gen:api`), 9, 13 |
| F7 OIDC pending logins in the store | MB-ADMIN-STUB-1, MB-ADMIN-STUB-2 |
| F8 others | MB-STORE-STUB-5, 10, 11, 12, 13; MB-ENGINE-STUB-6 (cross-node cache reset) |

The multi-file external `$ref` import (MB-IMPORT-STUB-2, effort L) needs a design
pass first; it is planned last in this wave.

## Wave 6: API guidelines and style (additive only)

| PR | Content |
|---|---|
| A1 | `Display` + `Error` for the public error types that lack them: `ClientError`, `ServeError::source`, `Problem`, `ChannelControlError`, `mb-ext::Rejection` (MOCKINGBIRD-STYLE-2, MB-ADMIN-STYLE-2, MB-STORE-STYLE-3) |
| A2 | `Debug` for every public type, redacting secrets (MOCKINGBIRD-STYLE-7, MB-PROTOCOLS-STYLE-5, MB-ENGINE-STYLE-2, MB-ADMIN-STYLE-5, MB-IMPORT-STYLE-7, MB-STORE-STYLE-2/15) |
| A3 | `#[must_use]` on guards and by-value builders; `Display`/`FromStr` next to the `as_str`/`parse` pairs; missing `Copy`/`Eq`/`Hash` derives |
| A4 | Dead code and lint allows in `src`: `StateWrite`, the `mb-admin` OpenAPI mirror types, the six `too_many_arguments` (parameter structs), `unreachable_patterns` |
| A5 | Crate docs, `# Errors`/`# Panics` sections and docs.rs metadata, `mb-ext` first (it is 1.0) |

## Deferred to 0.2.0

Breaking changes to the published crates, not done in this program: `#[non_exhaustive]`
on public enums and option structs; `String` error types replaced with typed errors
(MOCKINGBIRD-STYLE-3, MB-IMPORT-STYLE-2/3, MB-PROTOCOLS-STYLE-2/3/4/11, MB-STORE-STYLE-11/14,
MB-ENGINE-STYLE-1); public fields made private (MB-ADMIN-STYLE-3/15, MOCKINGBIRD-STYLE-14,
MB-ENGINE-STYLE-4); `mockingbird-cli` publishing its internals as a library
(MOCKINGBIRD-STYLE-1); `smart` submodules public only for tests (MB-IMPORT-STYLE-4);
`HttpMethod::Trace` (MB-ENGINE-STUB-5). Where a deprecation path exists, Wave 6 adds it. The [0.2.0 scope](#020-scope) section plans the release.

## Kept as documented refusals

MQTT over TLS (rustls-webpki advisories), WSDL 2.0, nested archives, AsyncAPI protocol
refusals (MB-IMPORT-STUB-11: message fixed), MTOM/XOP (documented), late SASL refusal
(MB-PROTOCOLS-STUB-12: validated at config load instead), OTLP over HTTP only
(MOCKINGBIRD-STUB-10: documented), `passthrough_unsupported` (MOCKINGBIRD-STUB-11:
message fixed), Redis requirements (MB-ENGINE-STUB-10: documented),
`passthrough_validate` for non-OpenAPI contracts (documented). Fixes to their messages
and docs ride in the Wave 5 PR of their crate.

## Product decisions (2026-10-29)

The user answered the batched questions:

| # | Question | Decision |
|---|---|---|
| 1 | Native AWS/GCP/Azure secret managers in `mb-ext` | **Not planned.** `env:`, `file:` and `vault:` references cover these clouds; `docs/configuration.md` says how. |
| 2 | Per-call, per-tenant `ProviderPolicy`; admin LLM work billed to the resolved tenant | **Yes**, with zero friction for single-tenant setups: see [D1](#d1-design-note-tenant-aware-provider-policy). |
| 3 | Console sessions: idle timeout, revocation, roles refreshed after login | **Yes** (D2). |
| 4 | Seeded, deterministic offline faker | **Yes**: make the testing skill's promise true, keep its wording (D3). |
| 5 | CLI commands to finish the import workflow | **Yes** (D4). |
| 6 | Protocol scope | **Now:** `basePath`/`servers[].url` prefixes (D5), non-JSON REST responses (D6), batched GraphQL (D7). **Deferred** to [#78](https://github.com/RJ-s-Org/Mockingbird/issues/78): chaos on AsyncAPI channels, MQTT 3.1.1, several brokers per mock, `hosts` on gRPC mocks. |
| 7 | Console screens for Admin API features without UI | **Deferred** to [#78](https://github.com/RJ-s-Org/Mockingbird/issues/78): the console comes second to the agent surfaces. |
| 8 | 0.2.0 for the breaking cleanups | **Yes**, after the non-breaking waves land, and released only on the user's go: see [0.2.0 scope](#020-scope). |

## Wave 7: decided features

Run after Wave 5, test-first, one PR each (D2 and D3 may split further).

| PR | Content | Main files |
|---|---|---|
| D1 | Tenant-aware provider policy, per the design note below | `mockingbird/src/runtime.rs`, `mb-engine` provider selection, `mb-protocols/src/call.rs`, `mb-admin/src/{lib,drafts}.rs`, `mb-admin/src/imports/` |
| D2 | Console sessions: an idle timeout (configurable, documented default, `0` disables), revocation (Admin API to end one session or every session of a principal, plus a console "sign out everywhere"), and roles re-derived after login (re-evaluated from the stored claims on mapping changes, refreshed from the IdP with its refresh token when it issued one, otherwise at re-login after the idle timeout). Uses the `last_seen` column that is written but never read (MB-ADMIN-STUB-7) | `mb-auth/src/session.rs`, `mb-admin/src/{sessions,auth_routes}.rs`, `mb-store` |
| D3 | Seeded deterministic faker: the same seed, contract and request give the same body offline. The seed is set per mock and overridable per request, and offline runs default to a fixed seed so that the testing skill's "deterministic offline mode" holds (MB-ENGINE-STUB-9) | `mb-engine/src/faker.rs`, `lib.rs`, mock config |
| D4 | CLI: `drafts approve`, `contracts activate`, `mocks update`, and a raw `api <METHOD> <PATH>` passthrough, each with its MCP counterpart (MOCKINGBIRD-STUB-5) | `mockingbird/src/{cli,client,mcp}.rs` |
| D5 | Honour Swagger `basePath` and OpenAPI `servers[].url` path prefixes. Prefixed paths match, and unprefixed paths keep matching so existing mocks do not break (MB-IMPORT-STUB-5) | `mb-import/src/{swagger2,openapi3}.rs`, `mb-protocols/src/http_rest/routing.rs` |
| D6 | Non-JSON REST responses: the declared media type, from a declared example or a faked text body (`text/*`, XML via the SOAP XML mapper, `application/octet-stream` from a declared example) (MB-PROTOCOLS-STUB-5) | `mb-protocols/src/http_rest/handler.rs`, `mb-engine` |
| D7 | Batched GraphQL: a JSON array of operations answered with an array, capped in size, each operation admitted and recorded on its own (MB-PROTOCOLS-STUB-9) | `mb-protocols/src/graphql/mod.rs` |

### D1 design note: tenant-aware provider policy

Today `ProviderPolicy::choose` runs once, when a mock's engine is built, with
`Tenancy::new("default", workspace)`. A rejection there fails the build.
Admin-side LLM work (imports, drafts) is attributed to tenant `default` without
asking the `TenantResolver` (MB-STORE-STUB-4, MB-ADMIN-STUB-3).

**Resolution order, per call.**
1. Admission resolves the tenancy through `TenantResolver`, as it already does
   for every data-plane call.
2. The configured profile is the mock's `llm_profile`, or else the workspace
   default, as today.
3. `ProviderPolicy::choose(tenancy, mock, configured)` runs, which is where
   per-tenant overrides live.
4. When the answer equals the configured profile, the call uses the provider the
   mock already built. Otherwise it uses a provider for the chosen profile, built
   on first use and cached per mock. The cache is bounded by the workspace's
   profiles and dropped when the mock or the profile changes.
5. A `Rejection` is answered as `application/problem+json` with its status and
   `Retry-After`, like a quota rejection, instead of failing the mock build.

**Defaults.** The self-hosted policy stays `ConfiguredProfile`, which returns the
configured profile unchanged. Step 4 therefore always takes the prebuilt-provider
path: one extra virtual call per LLM call, no allocation and no lookup. There are
no new config keys. A per-tenant policy exists only when an embedder installs one
through `Extensions::with_providers`; it is opt-in and layered on top, and it falls
back to the configured profile when it has no opinion.

**Admin-side work.** Imports and drafts build a `CallInfo` from the admin request:
the mock, operation id `import` or `draft`, and the request headers. They resolve
the tenancy through `TenantResolver` and store it on the import job, so the
background worker bills the same tenant. `SingleTenant` resolves to `default`, so
single-tenant behaviour and billing do not change. A resolver that rejects is
answered as `application/problem+json`, as on the data plane.

**Migration and semver.** No store or config migration. `mb-ext` gains no API: the
trait already takes a `Tenancy` and its docs already say "for a call", so this
makes the documented behaviour true. The mb-ext changelog records it, and
cargo-semver-checks sees no change. A policy that rejected tenant `default` at
build time now rejects per call instead.

**What single-tenant users see:** nothing new. There is no new config, the
quickstart and skills are unchanged, and the same profile is chosen for every
call.

**Tests first:**
- A policy that picks a different profile per tenant routes each call to that
  profile's provider.
- A rejecting policy yields problem+json with `Retry-After`.
- With `ConfiguredProfile`, no extra provider is ever built (counted).
- An import is billed to the resolved tenant (a `UsageSink` spy).

## 0.2.0 scope

Ships after Waves 0–7, and only on the user's go.
- Hide the `mockingbird-cli` library surface: `#[doc(hidden)]` on the internals,
  with the documented embedding API kept public (MOCKINGBIRD-STYLE-1).
- Everything under [Deferred to 0.2.0](#deferred-to-020).
- `mb-ext` stays 1.x: none of these changes touch it.

Wave 6 adds deprecations where a path exists, so 0.1.x users get warnings first.

## Out of scope here

CI-8 (deleting the tag-scoped Actions caches) needs the user's go. CI-4 (start the
service containers in the background) and CI-5 (console on the large runner) come
after Wave 0's measurements show whether they are on the critical path.
