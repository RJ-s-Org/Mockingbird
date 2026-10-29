# Mockingbird LLM evals

Model `reflection/beam-1.0-501b-a23b` at `http://127.0.0.1:58262/r/reflection__beam-1.0-501b-a23b~pilot/api/v1`, started 2026-10-29T19:33:26.398809+00:00.

| Metric | Value |
| --- | --- |
| Calls | 10 |
| Schema-valid rate | 90.0% |
| First-pass rate | 90.0% |
| Repair rate | 0.0% |
| Fallback rate | 10.0% |
| p50 latency | 2217 ms |
| p95 latency | 10096 ms |
| Prompt tokens per call | 900.4 |
| Completion tokens per call | 805.4 |

## Per contract

| Contract | Calls | LLM | Repaired | Fallback | p95 ms |
| --- | --- | --- | --- | --- | --- |
| asyncapi/commerce-events-kafka | 1 | 1 | 0 | 0 | 4422 |
| asyncapi/orders-amqp-2.6 | 1 | 1 | 0 | 0 | 1182 |
| graphql/devtools-github | 1 | 1 | 0 | 0 | 7430 |
| graphql/ecommerce-shopify-admin | 1 | 1 | 0 | 0 | 2038 |
| grpc/otlp-trace-service | 1 | 1 | 0 | 0 | 6908 |
| grpc/route-guide | 1 | 1 | 0 | 0 | 1735 |
| rest/payments-paypal-orders | 1 | 1 | 0 | 0 | 10096 |
| rest/payments-stripe | 1 | 0 | 0 | 1 | 4690 |
| soap/accounts-rpc | 1 | 1 | 0 | 0 | 2217 |
| soap/calculator | 1 | 1 | 0 | 0 | 980 |

## Per API type

| API type | Calls | LLM | Repaired | Fallback | p95 ms |
| --- | --- | --- | --- | --- | --- |
| asyncapi | 2 | 2 | 0 | 0 | 4422 |
| graphql | 2 | 2 | 0 | 0 | 7430 |
| grpc | 2 | 2 | 0 | 0 | 6908 |
| rest | 2 | 1 | 0 | 1 | 10096 |
| soap | 2 | 2 | 0 | 0 | 2217 |

## Per domain

| Domain | Calls | LLM | Repaired | Fallback | p95 ms |
| --- | --- | --- | --- | --- | --- |
| devtools | 2 | 2 | 0 | 0 | 7430 |
| ecommerce | 3 | 3 | 0 | 0 | 4422 |
| generic | 3 | 3 | 0 | 0 | 2217 |
| payments | 2 | 1 | 0 | 1 | 10096 |
