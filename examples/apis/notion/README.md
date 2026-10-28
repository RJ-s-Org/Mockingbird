# Notion

A mock of the Notion API for the "Ferngrove Studio" workspace, a Melbourne design
agency reached by its "Ferngrove Sync" integration: create, read and update pages, list
and append block children, query the "Launch Tracker" data source (and the older
database query), search, and the bot user. Pages created or updated through the mock
send Notion-shaped webhook events.

| | |
| --- | --- |
| Contract | [`notion.openapi.yaml`](notion.openapi.yaml), a hand-written subset that keeps Notion's operation ids |
| Upstream | Notion's own OpenAPI description, [`scripts/notion-openapi.json`](https://github.com/makenotion/notion-mcp-server/blob/main/scripts/notion-openapi.json) in makenotion/notion-mcp-server (MIT), pinned in [`example.json`](example.json); `mockingbird examples init notion --full` downloads it |
| Hosts | `api.notion.com` |
| Scenarios | `object-not-found` (404), `validation-error` (400), `rate-limited` (429), `outage` (503) |
| Webhooks | `page.created`, `page.updated` (sent as `page.properties_updated`) to `http://127.0.0.1:3000/notion/webhook`, signed `hmac-sha256` with `env:NOTION_WEBHOOK_SECRET` |

## Quickstart

```bash
mockingbird examples init notion --dir notion && cd notion   # or: cd examples/apis/notion
mockingbird serve -c mockingbird.yaml --offline
```

In another shell:

```bash
curl -s localhost:8080/v1/users/me -H 'Host: api.notion.com' \
  -H 'Authorization: Bearer ntn_test_123' -H 'Notion-Version: 2025-09-03'
```

The official Python SDK, unmodified, through the proxy (it uses httpx, which reads
`HTTPS_PROXY` and `SSL_CERT_FILE`):

```bash
eval "$(mockingbird proxy env)"
pip install notion-client
python - <<'PY'
from notion_client import Client
notion = Client(auth="ntn_test_123")
print(notion.users.me()["type"])
page = notion.pages.create(
    parent={"type": "data_source_id", "data_source_id": "248104cd-477e-80af-bc30-000bd28de8f9"},
    properties={"Name": {"title": [{"text": {"content": "Launch checklist"}}]}})
print(page["id"], page["url"])
rows = notion.data_sources.query("248104cd-477e-80af-bc30-000bd28de8f9",
    filter={"property": "Status", "status": {"equals": "In progress"}})
for r in rows["results"]:
    print(r["properties"]["Status"]["status"]["name"], r["properties"]["Name"]["title"][0]["plain_text"])
PY
```

notion-client 3.x sends `Notion-Version: 2025-09-03` and queries data sources; older
clients on `2022-06-28` call `POST /v1/databases/{id}/query` (`databases.query`), which
the contract also describes.

Drop `--offline` (and configure an LLM, see [the configuration reference](../../../docs/configuration.md#llm))
for answers that follow the persona: the title you sent on the created page, Launch
Tracker rows that match the filter, agency project names, and a `url` built from the
title and id.

## Expected output

`--offline` answers come from the schema faker, so values differ run to run and the
faker does not copy the request's nested `properties` into the page; the shape does not:

```json
{
  "object": "page",
  "id": "f04e4c7d-2a08-45de-a613-dda7d270665d",
  "created_time": "2006-04-26T12:53:16Z",
  "created_by": {"object": "user", "id": "c6b16aaa-48af-4c07-a733-0e57b1bfec4c"},
  "parent": {"type": "page_id", "page_id": "59833787-2cf9-4fdf-8782-e53db20768a5"},
  "archived": false,
  "in_trash": false,
  "properties": {
    "Name": {"id": "title", "type": "title", "title": [{"type": "text", "plain_text": "raven", ...}]},
    "Status": {"id": "Q%5Ex", "type": "status", "status": {"name": "In progress", "color": "red", ...}},
    ...
  },
  "url": "https://www.notion.so/Launch-checklist-251f498eb84f61e7f83ee0d2be0eab58",
  "public_url": null,
  "request_id": "4b76773b-c2f1-4aba-939d-a3d165cbc261",
  ...
}
```

The Python snippet prints lines like:

```
bot
3d1b03ca-da32-4011-a756-0f19b3ebae5e https://www.notion.so/Client-kickoff-c33f25c4e8dab00d60bfe45f2fb45162
In progress quartz
```

## Scenarios

```bash
curl -s localhost:8080/v1/pages/59833787-2cf9-4fdf-8782-e53db20768a5 -H 'Host: api.notion.com' \
  -H 'Authorization: Bearer ntn_test_123' -H 'Notion-Version: 2025-09-03' \
  -H 'X-Mockingbird-Scenario: object-not-found'
# HTTP 404 {"object":"error","status":404,"code":"object_not_found","message":"Could not find page with ID: ...","request_id":"..."}
```

The SDK raises `notion_client.APIResponseError` with `code` `APIErrorCode.ObjectNotFound`
for it, and `APIErrorCode.RateLimited` for `rate-limited` after its own retries (the
scenario sends no `Retry-After` header). `validation-error` applies to page creates and
updates, block appends and data source queries. Set `default_scenario: object-not-found`
on the mock to apply it to every call.

## Webhooks

Run your webhook handler on port 3000 (or change `url`) with the same secret the mock signs with:

```bash
export NOTION_WEBHOOK_SECRET=notion_whsec_test   # any value; the mock and your app share it
mockingbird serve -c mockingbird.yaml --offline
```

Each successful `POST /v1/pages` sends a `page.created` event and each
`PATCH /v1/pages/{id}` a `page.properties_updated` one, shaped like Notion's
(`workspace_id`, `subscription_id`, `integration_id`, `authors`, `entity`, `data.parent`).

Notion signs with `X-Notion-Signature: sha256=<hex HMAC-SHA256(verification_token, body)>`.
The mock computes the same HMAC but sends it as `X-Mockingbird-Signature`
([webhooks guide](../../../docs/webhooks.md#signatures)), so read that header when
testing against the mock. `mockingbird webhooks deliveries notion` lists the attempts.
