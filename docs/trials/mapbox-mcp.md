# Trial: Mapbox MCP server (TypeScript, Mapbox APIs), holdout run (2026-11-11)

[mapbox/mcp-server](https://github.com/mapbox/mcp-server) at `b03c3e4` is Mapbox's own MCP server. Its tools call Mapbox's search, geocoding, directions, matrix, isochrone, map matching, optimization, static images and tokens APIs, through `fetch` against `MAPBOX_API_ENDPOINT` (default `https://api.mapbox.com/`).

This is a **holdout** run on `main` at `0d591b1b`. It was the first trial on a maps API, and the first in which the mock was built the way the README tells a user to: a fresh Claude Code with the Mockingbird plugin, given one prompt.

| | |
| --- | --- |
| APIs mocked | Mapbox: Search Box forward, category and list, Geocoding v6 reverse, Directions v5, Matrix v1, Isochrone v1, Map Matching v5, Optimization v1, Static Images, Tokens v2 |
| Contract | One OpenAPI file (12 paths) the agent wrote from the URLs the app builds. Mapbox publishes no OpenAPI spec. |
| Model | OpenRouter `deepseek/deepseek-v4.1-flash-1017` (the agent chose it) |
| App edits | None. `MAPBOX_API_ENDPOINT` points at the mock. `MAPBOX_ACCESS_TOKEN` is a dummy JWT-shaped `pk.…`, because the app checks the token's shape and reads the username from it. |

## How the mock was built (as the user's agent)

Headless Claude Code (`claude -p`, Sonnet 5.5) ran in a fresh clone with only the Mockingbird plugin loaded (`--plugin-dir plugins/mockingbird` from `main`, the content `/plugin install mockingbird@mockingbird` installs) and `mockingbird` from `main` on `PATH`. The plugin's launcher uses that binary rather than installing a release. The whole prompt was **"mock the Mapbox API for this app"**.

It finished in **434 s and 48 turns** ($0.92 of Claude Code spend). It used the skills, MCP and CLI, with no edits to the app or to Mockingbird. Its steps:

1. It read the `mockingbird` and `mockingbird-mock-api` skills and ran `doctor`.
2. It grepped the app for the URLs it builds and wrote `mocks/mapbox/mapbox.yaml` by hand. Then it ran `validate_contract`.
3. It ran `import_contract`, `update_mock` and `start_server` over MCP. These landed on **another project's server**, already running on this machine on the default admin port (B1). The agent noticed, deleted the mock and the LLM profile it had added there, and moved to the CLI: `mockingbird init` with its own ports (8081, 9091, 50052) and `mockingbird server start`.
4. It checked directions, search and isochrone with curl and wrote a README with the run command. It did not get the app's own tools to run against the mock; its smoke script hung.

## Results

The flows are 14 tool calls through the unmodified server over stdio MCP, the way an MCP host calls them (`flows.json`):
- search, then details on the first result;
- details for an unknown id;
- reverse geocoding;
- category search;
- directions, matrix, isochrone, optimization and map matching;
- a static map, the ground-location tool, public tokens;
- search and details again.

The app's own unit tests (1267, all passing) stub `fetch`, so they never reach the mock.

| Step | `main` | With #480 and #476 |
| --- | --- | --- |
| Search "Eiffel Tower" | ✗ five results, each `undefined` (no `properties`) | ✓ "Eiffel Tower (Tour Eiffel), 5 Avenue Anatole France, 75007 Paris" |
| Place details for that result | ✗ no id to look up | ✗ 404: the app calls `/places/v1/details/retrieve/…`, the contract says `/search/details/v1/…` (B2) |
| Reverse geocoding | ✗ empty name | ✓ "350 5th Ave, New York, NY 10118" in one run; in two of three runs, the search's Eiffel Tower record (F3) |
| Category search "coffee" | ✗ "No results found" | ✓ named cafés |
| Directions | ✓ | ✓ |
| Matrix | ✓ but no `durations` (the app's output check warns) | ✓ |
| Isochrone, map matching, static map, ground location, tokens | ✓ | ✓ |
| Optimization | ✓ | ✓ (one earlier run without the top-level scalar rule answered no `code: "Ok"`, which the app reports as "Optimization error: undefined") |

| Run | Calls | Sources | Model latency | Tokens (prompt / completion) |
| --- | --- | --- | --- | --- |
| `main` | 13 | 11 `llm`, 1 `cache`, 1 404 | p50 1.9 s, p95 3.6 s | 12.8k / 2.5k |
| With #480 and #476 | 15 | 9 `llm`, 2 `patched` (a `null` the contract does not allow, dropped), 1 `cache`, 3 404 | p50 1.3 s, max 4.6 s (n = 11) | 16.2k / 4.0k |

## Failures

| # | Class | Failure | Repro: request → expected → got |
| --- | --- | --- | --- |
| F1 | Engine (fixed in #480) | Answers held only what the contract requires; hand-written contracts leave most members optional | `GET /search/searchbox/v1/forward?q=Eiffel+Tower&limit=10` → features with names → `{id, type, geometry}` only. Matrix lacked `durations`; optimization lacked `code`. In the prompt's schema of a 2xx answer, every optional member is now required and may be `null`, except exclusive alternatives, write-only members and paging state. |
| F3 | Model answer, session context | A lookup by coordinates answered with a record an earlier call in the session served | `GET /search/geocode/v6/reverse?longitude=-73.9857&latitude=40.7484` after a search for the Eiffel Tower → a New York address → the Eiffel Tower feature, in two of three runs with the fixes. The prompt shows the session's earlier records; nothing ties a computed lookup to its own inputs. Open. |
| F2 | Engine (fixed in #476) | Output-budget shaping removed each search result's optional `properties`, all of its scalars, to keep the requested 10 items | Same request → ten hollow items. An item's content member now outlasts the page size; the page shrinks instead. |
| B1 | Install path: plugin and MCP | The plugin's MCP server acts on whatever server holds the default admin port, not the project's own | On a machine with another project's server on 9090, `import_contract`, `update_mock` and `put_llm_profile` changed that server. The agent cleaned up and moved to the CLI. A user with two projects would hit the same thing. |
| B2 | Agent build (skills) | A path the app calls is not in the contract, and nothing told the agent | The app calls `GET /places/v1/details/retrieve/{id}`; the hand-written contract has `/search/details/v1/retrieve/{mapbox_id}`. The answer is a 404 `no operation matches`. The agent never ran the app's own calls against the mock, so it never saw it. |
| B3 | MCP friction (minor) | `put_llm_profile` rejected `api_key`, then `secret_ref` without a provider, before it took the profile | Three calls to save one OpenRouter profile. |
| B4 | Persona (minor) | The agent's persona pins "San Francisco Bay Area" data | Paris and London lookups still came back right, because the request wins, but the persona fights any other city. |

## Scorecard (measures 1–7)

| # | Measure | Target | Result |
| --- | --- | --- | --- |
| 1 | App's tests and main flows pass | 100% | With the fixes, every step answers usable data except the three place-details calls, which fail on the contract path (B2); reverse geocoding sometimes answers the wrong place (F3). On `main`, 4 more steps returned unusable data (F1, F2). The unit tests stub `fetch` (1267/1267). |
| 2 | Consistency the flows rely on | 100% | Partly. A repeated search is identical (`cache`). Search → details can't be checked because of B2. An unknown place id on the contract's path gets an invented 200 record: the agent wrote no world and no `unknown_ids` choice. |
| 3 | Agent build ≤ 15 min, skills/MCP/CLI only | ≤ 15 min | ✓ 434 s, 48 turns, no app or Mockingbird edits. It lost time to B1 and B3. |
| 4 | State/pool latency | p50 < 50 ms | ✓ cache p50 5 ms. Nothing is served from state: every call computes an answer. |
| 5 | Model latency | p50 < 2 s, p95 < 5 s, rare | p50 1.3 s and max 4.6 s (n = 11) ✓. Not rare: routing and search compute each answer, so 11 of 15 calls reach the model. |
| 6 | Model spend per session | < $0.05 | ✓ about $0.002 (16.2k prompt / 4.0k completion) |
| 7 | Errors: unknown ids, invalid input | declared status and body | Partly. A search with no `q` gets the declared 400. Malformed coordinates get a 200 `{"code": "InvalidInput"}` written by the model; Mapbox answers 422. An unknown place id gets an invented record (see 2). |

**Do the fixes generalize?** The two engine fixes are structural and name no vendor:
- in the prompt's schema of a 2xx answer, every optional member is asked for and may be `null`, except exclusive alternatives, write-only members and paging state;
- a list item's content member outlasts the page size.

They turned four unusable steps into working ones. What's left is on the build side. The plugin's MCP should talk to the project's own server (B1). The skills should have the agent run the app's own calls against the mock before it finishes, so a path the contract lacks (B2) shows up then.

## Run 2: with another project's server on the default ports (2026-11-11)

This run checks B1 and the port collisions from three trials. The fixes under test:
- #493: the MCP server acts only on the project's own server;
- #505: `init` and `server start` pick free ports;
- #506: `put_llm_profile` takes a secret reference in one call;
- this PR: an app is handed only the project's own proxy.

A decoy Mockingbird server from another folder held 8080, 9090, 50051 and its proxy on 8888, with one mock (`pets`). A fresh Claude Code with only the plugin got the same prompt, "mock the Mapbox API for this app". The build ran on `main` with #505 and #506; the flows ran on this PR.

**Agent build: 171 s, 23 turns, $0.57**, against 434 s, 48 turns and $0.92 in run 1.
- `doctor` showed the server on 9090 as another project's.
- `init_config` chose 8081, 9091 and 50052 by itself, and `start_server` started the project's own server.
- The contract covers 20 operations, including the `/places/v1/details/retrieve` path run 1 missed (B2).
- The agent didn't run the app's own calls against the mock either, and it suggested `mockingbird run`.

**Isolation.** The decoy's mocks, LLM profiles and request log were unchanged, and it got no calls from the build or the flows.
- **Before this PR:** `proxy env` and `run` handed the app `HTTPS_PROXY=127.0.0.1:8888`, the decoy's proxy, because the agent's config has no `proxy:` block. Every Mapbox call went through the decoy's proxy to the real api.mapbox.com and came back 401; the decoy logged `unmapped host api.mapbox.com (pass)`.
- **With this PR:** `proxy env` refuses, saying 8888 is held by something that is not this project's proxy. `mockingbird run -c mockingbird/mockingbird.yaml -- <app>` starts the project's server with its own proxy on 8889, and every Mapbox call reaches the project's mock.

| Step | Run 2, through `mockingbird run` |
| --- | --- |
| Search "Eiffel Tower" (first) | ✗ The model call timed out at 20 s; the fallback answer has no names. A second search came back right. |
| Place details for that result | ✗ No id, because the first search fell back |
| Place details, unknown id | Invented 200 record with no name (12.2 s) |
| Reverse geocoding | ✓ "350 5th Ave, New York, New York 10118" |
| Category search, directions, matrix, isochrone, optimization, map matching, static map, ground location, tokens | ✓ |

| Calls | Sources | Model latency | Tokens (prompt / completion) |
| --- | --- | --- | --- |
| 13 flow calls (plus the agent's 4 checks) | 16 `llm`, 1 fallback (timeout) | p50 2.5 s, max 13.6 s (category search); matrix 7.6 s | 20.3k / 5.4k, all 17 calls |

**Scorecard changes from run 1.**
- **3:** ✓ in 171 s, and nothing was written to the other project's server.
- **5:** ✗ p50 2.5 s, three calls over 5 s, and one 20 s timeout. This agent wrote a larger contract than run 1's and no persona; why these answers are slow is still open.
- The other measures are as in run 1.

## Run 3: fresh agent with `verify --run` (2026-11-12)

The binary and plugin were `main` at `b927dcc9`, which has #493, #505, #506 and #517. A fresh agent got the same prompt, and the flows ran with `mockingbird verify -c mockingbird.yaml --run "node mcp.mjs flows.json"`.

**Agent build: 121 s, 20 turns, $0.54.**
- It wrote a 15-operation contract by hand and noticed two paths it doesn't cover: `/places/v1/details/retrieve` (B2) and the `isochrone/v1/{profile}` form the ground-location tool builds.
- It asked instead of adding them.
- It didn't run the app's own calls, because the app's unit tests stub the network.

**`verify --run` catches B2.** It failed with two `unmatched_route` findings, each with a next step: ``Mock `mapbox` has no operation for GET /places/v1/details/retrieve/…: its contract lacks the route the app calls. …``. The other steps answered.

**New blocker: stale config ports (fix in progress, branch `cli/recorded-server`).**
- `init_config` wrote ports 8081/9091/50052. Another session's server took 8081 before the agent started its own.
- The agent restarted with `start_server` on 18081/19091/15052, but those ports were not written to the config.
- `verify --run` then refused the server that now held 9091 (another project's, correctly). After that it failed with "a background server already runs": it never found the project's own server, recorded in `.mockingbird/`.
- The numbers above come from correcting the config's ports by hand.

**Not measured: answer quality.** The OpenRouter key hit its spend limit during this run, so every model call got a 403 and fell back to the faker. `verify --run` reported each as a `fallback` with that reason. Quality and latency need a rerun once the limit is raised.

## Run 4: fresh agent on main with the model back (2026-11-12)

The binary and plugin were `main` at `e3604a9c`, which includes #557 and #559. The same prompt went to a fresh agent, and the flows ran through `mockingbird verify -c mockingbird/mockingbird.yaml --run "node mcp.mjs flows.json"`. `verify` used the agent's running server on 8080/9090 and started none; run 3's port problem didn't recur.

**Agent build: 107 s, 27 turns, $0.70, no app edits.**
- It wrote a 14-operation contract by hand, this time including `/places/v1/details/retrieve`, and served it with `hosts: [api.mapbox.com]` and the proxy.
- It checked search, directions, reverse geocoding and a feedback write through the proxy. As in run 3, it didn't run the app's own calls.
- It noted these gaps itself:
  - directions ignores `steps=true`, so no legs come back;
  - the feedback list is padded with generated records;
  - its sort parameters don't work.

**`verify --run`: passed, 0 failures, 15 requests in the window.** The app's text output is right for most steps. Its own output-schema check (an `Output validation warning` on stderr) still fails for four tools, so the contract's shapes don't match what the app reads in places (F4).

| Step | Result |
| --- | --- |
| Search "Eiffel Tower" | ✓ "Eiffel Tower (Tour Eiffel), Champ de Mars, 5 Avenue Anatole France, 75007 Paris" |
| Place details for that result | ✗ The mock answered from state with the same record the search served (4 ms). The app prints `Name: undefined`: it reads the Places API's flat object, and the contract declares a GeoJSON `Feature` (F4). |
| Place details, unknown id | ✗ A 200 with an invented record (served as `state`, its `mapbox_id` not the one asked for), with the same shape problem |
| Reverse geocoding | Text ✓ "350 5th Ave, New York, New York 10118". The app's output-schema check fails (F4). |
| Category search, directions, isochrone, map matching, static map, ground location, tokens | ✓ |
| Matrix, optimization | Text ✓. The app's output-schema check fails (F4). |
| Search again | ✓ Identical, from cache |
| Place details again | ✗ As above |

| Calls | Sources | Model latency | State/cache latency | Tokens (prompt / completion) |
| --- | --- | --- | --- | --- |
| 15 | 9 `llm` with tokens, 1 `patched`, 3 `state`, 1 `cache`, and the static image (labelled `llm` with no tokens or body, F5) | p50 1.5 s, max 4.4 s (n = 10) | p50 5.5 ms | 26.6k / 5.0k |

| # | Class | Failure | Repro: request → expected → got |
| --- | --- | --- | --- |
| F4 | Agent build (contract shapes) | Response shapes in the hand-written contract differ from what the app reads | `GET /places/v1/details/retrieve/{id}` → a flat object with `name`, `full_address` and so on (the app's `PlaceDetailsOutput`; only the legacy `/search/details/v1/retrieve` returns a `Feature`) → a `Feature`, so `name` is under `properties`. Reverse geocoding, matrix and optimization also fail the app's output schemas (missing members). `verify --run` can't see this: the answers are valid against the contract. The app ships an output schema for almost every tool (`src/tools/*/*.output.schema.ts`; not the static map) that the agent could have written the contract from. Open (skills). |
| F5 | Request log (minor) | The static image is logged as `source: llm` with 0 tokens and no body | `GET /styles/v1/mapbox/streets-v12/static/…` → an image or an empty body, with its true source → `llm`, 3 ms. Open. |

**Scorecard (run 4).** Measure 1 uses run 1's target: every flow passes.

| # | Measure | Result |
| --- | --- | --- |
| 1 | App's tests and main flows | ✗ The three place-details steps print the wrong data. Three more steps print right but fail the app's own output-schema check (F4). The app's unit tests stub `fetch`. `verify --run` passes. |
| 2 | Consistency | Partly. Details for a search result is that same record, from state, and a repeated search is identical. An unknown place id gets an invented record (no world, no unknown-id policy), as in run 1. |
| 3 | Agent build | ✓ 107 s, 27 turns, $0.70, no app edits |
| 4 | State/cache latency | ✓ p50 5.5 ms |
| 5 | Model latency | ✓ p50 1.5 s, max 4.4 s (n = 10). Not rare: 10 of 15 calls reach the model. |
| 6 | Spend | ✓ 26.6k prompt / 5.0k completion tokens, well under $0.05 at the listed `-1017` price |
| 7 | Errors | Partly, as in run 1. An unknown place id gets an invented 200 record. |
