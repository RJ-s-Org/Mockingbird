# Trial: GitHub CLI `gh` (Go)

[cli/cli](https://github.com/cli/cli) at `f7a06a1`, built from source (`go build ./cmd/gh`; the module declares `go 1.27.0`, so the Go 1.27.3 toolchain built it). `gh` talks to GitHub's REST and GraphQL APIs on the same host.

| | |
| --- | --- |
| APIs mocked | GitHub REST (`api.github.com/…`) and GraphQL (`api.github.com/graphql`) |
| Contracts | REST: the full upstream description (`examples init github-rest --full`, 1,254 operations). GraphQL: GitHub's own `schema.docs.graphql` from docs.github.com (the catalog's pinned octokit schema is too old for `gh`, below) |
| Model | OpenRouter `deepseek/deepseek-v4.1-flash`, default routing |
| Source edits | None. `GH_TOKEN` is a dummy; `GH_CONFIG_DIR` points at the trial directory so the user's own `gh` login is never read |
| Result | 8 of 10 commands succeed; 15 mock calls, 2 fallbacks (`createIssue`, one `pr view`) |

## Config and commands

```yaml
server: {port: 43080, admin_port: 43090, grpc_port: 43051}
proxy: {enabled: true, port: 43888}
llm:
  default_profile: openrouter
  profiles:
    openrouter:
      api_key: env:OPENROUTER_API_KEY
      base_url: https://openrouter.ai/api/v1
      model: deepseek/deepseek-v4.1-flash
mocks:
  github-rest:
    contract: upstream.api.github.com.json
    route_prefix: /
    hosts: [api.github.com]
    persona: &persona >-
      GitHub.com as seen by Mira Okafor (login "mira-okafor", user id 58213947,
      name "Mira Okafor"), a maintainer at "Fernwood Labs" (organization
      "fernwood-labs"), a small Berlin open-source company. Its repositories are
      fernwood-labs/trellis (a Rust HTTP router, about 4.2k stars, MIT, default
      branch main), fernwood-labs/trellis-docs (TypeScript) and fernwood-labs/sprout
      (a Go CLI). Owners and URLs agree with the request ... Counts are plausible:
      totalCount matches the full list, nodes holds at most the requested `first` items.
  github-graphql:
    contract: schema.docs.graphql
    route_prefix: /graphql
    hosts: [api.github.com]
    persona: *persona
```

```bash
export GH_CONFIG_DIR=$PWD/mockingbird/ghconfig GH_TOKEN=gho_trialtoken000000000000000000000000 \
       GH_NO_UPDATE_NOTIFIER=1 GH_PROMPT_DISABLED=1 NO_COLOR=1 GH_PAGER=cat
mockingbird run -c mockingbird/mockingbird.yaml -- ./mockingbird/flows.sh
```

`flows.sh` runs, against `fernwood-labs/trellis`: `gh api user`, `repo view`, `issue list`, `pr list`, `issue view 42`, `issue create`, `release list`, `search repos "http router" --language rust`, `pr view 7`, `repo view` again.

## What happened

**Two mocks, one host.** With both catalog configs as they were, `serve` refused to start: `mocks "github-graphql" and "github-rest" share the route prefix "/"`. The GraphQL example mounted itself at `/`. This PR moves it to `route_prefix: /graphql` (it answers `POST /graphql` there), and a new test checks that no two mocks in the catalog collide, so any examples can be combined in one config.

**Go on macOS trusted the CA.** The docs said Go on macOS ignores `SSL_CERT_FILE`; `gh` verified the proxy's certificates anyway. Go 1.27 changed this: on macOS and Windows, `SSL_CERT_FILE` replaces the platform verifier when the GODEBUG setting `x509sslcertoverrideplatform=1` is on, which is the default for modules that declare `go 1.27`. A test program declaring `go 1.26` and built with the same toolchain still failed (`x509: certificate signed by unknown authority`), and passed with `GODEBUG=x509sslcertoverrideplatform=1`; built with Go 1.26 it fails either way. `proxy env` now exports that GODEBUG (kept before any `GODEBUG` you set), and [proxy.md](../proxy.md#per-runtime) says which Go builds it covers.

**Run 1, catalog's pinned GraphQL schema (octokit, July 2024): 9 of 10.** `gh issue view` failed with `Unknown field "issueType" on type "Issue"` (and `parent`, `subIssues`, `subIssuesSummary`, `blockedBy`, `blocking`). `gh` assumes github.com has every feature and asks for them. octokit's current schema (July 2026) still lacks `issueType`, `blockedBy` and `blocking`; GitHub's own `schema.docs.graphql` has them all, so run 2 used that.

**Run 2, `schema.docs.graphql`: 8 of 10.**

| Command | Result | Mock calls |
| --- | --- | --- |
| `gh api user` | `mira-okafor`, full persona profile | REST `users/get-authenticated`, 1.9 s |
| `repo view` | **fails: `invalid UTF-8 string`** | GraphQL `repository` 0.5 s; REST `repos/get-readme` 4.9 s |
| `issue list` | 5 open issues with labels and dates | GraphQL 2.8 s |
| `pr list` | 5 pull requests, branches, one draft | GraphQL 2.2 s |
| `issue view 42` | title, labels, milestone, projects, empty `issue-type`/`parent`/`sub-issues` | GraphQL 2.7 s + 1.4 s |
| `issue create` | **prints nothing, exits 0** | GraphQL `repository` 0.7 s; `createIssue` **fallback after 16 s** |
| `release list` | v0.9.2 (Latest), v0.9.1, v0.10.0-rc.1 (Pre-release) | GraphQL 1.1 s |
| `search repos` | trellis, trellis-docs, sprout (persona repos) | REST `search/repos` 6.0 s |
| `pr view 7` | worked in run 1 (reviewers, checks, milestone); **fallback in run 2** | GraphQL 7.1 s |
| `repo view` (again) | same UTF-8 failure; README from `cache` | — |

| Calls | Sources | Fallbacks | Latency (answered by the LLM) | Tokens (prompt / completion) |
| --- | --- | --- | --- | --- |
| 15 (4 REST, 11 GraphQL) | 11 `llm`, 2 `cache`, 2 `fallback` | 2 | 0.5–6.0 s, median 1.9 s | 80k / 16.6k (58k prompt in the two fallbacks) |

## Findings

- **Create and update mutations generate their whole return type.** `createIssue(input) { issue { id url } }` asked the model for every field of `Issue`, recursively: 36–39k prompt tokens, 16–20 s, and an answer that failed validation, so the payload was `null`. That is deliberate (the stored resource must be whole for later reads), but on a type the size of GitHub's `Issue` it cannot work. `gh` printed nothing and exited 0, so a CI job would not notice. Open: generate the selection plus the type's scalar fields, and let the faker fill the rest of what is stored.
- **LLM-written base64 is corrupt.** `GET /repos/{owner}/{repo}/readme` returns `content` with `encoding: base64`. The model writes the base64 itself, and the decoded README had a garbled line in run 1 and invalid UTF-8 in run 2, which made `gh repo view` fail. Open: generate text and encode it locally when a sibling says `encoding: base64` (or the schema says `contentEncoding: base64`).
- **Unhelpful validation errors.** Both fallbacks were reported as `repaired output still invalid: <the whole answer> is not valid under any of the schemas listed in the 'anyOf' keyword`: the nullable wrapper's `anyOf` hides the real error, and the message carries kilobytes of model output.
- `createIssue`'s first answer was wrapped as `{"data": {...}, "errors": null}`, which the envelope check did not unwrap because of the `errors` member. This PR treats `null` members as absent there.
- `verify` prints every fallback reason in full, so the failure message for this run was about 10 KB. Its reasons should be truncated.
- Pull requests and issues got the same numbers (412, 409, 405, …); on GitHub they share one sequence.

## After the engine fixes

Run 3: the same build of `gh`, config, schema and model, on the engine-fixes branch (GraphQL mutations narrowed to their selection, base64 encoded locally).

| Command | Run 2 | Run 3 |
| --- | --- | --- |
| `repo view` (twice) | `invalid UTF-8 string` | name, description and the README rendered (REST `repos/get-readme` 3.4 s, then `cache`) |
| `issue create` | prints nothing, exits 0; `createIssue` fallback after 16 s, 38.8k prompt tokens | prints `https://github.com/fernwood-labs/trellis/issues/318`; `createIssue` `llm` in 1.0 s, 745 prompt tokens, 54 completion |
| `pr view 7` | fallback | reviewers, projects, additions/deletions (`llm`, 11.1 s) |
| the other 7 | worked | worked |

| | Calls | Sources | Fallbacks | GraphQL prompt / completion tokens |
| --- | --- | --- | --- | --- |
| Run 2 | 15 | 11 `llm`, 2 `cache`, 2 `fallback` | 2 | 74.7k / 10.4k |
| Run 3 | 15 | 13 `llm`, 2 `cache` | 0 | 25.9k / 5.1k |

10 of 10 commands succeed. The `pr view 7` fallback in run 2 was the large `anyOf` answer failing validation; in run 3 the model's answer validated, so that one is luck, not a fix.

## Run 4: stateful flows through skills and CLI (2026-11-10)

The question this time was whether one `gh` command sees what an earlier one did. The mock was built the way a user's coding agent would build it: `examples init github-rest --full` and `examples init github-graphql --full` merged into one config, `llm configure --provider openrouter --model deepseek/deepseek-v4.1-flash-1017` (key as `env:OPENROUTER_API_KEY`), `agent install claude-code`, `server start --proxy`, then the `mockingbird-world` loop: world files referenced from the config, checked with `world show` and with self-test calls to the data plane. `gh` (same `f7a06a1` build) went through the proxy with `GH_HOST=github.com` and a dummy `GH_TOKEN`. The binary was lane-2 `mockingbird 0.1.0` from today's `main`. Files are in `mockingbird-trials/gh-cli/mockingbird/trial4/` (`flows.sh`, `world-*.yaml`, `run1.txt`, `run2.txt`, `req-*.json`).

The flow is 20 commands. It covers `api user`, `repo view` and `repo list`. It creates an issue, then lists and views it, comments on it, views it with `--comments`, closes it, views it again, and lists closed and open issues. It reads the issue through REST `gh api`. It runs `pr list`, then `pr create`, `pr list` and `pr view` (twice). Last come a `gh api graphql` read of the issue and a REST `gh api` read of the PR.

Run 1 used only the heuristic world description. Run 2 added the agent's world entries:
- GraphQL: `issue` and `pullrequest` lifecycles; `createIssue`, `addComment` and `createPullRequest` as creates reading `/input`; `closeIssue` and `reopenIssue` as actions on target `input.issueId`; `Query.node` as a read of `issue`.
- REST: `issues` and `pulls` identified by `number` with lifecycles, a `state` filter and paging on the lists, and comments with an issue parent.

### What works

- **Every command exits 0 in run 2.** The proxy and Go 1.27 TLS work with no `gh` changes. `api user`, `repo view` (README rendered), `repo list`, issue create/list/view/comment/close, `pr list`, `pr create`, `pr view` and `gh api graphql` all print plausible persona data.
- **REST is fully stateful once described.** These self-tests were all answered from `state`: `POST /repos/…/issues` → 201 #58, then `GET …/issues/58` → same title, then `PATCH {state: closed}` → closed, then `GET …/issues?state=closed` → `[#58 closed]`, then `?state=open` → `[]`.
- **GraphQL root fields are stateful once described.** `createIssue` → `I_kwDOtrellis1`, then `closeIssue(input:{issueId})` → `CLOSED`. After that, `node(id:)` answers from `state` (`Selftest issue`, `CLOSED`).

### Failures

What `gh` asks through nested fields never sees state. So the stateful checks fail in both runs: the created issue is not listed, `issue view` shows another title, the closed issue still shows `OPEN`, and the comment is missing.

| # | Failure | Minimal repro (request → expected / got) | Class |
| --- | --- | --- | --- |
| 1 | GraphQL fields below a root field are projected from the root's generated answer, never read from stored records. This breaks `issue list/view`, `issue view --comments`, `pr view` of a created PR, the closed-state check, `issueOrPullRequest`, and `pr create`'s existing-PR check. | `createIssue{title:"Router panics on empty path"}` → #412, then `repository(owner,name){issue(number:412){title state}}` → expected the stored title and `OPEN`; got the model's `"Router: nested wildcard params lose their name…"`. After `closeIssue` the same read → expected `CLOSED`; got `OPEN`. Run 1 got `issue: null`, and `gh issue view` failed with "issue was not found". | inexpressible (the world format keys root fields only; it cannot say `Repository.issue(number)` reads an `issue` scoped to the repository, or that `Repository.issues(states)` lists them). Engine work is needed to serve nested connections from state. |
| 2 | Consequence of 1 seen in `gh`: `issue close 412` called **`closePullRequest`**. The model answered `issueOrPullRequest(number:412)` with a `PullRequest`. `gh` still printed "✓ Closed issue". | `repository{issueOrPullRequest(number:412){__typename}}` → expected `Issue` (just created); got `PullRequest` | model answer (enabled by 1) |
| 3 | Run 1: `pr create` was refused with "a pull request for branch fix-empty-path into main already exists". `gh`'s duplicate check got a generated open PR. | `repository{pullRequests(headRefName:"fix-empty-path",states:OPEN){nodes{number}}}` on a fresh session → expected `[]`; got PR #418 | model answer (an empty closed-world list would need 1) |
| 4 | `createIssue` took **144 s**. With the world's `request.record: /input`, the executor stopped with `record_invalid` (`render_error=<root> anyOf`). Fill then asked the model for the whole `Issue` type, the answer was truncated at `max_tokens` 16384 (provider Modal), and the mutation answered at last. This brings back the run-2 problem that run 3 had fixed for creates. | `mutation{createIssue(input:{repositoryId,title,body}){issue{id url}}}` with the run-2 world → expected about 2 s and a small prompt; got 143,859 ms and 16k truncated completion tokens. `server.log`: `fill fell back … LLM output truncated … stopped at max_tokens (16384)` | engine bug |
| 5 | The request log leaves out the fill call's tokens: the 144 s `createIssue` is recorded as 1,008 / 84. Spend and the token columns under-count. | the same call → expected about 1k / 16.5k; got 1,008 / 84 | engine bug |
| 6 | Run 1: `gh repo view` → `failed to decode readme: illegal base64 data at input byte 0`. The model wrote plain Markdown into `content` (`"# trellis — a small, fast…"`, `encoding: base64`), and it was passed through without being encoded. Run 2 was base64 and rendered. | `GET /repos/fernwood-labs/trellis/readme` → expected base64 `content`; got raw text (source `llm`) | engine bug (local encoding is not applied when the model writes plain text) |
| 7 | REST `pulls/get` falls back to the faker in both runs (12 s and 28 s): `repaired output still invalid: /labels/0: required: "archived_at"`. `gh api repos/…/pulls/431` printed `title: "bravo"`, `head: "meadow"`. | `GET /repos/fernwood-labs/trellis/pulls/431` → expected a PR shaped like the persona; got faker words | model answer (a missing required member could be patched locally instead) |
| 8 | REST and GraphQL are two mocks with two stores. The issue created over GraphQL (#412) read through REST: `gh api repos/…/issues/412` → `open`, `"Trailing-slash redirect loses query string…"`. | GraphQL `createIssue` then REST `GET /repos/…/issues/{n}` → expected the same issue; got a generated one | inexpressible (no shared world across the mocks of one API) |
| 9 | `server start` on ports another server already holds (the Plaid trial took 43080/43090 seconds earlier) printed `ready: true` with a pid that had already exited. The client commands then talked to the Plaid server: `mocks list` showed `plaid`, and `world show github-rest` gave 404. `server.log` had `cannot bind 127.0.0.1:43080: Address already in use`. | two `server start` runs with the same ports → expected an error on the second; got `ready: true` | engine bug (CLI) |
| 10 | The world vocabulary in the skill and in `docs/world.md` does not cover `set`, `effects`, `refuse` or `computed`, yet the `no_effect` gap tells you to "write its `set` … an expression". Neither says how a GraphQL target names an argument: `target: {member: issueId}` was refused as "not a member of the request body", and `input.issueId` was accepted. | `patch` `Mutation.closeIssue.target.member: issueId` → expected a hint to use `input.issueId`; got `unresolved` | skills/MCP gap |
| 11 | The full contracts start with 1,350 gaps (GraphQL 541, REST 809). `world show` has no per-operation or per-type filter, so an agent cannot find the gaps for the 10 operations an app uses without its own script. | `world show github-rest --json` → 809 gaps | skills/MCP gap |
| 12 | `llm configure -c mockingbird.yaml` rewrote the config and dropped every comment from the example (it kept a `.bak`). | — | engine bug (CLI, minor) |

### Numbers

| | Run 1 (heuristic world) | Run 2 (agent world) |
| --- | --- | --- |
| Commands exiting 0 | 17 of 20 (`repo view` base64, `pr create` "already exists", `issue view --json` not found) | 20 of 20 |
| Stateful checks passing (listed after create, view matches, comment shown, closed shows closed, REST sees it) | 0 of 5 | 0 of 5 |
| Mock calls | 24 GraphQL, 4 REST | 25 GraphQL, 4 REST (plus 10 self-test calls) |

- **Model-served latency**, 59 calls answered `llm` across both runs and self-tests: **p50 2.3 s, p95 9.7 s**. The worst were `createIssue` at 144 s (failure 4) and `pulls/get` at 28 s (a faker fallback, not counted above). State-served calls took 1–20 ms.
- **OpenRouter spend**: about **$0.03**. The request log records 204k prompt and 27k completion tokens, plus about 16k truncated fill tokens it does not record (failure 5). At $0.006/M prompt and $0.50/M completion that is about $0.03. The key's daily counter showed $0.21, but other trials share the key.

### Run 5: after nested fields from state (#406 and its follow-up)

Run 5 used the same config, world (minus the `request.record: /input` entries) and `gh` build, on a binary built from `claude/gql-nested-2`. Fields below a query's top-level field now read stored records: an object read by arguments, and a list or connection under a stored parent, filtered by its arguments. Records that `gh` reads or writes without selecting their `id` are named by a lookup key (`Issue.number`, `PullRequest.number`), or get an id of their own.

| Stateful check | Run 2 | Run 5 |
| --- | --- | --- |
| Created issue is listed | no | yes: `issue list` shows only #418, `Router panics on empty path` |
| `issue view` matches it | no | yes: title, body and state |
| The comment shows | no | yes in `issue view --json comments` (`Reproduced on main; fix incoming.`). `issue view --comments` exits 1 (below) |
| Closed shows closed | no | yes: `--state closed` lists #418 `CLOSED`, `--state open` is empty, and `gh api graphql … issue(number: 418)` gives `CLOSED` |
| REST sees it | no | no (two mocks, failure 8) |

That is **4 of 5**. `pr create` now succeeds and `pr list`, `pr view` and `pr view --json` show the new PR (#421, `fix-empty-path`). 19 of 20 commands exit 0.

New in run 5:
- **Engine bug:** `issue view --comments` fails with `parsing time "garnet"`. The comment is served from state, and a field no record holds (the comment's `createdAt`) is filled by the faker. GitHub's custom `DateTime` scalar is compiled as an unconstrained value (`{"description": "GraphQL scalar DateTime"}`), so the faker writes a word. Fix in the importer: map date and time scalars to `format: date-time`.
- `createIssue` (145 s) and `closeIssue` (33 s) still take the fill path (failure 4). Model-served latency over 23 calls: p50 2.0 s, p95 31.6 s.
- OpenRouter: the key's daily counter is $1.23 across every trial sharing it. This session's calls stay well under $1.
