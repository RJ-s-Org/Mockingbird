# Plaid

A mock of the Plaid API in Sandbox for "Ledgerly", a budgeting app linked to First
Platypus Bank: Link tokens, public token exchange, accounts, Auth (ACH numbers),
Identity and `/transactions/sync`, all POST with JSON bodies like the real API, and a
`TRANSACTIONS` / `SYNC_UPDATES_AVAILABLE` webhook.

| | |
| --- | --- |
| Contract | [`plaid.openapi.yaml`](plaid.openapi.yaml), a hand-written subset that keeps Plaid's operation ids (`linkTokenCreate`, `accountsGet`, ...) |
| Upstream | [plaid/plaid-openapi](https://github.com/plaid/plaid-openapi) `2020-09-14.yml`, not pinned: the repository has no license file (see [`example.json`](example.json)) |
| Hosts | `sandbox.plaid.com`, `production.plaid.com` |
| Scenarios | `item-login-required` (400), `rate-limited` (429), `outage` (500) |
| Webhooks | `SYNC_UPDATES_AVAILABLE` after `/transactions/sync` to `http://127.0.0.1:8000/plaid/webhook`, signed `hmac-sha256` with `env:PLAID_WEBHOOK_SECRET` |

## Quickstart

```bash
mockingbird examples init plaid --dir plaid && cd plaid   # or: cd examples/apis/plaid
mockingbird serve -c mockingbird.yaml --offline
```

In another shell:

```bash
curl -s localhost:8080/accounts/get -H 'Host: sandbox.plaid.com' -H 'content-type: application/json' \
  -d '{"client_id":"test_client_id","secret":"test_secret","access_token":"access-sandbox-de3ce8ef-33f8-452c-a685-8671031fc0f6"}'
```

The official Python SDK, unmodified, through the proxy. `plaid-python` is generated
by OpenAPI Generator and reads neither the proxy variables nor the system CA bundle,
so the snippet hands it both:

```bash
eval "$(mockingbird proxy env)"
pip install plaid-python
python - <<'PY'
import os, plaid
from plaid.api import plaid_api
from plaid.model.sandbox_public_token_create_request import SandboxPublicTokenCreateRequest
from plaid.model.item_public_token_exchange_request import ItemPublicTokenExchangeRequest
from plaid.model.accounts_get_request import AccountsGetRequest
from plaid.model.transactions_sync_request import TransactionsSyncRequest
from plaid.model.products import Products

config = plaid.Configuration(host=plaid.Environment.Sandbox,
                             api_key={"clientId": "test_client_id", "secret": "test_secret"})
config.proxy = os.environ["HTTPS_PROXY"]          # the generated client ignores proxy env vars
config.ssl_ca_cert = os.environ["SSL_CERT_FILE"]  # ...and uses certifi unless told otherwise
client = plaid_api.PlaidApi(plaid.ApiClient(config))

public = client.sandbox_public_token_create(SandboxPublicTokenCreateRequest(
    institution_id="ins_109508", initial_products=[Products("transactions")]))
access = client.item_public_token_exchange(ItemPublicTokenExchangeRequest(public_token=public.public_token))
accounts = client.accounts_get(AccountsGetRequest(access_token=access.access_token))
print(access.item_id, [(a.name, a.mask, str(a.type)) for a in accounts.accounts])
sync = client.transactions_sync(TransactionsSyncRequest(access_token=access.access_token))
print(len(sync.added), "added, has_more =", sync.has_more)
PY
```

Drop `--offline` (and configure an LLM, see [the configuration reference](../../../docs/configuration.md#llm))
for answers that follow the persona: the Plaid Checking / Saving / Credit Card
accounts, Alberta Charleson as the owner, everyday US merchants in the transactions.

## Expected output

`--offline` answers come from the schema faker, so values differ run to run; the shape
does not:

```json
{
  "accounts": [
    {
      "account_id": "pztElxwgv3myoMU1bdjPYj1aB4LVsvVVVZtHS",
      "balances": {
        "available": 21372.81,
        "current": 17254.41,
        "iso_currency_code": "USD",
        "limit": 289.81,
        "unofficial_currency_code": null
      },
      "mask": "8746",
      "subtype": "savings",
      ...
    }
  ],
  "item": { "item_id": "...", "available_products": ["liabilities"], ... },
  "request_id": "..."
}
```

The Python snippet prints lines like:

```
aJKQCOKNIjDfJap6lYQnnVc4Uaa1Vzv9sN8fT [('iris', '2315', 'depository')]
1 added, has_more = False
```

## Scenarios

```bash
curl -s localhost:8080/accounts/get -H 'Host: sandbox.plaid.com' -H 'content-type: application/json' \
  -H 'X-Mockingbird-Scenario: item-login-required' \
  -d '{"client_id":"test_client_id","secret":"test_secret","access_token":"access-sandbox-de3ce8ef-33f8-452c-a685-8671031fc0f6"}'
# HTTP 400 {"error_type":"ITEM_ERROR","error_code":"ITEM_LOGIN_REQUIRED","error_message":"the login details of this item have changed ...","status":400,...}
```

The SDK raises `plaid.ApiException` with `status` 400 (or 429 for `rate-limited`,
`error_type` `RATE_LIMIT_EXCEEDED`); `json.loads(e.body)["error_code"]` is the Plaid
code. Set `default_scenario: item-login-required` on the mock to send your app down
its Link update-mode path on every call.

## Webhooks

Run your webhook handler on port 8000 (or change `url`):

```bash
export PLAID_WEBHOOK_SECRET=whsec_test_local   # any value; the mock and your app share it
mockingbird serve -c mockingbird.yaml --offline
```

Each successful `POST /transactions/sync` sends
`{"webhook_type":"TRANSACTIONS","webhook_code":"SYNC_UPDATES_AVAILABLE","item_id":...,"environment":"sandbox",...}`;
`mockingbird webhooks deliveries plaid` lists the attempts.

Plaid signs real webhooks with an ES256 JWT in the `Plaid-Verification` header, checked
against a key from `/webhook_verification_key/get`. The mock cannot produce that: it
signs with `X-Mockingbird-Signature: sha256=<hex HMAC-SHA256(secret, body)>` (see
[the webhooks guide](../../../docs/webhooks.md#signatures)), so skip JWT verification
in tests or verify that header instead.
