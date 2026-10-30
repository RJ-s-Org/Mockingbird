# Mockingbird LLM evals

Model `thinkingmachines/inkling@nojson` at `http://127.0.0.1:58282/r/thinkingmachines__inkling@nojson~pilot/api/v1`, started 2026-10-30T07:30:20.927420+00:00.

| Metric | Value |
| --- | --- |
| Calls | 10 |
| Schema-valid rate | 100.0% |
| First-pass rate | 70.0% |
| Repair rate | 30.0% |
| Fallback rate | 0.0% |
| p50 latency | 1505 ms |
| p95 latency | 9611 ms |
| Prompt tokens per call | 685.1 |
| Completion tokens per call | 117.8 |
| Streaming operations not evaluated | 3 |

## Per contract

| Contract | Calls | LLM | Repaired | Fallback | p95 ms |
| --- | --- | --- | --- | --- | --- |
| asyncapi/commerce-events-kafka | 1 | 0 | 1 | 0 | 2122 |
| asyncapi/orders-amqp-2.6 | 1 | 1 | 0 | 0 | 1505 |
| graphql/devtools-github | 1 | 1 | 0 | 0 | 4247 |
| graphql/ecommerce-shopify-admin | 1 | 1 | 0 | 0 | 1452 |
| grpc/otlp-trace-service | 1 | 1 | 0 | 0 | 2813 |
| grpc/route-guide | 1 | 1 | 0 | 0 | 502 |
| rest/payments-paypal-orders | 1 | 0 | 1 | 0 | 9611 |
| rest/payments-stripe | 1 | 0 | 1 | 0 | 1408 |
| soap/accounts-rpc | 1 | 1 | 0 | 0 | 2690 |
| soap/calculator | 1 | 1 | 0 | 0 | 361 |

## Per API type

| API type | Calls | LLM | Repaired | Fallback | p95 ms |
| --- | --- | --- | --- | --- | --- |
| asyncapi | 2 | 1 | 1 | 0 | 2122 |
| graphql | 2 | 2 | 0 | 0 | 4247 |
| grpc | 2 | 2 | 0 | 0 | 2813 |
| rest | 2 | 0 | 2 | 0 | 9611 |
| soap | 2 | 2 | 0 | 0 | 2690 |

## Per domain

| Domain | Calls | LLM | Repaired | Fallback | p95 ms |
| --- | --- | --- | --- | --- | --- |
| devtools | 2 | 2 | 0 | 0 | 4247 |
| ecommerce | 3 | 2 | 1 | 0 | 2122 |
| generic | 3 | 3 | 0 | 0 | 2690 |
| payments | 2 | 0 | 2 | 0 | 9611 |
