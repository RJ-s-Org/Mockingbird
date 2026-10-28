# Mockingbird

Mockingbird mocks the APIs your code calls (REST, GraphQL, gRPC, SOAP, server-sent events, WebSocket, Kafka, AMQP and MQTT) from their contracts, with an LLM writing answers that are realistic, schema-valid and consistent for the whole session. Your app does not change: point it at the proxy and its calls to `api.stripe.com` or `api.github.com` reach the mocks.

```sh
curl -fsSL https://raw.githubusercontent.com/rj-s-org/mockingbird/main/install.sh | sh
mockingbird serve --offline orders.yaml      # faker only, no key needed
```

## Where to start

| If you want to | Read |
| --- | --- |
| Get a mock running in a minute | [Quickstart](quickstart.md) |
| Understand mocks, sessions, sources and scenarios | [Concepts](concepts.md) |
| Run an app you cannot edit against mocks | [Proxy and `mockingbird run`](proxy.md) |
| Mock a gRPC, GraphQL, SOAP or event-driven API | [Protocols](protocols.md), [Events](events.md) |
| Mock an API that has docs but no OpenAPI | [Smart import](import.md) |
| Let a coding agent set it all up | [Coding agents and MCP](agents.md) |
| Use mocks in CI, Terraform, Kubernetes or Testcontainers | [Integrations](integrations.md) |
| Run it for a team | [Docker Compose](deploy/compose.md), [Kubernetes](deploy/kubernetes.md), [Running in production](operations/production.md) |
| Look up a config key, flag or endpoint | [Configuration](configuration.md), [Admin API](admin-api.md), [CLI](cli.md) |

Mockingbird is dual-licensed under MIT and Apache 2.0. The source, issues and releases are on [GitHub](https://github.com/RJ-s-Org/Mockingbird).
