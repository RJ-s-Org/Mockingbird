# GitHub GraphQL

A mock of the GitHub GraphQL API (v4) at `https://api.github.com/graphql`, seen by
Mira Okafor of "Fernwood Labs": `viewer`, `user`, `repository` with issues, pull
requests and stargazers, `search`, `rateLimit`, and the `createIssue` and
`addComment` mutations. It shares `api.github.com` with the
[GitHub REST example](../github-rest/README.md); put both mocks in one config to serve
both APIs on one host.

| | |
| --- | --- |
| Contract | [`github.graphql`](github.graphql), a hand-written SDL subset (6 queries, 2 mutations) that keeps GitHub's type, field and argument names |
| Upstream | [octokit/graphql-schema](https://github.com/octokit/graphql-schema) `schema.graphql` (MIT), pinned in [`example.json`](example.json); `mockingbird examples init github-graphql --full` downloads it |
| Hosts | `api.github.com` (path `/graphql`) |
| Scenarios | `rate-limited` (`rateLimit.remaining` 0), `slow` (4 s), `outage` (502 field error) |
| Webhooks | none (GitHub's webhooks are in the REST example) |

## Quickstart

```bash
mockingbird examples init github-graphql --dir github-graphql && cd github-graphql   # or: cd examples/apis/github-graphql
mockingbird serve -c mockingbird.yaml --offline
```

In another shell:

```bash
curl -s localhost:8080/graphql -H 'Host: api.github.com' -H 'Authorization: bearer ghp_test123' \
  -H 'content-type: application/json' \
  -d '{"query":"{ viewer { login name repositories(first: 3) { totalCount nodes { nameWithOwner stargazerCount } } } }"}'
```

[`@octokit/graphql`](https://github.com/octokit/graphql.js), unmodified, through the
proxy (Node ≥ 22.21 or ≥ 24.5, for `NODE_USE_ENV_PROXY`):

```bash
eval "$(mockingbird proxy env)"
npm install @octokit/graphql
cat > viewer.mjs <<'JS'
import { graphql } from "@octokit/graphql";
const gh = graphql.defaults({ headers: { authorization: "token ghp_test123" } });
const { viewer, rateLimit } = await gh(`{
  viewer { login repositories(first: 3) { totalCount nodes { nameWithOwner stargazerCount } } }
  rateLimit { limit remaining }
}`);
console.log(viewer.login, viewer.repositories.totalCount, rateLimit);
JS
node viewer.mjs
```

Drop `--offline` (and configure an LLM, see [the configuration reference](../../../docs/configuration.md#llm))
for answers that follow the persona: `viewer` is `mira-okafor`, node ids carry
GitHub's prefixes, `totalCount` agrees with the list, mutations echo their input.

## Expected output

`--offline` answers come from the schema faker: the selection's shape is right, but
strings (including the custom scalars `DateTime` and `URI`) are placeholder words and
lists hold one item:

```json
{
  "data": {
    "viewer": {
      "login": "sable",
      "name": "quartz",
      "repositories": { "totalCount": 90, "nodes": [{ "nameWithOwner": "raven", "stargazerCount": 51 }] }
    }
  },
  "extensions": { "mockingbird": { "sources": { "viewer": "fallback" }, ... } }
}
```

The Node snippet prints a line like `garnet 100 { limit: 95, remaining: 35 }`.

## Scenarios

```bash
curl -s localhost:8080/graphql -H 'Host: api.github.com' -H 'content-type: application/json' \
  -H 'X-Mockingbird-Scenario: rate-limited' -d '{"query":"{ rateLimit { limit remaining used } }"}'
# {"data":{"rateLimit":{"limit":5000,"remaining":0,"used":5000}},...}

curl -s localhost:8080/graphql -H 'Host: api.github.com' -H 'content-type: application/json' \
  -H 'X-Mockingbird-Scenario: outage' -d '{"query":"{ viewer { login } }"}'
# HTTP 200 {"data":null,"errors":[{"message":"Bad Gateway (forced status 502)","extensions":{"code":"INTERNAL_SERVER_ERROR","status":502},...}]}
```

`@octokit/graphql` throws a `GraphqlResponseError` for `outage`. `slow` adds 4 s to
every root field, for client timeouts.

A GraphQL contract declares no HTTP statuses, so scenarios may only force a 5xx; the
real exhausted-limit answer (`errors[].type: "RATE_LIMITED"`) is closest with the
per-request header `X-Mockingbird-Status: 429`, which yields
`errors[].extensions.code: "RATE_LIMITED"`.
