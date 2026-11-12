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
| F1 | Skills gap / proxy default | Requests to `verify.twilio.com` passed through to the real Twilio. The agent mocked only the hosts in the catalog example, and `proxy.unmapped: pass` sends every other host upstream. The CLI got real 401s (code 20003) for its placeholder account. `mockingbird report` lists the host under `unmapped_hosts`, but nothing made the agent look | **Fixed.** (a) The proxy now blocks an unmapped host that shares a mocked host's registrable domain, with a 502 problem+json naming the host and the fix (`proxy.vendor_subdomains: pass` turns this off); `report` lists it (this PR). (b) The main skill's hand-back checklist covers vendor hosts (#500). |
| F2 | Engine | An update sending form parameters under other names (`SmsUrl` for `sms_url`) fell back to the model with `undeclared_member`, even though the world's `set` placed every one: 3.0–3.4 s and 6.6–11.4k prompt tokens per update | **Fixed in #469**: a request member that a described `set` reads is no longer a fallback. In run 2, `UpdateIncomingPhoneNumber` came from state in 20 ms |
| F2b | Engine heuristic | Tier 0 classes a `POST` to the record's own path (`POST /Messages/{Sid}.json`, Stripe's `POST /v1/customers/{customer}`) as an action. Form APIs update that way, so every such update is a `no_effect` gap the agent has to describe, and an undescribed one goes to the model (`UpdateMessage`: 3.7 s) | Open; proposed. Class a `POST` whose last path segment is the record's parameter as an update when its body declares members of the record (names compared ignoring case and `_`). This changes classification for every API that updates by POST, so it needs the orchestrator's go |
| F3 | Default | A read of a SID the session never held is generated: 200 with an invented message. The skill says to set `unknown_ids: "404"` "when the app or its tests expect" a 404, but nothing in a one-line prompt says so. Stripe hit the same thing (F2 there) | Fixed: `unknown_ids` is inferred per type (see the third rerun) |
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

## Rerun on 2026-11-11: after the fixes

The rerun used `main` plus #483 and #484 (both merged since); the marketplace skills already included #479. A fresh agent got the same prompt in a clean copy of the app.

**Build.** 217 s, 37 turns, $1.88 of Claude Code usage. The agent loaded `mockingbird` and `mockingbird-world`, ran `doctor`, then `list_examples {category: messaging}` and `init_example twilio`. That gave it the example's **hand-written subset**: Messages, Calls and Lookups v2, with no IncomingPhoneNumbers. It then wrote world overlays and self-tested.

**What it still missed.** It never loaded `mockingbird-mock-api`, the skill where #479 put the coverage, `unknown_ids` and hand-back guidance. The catalog path (`init_example`) doesn't route through that skill. As a result:
- Verify wasn't mocked;
- `unknown_ids` stayed `generate`;
- the run instructions are still an `eval` of `mockingbird proxy env`.

**Flows: 8 of 21.**
- 8 of the 9 Messages checks pass, every one answered from state: create 24 ms, update 6 ms, no model call.
- The ninth, the unknown-SID read, is generated (2.6 s, F3 again).
- Phone numbers (3) aren't mocked by this agent's subset (404 from the mock).
- Verify (9) still reaches the real API.

| # | Measure | First run | Rerun |
| --- | --- | --- | --- |
| 1 | Flows | 11/21 | 8/21 (a smaller contract) |
| 2 | Consistency | 8/8 | 8/8 of what's mocked |
| 3 | Agent build | 195 s | 217 s |
| 4 | State latency | p50 8–20 ms | p50 7.5 ms (8 calls) |
| 5 | Model latency | 4–5 of 12 calls, p50 2.7–3.2 s | 1 of 12 calls (the unknown SID), 2.6 s |
| 6 | Model spend | $0.0006–0.0008 | about $0.0002 (3.8k prompt / 0.3k completion) |
| 7 | Errors | deleted 404, unknown 200 | the same |

**Fix in this PR.** The main `mockingbird` skill, which every agent loads, now ends "the whole job" with a hand-back checklist that applies to a catalog example as much as to an import:
- check coverage, using `init_example {full: true}` where the example has a full spec, and `mockingbird verify --no-unmapped --no-unmatched`;
- set `unknown_ids: 404` unless the app reads ids it didn't create;
- give the user `mockingbird run -c <config> -- <command>`.

The MCP server's instructions say the same in one sentence.

## Second rerun: the vendor-domain block (2026-11-11)

Built on `main` plus this PR. The skills came from the marketplace, so they include #500's hand-back checklist. A fresh agent got the same prompt in a clean copy of the app.

**Build.** 352 s, 40 turns, $1.84 of Claude Code usage. This time the agent:
- used `init_example twilio` with `full: true`, which gives the upstream Api v2010 spec, so phone numbers are mocked;
- described Message, Call and phone-number creates and lists;
- handed back `mockingbird run -c mockingbird/mockingbird.yaml -- ./bin/run …` (with the `eval` form only as an option, and a warning about `PATH`);
- named what it left out: "Commands for other Twilio hosts, like `api:messaging:v1:…`, `api:verify:…` or `serverless`, are blocked by the proxy".

It still didn't set `unknown_ids`.

**Flows (run under `mockingbird run`): 11 of 21.**
- Messages: 8 of 9. The unknown SID is still generated (F3).
- Phone numbers: 3 of 3.
- Verify: 0 of 9, but each call now gets the proxy's 502 problem+json instead of reaching `verify.twilio.com`. The CLI prints the detail: "verify.twilio.com is not served by any mock, but shares its domain with mocked api.twilio.com, lookups.twilio.com; it is blocked so test traffic does not reach the real vendor. Add …".

| # | Measure | First run | Second rerun |
| --- | --- | --- | --- |
| 1 | Flows | 11/21 (9 Verify calls reached the real Twilio) | 11/21 (9 Verify calls blocked locally) |
| 2 | Consistency | 8/8 | 8/8 |
| 3 | Agent build | 195 s | 352 s |
| 4 | State latency | p50 8–20 ms | p50 30 ms (7 calls, load average about 60) |
| 5 | Model latency | 5 of 12, p50 2.8 s | 5 of 12, p50 2.2 s, max 3.9 s |
| 6 | Model spend | $0.0008 | about $0.001 (29.8k prompt / 1.7k completion) |
| 7 | Errors | deleted 404, unknown 200 | the same; Verify gets a declared 502 problem |

**Still open.**
- **Form-named updates go to the model.** #483 now classes `UpdateMessage` as an update. But a form update sends `Body`/`SmsUrl` against a record holding `body`/`sms_url`, so with no world `set` it falls back with `undeclared_member` (2.2 s each). The fix: the update executor matches request members to record members ignoring case and `_`, as #483's classifier already does.
- **`unknown_ids` left at `generate`** (F3), although the checklist says to set it.

## Third rerun: unknown ids inferred per type (2026-11-11)

Same agent-built mock as the second rerun (no `unknown_ids` in its config), served by this PR's binary under `mockingbird run`. A type the contract can create answers an id the session never made with 404 once the session has written a record of that type (created, changed, deleted or seeded one). That fact is stored, so it holds after the record is deleted and across restarts.

**Flows: 11 of 21.** The tally is unchanged, but the failures moved:
- "messages fetch unknown sid is 404" now passes. The flow creates a message, deletes it, then reads `SMffff…`: 404, "Messages SMffff… does not exist in session default". An earlier build of this PR still generated here, because it counted only the records held at the time and the flow had deleted its only one.
- "messages create" now fails, on the flow's own check: the mock's sid starts with `MM`, which Twilio's sid pattern allows (`^(SM|MM)…`), and the flow wants `SM`.
- Verify stays 0 of 9: the agent didn't mock `verify.twilio.com`, so the proxy blocks it.

## Files

These are outside the repo, in `mockingbird-trials/twilio/`:
- `app/`: the CLI clone, with the agent's `mockingbird/` folder (config, world overlays, `env.sh`) and `.claude/settings.json` (the plugin);
- `trial/prompt.txt`: the prompt;
- `trial/agent.jsonl`: the agent's transcript;
- `trial/flows.py`: the checks;
- `trial/run{1,2}.txt` and `trial/run{1,2}-requests.jsonl`: the runs and their request logs (cumulative: ids 1–16 are the agent's self-test, 17–28 run 1, 29–40 run 2);
- `trial/run1-report.json`: `mockingbird report` after run 1;
- `app2/` and `trial2/`: the rerun's app copy (with the agent's `mock/` folder), transcript, flows and request log.
- `app3/` and `trial3/`: the second rerun's app copy (with the agent's `mockingbird/` folder), transcript, flows and request log.
