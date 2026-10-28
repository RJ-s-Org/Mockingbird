# Twilio

A mock of the Twilio REST API for "Harbor Lane Dental", a dental practice that texts
and calls patients with appointment reminders: send, list and fetch SMS, place calls
(form-encoded, like the real API), look up phone numbers on `lookups.twilio.com`, and
status callbacks to your app.

| | |
| --- | --- |
| Contracts | [`twilio.openapi.yaml`](twilio.openapi.yaml) (Api 2010-04-01) and [`twilio-lookups.openapi.yaml`](twilio-lookups.openapi.yaml) (Lookups v2), hand-written subsets that keep Twilio's operation ids |
| Upstream | [twilio/twilio-oai](https://github.com/twilio/twilio-oai) `spec/json/twilio_api_v2010.json` (MIT), pinned in [`example.json`](example.json); `mockingbird examples init twilio --full` downloads it |
| Hosts | `api.twilio.com`, `lookups.twilio.com` |
| Scenarios | `invalid-to-number` (400, error 21211), `rate-limited` (429, error 20429), `outage` (503, error 20503) |
| Webhooks | `CreateMessage`, `CreateCall` status callbacks to `http://127.0.0.1:4242/twilio/status`, signed `hmac-sha256` with `env:TWILIO_WEBHOOK_SECRET` (not `X-Twilio-Signature`, see below) |

## Quickstart

```bash
mockingbird examples init twilio --dir twilio && cd twilio   # or: cd examples/apis/twilio
mockingbird serve -c mockingbird.yaml --offline
```

In another shell:

```bash
curl -s localhost:8080/2010-04-01/Accounts/ACaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa/Messages.json \
  -H 'Host: api.twilio.com' -u ACaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa:test_token \
  --data-urlencode To=+15558675310 --data-urlencode From=+14015550142 \
  --data-urlencode "Body=Reminder: cleaning Thu Oct 30, 9:30 AM"
```

The official Python SDK, unmodified, through the proxy (it reaches both hosts):

```bash
eval "$(mockingbird proxy env)"
pip install twilio
python - <<'PY'
from twilio.rest import Client
client = Client("ACaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa", "test_token")
msg = client.messages.create(to="+15558675310", from_="+14015550142",
                             body="Harbor Lane Dental: cleaning Thu Oct 30, 9:30 AM. Reply C to confirm.")
print(msg.sid, msg.status, msg.date_created)
for m in client.messages.list(limit=2):
    print(" ", m.sid, m.status)
call = client.calls.create(to="+15558675310", from_="+14015550142", url="http://demo.twilio.com/docs/voice.xml")
print(call.sid, call.status)
number = client.lookups.v2.phone_numbers("+14015550199").fetch(fields="line_type_intelligence")
print(number.phone_number, number.valid, number.line_type_intelligence["type"])
PY
```

Drop `--offline` (and configure an LLM, see [the configuration reference](../../../docs/configuration.md#llm))
for answers that follow the persona: `to`, `from` and `body` copied from the request,
the account SID from the path, reminder texts, a `queued` status.

## Expected output

`--offline` answers come from the schema faker, so values differ run to run; the shape
does not. The faker also echoes the form fields it was sent (`To`, `From`, `Body`):

```json
{
  "Body": "Reminder: cleaning Thu Oct 30, 9:30 AM",
  "From": "+14015550142",
  "To": "+15558675310",
  "account_sid": "ACaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
  "api_version": "2010-04-01",
  "date_created": "Fri, 15 Oct 2026 10:34:23 +0000",
  "date_sent": null,
  "direction": "outbound-call",
  "error_code": null,
  "num_segments": "3",
  "price": "-0.06237",
  "price_unit": "USD",
  "sid": "SM2080c57694f2c510eda219971553b3ee",
  "status": "queued",
  "uri": "/2010-04-01/Accounts/ACaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa/Messages/SMa9edb8d07a266fe75deb1c7713f0c4ce.json",
  ...
}
```

The Python snippet prints lines like:

```text
SM08b09610afc9ae164e8b4d705d3feb22 received 2026-10-24 15:27:36+00:00
  SM2f5bb92e3324f014ac3312e9cc15fde0 sending
CA4092820b04b753743f5440fbc86d9a97 failed
+14395554017 False pager
```

## Scenarios

```bash
curl -s localhost:8080/2010-04-01/Accounts/ACaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa/Messages.json \
  -H 'Host: api.twilio.com' -u ACaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa:test_token \
  -H 'X-Mockingbird-Scenario: invalid-to-number' --data-urlencode To=+1555123 -d Body=Hi
# HTTP 400 {"code":21211,"message":"Invalid 'To' Phone Number: +1555123","more_info":"https://www.twilio.com/docs/errors/21211","status":400}
```

The SDK raises `TwilioRestException` with `status` 400 and `code` 21211 for it (and 20429
for `rate-limited`). Set `default_scenario: invalid-to-number` on the mock to apply it to
every call. The `lookups.twilio.com` mock has its own `rate-limited` scenario.

## Webhooks

Twilio POSTs each status change of a message or call to its `StatusCallback` URL. The
mock sends one callback per `POST .../Messages.json` and `POST .../Calls.json` to
the configured URL; run your handler on port 4242 (or change `url`):

```bash
export TWILIO_WEBHOOK_SECRET=whsec_test_local   # any value; the mock and your app share it
mockingbird serve -c mockingbird.yaml --offline
```

The body carries Twilio's callback fields (`MessageSid`, `MessageStatus`, `AccountSid`,
`From`, `To`, ...; `CallSid`, `CallStatus`, ... for calls). Two differences from the real
thing: the fields arrive as JSON rather than `application/x-www-form-urlencoded`, and the
signature is `X-Mockingbird-Signature: sha256=<hex HMAC-SHA256(secret, body)>`. Twilio's
`X-Twilio-Signature` (base64 HMAC-SHA1 of the URL plus the sorted form parameters,
keyed with the auth token) is not produced, so `twilio.request_validator.RequestValidator`
does not accept these callbacks; verify the HMAC instead (see
[the webhooks guide](../../../docs/webhooks.md)). `mockingbird webhooks deliveries twilio`
lists the attempts.
