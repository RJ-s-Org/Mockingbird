# Discord

A mock of the Discord HTTP API v10 as seen by "Quillbot", the helper bot of a writers'
community server: the bot's own user, the guild and its channels, messages (list, get,
create, delete) and interaction responses.

| | |
| --- | --- |
| Contract | [`discord.openapi.yaml`](discord.openapi.yaml), a hand-written subset that keeps Discord's operation ids (`get_my_user`, `create_message`, ...) |
| Upstream | [discord/discord-api-spec](https://github.com/discord/discord-api-spec) `specs/openapi.json` (MIT), pinned in [`example.json`](example.json); `mockingbird examples init discord --full` downloads it |
| Hosts | `discord.com` (mounted at `/api/v10`) |
| Scenarios | `unknown-channel` (404, code 10003), `rate-limited` (429), `outage` (502) |
| Webhooks | none: Discord delivers interactions to your endpoint signed with Ed25519 (`X-Signature-Ed25519`, `X-Signature-Timestamp`), which Mockingbird does not produce; gateway events are a WebSocket |

## Quickstart

```bash
mockingbird examples init discord --dir discord && cd discord   # or: cd examples/apis/discord
mockingbird serve -c mockingbird.yaml --offline
```

In another shell:

```bash
curl -s localhost:8080/api/v10/users/@me -H 'Host: discord.com' -H 'Authorization: Bot test-token'
```

Discord has no official Python SDK, and `discord.py` is built around the gateway, so this
uses `httpx` against the REST API, unmodified, through the proxy:

```bash
eval "$(mockingbird proxy env)"
pip install httpx
python - <<'PY'
import httpx
api = httpx.Client(base_url="https://discord.com/api/v10",
                   headers={"Authorization": "Bot test-token", "User-Agent": "DiscordBot (https://example.com, 1.0)"})
me = api.get("/users/@me").json()
print(me["username"], me["id"], me["bot"])
for ch in api.get("/guilds/1170000000000000000/channels").json()[:3]:
    print(" ", ch["id"], ch["type"], ch["name"])
msg = api.post("/channels/1180000000000000001/messages", json={"content": "Weekly sprint starts in 10 minutes!"}).json()
print(msg["id"], msg["content"])
PY
```

Drop `--offline` (and configure an LLM, see [the configuration reference](../../../docs/configuration.md#llm))
for answers that follow the persona: the channel id from the path, Quillbot as the author,
members and messages of a writing community.

## Expected output

`--offline` answers come from the schema faker, so values differ run to run; the shape
does not:

```json
{
  "avatar": "151f0791b68c32f05aebfef48c1dbdf6",
  "bot": true,
  "discriminator": "0",
  "email": null,
  "flags": 524288,
  "global_name": null,
  "id": "1895545262159003263",
  "locale": "en-GB",
  "mfa_enabled": false,
  "public_flags": 524288,
  "username": "Quillbot",
  "verified": true
}
```

The Python snippet prints lines like:

```text
Quillbot 1895545262159003263 True
  147621995365687766 2 help
  138614552030331140 0 events
  111027181812666226 4 welcome
1781194697751966726 Weekly sprint starts in 10 minutes!
```

## Scenarios

```bash
curl -s localhost:8080/api/v10/channels/1180000000000000001/messages -H 'Host: discord.com' \
  -H 'Authorization: Bot test-token' -H 'X-Mockingbird-Scenario: unknown-channel'
# HTTP 404 {"code":10003,"errors":{},"message":"Unknown Channel"}

curl -s localhost:8080/api/v10/channels/1180000000000000001/messages -H 'Host: discord.com' \
  -H 'Authorization: Bot test-token' -H 'X-Mockingbird-Scenario: rate-limited' \
  -H 'content-type: application/json' -d '{"content":"hi"}'
# HTTP 429 {"code":0,"global":false,"message":"You are being rate limited.","retry_after":1.337}
```

The 429 body has `retry_after`, but no `Retry-After` or `X-RateLimit-*` headers are sent,
so a client that reads the headers (as `@discordjs/rest` does) sees none. Set
`default_scenario: rate-limited` on the mock to apply it to every call.
