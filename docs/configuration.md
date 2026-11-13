# Configuration reference

Mockingbird reads one YAML file, `mockingbird.yaml`, passed with `mockingbird serve -c <file>`. Every key is optional, and an empty file is an all-defaults configuration. Unknown keys are an error, so a typo fails startup instead of being ignored.

**Precedence:** the file < `MB_*` environment variables < command-line flags. An empty environment variable counts as unset.

**Reloading:** with `--watch`, a change to the file or to a contract it names reconciles mocks, scenarios, LLM profiles and workspaces again. Keys marked *read at startup* are read once, when the process starts; change them with a restart (or a rolling restart, see the [runbook](operations/runbook.md#rolling-upgrades)). A reload that changes one of these sections (`server`, `storage`, `protocols`, `auth`, `audit`, `outbound`, `bus`, `limits`, `pii`, `import`, `proxy`) logs `config reload: the <section>: section changed; restart the server to apply it` and applies the rest; a `brokers` change rejects the reload.

**Secrets** never appear in the file. A key that takes a secret takes a *reference*: `env:NAME` (an environment variable), `file:/path` (a file's contents, whitespace trimmed) or `vault:<mount>/<path>#<field>` (a HashiCorp Vault KV v2 field, enabled when `VAULT_ADDR` is set; see [Kubernetes secrets](deploy/kubernetes.md#secrets)). AWS, GCP and Azure secret managers have no native reference: inject their secrets as environment variables or files (on Kubernetes, the Secrets Store CSI driver mounts each one as a file for a `file:` reference) or serve them through Vault. A `file:` reference into the key store (`~/.mockingbird/secrets/`, written by `mockingbird keys setup`) is refused when the file is readable by group or others.

**Relative paths** in the file (`mocks.<name>.contract`, `sqlite:` paths in `storage.url` and `storage.state_url`, `server.tls`, `outbound.ca_file`, `brokers.<name>.ca_file`, `proxy.ca_dir`) are relative to the file's directory, so `mockingbird run -c elsewhere/mockingbird.yaml` and `cd elsewhere && mockingbird run` use the same files. Relative paths in environment variables and flags are relative to the working directory.

**Durations** are written as `250ms`, `30s`, `5m`, `12h` or `1d`.

[`examples/mockingbird.full.yaml`](examples/mockingbird.full.yaml) sets every key below; [`examples/mockingbird.yaml`](examples/mockingbird.yaml) is a commented starting point. A test (`crates/mockingbird/tests/it/docs_reference.rs`) checks that the full example loads and that this page names every key the code accepts.

In key paths, `<name>` stands for a name you choose (a mock, broker, profile, scenario or channel id), and `[]` for an item of a list. The top-level sections are `server`, `storage`, `bus`, `outbound`, `limits`, `pii`, `import`, `auth`, `audit`, `llm`, `observability`, `protocols`, `brokers` and `mocks`.

## `server`

Listeners and the node's role. Read at startup.

| Key | Type | Default | Range / rule | Env / flag | Meaning |
| --- | --- | --- | --- | --- | --- |
| `server` | section | | | | |
| `server.host` | IP address | `127.0.0.1` | A non-loopback address needs `auth.mode: oidc` or `server.insecure_no_auth` | `MB_HOST`, `--host` | Bind address of every port. |
| `server.port` | port | `8080` | | `MB_PORT`, `--port` | Data plane: REST, SSE, GraphQL, SOAP and WebSocket mocks. When it is not set and 8080 is taken, `init` and `server start -c` write a free port here (see [Quickstart](quickstart.md)). |
| `server.admin_port` | port | `9090` | Must be free when set explicitly | `MB_ADMIN_PORT`, `--admin-port` | Admin API, console and ops endpoints. When it is not set explicitly and 9090 is taken, `serve` starts without it and logs a warning, while `init` and `server start -c` write a free port here. |
| `server.grpc_port` | port | `50051` | Must be free when set explicitly | `MB_GRPC_PORT`, `--grpc-port` | gRPC (h2c, or h2 over TLS with `server.tls`), shared by every gRPC mock. Skipped with a warning like `admin_port`; `init` and `server start -c` write a free port here when it is not set and 50051 is taken. |
| `server.insecure_no_auth` | bool | `false` | | `--insecure-no-auth` | Allow a non-loopback `server.host` with `auth.mode: none`. Everyone who reaches the admin port is then a sysadmin. |
| `server.api_webhook_secret_refs` | list of globs | `["env:*_WEBHOOK_SECRET", "env:MB_WEBHOOK_*"]` | `*` matches any run of characters | | Secret references a mock's webhook target may use when it is set through the Admin API; anything else is a 422. Mocks in this file are not restricted. |
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
| `storage.session_ttl` | duration | `24h` | | | A session's state expires this long after it was last used: any request that reads or writes it (a create, a read served from state, a list that seeds it) restarts the clock. |

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
| `outbound.ca_file` | path | unset | PEM bundle | | CA certificates trusted in addition to the default roots (the Vault client also adds `VAULT_CACERT`). |
| `outbound.proxy` | URL | unset | `http://` or `https://` URL with a host | | Proxy for outbound requests, the Vault client's included. |
| `outbound.no_proxy` | list of strings | `[]` | Host names, or `.domain` suffixes | | Hosts reached without the proxy. |

## `proxy`

An HTTP proxy that lets unmodified apps reach mocks by their real host names: it intercepts HTTPS to every host a mock lists in `mocks.<name>.hosts` (and to `proxy.images`) with a certificate from a local CA, and passes other hosts through or blocks them. Started by `serve` on nodes serving the data plane; `mockingbird proxy` runs it on its own. See [the proxy guide](proxy.md). Read at startup.

| Key | Type | Default | Range / rule | Env / flag | Meaning |
| --- | --- | --- | --- | --- | --- |
| `proxy` | section | | | | |
| `proxy.enabled` | bool | `false` | | `--proxy` | Whether `serve` starts the proxy. |
| `proxy.host` | IP address | `127.0.0.1` | Off loopback, `proxy.unmapped` must be `block` | | The proxy's bind address. Off loopback anyone who reaches the port can use it, so `pass` (an open relay) is refused there. |
| `proxy.port` | port | `8888` | | `--proxy-port` | The proxy's port. When it is not set, 8888 is taken and `server start -c` runs the proxy, a free port is written here. `proxy env` and `run` read it. |
| `proxy.unmapped` | `pass`, `block` | `pass` | | | What happens to a host no mock claims: `pass` tunnels it to the real host; `block` answers 502 problem+json naming the host. Each such host is logged once. |
| `proxy.vendor_subdomains` | `block`, `pass` | `block` | | `mockingbird proxy --vendor-subdomains` | What happens to an unmapped host that shares a registrable domain (eTLD+1 by the Public Suffix List) with a host a mock lists, like `verify.twilio.com` or `twilio.com` next to a mocked `api.twilio.com`: `block` answers 502 problem+json naming the host and the mocked hosts of its domain, even under `proxy.unmapped: pass`, so test traffic never reaches the vendor a mock stands in for; `pass` leaves it to `proxy.unmapped`. IP literals and single-label hosts are never matched, and `foo.co.uk` and `bar.co.uk` are different domains. |
| `proxy.images` | list of strings | `[]` | A host name (no port), optionally followed by a path prefix: `image.tmdb.org`, `movies-proxy.vercel.app/ipx` | | Requests answered with a generated placeholder PNG (a solid colour from the path; size from a `w123` path segment or an ipx-style `s_400x600`/`w_400` modifier, else 300×450). A path prefix scopes it to part of a host a mock serves, e.g. an image endpoint the browser fetches. |
| `proxy.ca_dir` | path | `~/.mockingbird/ca` | Relative to the config file | `MB_CA_DIR` | Where the local CA (`ca.pem`, `ca.key`) is kept; created on first use, never installed system-wide. |

## `limits`

Global admission limits. Per-mock limits are under [`mocks.<name>.limits`](#mocksnamelimits). Read at startup.

| Key | Type | Default | Range / rule | Env / flag | Meaning |
| --- | --- | --- | --- | --- | --- |
| `limits` | section | | | | |
| `limits.max_concurrent_generations` | integer | `64` | 1..=4096 | | LLM generations running at once across every mock on the node. A call that gets no slot within its LLM timeout is served by the faker (`mb_generation_queue_timeouts_total`). |
| `limits.max_request_bytes` | integer (bytes) | `1048576` | 1024..=67108864 | | Larger request bodies are answered 413 and recorded. |
| `limits.max_response_bytes` | integer (bytes) | `1048576` | 1024..=16777216 | | Larger generated responses are replaced by faker output. |
| `limits.output_budget_tokens` | integer (tokens) | `4000` | 0, or 100..=100000 | | Completion tokens the LLM is asked to write per answer. When a response schema's estimated answer is larger, the LLM writes a shorter one: optional object and array members the request does not name (such as `append_to_response` sub-resources it did not ask for) are left out, except a list item's content (its one object member holding more of the item's scalar members than the rest of it, such as a GeoJSON feature's `properties`), arrays get a `maxItems`, and then required objects below the top level are left out and filled by the faker. A word of the request names a member sharing its stem (`type=track` names `tracks`), and an array is capped below the request's `limit` only when nothing else fits. Output tokens set latency, so this trades a little fidelity on big pages (GitHub search, TMDB details) for answers in seconds. `0` turns it off. |
| `limits.hedge_after` | duration | `2s` | 0s..=60s | | How long an LLM call of a served answer (generation or the executor's fill; not streams) may take, its whole reply, before the same request is sent once more; the first answer that passes its check (schema, echoes) is taken and the other call is dropped, which cancels its request and charges it its prompt (estimated at 4 bytes a token). An answer that does not pass sends the second request at once, before any repair round; a call that errors is not hedged. While no answer has passed, a third request goes out at three times this (6 s by default; never sooner than this after the second), so a call whose first two requests both stalled is still answered well before `limits.generation_timeout`; no more go out after that. A provider router such as OpenRouter serves a hedge from another provider, which cuts the slow tail one provider gives (time to first token varies from under 1 s to 8 s across providers of one model): on Stripe creates, 2s took p95 from 13.9 s to 4.7 s. On `openrouter.ai`, unless the profile's `extra_body` sets `provider`, each request is routed by another rule: the first to the fastest provider (`provider: {sort: "throughput"}`), the first hedge by OpenRouter's default spread by price, the second to the lowest latency (`sort: "latency"`); a model slug ending in `:nitro` or `:floor` keeps its own routing. Sorted providers can cost more per token than the cheapest. The cost is more calls: with replies taking 2–3 s, about half are hedged. Each hedge needs the token budget, a free `limits.max_concurrent_generations` slot right away (it never waits for one) and time before the generation timeout. `0s` turns hedging off. |
| `limits.generation_timeout` | duration | `20s` | 1s..=600s | | Longest an answer the mock serves waits on the LLM, its repairs included, before the faker answers (`X-Mockingbird-Fallback-Reason: LLM request timed out: …`, naming this setting); a profile whose `timeout` is shorter keeps its own. One slow provider call then costs an app at most this long. Contract imports from documentation keep the profile's `timeout`. |
| `limits.repetition_guard` | bool | `true` | | | Stop an LLM reply that has fallen into a loop, the way some models write `0_0_0_…` or the same line hundreds of times until `max_tokens`. Mockingbird watches the streamed text (answer and reasoning, separately). An 8 KiB window holding at most 256 distinct 8-byte sequences is of low variety: a piece of up to 200 characters repeated, or a few such pieces shuffled, is; most real data is not (an array of objects alike but for their ids holds about 700, base64 and long numbers nearly 8,000). Some real answers are of low variety too (a seat map, an array of a few enum values, a bitmap, placeholder items), so the stream is stopped only once the text has been of low variety throughout `limits.repetition_guard_span` bytes. Then the same request is sent once more; a retry that loops too falls back to the faker with `X-Mockingbird-Fallback-Reason: LLM output repeated itself: provider …, aborted after about N tokens …`. Stopped attempts are charged their estimated tokens (4 bytes to a token), reasoning bytes as reasoning tokens. Stops are counted in `mb_llm_cut_replies_total{reason="repetition"}`, next to `reason="max_tokens"` for replies the provider cut at the limit and `reason="answer_cap"` for streamed answers stopped at four times the larger of their schema's estimated answer and `limits.output_budget_tokens` (at least 8 KiB; none with the budget off), whose text so far is then salvaged like a reply cut at `max_tokens`; an answer that is looping when it passes that cap is stopped as a loop (`reason="repetition"`) and retried, sooner than the span. On, LLM calls are sent with `stream: true` (and `stream_options.include_usage`, dropped for good if the endpoint rejects it). An endpoint that answers with a plain JSON reply anyway is read as one; one that refuses `stream` itself (a 4xx naming it) gets plain requests from then on, unguarded. A profile whose `extra_body` sets `stream` or `n` above 1 is never streamed, so its calls are unguarded. A stream that ends with neither `[DONE]` nor a finish reason is a transport error, retried once. A profile can turn the guard off for itself with `llm.profiles.<name>.repetition_guard: false`. `false` here turns it off for every profile: plain requests, whole replies. |
| `limits.repetition_guard_span` | integer (bytes) | `32768` | 8192..=1048576 | | How long a reply must have been of low variety before `limits.repetition_guard` stops it. Shorter stops loops sooner and saves more tokens, at more risk of stopping a legitimate low-variety answer: on three recorded benchmark runs, 32 KiB stopped 98 of nemotron-3.5-lightning's 258 loops in one run (most of its loops end at `max_tokens` before 32 KiB, as loops tokenize densely) and no finished answer of the other models; 16 KiB stopped 217 of them, and also 3 answers that had finished (each itself repetitive). |

## `pii`

The output PII scrubber replaces personal-data look-alikes in generated responses with synthetic values of the same shape. Scrubbed output still validates against the contract, and scrubbing twice changes nothing. A replacement is not drawn from the value it replaces: it is random, or, in an LLM answer of a mock with a `faker_seed` (or a request with `X-Mockingbird-Seed`), drawn from the seed, the call (its PII masked) and the value's place in the answer, so a seeded run repeats without the replacement revealing or linking its original. The masking has limits (a value of the request it misses still reaches the seed, see [`faker_seed`](#mocks)). Request bodies kept or shown as context are scrubbed at random whatever the seed. Read at startup.

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
| `auth.session_idle_timeout` | duration | `1h` | At most `30d`; `0` disables | | A console login also ends once it has not been used for this long (no request with its cookie). Activity is recorded at most once a minute, or every half timeout when that is shorter. |
| `auth.sysadmin_subjects` | list | `[]` | | | OIDC `sub` values that are always sysadmin. |
| `auth.public_metrics` | bool | `true` | | | `false`: `/metrics` needs the same credentials as the Admin API. |
| `auth.oidc` | section | unset | Required in `oidc` mode | | The OIDC relying party. |
| `auth.oidc.issuer` | URL | | Exactly the discovery document's `issuer`, trailing slash included | | OIDC issuer. |
| `auth.oidc.client_id` | string | | | | Client id. |
| `auth.oidc.client_secret` | secret reference | unset | | | Client secret; omit it for a public client. |
| `auth.oidc.scopes` | list | `[openid, profile, email, groups]` | | | Scopes requested at login. Add `offline_access` when your provider issues refresh tokens only for it: console roles are then refreshed from the provider after login ([roles after login](console.md#auth-modes)). |
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
| `llm.profiles.<name>.base_url` | URL | required | | `MB_LLM_BASE_URL` (default `https://openrouter.ai/api/v1`) | OpenAI-compatible base URL. |
| `llm.profiles.<name>.model` | string | required | | `MB_LLM_MODEL` (default `deepseek/deepseek-v4.1-flash-1017`, the dated 2026-10-17 snapshot of DeepSeek V4.1 Flash on OpenRouter) | Model name. If the provider stops serving it, calls fail and, unless the profile sets `fallback`, the mock falls back to the faker, with the reason in `X-Mockingbird-Fallback-Reason`; Mockingbird never picks another model itself. |
| `llm.profiles.<name>.api_key` | secret reference | unset | | `MB_LLM_API_KEY` (stored as `env:MB_LLM_API_KEY`) | Provider API key. |
| `llm.profiles.<name>.output_mode` | string | `json_schema` | `none`, `json_object` or `json_schema` | `MB_LLM_OUTPUT_MODE` | How the provider is asked for JSON. `json_schema` sends the answer's schema as `response_format` (structured outputs, with `strict: false`), and is what most answers valid on the first try: with `deepseek/deepseek-v4.1-flash-1017` first try rose from 66% to 98%. Free-text strings in that schema carry a 2,000-character limit, so a model can't loop in one. A schema the provider rejects, such as OpenAI with a root that isn't an object, is asked for in JSON mode instead, with a warning, and an empty answer is tried once more in JSON mode. An array answer is asked for as `{"items": [...]}`, except with `none`. `json_object` sends `response_format: {type: json_object}` (JSON mode). `none` sends no `response_format`, for models that answer worse or not at all with one. |
| `llm.profiles.<name>.json_schema` | bool | unset | Not with `output_mode` | `MB_LLM_JSON_SCHEMA` | Deprecated alias of `output_mode`: `true` is `json_schema`, `false` is `json_object`. |
| `llm.profiles.<name>.timeout` | duration | `60s` | | `--llm-timeout` (profile `default`) | Time before the call falls back to the faker; a served answer waits at most `limits.generation_timeout` (20s) when that is shorter. |
| `llm.profiles.<name>.temperature` | number | `0.7` | | | Sampling temperature. |
| `llm.profiles.<name>.max_tokens` | integer | `16384` | | | Completion token limit. A reply cut at this limit falls back to the faker, with the reason in `X-Mockingbird-Fallback-Reason`; a reply looping towards it is stopped first (see `limits.repetition_guard`). |
| `llm.profiles.<name>.repetition_guard` | bool | `true` | | | `false` turns `limits.repetition_guard` off for this profile's calls: they are sent as plain (unstreamed) requests and a looping reply runs on to `max_tokens`. Also in the Admin API (`repetition_guard` on `PUT /llm-profiles/<name>`). |
| `llm.profiles.<name>.fallback` | string | unset | A profile name | | Profile tried when this one's provider fails. |
| `llm.profiles.<name>.headers` | map | `{}` | Lowercase header names; not `authorization`, `content-type`, `content-length`, `host` or `transfer-encoding` | | Extra request headers sent to the provider, such as Cloudflare AI Gateway's `cf-aig-authorization` or Azure OpenAI's `api-key`. A value may embed secret references as `${env:NAME}`, `${file:/path}` or `${vault:<mount>/<path>#<field>}` (`Bearer ${env:CF_AIG_API_KEY}`); a header whose name contains `auth`, `key`, `token`, `secret`, `password` or `cookie` must use one. Through the Admin API each reference must match `server.api_secret_refs`, and only a sysadmin may set them. |
| `llm.profiles.<name>.headers.<name>` | string | | | | One header value. |
| `llm.profiles.<name>.extra_body` | map | `{}` | Not `model`, `messages`, `response_format`, `temperature`, `max_tokens` or `stream`; at most 8 KiB as JSON | | JSON merged into every request body, such as OpenRouter's `provider` routing or `reasoning` settings. Requests to `openrouter.ai` send `provider: {sort: "throughput"}` unless this sets `provider` (OpenRouter otherwise spreads calls by price over every provider of the model, slow queued tiers included; hedges are routed by other rules, see `limits.hedge_after`), and `reasoning: {enabled: false}` unless this sets `reasoning` (hidden reasoning was most of a large answer's tokens and latency); a model that rejects it (one that always reasons) is asked again with `reasoning: {effort: "minimal"}`, then without `reasoning`. The client remembers the step and logs a warning naming the model once. Reasoning tokens count against `max_tokens`. |

`--offline` serves every mock with the faker and makes no LLM calls.

A provider that refuses a profile's key for good is not called again for the rest of the process: a `401`, a `402`, or a `403` (or OpenAI's quota `429`) whose body says the key is past its limit or out of credit (`Key limit exceeded`, `Insufficient credits`, `insufficient_quota`) marks the profile unusable. Its calls (generation, the executor's fill, error bodies, streams, record pools) then fall back to local fill and the faker at once, with no retry, hedge or timeout, and their answers count as the faker's, as offline ones do (`X-Mockingbird-Fallback-Reason: the LLM provider is unusable for this run: …`). Another `403` is an ordinary error. A `429` backs the profile off for its `Retry-After` in seconds (60 s without one, 15 min at most): calls during the backoff fall back at once, without waiting, and the provider is called again after it. Each is logged once, as a warning naming the profile, the status and the reason. `/readyz` (and `server status`) lists every profile under `llm` with its `state` (`ok`, `unusable` or `backing_off` until `until_unix`) and `fallbacks`, the calls that fell back because of it, and reports the server `degraded` while one is not `ok`. A reload that changes the profile's `api_key` or `base_url` clears the mark; other changes keep it.

## `observability`

| Key | Type | Default | Range / rule | Env / flag | Meaning |
| --- | --- | --- | --- | --- | --- |
| `observability` | section | | | | |
| `observability.request_log_retention` | integer | `1000` | | | Request-log entries kept per mock. |
| `observability.redact_fields` | list | `[password, token, secret, authorization, apiKey, api_key]` | | | A recorded JSON, form or query key is redacted when, lowercased and without `_`, `-` and `.`, it contains one of these (normalized the same way). |

`OTEL_EXPORTER_OTLP_ENDPOINT` or `OTEL_EXPORTER_OTLP_TRACES_ENDPOINT` enables OTLP trace export, and `RUST_LOG` sets the log filter (default `info`). Spans are exported over HTTP/protobuf only: `OTEL_EXPORTER_OTLP_PROTOCOL=grpc` (or `OTEL_EXPORTER_OTLP_TRACES_PROTOCOL=grpc`) is not followed and logs a warning at startup, so point the endpoint at the collector's HTTP port (usually 4318).

## `protocols`

Knobs shared by every mock. Read at startup.

| Key | Type | Default | Range / rule | Env / flag | Meaning |
| --- | --- | --- | --- | --- | --- |
| `protocols` | section | | | | |
| `protocols.graphql` | section | | | | GraphQL mocks. |
| `protocols.graphql.path` | path | `/graphql` | Starts with `/` | | Endpoint below each GraphQL mock's route prefix; `mocks.<name>.graphql_path` overrides it for one mock. |
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
| `brokers.<name>.client_id_prefix` | string | `mockingbird` | | | Prefix of client ids and consumer groups: a mock's Kafka group (and AMQP shared queue) is `<prefix>-<mock>` (`<prefix>-<workspace>.<mock>` outside `default`). With a shared (non-`local`) bus, MQTT consumers subscribe to `$share/<group>/<topic>` with that same per-mock group, so the instances of one mock share the messages while every other mock gets them all. |

## `mocks`

Mocks declared as code, by name. They are reconciled into the store at startup and on `--watch`, by the leader; they are read-only through the Admin API.

| Key | Type | Default | Range / rule | Env / flag | Meaning |
| --- | --- | --- | --- | --- | --- |
| `mocks` | map | `{}` | | | Mocks by name. |
| `mocks.<name>` | section | | Unique per workspace | | One mock. |
| `mocks.<name>.workspace` | string | `default` | | | Workspace (created if missing). |
| `mocks.<name>.contract` | path | required | Relative to the config file | | OpenAPI 3.x, Swagger 2.0, GraphQL SDL, protobuf, WSDL 1.1 or AsyncAPI 2.x/3.0 contract. |
| `mocks.<name>.world` | path | unset | Relative to the config file; YAML, or JSON for a `.json` file | | The contract's [world description](world.md#in-a-config-file): an overlay of the entries to set. On every reconcile (startup, and with `--watch` each edit of the config, the contract or this file) it is validated against the active contract version, and its entries that apply are stored as a new revision by `user` when they differ from the newest revision's (an unchanged file adds none). Refused entries are logged at warn level and do not stop the server; until a load whose file has none, `world show` lists them under `refused` and `doctor` warns about them. A file that cannot be read or parsed, or is refused whole (a file over 1 MiB, a description over 256 KiB in its JSON form, nested over 32, not of the format's shape), fails the reconcile naming the file. Concurrent reconciles (replicas, a reload beside an API write) store it once. Removing the key leaves the revisions as they are. |
| `mocks.<name>.route_prefix` | path | `/<name>`; `/` with `hosts` | | | Where the mock is mounted on the data plane. A mock bound to `hosts` defaults to `/`: the proxy forwards the app's own paths, and the host tells the mock apart. |
| `mocks.<name>.path_rewrite` | map | unset | `strip_prefix`, `add_prefix` | | Rewrites the path below `route_prefix` before it is matched against the contract (see [the proxy guide](proxy.md#one-contract-under-another-host-and-path)). |
| `mocks.<name>.path_rewrite.strip_prefix` | string | unset | `/seg(/seg)*`, not `/` | | Removed from the front of the path below `route_prefix` (at a segment boundary) before the contract sees it. |
| `mocks.<name>.path_rewrite.add_prefix` | string | unset | `/seg(/seg)*`, not `/` | | Put in front of the path below `route_prefix` (after `strip_prefix`). Mounts a contract under another base path: TMDB's `/3/...` contract at `movies-proxy.vercel.app/tmdb/...` with `route_prefix: /tmdb` and `add_prefix: /3`. |
| `mocks.<name>.graphql_path` | path | `protocols.graphql.path` | Starts with `/` | | A GraphQL mock's endpoint below its `route_prefix`, for APIs whose endpoint is not `/graphql` (Shopify's `/api/<version>/graphql.json`). Other mocks keep the server-wide path. |
| `mocks.<name>.hosts` | list of strings | `[]` | Lowercase host names, no port | | Hosts the mock answers for (by `Host`, e.g. through `mockingbird proxy`). With hosts, two mocks may share a route prefix; a mock with hosts is not reached under other hosts. The first is also the host the URL members of the records it creates link under, where the contract's example of a member names no other ([URL members](world.md#at-runtime)). |
| `mocks.<name>.persona` | string | unset | | | Free text added to every prompt (locale, realism, style). |
| `mocks.<name>.llm_profile` | string | `llm.default_profile` | | | LLM profile. |
| `mocks.<name>.header_overrides` | bool | `true` | | | Honour `X-Mockingbird-*` request headers (status, latency, scenario, seed, `X-Mockingbird-Chaos: off`). |
| `mocks.<name>.faker_seed` | integer | unset | `0` to `2^64-1` | | Seeds the faker: the same seed, contract and request (path parameters, query and body) give the same faker body, with dates drawn back from 2026-01-01T00:00:00Z instead of the clock. Unset, offline answers use seed `0` (so `--offline` runs repeat) and LLM fallbacks are random. `X-Mockingbird-Seed: <n>` overrides it for one request, but does not override mentions, list items or stored resources; a plain read's own stored answer it does override, on the node that stored it and until a restart, since that is remembered per process. Mocks created through the Admin API take it as `faker_seed` on create and patch (`null` clears it), `mockingbird mocks update --faker-seed <n>` (`none` clears it), and the MCP `create_mock` and `update_mock` tools. The seed also draws what the engine fills into an LLM answer itself (a value breaking its pattern or format, a required member the answer left out, a member the output budget cut, a stream item's repair), the declared response headers' values (ids, rate-limit numbers; an `X-RateLimit-Reset` time still follows the clock) and the PII replacements in an LLM answer: repairs from the seed and what they repair, with the operation and the request (path parameters, query and body), headers from the seed, operation, status and request, so identical runs with identical LLM answers answer alike and different requests still get different values. Every repair's seed, the PII replacements' included, takes the request with the PII the scrubber finds in it masked first, so two requests that differ only in a PII-shaped value (an email, a phone number) get the same refills and replacements. A PII replacement is drawn from that seed (`X-Mockingbird-Seed` included), the operation, the masked request and the place of the value in the answer (in a write answering no body, the place in its effects), not from the value replaced: the seed is readable through the config and the Admin API, so a replacement derived from the original would let anyone holding it test candidate originals (an SSN, a phone number) offline, and would give one real value the same fake one in every request. The trade-off: the same original in two calls, or in two places of one answer, gets different replacements, and another original in the same place of the same call gets the same one. The masking is the scrubber's, with its limits: JSON numbers, strings over the scrub size cap (64 KiB) and values its patterns miss are not masked, so they reach the seed as they are, and a request holding the same original as the answer, unmasked, does seed its replacement. Error bodies the engine writes itself ignore `X-Mockingbird-Seed`: each is written once per operation, status and cause, whatever the request, and seeded by `faker_seed` alone. PII scrubbed from request bodies kept or shown as context is always replaced at random, so a seeded prompt showing stored bodies that hold real PII differs between runs, and so can its answer. Unset with an LLM, these are random. Event messages are not seeded. The values are reproducible with the same Mockingbird version. |
| `mocks.<name>.broker` | string | picked automatically | A `brokers` name | | Broker the mock's channels use. |
| `mocks.<name>.data_plane_auth` | section | | | | How callers of the mock authenticate. |
| `mocks.<name>.data_plane_auth.mode` | `none`, `api_key`, `passthrough_validate` | `none` | | | `api_key`: a `data-plane` API key of the mock's workspace is required. `passthrough_validate`: credentials must be present in the form the contract's security requirements declare, without being verified. |
| `mocks.<name>.data_plane_auth.header` | string | `x-api-key` | Lowercase | | Header carrying the key in `api_key` mode (or `?api_key=`). |
| `mocks.<name>.consistency` | `on`, `off` | `on` | | | `on`: a session repeating a read gets the same answer (`X-Mockingbird-Source: cache`, 500 answers per session, cleared by any write of the resource type and by a session reset, seed or mock reset, on every node), and the items of a list answer, the entities any answer mentions and a read by id's own answer (repeated as `state`; not one that names `X-Mockingbird-Seed`) are stored so a later read by id returns them. A read with a query, on a route read by a field such as a number, by a name its `id` was set to on a route with an earlier path parameter (`/repos/{owner}/{repo}` or `/orgs/{org}/teams/{team_slug}` with a string `id`), or on a route naming its target after another parameter with no member showing that parameter's value, is not stored and can change after a later answer mentions that entity. On such a route, unless the parameter is named as an id, a read needs to know the owner. A resource listed or written under the same route template answers for the collection path it came from (`/orgs/octo/teams` for `/orgs/{org}/teams/{slug}`). Otherwise a name must show its owner: every relative path ending in the name must end in the owner and name (`full_name`, `path_with_namespace`), else a member tied to the parameter (`namespace`, `owner.login`, `org_id`, `organization`) decides, else a link showing the owner. A number that shows nothing is still served, and so is a string id no name member holds (`txn_1`), unless a tied member names another owner. Any other read is generated. A write acts unless the resource is known to be another's, by a link elsewhere too; then a delete leaves it and an update does not change it. With an LLM, `on` also applies a 204 update's declared members to the stored copy, and remembers a write's request when its answer did not show it (a create answering less than its read, an action answering 204, a create under a parent, what a 204 update could not apply): the resource's next generation is told of it, so a read that could have answered `state` generates, and stops trying after 2 failed attempts until a later write ([how](concepts.md#sessions-and-state)). `off` generates every answer anew, applies none of the changes an answer lists to other resources and remembers no request. Reads of a stored resource are answered from session state (`state`) either way, unless a remembered request is waiting. |
| `mocks.<name>.unknown_ids` | `inferred`, `generate`, `404`, `null` | `inferred` | | | How a read by path id of a resource the session does not hold (created or listed) is answered: generated, or the operation's declared 404 (its `default` when it declares that instead; with neither, a declared `410`; else problem+json). Unset (`inferred`), it is decided per resource type: a type the contract can create (an operation of kind create, in the world description, answers it) answers 404 once the session has created, changed or deleted a record of it, or the Admin API seeded one (a marker kept in the session's state, which outlives its records; what a read, a list or a search stored does not count); any other type, and a creatable one before that, is generated, so reference data the app reads from its config (a Stripe price id) keeps answering. `generate` or `404` decides for every type of the mock. `null` is `404` (REST has no null answer), except in GraphQL, where what `404` answers with a `NOT_FOUND` error answers the field `null` with no error: a lookup by an argument other than the identity (`product(handle:)`, `user(login:)`, and lookups below the root), and a read of a record the session deleted. A root lookup by an unknown `id` still generates, as under `404`. A non-null field keeps the `NOT_FOUND` error, since `null` there would null its parent. YAML `unknown_ids: null` sets this value (it is not read as unset; a null or empty value logs a warning at load, since an empty key may not mean it), as `"null"` does, and so does JSON `null` in a PATCH (creating a mock through the Admin API takes only the string `"null"`). For a GraphQL API pick `404` when its docs say an unknown record answers an error (GitHub), `null` when they say it answers `null` with no error (Shopify Storefront). `get_world` lists each type's answer and why under `unknown_ids`, and each GraphQL root lookup by a value other than the identity under `unknown_ids.lookups`; while the mock leaves it `inferred`, `start_server` (those the app's source calls, `user(login:`) and `server_status` (those the app called) name them in a `next_steps` line, since their unknown values answer made-up records. An id the session deleted under that path's parent gets that 404 under any setting, with an LLM and `consistency` on. The path parameter counts by what it selects, not by its name: the one the world description writes as the read's `target.path_param`; else the identity's parameter when it is the route's last one, a format suffix aside (`/v1/customers/{customer}`, `/Messages/{Sid}.json`); else one further up the route that is the identity field or id-like (`id`, `orderId`, `movie_id`). Reads without one are never 404'd, but for a GraphQL root lookup by an argument other than the identity (an inferred lookup argument such as `user(login:)`, a world `target.member` or `identity.alternates` such as `product(handle:)`), whose unknown value answers the field `null` with a `NOT_FOUND` error ([protocols](protocols.md#graphql-reads-by-a-lookup-argument)); a forced status wins. On a route naming its target after another parameter, a value some owner's resource goes by is generated, as the route may not be able to tell whose it is. |
| `mocks.<name>.default_scenario` | string | unset | A key of `mocks.<name>.scenarios` | | Scenario applied when a call sends no `X-Mockingbird-Scenario` header. |
| `mocks.<name>.shares` | string, `none` | unset: the default group of its domain | A name: another mock of its workspace is in that group (names it, or joined it by default) | | The group the mocks of one API (GitHub's REST and GraphQL) keep their sessions in together: a record one stores, the others read. **By default** a mock that sets no `shares` joins the group of the other mocks of its workspace that set none and whose `hosts` share a registrable domain with its own (eTLD+1 by the Public Suffix List, as `proxy.vendor_subdomains` reads it: `api.github.com` and `uploads.github.com` are `github.com`); mocks linked so, directly or through another, form one group named after the domain (the alphabetically first, when they share several). A mock without hosts, or with only IP addresses and single-label hosts (`localhost`), joins none, and nor does one whose domain no other mock has. A name puts the mock in that group instead (mocks of other domains, or with no hosts, included); `none` keeps it out of every group, the default one too: set it on one of two mocks of one domain that must not share records, such as two versions of an API on one host. The group is worked out from the stored mocks whenever one changes (its hosts, its `shares`, or one added or deleted), so the mocks it regroups are rebuilt into their new group; the sessions written under the old group stay there. A type is the same when its names agree beyond case and a plural `s` (`issues`, `issue`), or as the world's `types.<t>.same_as` says. Where the world gives a type no `same_as`, the group's contracts are read for it: a type of one mock is a type of another when their whole names agree but for case, punctuation and a plural (REST `pulls`, whose answers are `pull-request`s, and GraphQL `PullRequest`), or one is a short form of the other's whole name (`repos` and `Repository`, `orgs` and `Organization`: the start of one word at least three letters longer; and GitHub's `pulls` and `PullRequest`), never by one word of a compound name (`users` is not `UserStatus`), and both declare the same identity (a `number` in both or in neither, and then an `id` or `node_id` in each). Nothing is inferred for a type when another type of the group, of any mock (two mocks of one protocol included), is named like it already, and a type of another mock that one of the reading mock's own types is named like is left to that one. A world's `same_as` always wins over this, for the reads of the mock whose world says it: the other mocks of the group still infer theirs. A record read by a member that is unique only under its owner (a pull request's `number` in its repository) is the one the path's owner holds, not the first of that number; the whole chain of the path's parameters counts (`/repos/{owner}/{repo}/…`: `a/r` and `b/r` are two repositories), as the record shows the repository it belongs to: in its own `url`, `html_url` or `repository_url` (`https://github.com/a/r/pull/5`), or in its `repository`, `baseRepository` or `base.repo` by a link, a `full_name` (`a/r`) or a `name` beside an `owner`. Its other members say nothing (a fork's pull request, whose `head` is another owner's, belongs to its base), names compare without case, and a record that shows no owner is taken under any. The types are those of the mounted mocks: a mock deleted, unmounted or left without a contract leaves its group's types. Members are matched between camelCase and snake_case, enum values by case and separators (`CLOSED` is `closed`, `NOT_PLANNED` is `not-planned`; to the reading member's `enum`, else, for a member typed as a string, the values its description lists, such as "either 'open' or 'closed'", as a spelling only: a value with no one match is kept, never refused; only a symbol is respelled, a value the mock that stored the record declares an `enum` for, or a string whose description lists that value, so text it declares a plain string keeps its spelling), and REST's `node_id` is GraphQL's `id` (each fills the other where the reading shape has none it can take), in the objects the record nests and the objects of its lists too, each as the reading shape declares it there (a union or interface, a `oneOf` of types such as GraphQL's `owner: RepositoryOwner`, as the type its `__typename` names, else the one declaring most of its members, an object none declares a member of kept as it is), six levels and 400 objects deep at most: REST's `owner: {id: 42, node_id: "U_1"}` read over GraphQL is `owner {id: "U_1"}`. A number where the reading shape declares a plain string (`type: string` with no `format`, `enum`, `pattern` or `const`) is that string, unless the member names a record (`id`, `<x>_id`), which is left to `node_id` or to fill; a scalar one shape holds flattened, named for the object the other holds it in, is nested there (GraphQL's `headRefName` is REST's `head.ref`, a trailing `Name` read as the member itself, and back), unless two members would fill one place; what the reading shape requires beyond that is completed from state on a read rather than generated; a path member named for its type (`pull_number` of `pulls`) also matches the record's `number`. `server start` (CLI and MCP `start_server`) answers `shares`, each mock's group as `{group, default}` (`null` for none) as the config's mocks alone group them (a mock of the workspace created through the Admin API on a persistent store also counts on the server), and `next_steps` naming mocks that answer for one host without a common group (one sets `none`, they name different groups, or the host has no registrable domain); `get_world` answers the mock's `shares` alike. The Admin API's session endpoints of any of the mocks reach the shared sessions. A `same_as` name is the other contract's stored type name as written. |
| `mocks.<name>.strict_requests` | bool | `false` | | | Refuse a request that carries a member its schema does not declare. By default an undeclared member (one an `additionalProperties: false` or `unevaluatedProperties: false` object does not list, at any depth of the body or nested in a query parameter; a query parameter the operation does not declare is never refused) is let through on every HTTP contract, and an `anyOf`/`oneOf` branch failing only on undeclared members counts as matching, as an app's SDK is often pinned to another version of a versioned API than the contract (stripe-node sending `payment_method_types` the newest spec no longer declares); every other violation (a missing required member, a wrong type, a bad enum value, a wrong shape) is still refused. A contract inferred by the smart import (from documentation pages, text or recorded traffic; `info.x-mockingbird-inferred: true`) guesses at its request schemas, so by default its requests are not refused for a missing member or a scalar of another type either; a wrong shape or a bad enum value still is. `true` validates every request strictly. |

### `mocks.<name>.chaos`

Fault injection. Absent injects nothing. A request with `X-Mockingbird-Chaos: off` bypasses it when header overrides are enabled. An injected error uses the operation's declared 503, 500, other 5xx or 429 response; else its `default` response (HTTP 503, faker-filled, its message-like field set to `Service Unavailable (injected by chaos)`); else the protocol's native one (HTTP 503 problem+json, gRPC `UNAVAILABLE`, SOAP Fault), and is marked `X-Mockingbird-Chaos: error` (gRPC metadata `x-mockingbird-chaos: error`, GraphQL `extensions.chaos = "error"`).

| Key | Type | Default | Range / rule | Env / flag | Meaning |
| --- | --- | --- | --- | --- | --- |
| `mocks.<name>.chaos` | section | unset | | | The mock's chaos. |
| `mocks.<name>.chaos.error_rate` | number | `0` | 0.0..=1.0 | | Share of calls answered with an injected error. `0` never injects. |
| `mocks.<name>.chaos.latency_jitter` | section | unset | | | Uniformly random latency added to every call. |
| `mocks.<name>.chaos.latency_jitter.min` | duration | `0s` | `min` ≤ `max` | | Least added latency. |
| `mocks.<name>.chaos.latency_jitter.max` | duration | `0s` | At most `30s` | | Most added latency. |
| `mocks.<name>.chaos.drop_stream_rate` | number | `0` | 0.0..=1.0 | | Share of streamed responses cut short. |

`MB_CHAOS_SEED` (an integer) seeds the random choices, for repeatable test runs.

### `mocks.<name>.webhooks`

Where the mock POSTs an event after each successful (2xx) write: a create, update, delete or other command, never a read. The body is the contract's OpenAPI `webhooks`/`callbacks` payload when it defines one, else a Stripe-shaped event `{id, object: "event", type: "<resource>.<created|updated|deleted>", created, data: {object: <response>}}`. Each delivery is retried 3 times (after 1 s, 2 s, 4 s) on a non-2xx answer or transport error and never delays the response. See [the webhooks guide](webhooks.md).

| Key | Type | Default | Range / rule | Env / flag | Meaning |
| --- | --- | --- | --- | --- | --- |
| `mocks.<name>.webhooks` | list | `[]` | At most 16 | | The mock's webhook targets. |
| `mocks.<name>.webhooks[]` | section | | | | One target. |
| `mocks.<name>.webhooks[].url` | URL | required | Absolute `http`/`https`, no `user:password@` | | Where events are POSTed. Redirects are not followed (a 3xx is a failed attempt). The query is shown as `?<redacted>` by the Admin API and in audit events, and is kept off the delivery log. |
| `mocks.<name>.webhooks[].events` | list of strings | `["*"]` | Not empty | | Which events go to this target: operation ids (`createCustomer`), event types (`customer.created`) or `*`. |
| `mocks.<name>.webhooks[].secret` | secret reference | unset | `env:`, `file:` or `vault:` | | Signing key. A reference that does not resolve keeps the mock serving; that target's deliveries are recorded `failed` and nothing is sent unsigned. |
| `mocks.<name>.webhooks[].signing` | `stripe`, `hmac-sha256`, `none` | `hmac-sha256` with a secret, else `none` | `stripe` and `hmac-sha256` need `secret` | | `stripe`: `Stripe-Signature: t=<unix s>,v1=<hex HMAC-SHA256(secret, "<t>.<body>")>`. `hmac-sha256`: `X-Mockingbird-Signature: sha256=<hex HMAC-SHA256(secret, body)>`. |
| `mocks.<name>.webhooks[].delay_ms` | integer (ms) | `0` | 0..=60000 | | Wait before the first attempt. |

### `mocks.<name>.limits`

Admission limits of one mock. Unset fields use the global [`limits`](#limits), or no limit.

| Key | Type | Default | Range / rule | Env / flag | Meaning |
| --- | --- | --- | --- | --- | --- |
| `mocks.<name>.limits` | section | `{}` | | | The mock's limits. |
| `mocks.<name>.limits.rate_limit` | section | unset | | | Token bucket, per mock on each node. Over the limit, calls are answered 429 with `Retry-After` (in the contract's 429 shape when it declares one). |
| `mocks.<name>.limits.rate_limit.requests_per_second` | number | | (0, 100000] | | Refill rate. |
| `mocks.<name>.limits.rate_limit.burst` | integer | | 1..=100000 | | Bucket size. |
| `mocks.<name>.limits.daily_token_budget` | integer | unset | At least 1 | | LLM tokens per UTC day across every replica (flushed to the store every 5 s; the leader prunes usage older than 90 days daily). Once spent, calls are served by the faker and `/readyz` reports it. |
| `mocks.<name>.limits.record_pool_tokens` | integer | `30000` | At least 0; under 5000 keeps no pool | | LLM tokens the mock's record pool may spend. When the mock starts and when its world changes, the model writes about 10 schema-valid records of each type the executor stores (no more than a page of each of the type's lists admits), in the background with the persona, so that a read of an id the session does not hold and a first list of a type are answered from state in milliseconds instead of by a model call, and a create takes the members its request did not send from a pool record instead of a model fill (the request record notes `pool`). Until a type's records are written, calls go on as before; a type whose read schemas did not change keeps its records across world changes. A list the session never wrote answers the pool's records, and from then on so does every list of the type that names no scope. A call that would pass the cap at the cost of the dearest call so far is not made, and a cap under 5000 keeps no pool; the cap counts per start or world change of the mock, and per replica (each fills its own pool). With a `daily_token_budget` the pool spends at most a tenth of it. A pool call is made only while a generation slot (`limits.max_concurrent_generations`) stays free for requests after it takes one, else a second later, so requests never queue behind it; with a single slot the mock keeps no pool. `0` keeps no pool. A mock without an LLM profile keeps none. |
| `mocks.<name>.limits.max_request_bytes` | integer (bytes) | `limits.max_request_bytes` | 1024..=67108864 | | Overrides the global value for this mock. |
| `mocks.<name>.limits.max_response_bytes` | integer (bytes) | `limits.max_response_bytes` | 1024..=16777216 | | Overrides the global value for this mock. |

### `mocks.<name>.scenarios`

Named scenarios, applied with `X-Mockingbird-Scenario: <name>`, by the mock's `default_scenario` when a request names none, or from the console.

| Key | Type | Default | Range / rule | Env / flag | Meaning |
| --- | --- | --- | --- | --- | --- |
| `mocks.<name>.scenarios` | map | `{}` | | | Scenarios by name. |
| `mocks.<name>.scenarios.<name>` | section | | | | One scenario. |
| `mocks.<name>.scenarios.<name>.instructions` | string | unset | | | Extra prompt instructions. |
| `mocks.<name>.scenarios.<name>.status` | integer | unset | A status the contract declares, or a 4xx or 5xx | | Answer with this status's response variant (`set` applies to it too); an undeclared 4xx or 5xx is answered natively (in the shape of a declared error of the same class, else problem+json; see [protocols](protocols.md)). Any other status must be declared by one of the scenario's `operations` (by any operation when it lists none), or the config is rejected, as the Admin API answers 422; an operation that does not declare such a status serves its own. |
| `mocks.<name>.scenarios.<name>.latency` | duration | unset | | | Added latency. |
| `mocks.<name>.scenarios.<name>.set` | JSON object | unset | | | Fields merged (JSON merge patch; `null` deletes one) into the response body. Each key present must fit the response schema the scenario selects (the `status` response, else the success response) of at least one of its operations, or the config is rejected, as the Admin API answers 422; absent keys and `required` are not checked. At request time a `set` whose patched body would not validate (or whose body is not an object) is dropped whole: the unpatched answer is served, with a WARN log and `mb_scenario_set_ignored_total{reason}` (`invalid`, `not_an_object`). |
| `mocks.<name>.scenarios.<name>.operations` | list | `[]` (all) | Operation ids | | Operations the scenario applies to. Each entry is an operation id, or `{id, instructions}`. |
| `mocks.<name>.scenarios.<name>.operations[].id` | string | | An operation id | | The operation of an `{id, instructions}` entry. |
| `mocks.<name>.scenarios.<name>.operations[].instructions` | string | unset | | | Instructions replacing the scenario's `instructions` for that operation. |
| `mocks.<name>.scenarios.<name>.stream_items` | integer | unset | | | Items per streamed response unless `X-Mockingbird-Stream-Items` overrides it. |
| `mocks.<name>.scenarios.<name>.stream_item_interval` | duration | unset | At most `30s` | | Pause between streamed items. |
| `mocks.<name>.scenarios.<name>.headers` | map | `{}` | Header names and visible-ASCII values; not `Content-Type`, `Content-Length`, `Transfer-Encoding`, `Connection` or `X-Mockingbird-*` | | Response headers to send while the scenario applies (HTTP and GraphQL over HTTP), such as `Retry-After: "30"` or `x-ratelimit-remaining: "0"`. They replace the headers the contract declares for the response, which are otherwise sent with generated values. |
| `mocks.<name>.scenarios.<name>.headers.<name>` | string | | | | One header's value. |
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
| `MB_LLM_BASE_URL`, `MB_LLM_MODEL`, `MB_LLM_API_KEY`, `MB_LLM_OUTPUT_MODE` (deprecated: `MB_LLM_JSON_SCHEMA`) | `--llm-timeout` | The `default` LLM profile. |
| `MB_STATE_DIR` | `--state-dir` | Where `mockingbird server start` keeps the background server's pid, log and URLs (default `.mockingbird`). Not a config file key. |
| `MB_STOP_FILE` | | Set by `mockingbird server start` on the server it spawns: `serve` shuts down gracefully once this file exists. `server stop` creates it (the graceful path on Windows, where a background server gets no Ctrl-C), then kills the server after 10 s and reports `forced: true`. |
| | `--offline` | Faker only, no LLM calls. |
| | `-c`, `--config` | The config file. |
| | `--watch` | Reconcile again when the file or a contract changes (a usage error on `data` nodes). |
| `MB_CHAOS_SEED` | | Seed of the chaos random choices. |
| `VAULT_ADDR`, `VAULT_TOKEN`, `VAULT_TOKEN_FILE`, `VAULT_NAMESPACE`, `VAULT_CACERT` | | Enable and configure `vault:` secret references. |
| `MB_ADMIN_URL`, `MB_ADMIN_TOKEN`, `MB_WORKSPACE` | `--admin-url`, `--token`, `--workspace` | The CLI client's Admin API, API key and workspace. Without `--admin-url` or `MB_ADMIN_URL` the client uses this project's running background server recorded in its state directory (`MB_STATE_DIR`, else `.mockingbird`), wherever it listens; else `server.admin_port` (and `server.host`) of `./mockingbird.yaml` (or of `doctor -c` / `proxy -c`), with `MB_ADMIN_PORT` and `MB_HOST` applied over it as `server start` does, else `http://127.0.0.1:9090`. |
| `OTEL_EXPORTER_OTLP_ENDPOINT`, `OTEL_EXPORTER_OTLP_TRACES_ENDPOINT` | | OTLP trace export (HTTP/protobuf; a `grpc` protocol is warned about). |
| `RUST_LOG` | | Log filter. |
