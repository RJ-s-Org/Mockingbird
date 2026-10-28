# Improvements and enhancements

A running list of what to improve, mostly from using Mockingbird the way it is meant to be used: a coding agent (Claude Code, Codex, Cursor…) mocking a project's third-party APIs through the MCP server, with the console as a secondary interface. Each entry says where it came from. Move an entry to **Done** with its PR when it ships.

## Field trials, 2026-10-21

Three fresh Claude Code sessions, each with only the Mockingbird plugin, mocked the APIs of a real open-source project. The LLM was `deepseek/deepseek-v4.1-flash` on OpenRouter.

| Trial | API style | Outcome |
|---|---|---|
| Stripe `accept-a-payment` (Node server) | REST, form-encoded bodies, 8 MB OpenAPI | Ran end to end after four workarounds; 25% faker fallbacks |
| `github-readme-stats` | GitHub GraphQL + REST | Cards rendered after trimming both contracts and two source edits; 33% GraphQL fallbacks, REST always fell back |
| `nuxt/movies` | TMDB, no spec (documentation import) | Every page rendered; docs import needed undocumented profile settings; 20–80 s cold pages, 28% fallbacks |

Total spend was about $1. When the LLM answered, the data was excellent; the problems are around it.

## Trials through the proxy, 2026-10-28

The three trial apps ran again on `main` with the backlog merged, unmodified (`git diff` empty), with their API hosts mapped to mocks and the apps started under `eval "$(mockingbird proxy env)"`. Same model and routing as before; full upstream contracts.

| Trial | Reached the mock | Result | Sources | Prompt tokens | Latency |
|---|---|---|---|---|---|
| Stripe `accept-a-payment`: 8 creates, 4 retrieves, webhooks | `api.stripe.com`; stripe-node needs `global-agent` (below) | 12/12 calls answered, 0 fallbacks; 9/9 `payment_intent.created` webhooks verified by `stripe.webhooks.constructEvent` | 6 `llm`, 2 `repaired`, 4 `state` | 140k (17k per create) | creates 3.2–7.6 s (median 3.7 s); retrieves 0–25 ms |
| `github-readme-stats`: stats, top-langs, 5 pin cards, stats again | `api.github.com`, REST at `/` and GraphQL at `/graphql` on one host; axios honours `HTTPS_PROXY` | every card renders; the repeated stats query is a `cache` hit | GraphQL 12 `llm`, 2 `cache`; REST 2 `fallback` (`/search/commits`, timeout) | 10k | GraphQL 1.3–3.9 s; cards with `include_all_commits` wait 60 s (below) |
| `nuxt/movies`: 5 SSR pages incl. 2 detail pages | `movies-proxy.vercel.app` (the app's hosted proxy, see below); Node `fetch` with `NODE_USE_ENV_PROXY` | 5/5 pages render with real titles, cast and figures; detail pages agree with the list they came from (seeded from state) | 10 `llm`, 3 `repaired`, 0 fallbacks | 20k (69k completion) | 6.6–37 s per call, median 19.8 s |

## Open

### From the proxy trials (2026-10-28)

- **SDKs with their own HTTP agent ignore the proxy variables.** stripe-node creates its own `https.Agent`, so `NODE_USE_ENV_PROXY` does not apply and its calls went to the real `api.stripe.com` (the dummy key was rejected; nothing else was sent). `node -r global-agent/bootstrap` with `GLOBAL_AGENT_HTTP_PROXY` fixed it. Ship this: `mockingbird proxy env --node` could add a `NODE_OPTIONS=--require <preload>` that installs the proxy on every agent (and undici's dispatcher), with no package to install.
- **One contract under another host and path.** The movies app calls its hosted proxy (`movies-proxy.vercel.app/tmdb/<path>`), not TMDB, so the trial exported the TMDB contract and stripped `/3` from its paths to mount it there. A per-mock path rewrite (`strip_prefix`/`add_prefix`), or letting a mock reuse another mock's contract with a different base path, would avoid the copy.
- **Browser traffic does not go through the proxy.** Server-side rendering reaches the mocks; images (`movies-proxy.vercel.app/ipx/…`) and client-side navigation are fetched by the browser from the real host. Document browser setups (a PAC file or `--proxy-server` for Playwright/Chrome with the CA) or serve the images via `proxy.images` once the browser uses the proxy.
- **Adding a `mocks:` block to a running server's config did not take effect** (no reload line in the log, `/readyz` stayed `ready`); a restart did. Check whether a mock defined only in the database plus a new config `mocks:` section is a reload the watcher skips.

## Done

- Launch L1, engine speed and fidelity, [#57](https://github.com/RJ-s-Org/Mockingbird/pull/57) (numbers: [performance](performance.md#latency-against-a-real-llm)):
  - huge response schemas: an output budget (`limits.output_budget_tokens`, 1500) shapes what the LLM writes (unrequested optional sub-objects left out, arrays capped, deep required objects filled by the faker), and OpenRouter requests send `reasoning: {enabled: false}`; GitHub `search/commits` p50 29.8 s → 9.4 s with no fallback, and a fallback answer is now cached under consistency (the repeat is answered at once and still reported as a fallback);
  - large TMDB details: movie detail with every `append_to_response` part p50 51 s → 6.2 s (about 5–8k completion tokens, mostly hidden reasoning, down to ~1k);
  - persona formats: templates (`client_secret = "<id>_secret_<24 random chars>"`) and id prefixes (`(pi_, cus_) followed by 24 random alphanumeric chars`) are checked and patched after generation; Stripe `client_secret` format 2/3 → 5/5;
  - GraphQL queries with several root fields read state, the cache and stored resources per field, seed and cache per field, and are logged as one request (each field is still metered);
  - `scripts/trial-bench.py`: a repeatable latency bench over the trial contracts.
- Phase 1, [#31](https://github.com/RJ-s-Org/Mockingbird/pull/31):
  - the MCP server imports keys from the login shell;
  - `doctor` names the process that lacks a key, and `start_server` refuses a profile whose key is missing;
  - `init_config` ports;
  - `llm test` defaults to `mockingbird.yaml`;
  - 60 s / 16 384-token defaults and array-envelope unwrapping;
  - reasoning-only, in-band error and truncated replies are handled, with one retry;
  - `X-Mockingbird-Fallback-Reason` and `request_log.fallback_reason`;
  - profile `extra_body`;
  - docs import at temperature 0 in JSON-object mode;
  - the server-running hint only on connection errors.

  Measured afterwards on the Stripe trial's mock: 25 `POST /v1/payment_intents` with OpenRouter's default routing gave 24 `llm`, 1 `repaired`, **0 fallbacks** (the trial's baseline was 28%), with a median of 7.4 s and a worst case of 9.6 s. Every call went to the DigitalOcean route that failed during the trial. It returns content in JSON-object mode but drops the answer in JSON Schema mode, so the OpenRouter preset uses JSON-object mode.
- Phase 3 (part), [#34](https://github.com/RJ-s-Org/Mockingbird/pull/34):
  - GraphQL generates only the selected fields (with their arguments);
  - the prompt's schema is compact and fitted to a 64 KiB budget, nearest definitions first;
  - the prompt asks for short arrays;
  - the request log API shows the fallback reason.

  Trials re-run on the full upstream contracts (no trimming), OpenRouter `deepseek/deepseek-v4.1-flash` with default routing:

  | Trial | Run 1 (field trial) | After Phases 1–2 | After Phase 3 |
  |---|---|---|---|
  | Stripe `accept-a-payment`, 8 creates + 4 retrieves through the app | 25% fallbacks, trimmed contract | 0/22 fallbacks; 357k prompt tokens and 14.5 s median per call | 0/13 fallbacks; 23k prompt tokens and 5 s median per call |
  | `nuxt/movies`, 5 pages | 28% fallbacks, 20–80 s pages | 4/5 pages real, 1/9 fallbacks (timeout), 9–121 s pages | 5/5 pages real, 0/9 fallbacks, 16–68 s pages |
  | `github-readme-stats`, stats, top-langs, pin cards | trimmed schemas, 33% GraphQL fallbacks | every card errors: 347k prompt tokens, truncated or timed out | stats and top-langs render, 0/8 fallbacks, 750–1,350 prompt tokens, 2.5–7 s; pin card fails (sibling root fields contradict each other) |
- Phase 3 (part), [#35](https://github.com/RJ-s-Org/Mockingbird/pull/35): a GraphQL query's root fields are generated in one LLM call, so they agree; GraphQL-style `{"data": …}` answers are unwrapped. github-readme-stats pin cards: 0/6 → 10/10 render, one call each, ~1.5k prompt tokens, 1.6–3.5 s, no repairs.
- Phase 2, [#32](https://github.com/RJ-s-Org/Mockingbird/pull/32): `$ref`s inside `x-*` extensions resolve; GraphQL `String!` may implement `String`; OpenAPI form-encoded and multipart bodies (with bracket keys such as `a[b][0]`) reach generation; validation failures are reported instead of a faker-filled `default` error.
- [#36](https://github.com/RJ-s-Org/Mockingbird/pull/36): disallowed nulls are patched locally instead of costing a repair call.
- Improvements backlog, one pass ([spec](superpowers/specs/2026-10-22-improvements-backlog-design.md), [plans](superpowers/plans/2026-10-22-improvements-backlog.md)):
  - [#37](https://github.com/RJ-s-Org/Mockingbird/pull/37) (protocols): GraphQL aliases of one field get their own values; chaos errors use the contract's error schema (`default` shape at 503).
  - [#38](https://github.com/RJ-s-Org/Mockingbird/pull/38) (routing and proxy): `mocks.<name>.hosts`, so two APIs share one host (REST at `/`, GraphQL at `/graphql`); `mockingbird proxy` with a local CA, `proxy env`, `ca path`, `proxy.unmapped: pass|block`; placeholder images (`proxy.images`); config reloads are logged and a failed one shows in `/readyz`.
  - [#39](https://github.com/RJ-s-Org/Mockingbird/pull/39) (import and drafts): drafts get `required` lists and nullable fields, keep the docs' operationIds, drop operations below 0.4 confidence; embedded OpenAPI blocks are compiled directly; imports take `urls`/`documents` and `mode: merge|replace`; approving activates; huge-schema and version warnings.
  - [#41](https://github.com/RJ-s-Org/Mockingbird/pull/41) (engine): reads of a stored resource are served from session state; consistency is on by default (per-session cache, list items stored and seeded into detail reads); server-side unique ids; opt-in `unknown_ids: 404`; faker keeps counts small and non-negative, lists non-null, echoes path and argument values, honours `pattern`; runaway strings are capped; `default_scenario` and per-operation scenario instructions.
  - [#43](https://github.com/RJ-s-Org/Mockingbird/pull/43) (agent and MCP): MCP tools to view, edit and approve drafts, `get_request`, `proxy_env`; the key setup page (`open_key_setup`), storing keys in a `0600` file under `~/.mockingbird/secrets` referenced as `file:`; `agent install` writes the key passthrough; smaller MCP outputs and ANSI-free logs.
  - [#44](https://github.com/RJ-s-Org/Mockingbird/pull/44) (webhooks): `mocks.<name>.webhooks` POST an event after each successful write, signed Stripe-style or HMAC-SHA256, retried after 1, 2 and 4 s; `mockingbird webhooks sign|deliveries`; MCP `list_webhook_deliveries`.
  - CI: [#45](https://github.com/RJ-s-Org/Mockingbird/pull/45) (OpenSSL vendored into the binary, distroless `base-nossl` image; crypto-bigint advisories ignored with reasons), [#47](https://github.com/RJ-s-Org/Mockingbird/pull/47) (pulldown-cmark 0.13.5).

  Measured afterwards: the three trials through the proxy (table above).
