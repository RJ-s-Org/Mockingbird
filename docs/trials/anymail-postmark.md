# Trial: django-anymail on Postmark (holdout, transactional email)

This trial ran [anymail/django-anymail](https://github.com/anymail/django-anymail) on 2026-11-11, cloned from `main`. It was the first holdout trial on a transactional email API. Anymail sends Django's email through each provider's HTTP API, and its live integration tests (`tests/test_postmark_integration.py`) call the real Postmark API.

| | |
| --- | --- |
| App | django-anymail, Django 6.0 and `requests`, in a `uv` venv. The 5 Postmark live integration tests cover a plain send, a batch send with every option, a template batch send, an invalid `From` (expects 422 and `Invalid 'From' address`), and an invalid server token (expects an error matching `valid.*token`) |
| Baseline | Against the real API with Postmark's sandbox token, 4 tests pass. The template test is skipped because it needs a real server token and template id |
| Built by | A fresh headless Claude Code (`claude -p`, Sonnet 5.5 on the subscription) in the app's directory. The Mockingbird plugin was installed the way [agents.md](../agents.md) tells users to. The agent got one user prompt and no hints |
| Serving model | OpenRouter `deepseek/deepseek-v4.1-flash-1017`, `api_key: env:OPENROUTER_API_KEY` |
| Source edits | None to Anymail. Two Mockingbird fixes came out of the trial (below) |
| Result | **5 of 5 pass** on three runs in a row, including the template test, which the real sandbox can't run. Before the fixes, 4 of 5 passed: the invalid-token test got 200 |

## The build

**Install.**
- The plugin was added at local scope in the app: `claude plugin marketplace add rj-s-org/mockingbird --scope local`, then `claude plugin install mockingbird@mockingbird --scope local`. Both took about 1 s.
- `claude mcp list` showed `plugin:mockingbird:mockingbird` connected through the plugin's `mockingbird-mcp` launcher.
- For the trial, `PATH` held a build of `main`. The launcher runs the first `mockingbird` on `PATH`.

**Install friction (findings):**
- **The skills and the binary can come from different versions.** The plugin's skills come from `main`, but a machine that ran `install.sh` earlier keeps that release. On this machine that was `~/.mockingbird/bin/mockingbird` 0.1.0-rc.1, which the launcher falls back to without a word.
- **Updates can be missed.** `claude plugin update` said "already at the latest version (0.1.0)" after the marketplace moved, because the plugin's version stays at the workspace version while its skills change. Here the cache did match `main`, but a user can't tell.

**Prompt (verbatim):**

> Mock the Postmark API for this app so its Postmark integration tests run against the mock. Serve the endpoints with OpenRouter deepseek/deepseek-v4.1-flash-1017 (the key is in OPENROUTER_API_KEY).

**What the agent did:** 366 s, 38 turns, $0.76 of Claude Code usage.
- **Skills and tools:** it loaded the `mockingbird`, `mockingbird-mock-api` and `mockingbird-world` skills. Over MCP it ran `doctor` and `list_llm_providers`, and `start_server` twice. Everything else went through the CLI.
- **Contract:** Postmark publishes no OpenAPI document, so the agent wrote a 121-line contract from Anymail's backend. It covers `/email`, `/email/withTemplate/`, `/email/batch`, `/email/batchWithTemplates` and `/email/bulk`, plus the `X-Postmark-Server-Token` apiKey scheme, declared as a header parameter too.
- **World:** two `refuse` entries on `sendEmail`. A token that isn't `POSTMARK_API_TEST` or 36 characters gets 401 with `ErrorCode` 10. A `From` ending in `@localhost` gets 422 with `ErrorCode` 300.
- **Config:** a persona stating Postmark's error codes; `hosts: [api.postmarkapp.com]`; and its own ports (8181, 9191, 8989), because another server held the defaults.
- **Running the tests:** it ran the tests through `mockingbird proxy env`. It reported 3 of 4 passing (the template test was skipped for lack of environment variables), and correctly blamed Mockingbird for the token test: `request.headers['x-postmark-server-token']` was always null.

## Fixes

| Failure | Class | Fix |
| --- | --- | --- |
| `test_invalid_server_token` got 200. The world's `refuse` on the token header never fired | Engine: credential headers (names with `token`, `key`, `auth`, or the contract's apiKey headers) were withheld from world expressions as from the model, so `request.headers[...]` was null | [#470](https://github.com/RJ-s-Org/Mockingbird/pull/470): expressions read a call's credential headers, which still never reach the model, the request record or the cache key. An expression failure no longer quotes a value |
| The first 401 and 422 each took 19–20 s on a slow provider | Engine: a refusal stating its `code` and `message` still asked the model to write the error body, then overwrote both | [#478](https://github.com/RJ-s-Org/Mockingbird/pull/478): such a refusal answers with the faker's body carrying them |

## Scorecard (rerun on `main` with #470 and #478, fresh server)

| # | Measure | Result | Meets bar? |
| --- | --- | --- | --- |
| 1 | App tests and flows | 5/5 on 3 consecutive runs | **Yes** |
| 2 | Consistency | Nothing reads back: Postmark's send API stores nothing the tests fetch. Batch sends answer a distinct `MessageID` per recipient, which `test_all_options` checks | n/a |
| 3 | Agent build | One prompt, 366 s, skills, MCP and CLI only | **Yes** |
| 4 | State latency | p50 0 ms, max 1 ms (n=6: the 401s and 422s) | **Yes** |
| 5 | Model latency | p50 0.92 s, p95 1.9 s (n=9). 9 of 15 calls reached the model: every send generates, because the agent's world gave messages no identity | Latency **yes**, "rare" **no** |
| 6 | Model spend per session | 5 calls, about 3.8k prompt and 0.4k completion tokens, about $0.0002 | **Yes** |
| 7 | Errors | 422 with `ErrorCode` 300 and the declared message; 401 with `ErrorCode` 10 and the declared message | **Yes** (after #470) |

**Before the fixes (same mock, the binary the agent used):**
- 4/5 passed.
- On a fresh server with #470 only, one run hit a 20 s LLM timeout on the batch send and fell back to the faker. Its body broke `test_all_options` on the first run. The first refusals waited 19–20 s.
- Those runs measured model p50 3.1 s and p95 16.7 s. The provider was slow at the time.

**Spend:** 15 calls over the three runs, 11.3k prompt and 1.2k completion tokens, $0.00067 in all.
