# Mockingbird in CI

A CI job starts Mockingbird in the background, runs the app's tests
through its proxy (so the app calls its real API hosts and gets mock
answers, with no code changes), then reports what the mocks answered and
fails on what should not happen: fallbacks, hosts no mock claims, missing
calls.

```sh
mockingbird server start -c mockingbird.yaml --no-watch --proxy --replay .mockingbird/snapshot
eval "$(mockingbird proxy env -c mockingbird.yaml)"
npm test
mockingbird report --format markdown
mockingbird verify --no-fallbacks --no-unmapped
mockingbird server stop
```

- [Deterministic CI: record once, replay everywhere](#deterministic-ci-record-once-replay-everywhere)
- [Report and verify](#report-and-verify)
- [GitHub Actions](#github-actions) · [GitLab CI](#gitlab-ci) · [CircleCI](#circleci) · [Jenkins](#jenkins) · [Buildkite](#buildkite) · [Azure Pipelines](#azure-pipelines)

## Starting the server

`mockingbird server start` runs `serve` in the background (pid and log in
`--state-dir`, default `.mockingbird`) and waits until `/readyz` answers.
It prints the URLs as JSON and **exits 1 when the server is not ready
within `--wait`** (default `60s`), printing the end of the log, so a bad
config fails the job at once. Use `--no-watch` in CI. `--offline`,
`--replay DIR` and `--record DIR` pick where answers come from; without
them the config's LLM profile is used, with its key from the environment.

`mockingbird proxy env` prints the variables that route an app through the
proxy and make it trust the local CA (`--json` for a machine-readable
form). Which runtimes honour them is in [proxy.md](proxy.md).

## Deterministic CI: record once, replay everywhere

LLM answers vary from run to run and need a key. For CI, record them once
on a developer machine and commit them:

```sh
# 1. Record: run the app's tests against the LLM-backed mocks.
mockingbird server start -c mockingbird.yaml --proxy --record .mockingbird/snapshot
eval "$(mockingbird proxy env -c mockingbird.yaml)"
npm test
# 2. Optionally keep the session state the tests built (or that you seeded).
mockingbird snapshot export --dir .mockingbird/snapshot
mockingbird server stop
git add .mockingbird/snapshot
```

In CI, `--replay .mockingbird/snapshot` answers from the snapshot and
**never calls an LLM**, so no key is needed:

```sh
mockingbird server start -c mockingbird.yaml --no-watch --proxy --replay .mockingbird/snapshot
```

How a replay answers:

- Answers are matched on the prompt Mockingbird would send, which holds
  the operation, its response schema and status, the request (path
  parameters, query, body) and the session's stored resources. Recording
  at the LLM, not at the HTTP layer, means every protocol (REST, GraphQL,
  gRPC, SOAP, streams, events) replays, and state, the response cache and
  consistent ids rebuild themselves from the same answers.
- An exact prompt match is used first; a prompt seen several times gets
  its answers in recorded order, the last one repeating. Otherwise any
  answer recorded for the same operation and status is used (still
  validated against the schema). Otherwise the faker answers, and the
  request log records a `fallback` with the reason *no answer recorded in
  the snapshot*, which `verify --no-fallbacks` fails on.
- `verify --exact-replay` also fails when an answer came from another
  request of the same operation, for suites that must match exactly.
- Requests that change between runs (timestamps, random ids in bodies)
  only match at the operation level. A contract or prompt change (a new
  Mockingbird version can change prompts) turns exact matches into
  operation-level ones or misses: record again.
- `--replay` and `--record` on the same directory replay what is there and
  record misses from the LLM: a quick way to top up a snapshot.

The snapshot directory:

| File | Written by | |
|---|---|---|
| `answers.jsonl` | `serve --record` | One recorded answer per line (prompt digests and the answer text). Re-recording appends; delete it to start over. |
| `sessions/<mock>/<session>.json` | `snapshot export` | Each session's stored resources, seeded on `serve --replay` and by `snapshot import`. |
| `mocks/<mock>.json`, `contracts/<mock>.<ext>` | `snapshot export` | The mocks and their active contracts, for `snapshot import` into a server without the config (mocks created through the API or the console). |
| `snapshot.json` | `snapshot export` | Format version and the workspace the sessions came from. |

The answers and sessions hold what the LLM generated for your tests:
synthetic data, but review them like any fixture before committing.

`mockingbird snapshot import --dir DIR` creates the snapshot's mocks that
the running server lacks (from their contracts) and seeds its sessions,
through the Admin API (`--admin-url`, `--token`, `--workspace` as for the
other client commands).

## Report and verify

`mockingbird report` reads the running server's request log and
`/metrics` through the Admin API:

- per mock and operation: requests, response sources (`llm`, `repaired`,
  `cache`, `state`, `faker` for offline answers, `fallback` when the LLM
  call or the replay failed), 4xx and 5xx counts;
- fallback reasons (each names the failing JSON pointer and schema
  keyword, for example `/owner: anyOf: no branch matches; closest:
  /owner/login: type: 7 is not of type "string"`);
- routes no operation of the contract matches (`METHOD path`, answered
  404 or 405);
- hosts the proxy saw that no mock claims, with the `proxy.unmapped` mode
  (`pass` or `block`) and request counts;
- snapshot answers by result (`exact`, `operation`, `miss`, `recorded`).

`--format json` (default), `markdown` (for a job summary or merge request
comment) or `junit` (one test case per operation, failing on fallbacks or
5xx, and one per blocked host). `-o FILE` writes to a file.

`mockingbird verify` checks the same data and exits 1 when a check fails.
By default (`--format text`) it prints the totals and one short line per
failed check, naming the three most frequent fallback reasons or routes
(cut to 120 characters) and how many more there are; `--format json`
carries the whole report with every reason in full, and `markdown` or
`junit` as for `report` (each failure then also goes to stderr):

| Flag | Fails when |
|---|---|
| `--no-fallbacks` | an answer fell back to the faker because the LLM call or the replay failed |
| `--no-unmapped` | the proxy saw a host no mock claims |
| `--no-blocked` | the proxy blocked a host no mock claims |
| `--no-unmatched` | a request reached a mock but matched no operation of its contract (a route the contract lacks) |
| `--no-errors` | a mock answered 5xx |
| `--no-client-errors` | a mock answered 4xx (for example a request the contract rejects) |
| `--exact-replay` | a replay used an operation-level match |
| `--called OP[=N]` | operation `OP` (contract operation id; `mock/OP` for one mock) was called fewer than N times (default 1); repeatable |

Both see what the request log keeps: the last
`observability.request_log_retention` requests per mock (1000 by default;
raise it for long suites). Unmapped hosts are counted by the proxy that
`serve --proxy` runs; a standalone `mockingbird proxy` process keeps its
own counts.

## GitHub Actions

Use the action in [integrations/github-action](../integrations/github-action/README.md):

```yaml
      - uses: RJ-s-Org/Mockingbird/integrations/github-action@v0
        with:
          config: mockingbird.yaml
          mode: replay
      - run: npm test
      - uses: RJ-s-Org/Mockingbird/integrations/github-action/report@v0
        if: always()
        with:
          fail-on-fallback: true
          verify: --no-unmapped
```

It installs a pinned release (cached), starts the server, exports the
proxy environment to later steps, and the report step writes the Markdown
report to the job summary, uploads the report, the request log and the
server log as an artifact, and fails on the checks. Running the Docker
image instead is described there too.

## GitLab CI

[integrations/gitlab/mockingbird.yml](../integrations/gitlab/mockingbird.yml)
defines a hidden `.mockingbird` job: its `before_script` installs the
release binary (cached per version), starts the server and exports the
proxy environment into the job's shell; its `after_script` writes the
report (JUnit XML is attached as a test report) and stops the server.
`after_script` cannot fail a job, so run `verify` in `script`:

```yaml
include:
  - remote: https://raw.githubusercontent.com/RJ-s-Org/Mockingbird/v0.1.0/integrations/gitlab/mockingbird.yml
    inputs:
      config: mockingbird.yaml
      mode: replay

test:
  extends: .mockingbird
  image: node:22
  script:
    - npm ci && npm test
    - mockingbird verify --no-fallbacks --no-unmapped
```

The image needs `curl`, `tar` and `sha256sum` (Debian-based images have
them; on Alpine add `apk add curl`). A job with its own `before_script`
runs `- !reference [.mockingbird, before_script]` first.

As a service container (the app calls the mock's URL, no proxy):

```yaml
test:
  services:
    - name: ghcr.io/rj-s-org/mockingbird:0.1.0
      alias: mockingbird
      command: ["serve", "-c", "/builds/$CI_PROJECT_PATH/mockingbird.yaml", "--host", "0.0.0.0", "--insecure-no-auth", "--offline"]
  variables:
    ORDERS_API_URL: http://mockingbird:8080/orders
```

## CircleCI

```yaml
jobs:
  test:
    docker:
      - image: cimg/node:22.11
    steps:
      - checkout
      - run:
          name: Install and start Mockingbird
          command: |
            curl -fsSL https://raw.githubusercontent.com/RJ-s-Org/Mockingbird/main/install.sh | MOCKINGBIRD_VERSION=v0.1.0 sh
            echo 'export PATH="$HOME/.mockingbird/bin:$PATH"' >> "$BASH_ENV"
            ~/.mockingbird/bin/mockingbird server start -c mockingbird.yaml --no-watch --proxy --replay .mockingbird/snapshot
            ~/.mockingbird/bin/mockingbird proxy env -c mockingbird.yaml >> "$BASH_ENV"
      - run: npm ci && npm test
      - run:
          name: Mockingbird report
          when: always
          command: |
            mkdir -p reports
            mockingbird report --format junit -o reports/mockingbird.xml
            mockingbird verify --no-fallbacks --no-unmapped
      - store_test_results:
          path: reports
```

`$BASH_ENV` carries the proxy variables to the later steps.

## Jenkins

```groovy
pipeline {
  agent any
  environment { PATH = "${env.HOME}/.mockingbird/bin:${env.PATH}" }
  stages {
    stage('Mocks') {
      steps {
        sh 'curl -fsSL https://raw.githubusercontent.com/RJ-s-Org/Mockingbird/main/install.sh | MOCKINGBIRD_VERSION=v0.1.0 sh'
        sh 'mockingbird server start -c mockingbird.yaml --no-watch --proxy --replay .mockingbird/snapshot'
      }
    }
    stage('Test') {
      steps {
        // Each sh step is a new shell: load the proxy variables in the same one.
        sh 'eval "$(mockingbird proxy env -c mockingbird.yaml)" && npm ci && npm test'
      }
    }
  }
  post {
    always {
      sh 'mockingbird report --format junit -o mockingbird.xml || true'
      junit allowEmptyResults: true, testResults: 'mockingbird.xml'
      sh 'mockingbird verify --no-fallbacks; status=$?; mockingbird server stop; exit $status'
    }
  }
}
```

## Buildkite

```yaml
steps:
  - label: test
    command: |
      curl -fsSL https://raw.githubusercontent.com/RJ-s-Org/Mockingbird/main/install.sh | MOCKINGBIRD_VERSION=v0.1.0 sh
      export PATH="$HOME/.mockingbird/bin:$PATH"
      mockingbird server start -c mockingbird.yaml --no-watch --proxy --replay .mockingbird/snapshot
      trap 'mockingbird report --format markdown | buildkite-agent annotate --style info; mockingbird server stop' EXIT
      eval "$(mockingbird proxy env -c mockingbird.yaml)"
      npm ci && npm test
      mockingbird verify --no-fallbacks --no-unmapped
```

The Markdown report becomes a build annotation.

## Azure Pipelines

```yaml
steps:
  - script: |
      curl -fsSL https://raw.githubusercontent.com/RJ-s-Org/Mockingbird/main/install.sh | MOCKINGBIRD_VERSION=v0.1.0 sh
      echo "##vso[task.prependpath]$HOME/.mockingbird/bin"
      ~/.mockingbird/bin/mockingbird server start -c mockingbird.yaml --no-watch --proxy --replay .mockingbird/snapshot
      ~/.mockingbird/bin/mockingbird proxy env -c mockingbird.yaml --json \
        | jq -r '.vars | to_entries[] | "##vso[task.setvariable variable=\(.key)]\(.value)"'
    displayName: Start Mockingbird
  - script: npm ci && npm test
  - script: |
      mockingbird report --format junit -o $(Build.ArtifactStagingDirectory)/mockingbird.xml
      mockingbird verify --no-fallbacks --no-unmapped
    displayName: Mockingbird report
    condition: always()
  - task: PublishTestResults@2
    condition: always()
    inputs:
      testResultsFiles: $(Build.ArtifactStagingDirectory)/mockingbird.xml
```

`task.setvariable` turns each proxy variable into an environment variable
of the later steps.
