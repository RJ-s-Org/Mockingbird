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

- `storage.url: sqlite://./…` is relative to the working directory, while `contract:` paths are relative to the config file, so `mockingbird run -c elsewhere/mockingbird.yaml` opens a different database than `cd elsewhere && mockingbird run`. Resolve relative SQLite paths against the config's directory (a behaviour change; needs a decision).
- Go on macOS (keychain verifier) and macOS's system Ruby (LibreSSL) ignore `SSL_CERT_FILE`, so HTTPS to mocked hosts fails for them on a Mac; Linux and OpenSSL Rubies work. A per-runtime preload (as for Node.js) or a documented opt-in keychain install would close it.

### From the examples catalog (2026-10-28)

Found while building the 21 [API examples](examples.md) and 3 sample apps, offline and with live checks of Stripe, GitHub REST, Twilio, Slack and Shopify (OpenRouter `deepseek/deepseek-v4.1-flash`, default routing).

**Live checks**

| Example | Calls | Latency | Sources | Shape and state |
| --- | --- | --- | --- | --- |
| Stripe | 3 PaymentIntent creates, retrieve, customer create/list, 2 scenarios | creates 1.2–3.4 s; reads from state 7 ms | all `llm` or `state`, 0 fallbacks | Amount, currency, description, customer copied; `client_secret` = `<id>_secret_…`; list filtered by `email` came from state |
| GitHub REST | repo, issues, create issue, read it back, pulls, search, user | 2.8–29 s (repo 12.9 s, issues 17.9 s, search 14.3 s) | 3 `llm`, 3 `repaired`, **`GET /pulls` fell back after 29 s** | Persona kept (owners, URLs, labels). **Reading the created issue `#412` back returned a different issue**, and the create's number collided with a listed `#412` |
| Twilio | send SMS, fetch it, list, call, Lookups | 1.5–19 s (list 19.1 s, send 9.4 s) | all `llm` | To/From/Body copied, `uri` consistent with the sid. **Fetching the sent message by sid returned another message** |
| Slack | auth.test, 3× chat.postMessage, history, users.info | 0.5–7.5 s | all `llm`, 1 `repaired` | Before an example fix, a plain `chat.postMessage` came back `{"ok": false}`: the LLM picked the error branch of the 200's `anyOf` |
| Shopify Admin GraphQL | shop, products, productCreate, product(id) of it, orders, `userErrors` scenario | 0.6–2.4 s | all `llm` | gid ids, plant titles, prices; **`product(id:)` of the just-created gid returned a different product** |

**State and consistency**

- **Path ids that are not `id`-like are not state keys.** `issue_number`/`pull_number` (GitHub), `{issueIdOrKey}` (Jira) and `{MessageSid}.json` (Twilio, the `.json` suffix is part of the segment) miss the created resource, so create-then-read returns a generated stranger. Treat `*_number`, `*Key`, `*Sid` path params as ids, strip a `.json`/`.xml` suffix before matching, and allocate list numbers above the ones already listed.
- **GraphQL mutation results are not stored**, so `product(id:)` after `productCreate` generates anew. Store a mutation payload's object by its `id` (and global id) like a REST create.
- **A list read after a create omits the created item** when the list was cached earlier in the session (Twilio, Slack history). Invalidate or append on create.
- **Timestamps are not recent.** The LLM has no clock: Stripe `created` came back as 2025-01-15 every time, Slack `ts` values from 2024–2025; offline, `date-time` values spread over 2000–2033. Put the current time in the prompt and bias the faker to the last few days.

**Scenarios**

- **Scenarios cannot set response headers**, and headers a contract declares are never sent: no `Retry-After` on 429s (Slack, Discord, Jira, Zendesk, Auth0, Spotify, Notion declare it; zenpy and slack_sdk read it), no `x-ratelimit-*`, no SendGrid `X-Message-Id`. Add `headers:` to scenarios and emit declared headers with generated values.
- **An invalid `set` is dropped whole, at request time, with only a WARN** (`scenario set produces an invalid body; ignoring it`): a value outside an `enum`, against a `const` (Slack `ok: false` on a `const: true` schema), a snake_case key in a gRPC message, or an object in a GraphQL list without `__typename`. Validate `set` against every response it can apply to when the config loads and reject it there.
- **`null` in `set` deletes the field** (JSON merge patch), so a required nullable field cannot be forced to `null` (Plaid `display_message`).
- **An undeclared 5xx ignores `set`**: `operation repos/get declares no response for forced status 503` comes back as problem+json without the scenario's body. Several examples declare 503 on every operation only for this. Apply `set` to the native answer.
- **GraphQL and gRPC scenarios cannot force a 4xx**: only 200 is declared, so `status: 429` is rejected (`status 429 is not declared by any operation …`), while the `X-Mockingbird-Status: 429` header works (`RATE_LIMITED`). Map 4xx statuses to GraphQL errors (GitHub `RATE_LIMITED`, Shopify `THROTTLED` with `extensions.cost`) and gRPC codes (`RESOURCE_EXHAUSTED`). A forced GraphQL 5xx answers HTTP 200 with `errors[].extensions.status`; GitHub answers a real 502/503.
- **`"4XX"` range responses do not count as declaring 404** (Discord's upstream spec), so scenarios that work on the subset are refused on `--full`.
- The gRPC `set` uses the protobuf JSON mapping (lowerCamelCase keys, int64 as strings); document it in the configuration reference.

**Webhooks**

- **Signing schemes the real APIs use** (each example uses `hmac-sha256` meanwhile): same HMAC as `hmac-sha256` under another header (GitHub `X-Hub-Signature-256` plus `X-GitHub-Event`/`X-GitHub-Delivery`; Notion `X-Notion-Signature`); base64 HMAC-SHA256 (Shopify `X-Shopify-Hmac-Sha256` plus topic and shop headers; Square over URL + body; Zendesk over timestamp + body; HubSpot v3 over method + URI + body + timestamp); Slack `v0=` over `v0:<ts>:<body>`; Twilio base64 HMAC-SHA1 over URL + sorted form params (and callbacks are form-encoded, not JSON); Ed25519 (Discord), ECDSA (SendGrid), JWT (Plaid), certificate signatures (PayPal). A configurable header name and encoding would cover half of these.
- **Event types cannot be mapped to the API's names**: `POST …/Messages.json` → `Messages.json.created`, `/transactions/sync` → `sync.created`, `POST /pulls` → `pull.created`, GraphQL `productCreate` → `productcreatepayload.updated`. Add `webhooks[].event_types: {<operationId>: products/create}` and strip `.json`.
- The upstream GitHub spec is OpenAPI 3.0.3 with `x-webhooks`; those payloads are not used for event bodies.

**Contracts and validation**

- **`serve` panics on a schema `validate` accepts**: octokit/graphql-schema at `597478f` declares `EnterpriseOwnerInfo.repositoryDeployKeySetting` twice, and async-graphql panics with ``Field `repositoryDeployKeySetting` already exists`` (`dynamic/object.rs:88`). Reject or de-duplicate at compile time. The example pins `503d8f1`.
- **Dangling `$ref`s fail compilation**: Square's `api.json` (every release since 2025-10) references undefined `AppFeeAllocation`/`CurrencyExchange`: `unresolvable $ref referenced from schema CreatePaymentRequest`. Treat an unresolvable ref as an open schema with a warning.
- **Form booleans are case-sensitive**: stripe-python sends `automatic_payment_methods[enabled]=True`, which fails with `body/automatic_payment_methods/enabled: "True" is not of type "boolean"`; `true` passes. Accept `True`/`False`.
- **Required header parameters are not checked** (a request without `Notion-Version` gets 200).
- **Request-validation 400 bodies are faker-filled**: Stripe's answer to `amount=abc` carries `type: idempotency_error` and `doc_url: "bravo"` around the real message.
- **Two mocks cannot share a route prefix on one host**, so PayPal is three mocks with mount-relative paths, and every Zendesk path is duplicated with a `.json` suffix. A per-mock path alias or suffix rule would avoid it.
- **One response shape per operation**: Auth0's `include_totals` switches between an array and a page object; Discord's `with_response` between 200 and 204. With two 2xx responses the faker always picks the first.

**Generated data**

- **Request fields are echoed into responses**, including secrets: Plaid `secret`/`client_id`, Auth0 `/oauth/token` `client_secret` and `post_users` `password`, a token endpoint's `grant_type`, Twilio's `To`/`From`/`Body` in request casing beside the schema's `to`/`from`/`body`. Echo only properties the response schema defines, and never fields named like secrets. The examples close their response objects with `additionalProperties: false` meanwhile.
- **Offline faker**: optional fields are always filled (a `cursor` or `next_page_uri: "cedar"` sends SDK pagers into endless loops or `GET /cedar`; a healthy OTLP export gets a random `partialSuccess`); GraphQL `ID` and custom scalars (`DateTime`, `URI`, `Decimal`) are random words; a union member is picked ignoring arguments (`search(type: REPOSITORY)` returned a `User`); `format: uri` wins over `pattern`; patterns like `^[\S\s]*$` produce non-ASCII ids; path and query params are not reflected (`account_sid`, `channel_id`, the queried email); a nested request object (HubSpot/Notion `properties`) is not copied on create. An offline 202 without a body is labelled `x-mockingbird-source: llm`.
- With an LLM, a 200 whose schema is `anyOf [success, error]` sometimes gets the error branch (Slack); prefer the first branch unless a scenario applies.
- Large list schemas are still slow or fall back: GitHub `GET /pulls` (10 pull objects) fell back after 29 s; GitHub issue lists and Twilio message lists took 18–19 s.

**Proxy and SDKs**

- SDKs that ignore the proxy variables or the CA bundle need explicit settings in the snippets: stripe-python (`stripe.ca_bundle_path`), plaid-python and hubspot-api-client (`proxy`, `ssl_ca_cert`), Go on macOS (ignores `SSL_CERT_FILE`). `mockingbird proxy env --python`/`--go` hints, or a doc table per SDK, would help.
- An unresolved webhook secret logs a WARN for every delivery; once per target is enough.
- The `kafka` broker kind is not in the default build (`this build has no kafka support`), so the Kafka example runs on the memory broker unless you use the full image.

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
- L2, zero-edit proxy and routing (launch-l2-proxy):
  - `mockingbird run [-c config] -- <command>` starts the server and proxy unless one answers, runs the command with the proxy environment, stops what it started and exits with the command's code;
  - `proxy env` covers more runtimes with no package install: a Node.js preload in `NODE_OPTIONS` (every `http`/`https` agent, including SDKs' own, and undici's global dispatcher), `JAVA_TOOL_OPTIONS` with a JKS truststore, `CURL_CA_BUNDLE`, `DENO_CERT`; stripe-node reaches the mock without `global-agent`;
  - `mocks.<name>.path_rewrite` (`strip_prefix`/`add_prefix`) mounts one contract under another base path, e.g. TMDB's `/3` contract at `movies-proxy.vercel.app/tmdb`;
  - browsers: a PAC file at `/proxy.pac`, `mockingbird proxy browser` (Chrome/Chromium/Edge through the proxy, trusting the CA by its SPKI hash in a profile of its own), Playwright and Cypress setup in [proxy.md](proxy.md); `proxy.images` entries may name a path prefix on a mocked host (`movies-proxy.vercel.app/ipx`);
  - the reload gap: with `--watch` an added `mocks:` block was applied, but a reload with nothing to change logged only at debug level, `serve -c` without `--watch` never reloaded and said nothing, and a changed `proxy:` section (added in the same edit in the trial) cannot apply until restart. Each reload now logs at info level, `serve` without `--watch` says it reads the file once, and a changed `proxy:` section logs a restart warning.
