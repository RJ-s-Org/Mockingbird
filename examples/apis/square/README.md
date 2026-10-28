# Square

A mock of the Square API in the sandbox for "Little Fox Bakery", a two-location bakery
in Oakland: payments (create, get, list, complete), refunds, customers, locations and
the catalog under `/v2`, Square's `{"errors": [...]}` envelope, and `payment.created` /
`customer.created` webhooks in Square's event shape.

| | |
| --- | --- |
| Contract | [`square.openapi.yaml`](square.openapi.yaml), a hand-written subset that keeps Square's operation ids (`CreatePayment`, `ListCustomers`, ...) |
| Upstream | [square/connect-api-specification](https://github.com/square/connect-api-specification) `api.json` (Apache-2.0), pinned in [`example.json`](example.json) for reference; `--full` is off because the file has dangling `$ref`s Mockingbird cannot compile |
| Hosts | `connect.squareupsandbox.com`, `connect.squareup.com` |
| Scenarios | `card-declined` (402), `generic-decline` (400), `rate-limited` (429), `outage` (500) |
| Webhooks | `payment.created`, `customer.created` to `http://127.0.0.1:8000/square/webhook`, signed `hmac-sha256` with `env:SQUARE_WEBHOOK_SECRET` |

## Quickstart

```bash
mockingbird examples init square --dir square && cd square   # or: cd examples/apis/square
mockingbird serve -c mockingbird.yaml --offline
```

In another shell:

```bash
curl -s localhost:8080/v2/payments -H 'Host: connect.squareupsandbox.com' \
  -H 'Authorization: Bearer EAAAEtest' -H 'Square-Version: 2026-09-16' -H 'content-type: application/json' \
  -d '{"source_id":"cnon:card-nonce-ok","idempotency_key":"7b0f3ec5-086a-4871-8f13-3c81b3875218","amount_money":{"amount":450,"currency":"USD"}}'
```

The official Python SDK, unmodified, through the proxy (it uses `httpx`, which reads
`HTTPS_PROXY` and `SSL_CERT_FILE`):

```bash
eval "$(mockingbird proxy env)"
pip install squareup
python - <<'PY'
import uuid
from square import Square
from square.environment import SquareEnvironment

client = Square(token="EAAAEtest_sandbox_token", environment=SquareEnvironment.SANDBOX)
location = client.locations.list().locations[0]
payment = client.payments.create(
    source_id="cnon:card-nonce-ok", idempotency_key=str(uuid.uuid4()),
    amount_money={"amount": 450, "currency": "USD"}, location_id=location.id).payment
print(location.id, payment.id, payment.status, payment.total_money.amount)
for customer in client.customers.list(limit=5):
    print(customer.id, customer.given_name, customer.email_address)
    break
PY
```

List answers carry a `cursor` that is sometimes `null` offline, so the SDK's pagers end
after a few pages instead of walking an endless generated list.

Drop `--offline` (and configure an LLM, see [the configuration reference](../../../docs/configuration.md#llm))
for answers that follow the persona: the Temescal and Farmers Market locations, the
amount you charged, Morning Buns and Sourdough Loaves in the catalog.

## Expected output

`--offline` answers come from the schema faker, so values differ run to run; the shape
does not:

```json
{
  "payment": {
    "id": "Q3LZeEH0shKnDkih0EBRKbRM6FW6j",
    "status": "COMPLETED",
    "source_type": "CARD",
    "amount_money": {...},
    "card_details": {...},
    "location_id": "L2KJTYIODLKY5",
    "order_id": "PoH1OLnTEHNr2wq5r4YcOLfyIfKYk",
    "receipt_number": "evhp",
    "created_at": "2029-02-02T07:03:53Z",
    ...
  }
}
```

The Python snippet prints lines like:

```
LPQZNR71J77AH 4lrem2VKJ63JgxmSl4oRYwQq0yKcm CANCELED 77
DE4OU20KJCI7XCRDDUCKXCHNCL bravo fjord@example.com
```

## Scenarios

```bash
curl -s localhost:8080/v2/payments -H 'Host: connect.squareupsandbox.com' \
  -H 'Authorization: Bearer EAAAEtest' -H 'content-type: application/json' \
  -H 'X-Mockingbird-Scenario: card-declined' \
  -d '{"source_id":"cnon:card-nonce-declined","idempotency_key":"k1","amount_money":{"amount":450,"currency":"USD"}}'
# HTTP 402 {"errors":[{"category":"PAYMENT_METHOD_ERROR","code":"CARD_DECLINED","detail":"Card declined."}]}
```

The SDK raises `square.core.api_error.ApiError` with `status_code` 402 and
`errors[0].code == "CARD_DECLINED"`. `generic-decline` answers 400 `GENERIC_DECLINE`,
`rate-limited` 429 `RATE_LIMIT_ERROR` / `RATE_LIMITED` on every operation. Set
`default_scenario: card-declined` on the mock to apply it to every call. (The real
sandbox also declines on the test nonce `cnon:card-nonce-declined`; the mock does not
read the nonce.)

## Webhooks

Run your webhook handler on port 8000 (or change `url`):

```bash
export SQUARE_WEBHOOK_SECRET=whsec_test_local   # any value; the mock and your app share it
mockingbird serve -c mockingbird.yaml --offline
```

Each successful `POST /v2/payments` sends `payment.created` and each `POST /v2/customers`
`customer.created`, as `{"merchant_id", "type", "event_id", "created_at", "data": {"type",
"id", "object": {"payment": {...}}}}`. `mockingbird webhooks deliveries square` lists
the attempts.

Square signs real webhooks with `x-square-hmacsha256-signature`, the base64 HMAC-SHA256
of the notification URL followed by the body, checked by the SDK's
`square.utils.webhooks_helper.verify_signature`. The mock cannot produce that: it signs with
`X-Mockingbird-Signature: sha256=<hex HMAC-SHA256(secret, body)>` (see
[the webhooks guide](../../../docs/webhooks.md#signatures)), so skip Square verification in
tests or verify that header instead.
