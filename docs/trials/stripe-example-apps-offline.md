# Trial: the in-repo Stripe example apps, built by a fresh agent with no model

This trial ran on 2026-11-12, against `main` at `95979a49`. It covered the repository's two Stripe example apps, [`examples/apps/fastapi-stripe`](../../examples/apps/fastapi-stripe) and [`examples/apps/node-shopify-stripe`](../../examples/apps/node-shopify-stripe).

| | |
| --- | --- |
| Apps | The example apps without their `mockingbird.yaml` and with plain tests: they talk to an app already running at `APP_URL` and know nothing about Mockingbird |
| Built by | A fresh headless Claude Code (`claude -p`, Opus 5.5) in a clean copy of each app, with the plugin installed at project scope from the marketplace. One prompt, no hints |
| Serving model | None: the OpenRouter key was out of credit, and the mocks ran `--offline` |
| Scored | Only what doesn't depend on wording: the app's tests through `verify --run`, and a script of create, read, update, list, 404, delete and status checks |

**Prompt** (`<port range>` was 31000–31999 for fastapi and 32000–32999 for shopify):

> Mock the Stripe API for this app with Mockingbird so I can run it and its tests locally without a Stripe account. There's no model API key on this machine. Use ports between `<port range>` for anything you start. When done, tell me how to run the app and the tests against the mock.

For node-shopify-stripe, the prompt says "the external APIs this app uses" in place of "the Stripe API".

## fastapi-stripe

**Build.** 183 s, 35 turns, $1.56. The agent:
- loaded the `mockingbird` and `mockingbird-world` skills;
- took the catalog's Stripe example (customers, PaymentIntents, refunds, a `card-declined` scenario, Stripe-signed webhooks);
- moved the ports and pointed the webhook at the app;
- installed the app's requirements into `.venv`;
- ran `verify --serve ... --run 'pytest -q'` itself.

**App tests**, rerun cold through `verify --run --serve '.venv/bin/uvicorn app:app --port 31242' --serve-port 31242`: **3/3**. Those are checkout, a signed `payment_intent.created` webhook accepted by `stripe.Webhook.construct_event`, and the declined card (402).

**Stripe checks: 17/19.** The example contract has no `cancel` route, and its world gives `confirm` no status change.

## node-shopify-stripe

**Build.** 129 s, 24 turns, $1.27. The agent:
- loaded the two skills;
- took the catalog's Shopify Admin GraphQL and Stripe examples;
- set a shared webhook secret and added `start:mock` and `test:mock` npm scripts;
- left the app's code unchanged.

**App tests**, rerun cold through `verify --run --serve 'node server.js' --serve-port 32000`: **2/2**. Those are products from Shopify, and a PaymentIntent whose signed webhook the app verifies.

**Stripe checks: 17/19**, the same two gaps as fastapi.

A first run, launched in parallel with three other trials, lost its plugin install to a race in Claude Code's shared plugin registry. That agent used the full Stripe spec instead, and its mock showed a read bug: `GET /v1/customers/{customer}` answers `anyOf [customer, deleted_customer]`, which was classified as a list, so created customers never read back. #587 fixes it, and the PR that adds this page makes existing mocks pick up such import fixes after an upgrade.

## What's left

- **The catalog's Stripe example:** `confirm` doesn't move a PaymentIntent's status, and there's no `cancel`. Neither app calls these server-side.
- **`expand[]=customer`** stays the id with the catalog example, because its contract declares `customer` as a plain string and has no `expand` parameter.
- **`OPENROUTER_API_KEY` in the shell:** every agent noticed that `doctor` still saw it, from the shell profile, although the prompt said there was no key. They all kept `--offline`.
