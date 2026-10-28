# PayPal

A mock of the PayPal REST API in the sandbox for "Fernwood Coffee Roasters": OAuth 2.0
client-credentials tokens, Orders v2 (create, get, patch, confirm, authorize, capture)
and Payments v2 (captures, refunds, authorizations), with order and capture webhooks.
Three mocks share the host, split by path the way PayPal splits its specs.

| | |
| --- | --- |
| Contracts | [`paypal-orders.openapi.yaml`](paypal-orders.openapi.yaml) (mounted at `/`), [`paypal-payments.openapi.yaml`](paypal-payments.openapi.yaml) (at `/v2/payments`), [`paypal-oauth.openapi.yaml`](paypal-oauth.openapi.yaml) (at `/v1/oauth2`); hand-written subsets that keep PayPal's operation ids (`orders.create`, `captures.refund`, ...) |
| Upstream | [paypal/paypal-rest-api-specifications](https://github.com/paypal/paypal-rest-api-specifications) `openapi/checkout_orders_v2.json` (Apache-2.0), pinned in [`example.json`](example.json); `mockingbird examples init paypal --full` downloads it for the orders mock |
| Hosts | `api-m.sandbox.paypal.com`, `api-m.paypal.com` |
| Scenarios | `instrument-declined` (422), `rate-limited` (429), `outage` (500) |
| Webhooks | `CHECKOUT.ORDER.APPROVED` (after confirm-payment-source), `PAYMENT.CAPTURE.COMPLETED` (after capture), `PAYMENT.CAPTURE.REFUNDED` (after refund) to `http://127.0.0.1:8000/paypal/webhook`, signed `hmac-sha256` with `env:PAYPAL_WEBHOOK_SECRET` |

## Quickstart

```bash
mockingbird examples init paypal --dir paypal && cd paypal   # or: cd examples/apis/paypal
mockingbird serve -c mockingbird.yaml --offline
```

In another shell:

```bash
curl -s localhost:8080/v1/oauth2/token -H 'Host: api-m.sandbox.paypal.com' \
  -u client_id_test:client_secret_test -d grant_type=client_credentials
curl -s localhost:8080/v2/checkout/orders -H 'Host: api-m.sandbox.paypal.com' -H 'Authorization: Bearer A21AAtest' \
  -H 'content-type: application/json' \
  -d '{"intent":"CAPTURE","purchase_units":[{"amount":{"currency_code":"USD","value":"42.00"}}]}'
```

The official Python SDK, unmodified, through the proxy (it uses `requests`, which
reads the proxy variables and `REQUESTS_CA_BUNDLE`):

```bash
eval "$(mockingbird proxy env)"
pip install paypal-server-sdk
python - <<'PY'
from paypalserversdk.paypal_serversdk_client import PaypalServersdkClient
from paypalserversdk.http.auth.o_auth_2 import ClientCredentialsAuthCredentials
from paypalserversdk.configuration import Environment
from paypalserversdk.models.order_request import OrderRequest
from paypalserversdk.models.checkout_payment_intent import CheckoutPaymentIntent
from paypalserversdk.models.purchase_unit_request import PurchaseUnitRequest
from paypalserversdk.models.amount_with_breakdown import AmountWithBreakdown

client = PaypalServersdkClient(
    environment=Environment.SANDBOX,  # https://api-m.sandbox.paypal.com
    client_credentials_auth_credentials=ClientCredentialsAuthCredentials(
        o_auth_client_id="client_id_test", o_auth_client_secret="client_secret_test"))

order = client.orders.create_order({"body": OrderRequest(
    intent=CheckoutPaymentIntent.CAPTURE,
    purchase_units=[PurchaseUnitRequest(amount=AmountWithBreakdown(currency_code="USD", value="18.00"))])}).body
print(order.id, order.status)
captured = client.orders.capture_order({"id": order.id, "prefer": "return=representation"}).body
capture = captured.purchase_units[0].payments.captures[0]
print(captured.status, capture.id, capture.status)
PY
```

The SDK fetches its token from the mocked `/v1/oauth2/token` first. Drop `--offline`
(and configure an LLM, see [the configuration reference](../../../docs/configuration.md#llm))
for answers that follow the persona: a `CREATED` order with an `approve` link, a
`COMPLETED` capture for the amount you sent, Fernwood as the payee.

## Expected output

`--offline` answers come from the schema faker, so values differ run to run (statuses
included); the shape does not. `POST /v2/checkout/orders` answers 201:

```json
{
  "create_time": "2005-02-09T20:10:32Z",
  "id": "3ZWSPJ2IZ3NFR71NZ",
  "intent": "CAPTURE",
  "links": [{"href": "https://example.com/nectar", "method": "OPTIONS", "rel": "refund"}],
  "payer": {"email_address": "fjord@example.com", "payer_id": "579JVT4KXQBHZ", ...},
  "purchase_units": [{"amount": {"currency_code": "USD", "value": "75.19"}, ...}],
  ...
}
```

The Python snippet prints lines like:

```
7825HCETFZY8FIT1Q VOIDED
COMPLETED 8IVRJQEMXIMVL8AOZ PENDING
```

## Scenarios

```bash
curl -s localhost:8080/v2/checkout/orders/5O190127TN364715T/capture -H 'Host: api-m.sandbox.paypal.com' \
  -H 'Authorization: Bearer A21AAtest' -H 'content-type: application/json' \
  -H 'X-Mockingbird-Scenario: instrument-declined' -d '{}'
# HTTP 422 {"name":"UNPROCESSABLE_ENTITY","details":[{"issue":"INSTRUMENT_DECLINED","description":"The instrument presented  was either declined ..."}],"links":[{"rel":"information_link",...}],"debug_id":"bbc9f67e397d4",...}
```

The SDK raises `paypalserversdk.exceptions.error_exception.ErrorException` with
`response_code` 422 and `details[0].issue == "INSTRUMENT_DECLINED"`. On a refund the
same scenario answers `REFUND_FAILED_INSUFFICIENT_FUNDS`; `rate-limited` answers 429
`RATE_LIMIT_REACHED` on every operation, the token endpoint included. Each mock has its
own scenarios, so `default_scenario` goes on the mock (or mocks) you want it on.

## Webhooks

Run your webhook handler on port 8000 (or change `url`):

```bash
export PAYPAL_WEBHOOK_SECRET=whsec_test_local   # any value; the mock and your app share it
mockingbird serve -c mockingbird.yaml --offline
```

A successful confirm-payment-source sends a `CHECKOUT.ORDER.APPROVED` event, a capture
`PAYMENT.CAPTURE.COMPLETED` and a refund `PAYMENT.CAPTURE.REFUNDED`, each in PayPal's
envelope (`id: "WH-..."`, `event_version`, `resource_type`, `summary`, `resource`).
`mockingbird webhooks deliveries paypal-orders` (or `paypal-payments`) lists the attempts.

PayPal signs real webhooks with `PAYPAL-TRANSMISSION-SIG`, a certificate signature over
the transmission id, time, webhook id and the CRC32 of the body, checked with
`/v1/notifications/verify-webhook-signature` or the certificate at `PAYPAL-CERT-URL`.
The mock cannot produce that: it signs with `X-Mockingbird-Signature:
sha256=<hex HMAC-SHA256(secret, body)>` (see [the webhooks guide](../../../docs/webhooks.md#signatures)),
so skip PayPal verification in tests or verify that header instead.
With `--full`, the upstream Orders spec has no `webhooks` section and events take the
generic Stripe-like shape.
