# Example gallery

Ready-to-run configurations, one per protocol family. Each `mockingbird.yaml` serves
contracts from the repository's [`fixtures/`](../../fixtures) directory, so run the
commands from the repository root. `--offline` serves schema-faker output without an
LLM; drop it and set `MB_LLM_API_KEY` (or an `llm:` section) for generated responses.

| Example | Shows | Run |
| --- | --- | --- |
| [`rest`](rest/mockingbird.yaml) | OpenAPI 3.0 order API with session state, a persona and scenarios | `mockingbird serve -c examples/gallery/rest/mockingbird.yaml --offline` |
| [`graphql`](graphql/mockingbird.yaml) | Star Wars GraphQL schema: queries, mutations, subscriptions | `mockingbird serve -c examples/gallery/graphql/mockingbird.yaml --offline` |
| [`grpc`](grpc/mockingbird.yaml) | RouteGuide gRPC service: unary, server, client and bidi streaming, with reflection | `mockingbird serve -c examples/gallery/grpc/mockingbird.yaml --offline` |
| [`soap`](soap/mockingbird.yaml) | WSDL 1.1 calculator over SOAP 1.1 and 1.2, with `?wsdl` | `mockingbird serve -c examples/gallery/soap/mockingbird.yaml --offline` |
| [`events`](events/mockingbird.yaml) | AsyncAPI order events on an in-process broker, and a WebSocket chat channel | `mockingbird serve -c examples/gallery/events/mockingbird.yaml --offline` |
| [`chaos`](chaos/README.md) | Injected errors, latency jitter and dropped streams (configuration added with the chaos keys) | `mockingbird serve -c examples/gallery/chaos/mockingbird.yaml --offline` |

Each file starts with a comment showing a request to try. The Admin API and console are
on `http://127.0.0.1:9090`, the data plane on `http://127.0.0.1:8080` and gRPC on
`127.0.0.1:50051`.
