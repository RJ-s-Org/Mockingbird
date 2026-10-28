# Admin API

The full, interactive reference is [rendered from the OpenAPI document](api/index.html) that `mockingbird openapi` prints and the admin port serves at `/api/openapi.json`. This page covers what the reference does not: the resources at a glance, the request log, and how the admin port protects itself.

The Admin API lives under `http://127.0.0.1:9090/api/v1`; its OpenAPI document is served at [`/api/openapi.json`](http://127.0.0.1:9090/api/openapi.json). Every error is an RFC 9457 `application/problem+json` document.

| Resource | Endpoints |
| --- | --- |
| Mocks | `GET/POST /mocks`, `GET/PATCH/DELETE /mocks/{mock}` |
| Contracts | `GET/POST /mocks/{mock}/contracts`, `GET …/contracts/{version}`, `GET …/{version}/source` (an `application/octet-stream` attachment; the sniffed type is in `X-Mockingbird-Source-Type`), `POST …/{version}/activate` |
| Scenarios | `GET /mocks/{mock}/scenarios`, `PUT/DELETE …/scenarios/{name}` |
| LLM profiles | `GET /llm-profiles`, `GET/PUT/DELETE /llm-profiles/{name}` |
| Sessions | `GET /mocks/{mock}/sessions`, `GET/PUT/DELETE …/sessions/{session}` (export, seed, reset) |
| Request log | `GET /mocks/{mock}/requests` |
| Imports | `GET/POST /mocks/{mock}/imports`, `GET/DELETE …/imports/{job}` (see [Smart import](import.md)) |
| Drafts | `GET/PUT …/contracts/{version}/draft`, `POST …/{version}/approve` (activates unless `{"activate": false}`), `POST …/{version}/preview` |
| Channels and brokers | `GET /mocks/{mock}/channels`, `GET/PUT …/channels/{channel}`, `POST …/{channel}/publish`, `GET/PUT /mocks/{mock}/event-settings`, `GET /brokers` (see [Events](events.md)) |

The request log keeps the newest `observability.request_log_retention` calls per mock. Keys matching `observability.redact_fields` are redacted anywhere in recorded JSON bodies, form bodies and query strings: a key matches when, lowercased and stripped of `_`, `-` and `.`, it contains a field normalized the same way (so `token` also redacts `access_token` and `refresh_token`, and `apiKey` redacts `x-api-key`; it errs towards hiding, e.g. `tokenizer_version`); multipart bodies are not recorded, a request body over 64 KiB is replaced by `{"truncated": true, "bytes": N}`, and a response body over 64 KiB by `{"truncated": true, "bytes": N, "preview": "…"}` (the first 1 KiB of the redacted JSON). Redaction and truncation run on the background writer, never on the request path. Queued entries are written before a graceful shutdown completes (for up to 5 s).

Changes take effect on the data plane without a restart: activating a contract version or editing a mock swaps its runtime in place.

If a change breaks a mock (for example an LLM profile it uses now names a secret that is not set), the mock keeps serving its previous version, and `/readyz` reports `degraded` with the reason `mock <name>: <error>` until it is fixed. `PUT` and `DELETE` of an LLM profile still succeed in that case, and list the affected mocks in `warnings`. Two mocks may not share a route prefix: creating or moving a mock onto another mock's exact prefix is a 409.

## Admin port safety

Besides authentication (see [Console and auth](console.md)), the admin port defends itself against browsers and pasted configs:

- **Host check.** While the admin port is bound to a loopback address (the default), it answers only requests whose `Host` is `localhost`, `127.0.0.1` or `[::1]` (any port); in `oidc` mode it answers only to the host of `auth.external_url`. Anything else gets `421 Misdirected Request`, except the ops endpoints. This blocks DNS-rebinding attacks from web pages.
- **Content types.** Every mutating request needs `Content-Type: application/json` (activation too, whose body is ignored), and a contract upload needs `application/json`, `application/yaml`, `application/x-yaml`, `text/yaml`, `application/vnd.oai.openapi` (`+json`/`+yaml`), `application/graphql`, `text/x-protobuf`, `application/x-protobuf`, `application/xml`, `text/xml`, `application/wsdl+xml`, `application/zip`, `application/gzip` or `application/octet-stream`; anything else is a `415`. A cross-site form or no-cors request cannot send these, so a web page cannot change mocks behind your back. The CLI client sets them for you; with curl, pass `-H 'content-type: application/json'` (or `application/yaml` for a YAML contract).
- **Secret references.** An LLM profile created or updated through the API may only use secret references matching `server.api_secret_refs` (globs; default `["env:MB_LLM_*", "env:*_API_KEY"]`); `file:` references and other variables are rejected with a `422 Secret reference not allowed` whose detail lists the accepted forms (for the defaults: `secret_ref not allowed by server.api_secret_refs; accepted: env:MB_LLM_* or env:*_API_KEY (* matches any characters)`). Profiles in the config file are not restricted. Only a sysadmin may set or change a profile's `secret_ref`, or change the `base_url` of a profile that has one (`403` otherwise): a workspace admin can still create profiles without a secret and change a profile's other fields, but cannot point a secret at a host of their choosing. Widen the list in `mockingbird.yaml` if you need to:

  ```yaml
  server:
    api_secret_refs: ["env:MB_LLM_*", "env:*_API_KEY", "file:/run/secrets/*"]
  ```
