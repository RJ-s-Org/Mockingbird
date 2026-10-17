# Docker Compose

[`deploy/compose/docker-compose.yml`](../../deploy/compose/docker-compose.yml) runs Mockingbird with Postgres and Rauthy (OIDC login), and adds HA and broker services through profiles. The stack's own [README](../../deploy/compose/README.md) has the full detail, including what to do when the Rauthy bootstrap fails.

> **Development only.** Every secret in the Compose file is a published dev value, and everything is plain HTTP. Do not expose the stack beyond your machine.

## Default stack

```sh
docker compose -f deploy/compose/docker-compose.yml up --build
```

| URL | What |
| --- | --- |
| http://localhost:9090 | Console and Admin API (log in through Rauthy) |
| http://localhost:8080 | Data plane |
| http://localhost:8081/auth/v1/admin | Rauthy admin UI |

Log in as `demo@localhost.test` / `Demo-Passw0rd-123!` (a sysadmin and `default` workspace admin). Mockingbird runs `--offline` here, so no LLM key is needed; to use an LLM, set `MB_LLM_API_KEY` on the `mockingbird` service and remove `--offline`.

## Profiles

| Profile | Adds | Reach it at |
| --- | --- | --- |
| `ha` | `postgres-ha`, `redis`, `mockingbird-control` (`--role control`), `mockingbird-data` (`--role data`) | console and Admin API http://localhost:19090, data plane http://localhost:18080, gRPC `localhost:15051` |
| `kafka` | a single-node KRaft Kafka | `localhost:9092` from the host, `kafka:19092` from containers |
| `amqp` | RabbitMQ (user and password `mockingbird`) | `amqp://localhost:5672`, management UI http://localhost:15672 |
| `mqtt` | Mosquitto (anonymous) | `mqtt://localhost:1883` |

Combine profiles freely:

```sh
docker compose -f deploy/compose/docker-compose.yml --profile ha up --build
docker compose -f deploy/compose/docker-compose.yml --profile kafka --profile amqp --profile mqtt up --build
```

### `ha`

The `ha` profile is the split-role layout of a Kubernetes install, on one machine. Both nodes read [`mockingbird-ha.yaml`](../../deploy/compose/mockingbird-ha.yaml): Postgres control storage on their own `postgres-ha`, Redis session state and the Redis bus. The control node owns the Admin API, console, reconcile and maintenance (as the leader); the data node serves the mocks and reloads when the control node announces a change on the bus.

Try it:

```sh
# Create a mock and upload a contract through the control node...
curl -fsS -XPOST localhost:19090/api/v1/mocks \
  -H 'content-type: application/json' -d '{"name":"ha"}'
curl -fsS -XPOST localhost:19090/api/v1/mocks/ha/contracts \
  -H 'content-type: application/yaml' --data-binary @fixtures/openapi/orders-3.0.yaml
# ...and call it on the data node (it reloads within a second or two).
curl -i localhost:18080/ha/orders/o1
# Readiness shows the node's role and whether it leads.
curl -s localhost:19090/readyz
```

CI runs this path on every change, then stops `mockingbird-control` and checks that the data node keeps serving.

Scale the data plane with `--scale mockingbird-data=2` after removing its published ports. The HA nodes run without login and offline.

### Broker profiles

Point a `brokers:` entry of your `mockingbird.yaml` at a profile's service. From the default `mockingbird` container:

```yaml
brokers:
  kafka:
    kind: kafka
    bootstrap_servers: kafka:19092
  rabbit:
    kind: amqp
    url: amqp://mockingbird@rabbitmq:5672/%2f
    sasl: {mechanism: PLAIN, username: mockingbird, password: env:AMQP_PASSWORD}
  mosquitto:
    kind: mqtt
    url: mqtt://mosquitto:1883
```

The broker clients are in the full image only; the slim image (`-slim`) has none.

## Resetting

```sh
docker compose -f deploy/compose/docker-compose.yml down -v
```

removes the containers and volumes; add the same `--profile` flags to remove the profiles' services too.
