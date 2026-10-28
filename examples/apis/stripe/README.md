# Stripe

A stateful mock of the Stripe API for "Juniper & Co", a small plant shop: customers,
PaymentIntents (form-encoded, like the real API), refunds, and webhooks signed with
`Stripe-Signature` so `stripe.webhooks.constructEvent` accepts them.

| | |
| --- | --- |
| Contract | [`stripe.openapi.yaml`](stripe.openapi.yaml), a hand-written subset that keeps Stripe's operation ids |
| Upstream | [stripe/openapi](https://github.com/stripe/openapi) `spec3.json` (MIT), pinned in [`example.json`](example.json); `mockingbird examples init stripe --full` downloads it |
| Hosts | `api.stripe.com` |
| Scenarios | `card-declined` (402), `rate-limited` (429), `outage` (500) |
| Webhooks | `payment_intent.created`, `customer.created` to `http://127.0.0.1:4242/webhook`, signed `stripe` with `env:STRIPE_WEBHOOK_SECRET` |

## Quickstart

```bash
mockingbird examples init stripe --dir stripe && cd stripe   # or: cd examples/apis/stripe
mockingbird serve -c mockingbird.yaml --offline
```

In another shell:

```bash
curl -s localhost:8080/v1/payment_intents -H 'Host: api.stripe.com' -u sk_test_123: \
  -d amount=2000 -d currency=usd -d description="Fiddle leaf fig"
```

The official Python SDK, unmodified, through the proxy:

```bash
eval "$(mockingbird proxy env)"
pip install stripe
python - <<'PY'
import os, stripe
stripe.api_key = "sk_test_123"
stripe.ca_bundle_path = os.environ["SSL_CERT_FILE"]   # stripe-python ships its own CA bundle
intent = stripe.PaymentIntent.create(amount=2000, currency="usd")
print(intent.id, intent.status, intent.client_secret[:12] + "...")
PY
```

Drop `--offline` (and configure an LLM, see [the configuration reference](../../../docs/configuration.md#llm))
for answers that follow the persona: amounts copied from the request, a plant-shop
description, consistent customers.

## Expected output

`--offline` answers come from the schema faker: the shape, id patterns and the fields
copied from the request are right, free text is random words, and values change run to run:

```json
{
  "amount": 2000,
  "currency": "usd",
  "description": "Fiddle leaf fig",
  "id": "pi_LEFtHzCBsMjPE7NXgRva4PZr",
  "client_secret": "pi_0prreXxw90X7RNgD9LnGqtLM_secret_95XhdWIToe3uOfJPTyTcUFua",
  "object": "payment_intent",
  "livemode": false,
  "status": "processing",
  "customer": "raven",
  ...
}
```

With an LLM profile (OpenRouter `deepseek/deepseek-v4.1-flash`, 1.2 to 3.4 s per create):

```json
{"id":"pi_3QkL8mN2pR7sT4vW9xY6zA1b","amount":4500,"currency":"usd","description":"Fiddle leaf fig",
 "customer":"cus_Q1w2e3r4t5y6u7","client_secret":"pi_3QkL8mN2pR7sT4vW9xY6zA1b_secret_5HjK2mNpQrStUvWxYz012345",
 "status":"requires_payment_method","livemode":false, ...}
```

A later `GET /v1/payment_intents/<id>` answers the same object from session state.

The Python snippet prints a line like `pi_geyKZLolcspM1LTDcY393whj requires_payment_method pi_geyKZLolc...`.

## Scenarios

```bash
curl -s localhost:8080/v1/payment_intents -H 'Host: api.stripe.com' -u sk_test_123: \
  -H 'X-Mockingbird-Scenario: card-declined' -d amount=2000 -d currency=usd
# HTTP 402
# {"error":{"code":"card_declined","decline_code":"insufficient_funds","doc_url":"https://stripe.com/docs/error-codes/card-declined","message":"Your card has insufficient funds.",...,"type":"card_error"}}
```

The SDK raises `stripe.error.CardError` for it, and `stripe.error.RateLimitError` for
`rate-limited`. Set `default_scenario: card-declined` on the mock to apply it to every call.

## Webhooks

Run your webhook handler on port 4242 (or change `url`) with the same secret the mock signs with:

```bash
export STRIPE_WEBHOOK_SECRET=whsec_test_local   # any value; the mock and your app share it
mockingbird serve -c mockingbird.yaml --offline
```

Each successful `POST /v1/payment_intents` and `POST /v1/customers` sends an event;
`mockingbird webhooks deliveries stripe` lists the attempts.
