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
