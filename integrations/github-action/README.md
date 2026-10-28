# Mockingbird GitHub Action

Starts a Mockingbird server for the rest of the job and sends the job's
HTTP(S) traffic through its proxy, so the app under test talks to its real
API hosts (`api.stripe.com`, `api.github.com`, …) and gets mock answers,
with no code or config changes in the app. A second step writes what the
mocks answered to the job summary and can fail the job on fallbacks,
unmapped hosts or missing calls.

```yaml
jobs:
  test:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - uses: RJ-s-Org/Mockingbird/integrations/github-action@v0
        with:
          config: mockingbird.yaml
          mode: replay            # answers from .mockingbird/snapshot, no LLM key
      - run: npm ci && npm test   # HTTPS_PROXY and the CA bundle are already set
      - uses: RJ-s-Org/Mockingbird/integrations/github-action/report@v0
        if: always()
        with:
          fail-on-fallback: true
          verify: --no-unmapped --called createPaymentIntent
```

The setup and the report are two steps because a composite action cannot
run a post step; give the report step `if: always()` so it also runs (and
stops the server) when the tests fail.

## Modes

| `mode` | Answers from | Needs |
|---|---|---|
| `offline` (default) | the faker, from the contract's schemas | nothing |
| `replay` | the answers committed in `snapshot` (exact prompt first, then any answer for the same operation); the faker on a miss. Never calls an LLM. | a snapshot recorded with `mockingbird serve --record` (see [docs/ci.md](../../docs/ci.md#deterministic-ci-record-once-replay-everywhere)) |
| `record` | the config's LLM profile; every answer is appended to `snapshot` | the profile's key as a step env var |
| `llm` | the config's LLM profile | the profile's key as a step env var |

An LLM profile reads its key from the environment (`api_key: env:OPENROUTER_API_KEY`
in `mockingbird.yaml`); pass the secret on the step and the server inherits it:

```yaml
      - uses: RJ-s-Org/Mockingbird/integrations/github-action@v0
        with: { config: mockingbird.yaml, mode: llm }
        env:
          OPENROUTER_API_KEY: ${{ secrets.OPENROUTER_API_KEY }}
```

## Setup inputs (`integrations/github-action`)

| Input | Default | |
|---|---|---|
| `version` | `v0.1.0` | Release tag to install, or `latest`. The binary is cached per version, OS and architecture. |
| `binary` | | A mockingbird binary to use instead (a source build); skips the install. |
| `config` | | Config file. Watching for changes is off in CI. |
| `contracts` | | Contract files to serve without a config, space-separated. |
| `mode` | `offline` | `offline`, `replay`, `record` or `llm` (above). |
| `snapshot` | `.mockingbird/snapshot` | Snapshot directory for `replay` and `record`. |
| `proxy` | `true` | Start the proxy and export its environment to later steps. |
| `port`, `admin-port` | from the config | Data-plane and Admin API ports. |
| `wait` | `60s` | How long to wait for readiness; the step fails after that. |
| `state-dir` | `$RUNNER_TEMP/mockingbird` | Pid, server log and reports. |

Outputs: `data-plane`, `admin`, `proxy` (URLs) and `version`.

Later steps get `HTTPS_PROXY`, `HTTP_PROXY` (and the lowercase forms),
`NO_PROXY=localhost,127.0.0.1`, `NODE_EXTRA_CA_CERTS`, `SSL_CERT_FILE`,
`REQUESTS_CA_BUNDLE` and `NODE_USE_ENV_PROXY=1` (with `proxy: true`), plus
`MOCKINGBIRD_URL` (the data plane), `MB_ADMIN_URL` and `MB_STATE_DIR`, and
`mockingbird` on the `PATH`. Every later step goes through the proxy,
including other actions: with `proxy.unmapped: block`, a step that talks
to GitHub (`actions/upload-artifact`, `actions/cache`) or a package
registry needs the proxy variables cleared before it (`printf '%s=\n' HTTPS_PROXY
HTTP_PROXY https_proxy http_proxy >> "$GITHUB_ENV"` in a step; a step's
`env:` cannot set the lowercase and uppercase names apart), as the report
step does before its upload; with `pass` (the default) they are
forwarded. See [docs/proxy.md](../../docs/proxy.md) for
which runtimes honour which variable (Java and some Go clients need a
flag).

## Report inputs (`integrations/github-action/report`)

| Input | Default | |
|---|---|---|
| `fail-on-fallback` | `false` | Fail when an answer fell back to the faker because the LLM call or the replay failed (faker answers in `offline` mode are not fallbacks). |
| `verify` | | More [`mockingbird verify`](../../docs/ci.md#report-and-verify) flags, e.g. `--no-unmapped --no-errors --called orders/getOrder=2`. |
| `junit` | `true` | Also write JUnit XML (one test case per operation). |
| `artifact` | `mockingbird` | Artifact name for the report, the request log of every mock and the server log; empty uploads nothing. |
| `stop` | `true` | Stop the server afterwards. |

## Docker instead of the binary

Without the proxy (the app is configured with the mock's URL), the image
can run the mocks. GitHub's `services:` containers cannot be given a
command, and the image's default command is `--help`, so start the
container in a step, after the checkout, with the config mounted:

```yaml
      - uses: actions/checkout@v4
      - name: Start Mockingbird
        run: |
          docker run -d --name mockingbird -v "$PWD:/work:ro" -p 8080:8080 -p 9090:9090 \
            ghcr.io/rj-s-org/mockingbird:0.1.0 \
            serve -c /work/mockingbird.yaml --host 0.0.0.0 --insecure-no-auth --offline
          for _ in $(seq 1 60); do curl -fsS http://localhost:9090/readyz && exit 0; sleep 1; done
          docker logs mockingbird; exit 1
      - run: npm test
        env:
          ORDERS_API_URL: http://localhost:8080/orders
```

With `--replay /work/.mockingbird/snapshot` instead of `--offline` it
replays a committed snapshot. `mockingbird report` and `verify` work
against the container from the runner with `--admin-url
http://localhost:9090`. In GitLab, CircleCI and Buildkite, where service
containers can take a command, see [docs/ci.md](../../docs/ci.md).
