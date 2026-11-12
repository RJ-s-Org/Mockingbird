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

## Run 7 (2026-11-10): built through the skills and MCP, `deepseek-v4.1-flash-1017`

Release binary from `main` at `0c843950`, built into the shared lane. Plaid quickstart `cd23ead`, Python backend, no source edits. The coding agent used only the installed skills, the MCP tools (`call_mock`, `get_world`, `patch_world`, `list_requests`, `reset_session`) and the CLI:

```bash
mockingbird examples init plaid --dir mb-run7
mockingbird llm configure --provider openrouter --model deepseek/deepseek-v4.1-flash-1017 --default -c mockingbird.yaml   # api_key: env:OPENROUTER_API_KEY
mockingbird server start --proxy -c mockingbird.yaml      # ports 43080/43090/43888
eval "$(mockingbird proxy env -c mockingbird.yaml)"; PLAID_CLIENT_ID=test_client_id PLAID_SECRET=test_secret PORT=8017 python server.py
```

World entries written, following the `mockingbird-world` skill:
- `item` has identity `item_id`.
- `itemPublicTokenExchange` is a create of `item` (answer record at the root).
- `itemGet` reads `item` with target `access_token` and `not_found: 400`.
- The four account reads and `transactionsSync` are lists with `answer.records`.
- `access_token`, `client_id` and `secret` are listed as `ignores`.
- `transactionsSync` has cursor paging.

The flows were run twice: once before the `ignores` and paging entries (7a), once after (7b). The front end's order is `info`, `create_link_token`, `set_access_token`, then `accounts`, `auth`, `identity`, `transactions`, `balance` and `item`, and finally `accounts` again.

### What works

- **All 9 routes answer 200 in both runs, with 0 fallbacks.** `plaid-python` reaches the mock through the proxy preload.
- **The exchange's `item_id` holds through the session.** It comes back from `/item/get` and in the `item` of `/accounts/get`, `/auth/get` and `/accounts/balance/get`.
- **Account data agrees across calls.** `account_id`s, masks and balances match on accounts, auth, identity, balance and the transactions' `account_id`.
- **A repeated `/accounts/get` is a cache hit** (3–5 ms).
- **Dates are now recent:** the link token expires `11 Nov 2026`, and transactions are dated 6–10 Nov 2026. The persona held: First Platypus Bank, masks 0000/1111/3333, Alberta Charleson, ACH routing 011401533.
- **Self-test with `call_mock` worked:** an exchange followed by `/item/get` gave back the same `item_id`.

### Failures

| # | Repro (request → expected / got) | Class |
| --- | --- | --- |
| 1 | `POST /accounts/get {"access_token":"access-sandbox-00000000-0000-4000-8000-000000000000"}` (never issued) → expected 400 `INVALID_ACCESS_TOKEN` / got 200 with invented accounts. In the app's session, it returns that session's accounts under a new `item_id` (`b2Nc4d…`, not the exchanged `k7Qm2x…`). | inexpressible: `access_token` names the Item but is not an Item member. `types.item.identity.alternates: [access_token]` is refused with `access_token is not a member of a item record`, and the format cannot say that a create's answer member is an alias for the record. |
| 2 | With `itemPublicTokenExchange: {kind: create, type: item, answer: {record: ""}}`, `POST /item/public_token/exchange` → expected the item stored / got `exec_fallback: record_invalid` (the answer `{access_token,item_id,request_id}` lacks the Item's required members). `/item/get` is then `not_held`, so the model answers it and `not_found: 400` never applies. | engine bug: a create whose answer carries the identity and only some members should store a partial record. Arguably inexpressible. |
| 3 | `POST /accounts/get` (and `/auth/get`, `/identity/get`, `/accounts/balance/get`, `/transactions/sync`) → expected an answer from state / got model-served every time. The fallback is `undescribed_param access_token`; once `access_token` is in `ignores`, it is `unscoped_list`. 25 of 27 calls were `llm`, and `/accounts/get` took 3–9 s. | inexpressible: the list's scope is the Item named by `access_token`, and account records have no item member for `scope.member`. |
| 4 | `POST /transactions/sync {"cursor":""}` → expected a page of about 10–30 recent transactions / got 1 transaction with `has_more: true`. The second page, `{"cursor":"<next_cursor>"}`, gives `exec_fallback: unknown_token` (the executor did not mint the model's cursor) and 1 more transaction. That is 2 transactions in total in 7b, and 1 in 7a. | model answer (thin data). Follows from #3. |
| 5 | `POST /identity/get` → expected all 3 accounts / got 2: the credit card is missing in both runs, although the contract allows 3. | model answer |
| 6 | 7b: `/accounts/get` → expected distinct ids / got the first `account_id` equal to the exchange's `item_id` (`k7Qm2xR9tLpV4nB8…`). | model answer |
| 7 | `call_mock {path:"/item/public_token/exchange"}` on the catalog mock (`route_prefix: /`, `hosts: [sandbox.plaid.com]`) → expected 200 / got 404 `no mock is mounted … for host 127.0.0.1`. It works only with `headers: {Host: sandbox.plaid.com}`, which no skill mentions. Also, when the MCP server runs outside the server's directory, `call_mock` goes to `:8080`, because it reads `.mockingbird/server.json` from its own working directory. | skills/MCP gap |
| 8 | `patch_world` with `types.account.identity` plus `operations.accountsGet.type: account` in one patch → expected a new type / got both refused (`no operation has resource type account`). The four account reads stay as four heuristic types (`accounts`, `auth`, `identity`, `balance`). | engine bug (world validation checks types before the same patch's operation types) |

### Numbers

| Calls | Sources | Fallbacks | Model-served latency | Tokens (prompt / completion) | OpenRouter spend |
| --- | --- | --- | --- | --- | --- |
| 27 (2 flow runs plus probes) | 25 `llm`, 2 `cache` | 0 | p50 2.3 s, p95 8.0 s, max 9.2 s | 44.3k / 9.9k | about $0.005 at $0.006/M prompt and $0.50/M completion (the shared key's daily usage was $0.066 in total) |

## Run 9 (2026-11-11): after the fixes for findings #1–#3, #7 and #8

Built from `main` plus the token-alias PR (#415). Same app, contract and model (`deepseek/deepseek-v4.1-flash-1017`). The world the agent wrote now says what Plaid's documentation says:

```json
{"types": {
   "item": {"identity": {"member": "item_id", "alternates": ["access_token"]}},
   "account": {"identity": {"member": "account_id"}, "parent": {"type": "item"}},
   "transaction": {"identity": {"member": "transaction_id"}, "parent": {"type": "item"}}},
 "operations": {
   "itemPublicTokenExchange": {"kind": "create", "type": "item", "answer": {"record": ""}},
   "itemGet": {"kind": "read", "type": "item", "target": {"member": "access_token"}, "answer": {"record": "/item"}, "not_found": 400},
   "accountsGet": {"kind": "list", "type": "account", "answer": {"records": "/accounts"}, "scope": {"param": "access_token"}, "ignores": ["client_id", "secret"], "not_found": 400}}}
```

`authGet`, `identityGet` and `accountsBalanceGet` are written like `accountsGet`. `transactionsSync` is too, plus cursor paging.

All of it applies in one `patch_world` (no refusals; #397). `call_mock {mock: "plaid", …}` reaches the host-bound mock from any directory with no hand-set `Host` (#404).

### What changed

- **Exchange (#408 and #415):** `/item/public_token/exchange` is answered from state. The Item is stored, with its members filled once. The `access_token` is minted in the contract's shape, and `/item/get` by that token reads the Item back from state.
- **Unknown token:** `{"access_token": "access-sandbox-nope"}` on `/item/get` or `/accounts/get` answers the declared 400 (`ITEM_NOT_FOUND`-style body), not invented data.
- **Account lists:** `/accounts/get` generates once, and that marks the Item's accounts. `/auth/get`, `/accounts/balance/get` and the repeated `/accounts/get` are then answered from state in 3–24 ms, with the same `account_id`s and balances.

| Call (app session) | Run 7 | Run 9 |
| --- | --- | --- |
| `/link/token/create` | llm | llm (`no_identity`: nothing to store) |
| `/item/public_token/exchange` | llm | **state** (fill: 1 model call) |
| `/accounts/get` | llm | llm (`never_listed`: first list of the Item) |
| `/auth/get` | llm | **state**, 3 ms |
| `/identity/get` | llm | patched (`render_failed`: stored accounts have no `owners`) |
| `/transactions/sync` (2 pages) | llm, llm | llm, llm (`never_listed`; page 2 `unknown_token`) |
| `/accounts/balance/get` | llm | **state**, 5 ms |
| `/item/get` | llm | **state**, 0 ms |
| `/institutions/get_by_id` | llm | llm (`not_held`: a catalog record) |
| `/accounts/get` again | cache | **state**, 15 ms |

**5 of 11 app calls from state (run 7: 0, plus 1 cache hit).**

| Model-served latency (app session) | Tokens (prompt / completion, whole run with probes) | OpenRouter spend |
| --- | --- | --- |
| p50 3.3 s, max 6.6 s (6 calls) | 18.6k / 3.5k | about $0.002 |

### Still open (not blocking the flows)

- **`/identity/get` (`render_failed`):** its items require `owners`, which the accounts stored by `/accounts/get` do not hold, so the model answers it, with the same accounts.
- **`/transactions/sync` page 2 (`unknown_token`):** the first page is generated, so its `next_cursor` is the model's, and the executor cannot continue it. The model answers page 2.

## Rerun (2026-11-11): a fresh Claude Code given one prompt

`main` at `0d591b1b`, model `deepseek/deepseek-v4.1-flash-1017`, Python backend unchanged.

**Setup:**
- The mock was built by a fresh headless Claude Code (`claude -p`, default model) in a clean clone of the quickstart.
- The plugin was installed the README way: `claude plugin marketplace add rj-s-org/mockingbird`, then `claude plugin install mockingbird@mockingbird`. As in the healthchecks rerun, `main`'s binary was on `PATH`, because the plugin would install the stale `v0.1.0` release.
- The agent was given only this prompt:

> I want to test this Plaid quickstart's Python backend (python/server.py) without Plaid credentials. Use Mockingbird to mock the Plaid API it calls, serving the mock with the OpenRouter model deepseek/deepseek-v4.1-flash-1017; my key is in OPENROUTER_API_KEY. My test script is mb-flow/flows.sh: it calls the backend's API routes in the order the React front end does (link token, set_access_token, then accounts, auth, identity, transactions, balance, item) on port 8017. Run the backend unchanged with dummy credentials and PLAID_ENV=sandbox, and make every flow return real-looking data from the mock.

**How the agent built it:**
- **Contract:** it downloaded Plaid's official OpenAPI spec and imported it with MCP `import_contract`. It did not use the catalog's Plaid example, which carries the reviewed world description (token aliases, scoped lists) that run 9 answered from. It found the catalog only at the end. The skills did not point to the catalog yet; #481, which landed after this run, makes it the first step.
- **Tools:** 89 turns. 36 `Bash` calls, and MCP `patch_world` ×9, `reset_session` ×7, `update_mock` ×6, `call_mock` ×4, plus `start_server`, `get_world`, `put_scenario` and `put_world`.
- **World attempts:** it tried to describe the item, accounts and transactions with `patch_world`. The calls fell back with `exec_fallback: not_plain`, and one revision answered `/item/get` with another item, so it put back the defaults.
- **Route prefix:** it set `hosts` and then `route_prefix: /` with `update_mock`. The mock kept answering only under `/plaid` until the server restarted (failure 2).
- **Time:** 11.3 min from the prompt to all ten flows answering 200 with consistent ids. Agent cost $3.40 at API prices (on the subscription here).

### Scorecard (measured run after the agent finished)

| # | Measure | Result |
| --- | --- | --- |
| 1 | Flows pass | 10 of 10 routes answer 200 with Plaid-shaped data |
| 2 | Consistency the flows check | account ids agree across accounts, auth, balance, identity and transactions, and `item_id` across exchange and item. These agree only because the model was given the same context: none of the flow's account, item or transaction calls is answered from state. Identity lists 2 of the 3 accounts |
| 3 | Agent build | **11.3 min** (target ≤ 15) |
| 4 | State-served latency | 6 ms (1 call, `institutions/get_by_id`). The repeated `accounts/get` came from `cache` in 7 ms |
| 5 | Model-served latency | **p50 4.5 s, p95 6.7 s** (8 calls; fails < 2 s). Prompts ran to 19k tokens, the full Plaid spec's schemas |
| 6 | Spend per session | $0.0026 (92k prompt / 4.0k completion tokens) |
| 7 | Errors | not exercised by the flows |

**Sources:** 10 calls. 7 `llm`, 1 `patched`, 1 `state`, 1 `cache`. Run 9, with the catalog example, answered 5 of 11 from state.

### Failures

| # | Repro | Class | Status |
| --- | --- | --- | --- |
| 1 | The agent wrote its mock from the vendor's full spec instead of the catalog example, so only `institutions/get_by_id` was answered from state and the model's prompts ran to 11–19k tokens. | skills gap | fixed by #481 (the skills check the catalog first) |
| 2 | `update_mock {route_prefix: "/"}` on a running server succeeded, and the mock's view showed `/`, but the agent saw the mock answer only under `/plaid` until it restarted the server. | engine (suspected) | not reproduced on `main`: a PATCH of `route_prefix` and `hosts` remounts the mock live on the data plane and the proxy's host list (`proxy::patching_prefix_and_hosts_remounts_the_mock_live`). One possible cause: the agent had started the server twice, and the second start hit "address in use" with the first still running. A PATCH answered by one process does not reach another's data plane. #474 (a host-bound mock defaults to `/`) removes the need for the edit |
| 3 | `/transactions/sync` returns 1 transaction; the persona and a default scenario asking for more were ignored. | model answer | out of scope (the bar is testability) |

## Rerun 2 (2026-11-11): the catalog first

`main` at `0d68585b`, which includes #474 and #481. Same harness as the rerun above: a fresh headless Claude Code, the plugin installed the README way, `main`'s binary on `PATH`, the same one prompt.

**How the agent built it:**
- It started from `list_examples` and `init_example plaid`, the catalog's contract and config, as the skills now say.
- Another project's server held the default ports, so it moved to 8180, admin 9190 and proxy 8988.
- It wrote a small world: link token, public token, and the Item with `access_token` as an issued alternate.
- It added `minItems: 6` to `/transactions/sync`'s `added`, after the model returned one or two transactions.
- `verify --no-unmapped --no-unmatched --no-fallbacks` passed on 62 requests.
- **Tools:** 60 turns. 33 `Bash` calls, and MCP `call_mock` ×5, `get_world` ×4, `list_requests` ×3, `list_examples` ×2, `init_example`, `configure_llm`, `start_server`, `test_llm` and `get_request`.
- **Time:** **7.7 min** from the prompt to all ten flows passing. Agent cost $2.47 at API prices (on the subscription here).

### Scorecard (measured run)

| # | Measure | Result |
| --- | --- | --- |
| 1 | Flows pass | 10 of 10 routes answer 200 |
| 2 | Consistency the flows check | `/item/get` returns the exchanged Item, from state. Account ids agree across the five account calls, but only because the model was given the same context: those calls are generated |
| 3 | Agent build | **7.7 min** (target ≤ 15) |
| 4 | State-served latency | p50 26 ms, p95 590 ms (4 calls: link token, exchange, item, institution). The repeated `accounts/get` came from `cache` in 25 ms |
| 5 | Model-served latency | **p50 4.6 s, p95 5.4 s** (5 calls: accounts, auth, identity, transactions, balance; fails < 2 s) |
| 6 | Spend per session | $0.0022 (27.6k prompt / 4.1k completion tokens) |
| 7 | Errors | not exercised by the flows |

**Sources:** 10 calls: 4 `state`, 1 `cache`, 5 `llm`.

### Failures

| # | Repro | Class | Status |
| --- | --- | --- | --- |
| 1 | `world show plaid` reported **0 gaps**, while every account call carried `request.world_gap: ["no_identity"]` and was generated. The agent trusted the gap list and stopped. | engine (reporting) | fixed in #521 |
| 2 | The catalog's Plaid example shipped no world description, so the account calls had nothing to answer from. | catalog | fixed in this PR: `plaid.world.yaml` describes accounts as children of the Item, scoped by the issued token. Later account lists come from state |
| 3 | Lists with `client_id`/`secret` in the request body stayed `never_listed` despite `ignores: [client_id, secret]` (`asks_only_a_page` did not consult `ignores`). The Python SDK sends credentials as headers, so the quickstart was unaffected. | engine | fixed by the `asks_only_a_page` `ignores` fix |
| 4 | A list's envelope `item.item_id` is generated on every call and doesn't match the exchanged Item. `/auth/get`'s `numbers.ach[].account_id` can name an unlisted account. | engine | partly fixed in #526 (lists from state); the first generated list in the PR after it (see rerun 3) |
| 5 | `set` on the link-token create was ignored with no `expr_failed`. The `token` type has no read operation, so it has no record schema (the open follow-up from #494). | engine | open |
| 6 | Identical requests are answered from the response cache, so two identical `link/token/create` calls give the same token. | engine default | noted; not blocking the flows |

## Rerun 3 (2026-11-11): after the list fixes

`main` at `b7ce68a2`, with #521, #525 and #526. Same harness: a fresh headless Claude Code, the plugin installed the README way, `main`'s binary on `PATH`, and the same one prompt.

**How the agent built it:**
- It started from `list_examples` and `init_example plaid`, which now ships `plaid.world.yaml`.
- It raised `limits.output_budget_tokens` from the default 1500 to 6000, after `/transactions/sync` returned one transaction and `/identity/get` two of three accounts.
- It recorded the exchanged Item's `institution_id` (`ins_109508`) in the world. Before that, `/api/item` failed with a 500 because the stored Item had none.
- It added a default scenario telling the model to reuse the Item's accounts, and set `unknown_ids: 404`.
- `verify` was clean: 19 requests, 0 fallbacks, nothing unmatched.
- **Tools:** 46 turns. 32 `Bash` calls, and MCP `list_requests` ×2, `doctor`, `list_examples`, `init_example`, `configure_llm`, `test_llm` and `start_server`.
- **Time:** **5.7 min** of agent turns. The wall clock was 11.0 min, because a backend left in the foreground ran into the shell's 10-minute timeout after the flows had passed. Agent cost $2.42 at API prices (on the subscription here).

### Scorecard (measured run)

| # | Measure | Result |
| --- | --- | --- |
| 1 | Flows pass | 10 of 10 routes answer 200 |
| 2 | Consistency the flows check | `/item/get` returns the exchanged Item. `balance` and the repeated `accounts/get`, both from state, carry the exchanged `item_id` (#526). The first generated `accounts/get` and the model-answered `auth/get` carry drawn `item_id`s (failure 1). `auth`'s `numbers.ach` named listed accounts on this run. **0 of 8 transactions name a listed account** (failure 2) |
| 3 | Agent build | **5.7 min** of agent turns (target ≤ 15) |
| 4 | State-served latency | 5 calls (exchange, balance, item, institution, repeated accounts): p50 5 ms, p95 5 ms |
| 5 | Model-served latency | 5 calls: **p50 2.2 s, p95 9.9 s** (`transactions/sync` with 8 transactions; the 1.5k output budget raised to 6k). Fails < 2 s |
| 6 | Spend per session | $0.0031 (24.8k prompt / 5.8k completion tokens) |
| 7 | Errors | not exercised by the flows |

**Sources:** 10 calls: 5 `state`, 4 `llm`, 1 `patched`. Rerun 2: 4 `state`, 1 `cache`, 5 `llm`.

### Failures

| # | Repro | Class | Status |
| --- | --- | --- | --- |
| 1 | The first generated `/accounts/get`, and lists answered by the model, carry a drawn `item.item_id`; only lists from state carry the held Item. | engine | fixed in this PR: a generated list's parent member is the held parent, and envelope references to the listed type (`numbers.ach[].account_id`) name listed records, on generated lists and lists from state |
| 2 | `/transactions/sync` (its own type, scoped by the token): `latest_transactions[].account_id` names no listed account (0 of 8). A member of the listed records that references another type is not tied to that type's held records. Likely related: a generated list whose records sit beside other non-empty arrays of objects with no standard envelope key (`accounts`, `added`, `modified`, `removed`) never has its records stored, so the next list falls back with `never_written`. | engine | fixed in this PR: a list's records are found at the described `answer.records` (stored, marked listed, keyed by the type's described identity member); the records of another described type beside them (`accounts`) are stored under that type; and a reference to that type (`account_id`) names one the answer lists beside, or one the session listed under the same Item. A sync from state shows the held accounts its transactions name |
| 3 | The default `limits.output_budget_tokens` (1500) truncates realistic lists: 1 transaction, 2 of 3 identity accounts. | engine default | fixed in this PR: the default is 4000. The sync is estimated at about 2,850 tokens and `/identity/get` at about 1,700; TMDB details (about 5k) and GitHub search are still shaped |
| 4 | The exchange's stored Item had no `institution_id`, so `/item/get` → `/institutions/get_by_id(null)` failed with 500 until the agent set it in the world. | catalog / engine | fixed in this PR: the create's fill left the nullable `institution_id` `null`, as on a fresh record, and the app's SDK refused `null`. A read from state now gives a record a create stored a record's id for each server-set reference it holds as `null` (named for a type the contract reads but never creates, which no request, parameter, `set` or effect of its type sets, and that the contract's examples show set: the catalog's Item now carries Plaid's `ins_109508` example), and stores it only over the record as read |
