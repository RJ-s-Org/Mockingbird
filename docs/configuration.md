# Configuration reference

Mockingbird reads one YAML file, `mockingbird.yaml`, passed with `mockingbird serve -c <file>`. Every key is optional, and an empty file is an all-defaults configuration. Unknown keys are an error, so a typo fails startup instead of being ignored.

**Precedence:** the file < `MB_*` environment variables < command-line flags. An empty environment variable counts as unset.

**Reloading:** with `--watch`, a change to the file or to a contract it names reconciles mocks, scenarios, LLM profiles and workspaces again. Keys marked *read at startup* are read once, when the process starts; change them with a restart (or a rolling restart, see the [runbook](operations/runbook.md#rolling-upgrades)).

**Secrets** never appear in the file. A key that takes a secret takes a *reference*: `env:NAME` (an environment variable), `file:/path` (a file's contents, whitespace trimmed) or `vault:<mount>/<path>#<field>` (a HashiCorp Vault KV v2 field, enabled when `VAULT_ADDR` is set; see [Kubernetes secrets](deploy/kubernetes.md#secrets)).

**Durations** are written as `250ms`, `30s`, `5m`, `12h` or `1d`.

[`examples/mockingbird.full.yaml`](examples/mockingbird.full.yaml) sets every key below; [`examples/mockingbird.yaml`](examples/mockingbird.yaml) is a commented starting point. A test (`crates/mockingbird/tests/docs_reference.rs`) checks that the full example loads and that this page names every key the code accepts.

In key paths, `<name>` stands for a name you choose (a mock, broker, profile, scenario or channel id), and `[]` for an item of a list. The top-level sections are `server`, `storage`, `bus`, `outbound`, `limits`, `pii`, `import`, `auth`, `audit`, `llm`, `observability`, `protocols`, `brokers` and `mocks`.

## `server`

Listeners and the node's role. Read at startup.

| Key | Type | Default | Range / rule | Env / flag | Meaning |
| --- | --- | --- | --- | --- | --- |
| `server` | section | | | | |
| `server.host` | IP address | `127.0.0.1` | A non-loopback address needs `auth.mode: oidc` or `server.insecure_no_auth` | `MB_HOST`, `--host` | Bind address of every port. |
| `server.port` | port | `8080` | | `MB_PORT`, `--port` | Data plane: REST, SSE, GraphQL, SOAP and WebSocket mocks. |
| `server.admin_port` | port | `9090` | Must be free when set explicitly | `MB_ADMIN_PORT`, `--admin-port` | Admin API, console and ops endpoints. When it is not set explicitly and 9090 is taken, the node starts without it and logs a warning. |
| `server.grpc_port` | port | `50051` | Must be free when set explicitly | `MB_GRPC_PORT`, `--grpc-port` | gRPC (h2c, or h2 over TLS with `server.tls`), shared by every gRPC mock. Skipped with a warning like `admin_port`. |
| `server.insecure_no_auth` | bool | `false` | | `--insecure-no-auth` | Allow a non-loopback `server.host` with `auth.mode: none`. Everyone who reaches the admin port is then a sysadmin. |
| `server.api_secret_refs` | list of globs | `["env:MB_LLM_*", "env:*_API_KEY"]` | `*` matches any run of characters | | Secret references an LLM profile may use when it is created or updated through the Admin API; anything else is a 422. Profiles in this file are not restricted. |
| `server.role` | `all`, `control`, `data` | `all` | `control` and `data` need a `postgres://` `storage.url` and a `postgres://` or `redis(s)://` effective state URL and a shared bus; SQLite, `memory` and `bus.url: local` are refused | `MB_ROLE`, `--role` | Which planes the node serves; see [roles](architecture.md#roles). |
| `server.trust_forwarded_headers` | bool | `false` | | | Trust `X-Forwarded-Proto`/`X-Forwarded-Host` (and RFC 7239 `Forwarded`) when building SOAP `?wsdl` addresses. Turn it on only behind a reverse proxy that sets them. |
| `server.tls` | section | unset | Both files or neither | | Terminate TLS in Mockingbird on every port. Absent serves plain HTTP. |
| `server.tls.cert_file` | path | | Readable PEM certificate chain at startup | | Server certificate chain. Checked every 60 s and reloaded when it changes. |
| `server.tls.key_file` | path | | Readable PEM private key at startup | | Private key of the certificate. |

With `server.tls` set, `auth.external_url` must be an `https://` URL in every auth mode: the default `http://127.0.0.1:9090` fails startup, so set `auth.external_url` too.

## `storage`

| Key | Type | Default | Range / rule | Env / flag | Meaning |
| --- | --- | --- | --- | --- | --- |
| `storage` | section | | | | |
| `storage.url` | URL | `sqlite::memory:` | `sqlite::memory:`, `sqlite://<path>` (created if missing) or `postgres://…` | `MB_STORAGE_URL`, `--storage-url` | Control-plane store: mocks, contracts, scenarios, profiles, API keys, audit and request logs, token usage. Migrations run at startup under an advisory lock. |
| `storage.state_url` | URL | unset (same as `storage.url`) | `memory`, `sqlite…`, `postgres://…`, `redis://` or `rediss://` | `MB_STATE_URL` | Session state store. `memory` keeps state in the process. Redis must run with `maxmemory-policy noeviction`; Redis Cluster is not supported (use a single primary; a managed service with automatic failover works). |
| `storage.session_ttl` | duration | `24h` | | | A session's state expires this long after its last write. |

## `bus`

| Key | Type | Default | Range / rule | Env / flag | Meaning |
| --- | --- | --- | --- | --- | --- |
| `bus` | section | | | | The cross-node change bus that tells other nodes to reload. |
| `bus.url` | URL | unset | `redis://`, `rediss://`, `postgres://` or `local` | `MB_BUS_URL` | Unset: Postgres `LISTEN/NOTIFY` on `storage.url` when it is Postgres, else in-process. `local` is in-process (one node) and is refused for `server.role` `control`/`data`. |

## `outbound`

Settings for the HTTP requests Mockingbird makes to LLM providers, the audit webhook and the OIDC issuer. Read at startup.

| Key | Type | Default | Range / rule | Env / flag | Meaning |
| --- | --- | --- | --- | --- | --- |
| `outbound` | section | | | | |
| `outbound.ca_file` | path | unset | PEM bundle | | CA certificates trusted in addition to the default roots. |
| `outbound.proxy` | URL | unset | `http://` or `https://` URL with a host | | Proxy for outbound requests. |
| `outbound.no_proxy` | list of strings | `[]` | Host names, or `.domain` suffixes | | Hosts reached without the proxy. |

## `proxy`

An HTTP proxy that lets unmodified apps reach mocks by their real host names: it intercepts HTTPS to every host a mock lists in `mocks.<name>.hosts` (and to `proxy.images`) with a certificate from a local CA, and passes other hosts through or blocks them. Started by `serve` on nodes serving the data plane; `mockingbird proxy` runs it on its own. Read at startup.

| Key | Type | Default | Range / rule | Env / flag | Meaning |
| --- | --- | --- | --- | --- | --- |
| `proxy` | section | | | | |
| `proxy.enabled` | bool | `false` | | `--proxy` | Whether `serve` starts the proxy. |
| `proxy.port` | port | `8888` | | `--proxy-port` | The proxy's port, on `server.host`. |
| `proxy.unmapped` | `pass`, `block` | `pass` | | | What happens to a host no mock claims: `pass` tunnels it to the real host; `block` answers 502 problem+json naming the host. Each such host is logged once. |
| `proxy.images` | list of strings | `[]` | Lowercase host names, no port | | Hosts answered with a generated placeholder PNG (a solid colour from the path; size from a `w123` path segment, else 300×450). |
| `proxy.ca_dir` | path | `~/.mockingbird/ca` | Relative to the config file | `MB_CA_DIR` | Where the local CA (`ca.pem`, `ca.key`) is kept; created on first use, never installed system-wide. |

## `limits`

Global admission limits. Per-mock limits are under [`mocks.<name>.limits`](#mocksnamelimits). Read at startup.

| Key | Type | Default | Range / rule | Env / flag | Meaning |
| --- | --- | --- | --- | --- | --- |
| `limits` | section | | | | |
| `limits.max_concurrent_generations` | integer | `64` | 1..=4096 | | LLM generations running at once across every mock on the node. A call that gets no slot within its LLM timeout is served by the faker (`mb_generation_queue_timeouts_total`). |
| `limits.max_request_bytes` | integer (bytes) | `1048576` | 1024..=67108864 | | Larger request bodies are answered 413 and recorded. |
| `limits.max_response_bytes` | integer (bytes) | `1048576` | 1024..=16777216 | | Larger generated responses are replaced by faker output. |

## `pii`

The output PII scrubber replaces personal-data look-alikes in generated responses with synthetic values of the same shape. Scrubbed output still validates against the contract, and scrubbing twice changes nothing. Read at startup.

| Key | Type | Default | Range / rule | Env / flag | Meaning |
| --- | --- | --- | --- | --- | --- |
| `pii` | section | | | | |
| `pii.enabled` | bool | `false` | | | Turn the scrubber on. |
| `pii.patterns` | list | all six builtins | Subset of `email`, `phone`, `credit_card`, `us_ssn`, `iban`, `ipv4` | | Builtin patterns to replace. Card numbers must pass the Luhn check. |
| `pii.custom` | list | `[]` | At most 32 entries | | Extra patterns. |
| `pii.custom[].name` | string | | `[a-z0-9_]{1,32}` | | Pattern name, the `pattern` label of `mb_pii_replacements_total`. |
| `pii.custom[].regex` | regex | | Compiled with a 1 MiB size limit | | What to replace. |
| `pii.custom[].replacement` | template | | `#` becomes a random digit, `?` a random lowercase letter, anything else is literal | | What to write instead. |

## `import`

Smart imports (documentation, HAR and Postman). Read at startup.

| Key | Type | Default | Range / rule | Env / flag | Meaning |
| --- | --- | --- | --- | --- | --- |
| `import` | section | | | | |
| `import.workers` | integer | `2` | 1..=16 | | Import worker tasks (on `all` and `control` nodes). |
| `import.max_tokens` | integer | `200000` | At least 1000 | | LLM token budget of one import job. |
| `import.llm_profile` | string | unset | | | Profile used for extraction instead of the mock's own (looked up in the mock's workspace, then `default`). |
| `import.url_allowlist` | list | `[]` | Host names, `*.suffix` host suffixes, addresses and CIDR ranges | | Private destinations URL imports may reach; every other private address is refused. |
| `import.max_active_jobs_per_mock` | integer | `10` | At least 1 | | Queued plus running jobs per mock; another import is answered 429. |

## `auth`

How the Admin API, console and CLI authenticate. Read at startup.

| Key | Type | Default | Range / rule | Env / flag | Meaning |
| --- | --- | --- | --- | --- | --- |
| `auth` | section | | | | |
| `auth.mode` | `none`, `oidc` | `none` | `oidc` needs `auth.oidc` | | `none`: every admin request is the local sysadmin. `oidc`: console login, bearer JWTs and API keys. |
| `auth.external_url` | URL | `http://127.0.0.1:9090` | `http(s)://` URL with a host; `https://` when `server.tls` is set | | The URL browsers reach the admin port at. The OIDC redirect is `<external_url>/auth/callback`, `https` makes cookies `Secure`, and in `oidc` mode the admin port answers only to this host (except `/healthz`, `/readyz` and `/metrics`). |
| `auth.session_ttl` | duration | `12h` | At most `30d` | | Absolute lifetime of a console login. |
| `auth.sysadmin_subjects` | list | `[]` | | | OIDC `sub` values that are always sysadmin. |
| `auth.public_metrics` | bool | `true` | | | `false`: `/metrics` needs the same credentials as the Admin API. |
| `auth.oidc` | section | unset | Required in `oidc` mode | | The OIDC relying party. |
| `auth.oidc.issuer` | URL | | Exactly the discovery document's `issuer`, trailing slash included | | OIDC issuer. |
| `auth.oidc.client_id` | string | | | | Client id. |
| `auth.oidc.client_secret` | secret reference | unset | | | Client secret; omit it for a public client. |
| `auth.oidc.scopes` | list | `[openid, profile, email, groups]` | | | Scopes requested at login. |
| `auth.oidc.roles_claim` | string | `groups` | | | Claim holding `<group_prefix>:<workspace>:<role>` and `<group_prefix>:sysadmin` strings. |
| `auth.oidc.group_prefix` | string | `mb` | | | Prefix of the role strings. |
| `auth.oidc.audience` | string | `client_id` | | | The `aud` bearer JWTs must carry. |

## `audit`

| Key | Type | Default | Range / rule | Env / flag | Meaning |
| --- | --- | --- | --- | --- | --- |
| `audit` | section | | | | |
| `audit.retention_days` | integer | `365` | 1..=3650 | | Older audit events are pruned daily (by the leader). |
| `audit.webhook_url` | URL | unset | | | Every audit event is POSTed here. |
| `audit.webhook_secret` | secret reference | unset | | | Key of the `X-Mockingbird-Signature` HMAC. |

## `llm`

| Key | Type | Default | Range / rule | Env / flag | Meaning |
| --- | --- | --- | --- | --- | --- |
| `llm` | section | | | | |
| `llm.default_profile` | string | unset | A profile name | | Profile of mocks that name none. Set to `default` when the `MB_LLM_*` variables create that profile and none is set. |
| `llm.profiles` | map | `{}` | | | LLM profiles by name. The `MB_LLM_*` variables configure the profile named `default`. |
| `llm.profiles.<name>` | section | | | | One profile. |
| `llm.profiles.<name>.workspace` | string | `default` | | | Workspace the profile belongs to (created if missing). |
| `llm.profiles.<name>.base_url` | URL | required | | `MB_LLM_BASE_URL` (default `https://api.deepseek.com`) | OpenAI-compatible base URL. |
| `llm.profiles.<name>.model` | string | required | | `MB_LLM_MODEL` (default `deepseek-chat`) | Model name. |
| `llm.profiles.<name>.api_key` | secret reference | unset | | `MB_LLM_API_KEY` (stored as `env:MB_LLM_API_KEY`) | Provider API key. |
| `llm.profiles.<name>.json_schema` | bool | `false` | | `MB_LLM_JSON_SCHEMA` | Request JSON Schema structured output instead of JSON mode. |
| `llm.profiles.<name>.timeout` | duration | `60s` | | `--llm-timeout` (profile `default`) | Time before the call falls back to the faker. |
| `llm.profiles.<name>.temperature` | number | `0.7` | | | Sampling temperature. |
| `llm.profiles.<name>.max_tokens` | integer | `16384` | | | Completion token limit. A reply cut at this limit falls back to the faker, with the reason in `X-Mockingbird-Fallback-Reason`. |
| `llm.profiles.<name>.fallback` | string | unset | A profile name | | Profile tried when this one's provider fails. |
| `llm.profiles.<name>.headers` | map | `{}` | Lowercase header names; not `authorization`, `content-type`, `content-length`, `host` or `transfer-encoding` | | Extra request headers sent to the provider, such as Cloudflare AI Gateway's `cf-aig-authorization` or Azure OpenAI's `api-key`. A value may embed secret references as `${env:NAME}` or `${file:/path}` (`Bearer ${env:CF_AIG_API_KEY}`); a header whose name contains `auth`, `key`, `token`, `secret`, `password` or `cookie` must use one. Through the Admin API each reference must match `server.api_secret_refs`, and only a sysadmin may set them. |
| `llm.profiles.<name>.headers.<name>` | string | | | | One header value. |
| `llm.profiles.<name>.extra_body` | map | `{}` | Not `model`, `messages`, `response_format`, `temperature`, `max_tokens` or `stream`; at most 8 KiB as JSON | | JSON merged into every request body, such as OpenRouter's `provider` routing or `reasoning` settings (`{reasoning: {enabled: false}}`). |

`--offline` serves every mock with the faker and makes no LLM calls.

## `observability`

| Key | Type | Default | Range / rule | Env / flag | Meaning |
| --- | --- | --- | --- | --- | --- |
| `observability` | section | | | | |
| `observability.request_log_retention` | integer | `1000` | | | Request-log entries kept per mock. |
| `observability.redact_fields` | list | `[password, token, secret, authorization, apiKey, api_key]` | | | A recorded JSON, form or query key is redacted when, lowercased and without `_`, `-` and `.`, it contains one of these (normalized the same way). |

`OTEL_EXPORTER_OTLP_ENDPOINT` enables OTLP trace export, and `RUST_LOG` sets the log filter (default `info`).

## `protocols`

Knobs shared by every mock. Read at startup.

| Key | Type | Default | Range / rule | Env / flag | Meaning |
| --- | --- | --- | --- | --- | --- |
| `protocols` | section | | | | |
| `protocols.graphql` | section | | | | GraphQL mocks. |
| `protocols.graphql.path` | path | `/graphql` | Starts with `/` | | Endpoint below each GraphQL mock's route prefix. |
| `protocols.graphql.introspection` | bool | `true` | | | Answer introspection queries. |
| `protocols.graphql.max_depth` | integer | `15` | | | Deepest accepted query. |
| `protocols.graphql.max_complexity` | integer | `1000` | | | Most complex accepted query. |
| `protocols.graphql.max_root_fields` | integer | `20` | 1..=1000 | | Root fields one operation may select. |
| `protocols.graphql.max_subscriptions_per_connection` | integer | `16` | 1..=1000 | | Concurrent subscriptions on one WebSocket connection. |
| `protocols.graphql.max_connections_per_mock` | integer | `1000` | 1..=100000 | | Concurrent WebSocket connections to one mock. |
| `protocols.graphql.connection_init_timeout` | duration | `5s` | `1s` to `10s` | | A WebSocket connection that sends no `connection_init` in time is closed (code 4408) and stops counting against `max_connections_per_mock`. |
| `protocols.stream` | section | | | | Streamed responses: SSE, GraphQL subscriptions and gRPC streams. |
| `protocols.stream.default_items` | integer | `5` | | | Items when the request asks for no count (`X-Mockingbird-Stream-Items`). |
| `protocols.stream.max_items` | integer | `50` | | | Most items per streamed response. |
| `protocols.stream.item_interval` | duration | `0s` | At most `30s` | | Pause before every item except the first. |
| `protocols.sse` | section | | | | Server-sent events. |
| `protocols.sse.keepalive` | duration | `15s` | At least `1s` | | Interval between keepalive comments. |
| `protocols.grpc` | section | | | | gRPC mocks. |
| `protocols.grpc.max_message_bytes` | integer (bytes) | `4194304` | At least 1024 | | Largest message decoded or encoded. |

## `brokers`

Message brokers the AsyncAPI mocks' channels use, by name. Read at startup.

| Key | Type | Default | Range / rule | Env / flag | Meaning |
| --- | --- | --- | --- | --- | --- |
| `brokers` | map | `{}` | | | Brokers by name. |
| `brokers.<name>` | section | | | | One broker connection. |
| `brokers.<name>.kind` | `kafka`, `amqp`, `mqtt`, `memory` | required | The kind must be compiled in (the slim image has only `memory`) | | Broker type. `memory` is in-process, for tests. |
| `brokers.<name>.url` | URL | unset | Required for `amqp` and `mqtt`; no password in it; MQTT only over plain `mqtt://` | | Broker URL. |
| `brokers.<name>.bootstrap_servers` | string | unset | Required for `kafka` | | Comma-separated `host:port` list. |
| `brokers.<name>.tls` | bool | `false` | Not for MQTT | | Connect over TLS. |
| `brokers.<name>.ca_file` | path | unset | PEM; not for MQTT | | CA bundle trusted in addition to the system roots. |
| `brokers.<name>.sasl` | section | unset | | | SASL credentials. |
| `brokers.<name>.sasl.mechanism` | string | | `PLAIN`, `SCRAM-SHA-256`, `SCRAM-SHA-512` | | SASL mechanism. |
| `brokers.<name>.sasl.username` | string | | | | User name. |
| `brokers.<name>.sasl.password` | secret reference | | A reference, never a password | | Password. |
| `brokers.<name>.client_id_prefix` | string | `mockingbird` | | | Prefix of client ids and consumer groups: a mock's Kafka group (and AMQP shared queue) is `<prefix>-<mock>` (`<prefix>-<workspace>.<mock>` outside `default`). With a shared bus, MQTT consumers subscribe to `$share/<prefix>/<topic>`. |

## `mocks`

Mocks declared as code, by name. They are reconciled into the store at startup and on `--watch`, by the leader; they are read-only through the Admin API.

| Key | Type | Default | Range / rule | Env / flag | Meaning |
| --- | --- | --- | --- | --- | --- |
| `mocks` | map | `{}` | | | Mocks by name. |
| `mocks.<name>` | section | | Unique per workspace | | One mock. |
| `mocks.<name>.workspace` | string | `default` | | | Workspace (created if missing). |
| `mocks.<name>.contract` | path | required | Relative to the config file | | OpenAPI 3.x, Swagger 2.0, GraphQL SDL, protobuf, WSDL 1.1 or AsyncAPI 2.x/3.0 contract. |
| `mocks.<name>.route_prefix` | path | `/<name>` | | | Where the mock is mounted on the data plane. |
| `mocks.<name>.hosts` | list of strings | `[]` | Lowercase host names, no port | | Hosts the mock answers for (by `Host`, e.g. through `mockingbird proxy`). With hosts, two mocks may share a route prefix; a mock with hosts is not reached under other hosts. |
| `mocks.<name>.persona` | string | unset | | | Free text added to every prompt (locale, realism, style). |
| `mocks.<name>.llm_profile` | string | `llm.default_profile` | | | LLM profile. |
| `mocks.<name>.header_overrides` | bool | `true` | | | Honour `X-Mockingbird-*` request headers (status, latency, scenario, `X-Mockingbird-Chaos: off`). |
| `mocks.<name>.broker` | string | picked automatically | A `brokers` name | | Broker the mock's channels use. |
| `mocks.<name>.data_plane_auth` | section | | | | How callers of the mock authenticate. |
| `mocks.<name>.data_plane_auth.mode` | `none`, `api_key`, `passthrough_validate` | `none` | | | `api_key`: a `data-plane` API key of the mock's workspace is required. `passthrough_validate`: credentials must be present in the form the contract's security requirements declare, without being verified. |
| `mocks.<name>.data_plane_auth.header` | string | `x-api-key` | Lowercase | | Header carrying the key in `api_key` mode (or `?api_key=`). |

### `mocks.<name>.chaos`

Fault injection. Absent injects nothing. A request with `X-Mockingbird-Chaos: off` bypasses it when header overrides are enabled. An injected error uses the operation's declared error shape, or the protocol's native one (HTTP 503, gRPC `UNAVAILABLE`, SOAP Fault), and is marked `X-Mockingbird-Chaos: error` (gRPC metadata `x-mockingbird-chaos: error`, GraphQL `extensions.chaos = "error"`).

| Key | Type | Default | Range / rule | Env / flag | Meaning |
| --- | --- | --- | --- | --- | --- |
| `mocks.<name>.chaos` | section | unset | | | The mock's chaos. |
| `mocks.<name>.chaos.error_rate` | number | `0` | 0.0..=1.0 | | Share of calls answered with an injected error. `0` never injects. |
| `mocks.<name>.chaos.latency_jitter` | section | unset | | | Uniformly random latency added to every call. |
| `mocks.<name>.chaos.latency_jitter.min` | duration | `0s` | `min` ≤ `max` | | Least added latency. |
| `mocks.<name>.chaos.latency_jitter.max` | duration | `0s` | At most `30s` | | Most added latency. |
| `mocks.<name>.chaos.drop_stream_rate` | number | `0` | 0.0..=1.0 | | Share of streamed responses cut short. |

`MB_CHAOS_SEED` (an integer) seeds the random choices, for repeatable test runs.

### `mocks.<name>.limits`

Admission limits of one mock. Unset fields use the global [`limits`](#limits), or no limit.

| Key | Type | Default | Range / rule | Env / flag | Meaning |
| --- | --- | --- | --- | --- | --- |
| `mocks.<name>.limits` | section | `{}` | | | The mock's limits. |
| `mocks.<name>.limits.rate_limit` | section | unset | | | Token bucket, per mock on each node. Over the limit, calls are answered 429 with `Retry-After` (in the contract's 429 shape when it declares one). |
| `mocks.<name>.limits.rate_limit.requests_per_second` | number | | (0, 100000] | | Refill rate. |
| `mocks.<name>.limits.rate_limit.burst` | integer | | 1..=100000 | | Bucket size. |
| `mocks.<name>.limits.daily_token_budget` | integer | unset | At least 1 | | LLM tokens per UTC day across every replica (flushed to the store every 5 s; the leader prunes usage older than 90 days daily). Once spent, calls are served by the faker and `/readyz` reports it. |
| `mocks.<name>.limits.max_request_bytes` | integer (bytes) | `limits.max_request_bytes` | 1024..=67108864 | | Overrides the global value for this mock. |
| `mocks.<name>.limits.max_response_bytes` | integer (bytes) | `limits.max_response_bytes` | 1024..=16777216 | | Overrides the global value for this mock. |

### `mocks.<name>.scenarios`

Named scenarios, applied with `X-Mockingbird-Scenario: <name>` or from the console.

| Key | Type | Default | Range / rule | Env / flag | Meaning |
| --- | --- | --- | --- | --- | --- |
| `mocks.<name>.scenarios` | map | `{}` | | | Scenarios by name. |
| `mocks.<name>.scenarios.<name>` | section | | | | One scenario. |
| `mocks.<name>.scenarios.<name>.instructions` | string | unset | | | Extra prompt instructions. |
| `mocks.<name>.scenarios.<name>.status` | integer | unset | A status the contract declares, or a 5xx | | Answer with this status's response variant (`set` applies to it too); an undeclared 5xx is answered natively. Any other status must be declared by one of the scenario's `operations` (by any operation when it lists none), or the config is rejected, as the Admin API answers 422; an operation that does not declare it serves its own status. |
| `mocks.<name>.scenarios.<name>.latency` | duration | unset | | | Added latency. |
| `mocks.<name>.scenarios.<name>.set` | JSON object | unset | | | Fields forced into the response body. |
| `mocks.<name>.scenarios.<name>.operations` | list | `[]` (all) | Operation ids | | Operations the scenario applies to. |
| `mocks.<name>.scenarios.<name>.stream_items` | integer | unset | | | Items per streamed response unless `X-Mockingbird-Stream-Items` overrides it. |
| `mocks.<name>.scenarios.<name>.stream_item_interval` | duration | unset | At most `30s` | | Pause between streamed items. |
| `mocks.<name>.scenarios.<name>.chaos` | section | unset | | | Replaces the mock's chaos while the scenario applies. |
| `mocks.<name>.scenarios.<name>.chaos.error_rate` | number | `0` | 0.0..=1.0 | | As `mocks.<name>.chaos.error_rate`. |
| `mocks.<name>.scenarios.<name>.chaos.latency_jitter` | section | unset | | | As `mocks.<name>.chaos.latency_jitter`. |
| `mocks.<name>.scenarios.<name>.chaos.latency_jitter.min` | duration | `0s` | `min` ≤ `max` | | Least added latency. |
| `mocks.<name>.scenarios.<name>.chaos.latency_jitter.max` | duration | `0s` | At most `30s` | | Most added latency. |
| `mocks.<name>.scenarios.<name>.chaos.drop_stream_rate` | number | `0` | 0.0..=1.0 | | As `mocks.<name>.chaos.drop_stream_rate`. |

### `mocks.<name>.channels`

Overrides of an AsyncAPI mock's event channels, by channel id. Channels not listed keep the contract's settings.

| Key | Type | Default | Range / rule | Env / flag | Meaning |
| --- | --- | --- | --- | --- | --- |
| `mocks.<name>.channels` | map | `{}` | | | Channels by id. |
| `mocks.<name>.channels.<name>` | section | | A channel id of the contract | | One channel. |
| `mocks.<name>.channels.<name>.enabled` | bool | `true` | | | `false` stops producing (or consuming) on the channel. |
| `mocks.<name>.channels.<name>.triggers` | list | the contract's triggers (manual when it has none) | | | When the channel produces. Each item is one of the triggers below, or the plain string `manual` (on demand: the Admin API, the console or `mockingbird channels publish`). |
| `mocks.<name>.channels.<name>.triggers[].every` | duration | | At least `100ms` | | On a schedule. |
| `mocks.<name>.channels.<name>.triggers[].cron` | string | | 5-field (or 6-field, with seconds) cron expression, UTC | | On a cron schedule. |
| `mocks.<name>.channels.<name>.triggers[].on_state_change` | section | | | | When the mock's state changes. |
| `mocks.<name>.channels.<name>.triggers[].on_state_change.resource_type` | string | | | | Resource type to watch. |
| `mocks.<name>.channels.<name>.triggers[].on_state_change.kind` | `upserted`, `deleted`, `any` | `any` | | | Which changes fire. |

## Environment variables and flags

| Variable | Flag | Key or meaning |
| --- | --- | --- |
| `MB_HOST` | `--host` | `server.host` |
| `MB_PORT` | `--port` | `server.port` |
| `MB_ADMIN_PORT` | `--admin-port` | `server.admin_port` |
| `MB_GRPC_PORT` | `--grpc-port` | `server.grpc_port` |
| | `--insecure-no-auth` | `server.insecure_no_auth` |
| `MB_ROLE` | `--role` | `server.role` |
| | `--proxy` | `proxy.enabled` |
| | `--proxy-port` | `proxy.port` |
| `MB_CA_DIR` | | `proxy.ca_dir` (also used by `mockingbird ca path` and `mockingbird proxy env`) |
| `MB_STORAGE_URL` | `--storage-url` | `storage.url` |
| `MB_STATE_URL` | | `storage.state_url` |
| `MB_BUS_URL` | | `bus.url` |
| `MB_LLM_BASE_URL`, `MB_LLM_MODEL`, `MB_LLM_API_KEY`, `MB_LLM_JSON_SCHEMA` | `--llm-timeout` | The `default` LLM profile. |
| `MB_STATE_DIR` | `--state-dir` | Where `mockingbird server start` keeps the background server's pid, log and URLs (default `.mockingbird`). Not a config file key. |
| | `--offline` | Faker only, no LLM calls. |
| | `-c`, `--config` | The config file. |
| | `--watch` | Reconcile again when the file or a contract changes (a usage error on `data` nodes). |
| `MB_CHAOS_SEED` | | Seed of the chaos random choices. |
| `VAULT_ADDR`, `VAULT_TOKEN`, `VAULT_TOKEN_FILE`, `VAULT_NAMESPACE`, `VAULT_CACERT` | | Enable and configure `vault:` secret references. |
| `MB_ADMIN_URL`, `MB_ADMIN_TOKEN`, `MB_WORKSPACE` | `--admin-url`, `--token`, `--workspace` | The CLI client's Admin API, API key and workspace. |
| `OTEL_EXPORTER_OTLP_ENDPOINT` | | OTLP trace export. |
| `RUST_LOG` | | Log filter. |
