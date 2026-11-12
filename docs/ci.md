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
- [Report and verify](#report-and-verify) · [Run the app's tests: `verify --run`](#run-the-apps-tests-verify---run)
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
| `mocks/<mock>.json`, `contracts/<mock>.<ext>` | `snapshot export` | The mocks and their active contracts, for `snapshot import` into a server without the config (mocks created through the API or the console). A mock file carries `name`, `route_prefix`, `persona`, `consistency`, `unknown_ids`, `hosts`, `path_rewrite` and `header_overrides`, plus `chaos`, `limits` and `default_scenario` when exported with `--include` (below). Never the LLM profile, data-plane auth or webhooks, which name profiles, secrets and targets of the source environment (set them on the target after the import). |
| `scenarios/<mock>/<scenario>.json` | `snapshot export --include scenarios` | Each scenario of the mock, as its `PUT /mocks/{mock}/scenarios/{name}` body. |
| `worlds/<mock>.json` | `snapshot export --include world` | The newest [world description](world.md#in-a-snapshot) overlay of the mock's active contract version, added by `snapshot import` as a revision by `user`. |
| `snapshot.json` | `snapshot export` | Format version and the workspace the sessions came from; with `--include`, the parts it added (`"include": ["scenarios", "chaos", "limits"]`). |

By default a snapshot leaves out each mock's scenarios, chaos, limits and world description:
they shape test runs, and a snapshot committed for one suite should not
force errors or latency on another. Opt in per export with `--include`
(comma-separated, or repeated):

```sh
mockingbird snapshot export --dir .mockingbird/snapshot --include scenarios,chaos,limits
```

`scenarios` also exports the mock's default scenario. Without `--include`
the export writes exactly what it always did, and a re-export without
`scenarios` removes a mock's `scenarios/<mock>/` directory. The format
version stays 1, so snapshots exported either way import into this and
older builds; an older build's import restores `chaos` and `limits` (they
are part of the mock it creates) but ignores the scenario files and the
default scenario, so set those on the target yourself.

The answers and sessions hold what the LLM generated for your tests:
synthetic data, but review them like any fixture before committing.

`mockingbird snapshot import --dir DIR` creates the snapshot's mocks that
the running server lacks (from their contracts) and seeds its sessions,
through the Admin API (`--admin-url`, `--token`, `--workspace` as for the
other client commands). When the snapshot holds them, each created mock
also gets its chaos and limits, then its scenarios (after the contract, so
they are checked against it) and its default scenario; its summary counts
them as `restored_scenarios`. Mocks the server already has are left as they
are.

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
- snapshot answers by result (`exact`, `operation`, `miss`, `recorded`);
- `next_steps`, one per host the proxy blocked since the server started
  that no mock lists now: how many times it was blocked (over HTTPS, once
  per `CONNECT`); for a vendor's other host, to add it to the hosts of the
  mock serving its domain if it is the same API; the catalog examples
  whose mocks list the host, or, when none does, to import its contract
  into a mock; and, as a last resort, to let it through
  (`proxy.vendor_subdomains: pass` for a vendor's other host,
  `proxy.unmapped: pass` otherwise). Omitted when nothing is left
  blocked. The MCP `server_status` answer carries the same `next_steps`,
  and `verify --format text` prints them as `Next:` lines when a check
  fails.

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
| `--no-unclaimed-hosts` | the project can call a host no mock lists that a bundled description or catalog example serves ([below](#hosts-the-project-can-call)); scans the project of `-c/--config` (default `./mockingbird.yaml`) and prints the summary as a `Next:` line |
| `--run "<command>"` | the command fails, or the requests it made point at the mock ([below](#run-the-apps-tests-verify---run)) |

Both see what the request log keeps: the last
`observability.request_log_retention` requests per mock (1000 by default;
raise it for long suites). Unmapped hosts are counted by the proxy that
`serve --proxy` runs; a standalone `mockingbird proxy` process keeps its
own counts.

## Run the app's tests: `verify --run`

`mockingbird verify --run "<command>"` runs the app's own command (its
tests, an end-to-end suite) against the mocks and fails on what, in the
requests that command made, points at the mock. The MCP tool `verify_run`
does the same (`command`, `cwd`, `serve`, `serve_port`, `serve_ready`,
`serve_timeout_secs`, `config`, `timeout_secs`, `offline`) and answers the
JSON below.

```sh
mockingbird verify -c mockingbird.yaml --run "npm test"
# tests that call the app's own server (SERVER_URL=http://localhost:4242)
mockingbird verify -c mockingbird.yaml --run "bundle exec rspec spec/checkout_spec.rb" \
  --serve "cd server && npm start" --serve-port 4242
```

- **How it runs.** As `mockingbird run` does: the config's server is used
  when it answers (the project's background server recorded in its state
  directory, wherever it listens, else the one on the config's admin port
  when it was started with this config), else one is started for the run (`--offline`,
  `--state-dir`, default `$MB_STATE_DIR` else `.mockingbird`) and stopped
  after it; the command gets the proxy environment and the CA preloads for
  every runtime. It runs with `sh -c` (`cmd /C`, the line passed as
  written, on Windows) from the folder `verify` was started in, so `--run
  ./x.sh` works from a subfolder (the MCP tool: its `cwd`, a folder of the
  project given relative to the project root, which must exist and may not
  lead out of it; else the project root as
  [below](#hosts-the-project-can-call)), with no stdin, in a process group
  of its own. When it ends, whatever it left running in that group (a
  server it started in the background) is stopped too. A process that
  leaves the group (`setsid`, a daemon that detaches, a container started
  with `docker run -d` or `docker compose up -d`) is not: stop it yourself. The CLI echoes its
  output on stderr as it comes. Ctrl-C or SIGTERM to the CLI stops the
  command's process group and still prints the report, with the command
  marked `interrupted`. Only the command the user or agent passed is run.
- **The app's own server: `--serve "<command>"`.** Many end-to-end and
  integration suites call the app's server, which must run while they
  do. `--serve` starts it first, the same way (the same folder, the proxy
  environment and CA preloads, a process group of its own), so its calls
  to the APIs reach the mocks and count in the window. The tests start
  once it is ready: `--serve-port <port>` (a TCP connect to localhost
  succeeds) and/or `--serve-ready "<regex>"` (a line of its stdout or
  stderr matches; an ECMA 262 regular expression, as a JSON Schema
  `pattern`, found anywhere in the line; colours and other ANSI escapes
  are stripped first, lines end at `\n` and a trailing `\r` is dropped, so
  output a progress bar redraws with bare `\r` reaches the pattern as one
  line). An invalid pattern is refused before anything starts. With neither, the port the
  [test detection](#test-commands-and-the-apps-server) proposes for that
  exact serve command is waited for, else it must still be running after
  3 s (and a note says readiness was only assumed). A port that already
  accepts connections before it starts (or one of Mockingbird's own ports
  in the config, which the next step names), a serve command that exits
  first, or one not ready within `--serve-timeout` (default `60s`, at most
  `10m`; MCP `serve_timeout_secs`, at most 600) is a `serve_failed`
  failure: the tests are not run (`command.not_run`). The readiness wait
  counts apart from `--timeout` (MCP `timeout_secs`): a run takes at most
  their sum, plus the server start and a few seconds to stop and settle.
  After the tests end, however they end (an exit, the timeout, Ctrl-C,
  an error, the MCP client going away), its process group is stopped:
  TERM, then KILL after 5 s. The answer's `serve` has the command, what
  readiness waited for (`ready_by`) and how long it took (`ready_ms`),
  its exit code when it ended on its own (a crash while the tests ran is
  also a note), and the last 30 lines of its output, apart from the
  tests' own.
- **The window.** Before the command starts, the newest request-log id of
  every mock (in every workspace the token sees) and the proxy's
  unmapped-host counts are read. Afterwards, once the request log has
  settled (at most 3 s), only newer entries and the growth of those counts
  are examined, so earlier traffic does not count. Requests other clients
  send to the same server while the command runs do count. A mock whose
  log could not be read beforehand is left out, and `notes` say so; they
  also say when a mock's log no longer reaches back to the command's start
  (`observability.request_log_retention`) or dropped records while it ran.
- **What fails**, each with a `next_step`, each request counted once:

  | `kind` | When |
  |---|---|
  | `unmatched_route` | a request reached a mock but matched no operation of its contract (404 or 405) |
  | `fallback` | an answer fell back to the faker because the LLM call or a replay failed, with the reason (an offline mock's faker answers are not fallbacks) |
  | `error_status` | a mock answered 5xx, rejected a request in validation (`source: rejected`), or, when the command failed, answered another 4xx: the `operation`, `status`, `source` (absent for a problem the mock answered itself: a scenario, a forced status, a world rule such as `refuse` or an unknown id), the problem's detail read from the recorded response, and the `request_id`; the next step names `get_world` and `get_request`, and for a 4xx says to ignore it if the test expected that status |
  | `blocked_host` | the proxy blocked a host no mock claims (the same step as [`report`](#report-and-verify)'s) |
  | `unclaimed_hosts` | the command reached a host that a bundled description or catalog example serves and no mock lists ([below](#hosts-the-project-can-call)) |
  | `check` | an assertion given beside `--run` (`--called`, `--no-unmapped`, `--no-client-errors`, `--exact-replay`) fails on the window |
  | `serve_failed` | the `--serve` command exited, was not ready in time or found its port taken; the tests were not run |
  | `missing_dependencies` | the command exited non-zero, or the `--serve` command failed to start (or exited non-zero while the failing command ran), and its output shows a dependency that is not installed; like `serve_failed`, it does not point at the mock ([below](#missing-dependencies)) |

  When the command exits 0, a 4xx that matched an operation and passed
  validation is taken as a status the test expected (a lookup of a
  missing record, a refused action): it is listed under `notes`, not
  failed on. Also under `notes`: hosts the proxy passed to the real API
  (`proxy.unmapped: pass`), and the project's unclaimed hosts when the
  command called none of them.
- <a id="missing-dependencies"></a>**Missing dependencies.** When the
  command exits non-zero, or the serve command fails before it is ready
  (or exits non-zero while a command that fails runs), the last lines of
  its output are read for a runtime's own error, where the runtime prints
  it: at the start of a line (after `E `, `Error: ` or `Error [ERR_…]: `),
  so a test's assertion that quotes the text does not count. Ruby `Could
  not find gem`, `Bundler::GemNotFound`, `cannot load such file -- X`
  (`LoadError`), also at the end of a raised line; Node `Cannot find
  module 'X'`, `Cannot find package 'X'` (`ERR_MODULE_NOT_FOUND`); Python
  `No module named X` (`ModuleNotFoundError`, `ImportError`, `python -m`);
  Go `cannot find package`, `no required module provides package`,
  `missing go.sum entry`, also after `go: ` or a file position; Java
  `ClassNotFoundException`, `NoClassDefFoundError` after `Exception in
  thread`, after `Caused by:` or at the start of a line a stack frame
  follows (a class probe a library logs is not one); failing those, the
  shell's `command not found`. The last such error wins. A name that is
  the project's own module is not a missing dependency: a path
  (`./routes`, `/app/x.js`), a Node alias (`@/…`, `#…`, `~/…`) or a file
  a `baseUrl` resolves (`x.js`, `x.ts`, `src/x`), a Python package or
  module (`x/`, `x.py`, `src/x`), or a Ruby file (`lib/x.rb`, `x.rb`), in
  the folder the command runs in or the project root.
  The next step names the project's install command, from the files of
  the folder the failing command runs in (after its leading `cd <dir>`
  steps joined with `&&` or `;`; a `|| exit` after one is skipped and
  `cd -` goes back), else of the nearest folder above it up to the
  project root:

  | Project files | Install command |
  |---|---|
  | `Gemfile` | `bundle install` |
  | `pnpm-lock.yaml` | `pnpm install --frozen-lockfile` |
  | `yarn.lock` | `yarn install --immutable` with `.yarnrc.yml` (Yarn 2+), else `yarn install --frozen-lockfile` |
  | `bun.lock`, `bun.lockb` | `bun install` |
  | `package-lock.json` | `npm ci` |
  | `package.json` alone | `npm install` |
  | `uv.lock`, `poetry.lock`, `pdm.lock`, `Pipfile` | `uv sync`, `poetry install`, `pdm install`, `pipenv install --dev` |
  | `requirements.txt` | `pip install -r requirements.txt`, with an `-r` for each of its test and development variants (`requirements-dev.txt`, `requirements_test.txt`, ...); without it, a `requirements/` folder's `test.txt`, `dev.txt`, ... (else `base.txt`), else any `requirements*.txt` (at most 4 files) |
  | `pyproject.toml`, `setup.py` | `pip install -e '.[test]'` when `[project.optional-dependencies]` has a `test`, `tests`, `testing` or `dev` extra, else `pip install -e .` |
  | `go.mod` | `go mod tidy` |
  | `pom.xml` | `mvn -q -DskipTests package` (`./mvnw` when the project has it) |
  | `build.gradle` | `./gradlew build -x test` (`gradle` without the wrapper) |

  A folder with a `package.json` or `pyproject.toml` but no lockfile is
  taken as a workspace member: the install runs from the nearest folder
  above it, up to the git root, with a lockfile or a workspace
  (`pnpm-workspace.yaml` → `pnpm install`, a `package.json` with
  `workspaces` → `npm install`, `uv.lock` or `[tool.uv.workspace]` → `uv
  sync --all-packages`). For example: "The tests failed on a missing
  module `requests` (ModuleNotFoundError). Install the project's
  dependencies (`cd /app/api && pip install -r requirements.txt`), then
  run verify --run again. Run the install in your own shell, not under
  `mockingbird run`." (its proxy may block the package registry). A
  missing toolchain (`mvn`, `bundle`, `node`) is named to install first;
  a missing command no runtime claims gets no project install, only "not
  installed on this machine (or not on PATH)". When the serve command
  failed this way, its `serve_failed` step defers to this one. A failure
  with none of these errors gets no such step.
- **Exit code** 1 when the command exits non-zero, is stopped at the
  timeout or interrupted, or any failure is found; 0 otherwise.
- **Caps.** `--timeout` (default `10m`; the MCP tool's `timeout_secs`
  defaults to 300 and is capped at 1800), after which the command's
  process group is stopped; the last 60 lines of output (each cut to 400
  characters); 5,000 request-log entries examined (newest first;
  `window.truncated` says when the cap stopped the reading); 20 recorded
  responses read for problem details; and 50 failures listed
  (`more_failures` counts the rest). The MCP server stops a running
  command when its client disconnects (stdin closes); it does not act on
  a client's cancel notification, so the timeout bounds a call that is
  given up on.

`--format text` (the default) prints a status line, each failure with a
`Next:` line, the notes, and the output's last lines when the command
failed; `--format json` prints:

```json
{
  "ok": false,
  "command": {"command": "npm test", "cwd": "/app", "exit_code": 0, "timed_out": false,
              "duration_ms": 4210, "output_tail": "...", "output_truncated": false},
  "window": {"requests": 2, "truncated": false, "server_started": true, "admin": "http://127.0.0.1:9090"},
  "failures": [
    {"kind": "unmatched_route", "mock": "orders", "status": 404, "count": 1, "request_id": 1,
     "detail": "orders: GET /nope matched no operation of its contract (answered 404, once)",
     "next_step": "Mock `orders` has no operation for GET /nope: ..."},
    {"kind": "error_status", "mock": "orders", "operation": "listOrders", "status": 400, "source": "rejected",
     "count": 1, "request_id": 2,
     "detail": "orders: listOrders answered 400 (once; source rejected: the request failed the contract's validation): Invalid request: query/status: enum: ...",
     "next_step": "Mock `orders` answered 400 to operation listOrders. If the app's call is right, the mock is wrong: check its world (get_world {mock: orders}) and this request (get_request {mock: orders, id: 2}; ...) ..."}
  ],
  "notes": ["orders: getOrder answered 404 (once; no source: ...); the command passed, so it is taken as a status the test expected"]
}
```

`markdown` and `junit` are not available with `--run`; `--timeout`,
`--offline`, `--state-dir` and `--serve` need `--run`, and
`--serve-port`, `--serve-ready` and `--serve-timeout` need `--serve`.

### Test commands and the app's server

The MCP `start_server` and `server_status` answers name the app's test
commands in one `next_steps` line, found from the project root and its
folders up to two levels down (dependencies, build outputs and hidden
folders skipped; at most 4,000 directory entries and files looked at in
all, pairs included, and 8 commands listed; the result is reused while
the project's files look unchanged, for up to 30 s): `package.json` `scripts.test` (`npm test`, or `pnpm` /
`yarn` beside their lock file; npm's placeholder script is left out) and
other test scripts (`test:e2e`, `test-integration`, `e2e`, ...: `npm run
<name>`; not watch, lint, type check or format scripts); a Makefile `test`
target (else a `test-*`, `test_*` or `e2e` one); pytest (`pytest.ini`,
`[tool.pytest…]` in `pyproject.toml`, `[tool:pytest]` in `setup.cfg`,
`[pytest]` in `tox.ini`, `conftest.py`, or `tests/test_*.py`); RSpec
(`spec/` with `.rspec` or a Gemfile naming rspec: `bundle exec rspec`,
else `rspec`); `go test ./...` (`go.mod` with a `*_test.go` file);
`cargo test` (`Cargo.toml`). Under `app_tests` every command reads `cd
<absolute folder> && <command>`, so it runs from any folder; the
`next_steps` line gives them from the project root (`cd <dir> && …` for a
subfolder), where the MCP tool runs them by default. A subfolder's `cargo
test` or pytest is left out below a folder that has one (a Cargo
workspace, a pytest rootdir).

Tests that need the app's own server get a pair (at most 8), and the
`next_steps` line then leads with one, ready to run from the project root
(the MCP tool's default `cwd`): `verify_run {command: "bundle exec rspec
spec/checkout_server_spec.rb", serve: "cd checkout/server/node && npm
start", serve_port: 4242}`. The answer then also lists every command and
pair under `app_tests` (`commands`, `pairs`; at most 6,000 characters,
`left_out` counting what did not fit), each command as `cd <absolute
folder> && …` so it runs from any folder. A pair comes from:

- Playwright's `webServer` (`command`, `port` or `url`, `cwd`) only when
  it sets `reuseExistingServer: true`: otherwise Playwright starts the
  server itself;
- a local base URL with a port in a test helper: a whole identifier such
  as `SERVER_URL`, `BASE_URL`, `baseURL` or `APP_URL` (not
  `DATABASE_URL` or `OPENAI_BASE_URL`) on a line that is not a comment,
  set to `http://localhost:<port>` (or 127.0.0.1, 0.0.0.0, [::1]), on a
  port that is not one of the config's own (server, admin, gRPC, proxy).
  The helper decides the test command: `spec/spec_helper.rb` and
  `spec/rails_helper.rb` the RSpec one, `conftest.py` pytest,
  `playwright.config.*` the script that runs Playwright (else `npx
  playwright test`), `cypress.config.*` Cypress (else `npx cypress run`),
  `jest.setup.*` and `setupTests.*` Jest or Vitest (else `npx jest`), and
  `.env.test` the folder's first command.

The server it pairs with:

- in a project of several samples, each with an `<x>/server` folder, one
  pair per sample that the runner's test files name: each file belongs to
  the sample with the longest name it starts with
  (`spec/checkout_flow_server_spec.rb` to `checkout-flow`, not
  `checkout`), and goes on the test command (RSpec, pytest, and `npm test
  -- <files>`); when some of a sample's files also name the server
  (`_server_`), only those run and the others are listed in the pair's
  `other_tests`;
- else (or when no file names a sample) the one `<x>/server` or `server/`
  folder, else a `start`, `dev` or `serve` script in `package.json` next
  to the tests (or at the root).

A server folder that holds a folder per language is started from one with
a start command (a `package.json` `start`, `dev` or `serve` script;
`app.py`, `server.py` or `main.py`; `server.rb` or `app.rb`; a Go `main`):
first one whose dependencies are installed (`node_modules`, a `.venv` or
`venv`, `vendor/bundle`), then one in the test runner's language, then
Node, Python, Ruby and Go, then the shortest name.

## Hosts the project can call

The report only sees hosts the app called. `mockingbird hosts -c
mockingbird.yaml` (MCP `project_hosts`) finds the vendor hosts the app
*can* call before it runs, from its files, and lists those no mock claims.

It scans the **project root**: the nearest folder at or above the
config's folder that holds `.git` or a package manifest (`package.json`,
`pyproject.toml`, `Gemfile`, `go.mod`, `Cargo.toml`, `pom.xml`,
`build.gradle`, ...), never climbing to the home folder; without one, the
config's folder. A root that is the home folder or a filesystem root is
refused (an error): keep the config in the app's project folder. Two
signals:

- **Bundled API descriptions**, anywhere in the project, installed
  dependencies included (`node_modules`, `vendor`, `.venv`,
  `site-packages`, `.gems`, ...): JSON and YAML files that are OpenAPI 3
  (the hosts of `servers[].url`, at the top, per path or per operation,
  with `{variables}` set to their defaults) or Swagger 2 (`host`). In a
  dependency, a JSON file is parsed only when it starts with `{` and its
  first 8 KiB contain one of the keys `"openapi"`, `"swagger"`,
  `"paths"`, `"components"`, `"servers"` or `"basePath"` (at any depth,
  since writers may put `openapi` after a large `components`); a YAML
  file when a line of its first 8 KiB starts with one of them. The app's
  own JSON and YAML files are read whole (up to 4 MiB) and sniffed the
  same way over all of it. The parse then requires a top-level `openapi`
  or `swagger` key; a member of an unexpected shape (a scalar where an
  object belongs) is ignored, not the whole file. Each host is recorded
  with the file, its `info.title` and the number of operations served on
  it; a file that names the host but serves no operation there is left
  out beside one that does. Dependencies' `test`, `fixtures` and
  `examples` folders are skipped.
- **URL literals** (`https://host`) in the app's own code and configuration
  files (not its dependencies, its test folders `test`, `tests`,
  `__tests__`, `spec`, `specs`, `e2e`, documentation, package manifests,
  lock files, or a file that parsed as a description), kept only when the
  host shares a registrable domain (eTLD+1) with a host of a bundled
  description or a mocked host, or is itself a host a catalog example's
  mock lists, so documentation links stay out.

Each listed host carries its `evidence`: `spec` (a bundled description
serves it), `catalog` (a catalog example serves it) or `literal` (only a
URL in the app's files names it). `literal` hosts are the weaker tier:
listed, but `verify --no-unclaimed-hosts` does not fail on them.

The walk never enters `.git`, the config's own folder (when it is not the
root), CI folders (`.github`, ...) or symlinks, so it never leaves the
root, and leaves out every file a mock of the config names (`contract`,
`world`), wherever the config sits. The app's build outputs (`target`,
`dist`, `build`, `out`, `.next`, ...) are skipped, but not inside
dependencies, where packages ship their descriptions in `dist/`. IP
addresses, `localhost` and reserved names (`example.com`, `*.test`, ...)
are ignored. A host counts as claimed when a mock of the config (or of the
running server, for `start_server`, `server_status` and `verify`) lists it
in `hosts`, or it is a `proxy.images` host.

The scan is capped: 200,000 directory entries, 4 MiB per file (a larger
description is skipped and counted under `skipped_large`; a larger source
file is read up to 4 MiB), 256 MiB in all, and 2 s of wall-clock time.
When a cap stops it, `scan.complete` is `false`, `scan.stopped` names the
cap, and the summary and a last `next_steps` entry say the list may be
incomplete. Within one process (an MCP server), a scan is reused while
the modification times of the root and the two levels below it are
unchanged (`scan.cached`); a scan the time budget stopped is reused for
60 s only, and no scan of a root with more than 20,000 entries at those
levels is reused.

The answer, as JSON:

- `project_root`, `config`;
- `scan`: `files_visited`, `files_read`, `bytes_read`, `elapsed_ms`,
  `specs` (descriptions found), `complete`, `stopped`, `cached`;
- `summary`: one line, how many hosts have no mock per domain (with how
  many have a bundled description or a catalog example) and where the
  detail is;
- `unclaimed`: per host no mock lists, its `domain`, `evidence`, the
  `specs` that serve it (`path` relative to the root, `title`,
  `operations`), the `sources` naming it (up to three files) and the
  catalog `examples` whose mocks list it. Several `specs` for one host
  (versions of one product) are all needed, not alternatives: import each
  into its own mock listing the host, with `route_prefix` and
  `path_rewrite.add_prefix` set to the same value, its version path (say
  `/v2`): `path_rewrite` applies below `route_prefix`, so the prefix alone
  would cut the version off the path the contract sees. Leave such mocks
  without a common `mocks.<name>.shares` group, so the versions' records
  stay apart, unless the descriptions are views of the same records;
- `claimed`: the hosts found that a mock lists;
- `next_steps`: one per registrable domain, about 500 characters each
  (domains with a mocked host first, then those with a description or
  catalog example; at most 8, the rest counted in one more entry). Each
  names its hosts by short labels (`verify` for `verify.vendor.com`):
  those with a bundled description under the directory of their files
  (largest APIs first, `(n)` marking n versions) to `import_contract`,
  those in the catalog with the example to `init_example`, and those only
  named in the app's files.

The MCP `start_server` (with a config) and `server_status` answers append
the `summary` line to their `next_steps` (after the other steps; a host a
blocked-host step already names is left out of it), and `mockingbird
verify --no-unclaimed-hosts` fails while a `spec` or `catalog` host is
listed, printing the summary as a `Next:` line. `verify -c` also picks the
server whose admin listener that config describes, without `--admin-url`.

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
