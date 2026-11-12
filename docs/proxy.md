# Reaching mocks by their real host names: `mockingbird proxy`

An app that hardcodes `https://api.github.com` or `https://api.stripe.com` can talk to Mockingbird without source edits. Give the mocks the hosts they stand in for (`mocks.<name>.hosts`), then run the app under `mockingbird run`:

```bash
mockingbird run -c mockingbird.yaml -- npm test
```

## `mockingbird run`

```text
mockingbird run [-c <config>] [--offline] [--record <dir> | --replay <dir>] [--wait <duration>] [--keep] [--state-dir <dir>] -- <command> [args…]
```

1. Starts the server with its proxy (`server start --proxy`, watching the config) unless the config's own server already answers on its admin port. A running server without its proxy gets one in front of it for the duration of the command. The command only ever gets this config's own proxy (see [Whose proxy](#whose-proxy)): when another listener, such as another project's Mockingbird proxy, holds the proxy port, `run` names it and leaves it alone. With `proxy.port` unset it runs its proxy on the first free port above the default instead; with `proxy.port` set it exits 1 without running the command. `--offline`, `--record`, `--replay` and `-c` shape only a server `run` starts: when it reuses a running one, `run` prints a warning naming the flags that do not apply to it (stop that server with `mockingbird server stop` to use them).
2. Waits until `/readyz` is ready (`--wait`, default `60s`); if it is not, prints the end of the server log and exits 1.
3. Runs the command with the [proxy variables](#the-variables) for every runtime, stdin/stdout/stderr attached. SIGINT and SIGTERM sent to `run` reach the command.
4. Stops what it started (unless `--keep`) and exits with the command's exit code: `128 + n` when signal `n` ended it, `127` when it cannot be started, `1` when Mockingbird itself failed before the command ran.

| Flag | Default | |
| --- | --- | --- |
| `-c`, `--config` | `./mockingbird.yaml` | The config to serve; its `server.*` ports and `proxy.*` settings apply. |
| `--offline` | off | Faker only, no LLM (for a server `run` starts). |
| `--record` | none | Record every LLM answer into this snapshot directory (see [CI](ci.md)). |
| `--replay` | none | Answer from this snapshot directory without calling an LLM; the faker answers a miss. |
| `--wait` | `60s` | How long to wait for readiness. |
| `--keep` | off | Leave a server `run` started running (stop it with `mockingbird server stop`). |
| `--state-dir` | `.mockingbird` (`MB_STATE_DIR`) | Where the pid and `server.log` of the server `run` starts are kept. |

The server inherits `run`'s environment (LLM keys, webhook secrets), but not the proxy variables, so its own LLM calls go straight to the provider. `storage.url: sqlite://./…` is relative to the config file, like `contract:`, wherever `run` is started.

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

`mockingbird proxy env --json` prints the same variables as JSON, with the CA, bundle, truststore, Node.js preload and Python preload paths and the CA's SPKI hash. `-c <config>` and `--port` pick up a non-default `server.host`, `proxy.port` or `proxy.ca_dir`.

To put the proxy in front of a server that runs without `proxy.enabled` (for example one started elsewhere), run it on its own:

```bash
mockingbird proxy --data-url http://127.0.0.1:8080 --admin-url http://127.0.0.1:9090
```

It reads the hosts the server's mocks claim from `GET /proxy/hosts` on the admin port (refreshed every 2 seconds) and forwards intercepted requests to the data plane with their `Host` header, so they dispatch the same way.

### Whose proxy

Several projects on one machine each run their own Mockingbird server, and another project's proxy may hold this config's proxy port (8888 by default). That proxy has none of this project's mocks, so it would pass the app's calls on to the real APIs. `run`, `proxy env`, `proxy browser` and the MCP `proxy_env` tool therefore hand out only the proxy of the config's own server:

- A server's `/readyz` reports its `proxy` address, or `null` when it runs no proxy. Every proxy answers `GET /mockingbird-proxy.json`, sent to the proxy itself, with the `config` and `dir` of the project it serves (to callers on this machine). This works for a server's own proxy, a standalone `mockingbird proxy` and the proxy `run` puts in front of a server.
- With a config (`-c`, else `./mockingbird.yaml`), these commands look up the proxy address in the config's own server's `/readyz`, falling back to `proxy.port`. They then check that the proxy there answers with this config.
- When another listener holds that address, they fail and name it (for another Mockingbird proxy, the config and folder it serves). Leave it running. Give this project's proxy a free port with `proxy.port`, or leave `proxy.port` unset: `server start -c mockingbird.yaml --proxy` then picks a free port and writes it into the config.
- When the config's server runs and has no proxy, `proxy env` fails with that message instead of printing an address. `run` puts a proxy in front of that server instead.
- When nothing listens there yet, `proxy env` prints the variables and warns on stderr that no proxy listens yet.

Without any config there is no project to check against, and `proxy env` prints the defaults as before.

## How it works

- **Mapped hosts are intercepted.** A `CONNECT` to a host a mounted mock lists in `hosts` (or to a `proxy.images` host) is answered by the proxy itself. It terminates TLS with a certificate for that host, signed by the local CA, and serves the requests from the data plane. Dispatch filters mocks by `Host`, then picks the longest route prefix, so two mocks can share a prefix on different hosts. Plain `http://` requests to a mapped host are served the same way.
- **Unmapped hosts are passed through or blocked.** With `proxy.unmapped: pass` (the default) other hosts are tunnelled to the real server untouched; with `block` the proxy answers 502 `application/problem+json` naming the host (over HTTPS inside a tunnel it intercepts with the local CA, as below). Each unmapped host is logged once and counted in `mb_proxy_unmapped_requests_total` by `host` and `mode`; past 500 distinct hosts, new ones are no longer logged and are counted as `host="other"`.
- **Other hosts of a mocked vendor are blocked.** An unmapped host that shares a registrable domain with a host a mock lists (by the Public Suffix List: `verify.twilio.com` and `twilio.com` when `api.twilio.com` is mocked, but not `bar.co.uk` next to `foo.co.uk`) is blocked even under `proxy.unmapped: pass`, so an SDK's call to another of the vendor's hosts never reaches the real API with test credentials. The proxy answers 502 `application/problem+json` with `type` `urn:mockingbird:proxy:vendor-subdomain`, the `host`, the `mapped_hosts` of its domain, and a `detail` saying how to fix it: add the host to a mock's `hosts` (a mock for that product), or set `proxy.vendor_subdomains: pass` (`mockingbird proxy --vendor-subdomains pass`) to send it to the real host. Such requests are counted with `mode="block"` and a `sibling_of` label, and `mockingbird report` lists the host under `unmapped_hosts` with `mode: block` and `sibling_of`, so `mockingbird verify --no-unmapped` fails on it. `mockingbird report` and the MCP `server_status` answer also add a `next_steps` entry per such host that no mock lists yet, naming the mock of its domain to add it to, the catalog examples that list it, and `proxy.vendor_subdomains: pass` as the last resort. IP literals and single-label hosts are never treated as a vendor's other hosts. Over HTTPS (a `CONNECT` to port 443 or 80) the proxy accepts the `CONNECT` and terminates TLS with a certificate for the host signed by the local CA, as for a mapped host, then answers every request in the tunnel with that 502, so a client that trusts the CA (as the variables below make it) reads the explanation instead of only "tunnel connection failed"; a client that does not trust the CA gets a TLS error instead. A host blocked by `proxy.unmapped: block` is answered the same way over HTTPS. Over HTTPS a host is counted once per `CONNECT`, not per request in the tunnel. A `CONNECT` to another port (a database, SMTP) is still refused with the 502 itself, since its server would speak first.
- **The CA stays local.** It is created on first use in `~/.mockingbird/ca` (`proxy.ca_dir`, `MB_CA_DIR`): `ca.pem`, and `ca.key` readable by its owner only. It is never installed in a system trust store and no OS settings are changed. Apps trust it through the variables below. `mockingbird ca path` prints the certificate path.
- **Placeholder images.** Requests matching `proxy.images` get a solid-colour PNG chosen from the path, sized from a `w123`-style path segment or an ipx-style `s_400x600`/`w_400` modifier (else 300×450). No LLM call is made. An entry is a host (`image.tmdb.org`) or a host and path prefix (`movies-proxy.vercel.app/ipx`) for an image endpoint on a host a mock serves.
- **Tunnels carry HTTPS or plain HTTP.** A `CONNECT` to a mapped host, or to a blocked one on port 443 or 80, is answered with TLS when the client starts a handshake, and as plain HTTP otherwise (the Node.js preload tunnels `http://` requests too).
- **A PAC file** is served at `http://127.0.0.1:8888/proxy.pac`: loopback goes direct, every other host through the proxy.
- **A mapped host never leaks to the real one.** A host a mock lists stays intercepted while that mock is not mounted (its contract failed to build, it has none active, or another mock holds its route): requests get 503 problem+json naming the mock, never the real API.
- **Loopback by default.** The proxy binds `proxy.host` (`127.0.0.1`). On any other address `proxy.unmapped` must be `block`, so the port is not an open relay.
- **Start `serve` from a clean shell.** The server's own outbound clients (LLM providers, OIDC, Vault) honour `HTTPS_PROXY` like any app. Started from a shell that ran `mockingbird proxy env`, they reach their providers through the proxy: that works with `pass`, and fails with `block`. Use `outbound.proxy`/`outbound.no_proxy` (which every one of these clients, Vault included, follows instead of the variables) or a shell without the variables.
- A mock with `hosts` answers only under those hosts: `curl localhost:8080/…` no longer reaches it, but `curl -H 'Host: api.github.com' localhost:8080/…` does.
- **Apps that refuse private addresses** (an SSRF guard that checks the socket it connects to, like healthchecks' `INTEGRATIONS_ALLOW_PRIVATE_IPS`) see the proxy at `127.0.0.1` and refuse it. Allow private addresses in the app's test settings.

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
| `PYTHONPATH` | `<ca_dir>/python`, then any `PYTHONPATH` you had | Python: the `sitecustomize` preload (below) |
| `RUBYLIB`, `RUBYOPT` | `<ca_dir>/ruby` then any `RUBYLIB` you had; any `RUBYOPT` you had, then `-rmockingbird_proxy` | Ruby: the `Net::HTTP` preload (below) |
| `GODEBUG` | `x509sslcertoverrideplatform=1`, then any `GODEBUG` you had | Go ≥ 1.27 on macOS and Windows: verify with `SSL_CERT_FILE` |

The files live next to the CA (`~/.mockingbird/ca` by default) and are rewritten each time the variables are printed. Running `eval "$(mockingbird proxy env)"` twice does not add the preloads, the Java options or the Go setting twice.

## Per runtime

- **Node.js:** nothing to install. The preload in `NODE_OPTIONS` sends every `http`/`https` agent through the proxy, including agents an SDK creates itself (stripe-node, `got`, the AWS SDK), and replaces undici's global dispatcher, which `fetch` and the `undici` package share. It honours `NO_PROXY`, trusts the local CA also when a client passes its own `ca`, and does nothing when no proxy variable is set. `axios` reads the proxy variables itself and works too. On Node ≥ 24.5 (or 22.21) `NODE_USE_ENV_PROXY=1` covers `fetch` natively and the preload covers the agents.
- **Python:** `requests` and `httpx` read `HTTPS_PROXY` and `REQUESTS_CA_BUNDLE`/`SSL_CERT_FILE`; `urllib.request` reads `HTTPS_PROXY` and `SSL_CERT_FILE`; `pycurl` (libcurl) reads `https_proxy`. The preload that `PYTHONPATH` names covers the rest: Python imports it at startup as `sitecustomize`, and it points `certifi.where()` at the bundle (for clients that pin certifi's roots) and sends a plain `urllib3.PoolManager` through `HTTPS_PROXY` for hosts outside `NO_PROXY`, as OpenAPI-generated SDKs such as `plaid-python` build one. It leaves `ProxyManager` and other subclasses alone, then runs the next `sitecustomize` on the path if the environment has one. `aiohttp` ignores the proxy variables unless the session is created with `trust_env=True`.
- **Go `net/http`:** `HTTPS_PROXY` through `http.ProxyFromEnvironment`, which the default transport uses; a custom `http.Transport` built without `Proxy: http.ProxyFromEnvironment` (`Proxy: nil`) goes direct, and Go has no preload hook, so point such a client at the mock's URL instead. On Linux `SSL_CERT_FILE`. On macOS (and Windows), binaries built with Go 1.27 or later verify with `SSL_CERT_FILE` when `GODEBUG=x509sslcertoverrideplatform=1`, which `proxy env` sets (it is already the default for modules that declare `go 1.27`). Binaries built with an older Go verify with the system keychain and ignore `SSL_CERT_FILE`, so HTTPS to mocked hosts fails there unless the CA is trusted in the keychain (Mockingbird never does that for you); rebuild with Go 1.27, or run them in a Linux container or CI.
- **Ruby `Net::HTTP`** (and gems built on it: Faraday's default adapter, HTTParty, rest-client, stripe-ruby): nothing to install. `Net::HTTP` reads `http_proxy` and, with Ruby linked against OpenSSL, `SSL_CERT_FILE`. The preload `RUBYOPT` loads covers the rest: a connection created with an explicit nil proxy (`Net::HTTP.new(host, port, nil)`, as stripe-ruby does) uses the proxy from the environment, subject to `NO_PROXY`, and verification trusts the local CA in addition to the client's own `ca_file`, `ca_path` or `cert_store` (also on macOS's system Ruby, which ignores `SSL_CERT_FILE`). A connection given a proxy of its own is left alone, verification is never turned off, and nothing changes when no proxy variable is set. Clients not built on `Net::HTTP` (`httpclient`, `excon`, `typhoeus`) read the proxy variables themselves, or not at all.
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
