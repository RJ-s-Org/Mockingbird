# Trial: Next.js Commerce (Next.js, Shopify Storefront GraphQL)

[vercel/commerce](https://github.com/vercel/commerce) at `f7521d5`: Vercel's storefront template (Next.js 15 canary with partial prerendering, React 19, about 13k stars). Every page reads Shopify's Storefront API; the cart is a cookie holding a Shopify cart id.

| | |
| --- | --- |
| API mocked | Shopify Storefront GraphQL, `https://<shop>.myshopify.com/api/2023-01/graphql.json` |
| Contract | Storefront API 2025-07: `storefront.schema.json` (introspection JSON) from `@shopify/hydrogen-react@2025.7.0`, printed as SDL with graphql-js (280 KB, 72 root operations) |
| Model | OpenRouter `deepseek/deepseek-v4.1-flash`, default routing |
| Source edits | None (`git diff` empty) |
| Result | `next build` and every catalog page work (0 fallbacks); **the cart does not hold together** |

## Config and commands

```yaml
server: {port: 44080, admin_port: 44090, grpc_port: 44051}
proxy:
  enabled: true
  port: 44888
  images: [cdn.shopify.com/s/files]       # product images: generated placeholder PNGs
protocols:
  graphql:
    path: /graphql.json                    # Shopify's endpoint name
llm:
  default_profile: openrouter
  profiles:
    openrouter:
      api_key: env:OPENROUTER_API_KEY
      base_url: https://openrouter.ai/api/v1
      model: deepseek/deepseek-v4.1-flash
mocks:
  shopify-storefront:
    contract: storefront-2025-07.graphql
    route_prefix: /api/2023-01
    hosts: [fernwood-goods.myshopify.com]
    persona: >-
      Shopify Storefront API for "Fernwood Goods", a small Portland shop selling
      outdoor apparel and camp gear in USD: t-shirts, hoodies, beanies, enamel
      mugs, canvas totes, stickers. Collections have handles like "apparel",
      "camp-gear", "hidden-homepage-featured-items" and
      "hidden-homepage-carousel". Products have 1-3 variants (sizes or colours),
      prices 8.00-89.00 as decimal strings with currencyCode USD, handles in
      kebab-case, and images at
      https://cdn.shopify.com/s/files/1/0754/3727/7491/files/<handle>-<n>.png
      with width 1000 and height 1000. Global ids look like
      gid://shopify/Product/8123456789012 (13 digits), gid://shopify/ProductVariant/...,
      gid://shopify/Collection/..., gid://shopify/Cart/<26 chars>?key=<32 hex>.
      Menus "next-js-frontend-header-menu" and "next-js-frontend-footer-menu"
      link to /search/<collection-handle> and /<page-handle>. updatedAt values
      are ISO 8601 in October 2026. A cart's lines, quantities and totals add up.
```

```bash
export SHOPIFY_STORE_DOMAIN=fernwood-goods.myshopify.com SHOPIFY_STOREFRONT_ACCESS_TOKEN=trial-storefront-token \
       SHOPIFY_REVALIDATION_SECRET=trial COMPANY_NAME="Fernwood Goods" SITE_NAME="Fernwood Goods"
mockingbird run -c mockingbird/mockingbird.yaml --keep -- ./node_modules/.bin/next build
mockingbird run -c mockingbird/mockingbird.yaml -- ./node_modules/.bin/next start -p 3010
```

`pnpm build` refused to run under pnpm 11 (`ERR_PNPM_IGNORED_BUILDS`: sharp's install script is not approved) and wrote a `pnpm-workspace.yaml`; the trial called `next` directly and deleted that file.

## What happened

**The build runs against the mock.** `next build` prerenders the home page and menus at build time, so a plain `mockingbird run -- next build` is a working CI recipe for a storefront: 14 s, 5 Storefront calls (`menu` ×2, `collection` ×2, `collections`), all `llm`.

**Pages.** Served by `next start` and fetched with curl, then a product page and the cart in the browser pane:

| Page | Status | Time | Content |
| --- | --- | --- | --- |
| `/` | 200 | 0.1 s | prerendered at build time |
| `/search` | 200 | 6.8 s | product grid from `products` |
| `/search/apparel` | 200 | 6.6 s | collection products |
| `/product/fern-tee` | 200 | 8.0 s (cached: 0.01 s) | "Fern Tee", $32.00, sizes S/M/L (L out of stock), recommendations |
| `/product/ridgeline-hoodie` | 200 | 3.8 s | "Ridgeline Hoodie", $89.00 |

The listing and the detail page agree: `fern-tee` is `gid://shopify/Product/8123456789012` at 32.00 in both the collection answer and `product(handle:)`. Product images render as solid-colour placeholders (`proxy.images`), fetched by Next's image optimizer on the server, so the browser needed no proxy.

**The cart is invented.** Selecting size M and clicking "Add to cart":

1. `cartCreate(input: {lines: null})` returned an empty cart with id `gid://shopify/Cart/c1-8f3a2b6d9e014c77b52a10deadbeef` (the persona's id format was ignored).
2. `cart(id:)` for that id (the cart drawer) was generated anew: three lines, a Fernwood Trail Hoodie M/Forest ×2, a Basecamp Enamel Mug and a Ridge Line Canvas Tote, 13.9 s.
3. `cartLinesAdd(cartId, lines: [{merchandiseId: <Fern Tee M>, quantity: 1}])` answered a cart holding a "Fernwood Fleece Hoodie", not the variant sent.

The drawer showed the invented lines; the Fern Tee never appeared. This is the open "GraphQL mutation results are not stored" item, now with a real app: the cart needs `cartCreate` → `cart(id)` → `cartLinesAdd` to read and update one stored object.

| Calls | Sources | Fallbacks | Latency (generated) | Tokens (prompt / completion) |
| --- | --- | --- | --- | --- |
| 37 | 26 `llm`, 2 `repaired`, 9 `cache` | 0 | 0.8–13.9 s, median 2.1 s | 147k / 29k |

`verify --no-fallbacks --no-unmapped` passes, although the cart is wrong. The bar measures plumbing, not whether state holds.

## Notes

- Shopify publishes the Storefront schema as introspection JSON, not SDL; Mockingbird only takes SDL, so it had to be converted. Open: accept introspection JSON as a GraphQL contract.
- `protocols.graphql.path` is global. Shopify's endpoint is `graphql.json`, so a config with this mock and another GraphQL API at `/graphql` cannot serve both. Open: a per-mock GraphQL path.
- The app asks for API version 2023-01 and the contract is 2025-07; nothing the template queries changed between the two, so no query failed validation.
- Prompts are large: the Storefront types the selections touch (`Product`, `Collection`, `Cart`) put 4–12k tokens in each prompt, and the cart queries about 11.6k.
