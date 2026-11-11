# Trial: Stripe accept-a-payment, four Node servers and the sample's own specs

[stripe-samples/accept-a-payment](https://github.com/stripe-samples/accept-a-payment) at `1a57c3b`, run on 2026-11-10 against `main` at `1e4249be` (#391).

| | |
| --- | --- |
| Integrations | `custom-payment-flow`, `payment-element`, `prebuilt-checkout-page`, `elements-with-checkout-sessions`, each with its Node server (Express, stripe-node) |
| Tests | the sample's RSpec server specs (`spec/*_server_spec.rb`, `spec/prebuilt_checkout_page_spec.rb`), unmodified. They call the app and then read the created objects back through stripe-ruby |
| Stripe calls | `POST /v1/payment_intents`, `GET /v1/payment_intents/{intent}`, `POST /v1/customers`, `POST /v1/checkout/sessions`, `GET /v1/checkout/sessions/{session}` (with `expand[]=line_items`) |
| Contract | Stripe `spec3.json` (8 MB), imported unmodified as a mock in `mockingbird.yaml` |
| Model | OpenRouter `deepseek/deepseek-v4.1-flash-1017`, `api_key: env:OPENROUTER_API_KEY` |
| Built with | `mockingbird init`, `world show`, a `mocks.stripe.world` overlay, curl self-tests, then `mockingbird run` |
| Source edits | None. Each server got a git-ignored `.env` with placeholder keys and `PRICE=price_1Mockingbird000000` |
| Result | **14/16 spec examples pass.** All main server flows work. The 2 failures are both Stripe error answers the mock can't produce |

## How the mock was built

1. Ran `mockingbird init --provider openrouter --model deepseek/deepseek-v4.1-flash-1017 --contract ./stripe.json`, then added `route_prefix: /`, `hosts: [api.stripe.com]`, a persona, `unknown_ids: "404"`, `world: ./stripe-world.yaml` and `proxy`.
2. `world show stripe` listed 287 gaps. I wrote an overlay for the operations the app uses:
   - lifecycles for `payment_intents` and `sessions`;
   - `GetCustomersCustomer` as a `read`, because the heuristic classed it as a `list`;
   - cancel and expire transitions;
   - after the first self-test, `set: {id: "'pi_' + mint(...)"}` and `"'cus_' + mint(...)"` on the two creates.
3. Self-tested with curl: create, read back, create a checkout session, read it with `expand`. Then ran the specs and flows under `mockingbird run -c mockingbird.yaml -- ./runall.sh`.
4. stripe-ruby passes an explicit nil proxy to `Net::HTTP`, so it ignores `HTTPS_PROXY`. For the spec process only, I preloaded `Stripe.proxy = ENV['HTTPS_PROXY']` and `Stripe.ca_bundle_path = ENV['SSL_CERT_FILE']` through `RUBYOPT`.

## Flows that work

- **custom-payment-flow:**
  - `/config`;
  - 9 creates: card USD/EUR/GBP, ACSS CAD with its `mandate_options`, iDEAL, SEPA, Link, Bancontact, and customer_balance with `customers.create` and `confirm`;
  - stripe-ruby retrieve gives back the same `payment_method_types` and currency;
  - `/payment/next`, 4 of 4, redirected from `state`.
- **payment-element:** `GET /create-payment-intent` with `automatic_payment_methods`, then a retrieve. 3 of 3 specs pass.
- **prebuilt-checkout-page:**
  - `POST /create-checkout-session` answers 303 to `https://checkout.stripe.com/c/pay/cs_test_…`;
  - retrieve with `expand[]=line_items` comes from state;
  - `/checkout-session?sessionId=` works.
- **elements-with-checkout-sessions:**
  - create, then retrieve with `expand[]=line_items`, comes from `state`;
  - `line_items.data` holds the request's `price_data` line;
  - `adaptive_pricing.enabled` and `customer_email` are preserved;
  - `/session-status` gives `open`/`unpaid`.
- Mock totals: 0 fallbacks. Every read of a created object came from `state` (18 of 18).

Not tested: browser confirmation through Stripe.js, which talks to Stripe's own servers. The Playwright e2e specs are out of scope.

## Failures

Each failure below lists the request, what Stripe answers, what the mock answered, and the class.

1. **AU-only payment method with a non-AUD currency isn't refused.**
   - Request: `POST /v1/payment_intents` with `currency=usd&payment_method_types[]=au_becs_debit`.
   - Expected: 400, `This payment method is available to Stripe accounts in AU…`.
   - Got: 200 `requires_payment_method`.
   - The spec "fails gracefully if missmatched currency" fails.
   - The fix I tried was a `refuse` entry with `status: 400`. The world validator refused it: "operation `PostPaymentIntents` declares no 400 response". Stripe declares only `default` for errors, and the validator doesn't count `default` as declaring any error status.
   - **Class: engine bug.** OpenAPI's `default` covers every undeclared status. With that fixed, `refuse` can express this failure.
2. **Retrieving an unknown id answers 200 instead of 404.**
   - Request: `GET /v1/checkout/sessions/cs_test_invalid`. The same happens for `/v1/payment_intents/pi_unknown123` and `/v1/customers/cus_unknown1`.
   - Expected: 404 `resource_missing`, which the app turns into a 400.
   - Got: 200 from `llm`, an invented session. The spec "returns an error for an invalid session_id" fails.
   - `unknown_ids: "404"` is set (`mocks get` confirms it) and does nothing: Stripe's path parameters (`{intent}`, `{session}`, `{customer}`) aren't "id-like" by name, even though the world gives each read `target: {path_param: …}` on a type whose identity is `id`.
   - `not_found: 404` on the read is refused for the same reason as failure 1 (`default` only).
   - **Class: engine bug.** A world-described target should count as a path id, and `default` should be accepted.
3. **Ids minted by the executor were faker words.**
   - Request, before the `set` fix: `POST /v1/payment_intents` with `amount=5999&currency=cad`.
   - Expected: `id: pi_…` and `client_secret: <id>_secret_…`.
   - Got: `"id": "lumen"` and `"client_secret": "pi_lumen_secret_…"`, and the customer was `"id": "fjord"`. The app's `clientSecret.split('_secret')[0]` gives `pi_lumen`, an id the mock doesn't hold.
   - The model wrote `pi_…` ids, but the executor's minted id replaced them. The contract has no id pattern, and the persona says to use `pi_`.
   - **Class: engine bug.** The minted id ignores the model's or the persona's id format. The skill gap in failure 4 made it worse.
4. **The skills never mention the tools that fixed or would fix failures 1 to 3.**
   - The `mockingbird-world` skill documents `changes`, `transition`, `not_found` and `idempotency`, but not `set`, `mint()`, `refuse`, `computed` or `effects`. The `mockingbird-mock-api` skill doesn't mention `unknown_ids`.
   - I found all of these in `docs/world.md` and `docs/configuration.md`, outside the skills.
   - Related: a world-file entry refused on reload shows up only in `server.log`. `world show` reports `refused: []` and no gap for it.
   - **Class: gap in the skills or MCP.** The refusal reporting is a smaller engine or reporting bug.
5. **The Checkout Session `url` fragment degenerates into a repetition loop.**
   - Request: `POST /v1/checkout/sessions` with `mode=payment&line_items[0][price]=…&line_items[0][quantity]=3`.
   - Expected: a short `#…` fragment.
   - Got: `cs_test_a1B2c3D4…#fidkdWxOYHwnPyd1blpxYHZxWjA0TzZ1c0Z2b0N2Q2N1…`, about 10 KB of repeats, and up to 5.6k completion tokens. Creates took 10 to 50 s (median 21 s). `amount_total` is `null`.
   - The app still redirects, so no test fails.
   - **Class: the model's answer.**
6. **customer_balance with `confirm=true` gives `status: requires_action` but `next_action: null`.**
   - Expected: `next_action.type: display_bank_transfer_instructions`.
   - The app returns `nextAction: null`, so the client would show nothing. No spec covers this.
   - **Class: the model's answer.** It came from an executor create whose values the model fills, so it could also be the executor dropping `next_action`. Not isolated.

Minor:
- The executor's creates answer `payment_method_options` with an empty `{}` for every one of the roughly 50 payment-method keys, beside the one the request sent. This is harmless here.
- A request that fails validation (`rejected`, 400) still spent an 18k-token model call to write its error body.

## Numbers

| | |
| --- | --- |
| Requests to the mock | 58 (specs, flows, self-tests and probes): 39 from `state`, 16 from `llm`, 3 `rejected` (my own malformed curl), 0 fallbacks |
| Model-served (`llm`) latency | p50 10.3 s, p95 23.3 s, max 49.9 s (n = 16). Checkout creates dominate this: 10 to 50 s |
| Executor creates that filled values with the model | 21 PaymentIntent and Customer creates: p50 5.1 s, p95 15.1 s (2.2 to 15.6 s) |
| All calls that used the model | 38: p50 5.4 s, p95 22.5 s |
| Tokens | 846k prompt, 20k completion. Every Stripe call carries a 17k to 25k-token prompt |
| OpenRouter spend | $0.20 at most: the shared key's usage went from $43.403 to $43.602 over the trial, and other sessions use the same key |

## Rerun on 2026-11-11

This rerun used the same mock, specs and flows, against `main` with #399, #400, #401, #413, #423 and #426, plus #430 and #433. The world gained a `PostCheckoutSessions` create that mints `cs_test_` ids and builds the hosted `url` from the session's id. The executor now serves Checkout creates, so this is the first run that needs it.

| | 2026-11-10 | 2026-11-11 |
| --- | --- | --- |
| Spec examples passing | 14/16 | **16/16** |
| Requests answered from state | 67% (39 of 58) | **100%** (31 of 31) |
| All requests | | p50 1.8 s, p95 6.6 s |
| Calls that used the model | p50 5.4 s, p95 22.5 s | p50 2.8 s, p95 8.8 s (n = 19) |
| PaymentIntent creates | p50 5.1 s, p95 15.1 s | p50 2.3 s, p95 3.8 s (n = 13) |
| Checkout creates | 10 to 50 s (full generation) | p50 8.8 s, p95 14.1 s (n = 4, executor plus fill) |
| Tokens | 846k prompt, 20k completion | 244k prompt, 8.6k completion |
| OpenRouter spend | $0.20 at most | $0.11 for this rerun and the one before it; $1.13 for all the latency work on this trial since 2026-11-10 (shared key, other sessions included) |

What changed:
- **Prompts are smaller** (#399): about 6k tokens for a PaymentIntent fill, down from 25k.
- **Model calls are capped** in output and time (#400, #401), and a slow one is hedged after 2 s (#423).
- **The executor and fill handle more:**
  - a fill nulls nullable references and optional nullable objects instead of asking the model (#413, #433);
  - a sent `line_items` list no longer pushes the Checkout create into full generation (#430).

Still slow: a Checkout fill writes 800 to 1,750 tokens against a 13k to 20k-token prompt. It still asks for:
- the session's required objects (`automatic_tax`, `custom_text`, `custom_fields`, `shipping_options`);
- its optional non-nullable objects (`branding_settings`, `line_items`, `tax_id_collection`, …);
- about 30 scalars.

## Files

These are outside the repo, in `mockingbird-trials/accept-a-payment/mb-trial-1110/`:
- `mockingbird.yaml` and `stripe-world.yaml`;
- `selftest.sh`, `runall.sh` (the specs), `flows.sh` and `flows-checkout.sh`;
- `stripe_ruby_proxy.rb`, the spec-side preload;
- `run2.txt`, the spec output, and `run3.txt` and `run4.txt`, the flow output;
- `rerun.sh` and `score.py`, and the 2026-11-11 rerun's `rerun-r5.txt` (specs), `rerun-r5.json` (request log) and `rerun-r5-world.json`.
