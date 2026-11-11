# Trial: healthchecks (Python, Django)

[healthchecks/healthchecks](https://github.com/healthchecks/healthchecks) at `f6c46b5`: a cron-job monitoring service (Django 6.1, about 9k stars). When a check goes down, `manage.py sendalerts` notifies every channel attached to it through the integration's HTTP API, with `pycurl`.

| | |
| --- | --- |
| APIs mocked | Twilio SMS and voice (`api.twilio.com`), GitHub App issues (`api.github.com`), Slack incoming webhooks (`hooks.slack.com`), Telegram Bot API (`api.telegram.org`), Opsgenie alerts (`api.opsgenie.com`) |
| Contracts | Twilio and GitHub REST from the catalog; GitHub's installation-token operation and the Slack, Telegram and Opsgenie contracts written for the trial |
| Model | OpenRouter `deepseek/deepseek-v4.1-flash`, default routing |
| Source edits | None. Settings come from environment variables, the data from `manage.py shell` |
| Result | 6/6 channels notified (`Notification.error` empty), 7 mock calls, 0 fallbacks, 0.4–2.2 s |

## Setup

Test settings (`mockingbird/env.sh`, no real credentials):

```bash
export DEBUG=True SITE_ROOT=http://localhost:8000 DB_NAME=$PWD/mockingbird/hc-trial.sqlite
export INTEGRATIONS_ALLOW_PRIVATE_IPS=True
export TWILIO_ACCOUNT=AC00000000000000000000000000000000 TWILIO_AUTH=test-auth TWILIO_FROM=+14155550199
export TELEGRAM_TOKEN=7012345678:AAtest-token TELEGRAM_BOT_NAME=HcAlertsBot
export GITHUB_CLIENT_ID=Iv1.test GITHUB_APP_ID=123456 GITHUB_PUBLIC_LINK=https://github.com/apps/hc-test
export GITHUB_PRIVATE_KEY="$(cat $PWD/mockingbird/github-app-test.pem)"   # openssl genrsa 2048
export OPSGENIE_ENABLED=True SLACK_ENABLED=True
```

`INTEGRATIONS_ALLOW_PRIVATE_IPS=True` is needed because healthchecks refuses to connect to private addresses (an SSRF guard in `hc/lib/curl.py` that checks the socket it opens), and through the proxy that socket is `127.0.0.1:42888`. Any app with a resolved-address SSRF guard will need the same allowance.

`seed.py` creates a user, a project, the check `nightly-backup` and one channel per integration (`sms`, `call`, `slack` with a `hooks.slack.com/services/…` URL, `telegram` with a chat id, `opsgenie`, `github` with installation 4242 and repo `acme/backups`); `go-down.py` makes the check overdue.

```yaml
server: {port: 42080, admin_port: 42090, grpc_port: 42051}
proxy: {enabled: true, port: 42888}
llm:
  default_profile: openrouter
  profiles:
    openrouter:
      api_key: env:OPENROUTER_API_KEY
      base_url: https://openrouter.ai/api/v1
      model: deepseek/deepseek-v4.1-flash
mocks:
  twilio:
    contract: twilio.openapi.yaml          # catalog subset
    route_prefix: /
    hosts: [api.twilio.com]
    persona: >-
      Twilio REST API (2010-04-01) for an uptime-monitoring service that texts and
      calls its on-call engineers from +14155550199. ...
  github:
    contract: github-rest.openapi.yaml     # catalog subset + POST /app/installations/{id}/access_tokens
    route_prefix: /
    hosts: [api.github.com]
    persona: >-
      GitHub.com REST API v3. Created issues copy title and body from the request, ...
  slack-webhooks:
    contract: slack-webhooks.openapi.yaml  # POST /services/{team}/{bot}/{secret} -> text/plain "ok"
    route_prefix: /
    hosts: [hooks.slack.com]
  telegram:
    contract: telegram.openapi.yaml        # POST /bot{token}/sendMessage
    route_prefix: /
    hosts: [api.telegram.org]
    persona: >-
      Telegram Bot API for the bot @HcAlertsBot ... chat.id is the request's chat_id ...
  opsgenie:
    contract: opsgenie.openapi.yaml        # POST /v2/alerts, /v2/alerts/{id}/close
    route_prefix: /
    hosts: [api.opsgenie.com]
```

```bash
.venv/bin/python manage.py migrate && .venv/bin/python manage.py shell < mockingbird/seed.py
.venv/bin/python manage.py shell < mockingbird/go-down.py
mockingbird run -c mockingbird/mockingbird.yaml -- .venv/bin/python manage.py sendalerts
```

## What happened

**pycurl went through the proxy with no help:** libcurl reads `https_proxy`, and the TLS handshake with the local CA succeeded with the bundle `proxy env` exports.

**Run 1: 4 of 6 channels.** Telegram got 400 and Opsgenie 422 from the mock, because the hand-written contracts were stricter than the real APIs: healthchecks sends `message_thread_id: null`, `parse_mode: "html"` (Telegram accepts any case) and numeric Opsgenie `details` values. The mock was right to reject what its contract forbids; the contracts were wrong. healthchecks then disabled the Telegram channel (a permanent error), so it had to be re-enabled for the next run. Two reporting issues showed up here: the rejected requests were logged as source `fallback` with no fallback reason, so `report` and `verify --no-fallbacks` count a client's invalid request as a generation failure.

**Run 2, contracts loosened: 6 of 6.** But the answers echoed request fields the response schemas do not have: Twilio's `Body`, `From`, `To`, `RiskCheck` and `StatusCallback` in request casing beside the schema's `body`/`from`/`to`, and Telegram's `chat_id` and `parse_mode` at the top level. That is the open "request fields are echoed" item from the catalog work.

**Fix (this PR):** a create or update copies only the request members the response schema declares (all of them when it declares none).

**Run 3, this PR's binary: 6 of 6, clean answers.** Twilio answers carry only schema fields (`sid`, `to`, `from`, `status`, `direction`, …), Telegram only `ok` and `result`.

| Mock | Calls | Sources | Latency |
| --- | --- | --- | --- |
| twilio | 2 (`CreateMessage`, `CreateCall`) | 2 `llm` | 0.9 s, 1.5 s |
| github | 2 (installation token, `issues/create`) | 1 `llm`, 1 `repaired` | 1.4 s, 2.2 s |
| slack-webhooks | 1 | 1 `llm` (answered without a model call: the only value is `"ok"`) | 0 ms |
| telegram | 1 | 1 `llm` | 1.1 s |
| opsgenie | 1 | 1 `llm` | 0.4 s |
| **Total** | **7** | **0 fallbacks** | **5.5k prompt / 1.3k completion tokens** |

`verify --no-fallbacks --no-unmapped` passes.

## Notes

- The GitHub integration authenticates as a GitHub App: it signs a JWT with `GITHUB_PRIVATE_KEY` and calls `POST /app/installations/{id}/access_tokens` before creating the issue. The catalog's GitHub REST subset has no Apps operations; the trial added that one.
- A single-value answer (Slack's literal `"ok"`) is labelled `llm` with 0 tokens and 0 ms. It should carry its own source.
- Created issues got realistic numbers (`#417`) and the title and body healthchecks sent.

## Holdout run (2026-11-11): built by the agent through the skills and MCP

`main` at `6c131052`, model `deepseek/deepseek-v4.1-flash-1017`. No source edits; same seed data (`mb-holdout/` in the trial checkout). The flows are the ones healthchecks runs:
- **Down:** the check goes overdue, and `sendalerts` notifies 6 channels.
- **Up:** a ping through the app's own `/ping/<code>` flips the check up, and `sendalerts` notifies the 4 channels that send "up" messages. Opsgenie's is a close by alias.

**How the agent built the mock:**
- **Twilio and GitHub REST** came from the catalog (`examples init`), with their hosts set in `mockingbird.yaml`. The catalog's GitHub subset now has the installation-token operation.
- **Slack incoming webhooks, Opsgenie and Telegram** are not in the catalog. They were imported with MCP `import_contract` from their documentation pages (`api.slack.com/messaging/webhooks`, `docs.opsgenie.com/docs/alert-api`, `core.telegram.org/bots/api`), reviewed with `get_draft`, approved, and given hosts with `update_mock`.
  - The imports took 6 s, 33 s and 3.5 min. They produced 1, 18 and 173 operations.

### Results

| Run | Down flow | Up flow | Notes |
| --- | --- | --- | --- |
| 1, contracts as imported | 3 of 6 (SMS, call, GitHub) | not run (a script bug in the trial) | Slack, Telegram and Opsgenie answered 400 with the declared error bodies |
| 2, after the agent loosened 3 contracts | **6 of 6** | **4 of 4** | `Notification.error` empty everywhere |

The app's own integration tests (`manage.py test hc.integrations.{sms,call,slack,telegram,opsgenie,github}`) pass: 164 tests. They patch `curl.request`, though, so none reaches the mock. healthchecks has no test that exercises the integrations over HTTP.

### Scorecard (run 2)

| # | Measure | Result |
| --- | --- | --- |
| 1 | Flows pass | 10 of 10 notifications, after the contract edits in failures 1–3 |
| 2 | Consistency the flows check | nothing to check: healthchecks never reads back what it sent. The Opsgenie close by alias answers 202 |
| 3 | Agent build | 16.8 min from an empty directory to the passing run 2, including 3.5 min waiting on the Telegram import and about 4 min fixing failures 1–3 |
| 4 | State-served latency | **p50 3.3 s** (4 creates). A create answered from state still makes its fill call |
| 5 | Model-served latency | p50 0.9 s, p95 2.4 s (5 calls) |
| 6 | Spend per session | $0.0009 for the run (13.5k prompt / 1.6k completion tokens). Doc-import spend is not recorded (failure 6) |
| 7 | Errors | invalid input gets the declared body: Twilio `{"code": 21211, …}` with 400, Slack `invalid_payload`. An unknown Twilio message SID gets 200 with an invented message (failure 7) |

**Sources:** 11 calls. 4 from `state`, 36% (the Twilio message, call and second message, and the GitHub issue). 5 from `llm` (Telegram ×2, Opsgenie create and close, GitHub installation token). 2 `fallback` (Slack's literal `ok`).

### Failures

| # | Repro (request → expected / got) | Class |
| --- | --- | --- |
| 1 | Slack `POST hooks.slack.com/services/T…/B…/…` with healthchecks' `{"attachments": [...]}` → expected 200 `ok` / got 400 `invalid_payload`. The doc import made `text` required (the page's examples all send it), and it did not declare `attachments`. | doc import (built-in model) too strict |
| 2 | Telegram `POST /bot<token>/sendMessage {"message_thread_id": null, …}` → expected 200 / got 400 `message_thread_id: null is not of type "integer"`. The docs say "Optional Integer"; the real API accepts `null`. | engine: validation treats `null` in an optional, non-nullable member as an error, though real APIs commonly accept it. Could be fixed generally for optional members |
| 3 | Opsgenie `POST /v2/alerts {"details": {"Total pings": 0}}` → expected 202 / got 400 `details/Total pings: 0 is not of type "string"`. The docs type details values as strings; the real API accepts numbers. | doc import (built-in model): the contract is stricter than the API |
| 4 | Fixing an active contract that came from a doc import: no MCP tool and no skill says how. `edit_draft` only works on drafts. The path that worked was `admin_api GET /mocks/<m>/contracts/1/source`, edit, then `import_contract {source, activate: true}`. | skills/MCP gap |
| 5 | The Telegram import holds 173 operations and gives 179 world gaps. Only 5 paths carry the `/bot{token}` prefix; the rest are `/<method>`. The skill says to work through every gap, which is impossible here; it should say to cover the operations the app calls. | doc import (path prefix), skills gap (gap triage) |
| 6 | Import jobs (`GET /mocks/<m>/imports`) and the request log carry no token usage for doc imports, so the spend of an agent build is invisible. | engine (reporting) |
| 7 | `GET api.twilio.com/2010-04-01/Accounts/AC…/Messages/SM000….json` (never created) → expected 404 / got 200 with an invented message. The default `unknown_ids: generate` applies. healthchecks never does this. | engine default (measure 7). Not blocking |
| 8 | Creates answered from `state` (Twilio messages and calls, GitHub issue) take 1.0–3.4 s, because the create's fill is a model call. Measure 4 (p50 < 50 ms) fails for write-heavy apps. | engine (latency) |
| 9 | Slack's literal `ok` answer is source `fallback` ("the response media type is not JSON: answered from the declared example"). `report` counts it as a fallback, so `verify --no-fallbacks` would fail on a correct answer. | engine (reporting) |

**Generalized from the dev-app fixes:**
- **Host-bound mocks:** worked through `update_mock` and the proxy with no `Host` work (#404).
- **Rejected requests:** now logged as `rejected` with their validation reason, no longer as `fallback`.
- **Creates read back from state:** worked for Twilio and GitHub (the record stored, filled once).
- **Token aliases and scoped lists (#415):** not exercised, because no read-backs.
