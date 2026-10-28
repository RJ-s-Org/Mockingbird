# Twilio SendGrid

A mock of the SendGrid v3 API for "Fern & Fable Books", an Edinburgh bookshop that sends
order confirmations and a newsletter: Mail Send (202 with an empty body, like the real
API), transactional templates, bounce suppressions and global stats, with Event Webhook
batches to your app.

| | |
| --- | --- |
| Contract | [`sendgrid.openapi.yaml`](sendgrid.openapi.yaml), a hand-written subset that keeps SendGrid's operation ids (`SendMail`, `ListTemplate`, ...) |
| Upstream | [twilio/sendgrid-oai](https://github.com/twilio/sendgrid-oai) (MIT), split into one file per product (`spec/json/tsg_mail_v3.json`, `tsg_templates_v3.json`, ...); the commit is recorded in [`example.json`](example.json), and there is no single file for `--full` to download |
| Hosts | `api.sendgrid.com` |
| Scenarios | `invalid-recipient` (400), `rate-limited` (429), `outage` (500) |
| Webhooks | `SendMail` Event Webhook batches to `http://127.0.0.1:4242/sendgrid/events`, signed `hmac-sha256` with `env:SENDGRID_WEBHOOK_SECRET` (not SendGrid's ECDSA signature, see below) |

## Quickstart

```bash
mockingbird examples init sendgrid --dir sendgrid && cd sendgrid   # or: cd examples/apis/sendgrid
mockingbird serve -c mockingbird.yaml --offline
```

In another shell:

```bash
curl -si localhost:8080/v3/mail/send -H 'Host: api.sendgrid.com' -H 'Authorization: Bearer SG.test' \
  -H 'content-type: application/json' \
  -d '{"personalizations":[{"to":[{"email":"ana@example.com"}]}],"from":{"email":"orders@fernandfable.example"},"subject":"Your order has shipped","content":[{"type":"text/plain","value":"It is on its way."}]}'
```

The official Python SDK, unmodified, through the proxy:

```bash
eval "$(mockingbird proxy env)"
pip install sendgrid
python - <<'PY'
import json
from sendgrid import SendGridAPIClient
from sendgrid.helpers.mail import Mail
sg = SendGridAPIClient("SG.test")
message = Mail(from_email="orders@fernandfable.example", to_emails="ana@example.com",
               subject="Your order #1042 has shipped", plain_text_content="It is on its way.")
resp = sg.send(message)
print(resp.status_code, repr(resp.body))
templates = sg.client.templates.get(query_params={"generations": "dynamic", "page_size": 20})
for t in json.loads(templates.body)["result"]:
    print(" ", t["id"], t["name"])
PY
```

Drop `--offline` (and configure an LLM, see [the configuration reference](../../../docs/configuration.md#llm))
for answers that follow the persona: the shop's template names, customer bounces with
SMTP reasons, plausible daily stats.

## Expected output

Mail Send answers `HTTP/1.1 202 Accepted` with no body. Other `--offline` answers come
from the schema faker, so values differ run to run; the shape does not:

```json
{
  "_metadata": {
    "count": 2,
    "self": "https://api.sendgrid.com/v3/templates?page_size=20"
  },
  "result": [
    {
      "generation": "dynamic",
      "id": "d-ed6ee18f94e140a3f34a561da1347048",
      "name": "quartz",
      "updated_at": "2026-10-27 14:29:56",
      "versions": [
        {
          "active": 1,
          "editor": "design",
          "template_id": "d-ccde227e5d7cd429668ff5a789252faa",
          ...
        }
      ]
    }
  ]
}
```

The Python snippet prints lines like:

```text
202 b''
  d-ed6ee18f94e140a3f34a561da1347048 quartz
```

## Scenarios

```bash
curl -s localhost:8080/v3/mail/send -H 'Host: api.sendgrid.com' -H 'Authorization: Bearer SG.test' \
  -H 'X-Mockingbird-Scenario: invalid-recipient' -H 'content-type: application/json' \
  -d '{"personalizations":[{"to":[{"email":"ana@example.com"}]}],"from":{"email":"orders@fernandfable.example"},"subject":"Hi","content":[{"type":"text/plain","value":"Hi"}]}'
# HTTP 400 {"errors":[{"field":"personalizations.0.to.0.email","help":"http://sendgrid.com/docs/API_Reference/Web_API_v3/Mail/errors.html#message.personalizations.to","message":"Does not contain a valid address."}],"id":"14d2920cb768a1c10dfbaac17af63e50"}
```

The SDK raises `python_http_client.exceptions.BadRequestsError` (status 400) for it, and
`TooManyRequestsError` for `rate-limited`. Set `default_scenario: invalid-recipient` on the
mock to apply it to every call.

## Webhooks

Each successful Mail Send posts an Event Webhook batch (a JSON array of `processed` and
`delivered` events with `email`, `timestamp`, `sg_event_id`, `sg_message_id`, `smtp-id`)
to the configured URL. Run your handler on port 4242 (or change `url`):

```bash
export SENDGRID_WEBHOOK_SECRET=whsec_test_local   # any value; the mock and your app share it
mockingbird serve -c mockingbird.yaml --offline
```

SendGrid signs real batches with ECDSA (`X-Twilio-Email-Event-Webhook-Signature` and
`X-Twilio-Email-Event-Webhook-Timestamp`, verified against your public key). Mockingbird
does not produce that signature; it sends `X-Mockingbird-Signature: sha256=<hex
HMAC-SHA256(secret, body)>`, so verify that instead (see [the webhooks guide](../../../docs/webhooks.md)).
`mockingbird webhooks deliveries sendgrid` lists the attempts.
