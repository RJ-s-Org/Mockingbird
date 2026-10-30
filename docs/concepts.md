# Concepts

Mockingbird turns an API contract into a running mock whose answers come from an LLM, are checked against the contract, and stay consistent for the length of a session. This page names the parts; the other pages go into each one.

## Mocks and contracts

A **mock** is one API you stand in for. It has a name, a route prefix on the data plane (`/` by default when you serve one contract), optional `hosts:` (the real host names it answers for through the [proxy](proxy.md)), a persona, scenarios, chaos, limits and webhooks.

A mock serves the active version of its **contract**: OpenAPI 3.x, Swagger 2.0, GraphQL SDL or introspection JSON, protobuf (`.proto` or a descriptor set), WSDL 1.1 or AsyncAPI 2.x/3.0. The format is detected from the bytes, and each protocol family has its own adapter ([Protocols](protocols.md), [Events](events.md)). Uploading a contract adds a version; activating one swaps the mock's runtime in place, without a restart. When you have no contract, [smart import](import.md) drafts one from documentation, a HAR recording or a Postman collection, for you to review and approve.

Mocks come from the config file (`source = file`, read-only through the API) or from the [Admin API](admin-api.md), the [CLI](cli.md), the [console](console.md) or an agent through [MCP](agents.md) (`source = api`).

## How an answer is made

For each request, the adapter decodes it and validates it against the operation (a request that fails validation gets an error in the contract's declared error shape, and an `X-Mockingbird-Validation` header saying why). Then the engine answers from the first source that can:

| `X-Mockingbird-Source` | Where the answer came from |
| --- | --- |
| `state` | The session already holds the resource (a read after a create, or an item seeded from an earlier list), and the stored value matches the response schema. No LLM call. |
| `cache` | The session asked exactly this before and nothing of that resource type was written since. No LLM call. |
| `llm` | The LLM generated it, prompted with the operation, its response schema, the request, the session's relevant state, the mock's persona and the scenario's instructions, and the answer validated. |
| `repaired` | The LLM's answer had small errors (a pattern, a length, a null where none is allowed, a value breaking a `uuid`, `date-time`, `date`, `time`, `email`, `uri`, `ipv4`, `ipv6` or `hostname` format) that were patched locally, or fixed in one repair call. A value the request sent (a path id or query value) is never patched over a format; other formats are annotations. |
| `fallback` | The built-in faker generated the answer, because there is no LLM (`--offline`), or the LLM timed out, failed or never produced a valid answer. `X-Mockingbird-Fallback-Reason` says which. An offline answer without a body (a `204`) is `fallback` too. |
| `rejected` | The request failed validation and was answered with the contract's error shape (or problem+json), without the LLM. `X-Mockingbird-Validation` says why. A client error, so `verify --no-fallbacks` does not count it. |

Every answer, whatever its source, matches the contract, with one known gap: the faker does not generate for `not` or `if`/`then`/`else` and relies on retries for them, and when no retry validates, the fallback reason ends with `faker output does not validate against the schema` (counted by `mb_faker_invalid_total`). `--offline` serves faker answers only, which needs no key and no network: the usual choice for CI.

## Sessions and state

State is kept per **session**, named by the `X-Mockingbird-Session` header (or `default`). Writes (creates, updates, deletes) change the session's state, so a `POST /orders` followed by `GET /orders/{id}` returns the order you created, and a deleted resource is gone. Lists seed the session too: the items of a list answer can be read back by id. Reads are `GET`s, and `POST`s whose last path segment is a read verb in RPC style (`/accounts/get`, `/transactions/sync`, `/institutions/get_by_id`, `users.list`, `_search`), so Plaid's reads are cached and seeded too, and a read's answer is generated knowing the lists other reads stored under its list members' names (`/auth/get` knows the `accounts` `/accounts/get` answered). GraphQL answers store every object with an `id` under its type (`Cart` as `cart`): a mutation's are merged into what the session holds, a query's added when new, so `cartCreate` → `cart(id:)` → `cartLinesAdd` → `cart(id:)` reads and updates one cart, and a mutation is generated knowing what its `<x>Id` arguments and `gid://…/<Type>/…` ids name. `unknown_ids: 404` makes reads of ids the session never saw answer 404, as the real API would.

Sessions can be exported as fixtures, seeded from fixtures and reset ([CLI](cli.md), [Admin API](admin-api.md)), which is how tests start from a known state. State lives in memory, in the storage database, or in Redis (`storage.state_url`), so several replicas share it.

## Personas, overrides and chaos

- A **persona** is free text added to every prompt of a mock: locale, realism, style ("Terse, realistic retail data in en-GB").
- **Override headers** change one request: `X-Mockingbird-Status`, `X-Mockingbird-Latency`, `X-Mockingbird-Scenario`, `X-Mockingbird-Session` and `X-Mockingbird-Chaos: off`. The [protocols](protocols.md#override-mapping-per-protocol) page shows how each maps onto GraphQL, gRPC and SOAP.
- **Chaos** injects errors in the contract's own error shape, latency jitter and dropped streams, per mock or per scenario ([configuration](configuration.md#mocksnamechaos)).
- **Webhooks** POST signed events after writes, the way Stripe or GitHub would ([Webhooks](webhooks.md)).

## Scenarios

A scenario is a named set of overrides for a mock: extra LLM instructions, a forced status, added latency, fields forced into the body (`set`), the item count and pacing of streamed responses (`stream_items`, `stream_item_interval`), optionally limited to some operation ids. Select one per request:

```sh
curl -i localhost:8080/orders/orders/o1 -H 'X-Mockingbird-Scenario: out-of-stock'
```

A scenario's status applies only to operations whose contract declares that status (or a `default` response), where its `set` fields are forced into that response's body too; a 5xx also applies to operations that do not declare it and is answered natively, as with `X-Mockingbird-Status`. Other operations are served normally, with the scenario's other overrides. The Admin API rejects (`422 Scenario status not declared`), and a config file may not declare, a non-5xx status that none of the scenario's operations declares in the mock's active contract, since it could never take effect. An unknown scenario name is a 400. Scenarios come from the config file or from `PUT /api/v1/mocks/{mock}/scenarios/{name}`.

A mock's `default_scenario` applies when a request names none, so clients that cannot send headers still get it. A scenario's `operations` entries may be `{id, instructions}` to give one operation its own instructions in place of the scenario's.
