# Jira Cloud

A mock of the Jira Cloud platform REST API v3 for "Juniper & Co", a small plant shop
with two projects (PLNT "Plant Ops" and WEB "Storefront"): create, read and edit issues,
JQL search, workflow transitions, comments, projects and the current user, plus the
unsigned webhooks a Jira admin webhook sends.

| | |
| --- | --- |
| Contract | [`jira.openapi.yaml`](jira.openapi.yaml), a hand-written subset that keeps Atlassian's operation ids (`createIssue`, `getIssue`, `editIssue`, `searchAndReconsileIssuesUsingJql`, `getTransitions`, `doTransition`, `addComment`, ...) |
| Upstream | Atlassian's [`swagger-v3.v3.json`](https://developer.atlassian.com/cloud/jira/platform/swagger-v3.v3.json): no clear redistribution license, so not pinned (see [`example.json`](example.json)) |
| Hosts | `juniper.atlassian.net` |
| Scenarios | `rate-limited` (429), `issue-not-found` (404), `invalid-field` (400), `outage` (503) |
| Webhooks | `issue.created`, `issue.updated`, `comment.created` (bodies carry Jira's `webhookEvent`: `jira:issue_created`, `jira:issue_updated`, `comment_created`) to `http://127.0.0.1:4242/jira/webhook`, unsigned |

## Quickstart

```bash
mockingbird examples init jira --dir jira && cd jira   # or: cd examples/apis/jira
mockingbird serve -c mockingbird.yaml --offline
```

In another shell:

```bash
curl -s localhost:8080/rest/api/3/issue -H 'Host: juniper.atlassian.net' -u ana@example.com:test-token \
  -H 'content-type: application/json' \
  -d '{"fields":{"project":{"key":"PLNT"},"issuetype":{"name":"Task"},"summary":"Repot the fiddle leaf figs in bay 3"}}'
```

The community Python client [`jira`](https://pypi.org/project/jira/) (pycontribs; Atlassian
ships no official Python SDK), unmodified, through the proxy:

```bash
eval "$(mockingbird proxy env)"
pip install jira
python - <<'PY'
from jira import JIRA
jira = JIRA("https://juniper.atlassian.net", basic_auth=("ana@example.com", "test-token"),
            options={"rest_api_version": "3"})
issue = jira.create_issue(project="PLNT", issuetype={"name": "Task"},
                          summary="Repot the fiddle leaf figs in bay 3")
print(issue.key, issue.fields.status.name, "|", issue.fields.summary)
jira.add_comment(issue, {"type": "doc", "version": 1, "content": [
    {"type": "paragraph", "content": [{"type": "text", "text": "Moved to bay 4."}]}]})
print([t["name"] for t in jira.transitions(issue)])
print(len(jira.search_issues("project = PLNT ORDER BY created DESC", maxResults=5)), "issues")
PY
```

`requests` picks up the proxy and the CA bundle from the environment, so nothing else is
needed. The client also calls `getServerInfo`, `getProject` and `getFields`, which the
contract includes.

Drop `--offline` (and configure an LLM, see [the configuration reference](../../../docs/configuration.md#llm))
for answers that follow the persona: plant-shop summaries, the three team members as
assignees, the key you asked for.

## Expected output

`--offline` answers come from the schema faker, so values differ run to run; ids, keys,
`self` links, statuses and account ids follow Jira's formats:

```json
{"id":"10035","key":"PLNT-2810","self":"https://juniper.atlassian.net/rest/api/3/issue/10079"}
```

The Python snippet prints lines like:

```
PLNT-43 In Progress | bravo
['To Do', 'In Progress']
1 issues
```

Offline, the issue read back after the create has a generated summary; with an LLM it
follows the request.

## Scenarios

```bash
curl -s -i localhost:8080/rest/api/3/issue/PLNT-42 -H 'Host: juniper.atlassian.net' \
  -H 'X-Mockingbird-Scenario: issue-not-found'
# HTTP/1.1 404 {"errorMessages":["Issue does not exist or you do not have permission to see it."],"errors":{}}
```

`invalid-field` answers `createIssue`/`editIssue` with 400
`{"errorMessages":[],"errors":{"customfield_10020":"Field 'customfield_10020' cannot be set. ..."}}`,
`rate-limited` answers 429 `{"errorMessages":["Rate limit exceeded."],"errors":{}}`, and
`outage` 503. The `jira` client raises `JIRAError` with `status_code` set for each (it
retries 429 and 503 a few times first). Set `default_scenario` on the mock to apply one to
every call.

## Webhooks

Run your webhook handler on port 4242 (or change `url`):

```bash
mockingbird serve -c mockingbird.yaml --offline
```

Each successful `createIssue` sends `jira:issue_created`, `editIssue` sends
`jira:issue_updated` (with a `changelog`), and `addComment` sends `comment_created`.
Admin webhooks configured in Jira's settings are unsigned, so the mock sends them with
`signing: none`. `mockingbird webhooks deliveries jira` lists the attempts.
