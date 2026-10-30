# Mockingbird LLM evals

Model `thinkingmachines/inkling-small@nojson` at `http://127.0.0.1:58282/r/thinkingmachines__inkling-small@nojson~pilot/api/v1`, started 2026-10-30T07:29:40.672778+00:00.

| Metric | Value |
| --- | --- |
| Calls | 10 |
| Schema-valid rate | 100.0% |
| First-pass rate | 60.0% |
| Repair rate | 40.0% |
| Fallback rate | 0.0% |
| p50 latency | 8949 ms |
| p95 latency | 33575 ms |
| Prompt tokens per call | 836.2 |
| Completion tokens per call | 175.0 |
| Streaming operations not evaluated | 3 |

## Per contract

| Contract | Calls | LLM | Repaired | Fallback | p95 ms |
| --- | --- | --- | --- | --- | --- |
| asyncapi/commerce-events-kafka | 1 | 0 | 1 | 0 | 18626 |
| asyncapi/orders-amqp-2.6 | 1 | 1 | 0 | 0 | 777 |
| graphql/devtools-github | 1 | 0 | 1 | 0 | 8949 |
| graphql/ecommerce-shopify-admin | 1 | 1 | 0 | 0 | 15217 |
| grpc/otlp-trace-service | 1 | 1 | 0 | 0 | 1100 |
| grpc/route-guide | 1 | 1 | 0 | 0 | 28321 |
| rest/payments-paypal-orders | 1 | 0 | 1 | 0 | 8880 |
| rest/payments-stripe | 1 | 0 | 1 | 0 | 33575 |
| soap/accounts-rpc | 1 | 1 | 0 | 0 | 1398 |
| soap/calculator | 1 | 1 | 0 | 0 | 31277 |

## Per API type

| API type | Calls | LLM | Repaired | Fallback | p95 ms |
| --- | --- | --- | --- | --- | --- |
| asyncapi | 2 | 1 | 1 | 0 | 18626 |
| graphql | 2 | 1 | 1 | 0 | 15217 |
| grpc | 2 | 2 | 0 | 0 | 28321 |
| rest | 2 | 0 | 2 | 0 | 33575 |
| soap | 2 | 2 | 0 | 0 | 31277 |

## Per domain

| Domain | Calls | LLM | Repaired | Fallback | p95 ms |
| --- | --- | --- | --- | --- | --- |
| devtools | 2 | 1 | 1 | 0 | 8949 |
| ecommerce | 3 | 2 | 1 | 0 | 18626 |
| generic | 3 | 3 | 0 | 0 | 31277 |
| payments | 2 | 0 | 2 | 0 | 33575 |
