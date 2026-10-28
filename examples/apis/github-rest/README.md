# GitHub REST

A stateful mock of the GitHub REST API v3 for "Fernwood Labs", a small open-source
company: repositories, issues, pull requests, users, repository search and GitHub
Apps (installations and installation access tokens) on `api.github.com`, with `issues`
and `pull_request` webhooks for new issues and pull requests.

| | |
| --- | --- |
| Contract | [`github-rest.openapi.yaml`](github-rest.openapi.yaml), a hand-written subset (18 operations) that keeps GitHub's operation ids (`repos/get`, `issues/create`, ...) |
| Upstream | [github/rest-api-description](https://github.com/github/rest-api-description) `api.github.com.json` (MIT), pinned in [`example.json`](example.json); `mockingbird examples init github-rest --full` downloads it |
| Hosts | `api.github.com` |
| Scenarios | `rate-limited` (403), `not-found` (404), `outage` (503) |
| Webhooks | `issue.created`, `pull.created` to `http://127.0.0.1:3000/api/github/webhooks`, signed `hmac-sha256` with `env:GITHUB_WEBHOOK_SECRET` |

## Quickstart

```bash
mockingbird examples init github-rest --dir github-rest && cd github-rest   # or: cd examples/apis/github-rest
mockingbird serve -c mockingbird.yaml --offline
```

In another shell:

```bash
curl -s localhost:8080/repos/fernwood-labs/trellis -H 'Host: api.github.com' \
  -H 'Authorization: Bearer ghp_test123'
```

[PyGithub](https://github.com/PyGithub/PyGithub), unmodified, through the proxy:

```bash
eval "$(mockingbird proxy env)"
pip install PyGithub
python - <<'PY'
from github import Auth, Github
gh = Github(auth=Auth.Token("ghp_test123"))
repo = gh.get_repo("fernwood-labs/trellis")
print(repo.full_name, repo.stargazers_count, repo.default_branch)
issue = repo.create_issue(title="Router panics on empty path segment", labels=["bug"])
print(issue.number, issue.state, issue.html_url)
PY
```

A GitHub App's token exchange works too: `POST /app/installations/{installation_id}/access_tokens`
answers `201` with a `ghs_…` token, and `GET /installation/repositories` lists what it
can reach, so code built on `@octokit/auth-app` or PyGithub's `GithubIntegration` (and
healthchecks that mint a token) run against the mock:

```bash
curl -s -X POST localhost:8080/app/installations/51837264/access_tokens -H 'Host: api.github.com' \
  -H 'Authorization: Bearer eyJhbGciOiJSUzI1NiJ9.test.jwt'
```

Octokit (`@octokit/rest`) works the same way on Node ≥ 22.21 (`NODE_USE_ENV_PROXY`
is set by `proxy env`).

Drop `--offline` (and configure an LLM, see [the configuration reference](../../../docs/configuration.md#llm))
for answers that follow the persona: `fernwood-labs/trellis` as a Rust router with
plausible star counts, issue titles and labels copied from the request, URLs that
match the path.

## Expected output

`--offline` answers come from the schema faker, so values differ run to run; the shape
(GitHub's node id prefixes, URL forms, ISO timestamps) does not:

```json
{
  "id": 100563368,
  "node_id": "R_kgDOz44KaC",
  "name": "a3smp",
  "full_name": "nw0bk7-/ahmt030v5nmtrk5",
  "private": true,
  "owner": { "login": "zo9mf-sqf", "id": 461989, "type": "User", ... },
  "html_url": "https://github.com/sfx5uz/ub2q6kew-bphh3a970d",
  "stargazers_count": 6,
  "default_branch": "main",
  "visibility": "private",
  "created_at": "2018-12-07T20:46:00Z",
  ...
}
```

The Python snippet prints lines like:

```
nw0bk7-/ahmt030v5nmtrk5 6 main
1966 open https://github.com/nvpu9v9cq37pzx1/ihony/issues/3726
```

## Scenarios

```bash
curl -s localhost:8080/repos/fernwood-labs/trellis -H 'Host: api.github.com' \
  -H 'X-Mockingbird-Scenario: rate-limited'
# HTTP 403 {"message":"API rate limit exceeded for user ID 58213947. ...","documentation_url":"https://docs.github.com/rest/overview/rate-limits-for-the-rest-api","status":"403",...}
```

GitHub answers an exhausted primary rate limit with 403 (secondary limits with 429),
so `rate-limited` uses 403 and keeps working with the full upstream spec. PyGithub
recognises the message and raises `RateLimitExceededException` (pass `retry=None`,
or its default retry policy waits first). The scenario's `headers:` send
`x-ratelimit-limit: 5000`, `x-ratelimit-remaining: 0`, `x-ratelimit-used` and
`x-ratelimit-resource: core`, as GitHub does.
`not-found` answers 404 `{"message":"Not Found",...}` on `repos/get`, `issues/get`,
`pulls/get`, `users/get-by-username`, `apps/get-installation` and
`apps/get-repo-installation`; `outage` answers 503. Set
`default_scenario: rate-limited` on the mock to apply it to every call.

## Webhooks

Run your webhook handler on port 3000 (or change `url`) with the same secret the mock signs with:

```bash
export GITHUB_WEBHOOK_SECRET=test_local_secret   # any value; the mock and your app share it
mockingbird serve -c mockingbird.yaml --offline
```

Each successful `POST /repos/{owner}/{repo}/issues` sends an `issues` payload
(`{"action":"opened","issue":{...},"repository":{...},"sender":{...}}`) and each
`POST /repos/{owner}/{repo}/pulls` a `pull_request` payload (`action`, `number`,
`pull_request`, `repository`, `sender`), generated from the contract's `webhooks`.
`mockingbird webhooks deliveries github-rest` lists the attempts.

The signature is the one GitHub computes (`sha256=<hex HMAC-SHA256(secret, body)>`),
but it arrives in `X-Mockingbird-Signature`, not `X-Hub-Signature-256`, and there is
no `X-GitHub-Event` or `X-GitHub-Delivery` header (`X-Mockingbird-Event-Type` carries
`issue.created` or `pull.created`). A handler built on `@octokit/webhooks` needs the
header name mapped, for example:

```js
const sig = req.headers["x-hub-signature-256"] ?? req.headers["x-mockingbird-signature"];
```
