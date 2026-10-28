# Field trials

Real open-source apps run against Mockingbird mocks of the third-party APIs they call, with no source edits. Each case study gives the stack, the APIs, the exact config, the commands and the numbers. The summary and what the trials found to fix are in [improvements](../improvements.md#field-trials-20-2026-10-28).

## How a trial runs

1. Clone the app and get it building.
2. List every outbound API host and the operations the app's main flows call; take contracts from the [examples catalog](../examples.md) where it has them (`mockingbird examples init <name> [--full]`), else upstream or hand-written.
3. Write `mockingbird.yaml` with `hosts:` for each API, then run the app under `mockingbird run -c mockingbird.yaml -- <command>` (the proxy environment for every runtime, [proxy.md](../proxy.md)). The app's tracked files stay untouched; settings go in environment variables.
4. Drive the main flows (scripted with curl, the app's CLI, or a browser for client-side steps).
5. Measure from `GET /api/v1/workspaces/default/mocks/<mock>/requests` (calls, sources, fallbacks, latency, tokens) and `mockingbird report`, and hold the run to `mockingbird verify --no-fallbacks --no-unmapped`.

All runs used OpenRouter `deepseek/deepseek-v4.1-flash` with default routing.

## Field trials 2.0 (2026-10-28)

New apps, on a local merge of L1 (#57), L2 (#56), L3 (#53) and L6 (#54), then on this branch for the fixes:

| App | Stack | APIs | Result | Calls | Fallbacks | Latency (generated) |
| --- | --- | --- | --- | --- | --- | --- |
| [plaid/quickstart](plaid-quickstart.md) | Python, Flask, plaid-python | Plaid | 9/9 routes after the Python preload and two catalog operations; account ids differ between calls | 10 | 0 | 0.7–3.6 s, median 2.0 s |
| [healthchecks](healthchecks.md) | Python, Django, pycurl | Twilio SMS and voice, GitHub App issues, Slack webhooks, Telegram, Opsgenie | 6/6 channels notified; request fields no longer echoed | 7 | 0 | 0.4–2.2 s |
| [`gh` CLI](gh-cli.md) | Go 1.27 | GitHub REST and GraphQL | 8/10 commands; `issue create` gets a null payload, `repo view` fails on corrupt base64 | 15 | 2 | 0.5–6.0 s, median 1.9 s |
| [Next.js Commerce](nextjs-commerce.md) | Next.js 15, React 19 | Shopify Storefront GraphQL | `next build` and every catalog page; the cart is invented | 37 | 0 | 0.8–13.9 s, median 2.1 s |
| [Stripe accept-a-payment, Java](stripe-java.md) | Java, Spark, stripe-java | Stripe, webhooks | 8/8 creates, 4/4 retrieves | 12 | 0 | 1.0–2.9 s, median 1.6 s |

The three earlier trials, re-run unmodified on the same build:

| App | Stack | Result | Calls | Sources | Latency (generated) | Then (2026-10-28 proxy trial) |
| --- | --- | --- | --- | --- | --- | --- |
| Stripe accept-a-payment, Node ([report](stripe-java.md#what-happened)) | Node, Express, stripe-node | 8/8 creates, 4/4 retrieves, 8/8 webhooks verified | 12 | 5 `llm`, 3 `repaired`, 4 `state` | creates 1.3–2.9 s, median 1.8 s | creates 3.2–7.6 s, median 3.7 s |
| github-readme-stats | Node, Express, axios | 8/9 cards render; the torvalds/linux pin gets `user` and `organization` both null ("Something went wrong") | 10 | 9 `llm`, 1 `cache` | 0.6–11.1 s, median 1.4 s; `search/commits` 11.1 s | `search/commits` fell back after 60 s, twice |
| nuxt/movies | Nuxt 3 SSR, Node `fetch` | 6/6 pages (home, movie and TV lists, two details, home again) | 11 | 8 `llm`, 3 `repaired` | 3.7–28.1 s, median 9.7 s; cold home page 53 s | median 19.8 s |

Every run passed `verify --no-fallbacks --no-unmapped` except `gh` (2 fallbacks). The bar missed three real failures: two Plaid routes that 404'd (a path the contract lacks is a 4xx, not an unmapped host), the invented Commerce cart, and the torvalds/linux pin card (a valid answer the app cannot use).

All runs together used about 650k prompt and 100k completion tokens (the first field trials spent about $1 on roughly 5M prompt tokens). Most of the prompt went to Stripe's 17k-token `PaymentIntent` schema, gh's two oversized mutations and the Storefront cart types.
