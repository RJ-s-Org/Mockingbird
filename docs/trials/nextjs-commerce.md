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

## After the engine fixes

Run 3, on the engine-fixes branch: GraphQL answers store their objects by type, mutations generate only their selection and are generated knowing the cart and the variant they name. Same build, config and model; a fresh browser cookie. Size M, colour "Heather Moss", "Add to cart", then the cart drawer:

| Step | Run 1 | Run 3 |
| --- | --- | --- |
| `cartCreate` | empty cart, `llm` 1.5 s | empty cart, `llm` 1.5 s (stored as `cart`) |
| `cart(id:)` | invented three lines, `llm` 13.9 s | the stored empty cart, `state`, 1 ms |
| `cartLinesAdd` | a "Fernwood Fleece Hoodie", not the variant sent | "Fern Tee", M / Heather Moss, quantity 1 (`llm`, 1.8 s); a second add: quantity 2 (4.6 s) |
| `cart(id:)` (drawer) | the invented lines | the stored cart, `state`, 2–32 ms |

The drawer shows one line, Fern Tee, M / Heather Moss, quantity 2. The variant was stored when the product page read it, and it carries its product (`ProductVariant.product`, filled from the product it was read inside), so the prompt for `cartLinesAdd` names "Fern Tee". Before that link was added, the same run's cart line had the right variant (M / Moss) under an invented product name ("Fernwood Trail Tee").

| Calls | Sources | Fallbacks | Latency (generated) | Tokens (prompt / completion) |
| --- | --- | --- | --- | --- |
| 29 | 24 `llm`, 1 `repaired`, 4 `state` | 0 | 0.8–15.4 s, median 1.8 s | 134k / 22.7k |

Still open: the line price ($28.00) is not the product page's ($32.00), and the cart id ignores the persona's `?key=` format. The first "Add to cart" after the cart is created waits about 10 s in the app before its server action reaches the mock; a second click goes through at once (both runs).

## Run 4: built by the agent through the skills (2026-11-10)

Binary from `main` (lane-2 build, `mockingbird 0.1.0`), model `deepseek/deepseek-v4.1-flash-1017` on OpenRouter (`api_key: env:OPENROUTER_API_KEY`). The mock was built only the way the skills say: `mockingbird init --provider openrouter --contract …`, `agent install claude-code`, `mocks.shopify.graphql_path: /graphql.json` (the per-mock path that replaced the global one), `hosts`, `proxy.images`, a persona, then `world show` and a `world:` overlay file for the gaps the app touches:

```yaml
operations:
  Mutation.cartLinesAdd:    { kind: action, type: cart, target: { member: cartId }, answer: { record: /cart } }
  Mutation.cartLinesUpdate: { kind: action, type: cart, target: { member: cartId }, answer: { record: /cart } }
  Mutation.cartLinesRemove: { kind: action, type: cart, target: { member: cartId }, answer: { record: /cart } }
  Query.menu:         { target: { member: handle } }
  Query.pageByHandle: { target: { member: handle } }
  Query.products:     { kind: list, type: product, answer: { records: /edges }, order: { member: title } }
```

Self-test (curl with `Host: fernwood-goods.myshopify.com`): `cartCreate` → `cartLinesAdd` (Fern Tee M ×2, $56) → `cart(id)` from `state` in 3 ms → `cartLinesUpdate` → `cartLinesRemove` → empty cart from `state`. Config and world file: `mockingbird-trials/commerce/mb4/`.

### What works

- `next build` through `mockingbird run`: 29 s, 6 Storefront calls (menus ×2, two homepage collections, `collections`, `products`), exit 0.
- Every page answers 200: `/` (prerendered), `/search`, `/search/apparel`, `/search/camp-gear`, `/product/fern-tee`, `/about`, `/shipping-returns`, `/search?q=mug` (one result, a mug), `/search?sort=price-asc` (in price order). Menus and pages read the same as the persona describes; `pageByHandle` and `menu` answer from `state` (0–1 ms) after the first call.
- Cart in the browser pane: size M, Add to cart → the drawer shows Fern Tee, M, $28.00, the variant and price the product page showed (`cartLinesAdd`, `llm`, 6.2 s). Remove → "Your cart is empty", and stays empty after a reload. Every `cart(id)` read is `state` (0–11 ms).
- 0 fallbacks.

### Failures

| # | Class | Failure | Repro: request → expected → got |
| --- | --- | --- | --- |
| 1 | Engine bug | `cartLinesUpdate` does not store the line's new quantity; cart totals and the line disagree | after adding a line with quantity 2: `cartLinesUpdate(cartId, lines: [{id: <line>, quantity: 3}])` → line quantity 3, total 84.00 → `totalQuantity: 3`, `cost 84.00`, line `quantity: 2`. The request log shows the `cartline` effect `rejected: "no schema to check against"` (lines live inside the cart, not as their own stored type). In the app: "+" shows 2 / $56 until a reload, then the line reads 1 × $28.00 while the total says $56.00. |
| 2 | Engine bug | `cartCreate` is never answered by the executor (`exec_fallback: render_failed`), so an empty cart costs a full model call | `mutation { cartCreate { cart { …app's cart fragment… } } }` → an empty cart rendered locally → `llm`, 25.7k prompt tokens, 12.9 s. This is the ~10 s wait on the first Add to cart seen in every run. |
| 3 | Inexpressible | Lists and collections are not stored, so a product page disagrees with the listing that linked to it | `/search/apparel` lists `fernwood-organic-tee` at 32.00 (`gid…/Product/8123456789012`); `product(handle: "fernwood-organic-tee")` → that product → a new one, 34.00, `gid…/8123456789045`. The records sit at `products.edges[].node`; `answer.records` is one JSON pointer and cannot name `edges/*/node` (`/edges` is accepted but stores nothing readable: `product(id: <listed id>)` is `llm`, not `state`). `Query.collection`/`Query.products` stay `no_target`/`no_identity`. |
| 4 | Engine bug | A `target: {member: handle}` on a read whose type's identity is `id` writes the handle into `id` | world `Query.product: {target: {member: handle}}`, then `product(handle: "fern-tee") { id }` → `gid://shopify/Product/…` → `"fern-tee"`; a later `product(handle:)` for a product stored from a list is generated again. Reverted for the app run. |
| 5 | Model answer | One gid for many products | `gid://shopify/Product/8123456789012` is used for 7 different handles across lists (ridge tee, classic tee, trail tee, organic tee, logo tee, mug, tote); the persona's example id was copied. Only harmless because of #3. |
| 6 | Model answer | Homepage three-item grid missing | `collection(handle: "hidden-homepage-featured-items") { products(first: 100) }` → 3 products (persona says 3; the grid renders nothing below 3) → 2, both runs. |
| 7 | Model answer | Thin catalog | every list (`products(first: 100)`, each collection) answers 2 products; `/search` shows 2 items. |
| 8 | Skills/MCP gap | Expressions are referenced but not documented | the world schema says `set`, `computed`, `effects` and `refuse` take "a CEL subset, see docs/world.md"; neither `docs/world.md` nor the `mockingbird-world` skill describes expressions, so the agent cannot write cart arithmetic (line cost = quantity × price, totals) and leaves it to the model. |
| 9 | Skills/MCP gap | GraphQL heuristics point every mutation at its `*Payload` type | `world show` on the Storefront contract: 112 gaps, 36 of them `no_identity` on `CartLinesAddPayload`-style types; each cart mutation had to be retyped to `cart` with `target: cartId`, `answer: /cart`. The skill doesn't mention the payload pattern. |
| 10 | Skills/MCP gap | Direct calls to a mock with `hosts` 404 | `curl localhost:45080/api/2023-01/graphql.json` → the mock → 404 "no mock is mounted … for host 127.0.0.1" until `Host: fernwood-goods.myshopify.com` is sent; the skill's `curl localhost:8080/<mock>` example doesn't say so. |

### Numbers

| Calls | Sources | Fallbacks | Model-served latency | Tokens (prompt / completion) | OpenRouter spend |
| --- | --- | --- | --- | --- | --- |
| 80 (build, pages, cart, self-test) | 42 `llm`, 37 `state`, 1 `cache` | 0 | p50 2.8 s, p95 17.5 s, max 18.3 s (n = 42) | 268k / 27k | ≈ $0.02 (at OpenRouter's listed $0.006/M prompt, $0.50/M completion for `-1017`) |

The slow tail is list generation (`products` sorted by price 17.6 s, a 2-product collection 17.5 s) and `cartLinesUpdate` (18.3 s).

## Run 5: after the engine fixes (2026-11-11)

The binary was built from `main` with #402, #403 and #418 merged, plus #407 and #424, which were still waiting for CI. Model, persona and config are as in run 4. The world file now holds only the `handle` targets (`menu`, `pageByHandle`, `product`, `collection`); the cart mutations are no longer described by hand. Config: `mockingbird-trials/commerce/mb5/`. Fresh store and a fresh cart cookie.

**The cart flow works end to end.**

1. On the product page, select Fern Tee, M ($32).
2. Click Add to cart: the drawer shows Fern Tee M with its image.
3. Add twice more: quantity 3, $96.
4. Press +, then reload: the line reads 4 × $32 = $128.00, and the total agrees.
5. Remove, then reload: "Your cart is empty".

`next build` works, and every page answers 200, as in run 4.

| Finding (run 4) | Fix | Run 5 |
| --- | --- | --- |
| #9 mutations typed as their `*Payload` | #403 | `cartLinesAdd`/`Update`/`Remove` come out of the import as actions on `cart`, targeted by `cartId`, with the answer at `/cart`. Gaps dropped from 112 to 79, none on cart payloads. |
| #2 `cartCreate` model call (`render_failed`) | #402 | Answered by the executor (`state`): 1.5 s and 6.5k tokens for the fill, down from 12.9 s and 25.7k tokens. |
| #4 `handle` target written into `id` | #402 | Reads by handle keep the product's gid. A repeat read by the same handle comes from `state`. |
| #1 line quantity not stored | #402, #407 | Fixed. #402 put the line effect into the cart. The GraphQL resolver then merged the model's answer (quantity 1) over it, so #407 lays the stored effect values over the answer. |
| #5 one gid for many records (model answer) | #418, #424 | Fixed for the flow. The model reused the persona's example product id and one variant id across answers, which put a hoodie (run 5a) or a Classic Tee (run 5b) in the cart for Fern Tee M. A generated record reusing a held id under another parent, or under a different `handle` in a read by handle, now gets a new id in the same shape. |

| Calls | Sources | Fallbacks | Model-served latency | Tokens (prompt / completion) | OpenRouter spend |
| --- | --- | --- | --- | --- | --- |
| 62 (build, pages, cart) | 28 `llm`, 33 `state`, 1 `cache` | 0 | p50 4.3 s, p95 8.0 s, max 16.4 s (n = 28) | 160k / 29k | ≈ $0.02 |

Still open (none blocks the flow):

- The app's first Add to cart only creates the cart, and a second click adds the line (every run; the 10 s wait has dropped to about 2 s now that `cartCreate` is local).
- The executor mints the cart id as a faker word (`quartz`), not a `gid://shopify/Cart/…`. The app doesn't care. Class: engine.
- A cart id the server never created (a stale cookie from another run) is generated by `cart(id:)` with totals that don't add up. Start runs with a fresh cookie.
- Model answers left out of scope: thin lists (2 products), and a homepage grid with 2 of 3 items.

Session spend for all runs: about $0.06 of the $2 cap.
