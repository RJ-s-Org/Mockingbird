# Reaching mocks by their real host names: `mockingbird proxy`

An app that hardcodes `https://api.github.com` or `https://api.stripe.com` can talk to Mockingbird without source edits. Give the mocks the hosts they stand in for (`mocks.<name>.hosts`), then run the app under `mockingbird run`:

```bash
mockingbird run -c mockingbird.yaml -- npm test
```

## `mockingbird run`

```text
mockingbird run [-c <config>] [--offline] [--wait <duration>] [--keep] [--state-dir <dir>] -- <command> [args…]
```

1. Starts the server with its proxy (`server start --proxy`, watching the config) unless something already answers on the config's admin port. A running server without its proxy gets one in front of it for the duration of the command.
2. Waits until `/readyz` is ready (`--wait`, default `60s`); if it is not, prints the end of the server log and exits 1.
3. Runs the command with the [proxy variables](#the-variables) for every runtime, stdin/stdout/stderr attached. SIGINT and SIGTERM sent to `run` reach the command.
4. Stops what it started (unless `--keep`) and exits with the command's exit code: `128 + n` when signal `n` ended it, `127` when it cannot be started, `1` when Mockingbird itself failed before the command ran.

| Flag | Default | |
| --- | --- | --- |
| `-c`, `--config` | `./mockingbird.yaml` | The config to serve; its `server.*` ports and `proxy.*` settings apply. |
| `--offline` | off | Faker only, no LLM (for a server `run` starts). |
| `--wait` | `60s` | How long to wait for readiness. |
| `--keep` | off | Leave a server `run` started running (stop it with `mockingbird server stop`). |
| `--state-dir` | `.mockingbird` (`MB_STATE_DIR`) | Where the pid and `server.log` of the server `run` starts are kept. |

The server inherits `run`'s environment (LLM keys, webhook secrets), but not the proxy variables, so its own LLM calls go straight to the provider. `storage.url: sqlite://./…` is relative to the directory `run` is started in.

## Setup

```yaml
# mockingbird.yaml
proxy:
  enabled: true            # or: mockingbird serve --proxy
  images: [image.tmdb.org] # optional: placeholder images for these hosts
mocks:
  github-rest:
    contract: github-rest.yaml
    route_prefix: /
    hosts: [api.github.com]
  github-graphql:
    contract: github.graphql
    route_prefix: /graphql
    hosts: [api.github.com]
```

```bash
mockingbird serve -c mockingbird.yaml --offline
```

Then, in the app's shell (or use `mockingbird run`):

```bash
eval "$(mockingbird proxy env)"
```

Pass `--watch` to `serve` so edits to the config (a new `mocks:` block, new `hosts`) apply without a restart; `server start` and `mockingbird run` watch by default. A server without `--watch` says at startup that it reads the file once. The `proxy:` section itself is read at startup only: a reload that changes it logs a warning asking for a restart.

`mockingbird proxy env --json` prints the same variables as JSON, with the CA, bundle, truststore and Node.js preload paths and the CA's SPKI hash. `-c <config>` and `--port` pick up a non-default `server.host`, `proxy.port` or `proxy.ca_dir`.

To put the proxy in front of a server that runs without `proxy.enabled` (for example one started elsewhere), run it on its own:

```bash
mockingbird proxy --data-url http://127.0.0.1:8080 --admin-url http://127.0.0.1:9090
```

It reads the hosts the server's mocks claim from `GET /proxy/hosts` on the admin port (refreshed every 2 seconds) and forwards intercepted requests to the data plane with their `Host` header, so they dispatch the same way.

## How it works

- **Mapped hosts are intercepted.** A `CONNECT` to a host a mounted mock lists in `hosts` (or to a `proxy.images` host) is answered by the proxy itself. It terminates TLS with a certificate for that host, signed by the local CA, and serves the requests from the data plane. Dispatch filters mocks by `Host`, then picks the longest route prefix, so two mocks can share a prefix on different hosts. Plain `http://` requests to a mapped host are served the same way.
- **Unmapped hosts are passed through or blocked.** With `proxy.unmapped: pass` (the default) other hosts are tunnelled to the real server untouched; with `block` the proxy answers 502 `application/problem+json` naming the host. Each unmapped host is logged once.
- **The CA stays local.** It is created on first use in `~/.mockingbird/ca` (`proxy.ca_dir`, `MB_CA_DIR`): `ca.pem`, and `ca.key` readable by its owner only. It is never installed in a system trust store and no OS settings are changed. Apps trust it through the variables below. `mockingbird ca path` prints the certificate path.
- **Placeholder images.** Requests matching `proxy.images` get a solid-colour PNG chosen from the path, sized from a `w123`-style path segment or an ipx-style `s_400x600`/`w_400` modifier (else 300×450). No LLM call is made. An entry is a host (`image.tmdb.org`) or a host and path prefix (`movies-proxy.vercel.app/ipx`) for an image endpoint on a host a mock serves.
- **Tunnels carry HTTPS or plain HTTP.** A `CONNECT` to a mapped host is answered with TLS when the client starts a handshake, and as plain HTTP otherwise (the Node.js preload tunnels `http://` requests too).
- **A PAC file** is served at `http://127.0.0.1:8888/proxy.pac`: loopback goes direct, every other host through the proxy.
- **A mapped host never leaks to the real one.** A host a mock lists stays intercepted while that mock is not mounted (its contract failed to build, it has none active, or another mock holds its route): requests get 503 problem+json naming the mock, never the real API.
- **Loopback by default.** The proxy binds `proxy.host` (`127.0.0.1`). On any other address `proxy.unmapped` must be `block`, so the port is not an open relay.
- **Start `serve` from a clean shell.** The server's own outbound clients (LLM providers, OIDC, Vault) honour `HTTPS_PROXY` like any app. Started from a shell that ran `mockingbird proxy env`, they reach their providers through the proxy: that works with `pass`, and fails with `block`. Use `outbound.proxy`/`outbound.no_proxy` or a shell without the variables.
- A mock with `hosts` answers only under those hosts: `curl localhost:8080/…` no longer reaches it, but `curl -H 'Host: api.github.com' localhost:8080/…` does.

## The variables

| Variable | Value | For |
| --- | --- | --- |
| `HTTPS_PROXY`, `HTTP_PROXY`, `https_proxy`, `http_proxy` | `http://127.0.0.1:8888` | Every runtime |
| `NO_PROXY`, `no_proxy` | `localhost,127.0.0.1` | Every runtime |
| `SSL_CERT_FILE`, `REQUESTS_CA_BUNDLE`, `CURL_CA_BUNDLE` | `bundle.pem`: the system's root certificates plus the local CA, so passed-through hosts still verify | Python, Ruby, Go and .NET on Linux, curl |
| `NODE_EXTRA_CA_CERTS`, `MOCKINGBIRD_CA_FILE`, `DENO_CERT` | The local CA (`ca.pem`) | Node.js, Deno, Bun |
| `NODE_USE_ENV_PROXY` | `1` | Node.js ≥ 24.5 / 22.21 built-in proxy support |
| `NODE_OPTIONS` | `--require <ca_dir>/node-proxy.cjs`, then any `NODE_OPTIONS` you had | Node.js: every agent and `fetch` (below) |
| `JAVA_TOOL_OPTIONS` | `-Dhttp(s).proxyHost/-Port`, `-Dhttp.nonProxyHosts=localhost\|127.0.0.1`, `-Djavax.net.ssl.trustStore=<ca_dir>/truststore.jks` (JKS, password `changeit`), then any options you had | Java |

The files live next to the CA (`~/.mockingbird/ca` by default) and are rewritten each time the variables are printed. Running `eval "$(mockingbird proxy env)"` twice does not add the preload or the Java options twice.

## Per runtime

- **Node.js:** nothing to install. The preload in `NODE_OPTIONS` sends every `http`/`https` agent through the proxy, including agents an SDK creates itself (stripe-node, `got`, the AWS SDK), and replaces undici's global dispatcher, which `fetch` and the `undici` package share. It honours `NO_PROXY`, trusts the local CA also when a client passes its own `ca`, and does nothing when no proxy variable is set. `axios` reads the proxy variables itself and works too. On Node ≥ 24.5 (or 22.21) `NODE_USE_ENV_PROXY=1` covers `fetch` natively and the preload covers the agents.
- **Python:** `requests` and `httpx` read `HTTPS_PROXY` and `REQUESTS_CA_BUNDLE`/`SSL_CERT_FILE`; `urllib.request` reads `HTTPS_PROXY` and `SSL_CERT_FILE`. Bare `urllib3.PoolManager` ignores proxy variables (use `urllib3.ProxyManager(os.environ["HTTPS_PROXY"])`).
- **Go `net/http`:** `HTTPS_PROXY`; on Linux `SSL_CERT_FILE`. On macOS Go verifies with the system keychain and ignores `SSL_CERT_FILE`, so HTTPS to mocked hosts fails there unless the CA is trusted in the keychain (Mockingbird never does that for you); run Go apps in a Linux container or CI.
- **Ruby `Net::HTTP`** (and gems built on it): `https_proxy` and `SSL_CERT_FILE` with Ruby linked against OpenSSL (rbenv, asdf, Homebrew, Linux). macOS's system Ruby (`/usr/bin/ruby`, LibreSSL) ignores `SSL_CERT_FILE`.
- **Java:** `JAVA_TOOL_OPTIONS` covers `HttpURLConnection`, `java.net.http.HttpClient` and OkHttp. Apache HttpClient reads the system properties only when built with `useSystemProperties()`. The JVM prints `Picked up JAVA_TOOL_OPTIONS` on stderr.
- **.NET `HttpClient`:** `HTTPS_PROXY`/`NO_PROXY`; on Linux the CA through `SSL_CERT_FILE`. On macOS and Windows .NET trusts the system store only.
- **curl:** lowercase `https_proxy`, and `CURL_CA_BUNDLE`.

## Browsers

Server-side calls go through the proxy with the variables above; a browser needs its own settings so client-side requests and images reach the mocks too.

- **Chrome, Chromium, Edge:** `mockingbird proxy browser [-c config] [URL]` starts the first one it finds (or `--browser <path>`, `MB_BROWSER`, Playwright's downloaded Chromium) with `--proxy-server`, a profile of its own under the CA directory and `--ignore-certificate-errors-spki-list=<the CA's key hash>`, so it trusts the local CA without touching the system store. `--arg=<flag>` adds a browser flag (`--arg=--headless=new`); `--print` prints the command as JSON instead.
- **Any browser:** point its automatic proxy configuration at `http://127.0.0.1:8888/proxy.pac` and trust `ca.pem` in that browser.
- **Playwright:**

  ```ts
  // playwright.config.ts
  use: {
    proxy: { server: process.env.HTTPS_PROXY ?? 'http://127.0.0.1:8888' },
    ignoreHTTPSErrors: true,
  },
  ```

  then `mockingbird run -- npx playwright test`. Instead of `ignoreHTTPSErrors`, Chromium can trust only the local CA: `launchOptions: { args: ['--ignore-certificate-errors-spki-list=' + spki] }` with `spki` from `mockingbird proxy env --json` (`ca_spki_sha256`).
- **Cypress:** `mockingbird run -- npx cypress run`. Cypress sends the browser's traffic through its own proxy, which honours `HTTPS_PROXY`/`NO_PROXY` for its upstream requests; it trusts the CA through `NODE_EXTRA_CA_CERTS`. (Not yet exercised in Mockingbird's own trials; Playwright and Chrome are.)
- **Images:** add the image hosts, or the image path on a mocked host, to `proxy.images`, e.g. `[image.tmdb.org, movies-proxy.vercel.app/ipx]`.

## One contract under another host and path

`mocks.<name>.path_rewrite` rewrites the path below `route_prefix` before it is matched against the contract: `strip_prefix` is removed, then `add_prefix` is put in front. The movies app calls its own proxy at `https://movies-proxy.vercel.app/tmdb/<path>`, while TMDB's contract has `/3/<path>`:

```yaml
mocks:
  tmdb:
    contract: tmdb.openapi.json   # TMDB's paths: /3/movie/{movie_id}, …
    route_prefix: /tmdb
    hosts: [movies-proxy.vercel.app]
    path_rewrite:
      add_prefix: /3              # /tmdb/movie/550 → /3/movie/550
```

The same contract can also serve `api.themoviedb.org` from a second mock (`route_prefix: /`, `hosts: [api.themoviedb.org]`).
