# Shopify Admin GraphQL

A mock of the Shopify Admin GraphQL API (version 2026-10) for "Juniper Plants", a
Portland houseplant store at `juniper-plants.myshopify.com`: shop, products and
variants, inventory items and levels, locations, orders and customers, plus the
`productCreate`, `customerCreate` and `inventoryAdjustQuantities` mutations with
Shopify's `userErrors`, and a webhook after each product or customer create.

| | |
| --- | --- |
| Contract | [`shopify-admin.graphql`](shopify-admin.graphql), a hand-written SDL subset (9 queries, 3 mutations) with Shopify's type, field and argument names |
| Upstream | none: Shopify publishes no openly licensed schema file (see `notes` in [`example.json`](example.json)) |
| Hosts | `juniper-plants.myshopify.com` (path `/admin/api/2026-10/graphql.json`) |
| Scenarios | `invalid-product` (`userErrors` on `productCreate`), `slow` (3 s), `outage` (503 field error) |
| Webhooks | after `productCreate` and `customerCreate`, to `http://127.0.0.1:3000/webhooks`, signed `hmac-sha256` with `env:SHOPIFY_WEBHOOK_SECRET` |

## Quickstart

```bash
mockingbird examples init shopify-admin-graphql --dir shopify-admin-graphql && cd shopify-admin-graphql   # or: cd examples/apis/shopify-admin-graphql
mockingbird serve -c mockingbird.yaml --offline
```

In another shell:

```bash
curl -s localhost:8080/admin/api/2026-10/graphql.json -H 'Host: juniper-plants.myshopify.com' \
  -H 'X-Shopify-Access-Token: shpat_test123' -H 'content-type: application/json' \
  -d '{"query":"{ products(first: 3) { nodes { id title handle status totalInventory } pageInfo { hasNextPage endCursor } } }"}'
```

The config sets `protocols.graphql.path: /graphql.json` and mounts the mock at
`/admin/api/2026-10`, which gives Shopify's endpoint path.

Shopify's official [`@shopify/admin-api-client`](https://www.npmjs.com/package/@shopify/admin-api-client),
unmodified, through the proxy (Node ≥ 22.21 or ≥ 24.5, for `NODE_USE_ENV_PROXY`):

```bash
eval "$(mockingbird proxy env)"
npm install @shopify/admin-api-client
cat > shop.mjs <<'JS'
import { createAdminApiClient } from "@shopify/admin-api-client";
const client = createAdminApiClient({
  storeDomain: "juniper-plants.myshopify.com",
  apiVersion: "2026-10",
  accessToken: "shpat_test123",
});
const { data, errors } = await client.request(`query {
  shop { name currencyCode }
  products(first: 3) { nodes { id title status totalInventory } }
}`);
if (errors) throw new Error(JSON.stringify(errors));
console.log(data.shop.name, data.shop.currencyCode, data.products.nodes.length, "products");
JS
node shop.mjs
```

Plain `fetch` with the `X-Shopify-Access-Token` header works the same way.

Drop `--offline` (and configure an LLM, see [the configuration reference](../../../docs/configuration.md#llm))
for answers that follow the persona: plants and pots with `gid://shopify/Product/...`
ids, USD prices as decimal strings, orders named `#1001` upward, mutations that copy
their input and return empty `userErrors`.

## Expected output

`--offline` answers come from the schema faker: the selection's shape is right, but
strings and custom scalars (`DateTime`, `Decimal`, `URL`) are placeholder words and
connections hold one item:

```json
{
  "data": {
    "products": {
      "nodes": [{ "id": "fjord", "title": "meadow", "handle": "raven", "status": "ACTIVE", "totalInventory": 81 }],
      "pageInfo": { "hasNextPage": false, "endCursor": null }
    }
  },
  "extensions": { "mockingbird": { "sources": { "products": "fallback" }, ... } }
}
```

The Node snippet prints a line like `fjord GBP 1 products`.

## Scenarios

```bash
curl -s localhost:8080/admin/api/2026-10/graphql.json -H 'Host: juniper-plants.myshopify.com' \
  -H 'content-type: application/json' -H 'X-Mockingbird-Scenario: invalid-product' \
  -d '{"query":"mutation { productCreate(product: {title: \"\"}) { product { id } userErrors { field message } } }"}'
# {"data":{"productCreate":{"product":null,"userErrors":[{"field":["title"],"message":"Title can't be blank"}]}},...}
```

`slow` adds 3 s per root field; `outage` answers HTTP 200 with
`{"data":null,"errors":[{"message":"Service Unavailable (forced status 503)","extensions":{"code":"INTERNAL_SERVER_ERROR","status":503}}]}`.

Shopify's throttling answer (HTTP 200, `errors[].extensions.code: "THROTTLED"` and a
top-level `extensions.cost.throttleStatus`) cannot be produced: a GraphQL scenario
may only force a 5xx, and `set` reaches the root field's data, not the top-level
`errors` or `extensions`. A list inside `set` needs each object's `__typename`
(see `invalid-product`), or the patch is ignored.

## Webhooks

Run your webhook handler on port 3000 (or change `url`) with the same secret the mock signs with:

```bash
export SHOPIFY_WEBHOOK_SECRET=test_local_secret   # any value; the mock and your app share it
mockingbird serve -c mockingbird.yaml --offline
```

Each successful `productCreate` or `customerCreate` sends an event;
`mockingbird webhooks deliveries shopify-admin` lists the attempts. It differs from
Shopify's in three ways, so a handler needs adapting:

- **Signature:** `X-Mockingbird-Signature: sha256=<hex HMAC-SHA256(secret, body)>`.
  Shopify sends the same HMAC base64-encoded in `X-Shopify-Hmac-Sha256`.
- **Headers:** no `X-Shopify-Topic` (`products/create`), `X-Shopify-Shop-Domain` or
  `X-Shopify-Webhook-Id`; `X-Mockingbird-Event-Type` carries `productcreatepayload.updated`
  or `customercreatepayload.updated`.
- **Body:** Mockingbird's event envelope
  `{"id":"evt_...","object":"event","type":"productcreatepayload.updated","data":{"object":{<the mutation payload>}}}`
  rather than Shopify's REST-shaped product or customer.
