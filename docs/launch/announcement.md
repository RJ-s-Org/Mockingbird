# Announcement draft

For Show HN (title and the first paragraph), and as a blog post (the whole page). Numbers come from [the field trials](../improvements.md); check them against the release before posting.

---

**Title:** Show HN: Mockingbird – API mocks written by an LLM, consistent per session, for every protocol

Mockingbird is an open-source mock server (Rust, MIT/Apache) that takes an API contract (OpenAPI, GraphQL, protobuf, WSDL or AsyncAPI) and answers with data an LLM writes for each request, validated against the schema and kept consistent for the session. Its HTTPS proxy routes an app's calls to `api.stripe.com` or `api.github.com` to the mocks, so you can run the app as it is: `mockingbird run -- npm start`.

## Why we built it

Mock servers make you choose. Hand-written stubs are exact but take hours per API and rot as the API changes. Schema-driven fakers are instant but return `"string"` and `12345`, so a UI full of them tells you nothing, and a `GET` after a `POST` returns something unrelated. And most of them need you to change the app's base URL, which for SDKs like stripe-node or Octokit means patching code you do not own.

We wanted the third option: point at a contract, get data that looks like production, and have the mock remember what you did.

## What it does

- **An LLM writes the answers; the schema decides.** Each response is generated from the operation, the request, the session's state and a persona you write ("a small bookshop in Berlin, prices in EUR"). Mockingbird validates it, patches small errors locally (a pattern, a length, a stray null), and falls back to a schema-valid faker when the model times out or misbehaves. Every response says which happened in `X-Mockingbird-Source`, and why it fell back. `--offline` skips the LLM entirely, for CI.
- **State without scripts.** A create is remembered; the following read returns it; list items can be read back by id; repeated reads are cached. Sessions are isolated by a header and can be exported and seeded as fixtures.
- **No app changes.** Give a mock `hosts: [api.stripe.com]`, and the proxy (with a local CA that is never installed system-wide) answers for it. Everything else passes through.
- **Every protocol.** REST, GraphQL with subscriptions, gRPC with reflection and all four call kinds, SOAP 1.1/1.2, SSE, WebSocket, and AsyncAPI channels on Kafka, AMQP and MQTT, from one binary.
- **No contract? Draft one.** Smart import turns documentation pages, a HAR recording or a Postman collection into a contract with per-operation confidence, for you to review.
- **Built for coding agents.** `mockingbird mcp` exposes the whole product as MCP tools, with skills and plugins for Claude Code, Codex, Cursor, Gemini CLI, Kiro, VS Code, Windsurf and ChatGPT. "Mock the APIs this app calls and run it" is a reasonable thing to ask your agent.
- **Scenarios, chaos and webhooks.** Named scenarios force statuses, latency or fields; chaos injects errors in the API's own error format; writes can fire Stripe-style signed webhooks.

## Does it work on real apps?

We ran three open-source apps against mocks of their real APIs, unmodified (`git diff` empty), started under the proxy, with `deepseek/deepseek-v4.1-flash` on OpenRouter:

- **Stripe's `accept-a-payment` sample** against Stripe's full 8 MB OpenAPI: 12 of 12 calls answered with no fallbacks, and all 9 `payment_intent.created` webhooks passed `stripe.webhooks.constructEvent`.
- **`github-readme-stats`** against GitHub's REST and GraphQL schemas on one host: every card rendered; the repeated stats query came from the cache.
- **`nuxt/movies`** against a TMDB contract drafted from its documentation: every page rendered with consistent titles and cast, and detail pages agreed with the lists they came from.

The whole round cost well under a dollar in tokens.

## What it is not (yet)

- **Fast on a cold write.** A generated create takes a few seconds (3–8 s in the Stripe trial; large TMDB pages took 20 s or more). Reads of state and cache hits are milliseconds, and `--offline` is instant.
- **Exact.** If your test needs byte-identical responses, use scenarios with `set` fields, fixtures, `--offline`, or a hand-written stub tool; Mockingbird aims for plausible and consistent, not scripted.
- **A browser proxy out of the box.** Server-side traffic is covered; browsers need a proxy setting and the CA. The [known gaps](../improvements.md) are tracked in the open.

## Try it

```sh
curl -fsSL https://raw.githubusercontent.com/rj-s-org/mockingbird/main/install.sh | sh
mockingbird serve --offline orders.yaml
```

The docs, the comparison with WireMock, Prism, Mockoon and Microcks, and the examples are at https://github.com/RJ-s-Org/Mockingbird. We would love real-world contracts that break it; there is an issue template just for that.
