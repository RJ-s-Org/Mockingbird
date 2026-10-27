# Webhooks

A mock can POST an event to your app after each successful write, the way Stripe, GitHub or Shopify notify a server. Give the mock one or more targets:

```yaml
mocks:
  stripe:
    contract: stripe.yaml
    hosts: [api.stripe.com]
    webhooks:
      - url: http://localhost:4242/webhook
        events: ["*"]                         # or operation ids / event types
        secret: env:STRIPE_WEBHOOK_SECRET     # a reference, never the value
        signing: stripe
```

Targets can also be set through the Admin API (`webhooks` on `POST`/`PATCH …/mocks/{mock}`). There the secret reference must match `server.api_webhook_secret_refs` (by default `env:*_WEBHOOK_SECRET` and `env:MB_WEBHOOK_*`). Every key is listed in the [configuration reference](configuration.md#mocksnamewebhooks).

## When an event is sent

After a call to a command (create, update, delete or another non-read operation) answers 2xx. Reads, list seeding, validation errors (4xx), forced statuses outside 2xx and injected chaos errors send nothing. The response is never held up: events are built and delivered in the background.

A target receives an event when its `events` list contains `*`, the operation id or the event type.

## The event

**Event type.** `<resource>.<verb>`. The verb is `created` for creates, `deleted` for deletes and `updated` for everything else (Stripe's `POST /v1/customers/{id}` is an update). The resource is the path segment before a trailing `{param}` (`/v1/customers/{customer}` → `customer`), else the operation's resource type, singularised: `POST /v1/payment_intents` → `payment_intent.created`.

**Body.** When the contract describes the payload, the mock generates it from that schema (with the LLM, or the faker offline):

1. a `callbacks` entry of the operation (OpenAPI 3.0 and 3.1), else
2. a top-level `webhooks` entry (OpenAPI 3.1) named like the event type or the operation id (case-insensitive).

Callback URL expressions (`{$request.body#/callbackUrl}`) are ignored; events go to the configured targets only.

Otherwise the body is a Stripe-shaped event:

```json
{
  "id": "evt_3kQ9yZ0aB1cD2eF3gH4iJ5kL",
  "object": "event",
  "type": "customer.created",
  "created": 1700000000,
  "data": {"object": {"id": "cus_123", "email": "jo@example.com"}}
}
```

`data.object` is the response body. A delete answered without a body carries `{"id": "<path id>", "deleted": true}`.

**Headers.** `content-type: application/json`, `user-agent: Mockingbird-Webhooks/<version>`, `x-mockingbird-event-id`, `x-mockingbird-event-type` and the signature header.

## Signatures

| `signing` | Header |
| --- | --- |
| `stripe` | `Stripe-Signature: t=<unix seconds>,v1=<hex HMAC-SHA256(secret, "<t>.<raw body>")>` |
| `hmac-sha256` (default with a secret) | `X-Mockingbird-Signature: sha256=<hex HMAC-SHA256(secret, raw body)>` |
| `none` (default without a secret) | none |

Each attempt is signed again, so `t` is the attempt's time. A Stripe SDK verifies the event as it would a real one:

```js
const event = stripe.webhooks.constructEvent(req.rawBody, req.headers["stripe-signature"], process.env.STRIPE_WEBHOOK_SECRET);
```

For `hmac-sha256`, recompute the HMAC over the raw body and compare in constant time.

To produce a header for a test without a running mock:

```sh
mockingbird webhooks sign --secret-ref env:STRIPE_WEBHOOK_SECRET --body event.json --signing stripe
```

It prints `{"header": "stripe-signature", "value": "t=…,v1=…"}`; `--timestamp` fixes `t`.

## Delivery and retries

Each (event, target) pair is one delivery: after `delay_ms`, up to 4 attempts (the first and 3 retries after 1 s, 2 s and 4 s), each with a 10 s timeout. A 2xx answer delivers it; anything else is retried. At most 256 deliveries per mock are in flight; beyond that new ones are recorded `dropped`.

A secret reference that does not resolve (an unset environment variable) does not stop the mock: its deliveries are recorded `failed` with an error naming the reference, and nothing is sent unsigned.

Metrics: `mb_webhook_delivered_total`, `mb_webhook_failed_total`, `mb_webhook_dropped_total`.

## Listing deliveries

The newest 200 deliveries per mock are kept in memory (not across restarts):

- Admin API: `GET /api/v1/workspaces/{ws}/mocks/{mock}/webhooks/deliveries?limit=50`
- CLI: `mockingbird webhooks deliveries <mock>`
- MCP: the `list_webhook_deliveries` tool

Each record has its `status` (`pending`, `delivered`, `failed`, `dropped`), `attempts`, the last `response_status` and `error`, and the target URL without userinfo or query.

On a split deployment (`server.role: control` and `data`), deliveries happen and are listed on the data nodes; a control node lists none.
