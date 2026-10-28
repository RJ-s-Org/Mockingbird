# Improvements Backlog Implementation Plan (index)

> **For agentic workers:** this is the coordinating plan. Each workstream has its own plan; execute one per session, in its own worktree, with superpowers:subagent-driven-development or superpowers:executing-plans. Steps use checkbox (`- [ ]`) syntax.

**Goal:** Close every open item in `docs/improvements.md` and run the three field-trial apps unchanged through `mockingbird proxy`.

**Architecture:** Six workstreams split by file ownership, run in three waves; each is one branch and one PR, merged only with the user's OK. Wave 3 (this session) re-runs the trials and updates the docs.

**Tech Stack:** Rust workspace (axum, hyper-util, rustls, jsonschema 0.58, async-graphql 7.2), new crates `rand_regex 0.19.0` (WS1) and `rcgen 0.14.10` (WS4); console in `crates/mb-console/web` (Node 22).

**Spec:** `docs/superpowers/specs/2026-10-22-improvements-backlog-design.md`

## Global Constraints

- Gates on every PR: `cargo fmt --all --check`, `cargo clippy --workspace --all-targets --all-features -- -D warnings`, `cargo test --workspace --all-features`; console changes also `npm run typecheck && npm run lint && npm test` in `crates/mb-console/web`.
- No `unwrap()`/`expect()` outside tests; Admin API errors are `application/problem+json`; secrets are references only; never print or write an API key or webhook secret.
- New config keys in `docs/configuration.md` and `docs/examples/mockingbird.full.yaml`; regenerate the console client (`npm run gen:api`) after Admin API changes.
- Commits end with `Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>`; PR bodies end with `🤖 Generated with [Claude Code](https://claude.com/claude-code)`. No merge without the user's explicit OK for that PR; auto-merge is not available on this repository.
- Migration numbers: WS4 `0600_mock_hosts`, WS1 `0610_mock_behavior`, WS3 `0650_import_drafts`, WS6 `0700_mock_webhooks`. Keep them; a PR that merges after a higher number keeps its own (the migrator applies any missing version).
- Known pre-existing failures, reported but not fixed: `tls::llm_client_trusts_outbound_ca` on macOS; flaky `graphql::rejections_are_recorded`.

## Review Focus

- Two workstreams editing the same struct literals (`NewMock`/`Mock` in about 25 test sites: WS1 and WS4; `Bundle`/`RuntimeDeps`: WS6): the second PR to merge rebases and adds its field; the compiler lists every site.
- A config file using keys from several workstreams at once (`hosts`, `consistency`, `unknown_ids`, `default_scenario`, `proxy`, `webhooks`): WS7 runs one config with all of them; `docs/examples/mockingbird.full.yaml` must parse after every merge (the existing docs test checks it).
- GraphQL queries with several root fields (joint generation) skip WS1's state/cache/seeding: WS7 checks the github-readme-stats cards still render and notes the gap.
- A trial app whose HTTP client ignores proxy variables: WS7 records the runtime and the fallback used.
- Old clients approving drafts with `{}` now activate (WS3): the console sends `activate: false` to keep its two-step flow.

---

## Workstream plans

| WS | Plan | Branch | Tasks | Wave | Depends on |
|---|---|---|---|---|---|
| 1 | [Engine consistency and faker](2026-10-22-ws1-engine-consistency.md) | `feat/ws1-engine-consistency` | 16 | 1 | — |
| 2 | [Protocol fixes](2026-10-22-ws2-protocol-fixes.md) | `feat/ws2-protocol-fixes` | 4 | 1 | — |
| 3 | [Import and drafts](2026-10-22-ws3-import-drafts.md) | `feat/ws3-import-drafts` | 13 | 1 | — |
| 4 | [Routing, proxy and CA](2026-10-22-ws4-routing-proxy.md) | `feat/ws4-routing-proxy` | 11 | 1 | — |
| 5 | [Agent and MCP ergonomics](2026-10-22-ws5-agent-mcp.md) | `feat/ws5-agent-mcp` | 11 | 2 | WS3, WS4 merged |
| 6 | [Webhooks](2026-10-22-ws6-webhooks.md) | `feat/ws6-webhooks` | 12 | 2 | WS1 merged |

Cross-workstream interfaces (each consumer's Task 0 re-checks them on `main`):
- WS3 → WS5: `POST …/imports` with `url | urls | text | content_base64 | documents` and `mode`; `POST …/contracts/{v}/approve` with `{"activate": bool}` (default true); `warnings` on `ContractVersionSummary` and `UploadResult`.
- WS4 → WS5: `proxy::env::{options_for, proxy_env}`, `ProxyEnv`, `proxy::ca::ca_cert_path`, `proxy::DEFAULT_PROXY_PORT`, `ServeArgs.{proxy, proxy_port}`.
- WS1 → WS6: `write_state` returns the writes it made (call or seeded); WS6 hooks after it in `generate_narrowed`, for call writes only.
- WS3 ↔ WS1: `PROMPT_SCHEMA_BUDGET` moves to `mb-import`; `mb-engine/src/compact.rs` re-exports it (one line).

## Accepted deviations from the spec

- WS1: `format` errors are not patched (the validator does not report `format`); multi-root GraphQL queries skip state/cache/seeding; cache hits get their own `cache` source.
- WS2: subscription payloads are not re-keyed by alias; chaos `default`-shape errors use status 503.
- WS3: `compile` keeps its signature; `compile_with_warnings` is added.
- WS5: profiles reference `file:<absolute path>` (the existing syntax); the permission refusal applies only inside `~/.mockingbird/secrets`, so mounted container secrets keep working.
- WS6: delivery records are in memory (200 per mock), not in the state store.

---

### Task 1: Wave 1

- [ ] **Step 1:** Start four sessions, each in its own worktree from `main`, each executing its plan: WS1, WS2, WS3, WS4.
- [ ] **Step 2:** As each opens its PR, check CI and the diff; ask the user to approve the merge. Merge with `gh pr merge <n> --merge`; the next PR in the wave rebases (or syncs with base) and re-runs the gates.
- [ ] **Step 3:** After all four merge: `git pull` on `main`, run the gates once on the combined tree. Expected: green apart from the known failures.

### Task 2: Wave 2

- [ ] **Step 1:** Start two sessions from the updated `main`: WS5 and WS6. Each runs its Task 0 first and records the resolved names.
- [ ] **Step 2:** Same review and merge flow as wave 1. WS6's MCP task rebases onto WS5 if WS5 merged first.
- [ ] **Step 3:** Gates on the combined `main`.

### Task 3: Trials through the proxy (WS7)

**Files:** trial configs in `../mockingbird-trials/*/mockingbird.yaml` (not in this repo); `docs/improvements.md`.

- [ ] **Step 1:** Build release, reinstall the binary with `rm` then `cp` to `~/.mockingbird/bin/mockingbird`.
- [ ] **Step 2:** For each trial: add `hosts:` (`api.stripe.com`; `api.github.com` for REST at `/` and GraphQL at `/graphql`; `api.themoviedb.org`), `proxy: {enabled: true}`, and `proxy.images: [image.tmdb.org]` for movies; Stripe gets a `webhooks:` entry to the app's webhook route with `signing: stripe` and the app's webhook secret as a secret reference. Revert the trial apps' source edits (`git -C <app> diff` must be empty), stop the old app processes, start `mockingbird serve` and the apps with `eval "$(mockingbird proxy env)"`.
- [ ] **Step 3:** Exercise each app (Stripe: 8 creates + 4 retrieves and a webhook verified by the Stripe SDK; github-readme-stats: stats, top-langs, 5 pin cards; movies: 5 pages incl. 2 detail pages). Expected: every page renders, no source edits.
- [ ] **Step 4:** From `/api/v1/…/requests`, record per trial: fallbacks, repairs, `state`/`cache` sources, prompt tokens, page latency.
- [ ] **Step 5:** Update `docs/improvements.md`: move Phase 2's four shipped items to **Done** under #32, move every closed item to **Done** with its PR, add the measurements table, and list anything re-scoped with its reason. Commit on a docs branch, open the PR, ask the user to merge.
