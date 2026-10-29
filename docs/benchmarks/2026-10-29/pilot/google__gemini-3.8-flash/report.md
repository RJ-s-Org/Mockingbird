# Mockingbird LLM evals

Model `google/gemini-3.8-flash` at `http://127.0.0.1:62241/r/google__gemini-3.8-flash~pilot/api/v1`, started 2026-10-29T18:03:35.049592+00:00.

| Metric | Value |
| --- | --- |
| Calls | 10 |
| Schema-valid rate | 90.0% |
| First-pass rate | 90.0% |
| Repair rate | 0.0% |
| Fallback rate | 10.0% |
| p50 latency | 4345 ms |
| p95 latency | 13027 ms |
| Prompt tokens per call | 920.8 |
| Completion tokens per call | 636.5 |

## Per contract

| Contract | Calls | LLM | Repaired | Fallback | p95 ms |
| --- | --- | --- | --- | --- | --- |
| asyncapi/commerce-events-kafka | 1 | 0 | 0 | 1 | 10486 |
| asyncapi/orders-amqp-2.6 | 1 | 1 | 0 | 0 | 4345 |
| graphql/devtools-github | 1 | 1 | 0 | 0 | 8600 |
| graphql/ecommerce-shopify-admin | 1 | 1 | 0 | 0 | 3449 |
| grpc/otlp-trace-service | 1 | 1 | 0 | 0 | 4206 |
| grpc/route-guide | 1 | 1 | 0 | 0 | 3229 |
| rest/payments-paypal-orders | 1 | 1 | 0 | 0 | 6469 |
| rest/payments-stripe | 1 | 1 | 0 | 0 | 11347 |
| soap/accounts-rpc | 1 | 1 | 0 | 0 | 3193 |
| soap/calculator | 1 | 1 | 0 | 0 | 13027 |

## Per API type

| API type | Calls | LLM | Repaired | Fallback | p95 ms |
| --- | --- | --- | --- | --- | --- |
| asyncapi | 2 | 1 | 0 | 1 | 10486 |
| graphql | 2 | 2 | 0 | 0 | 8600 |
| grpc | 2 | 2 | 0 | 0 | 4206 |
| rest | 2 | 2 | 0 | 0 | 11347 |
| soap | 2 | 2 | 0 | 0 | 13027 |

## Per domain

| Domain | Calls | LLM | Repaired | Fallback | p95 ms |
| --- | --- | --- | --- | --- | --- |
| devtools | 2 | 2 | 0 | 0 | 8600 |
| ecommerce | 3 | 2 | 0 | 1 | 10486 |
| generic | 3 | 3 | 0 | 0 | 13027 |
| payments | 2 | 2 | 0 | 0 | 11347 |
