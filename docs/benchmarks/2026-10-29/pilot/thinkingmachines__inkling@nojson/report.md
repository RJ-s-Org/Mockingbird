# Mockingbird LLM evals

Model `thinkingmachines/inkling@nojson` at `http://127.0.0.1:62332/r/thinkingmachines__inkling@nojson~pilot/api/v1`, started 2026-10-29T18:06:05.609137+00:00.

| Metric | Value |
| --- | --- |
| Calls | 10 |
| Schema-valid rate | 100.0% |
| First-pass rate | 70.0% |
| Repair rate | 30.0% |
| Fallback rate | 0.0% |
| p50 latency | 639 ms |
| p95 latency | 3474 ms |
| Prompt tokens per call | 929.4 |
| Completion tokens per call | 126.6 |

## Per contract

| Contract | Calls | LLM | Repaired | Fallback | p95 ms |
| --- | --- | --- | --- | --- | --- |
| asyncapi/commerce-events-kafka | 1 | 0 | 1 | 0 | 2034 |
| asyncapi/orders-amqp-2.6 | 1 | 1 | 0 | 0 | 453 |
| graphql/devtools-github | 1 | 1 | 0 | 0 | 2825 |
| graphql/ecommerce-shopify-admin | 1 | 1 | 0 | 0 | 984 |
| grpc/otlp-trace-service | 1 | 1 | 0 | 0 | 488 |
| grpc/route-guide | 1 | 1 | 0 | 0 | 470 |
| rest/payments-paypal-orders | 1 | 0 | 1 | 0 | 3474 |
| rest/payments-stripe | 1 | 0 | 1 | 0 | 1607 |
| soap/accounts-rpc | 1 | 1 | 0 | 0 | 639 |
| soap/calculator | 1 | 1 | 0 | 0 | 356 |

## Per API type

| API type | Calls | LLM | Repaired | Fallback | p95 ms |
| --- | --- | --- | --- | --- | --- |
| asyncapi | 2 | 1 | 1 | 0 | 2034 |
| graphql | 2 | 2 | 0 | 0 | 2825 |
| grpc | 2 | 2 | 0 | 0 | 488 |
| rest | 2 | 0 | 2 | 0 | 3474 |
| soap | 2 | 2 | 0 | 0 | 639 |

## Per domain

| Domain | Calls | LLM | Repaired | Fallback | p95 ms |
| --- | --- | --- | --- | --- | --- |
| devtools | 2 | 2 | 0 | 0 | 2825 |
| ecommerce | 3 | 2 | 1 | 0 | 2034 |
| generic | 3 | 3 | 0 | 0 | 639 |
| payments | 2 | 0 | 2 | 0 | 3474 |
