# Slack Web API

A mock of the Slack Web API for the "Lantern Labs" workspace and its `deploybot` bot:
`auth.test`, `chat.postMessage`, `conversations.list`/`history`, `users.info`/`list` and
`reactions.add`, with Slack's error convention (HTTP 200 and `{"ok": false, "error": ...}`,
except 429 for rate limits).

| | |
| --- | --- |
| Contract | [`slack.openapi.yaml`](slack.openapi.yaml), a hand-written subset that keeps Slack's operation ids (`chat_postMessage`, ...) and adds POST variants of the read methods, which the SDKs call with POST |
| Upstream | [slackapi/slack-api-specs](https://github.com/slackapi/slack-api-specs) `web-api/slack_web_openapi_v2.json` (MIT, archived in 2021), pinned in [`example.json`](example.json); `mockingbird examples init slack --full` downloads it |
| Hosts | `slack.com` (mounted at `/api`) |
| Scenarios | `channel-not-found` (HTTP 200, `ok: false`), `ratelimited` (429), `outage` (503) |
| Webhooks | none: Events API and interactivity payloads are signed `X-Slack-Signature: v0=<hex HMAC-SHA256("v0:<timestamp>:<body>")>` with `X-Slack-Request-Timestamp`, which Mockingbird does not produce |

## Quickstart

```bash
mockingbird examples init slack --dir slack && cd slack   # or: cd examples/apis/slack
mockingbird serve -c mockingbird.yaml --offline
```

In another shell:

```bash
curl -s localhost:8080/api/chat.postMessage -H 'Host: slack.com' -H 'Authorization: Bearer xoxb-test' \
  -H 'content-type: application/json' -d '{"channel":"C07DEPLOYS1","text":"Deploy of api v2.14.0 finished"}'
```

The official Python SDK, unmodified, through the proxy (`slack_sdk` reads `HTTPS_PROXY`):

```bash
eval "$(mockingbird proxy env)"
pip install slack_sdk
python - <<'PY'
from slack_sdk import WebClient
client = WebClient(token="xoxb-test")
me = client.auth_test()
print(me["team"], me["user"], me["user_id"])
for ch in client.conversations_list(limit=3)["channels"]:
    print(" ", ch["id"], "#" + ch["name"])
resp = client.chat_postMessage(channel="C07DEPLOYS1", text="Deploy of api v2.14.0 finished :rocket:")
print(resp["ok"], resp["channel"], resp["ts"])
client.reactions_add(channel="C07DEPLOYS1", name="white_check_mark", timestamp=resp["ts"])
PY
```

Drop `--offline` (and configure an LLM, see [the configuration reference](../../../docs/configuration.md#llm))
for answers that follow the persona: the message posted by `deploybot` with the text
you sent, real channel names and people.

## Expected output

`--offline` answers come from the schema faker, so values differ run to run; the shape
does not. The faker also echoes the JSON fields it was sent (`text`):

```json
{
  "channel": "C07DEPLOYS1",
  "message": {
    "bot_id": "B00J8XB7I3B",
    "subtype": "bot_message",
    "team": "T05L9YZ1TAX",
    "ts": "1726866486.245314",
    "type": "message",
    "user": "U0JIS68U6RY",
    ...
  },
  "ok": true,
  "text": "Deploy of api v2.14.0 finished",
  "ts": "1781300525.581924"
}
```

The Python snippet prints lines like:

```text
Lantern Labs deploybot U05V4DGPEGE
  C0MD8L29RFR #random
  C0S75GJTFYZ #general
True C07DEPLOYS1 1775502970.423789
```

## Scenarios

```bash
curl -s localhost:8080/api/chat.postMessage -H 'Host: slack.com' -H 'Authorization: Bearer xoxb-test' \
  -H 'X-Mockingbird-Scenario: channel-not-found' \
  -H 'content-type: application/json' -d '{"channel":"C0NOPE00000","text":"hi"}'
# HTTP 200 {"channel":"C0NOPE00000","error":"channel_not_found","ok":false,"text":"hi"}

curl -s localhost:8080/api/users.list -H 'Host: slack.com' -H 'Authorization: Bearer xoxb-test' \
  -H 'X-Mockingbird-Scenario: ratelimited'
# HTTP 429 {"error":"ratelimited","ok":false}
```

`slack_sdk` raises `SlackApiError` for both (`e.response["error"]` is `channel_not_found`,
`ratelimited`). The 429 carries no `Retry-After` header, so the SDK's
`RateLimitErrorRetryHandler` falls back to its default wait. `channel-not-found` keeps the
HTTP 200 answer and turns it into the error shape with a merge patch (`ok: false`, and
`null` to drop `message`, `messages` and `ts`); the 200 schemas are `anyOf` the success and
error shapes so the patched body still validates. Set `default_scenario: channel-not-found`
on the mock to apply it to every call.
