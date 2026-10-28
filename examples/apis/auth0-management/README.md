# Auth0 Management API

A mock of an Auth0 tenant's Management API v2 for "Juniper & Co", a small plant shop:
the client-credentials token call, users (list, create, read, update, delete), users by
email, roles and role assignment, with Auth0's error shape
(`{statusCode, error, message, errorCode}`).

| | |
| --- | --- |
| Contract | [`auth0-management.openapi.yaml`](auth0-management.openapi.yaml), a hand-written subset that keeps the Management API's operation ids (`get_users`, `post_users`, `get_users_by_id`, `patch_users_by_id`, `delete_users_by_id`, `get_users-by-email`, `get_roles`, `get_user_roles`, `post_user_roles`) plus `oauth_token` for `POST /oauth/token` |
| Upstream | none pinned: Auth0 publishes no openly licensed OpenAPI description (see [`example.json`](example.json)) |
| Hosts | `juniper.us.auth0.com` (token and API on the same tenant host, as in Auth0) |
| Scenarios | `rate-limited` (429), `user-exists` (409 on create), `user-not-found` (404), `outage` (503) |
| Webhooks | none (Auth0 streams events through Log Streams, not per-call webhooks) |

## Quickstart

```bash
mockingbird examples init auth0-management --dir auth0-management && cd auth0-management   # or: cd examples/apis/auth0-management
mockingbird serve -c mockingbird.yaml --offline
```

In another shell:

```bash
curl -s localhost:8080/api/v2/users -H 'Host: juniper.us.auth0.com' -H 'Authorization: Bearer test-token' \
  -H 'content-type: application/json' \
  -d '{"connection":"Username-Password-Authentication","email":"jo.lee@example.com","password":"Test-passw0rd!","name":"Jo Lee"}'
```

The official Python SDK ([`auth0-python`](https://pypi.org/project/auth0-python/) 6.x),
unmodified, through the proxy. `ManagementClient` fetches its token from the mock's
`/oauth/token` with client credentials:

```bash
eval "$(mockingbird proxy env)"
pip install auth0-python
python - <<'PY'
from auth0.management import ManagementClient

client = ManagementClient(domain="juniper.us.auth0.com",
                          client_id="test-client-id", client_secret="test-client-secret")

user = client.users.create(connection="Username-Password-Authentication",
                           email="jo.lee@example.com", password="Test-passw0rd!", name="Jo Lee")
print(user.user_id, user.email, user.name)
roles = client.roles.list(per_page=10).items   # first page only (see below)
print([r.name for r in roles])
client.users.roles.assign(user.user_id, roles=[roles[0].id])
print(len(client.users.list_users_by_email(email="jo.lee@example.com")), "user(s) by email")
PY
```

The SDK uses `httpx`, which reads `HTTPS_PROXY` and `SSL_CERT_FILE` from the environment.
Read one page of a list (`.items`) rather than iterating the pager: the SDK asks for
pages until one comes back empty, and offline every page has items.

List calls answer the `include_totals=true` page shape (`{start, limit, length, total,
users|roles}`), which the SDKs always request; a plain `curl` without that parameter gets
the same shape, where Auth0 would return a bare array.

Drop `--offline` (and configure an LLM, see [the configuration reference](../../../docs/configuration.md#llm))
for answers that follow the persona: the shop's staff and customers, the four roles with
real descriptions, users-by-email that match the email asked for.

## Expected output

`--offline` answers come from the schema faker, so values differ run to run; `user_id`,
identities, Gravatar pictures and role ids follow Auth0's formats, and a create copies
`email` and `name` from the request (never the password):

```json
{
  "user_id": "auth0|5539848bb39deb9c5aec9e65",
  "email": "jo.lee@example.com",
  "email_verified": false,
  "name": "Jo Lee",
  "identities": [{"connection": "Username-Password-Authentication", "provider": "auth0", "user_id": "b14c7ab2e6e1a89f0bbd478b", ...}],
  "picture": "https://s.gravatar.com/avatar/4265bdac3c110ea4042d2461146c79a1?s=480&r=pg&d=https%3A%2F%2Fcdn.auth0.com%2Favatars%2Fdh.png",
  ...
}
```

The Python snippet prints lines like:

```
auth0|3eff4129ba49fcf750d00b9f jo.lee@example.com Jo Lee
['warehouse']
1 user(s) by email
```

## Scenarios

```bash
curl -s -i localhost:8080/api/v2/users -H 'Host: juniper.us.auth0.com' -H 'Authorization: Bearer test-token' \
  -H 'X-Mockingbird-Scenario: user-exists' -H 'content-type: application/json' \
  -d '{"connection":"Username-Password-Authentication","email":"jo.lee@example.com"}'
# HTTP/1.1 409 {"error":"Conflict","errorCode":"auth0_idp_error","message":"The user already exists.","statusCode":409}
```

`user-not-found` answers the by-id and role-assignment calls with 404
`{"statusCode":404,"error":"Not Found","message":"The user does not exist.","errorCode":"inexistent_user"}`,
and `rate-limited` answers 429 with `errorCode: too_many_requests`. The SDK raises its
`ApiError` subclasses (`ConflictError`, `NotFoundError`, `TooManyRequestsError`) for
them. Set `default_scenario` on the mock to apply one to every call.
