# Protocols

The importer detects a contract's format from its bytes (the file name does not matter), and the mock is served by the adapter for that protocol family:

| Format | Detected by | Served |
| --- | --- | --- |
| OpenAPI 3.x, Swagger 2.0 (JSON or YAML) | an `openapi: 3.*` or `swagger: "2.0"` key | REST at the route prefix, at each operation's path and also under the contract's [base paths](#base-paths). Operations whose success response is `text/event-stream` stream server-sent events (SSE). |
| GraphQL SDL | SDL definitions (`type Query`, `schema {`, …) | GraphQL at `<prefix>/graphql` (`protocols.graphql.path`, or `mocks.<name>.graphql_path` for one mock, e.g. Shopify's `/graphql.json`): queries over `GET` or `POST`, mutations over `POST`, subscriptions over a WebSocket upgrade of the same path (`graphql-transport-ws` or the legacy `graphql-ws`). |
| GraphQL introspection JSON | a `__schema` or `data.__schema` key | As GraphQL SDL; no conversion needed (Shopify publishes its Storefront schema this way, as `storefront.schema.json` in `@shopify/hydrogen-react`). |
| protobuf `.proto` | a `syntax = "proto…"` statement, or `service` and `message` definitions | gRPC on `:50051` (all four call kinds), with server reflection and the standard health service. Requests may be gzip-compressed (`grpc-encoding: gzip`), and responses are gzipped for clients that accept it (`grpc-accept-encoding`); other encodings are refused with `UNIMPLEMENTED`. The route prefix is ignored. |
| protobuf `FileDescriptorSet` | a binary descriptor set (`protoc --descriptor_set_out`) | As `.proto`. |
| AsyncAPI 2.0–2.6, 3.0 (JSON or YAML) | an `asyncapi: 2.*` or `asyncapi: 3.0.*` key | Event channels: generated messages published to Kafka, AMQP or MQTT and consumed from them, or WebSocket channels at `<prefix><address>`. See [Events](events.md). |
| WSDL 1.1 (with inline or bundled XSD) | an XML `definitions` root in the WSDL 1.1 namespace | SOAP 1.1 and 1.2 at `<prefix><address path>`, the path of each service's `soap:address`; `GET <prefix><address path>?wsdl` returns the WSDL with its addresses pointing at the mock, and the `schemaLocation` of every XSD uploaded with it pointing at `GET <prefix><address path>?xsd=<upload path>`, which serves that XSD (its own `schemaLocation`s rewritten the same way); followed `wsdl:import` locations point there too. |
| zip or tar.gz archive | the zip or gzip magic bytes | The contract inside: all `.proto` files compiled together, else the `.wsdl` that holds `wsdl:service` with the WSDLs and XSDs it imports, else the `.graphql`/`.graphqls`/`.gql` files joined as one schema, else the one file that is a contract (documentation, images, `README` and `LICENSE` files beside it are ignored). See [Archive uploads](cli.md#archive-uploads). |

Operation ids, as scenarios' `operations` lists and the request log use them, are `Query.<field>` / `Mutation.<field>` / `Subscription.<field>` for GraphQL, `<Service>.<Method>` for gRPC (`<package>.<Service>.<Method>` when the short form collides), and `<operation>` for SOAP (`<Service>.<operation>` on collision). Each gRPC service is served by one mock across every workspace (the gRPC listener is shared): uploading with activation, or activating, a contract whose services another mock's active contract serves is a 409, and a config file declaring such a clash is rejected. If a clash reaches the data plane anyway, the mock already serving the service keeps it and the newcomer is reported in `/readyz` as failed. gRPC reflection lists only the services the caller's data-plane key reaches (plus mocks without data-plane auth) and needs a valid key when any gRPC mock is in `api_key` mode.

## Base paths

A REST mock serves each operation at its contract path and also under every base path the contract declares, so a client configured with the server URL works without a `path_rewrite`. With `servers: [{url: https://api.example.com/v1}]` and the route prefix `/orders`, `GET /orders/v1/orders/42` and `GET /orders/orders/42` both reach `GET /orders/{id}`. Unprefixed paths keep working, so mocks of contracts that declare base paths do not change routes.

- Swagger 2.0: `basePath`.
- OpenAPI 3.x: the path of each top-level `servers[].url` (`https://api.example.com/v1` gives `/v1`, `/beta` gives `/beta`; a URL without a path, such as `https://api.stripe.com`, gives none). Several servers with different paths are all served, in declaration order, duplicates once.
- Server variables are replaced by their `default` (`https://{region}.example.com/{version}` with `version` defaulting to `v2` gives `/v2`). The other values of a variable's `enum` are not served; declare them as servers of their own if clients use them. A server whose URL names a variable without a default is skipped.
- A relative URL that does not start with `/` (`v1`, `./v1`) is relative to wherever the document is served and is skipped, as are the root (`/`), the query and the fragment.
- Path-item and operation `servers` are not read. For another layout, such as a gateway that adds a segment the contract does not declare, use `mocks.<name>.path_rewrite` ([configuration](configuration.md)).
- A contract path always wins over a prefixed one: with base path `/v1` and contract paths `/orders` and `/v1/orders`, `GET /v1/orders` is the `/v1/orders` operation. When two prefixed routes collide, the first base path keeps the route.

Base paths are read from the stored contract source when the mock is loaded, so contracts uploaded before this release gain them on the next reload.

## Response media types

A REST response is served in the media type the contract declares. JSON (`application/json`, any `+json`) is generated as usual. A response that declares only other media types is answered in the first of them:

| Declared media type | Answer |
| --- | --- |
| any, with an example | The example as written: the media type object's `example`, else the `value` of its first `examples` entry, else the schema's `example` (Swagger 2.0: the response's `examples` entry for the type). |
| `text/*` (`text/plain`, `text/csv`, …) | A faker value for the schema, served as text; a string when the media type declares no schema. |
| XML (`application/xml`, `text/xml`, `+xml`) | Generated as JSON for the schema (by the LLM like any other body), then rendered as XML by the SOAP XML mapper. The root element is the schema's `xml.name` (OpenAPI 3.1), else the name of the schema it references (`Pet` for `$ref: '#/components/schemas/Pet'`), else `response`; members become child elements in name order. |
| anything else (`application/octet-stream`, images, PDF) | Only a declared example; without one the response has no body, and the import warns with `response_body_omitted`. |

Text and example answers never call the LLM (its JSON-mode answer could not be that media type); they carry `X-Mockingbird-Source: fallback` and a fallback reason that says so. Scenario `set` patches do not apply to them. The `Content-Type` is the declared one whenever the body is served as text; a body that is not text (a text schema of `type: object`, an XML mapper failure, a build without the `soap` feature) is served as JSON labelled `application/json`, as before. Contract versions uploaded before this release keep their empty non-JSON responses until they are imported again; contracts from the config file are compiled afresh at startup.

## Import warnings

Compiling a contract never fails over what the mock cannot represent exactly; it relaxes or skips it and says so. `mockingbird validate` prints each warning, and uploads, the version list, activation and approval return them (`warnings: [{code, operation, message}]`). They never block activation.

| Code | Meaning |
| --- | --- |
| `status_range_expanded` | An OpenAPI response range (`2XX` to `5XX`) stands for the codes of its class the operation does not declare itself: `2XX` for `200` (only when no explicit 2xx is declared), `3XX` for 301, 302, 303, 304, 307 and 308, `4XX` for 400, 401, 403, 404, 405, 409, 410, 415, 422 and 429, `5XX` for 500 to 504. Explicit codes win; `1XX` is ignored. So scenarios and `X-Mockingbird-Status` can force those codes. |
| `dangling_ref` | A `$ref` names a component schema (`#/components/schemas/X`, `#/definitions/X`) the document does not define; it is stubbed as an open schema that accepts any value. A `$ref` to another file or another kind of component is still an error. |
| `cookie_param_ignored` | The operation declares cookie parameters; the mock neither validates nor requires them. |
| `invalid_resource_extension` | A value of an operation's [`x-mockingbird-resource`](import.md#correcting-it-x-mockingbird-resource) is not valid; it is ignored and inference stands. |
| `response_header_unresolved` | A response header's `$ref` does not resolve; the header is left out. |
| `security_scheme_unsupported` | A security requirement uses a scheme the mock cannot check (see [the console guide](console.md)). |
| `schema_too_large` | A response schema is over the 64 KiB prompt budget; the prompt shows a trimmed schema. |
| `xsd_wildcard_ignored` | An XSD uses `xs:any` or `xs:anyAttribute`; the elements and attributes they allow are never generated. |
| `response_body_omitted` | A response declares only a binary media type (`application/octet-stream`, …) and no example, so it is served without a body (see [response media types](#response-media-types)). |
| `wsdl_import_ignored` | A `wsdl:import` names a document that is not in the upload (a URL, say), so it is not followed; only the uploaded definitions are served. A document that needs the missing definitions fails instead, with an error naming the import. Imports of uploaded files are followed (see [Known limits](#known-limits)). |

OpenAPI 3.0 documents may declare webhooks under `x-webhooks` (GitHub's spec does); they are read as 3.1 `webhooks` are.

## Examples

The examples serve one contract each, so it is mounted at `/`; with a config file, put the mock's route prefix in front of each path.

GraphQL (`mockingbird serve --offline starwars.graphql`):

```sh
curl -s localhost:8080/graphql \
  -H 'content-type: application/json' \
  -d '{"query":"{ hero(episode: JEDI) { name } }"}'
```

The sources of the root fields are reported in the response, under `extensions.mockingbird.sources`.

### Batched GraphQL requests

A `POST` whose JSON body is an array of operations is a batch, answered `200` with an array of responses in the same order:

```sh
curl -s localhost:8080/graphql \
  -H 'content-type: application/json' \
  -d '[{"query":"{ hero(episode: JEDI) { name } }"},
       {"query":"query($e: Episode) { hero(episode: $e) { name } }","variables":{"e":"EMPIRE"}}]'
```

A batch holds at most 10 operations; a larger or empty one is a `400` problem and nothing runs. The operations run one after another (a query after a mutation sees its write), each as if it had been sent on its own: checked against the root-field, depth and complexity limits, admitted (tenant, quota and, in `api_key` mode, the data-plane key), given its own chaos decision and recorded as its own request-log entry. An operation that is refused answers its own entry, `{"errors": [{"message", "extensions": {"code", "status"}}]}` (with `retryAfter` in seconds for a `429`), and the others still run. The response's `x-mockingbird-source` is the worst source over the batch, and the largest injected latency is waited once. The 1 MiB body limit applies to the whole batch.

gRPC (`mockingbird serve --offline helloworld.proto`):

```sh
grpcurl -plaintext 127.0.0.1:50051 list
grpcurl -plaintext -d '{"name":"world"}' \
  -H 'x-mockingbird-session: alice' \
  127.0.0.1:50051 helloworld.Greeter/SayHello
```

SOAP 1.1 (`mockingbird serve --offline calculator.wsdl`, whose address is `http://www.example.com/calculator.asmx`):

```sh
curl -s localhost:8080/calculator.asmx \
  -H 'content-type: text/xml; charset=utf-8' \
  -H 'SOAPAction: "http://tempuri.org/Add"' \
  -d '<soap:Envelope xmlns:soap="http://schemas.xmlsoap.org/soap/envelope/">
        <soap:Body><Add xmlns="http://tempuri.org/"><intA>1</intA><intB>2</intB></Add></soap:Body>
      </soap:Envelope>'
curl -s 'localhost:8080/calculator.asmx?wsdl'
```

SOAP 1.2 sends `content-type: application/soap+xml; action="…"` instead of `SOAPAction`. Every failure is a SOAP Fault: `Client`/`Sender` for a 4xx status, `Server`/`Receiver` otherwise.

SSE (an OpenAPI operation answering `text/event-stream`):

```sh
curl -N localhost:8080/ticks -H 'X-Mockingbird-Stream-Items: 3'
```

## Streaming

SSE responses, GraphQL subscriptions and gRPC server- and bidi-streaming calls are generated as a stream of items, each validated as it completes (repaired once, or replaced by faker output). The number of items comes from, in order:

1. the `X-Mockingbird-Stream-Items` header (gRPC metadata `x-mockingbird-stream-items`; an integer from 1 to 10000);
2. the selected scenario's `stream_items`;
3. `protocols.stream.default_items` (5).

The count is capped at `protocols.stream.max_items` (50). A scenario's `stream_item_interval`, or else `protocols.stream.item_interval` (0), is the pause between items.

SSE streams and GraphQL subscriptions stay open after the generated items: they go on to deliver resources of the operation's type that are created or changed in the same session, until the client disconnects. A gRPC server stream completes after its last item. Generated stream items are never written to session state.

## Override mapping per protocol

The override headers work on every protocol: gRPC reads them as lowercased metadata (`x-mockingbird-session`, `x-mockingbird-status`, …), and GraphQL reads them from the HTTP request (or from the `connection_init` payload of a subscription). A forced status is reported natively.

REST answers a forced status with the operation's declared response for it (or its `default` response). A 5xx the operation does not declare is answered as a plain `application/problem+json` document with that status, since any API can fail that way; any other undeclared status is a `400 Undeclared status override`, and a value that is not a status code from 100 to 599 is a `400 Invalid override`.

gRPC answers with the status code standing for the HTTP status:

| HTTP status | gRPC status |
| --- | --- |
| 400, and any 4xx not listed | `INVALID_ARGUMENT` |
| 401 | `UNAUTHENTICATED` |
| 403 | `PERMISSION_DENIED` |
| 404 | `NOT_FOUND` |
| 409 | `ALREADY_EXISTS` |
| 412 | `FAILED_PRECONDITION` |
| 416 | `OUT_OF_RANGE` |
| 429 | `RESOURCE_EXHAUSTED` |
| 499 | `CANCELLED` |
| 501 | `UNIMPLEMENTED` |
| 503 | `UNAVAILABLE` |
| 504 | `DEADLINE_EXCEEDED` |
| any other | `INTERNAL` |

GraphQL answers `200` with a field error whose `extensions.code` stands for the status (and `extensions.status` holds the status itself):

| HTTP status | `extensions.code` |
| --- | --- |
| 400 | `BAD_USER_INPUT` |
| 401 | `UNAUTHENTICATED` |
| 403 | `FORBIDDEN` |
| 404 | `NOT_FOUND` |
| 409 | `CONFLICT` |
| 429 | `RATE_LIMITED` |
| any other 4xx | `BAD_REQUEST` |
| 5xx | `INTERNAL_SERVER_ERROR` |

Request limits: HTTP/REST requests at most 1,000 query pairs and 1,000 form or multipart fields (more is a 400 problem); GraphQL bodies 1 MiB, query depth 15 and complexity 1000 (`protocols.graphql`); gRPC messages 4 MiB (`protocols.grpc.max_message_bytes`); SOAP bodies 4 MiB.

## Known limits

- GraphQL directives the schema declares for query locations (`directive @inContext(country: CountryCode) on QUERY`, as Shopify's Storefront schema has; from SDL or from an introspection result's `directives`) are accepted wherever they appear in a query and ignored: the answer is generated as if they were absent. A variable that only such directives use is accepted too. A directive the schema does not declare, or declares only for type-system locations (`on OBJECT`, `on FIELD_DEFINITION`, …), fails validation with `Unknown directive`. `@skip` and `@include` work as usual.
- OpenAPI, Swagger and AsyncAPI `$ref`s to other files are resolved only against files uploaded together with the contract (an archive, or a directory given to the CLI; see [archive uploads](cli.md#archive-uploads)). Refs to URLs are never fetched: bundle such a spec into one file first.
- WSDL 2.0 is not supported; only WSDL 1.1 with SOAP 1.1/1.2 bindings is.
- External XSDs are never fetched: a `schemaLocation` must name a file uploaded with the WSDL in the same archive. Contracts imported before this release keep serving their `?wsdl` with the original `schemaLocation`s until they are imported again.
- `wsdl:import` is followed only within the upload: upload the WSDL files together in one archive. The file that holds `wsdl:service` is the root (when several do, the one no other `.wsdl` imports); its services are served, with the messages, portTypes, bindings and types of every document it reaches through `wsdl:import` (a WSDL or an XSD), resolved relative to the importing file. Each document is read once, so import cycles are fine; imports nest at most 16 deep. Definitions are matched by namespace and name, falling back to the name alone. A location not in the upload (a URL) is never fetched: see `wsdl_import_ignored` above. `?wsdl` points each followed `wsdl:import` at `?xsd=<upload path>`, which serves that document.
- Complex XSD `pattern` facets may yield faker output that does not validate against them; Mockingbird logs a warning when that happens.
- Token usage of streamed generations is not metered.
