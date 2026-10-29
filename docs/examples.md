# API examples

Ready-made mocks of 21 APIs developers integrate most, and sample apps that test against
them. Each example is a directory under [`examples/apis/`](../examples/apis) and is also
built into the binary, so you can start one anywhere:

```bash
mockingbird examples list                          # the catalog, as JSON
mockingbird examples show stripe                   # its entry, config and README
mockingbird examples init stripe --dir stripe      # write it into ./stripe
cd stripe && mockingbird serve -c mockingbird.yaml --offline
```

Every example has:

- a **contract**: a compact, hand-written subset of the real API (its paths, field names,
  id formats, error bodies and, where the official spec has them, its operation ids), so it
  validates, serves offline and fits in the binary;
- a **`mockingbird.yaml`** with `hosts:` set to the real API host and the proxy enabled,
  so an unmodified app or SDK reaches the mock under `eval "$(mockingbird proxy env)"`;
- a **persona**: a named fictional company (Juniper & Co, Fernwood Labs, Lantern Labs, ...)
  and the API's real id formats, which the LLM follows when you drop `--offline`;
- **scenarios** for the failures worth testing: rate limits, the API's typical domain error
  (card declined, invalid phone number, record not found) and an outage;
- **webhooks** where the real API sends them;
- a **README** with a three-command quickstart (serve, a curl, an official SDK through the
  proxy) and the output to expect.

## Catalog

| Example | Protocol | Hosts | Scenarios | Webhooks | Upstream spec | SDK in the README |
| --- | --- | --- | --- | --- | --- | --- |
| [Stripe](../examples/apis/stripe) | OpenAPI | `api.stripe.com` | `card-declined`, `rate-limited`, `outage` | stripe | [MIT](https://github.com/stripe/openapi), `--full` | `stripe` (python) |
| [PayPal](../examples/apis/paypal) | OpenAPI | `api-m.sandbox.paypal.com`, `api-m.paypal.com` | `instrument-declined`, `rate-limited`, `outage` | hmac-sha256 | [Apache-2.0](https://github.com/paypal/paypal-rest-api-specifications), `--full` | `paypal-server-sdk` (python) |
| [Square](../examples/apis/square) | OpenAPI | `connect.squareupsandbox.com`, `connect.squareup.com` | `card-declined`, `generic-decline`, `rate-limited`, `outage` | hmac-sha256 | [Apache-2.0](https://github.com/square/connect-api-specification), not fetched | `squareup` (python) |
| [Plaid](../examples/apis/plaid) | OpenAPI | `sandbox.plaid.com`, `production.plaid.com` | `item-login-required`, `rate-limited`, `outage` | hmac-sha256 | unknown, not vendored | `plaid-python` (python) |
| [GitHub REST](../examples/apis/github-rest) | OpenAPI | `api.github.com` | `rate-limited`, `not-found`, `outage` | hmac-sha256 | [MIT](https://github.com/github/rest-api-description), `--full` | `PyGithub` (python) |
| [GitHub GraphQL](../examples/apis/github-graphql) | GraphQL | `api.github.com` | `rate-limited`, `slow`, `outage` | – | [CC-BY-4.0](https://github.com/github/docs/blob/main/src/graphql/data/fpt/schema.docs.graphql), `--full` | `@octokit/graphql` (node) |
| [Shopify Admin GraphQL](../examples/apis/shopify-admin-graphql) | GraphQL | `juniper-plants.myshopify.com` | `invalid-product`, `slow`, `outage` | hmac-sha256 | proprietary, not vendored | `@shopify/admin-api-client` (node) |
| [Twilio](../examples/apis/twilio) | OpenAPI | `api.twilio.com`, `lookups.twilio.com` | `invalid-to-number`, `rate-limited`, `outage` | hmac-sha256 | [MIT](https://github.com/twilio/twilio-oai), `--full` | `twilio` (python) |
| [Slack Web API](../examples/apis/slack) | OpenAPI | `slack.com` | `channel-not-found`, `ratelimited`, `outage` | – | [MIT](https://github.com/slackapi/slack-api-specs), `--full` | `slack_sdk` (python) |
| [Twilio SendGrid](../examples/apis/sendgrid) | OpenAPI | `api.sendgrid.com` | `invalid-recipient`, `rate-limited`, `outage` | hmac-sha256 | [MIT](https://github.com/twilio/sendgrid-oai), not fetched | `sendgrid` (python) |
| [Discord](../examples/apis/discord) | OpenAPI | `discord.com` | `unknown-channel`, `rate-limited`, `outage` | – | [MIT](https://github.com/discord/discord-api-spec), `--full` | `httpx` (python) |
| [HubSpot CRM](../examples/apis/hubspot) | OpenAPI | `api.hubapi.com` | `contact-exists`, `invalid-email`, `rate-limited`, `outage` | hmac-sha256 | proprietary, not vendored | `hubspot-api-client` (python) |
| [Notion](../examples/apis/notion) | OpenAPI | `api.notion.com` | `object-not-found`, `validation-error`, `rate-limited`, `outage` | hmac-sha256 | [MIT](https://github.com/makenotion/notion-mcp-server), `--full` | `notion-client` (python) |
| [Jira Cloud](../examples/apis/jira) | OpenAPI | `juniper.atlassian.net` | `rate-limited`, `issue-not-found`, `invalid-field`, `outage` | unsigned | unknown, not vendored | `jira` (python) |
| [Zendesk Support](../examples/apis/zendesk) | OpenAPI | `juniper.zendesk.com` | `rate-limited`, `not-found`, `record-invalid`, `outage` | hmac-sha256 | proprietary, not vendored | `zenpy` (python) |
| [Auth0 Management API](../examples/apis/auth0-management) | OpenAPI | `juniper.us.auth0.com` | `rate-limited`, `user-exists`, `user-not-found`, `outage` | – | proprietary, not vendored | `auth0-python` (python) |
| [Spotify](../examples/apis/spotify) | OpenAPI | `api.spotify.com`, `accounts.spotify.com` | `token-expired`, `nothing-playing`, `not-found`, `rate-limited`, `outage`, `invalid-client` | – | proprietary, not vendored | `spotipy` (python) |
| [TMDB](../examples/apis/tmdb) | OpenAPI | `api.themoviedb.org`, `image.tmdb.org` | `rate-limited`, `not-found`, `invalid-api-key`, `outage` | – | proprietary, not vendored | `tmdbsimple` (python) |
| [OpenWeather](../examples/apis/openweathermap) | OpenAPI | `api.openweathermap.org` | `city-not-found`, `invalid-api-key`, `rate-limited`, `outage` | – | proprietary, not vendored | `pyowm` (python) |
| [Kafka commerce events (AsyncAPI)](../examples/apis/kafka-events) | AsyncAPI | – | – | – | hand-written only | `confluent-kafka` (python), not run |
| [OpenTelemetry OTLP (gRPC)](../examples/apis/grpc) | gRPC | – | `partial-success`, `collector-unavailable`, `deadline-exceeded` | – | [Apache-2.0](https://github.com/open-telemetry/opentelemetry-proto), not fetched | `opentelemetry-exporter-otlp-proto-grpc` (python) |

*Upstream spec*: `--full` means `mockingbird examples init <name> --full` downloads the
API's complete official spec from a pinned commit, checks its SHA-256 and points the
config at it. Scenarios the full spec cannot answer (it may not declare the 402 or 429 the
subset does) are left out and listed under `dropped_scenarios` in the output. *Not
fetched* means the official spec is openly licensed but cannot replace the subset as one
file (SendGrid splits it into 46 files, Square's has unresolvable `$ref`s, OpenTelemetry's
protos import each other). *Not vendored* means the vendor publishes no openly licensed
spec, so only the hand-written subset exists. Each `example.json` records the source,
licence, commit and hash.

*Webhooks*: `stripe` signs exactly as Stripe does, so `stripe.webhooks.constructEvent`
accepts the events. The other APIs sign with schemes Mockingbird cannot produce yet
(GitHub's `X-Hub-Signature-256`, Shopify's base64 HMAC, Twilio's HMAC-SHA1, Slack's `v0`,
Ed25519 and ECDSA signatures, ...), so those examples use `hmac-sha256`
(`X-Mockingbird-Signature: sha256=<hex>`) and each README says what the real header
looks like. See [improvements](improvements.md#from-the-examples-catalog-2026-10-28).

*SDK*: every snippet except the Kafka consumer was run against its example through the
proxy. Some SDKs bypass the proxy variables or ship their own CA bundle (stripe-python,
plaid-python, hubspot-api-client); their snippets pass the CA or proxy explicitly.

## Using an example

```bash
mockingbird examples init twilio --dir twilio && cd twilio
mockingbird serve -c mockingbird.yaml --offline
```

- **curl** the data plane with the API's host: `curl localhost:8080/... -H 'Host: api.twilio.com'`.
  A mock with `hosts:` answers only under those hosts.
- **Run your app or an SDK unmodified** through the proxy: `eval "$(mockingbird proxy env)"`
  in its shell, then call the real URL. See [the proxy guide](proxy.md).
- **Pick a scenario** per request with `X-Mockingbird-Scenario: <name>`, or for every call
  with `default_scenario` on the mock.
- **Get realistic data**: drop `--offline` and configure an LLM profile
  ([configuration](configuration.md#llm)). Offline answers come from the schema faker:
  right shapes and id formats, random words for free text.
- **Receive webhooks**: run your handler at the URL in `webhooks[].url` and export the
  secret variable the config names (for example `STRIPE_WEBHOOK_SECRET`); any value works,
  as long as your app uses the same one.

## Sample apps

Small apps in different stacks, each with a test that starts Mockingbird offline and runs
the app against it through the proxy. They are tested in CI.

| App | Stack | Shows |
| --- | --- | --- |
| [`fastapi-stripe`](../examples/apps/fastapi-stripe) | Python, FastAPI, `stripe` | Checkout with PaymentIntents, Stripe-signed webhooks verified by `stripe.Webhook.construct_event`, a declined card |
| [`go-github`](../examples/apps/go-github) | Go, `go-github` | A triage digest: list issues and pull requests, post an issue, handle GitHub's rate-limit error |
| [`node-shopify-stripe`](../examples/apps/node-shopify-stripe) | Node, `@shopify/admin-api-client`, `stripe` | Products from the Shopify Admin GraphQL API and orders from verified Stripe webhooks |

## Adding an example

1. Create `examples/apis/<name>/` with `example.json`, `mockingbird.yaml`, `README.md`
   and the contract (copy [`stripe`](../examples/apis/stripe)). `example.json` names the
   contracts, hosts, scenarios, upstream spec (source, licence, commit, URL, SHA-256) and
   the request the smoke test sends.
2. Add it to `EXAMPLES` in [`crates/mockingbird/src/examples.rs`](../crates/mockingbird/src/examples.rs);
   `cargo test -p mockingbird-cli --test it -- examples_catalog::` fails until every file is embedded.
3. Run `scripts/smoke-examples.sh target/debug/mockingbird <name>`: it validates the
   contracts, serves the example offline and sends the smoke request. The
   [Examples workflow](../.github/workflows/examples.yml) runs it for every example, and
   the sample apps' tests, when `examples/` changes.

Vendor a third-party spec only when its licence allows redistribution; otherwise
reference it by a pinned URL, or write the subset by hand from the public reference.
