# Shopify Storefront GraphQL

A mock of the Shopify Storefront GraphQL API (version 2026-10) for "Juniper Plants",
a Portland houseplant store at `juniper-plants.myshopify.com`: products and variants,
collections and their products, menus, pages, product recommendations, `node`, and a
cart that holds together through `cartCreate`, `cartLinesAdd`, `cartLinesUpdate`,
`cartLinesRemove` and `cart(id:)`. It covers what a headless storefront such as
[vercel/commerce](https://github.com/vercel/commerce) asks for. Hydrogen's queries add
`@inContext(country:, language:)`, which the contract declares (with a few country and
language codes) so they validate; the answers do not change with it. Operations outside
this subset are covered by importing Shopify's full schema instead: the introspection
JSON `storefront.schema.json` in `@shopify/hydrogen-react` imports as a GraphQL contract.

| | |
| --- | --- |
| Contract | [`shopify-storefront.graphql`](shopify-storefront.graphql), a hand-written SDL subset (12 queries, 4 mutations) with Shopify's type, field and argument names |
| World | [`shopify-storefront.world.yaml`](shopify-storefront.world.yaml): the cart's lines, quantities and totals (see [The cart](#the-cart)) |
| Upstream | none: Shopify publishes no openly licensed schema file (see `notes` in [`example.json`](example.json)) |
| Hosts | `juniper-plants.myshopify.com` (path `/api/2026-10/graphql.json`) |
| Scenarios | `invalid-merchandise` (`userErrors` on `cartLinesAdd`), `invalid-merchandise-on-create` (the same on `cartCreate`), `slow` (3 s), `outage` (503 field error) |
| Webhooks | none: the Storefront API sends none |

## Quickstart

```bash
mockingbird examples init shopify-storefront-graphql --dir shopify-storefront && cd shopify-storefront   # or: cd examples/apis/shopify-storefront-graphql
mockingbird serve -c mockingbird.yaml --offline
```

In another shell:

```bash
curl -s localhost:8080/api/2026-10/graphql.json -H 'Host: juniper-plants.myshopify.com' \
  -H 'X-Shopify-Storefront-Access-Token: test-storefront-token' -H 'content-type: application/json' \
  -d '{"query":"{ products(first: 3) { edges { node { id handle title priceRange { minVariantPrice { amount currencyCode } } } } } }"}'
```

The config sets `graphql_path: /graphql.json` and mounts the mock at `/api/2026-10`,
which gives Shopify's endpoint path. An app pinned to another version (vercel/commerce
asks for `/api/2023-01/graphql.json`) needs `route_prefix` set to that version's path.

An app, unmodified, through the proxy (Node ≥ 22.21 or ≥ 24.5, for `NODE_USE_ENV_PROXY`):

```bash
eval "$(mockingbird proxy env)"
cat > cart.mjs <<'JS'
const shop = async (query, variables = {}) => {
  const res = await fetch("https://juniper-plants.myshopify.com/api/2026-10/graphql.json", {
    method: "POST",
    headers: { "content-type": "application/json", "X-Shopify-Storefront-Access-Token": "test-storefront-token" },
    body: JSON.stringify({ query, variables }),
  });
  const { data, errors } = await res.json();
  if (errors) throw new Error(JSON.stringify(errors));
  return data;
};
const cart = "id totalQuantity cost { totalAmount { amount currencyCode } } lines(first: 10) { nodes { id quantity } }";
const { cartCreate } = await shop(`mutation { cartCreate { cart { ${cart} } userErrors { message } } }`);
const { product } = await shop(`{ product(handle: "fiddle-leaf-fig") { title variants(first: 1) { nodes { id } } } }`);
const variant = product.variants.nodes[0].id;
const add = `mutation($c: ID!, $v: ID!) { cartLinesAdd(cartId: $c, lines: [{merchandiseId: $v, quantity: 2}]) { cart { ${cart} } } }`;
await shop(add, { c: cartCreate.cart.id, v: variant });
const { cartLinesAdd } = await shop(add, { c: cartCreate.cart.id, v: variant });
console.log(cartLinesAdd.cart.lines.nodes.length, "line,", cartLinesAdd.cart.totalQuantity, "items");
JS
node cart.mjs
```

It prints `1 line, 4 items`: the second add of the same variant raised the line's
quantity. Shopify's own [`@shopify/storefront-api-client`](https://www.npmjs.com/package/@shopify/storefront-api-client)
(`createStorefrontApiClient({storeDomain, apiVersion: "2026-10", publicAccessToken})`)
sends the same request through the proxy; that snippet has not been run against the
example.

Drop `--offline` (and configure an LLM, see [the configuration reference](../../../docs/configuration.md#llm))
for answers that follow the persona: plants, pots and plant care with
`gid://shopify/Product/...` ids, 1-3 variants each, USD prices as decimal strings,
collections such as `indoor-plants`, and menus linking to `/collections/<handle>`.

## Expected output

`--offline` answers come from the schema faker: the selection's shape is right, but
strings and custom scalars (`Decimal`, `URL`, `HTML`) are placeholder words and
connections hold one item:

```json
{
  "data": {
    "products": {
      "edges": [{ "node": { "id": "garnet", "handle": "sable", "title": "pebble", "priceRange": { "minVariantPrice": { "amount": "raven", "currencyCode": "CAD" } } } }]
    }
  },
  "extensions": {
    "mockingbird": {
      "fallbackReasons": { "products": "no LLM is configured for this mock (offline)" },
      "sources": { "products": "fallback" }
    }
  }
}
```

The cart is answered from state either way: `cartCreate` answers a cart with an id like
`gid://shopify/Cart/<token>?key=<key>`, a `checkoutUrl` on the store's domain and
`totalQuantity: 0`. Offline, a variant the session generated has price `0.0` (the
faker's `Decimal` is a word), so totals stay `0.0` while quantities add up; with an LLM,
or for a variant seeded into the session, prices are the variants'.

## The cart

The world description makes the cart one stored object:

- `cartCreate` stores a cart (with its `note`, `attributes` and any `lines`); `cartLinesAdd`,
  `cartLinesUpdate` and `cartLinesRemove` change the cart their `cartId` names, and
  `cart(id:)` reads it back, all answered from state with no model call.
- `cartLinesAdd` of a variant the cart already holds, with the same attributes, raises
  that line's quantity, as Shopify merges lines; anything else becomes a new line
  (`quantity` defaults to 1).
- `cartLinesUpdate` sets the quantity (and attributes) of each line named by `id`;
  quantity 0 removes the line. `cartLinesRemove` removes the lines named in `lineIds`.
- A line copies its variant (title, selected options, price) from the variant the
  session holds: the one a `product`, `collection`, `products` or `node` read showed.
  A variant the session never read gets the title `Default Title` and price `0.0`.
- Each line's `cost`, and the cart's `totalQuantity` and `cost` (`subtotalAmount`,
  `totalAmount`, `checkoutChargeAmount`), are computed from the lines on every answer,
  in USD, in whole cents. There is no tax, shipping or discount before checkout, and
  the deprecated `totalTaxAmount` is `null`.
- A line's `merchandise.product` is the product the session holds (handle, title,
  price range, featured image, SEO, ...); its `options`, `variants` and `images` are
  empty there, so read the product itself for them.

## Scenarios

```bash
curl -s localhost:8080/api/2026-10/graphql.json -H 'Host: juniper-plants.myshopify.com' \
  -H 'content-type: application/json' -H 'X-Mockingbird-Scenario: invalid-merchandise' \
  -d '{"query":"mutation { cartLinesAdd(cartId: \"gid://shopify/Cart/x\", lines: [{merchandiseId: \"gid://shopify/ProductVariant/1\"}]) { cart { id } userErrors { code field message } } }"}'
# {"data":{"cartLinesAdd":{"cart":null,"userErrors":[{"code":"INVALID","field":["lines","0","merchandiseId"],"message":"The merchandise does not exist."}]}},...}
```

`invalid-merchandise-on-create` answers `cartCreate` the same way, with
`field: ["input", "lines", "0", "merchandiseId"]`.

`slow` adds 3 s per root field; `outage` answers HTTP 200 with
`{"data":null,"errors":[{"message":"Service Unavailable (forced status 503)","extensions":{"code":"INTERNAL_SERVER_ERROR","status":503}}]}`.

Shopify's throttling answer (HTTP 430 or `errors[].extensions.code: "THROTTLED"`)
cannot be produced: a GraphQL scenario may only force a 5xx. A list inside `set`
needs each object's `__typename` (see `invalid-merchandise`), or the patch is ignored.
