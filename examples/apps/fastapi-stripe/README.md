# FastAPI + Stripe: checkout with verified webhooks

A small FastAPI service for "Juniper & Co" that creates Stripe PaymentIntents with the
official `stripe` SDK and accepts Stripe webhooks, verifying `Stripe-Signature` with
`stripe.Webhook.construct_event`. The code has no Mockingbird-specific paths: it talks to
`https://api.stripe.com`, and the tests send it through `mockingbird proxy` to the
[Stripe example](../../apis/stripe)'s contract.

| File | |
| --- | --- |
| [`app.py`](app.py) | `POST /checkout`, `POST /webhook`, `GET /events` |
| [`mockingbird.yaml`](mockingbird.yaml) | The Stripe mock, webhooks to `http://127.0.0.1:4242/webhook`, a `card-declined` scenario |
| [`conftest.py`](conftest.py) | Starts `mockingbird serve --offline` and the app with `mockingbird proxy env`'s variables |
| [`test_checkout.py`](test_checkout.py) | Checkout, a signed `payment_intent.created` webhook reaching the app, a declined card |

## Run the tests

```bash
cd examples/apps/fastapi-stripe
python -m venv .venv && . .venv/bin/activate && pip install -r requirements.txt
MOCKINGBIRD_BIN=$(command -v mockingbird) pytest -q
```

```text
...                                                                      [100%]
3 passed in 4.82s
```

No network access or Stripe account is needed: the mock runs `--offline` on ports
48080/48090/48888, and the app listens on 4242.

## Run it by hand

```bash
export STRIPE_WEBHOOK_SECRET=whsec_test_juniper
mockingbird serve -c mockingbird.yaml --offline          # shell 1
eval "$(mockingbird proxy env)"                          # shell 2
uvicorn app:app --port 4242
curl -s localhost:4242/checkout -H 'content-type: application/json' -d '{"plant":"Monstera","amount":3900}'
curl -s localhost:4242/events     # the verified payment_intent.created event
```

Drop `--offline` and configure an LLM profile for answers that copy the amount and
description into the PaymentIntent, as Stripe does.
