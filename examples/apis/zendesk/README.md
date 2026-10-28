# Zendesk Support

A mock of the Zendesk Support API for "Juniper & Co", a small plant shop's help desk:
tickets (create, list, show, update), ticket comments, users, the current user and
search, plus ticket webhooks.

| | |
| --- | --- |
| Contract | [`zendesk.openapi.yaml`](zendesk.openapi.yaml), a hand-written subset that keeps Zendesk's operation ids (`ListTickets`, `CreateTicket`, `ShowTicket`, `UpdateTicket`, `ListTicketComments`, `ListUsers`, `CreateUser`, `ShowUser`, `ShowCurrentUser`, `ListSearchResults`); each path also answers with the `.json` suffix (ids ending in `Json`), as Zendesk does |
| Upstream | Zendesk's [`oas.yaml`](https://developer.zendesk.com/zendesk/oas.yaml), under the Zendesk Developer Terms, so not pinned (see [`example.json`](example.json)) |
| Hosts | `juniper.zendesk.com` |
| Scenarios | `rate-limited` (429), `not-found` (404 `RecordNotFound`), `record-invalid` (422 `RecordInvalid`), `outage` (503) |
| Webhooks | `zen:event-type:ticket.created` and ticket-updated events to `http://127.0.0.1:4242/zendesk/webhook`, signed `hmac-sha256` with `env:ZENDESK_WEBHOOK_SECRET` (not Zendesk's own scheme, see below) |

## Quickstart

```bash
mockingbird examples init zendesk --dir zendesk && cd zendesk   # or: cd examples/apis/zendesk
mockingbird serve -c mockingbird.yaml --offline
```

In another shell:

```bash
curl -s localhost:8080/api/v2/tickets -H 'Host: juniper.zendesk.com' -u 'ana@example.com/token:test-token' \
  -H 'content-type: application/json' \
  -d '{"ticket":{"subject":"My monstera arrived with yellow leaves","priority":"high","comment":{"body":"Order #10432 came today and two leaves are yellow."}}}'
```

The Python client [`zenpy`](https://pypi.org/project/zenpy/) (community-maintained; Zendesk
ships no official Python SDK), unmodified, through the proxy:

```bash
eval "$(mockingbird proxy env)"
pip install zenpy
python - <<'PY'
from zenpy import Zenpy
from zenpy.lib.api_objects import Comment, Ticket

zenpy = Zenpy(subdomain="juniper", email="ana@example.com", token="test-token")
audit = zenpy.tickets.create(Ticket(subject="My monstera arrived with yellow leaves",
                                    priority="high",
                                    comment=Comment(body="Order #10432 came today with two yellow leaves.")))
ticket = audit.ticket
print(ticket.id, ticket.status, ticket.priority, "|", ticket.subject)
for t in zenpy.search(type="ticket", status="open"):
    print("open:", t.id, t.subject)
    break
print(zenpy.users.me().name)
PY
```

zenpy calls the `.json` paths (`/api/v2/tickets.json`, `/api/v2/search.json`); `requests`
picks up the proxy and CA bundle from the environment.

Drop `--offline` (and configure an LLM, see [the configuration reference](../../../docs/configuration.md#llm))
for answers that follow the persona: plant-order subjects, the Customer Care agents, the
subject and priority you sent.

## Expected output

`--offline` answers come from the schema faker, so values differ run to run; ids, urls,
statuses and the `{ticket, audit}` envelope follow Zendesk's formats:

```json
{
  "ticket": {
    "id": 3186,
    "url": "https://juniper.zendesk.com/api/v2/tickets/2796.json",
    "subject": "cedar",
    "status": "open",
    "priority": "low",
    "requester_id": 360000371441,
    "via": {"channel": "chat", ...},
    ...
  },
  "audit": {"id": 136995, "events": [{"type": "Change", ...}], ...}
}
```

Offline, the ticket envelope is not filled from the request (the subject is generated)
and the `url` id differs from `id`; with an LLM both follow the request and the persona.

The Python snippet prints lines like:

```
8921 new None | delta
open: 5060 bravo
cedar
```

## Scenarios

```bash
curl -s -i localhost:8080/api/v2/tickets/1042 -H 'Host: juniper.zendesk.com' \
  -H 'X-Mockingbird-Scenario: not-found'
# HTTP/1.1 404 {"description":"Not found","details":{},"error":"RecordNotFound"}
```

`record-invalid` answers creates and updates with 422
`{"error":"RecordInvalid","description":"Record validation errors","details":{"requester":[{"description":"Requester: Email is not properly formatted","error":"InvalidValue"}]}}`.
zenpy raises `RecordNotFoundException` for `not-found` and `APIException` for the others
(its 429 retry waits on a `Retry-After` header, which the scenario does not send). Set
`default_scenario` on the mock to apply one to every call.

## Webhooks

Run your webhook handler on port 4242 (or change `url`) with the secret the mock signs with:

```bash
export ZENDESK_WEBHOOK_SECRET=zd_test_local   # any value; the mock and your app share it
mockingbird serve -c mockingbird.yaml --offline
```

Each successful create sends a `zen:event-type:ticket.created` event and each update a
ticket event (`ticket.status_changed`, `ticket.priority_changed`, `ticket.comment_added`),
shaped like Zendesk's event-subscribed webhooks (`account_id`, `subject: zen:ticket:<id>`,
`detail`, `event`).

Zendesk signs with `X-Zendesk-Webhook-Signature`, a base64 HMAC-SHA256 of
`X-Zendesk-Webhook-Signature-Timestamp` + body. Mockingbird cannot produce that header yet,
so these events carry `X-Mockingbird-Signature: sha256=<hex HMAC-SHA256(secret, body)>`;
verify that header in tests, or skip verification against the mock.
`mockingbird webhooks deliveries zendesk` lists the attempts.
