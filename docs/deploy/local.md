# Running locally

One process, no external services: SQLite or in-memory storage and in-process state. This is role `all`, the default.

## Get the binary

From a release: download `mockingbird-<version>-<target>.tar.gz` (`.zip` on Windows) from the GitHub release for one of `x86_64-unknown-linux-musl`, `aarch64-unknown-linux-musl`, `x86_64-apple-darwin`, `aarch64-apple-darwin` or `x86_64-pc-windows-msvc`, and check it against its `.sha256` file. Each archive also carries SLSA build provenance; see [verifying releases](kubernetes.md#verifying-images-and-binaries).

```sh
sha256sum -c mockingbird-0.1.0-x86_64-unknown-linux-musl.tar.gz.sha256
tar -xzf mockingbird-0.1.0-x86_64-unknown-linux-musl.tar.gz
./mockingbird-0.1.0-x86_64-unknown-linux-musl/mockingbird --version
```

From a checkout (Rust 1.97 or newer):

```sh
cargo install --path crates/mockingbird
```

The default build includes the Kafka, AMQP and MQTT clients; `--no-default-features` builds without them.

## Serve a contract

```sh
MB_LLM_API_KEY=sk-... mockingbird serve spec.yaml
```

`spec.yaml` can be OpenAPI 3.x, Swagger 2.0, GraphQL SDL, protobuf, WSDL 1.1 or AsyncAPI. One contract is mounted at `/`; several are each mounted under `/<file stem>`. The node listens on loopback only:

| Port | What |
| --- | --- |
| `127.0.0.1:8080` | Data plane: REST, SSE, GraphQL, SOAP and WebSocket mocks |
| `127.0.0.1:50051` | gRPC mocks (h2c) |
| `127.0.0.1:9090` | Admin API, console, `/healthz`, `/readyz`, `/metrics` |

The LLM is any OpenAI-compatible endpoint: `MB_LLM_BASE_URL` (default `https://api.deepseek.com`) and `MB_LLM_MODEL` (default `deepseek-chat`). A local Ollama works with `MB_LLM_BASE_URL=http://127.0.0.1:11434/v1` and any dummy key.

## Offline mode

```sh
mockingbird serve --offline spec.yaml
```

`--offline` answers every call with the schema faker: no LLM key, no network, fully deterministic shapes. Responses carry `X-Mockingbird-Source: fallback`. Use it in CI and for contract checks.

## With a config file

```sh
mockingbird serve -c mockingbird.yaml --watch
```

The file declares mocks, scenarios, LLM profiles and every setting; `--watch` reconciles again whenever the file or a contract changes. Start from [`docs/examples/mockingbird.yaml`](../examples/mockingbird.yaml); the [configuration reference](../configuration.md) lists every key. To keep mocks and sessions across restarts, use a file database:

```yaml
storage:
  url: sqlite:///var/lib/mockingbird/mockingbird.db
```

## TLS

Mockingbird can terminate TLS itself on every port (HTTP/1.1 and h2, gRPC over h2):

```yaml
server:
  tls:
    cert_file: /etc/mockingbird/tls/tls.crt   # PEM chain
    key_file: /etc/mockingbird/tls/tls.key    # PEM key
auth:
  external_url: https://mock.example.test:9090
```

- Both files are required, and they must be readable PEM at startup.
- `auth.external_url` must be an `https://` URL whenever `server.tls` is set, even in `auth.mode: none`. The default `http://127.0.0.1:9090` fails startup.
- The files are checked every 60 s, and a changed pair is loaded without a restart, so a renewed certificate is picked up in place. A pair that fails to load is logged and the previous certificate stays in use.

For a self-signed test certificate:

```sh
openssl req -x509 -newkey rsa:2048 -nodes -days 30 -subj '/CN=localhost' \
  -addext 'subjectAltName=DNS:localhost,IP:127.0.0.1' \
  -keyout key.pem -out cert.pem
```

Outbound TLS (to LLM providers, the audit webhook and the OIDC issuer) trusts the default roots plus `outbound.ca_file`; a corporate proxy goes in `outbound.proxy`.

## Binding other interfaces

With `auth.mode: none` (the default), a non-loopback `--host` is refused unless you pass `--insecure-no-auth`, which makes everyone who reaches the admin port a sysadmin. Only do that on a trusted network; otherwise configure OIDC (see the [Compose stack](compose.md)).
