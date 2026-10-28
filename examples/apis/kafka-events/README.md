# Kafka commerce events (AsyncAPI)

An event-driven example: the order, payment and shipment events of "Juniper & Co", a
small plant shop, described in AsyncAPI 3.0 with Kafka bindings and produced by
Mockingbird. Use it to develop and test consumers before the real producers exist.

| | |
| --- | --- |
| Contract | [`commerce-events.asyncapi.yaml`](commerce-events.asyncapi.yaml), hand-written for this example |
| Upstream | none (a fictional but typical event design) |
| Channels | `orderCreated` (`juniper.orders.order-created.v1`, every 10 s and on demand), `paymentAuthorized` and `shipmentDispatched` (on demand) |
| Kafka bindings | topic names and partitions, the order id as the record key, `ce_type`/`ce_id`/`correlation_id` record headers |
| Broker | `memory` (in-process) by default; a real Kafka cluster with a `kafka` build |
| Scenarios | none: scenarios apply to request/response calls, not to produced events |

## Quickstart

```bash
mockingbird examples init kafka-events --dir kafka-events && cd kafka-events   # or: cd examples/apis/kafka-events
mockingbird serve -c mockingbird.yaml --offline
```

In another shell:

```bash
mockingbird channels list commerce
mockingbird channels publish commerce orderCreated --count 3
mockingbird channels publish commerce shipmentDispatched
```

`mockingbird channels list commerce` shows each channel's `published` count and
`last_published_at`, which the 10 s schedule of `orderCreated` keeps moving.

Drop `--offline` (and configure an LLM, see [the configuration reference](../../../docs/configuration.md#llm))
for events that follow the persona: real plant names and prices, totals that add up,
Pacific Northwest addresses. `--context '{"order_id": "ord_..."}'` hands an existing
order to the prompt, so a `paymentAuthorized` can follow a given `orderCreated`.

## Expected output

`--offline` messages come from the schema faker: ids, SKUs, tracking numbers and enums
follow the contract's patterns, the free text is random.

```json
{
  "messages": [
    {
      "address": "juniper.orders.order-created.v1",
      "headers": {
        "ce_type": "com.juniper.order.created",
        "correlation_id": "b732c75a-07d7-43f3-b4ed-c44d3f88a07e",
        "x-mockingbird-mock": "commerce",
        "x-mockingbird-source": "fallback",
        ...
      },
      "key": "ord_FH3M8P9MJ065H679P7HB6E8JCN",
      "payload": {
        "channel": "web",
        "customer_id": "cus_35RQNGFM9DGWRGG5QRARHH9QS4",
        "items": [{"name": "quartz", "quantity": 13, "sku": "PLT-10947", "unit_price": {"amount": 15, "currency": "USD"}}],
        "order_id": "ord_PKNHPFD7DVQ4VGEX4B4KQ1KGNP",
        "shipping_address": {"city": "lumen", "country": "US", "postal_code": "77685", "region": "NE", ...},
        "total": {"amount": 71, "currency": "USD"},
        ...
      },
      "source": "fallback"
    }
  ]
}
```

## A real Kafka broker

The `kafka` broker kind is not in the default build (the default has `memory`, `amqp`
and `mqtt`); with it the server refuses to start with
`this build has no kafka support (use the full image or build with --features kafka)`.
Use the full image, or build with `cargo build --release --features kafka`, then point
the mock at your cluster:

```yaml
brokers:
  kafka:
    kind: kafka
    bootstrap_servers: localhost:9092
    # tls: true
    # sasl: {mechanism: SCRAM-SHA-256, username: commerce, password: env:KAFKA_PASSWORD}
mocks:
  commerce:
    broker: kafka
    ...
```

Records go to the topics the bindings name, keyed by order id, with the headers above.
Any Kafka client then consumes them, for example `confluent-kafka` in Python:

```bash
pip install confluent-kafka
python - <<'PY'
import json
from confluent_kafka import Consumer
c = Consumer({"bootstrap.servers": "localhost:9092", "group.id": "orders-demo", "auto.offset.reset": "earliest"})
c.subscribe(["juniper.orders.order-created.v1"])
msg = c.poll(15)
print(msg.key().decode(), dict(msg.headers())["ce_type"].decode(), json.loads(msg.value())["total"])
c.close()
PY
```

The broker settings (`tls`, `ca_file`, `sasl`, `client_id_prefix`) are in
[the configuration reference](../../../docs/configuration.md#brokers).
