# Trial: Twilio CLI (holdout, messaging API)

This trial ran [twilio/twilio-cli](https://github.com/twilio/twilio-cli) at `d7e6d0d` (7.3.0) on 2026-11-11, against `main` at `0d591b1b` (#464). It is the first holdout trial on a messaging API.

| | |
| --- | --- |
| App | twilio-cli from source (`npm ci`, `./bin/run`), Node 26. It sends every request through twilio-node's HTTP client, which honours `HTTP_PROXY` |
| Flows | 21 CLI commands (`trial/flows.py`, below). They send, read, list, redact and delete SMS; buy, configure and list a phone number; create, update, list and delete a Verify v2 service; start and check a verification; and fetch unknown or deleted SIDs |
| Built by | A fresh headless Claude Code (`claude -p`, Opus 5.5 on the subscription) in the app's directory. The Mockingbird plugin was installed the way [agents.md](../agents.md) tells users to. The agent got one user prompt and no hints |
| Serving model | OpenRouter `deepseek/deepseek-v4.1-flash-1017`, `api_key: env:OPENROUTER_API_KEY` |
| Source edits | None, to either the CLI or Mockingbird |
| Result | **11 of 21 pass.** Messages and phone numbers pass 11 of 12. Verify passes 0 of 9, because the agent never mocked `verify.twilio.com` |

## The build

**Install.**
- The plugin went in as a user would add it, at project scope in the app: `claude plugin marketplace add rj-s-org/mockingbird` (already added on this machine), then `claude plugin install mockingbird@mockingbird --scope project`.
- The plugin's MCP launcher runs the first `mockingbird` on `PATH`, falling back to `~/.mockingbird/bin`. For the trial, `PATH` held a build of `main`.

**Prompt (verbatim):**

> Mock the Twilio API for this app with Mockingbird so I can run it locally without a Twilio account. Serve the endpoints with OpenRouter model deepseek/deepseek-v4.1-flash-1017; the key is in OPENROUTER_API_KEY. Ports 8080, 9090 and 8888 are already used by other things on this machine. When done, tell me how to run the CLI against the mock.

The port sentence describes this shared machine; it says nothing about Mockingbird.

**What the agent did:** 195 s, 28 turns, 24 tool calls, $1.66 of Claude Code usage.
- It loaded the `mockingbird`, `mockingbird-setup` and `mockingbird-world` skills and ran `doctor` over MCP.
- It read the CLI's HTTP client and found the `HTTP_PROXY` support.
- It found the catalog's `twilio` example and ran `examples init twilio --full`, which gives Twilio's full Api v2010 spec plus a hand-written Lookups v2 contract.
- It started the server on its own ports (18080, 19190 and 18898) and wrote world overlays:
  - `Messages`, `Calls` and `IncomingPhoneNumbers` creates mint SIDs as `'SM' + mint('[0-9a-f]{32}')`;
  - lists are scoped to the account and filtered by `To`, `From`, `Status`, `FriendlyName` and `PhoneNumber`;
  - `UpdateIncomingPhoneNumber` gets a `set` that maps each form parameter to its record member (`SmsUrl` to `sms_url`);
  - Lookups gets computed members.
- It self-tested with the CLI (16 requests), then wrote `mockingbird/env.sh`, which `eval`s `mockingbird proxy env` and sets placeholder credentials and a throwaway `TWILIO_CONFIG_DIR`.

**What it left out:**
- **Verify.** The catalog example covers `api.twilio.com` and `lookups.twilio.com`, and the agent mocked only those. Its summary says Verify, Messaging Services and other products "will go to the real hosts through the proxy and fail without real credentials", and offers to add them.
- **`UpdateMessage`.** Tier 0 classes `POST /Messages/{Sid}.json` as an action, and `world show --operation UpdateMessage` lists it as a `no_effect` gap. The agent never looked at it and didn't describe it.
- **`unknown_ids`.** The agent left it at the default, `generate`.

## Scorecard

Run 1 was on `main`. Run 2 was the same mock on a binary with #469.

| # | Measure | Target | Result |
| --- | --- | --- | --- |
| 1 | App flows pass | 100% of what the API serves | ✗ **11/21.** Messaging and numbers pass 11/12; the one failure is an unknown SID (measure 7). Verify passes 0/9 because the agent didn't mock it (F1) |
| 2 | Consistency | 100% of what the flows check | ✓ for the mocked products, 8 of 8 checks: a message create reads back by fetch and in the unfiltered and `To`-filtered lists; the redact (`Body=""`) answers and reads back empty; a deleted message answers 404; a phone number's update answers its new `SmsUrl`, and the list shows it |
| 3 | Agent build | one prompt, skills/MCP/CLI only, ≤ 15 min | ✓ 195 s, no hand edits. The install friction is under F4 |
| 4 | State latency | p50 < 50 ms | ✓ p50 8 ms in run 1 (7 calls) and 20 ms in run 2 (8 calls; the machine's load average was 50–70) |
| 5 | Model latency | p50 < 2 s, p95 < 5 s, rare | ✗ run 1: 5 of 12 calls went to the model, p50 2.8 s, max 3.4 s. Run 2: 4 of 12, p50 3.2 s, max 3.7 s (1.55, 2.67, 3.67, 3.69 s). The model calls: the two creates' fills, `UpdateMessage` (F2b), the unknown-SID read (F3), and in run 1 `UpdateIncomingPhoneNumber` (F2) |
| 6 | Model spend per session | < $0.05 | ✓ about $0.0008 in run 1 (22.8k prompt / 1.3k completion tokens) and $0.0006 in run 2 (17.1k / 0.9k), at OpenRouter's listed `-1017` price on 2026-11-11 ($0.006 per million prompt tokens, $0.50 per million completion tokens) |
| 7 | Errors | declared status and body | Partly. A deleted SID answers Twilio's 404. A SID that was never created answers 200 with a made-up message (`unknown_ids: generate`, F3) |

## Findings

| # | Class | Finding | Status |
| --- | --- | --- | --- |
| F1 | Skills gap / proxy default | Requests to `verify.twilio.com` passed through to the real Twilio. The agent mocked only the hosts in the catalog example, and `proxy.unmapped: pass` sends every other host upstream. The CLI got real 401s (code 20003) for its placeholder account. `mockingbird report` lists the host under `unmapped_hosts`, but nothing made the agent look | Open. Two options. (a) The setup skill tells the agent to cover each vendor host the app's SDK can call, or to say which hosts it left out. (b) The proxy blocks unmapped subdomains of a mapped host's registrable domain (`*.twilio.com` when `api.twilio.com` is mapped), so test traffic never reaches the vendor |
| F2 | Engine | An update sending form parameters under other names (`SmsUrl` for `sms_url`) fell back to the model with `undeclared_member`, even though the world's `set` placed every one: 3.0–3.4 s and 6.6–11.4k prompt tokens per update | **Fixed in #469**: a request member that a described `set` reads is no longer a fallback. In run 2, `UpdateIncomingPhoneNumber` came from state in 20 ms |
| F2b | Engine heuristic | Tier 0 classes a `POST` to the record's own path (`POST /Messages/{Sid}.json`, Stripe's `POST /v1/customers/{customer}`) as an action. Form APIs update that way, so every such update is a `no_effect` gap the agent has to describe, and an undescribed one goes to the model (`UpdateMessage`: 3.7 s) | Open; proposed. Class a `POST` whose last path segment is the record's parameter as an update when its body declares members of the record (names compared ignoring case and `_`). This changes classification for every API that updates by POST, so it needs the orchestrator's go |
| F3 | Default | A read of a SID the session never held is generated: 200 with an invented message. The skill says to set `unknown_ids: "404"` "when the app or its tests expect" a 404, but nothing in a one-line prompt says so. Stripe hit the same thing (F2 there) | Open; proposed. Either default `unknown_ids` to `404`, or have the mock-api skill set it unless the app reads fixtures it never created |
| F4 | Install friction | (a) The plugin's launcher runs any `mockingbird` it finds, on `PATH` or in `~/.mockingbird/bin`, without comparing its version with the skills. Here `~/.mockingbird/bin` held a 2026-10-28 build, which would have run silently under `main`'s skills had `PATH` not held a newer one. (b) The admin client subcommands (`world show`, `report`, `mocks`…) default to `http://127.0.0.1:9090` and don't read `server.admin_port` from `mockingbird.yaml`. On this machine 9090 is another session's server, so the agent had to export `MB_ADMIN_URL` for every command. (c) The agent's `env.sh` `eval`s `mockingbird proxy env`; when `mockingbird` isn't on `PATH`, the `eval` does nothing and the CLI talks to the real Twilio. That is how this trial's first flow run went (401s, placeholder credentials). `mockingbird run -- <cmd>` doesn't have this problem | Open. (a) has the launcher warn when the binary is older than the plugin. (b) has client subcommands take the admin URL from `-c`/`./mockingbird.yaml`. (c) has the setup skill suggest `mockingbird run --` for running the app, rather than a sourced env file |

## Flows

| Check | Run 1 | Run 2 |
| --- | --- | --- |
| messages create, fetch, list, list `--to` | ✓ | ✓ |
| messages update (redact), fetch after redact | ✓ (model, 3.0 s) | ✓ (model, 3.7 s: F2b) |
| messages remove, then fetch is 404 | ✓ | ✓ |
| messages fetch unknown SID is 404 | ✗ 200 | ✗ 200 |
| phone number create, update `--sms-url`, list shows the update | ✓ (update by model, 3.4 s) | ✓ (update from state, 20 ms) |
| Verify: service create/update/fetch/list/remove, verification start/fetch/check, 404 after remove | ✗ 0/9, sent to the real `verify.twilio.com` | ✗ 0/9 |

## Files

These are outside the repo, in `mockingbird-trials/twilio/`:
- `app/`: the CLI clone, with the agent's `mockingbird/` folder (config, world overlays, `env.sh`) and `.claude/settings.json` (the plugin);
- `trial/prompt.txt`: the prompt;
- `trial/agent.jsonl`: the agent's transcript;
- `trial/flows.py`: the checks;
- `trial/run{1,2}.txt` and `trial/run{1,2}-requests.jsonl`: the runs and their request logs (cumulative: ids 1–16 are the agent's self-test, 17–28 run 1, 29–40 run 2);
- `trial/run1-report.json`: `mockingbird report` after run 1.
