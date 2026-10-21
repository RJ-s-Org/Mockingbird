# Improvements and enhancements

A running list of what to improve, mostly from using Mockingbird the way it is meant to be used: a coding agent (Claude Code, Codex, Cursor…) mocking a project's third-party APIs through the MCP server, with the console as a secondary interface. Each entry says where it came from. Move an entry to **Done** with its PR when it ships.

## Field trials, 2026-10-21

Three fresh Claude Code sessions, each with only the Mockingbird plugin, mocked the APIs of a real open-source project. The LLM was `deepseek/deepseek-v4.1-flash` on OpenRouter.

| Trial | API style | Outcome |
|---|---|---|
| Stripe `accept-a-payment` (Node server) | REST, form-encoded bodies, 8 MB OpenAPI | Ran end to end after four workarounds; 25% faker fallbacks |
| `github-readme-stats` | GitHub GraphQL + REST | Cards rendered after trimming both contracts and two source edits; 33% GraphQL fallbacks, REST always fell back |
| `nuxt/movies` | TMDB, no spec (documentation import) | Every page rendered; docs import needed undocumented profile settings; 20–80 s cold pages, 28% fallbacks |

Total spend was about $1. When the LLM answered, the data was excellent; the problems are around it.

### Phase 1: works out of the box for an agent

- `agent install` could still write the provider key's passthrough into `.mcp.json` for agents that start the MCP server without a login shell (the server now imports missing variables from `$SHELL -l -i`, which covers macOS and Linux desktops).

### Phase 2: real-world contracts load

- `$ref`s inside `x-*` extensions are reported unresolvable (Stripe, GitHub REST).
- GraphQL implementation check rejects `String!` implementing `String` (GitHub GraphQL).
- OpenAPI 3 form-encoded and multipart bodies are dropped before generation; bracket keys (`a[b][0]`) are not decoded.
- Validation failures are hidden behind a faker-filled `default` error.
- One host cannot serve two APIs (GraphQL at `/graphql` and REST at `/`): mocks cannot share a prefix.

### Phase 3: cheaper, faster, consistent generation

- Warn at import when an operation's schema is huge (the prompt now fits it to a budget, see Done).
- GraphQL aliases of one field with different arguments still share one generated value.
- GraphQL root fields of one query are generated independently, so they can contradict each other: github-readme-stats' pin card asks for `user(login:)` and `organization(login:)` and gets both, where GitHub returns exactly one (the card then fails with "Unexpected behavior"). Generate a query's root fields in one call, or tell each about its siblings.
- Detail objects (TMDB movie, TV) often fail validation once and need a repair call, doubling their 30 s; find out which constraints the first answer misses.
- GETs of a stored resource still call the LLM; serve them from session state.
- No consistency across calls: an opt-in response cache keyed by operation and parameters, list → detail entity seeding, server-side unique ids, optional 404 for unknown ids.
- Faker output breaks clients: int32-minimum counts, `null` lists, random words for ids and logins. Echo path and argument values into same-named fields, keep counts small and non-negative, prefer non-null lists, honour `format` and `pattern`.
- Unconstrained strings can run away (a 5 KB `address.city`): default `maxLength`.

### Phase 4: documentation import and review from MCP

- No MCP tools to view, edit or approve a draft; `approve` does not activate; each import replaces the contract instead of merging; a list of URLs or a docs folder is not accepted.
- Drafts have no `required` lists, so `{"page":1}` is a valid answer; operationIds from the docs are replaced; invented `default`s; low-confidence operations kept.
- Embedded OpenAPI blocks in documentation pages (ReadMe-hosted docs such as TMDB) are ignored and re-derived by the LLM.
- MCP output sizes: `validate_contract` lists every operation, `list_requests` returns full bodies, `server_logs` keeps ANSI codes; `server_logs`/`stop_server` resolve state from the MCP process's folder instead of the server.

### Phase 5: no source edits to reach the mock

- Apps hardcode `api.github.com`, `api.themoviedb.org`; every trial edited source to point at the mock. A `mockingbird proxy` mode (HTTP(S) proxy with a local CA, routing hosts to mocks) or host-based routing would avoid it. Image hosts (`image.tmdb.org`) could get placeholder images.

### Smaller items

- Chaos errors use problem+json instead of the contract's error schema, so SDKs treat them as success.
- Scenarios need request headers that SDK clients cannot send: a mock-level default scenario and per-operation instructions.
- Config hot reloads are not logged; `/readyz` and the log can disagree after a failed reload.
- Signed webhooks (Stripe) need a signing helper; Stripe's spec has no webhook definitions to drive events from.

## Ideas

- **Key setup page.** A small local web page that `mockingbird` opens (or that an agent opens in the browser for the user, from Claude, ChatGPT, Codex…) with a form for provider API keys. The keys go straight from the browser to Mockingbird, never through the agent's context, and are stored outside the project: the OS keychain, or a `0600` secrets file referenced as `${file:…}`. It also removes the "MCP process can't see my shell's key" problem, since the server reads the key from the store rather than from its environment. Expose it as an MCP tool (`open_key_setup`) that returns the URL and waits for the user to finish.

## Done

- Phase 1, [#31](https://github.com/RJ-s-Org/Mockingbird/pull/31):
  - the MCP server imports keys from the login shell;
  - `doctor` names the process that lacks a key, and `start_server` refuses a profile whose key is missing;
  - `init_config` ports;
  - `llm test` defaults to `mockingbird.yaml`;
  - 60 s / 16 384-token defaults and array-envelope unwrapping;
  - reasoning-only, in-band error and truncated replies are handled, with one retry;
  - `X-Mockingbird-Fallback-Reason` and `request_log.fallback_reason`;
  - profile `extra_body`;
  - docs import at temperature 0 in JSON-object mode;
  - the server-running hint only on connection errors.

  Measured afterwards on the Stripe trial's mock: 25 `POST /v1/payment_intents` with OpenRouter's default routing gave 24 `llm`, 1 `repaired`, **0 fallbacks** (the trial's baseline was 28%), with a median of 7.4 s and a worst case of 9.6 s. Every call went to the DigitalOcean route that failed during the trial. It returns content in JSON-object mode but drops the answer in JSON Schema mode, so the OpenRouter preset uses JSON-object mode.
- Phase 3 (part), [#34](https://github.com/RJ-s-Org/Mockingbird/pull/34):
  - GraphQL generates only the selected fields (with their arguments);
  - the prompt's schema is compact and fitted to a 64 KiB budget, nearest definitions first;
  - the prompt asks for short arrays;
  - the request log API shows the fallback reason.

  Trials re-run on the full upstream contracts (no trimming), OpenRouter `deepseek/deepseek-v4.1-flash` with default routing:

  | Trial | Run 1 (field trial) | After Phases 1–2 | After Phase 3 |
  |---|---|---|---|
  | Stripe `accept-a-payment`, 8 creates + 4 retrieves through the app | 25% fallbacks, trimmed contract | 0/22 fallbacks; 357k prompt tokens and 14.5 s median per call | 0/13 fallbacks; 23k prompt tokens and 5 s median per call |
  | `nuxt/movies`, 5 pages | 28% fallbacks, 20–80 s pages | 4/5 pages real, 1/9 fallbacks (timeout), 9–121 s pages | 5/5 pages real, 0/9 fallbacks, 16–68 s pages |
  | `github-readme-stats`, stats, top-langs, pin cards | trimmed schemas, 33% GraphQL fallbacks | every card errors: 347k prompt tokens, truncated or timed out | stats and top-langs render, 0/8 fallbacks, 750–1,350 prompt tokens, 2.5–7 s; pin card fails (sibling root fields, above) |
