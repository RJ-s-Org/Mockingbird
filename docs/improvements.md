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

## Field trials 2.0, 2026-10-28

Five more open-source apps in other stacks, unmodified, under `mockingbird run` on a local merge of L1, L2, L3 and L6, plus the three earlier trials re-run on the same build. Case studies with configs, commands and per-call numbers: [trials](trials/README.md).

| Trial | Stack | APIs | Result | Calls | Fallbacks |
|---|---|---|---|---|---|
| [plaid/quickstart](trials/plaid-quickstart.md) | Python, Flask | Plaid | Bypassed the proxy (plaid-python's plain `urllib3.PoolManager`) until the Python preload; then 9/9 routes after two catalog operations were added | 10 | 0 |
| [healthchecks](trials/healthchecks.md) | Python, Django, pycurl | Twilio, GitHub App, Slack webhooks, Telegram, Opsgenie | 6/6 channels; Twilio and Telegram answers echoed request fields until the fix | 7 | 0 |
| [`gh` CLI](trials/gh-cli.md) | Go 1.27 | GitHub REST + GraphQL | 8/10 commands; `issue create` null payload, `repo view` corrupt base64 | 15 | 2 |
| [Next.js Commerce](trials/nextjs-commerce.md) | Next.js 15 | Shopify Storefront GraphQL | Build and catalog pages work; the cart is invented | 37 | 0 |
| [Stripe accept-a-payment, Java](trials/stripe-java.md) | Java, Spark | Stripe | 8/8 creates (1.0–2.9 s), 4/4 retrieves | 12 | 0 |
| Stripe accept-a-payment, Node (re-run) | Node | Stripe | 8/8 creates 1.3–2.9 s (was 3.2–7.6 s), 8/8 webhooks verified | 12 | 0 |
| github-readme-stats (re-run) | Node | GitHub REST + GraphQL | 8/9 cards; `search/commits` answers in 11 s (was a 60 s fallback) | 10 | 0 |
| nuxt/movies (re-run) | Nuxt 3 | TMDB | 6/6 pages; median call 9.7 s (was 19.8 s), cold home page 53 s | 11 | 0 |

## Open

### From field trials 2.0 (2026-10-28)

- In Next.js Commerce the first "Add to cart" after the cart is created takes about 10 s to reach the mock (the server action waits on the app side, not on Mockingbird); a second click goes through at once.
- An answer with only one possible value (Slack's literal `"ok"`) is labelled `llm` with 0 tokens and 0 ms.
- github-readme-stats' torvalds/linux pin still gets `user` and `organization` both null (17 completion tokens), which the card reports as an error.
- TMDB list endpoints stay slow: `/movie/popular` took 28 s for 1.8k completion tokens (about 65 tokens/s from the routed provider), so the cold home page took 53 s. A smaller default list size or streaming would help. Facts drift too: TMDB id 94997 came back as "Game of Thrones" (it is House of the Dragon).

### From the proxy trials (2026-10-28)

- Go binaries built with Go < 1.27 on macOS (keychain verifier) and macOS's system Ruby (LibreSSL) ignore `SSL_CERT_FILE`, so HTTPS to mocked hosts fails for them on a Mac; Linux, OpenSSL Rubies and Go 1.27+ (with the `GODEBUG` that `proxy env` now sets) work. A documented opt-in keychain install would close the rest.

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

**Scenarios**

- **An invalid `set` is dropped whole, at request time, with only a WARN** (`scenario set produces an invalid body; ignoring it`): a value outside an `enum`, against a `const` (Slack `ok: false` on a `const: true` schema), a snake_case key in a gRPC message, or an object in a GraphQL list without `__typename`. Addressed for the keys present: the config load and the Admin API now reject a `set` key that no response the scenario can select accepts (a key under `anyOf`/`oneOf` is not resolved, so a GraphQL list item without `__typename` can still slip through); a `set` still dropped at request time is counted by `mb_scenario_set_ignored_total{reason}`.
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

- **`serve` panics on a schema `validate` accepts**: octokit/graphql-schema at `597478f` declares `EnterpriseOwnerInfo.repositoryDeployKeySetting` twice, and async-graphql panics with ``Field `repositoryDeployKeySetting` already exists`` (`dynamic/object.rs:88`). Reject or de-duplicate at compile time. (The example now pins GitHub's own schema, which loads.)
- **Dangling `$ref`s fail compilation**: Square's `api.json` (every release since 2025-10) references undefined `AppFeeAllocation`/`CurrencyExchange`: `unresolvable $ref referenced from schema CreatePaymentRequest`. Treat an unresolvable ref as an open schema with a warning.
- **Form booleans are case-sensitive**: stripe-python sends `automatic_payment_methods[enabled]=True`, which fails with `body/automatic_payment_methods/enabled: "True" is not of type "boolean"`; `true` passes. Accept `True`/`False`.
- **Required header parameters are not checked** (a request without `Notion-Version` gets 200).
- **Request-validation 400 bodies are faker-filled**: Stripe's answer to `amount=abc` carries `type: idempotency_error` and `doc_url: "bravo"` around the real message.
- **Two mocks cannot share a route prefix on one host**, so PayPal is three mocks with mount-relative paths, and every Zendesk path is duplicated with a `.json` suffix. A per-mock path alias or suffix rule would avoid it.
- **One response shape per operation**: Auth0's `include_totals` switches between an array and a page object; Discord's `with_response` between 200 and 204. With two 2xx responses the faker always picks the first.

**Generated data**

- **Offline faker**: optional fields are always filled (a `cursor` or `next_page_uri: "cedar"` sends SDK pagers into endless loops or `GET /cedar`; a healthy OTLP export gets a random `partialSuccess`); GraphQL `ID` and custom scalars (`DateTime`, `URI`, `Decimal`) are random words; a union member is picked ignoring arguments (`search(type: REPOSITORY)` returned a `User`); `format: uri` wins over `pattern`; patterns like `^[\S\s]*$` produce non-ASCII ids; path and query params are not reflected (`account_sid`, `channel_id`, the queried email); a nested request object (HubSpot/Notion `properties`) is not copied on create.
- With an LLM, a 200 whose schema is `anyOf [success, error]` sometimes gets the error branch (Slack); prefer the first branch unless a scenario applies.
- Large list schemas are still slow or fall back: GitHub `GET /pulls` (10 pull objects) fell back after 29 s; GitHub issue lists and Twilio message lists took 18–19 s.

**Proxy and SDKs**

- SDKs that ignore the proxy variables or the CA bundle need explicit settings in the snippets: stripe-python (`stripe.ca_bundle_path`, its own bundled roots), Go binaries built before Go 1.27 on macOS. plaid-python and other OpenAPI-generated clients built on a plain `urllib3.PoolManager` and certifi are covered by the Python preload since field trials 2.0.
- An unresolved webhook secret logs a WARN for every delivery; once per target is enough.
- The `kafka` broker kind is not in the default build (`this build has no kafka support`), so the Kafka example runs on the memory broker unless you use the full image.

## Done

- Relative `sqlite:` paths in `storage.url` / `storage.state_url`, and `server.tls`, `outbound.ca_file` and `brokers.<name>.ca_file`, are relative to the config file's directory, like `contract:` (were the working directory), so `mockingbird run -c elsewhere/mockingbird.yaml` opens the same database as `cd elsewhere && mockingbird run`. The Stripe Java trial hit `unable to open database file` running from a subdirectory.
- Consistency and realism gaps from the examples catalog and field trials 2.0 (PR below; live re-checks of Stripe, GitHub REST, Twilio, Slack and Shopify in its description):
  - Path ids that are not `id` are state keys. The importer reads which response field a path parameter names (`issue_number` → `number`, `issueIdOrKey` → `key`, `MessageSid` → `sid`), treats `{Sid}.json` as a parameter and `Messages.json` as the resource `Messages`, and identifies a created resource without an `id` by `sid` or `uuid`. A read by such a field finds the stored resource (GitHub issues by number, Jira issues by key or id, Twilio messages by sid), and `unknown_ids: 404` applies to them. A create's number moves above the numbers the session holds (and URLs ending in `/issues/<n>` follow), so a new issue no longer collides with a listed one.
  - Any successful command clears the session's cached reads, so a list after a create is generated again knowing the new item, also when the two have different resource types. An object a create's answer wraps (Slack's `chat.postMessage` → `message`) is stored under the list a read holds (`conversations.history`'s `messages`, found inside `anyOf [success, error]` too), and the prompt asks a list answer to include the matching related resources. Live: Twilio's list after a send held the message 3/3 (was missing), Slack's history after three posts held all three (was 0).
  - JSON-object output mode makes the model wrap a top-level array (`{"issues": [...]}`); when the list inside needs a local patch it is now patched and served instead of failing as "not of type array". GitHub `issues`/`pulls` lists: fell back on every call in the re-check before this, now 0 fallbacks.
  - Timestamps are recent: the prompt carries the current time (in the user message, so the system message stays cacheable), and the faker draws `date-time`, `date` and Unix-time integers (`created`, `*_at`, `timestamp`) from the last 30 days, mostly the last 3. Live: Stripe `created`, GitHub `created_at`, Twilio `date_created`, Slack `ts` and Shopify `createdAt` were all the current day. The catalog's date patterns pinned to October 2026 (Twilio, SendGrid, Discord, PayPal, OpenWeatherMap) accept any recent date now, and persona timestamp examples no longer name 2024.
  - Scenarios take `headers:` (HTTP and GraphQL over HTTP), and headers a contract declares are sent with generated values that agree with each other: `Retry-After`, `X-RateLimit-Limit`/`-Remaining`/`-Used`/`-Reset` (remaining `0` on a 429), random tokens for `*-Id` headers such as SendGrid's `X-Message-Id`. The GitHub REST example's `rate-limited` scenario sends `x-ratelimit-*`.
  - GraphQL answers keep money consistent: an object the arguments name (a cart line's variant) takes the price the session stored for it, and line costs, `Cart.cost` (subtotal, total plus any tax) and `totalQuantity` are computed from unit prices and quantities.
  - Persona id templates outside quotes, with a query part, are kept: `gid://shopify/Cart/<26 chars>?key=<32 hex>` on ids that start with `gid://shopify/Cart/`; `<32 hex>` reads like `<32 hex chars>`.
  - Catalog: GitHub REST has the GitHub Apps operations (`GET /app`, installations, `POST /app/installations/{installation_id}/access_tokens`, `GET /repos/{owner}/{repo}/installation`, `GET /installation/repositories`).
- Engine items from field trials 2.0, re-run with the same apps, configs and model ([gh CLI](trials/gh-cli.md#after-the-engine-fixes), [Next.js Commerce](trials/nextjs-commerce.md#after-the-engine-fixes), [plaid/quickstart](trials/plaid-quickstart.md#after-the-engine-fixes)):
  - GraphQL mutations generate only their selection, like queries. `gh issue create`: `createIssue` 38.8k prompt tokens, 16 s and a fallback with a `null` payload (the command printed nothing) → 745 prompt tokens, 1.0 s, `llm`, and the issue URL printed. The gh run's GraphQL prompt tokens fell from 74.7k to 25.9k, with 10/10 commands working (was 8/10) and 0 fallbacks (was 2).
  - GraphQL answers store every object with an `id` under its type (`Cart` → `cart`): a mutation's are merged into the session, a query's added when new. A mutation is generated knowing what its `<x>Id` arguments and `gid://…/<Type>/…` ids name, including inside input objects and lists, and a stored object read inside another (a variant inside its product) links back to it through the field of that type. Next.js Commerce: `cart(id:)` after `cartCreate` was a fresh 13.9 s invention with three random lines; now it is served from state in 1–2 ms, and `cartLinesAdd` answers the variant sent ("Fern Tee, M / Heather Moss", quantity 1, then 2), which the drawer shows. Subscriptions check a stored item against their selection, not the whole type.
  - `POST`s whose last path segment is a read verb (`/accounts/get`, `/transactions/sync`, `/institutions/get_by_id`, `users.list`, `_search`) are reads: cached, seeded (items keyed by `<singular>_id` when they have no `id`), and generated knowing the lists other reads stored under their list members' names. plaid/quickstart: six reads invented six sets of `account_id`s, and the repeated `/accounts/get` took 2.0 s and differed; now all six share the first answer's accounts and balances, and the repeat is a `cache` hit in 0 ms. A webhook target that names such a read's operation still gets its event (Plaid's `transactionsSync`).
  - Base64 fields (`format: byte`, `contentEncoding: base64`, or `content` beside `encoding: "base64"`) are asked for as plain text and encoded locally; a value that does not decode is encoded from its text (corrupt base64 keeps what decodes as readable text). `gh repo view` failed with `invalid UTF-8 string` twice; now it prints the README.
  - Request-validation rejections are logged and labelled with source `rejected`, not `fallback`, so `verify --no-fallbacks` no longer counts a client error as a generation failure.

- Field trials 2.0, developer experience (trials2-dx):
  - `verify --no-unmatched` fails on requests no contract operation matches (Plaid's `POST /accounts/balance/get` passed `--no-fallbacks --no-unmapped`); `report` lists those routes;
  - `verify` prints the totals and one short line per failed check by default (the three most frequent reasons, cut to 120 characters); `--format json` keeps every reason in full (the gh run printed about 10 KB);
  - validation errors name the JSON pointer and keyword, follow a failed `anyOf`/`oneOf` into its closest branch (`/owner: anyOf: no branch matches; closest: /owner/login: type: …`) and mask quoted values over 160 characters, in fallback reasons, logs, repair prompts and request-validation 400s;
  - `mocks.<name>.graphql_path`: Shopify's `/graphql.json` and GitHub's `/graphql` in one config;
  - GraphQL introspection JSON was already accepted as a contract: Shopify's `storefront.schema.json` (`@shopify/hydrogen-react` 2025.7.0) compiles to 72 operations and serves unconverted; the protocols guide says so;
  - the GitHub GraphQL example's `--full` pins GitHub's own `schema.docs.graphql` (github/docs `c6f68e2`, CC-BY-4.0, 309 operations), which has `Issue.issueType`, `blockedBy` and `blocking`;
  - `mb-protocols` SOAP test: the oversized request sends only its head, as #50 did for GraphQL (a client still uploading saw a reset now and then).

- Field trials 2.0 (launch-b1-trials), from the [trials](trials/README.md):
  - `proxy env` sets `PYTHONPATH` to a `sitecustomize` preload: `certifi.where()` names the bundle and a plain `urllib3.PoolManager` goes through `HTTPS_PROXY` (plaid-python reached the real Plaid sandbox before);
  - `proxy env` sets `GODEBUG=x509sslcertoverrideplatform=1`, so Go 1.27+ binaries on macOS and Windows trust the bundle (`gh` works on a Mac);
  - a create or update copies only the request members the response schema declares (Twilio's `To`/`Body`/`RiskCheck`/`StatusCallback` and Telegram's `chat_id`/`parse_mode` were echoed, as were the catalog's Plaid and Auth0 credentials before their objects were closed);
  - `{"data": …, "errors": null}` answers are unwrapped like `{"data": …}`;
  - `minimum`/`maximum`/`multipleOf` violations are patched locally instead of costing a repair call;
  - examples: Plaid gains `/accounts/balance/get` and `/institutions/get_by_id`; GitHub GraphQL mounts at `/graphql` so it combines with GitHub REST on one host, and a test checks that no two catalog mocks collide.

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
