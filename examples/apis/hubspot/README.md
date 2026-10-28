# HubSpot CRM

A mock of the HubSpot CRM v3 objects API for "Brightwater Analytics", a Boston B2B
start-up: contacts (list, create, get, update, archive and search), companies and deals,
with HubSpot's error bodies (`category`, `correlationId`, a separate rate-limit shape with
`policyName`) and webhooks for new and changed contacts and new deals.

| | |
| --- | --- |
| Contract | [`hubspot.openapi.yaml`](hubspot.openapi.yaml), a hand-written subset that keeps HubSpot's operation ids |
| Upstream | None pinned: HubSpot's [public API specs](https://github.com/HubSpot/HubSpot-public-api-spec-collection) are proprietary (see `upstream.notes` in [`example.json`](example.json)) |
| Hosts | `api.hubapi.com` |
| Scenarios | `contact-exists` (409), `invalid-email` (400), `rate-limited` (429), `outage` (503) |
| Webhooks | `contact.created`, `contact.updated`, `deal.created` to `http://127.0.0.1:3000/hubspot/webhook`, signed `hmac-sha256` with `env:HUBSPOT_WEBHOOK_SECRET` |

## Quickstart

```bash
mockingbird examples init hubspot --dir hubspot && cd hubspot   # or: cd examples/apis/hubspot
mockingbird serve -c mockingbird.yaml --offline
```

In another shell:

```bash
curl -s localhost:8080/crm/v3/objects/contacts -H 'Host: api.hubapi.com' \
  -H 'Authorization: Bearer pat-na1-test' -H 'content-type: application/json' \
  -d '{"properties":{"email":"dana.whitfield@example.com","firstname":"Dana","lastname":"Whitfield"}}'
```

The official Python SDK, unmodified, through the proxy. Its generated clients use
urllib3, which does not read `HTTPS_PROXY`, so the snippet passes the proxy and CA
bundle as client options:

```bash
eval "$(mockingbird proxy env)"
pip install hubspot-api-client
python - <<'PY'
import os
from hubspot import HubSpot
from hubspot.crm.contacts import SimplePublicObjectInputForCreate, PublicObjectSearchRequest
api = HubSpot(access_token="pat-na1-test",
              proxy=os.environ["HTTPS_PROXY"], ssl_ca_cert=os.environ["SSL_CERT_FILE"])
c = api.crm.contacts.basic_api.create(SimplePublicObjectInputForCreate(
    properties={"email": "dana.whitfield@example.com", "firstname": "Dana", "lastname": "Whitfield"}))
print(c.id, c.properties["email"], c.created_at)
r = api.crm.contacts.search_api.do_search(PublicObjectSearchRequest(
    filter_groups=[{"filters": [{"propertyName": "lifecyclestage", "operator": "EQ", "value": "customer"}]}], limit=5))
print(r.total, [x.id for x in r.results])
page = api.crm.deals.basic_api.get_page(limit=3)
print([(d.properties["dealname"], d.properties["amount"]) for d in page.results])
PY
```

Drop `--offline` (and configure an LLM, see [the configuration reference](../../../docs/configuration.md#llm))
for answers that follow the persona: the properties you sent echoed back on create,
`hs_object_id` equal to `id`, coherent timestamps, US mid-market buyers and
"<Company> - Analytics Pro" deals.

## Expected output

`--offline` answers come from the schema faker, so values differ run to run and the
faker does not copy the request's nested `properties` into the answer; the shape does not:

```json
{
  "id": "78617271",
  "properties": {
    "createdate": "2023-12-05T19:49:42Z",
    "email": "delta@example.com",
    "firstname": "delta",
    "lastname": "nectar",
    "hs_object_id": "387484455",
    "lastmodifieddate": "2028-07-01T00:19:46Z",
    "lifecyclestage": "subscriber",
    "phone": "+1 617 555 0916",
    ...
  },
  "createdAt": "2018-09-01T08:23:27Z",
  "updatedAt": "2011-02-21T20:42:06Z",
  "archived": false,
  ...
}
```

The Python snippet prints lines like:

```
52212 onyx@example.com 2022-06-08 01:18:57+00:00
82 ['625133387']
[('quartz', '9205.00')]
```

## Scenarios

```bash
curl -s localhost:8080/crm/v3/objects/contacts -H 'Host: api.hubapi.com' \
  -H 'Authorization: Bearer pat-na1-test' -H 'content-type: application/json' \
  -H 'X-Mockingbird-Scenario: contact-exists' -d '{"properties":{"email":"dana.whitfield@example.com"}}'
# HTTP 409 {"status":"error","message":"Contact already exists. Existing ID: 74981","category":"CONFLICT","correlationId":"a34713ef-...",...}
```

The SDK raises `hubspot.crm.contacts.ApiException` with `status` 409 for it, and 400 for
`invalid-email`. `rate-limited` answers HubSpot's rate-limit body
(`{"status":"error","errorType":"RATE_LIMIT","policyName":"TEN_SECONDLY_ROLLING",...}`)
without the `X-HubSpot-RateLimit-*` headers. Set `default_scenario: contact-exists` on
the mock to apply it to every call.

## Webhooks

Run your webhook handler on port 3000 (or change `url`) with the same secret the mock signs with:

```bash
export HUBSPOT_WEBHOOK_SECRET=hub_whsec_test   # any value; the mock and your app share it
mockingbird serve -c mockingbird.yaml --offline
```

Each successful contact create or update and each deal create POSTs a one-event array
shaped like HubSpot's (`subscriptionType` `contact.creation`, `contact.propertyChange` or
`deal.creation`, `portalId`, `objectId`, `occurredAt`, ...).

HubSpot signs with `X-HubSpot-Signature-v3` (base64 HMAC-SHA256 over method, URI, body
and `X-HubSpot-Request-Timestamp`), which Mockingbird does not produce: the mock signs
with `X-Mockingbird-Signature: sha256=<hex HMAC-SHA256(secret, raw body)>`
([webhooks guide](../../../docs/webhooks.md#signatures)), so verify that header when
testing against the mock. `mockingbird webhooks deliveries hubspot` lists the attempts.
