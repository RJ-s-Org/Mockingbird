# Go + GitHub: a triage digest with go-github

A small Go package that lists a repository's open issues and pull requests with
[go-github](https://github.com/google/go-github) and files a "Triage digest" issue. It
calls `https://api.github.com` like any go-github program; the test reaches the
[GitHub REST example](../../apis/github-rest)'s contract through `mockingbird proxy`.

| File | |
| --- | --- |
| [`digest.go`](digest.go) | `Run(ctx, httpClient, token, owner, repo)`: list, summarise, post |
| [`mockingbird.yaml`](mockingbird.yaml) | The GitHub REST mock on `api.github.com` and a `rate-limited` scenario (403, as GitHub answers an exhausted limit) |
| [`digest_test.go`](digest_test.go) | Starts `mockingbird serve --offline`, builds the HTTP client from `mockingbird proxy env --json`, checks the digest and the rate-limit error |

## Run the test

```bash
cd examples/apps/go-github
MOCKINGBIRD_BIN=$(command -v mockingbird) go test -v ./...
```

```text
=== RUN   TestDigest
=== RUN   TestDigest/posts_a_digest_issue
    digest_test.go:108: fernwood-labs/trellis: 0 issues, 1 pull requests, posted #1558
=== RUN   TestDigest/surfaces_GitHub's_rate_limit
--- PASS: TestDigest (1.54s)
PASS
```

Counts and numbers change run to run offline. The mock uses ports 49080/49090/49888.

Go's `net/http` honours `HTTPS_PROXY`, and on Linux `SSL_CERT_FILE`, so a Go program
run under `eval "$(mockingbird proxy env)"` needs no changes there. On macOS Go verifies
with the system keychain and ignores `SSL_CERT_FILE`, so the test loads the proxy's CA
(`ca_path` in `mockingbird proxy env --json`) into the client's `RootCAs` itself.
