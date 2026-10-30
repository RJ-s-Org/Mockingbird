# Mockingbird LLM evals

Model `nvidia/nemotron-3.5-lightning` at `http://127.0.0.1:58282/r/nvidia__nemotron-3.5-lightning~pilot/api/v1`, started 2026-10-30T07:28:12.659564+00:00.

| Metric | Value |
| --- | --- |
| Calls | 10 |
| Schema-valid rate | 90.0% |
| First-pass rate | 70.0% |
| Repair rate | 20.0% |
| Fallback rate | 10.0% |
| p50 latency | 1382 ms |
| p95 latency | 6065 ms |
| Prompt tokens per call | 1117.1 |
| Completion tokens per call | 247.5 |
| Streaming operations not evaluated | 3 |

## Per contract

| Contract | Calls | LLM | Repaired | Fallback | p95 ms |
| --- | --- | --- | --- | --- | --- |
| asyncapi/commerce-events-kafka | 1 | 0 | 1 | 0 | 2280 |
| asyncapi/orders-amqp-2.6 | 1 | 1 | 0 | 0 | 720 |
| graphql/devtools-github | 1 | 1 | 0 | 0 | 1382 |
| graphql/ecommerce-shopify-admin | 1 | 1 | 0 | 0 | 2532 |
| grpc/otlp-trace-service | 1 | 1 | 0 | 0 | 1911 |
| grpc/route-guide | 1 | 1 | 0 | 0 | 396 |
| rest/payments-paypal-orders | 1 | 0 | 0 | 1 | 6065 |
| rest/payments-stripe | 1 | 0 | 1 | 0 | 3177 |
| soap/accounts-rpc | 1 | 1 | 0 | 0 | 509 |
| soap/calculator | 1 | 1 | 0 | 0 | 1103 |

## Per API type

| API type | Calls | LLM | Repaired | Fallback | p95 ms |
| --- | --- | --- | --- | --- | --- |
| asyncapi | 2 | 1 | 1 | 0 | 2280 |
| graphql | 2 | 2 | 0 | 0 | 2532 |
| grpc | 2 | 2 | 0 | 0 | 1911 |
| rest | 2 | 0 | 1 | 1 | 6065 |
| soap | 2 | 2 | 0 | 0 | 1103 |

## Per domain

| Domain | Calls | LLM | Repaired | Fallback | p95 ms |
| --- | --- | --- | --- | --- | --- |
| devtools | 2 | 2 | 0 | 0 | 1911 |
| ecommerce | 3 | 2 | 1 | 0 | 2532 |
| generic | 3 | 3 | 0 | 0 | 1103 |
| payments | 2 | 0 | 1 | 1 | 6065 |
