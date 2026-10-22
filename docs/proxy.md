# Reaching mocks by their real host names: `mockingbird proxy`

An app that hardcodes `https://api.github.com` or `https://api.stripe.com` can talk to Mockingbird without source edits. Give the mocks the hosts they stand in for (`mocks.<name>.hosts`), run the proxy, and start the app with the proxy variables exported.

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

Then, in the app's shell:

```bash
eval "$(mockingbird proxy env)"
```

`mockingbird proxy env --json` prints the same variables as JSON, with the CA and bundle paths. `-c <config>` and `--port` pick up a non-default `server.host`, `proxy.port` or `proxy.ca_dir`.

To put the proxy in front of a server that runs without `proxy.enabled` (for example one started elsewhere), run it on its own:

```bash
mockingbird proxy --data-url http://127.0.0.1:8080 --admin-url http://127.0.0.1:9090
```

It reads the hosts the server's mocks claim from `GET /proxy/hosts` on the admin port (refreshed every 2 seconds) and forwards intercepted requests to the data plane with their `Host` header, so they dispatch the same way.

## How it works

- **Mapped hosts are intercepted.** A `CONNECT` to a host a mounted mock lists in `hosts` (or to a `proxy.images` host) is answered by the proxy itself. It terminates TLS with a certificate for that host, signed by the local CA, and serves the requests from the data plane. Dispatch filters mocks by `Host`, then picks the longest route prefix, so two mocks can share a prefix on different hosts. Plain `http://` requests to a mapped host are served the same way.
- **Unmapped hosts are passed through or blocked.** With `proxy.unmapped: pass` (the default) other hosts are tunnelled to the real server untouched; with `block` the proxy answers 502 `application/problem+json` naming the host. Each unmapped host is logged once.
- **The CA stays local.** It is created on first use in `~/.mockingbird/ca` (`proxy.ca_dir`, `MB_CA_DIR`): `ca.pem`, and `ca.key` readable by its owner only. It is never installed in a system trust store and no OS settings are changed. Apps trust it through the variables below. `mockingbird ca path` prints the certificate path.
- **Placeholder images.** Requests to `proxy.images` hosts get a solid-colour PNG chosen from the path, sized from a `w123`-style path segment (else 300×450). No LLM call is made.
- **A mapped host never leaks to the real one.** A host a mock lists stays intercepted while that mock is not mounted (its contract failed to build, it has none active, or another mock holds its route): requests get 503 problem+json naming the mock, never the real API.
- **Loopback by default.** The proxy binds `proxy.host` (`127.0.0.1`). On any other address `proxy.unmapped` must be `block`, so the port is not an open relay.
- **Start `serve` from a clean shell.** The server's own outbound clients (LLM providers, OIDC, Vault) honour `HTTPS_PROXY` like any app. Started from a shell that ran `mockingbird proxy env`, they reach their providers through the proxy: that works with `pass`, and fails with `block`. Use `outbound.proxy`/`outbound.no_proxy` or a shell without the variables.
- A mock with `hosts` answers only under those hosts: `curl localhost:8080/…` no longer reaches it, but `curl -H 'Host: api.github.com' localhost:8080/…` does.

## The variables

| Variable | Value |
| --- | --- |
| `HTTPS_PROXY`, `HTTP_PROXY`, `https_proxy`, `http_proxy` | `http://127.0.0.1:8888` |
| `NO_PROXY`, `no_proxy` | `localhost,127.0.0.1` |
| `NODE_EXTRA_CA_CERTS` | The local CA (`ca.pem`) |
| `SSL_CERT_FILE`, `REQUESTS_CA_BUNDLE` | `bundle.pem`: the system's root certificates plus the local CA, so passed-through hosts still verify |
| `NODE_USE_ENV_PROXY` | `1` |

## Per runtime

- **Node.js `fetch` and `http`/`https`:** `NODE_USE_ENV_PROXY=1` needs Node **≥ 24.5.0, or ≥ 22.21.0** on the 22 line. The variable appeared in v24.0.0 for `fetch` only; v24.5.0 and v22.21.0 extended it to the `http`/`https` global agent and added the `--use-env-proxy` flag, which does the same thing (Node.js [CLI](https://nodejs.org/api/cli.html#node_use_env_proxy1) and [HTTP](https://nodejs.org/api/http.html#built-in-proxy-support) docs). On older Node, use `global-agent` (below).
- **Node.js `axios`, `got` and other clients with their own agent:** `global-agent` (`GLOBAL_AGENT_HTTP_PROXY=$HTTPS_PROXY node -r global-agent/bootstrap app.js`). `NODE_EXTRA_CA_CERTS` covers the CA.
- **Python `requests`, `httpx`, `urllib`:** `HTTPS_PROXY`, and `REQUESTS_CA_BUNDLE`/`SSL_CERT_FILE` for the CA.
- **curl:** lowercase `http_proxy`/`https_proxy`, and `SSL_CERT_FILE` with OpenSSL builds (otherwise `--cacert "$(mockingbird ca path)"`).
- **Go `net/http`:** `HTTPS_PROXY` and `SSL_CERT_FILE`.
