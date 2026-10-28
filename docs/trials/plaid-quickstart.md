# Trial: plaid/quickstart (Python, Flask)

[plaid/quickstart](https://github.com/plaid/quickstart) at `cd23ead`: Plaid's reference app, a React front end over a choice of backends. This trial ran the Python backend (Flask 3.1, `plaid_python` 45.0.0) and drove the API routes its front-end tabs call.

| | |
| --- | --- |
| API mocked | Plaid (`sandbox.plaid.com`): `/link/token/create`, `/item/public_token/exchange`, `/accounts/get`, `/auth/get`, `/identity/get`, `/transactions/sync`, `/accounts/balance/get`, `/item/get`, `/institutions/get_by_id` |
| Contract | The catalog's Plaid subset (`mockingbird examples init plaid`), with the two operations this PR adds |
| Model | OpenRouter `deepseek/deepseek-v4.1-flash`, default routing |
| Source edits | None (`git diff` empty) |
| Result | 9/9 routes answer 200 after this PR's fixes; 10 mock calls, 0 fallbacks, 0.7–3.6 s (median 2.0 s) |

## Config and commands

```bash
mockingbird examples init plaid --dir mockingbird
mockingbird llm configure --provider openrouter --model deepseek/deepseek-v4.1-flash --default -c mockingbird/mockingbird.yaml
```

Then ports that do not collide with other servers on the machine:

```yaml
server:
  port: 41080
  admin_port: 41090
  grpc_port: 41051
proxy:
  enabled: true
  port: 41888
```

The `mocks.plaid` block is the catalog's, unchanged: `hosts: [sandbox.plaid.com, production.plaid.com]`, `route_prefix: /`, the "Ledgerly" persona and its scenarios.

```bash
cd python
PLAID_CLIENT_ID=test_client_id PLAID_SECRET=test_secret PLAID_ENV=sandbox \
PLAID_PRODUCTS=auth,transactions PLAID_COUNTRY_CODES=US PORT=8000 \
  mockingbird run -c ../mockingbird/mockingbird.yaml -- .venv/bin/python server.py
```

The flows (`flows.sh`) are the calls the front end makes: `POST /api/create_link_token`, `POST /api/set_access_token` with a sandbox public token, then `GET /api/{accounts,auth,identity,transactions,balance,item}` and `accounts` again. Plaid Link itself (the bank picker served from `cdn.plaid.com`) was not exercised; `set_access_token` takes any public token.

## What happened

**Run 1, integration build (L1+L2+L3+L6 merged locally): the app bypassed the proxy.** `create_link_token` came back from the real Plaid sandbox: `INVALID_FIELD: client_id must be a properly formatted, non-empty string` (the dummy credentials were rejected; nothing else was sent). `plaid_python` is OpenAPI-generated: it builds a plain `urllib3.PoolManager`, which ignores `HTTPS_PROXY`, and passes `certifi.where()` as its CA file, which ignores `SSL_CERT_FILE`. The mock's request log stayed empty.

**Fix (this PR): a Python preload.** `proxy env` now puts `<ca_dir>/python` first on `PYTHONPATH`. Python imports the `sitecustomize.py` there at startup; it points `certifi.where()` at the bundle and sends a plain `PoolManager`'s requests for hosts outside `NO_PROXY` through `HTTPS_PROXY`. With it, every call reached the mock and nothing else changed.

**Run 2, with the preload: 7 of 9 routes worked.** `/api/balance` and `/api/item` answered 404 `no operation matches POST /accounts/balance/get` (and `/institutions/get_by_id`): the catalog subset did not have them. This PR adds both. The first try at `/institutions/get_by_id` still failed in the SDK: plaid-python 45 requires `institution.connection_availability`, and `products` is a list. The contract now has both.

**Run 3, this PR's binary: 9/9.**

| Calls | Sources | Fallbacks | Latency (generated) | Tokens (prompt / completion) |
| --- | --- | --- | --- | --- |
| 10 | 2 `llm`, 8 `repaired` | 0 | 0.7–3.6 s, median 2.0 s | 14.2k / 4.4k |

Most `repaired` answers cost no extra LLM call: the model cannot count to 37, so `account_id`/`item_id` values fail `^[A-Za-z0-9]{37}$` and are refilled locally. In run 2, one answer (`/accounts/get`) needed a real repair round trip because a credit-card `current` balance came back negative against `minimum: 0`. This PR patches `minimum`/`maximum`/`multipleOf` errors locally too.

## Realism and consistency

- The persona held: First Platypus Bank, the three named accounts with masks 0000/1111/3333, Alberta Charleson's address on Identity, ACH numbers on Auth.
- **Account ids differ between calls.** `/accounts/get`, `/auth/get`, `/identity/get`, `/transactions/sync`, `/accounts/balance/get` and a repeated `/accounts/get` each invent their own `account_id`s and balances. Plaid's reads are all `POST`, so the consistency cache (reads only) and state seeding never apply. Open in [improvements](../improvements.md#from-field-trials-20-2026-10-28).
- **Dates are not recent:** a link token expiring "Wed, 12 Jun 2024", transactions dated June 2024 and June 2025, though the persona says "within the last 30 days" (an existing Open item; the prompt has no clock).

## The bar

`mockingbird verify --no-fallbacks --no-unmapped` passed on run 2 even though two routes 404'd: a path the contract does not have is a 4xx, not an unmapped host or a fallback. Open: count "no operation matches" as its own failure (`--no-unmatched`).

## After the engine fixes

Run 6, on the engine-fixes branch: `POST`s whose last segment is a read verb are reads (cached, seeded, generated knowing the lists other reads stored). Same app, config, contract and model.

| Route | Run 4 `account_id` of the first account | Run 6 |
| --- | --- | --- |
| `/api/accounts` | `GQ144VATLjBE…` | `g6Of06pOJanJ…` |
| `/api/auth` | `tW35i4Gf1Vol…` | `g6Of06pOJanJ…` |
| `/api/identity` | `57TkotGYFpEk…` | `g6Of06pOJanJ…` |
| `/api/transactions` | `pQ3rT7vW1xY4…` | `g6Of06pOJanJ…` |
| `/api/balance` | `XSfg0kLag3pn…` | `g6Of06pOJanJ…` |
| `/api/accounts` (again) | `VgjSr5AVGJpI…`, 2.0 s | `g6Of06pOJanJ…`, `cache`, 3 ms |

Balances agree too (available 1420.75, current 1583.42 on every route). The repeated `/accounts/get` is a `cache` hit; the other reads see the accounts `/accounts/get` seeded (stored under `accounts`, keyed by `account_id`).

| Calls | Sources | Fallbacks | Latency (generated) | Tokens (prompt / completion) |
| --- | --- | --- | --- | --- |
| 10 | 4 `llm`, 5 `repaired`, 1 `cache` | 0 | 0.8–5.2 s, median 2.2 s | 14.6k / 4.3k |

The catalog's `transactionsSync` webhook still fires: a target that names a read's operation gets its event.
