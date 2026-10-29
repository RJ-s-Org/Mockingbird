# Mockingbird LLM evals

Model `thinkingmachines/inkling-small@nojson` at `http://127.0.0.1:62332/r/thinkingmachines__inkling-small@nojson~pilot/api/v1`, started 2026-10-29T18:06:39.833785+00:00.

| Metric | Value |
| --- | --- |
| Calls | 10 |
| Schema-valid rate | 90.0% |
| First-pass rate | 60.0% |
| Repair rate | 30.0% |
| Fallback rate | 10.0% |
| p50 latency | 705 ms |
| p95 latency | 8734 ms |
| Prompt tokens per call | 1390.8 |
| Completion tokens per call | 340.2 |

## Per contract

| Contract | Calls | LLM | Repaired | Fallback | p95 ms |
| --- | --- | --- | --- | --- | --- |
| asyncapi/commerce-events-kafka | 1 | 0 | 1 | 0 | 1537 |
| asyncapi/orders-amqp-2.6 | 1 | 1 | 0 | 0 | 442 |
| graphql/devtools-github | 1 | 0 | 0 | 1 | 8734 |
| graphql/ecommerce-shopify-admin | 1 | 1 | 0 | 0 | 1483 |
| grpc/otlp-trace-service | 1 | 1 | 0 | 0 | 592 |
| grpc/route-guide | 1 | 1 | 0 | 0 | 501 |
| rest/payments-paypal-orders | 1 | 0 | 1 | 0 | 2052 |
| rest/payments-stripe | 1 | 0 | 1 | 0 | 1191 |
| soap/accounts-rpc | 1 | 1 | 0 | 0 | 705 |
| soap/calculator | 1 | 1 | 0 | 0 | 483 |

## Per API type

| API type | Calls | LLM | Repaired | Fallback | p95 ms |
| --- | --- | --- | --- | --- | --- |
| asyncapi | 2 | 1 | 1 | 0 | 1537 |
| graphql | 2 | 1 | 0 | 1 | 8734 |
| grpc | 2 | 2 | 0 | 0 | 592 |
| rest | 2 | 0 | 2 | 0 | 2052 |
| soap | 2 | 2 | 0 | 0 | 705 |

## Per domain

| Domain | Calls | LLM | Repaired | Fallback | p95 ms |
| --- | --- | --- | --- | --- | --- |
| devtools | 2 | 1 | 0 | 1 | 8734 |
| ecommerce | 3 | 2 | 1 | 0 | 1537 |
| generic | 3 | 3 | 0 | 0 | 705 |
| payments | 2 | 0 | 2 | 0 | 2052 |
