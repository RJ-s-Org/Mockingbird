# Node + Shopify + Stripe: storefront backend with verified webhooks

A dependency-light Node server for "Juniper Plants": `GET /products` queries the Shopify
Admin GraphQL API with the official
[`@shopify/admin-api-client`](https://www.npmjs.com/package/@shopify/admin-api-client),
and `POST /stripe/webhook` verifies Stripe webhooks with `stripe.webhooks.constructEvent`
and records orders. The code calls `https://juniper-plants.myshopify.com` and is unaware of
Mockingbird; the test runs it under the variables `mockingbird proxy env` prints, against
the [Shopify](../../apis/shopify-admin-graphql) and [Stripe](../../apis/stripe) examples.

| File | |
| --- | --- |
| [`server.js`](server.js) | `GET /products`, `POST /stripe/webhook`, `GET /orders` |
| [`mockingbird.yaml`](mockingbird.yaml) | Both mocks; Stripe webhooks to `http://127.0.0.1:3000/stripe/webhook` |
| [`storefront.test.js`](storefront.test.js) | Starts the mock and the app (`node --test`), lists products, creates a PaymentIntent and waits for the verified webhook |

## Run the tests

Node 22.21 or newer (for `NODE_USE_ENV_PROXY`):

```bash
cd examples/apps/node-shopify-stripe
npm ci
MOCKINGBIRD_BIN=$(command -v mockingbird) npm test
```

```text
✔ lists products from the Shopify Admin GraphQL API (709.522667ms)
✔ records an order from a verified Stripe webhook (218.794333ms)
ℹ pass 2
ℹ fail 0
```

The mock uses ports 50080/50090/50888 and the app 3000.

## Run it by hand

```bash
export STRIPE_WEBHOOK_SECRET=whsec_test_juniper
mockingbird serve -c mockingbird.yaml --offline      # shell 1
eval "$(mockingbird proxy env)" && npm start         # shell 2
curl -s localhost:3000/products
```

Only fetch-based clients follow `NODE_USE_ENV_PROXY`. SDKs that create their own
`https.Agent` (stripe-node's API client, for one) need
`node -r global-agent/bootstrap` with `GLOBAL_AGENT_HTTP_PROXY=$HTTPS_PROXY`; see
[the proxy guide](../../../docs/proxy.md#per-runtime). This app only verifies Stripe
webhooks, so it never calls Stripe.
