# Mockingbird LLM evals

Model `qwen/qwen3.8-flash` at `http://127.0.0.1:62384/r/qwen__qwen3.8-flash~engine/api/v1`, started 2026-10-29T18:10:52.974918+00:00.

| Metric | Value |
| --- | --- |
| Calls | 162 |
| Schema-valid rate | 92.0% |
| First-pass rate | 55.6% |
| Repair rate | 36.4% |
| Fallback rate | 8.0% |
| p50 latency | 7270 ms |
| p95 latency | 25439 ms |
| Prompt tokens per call | 1048.1 |
| Completion tokens per call | 480.0 |

## Per contract

| Contract | Calls | LLM | Repaired | Fallback | p95 ms |
| --- | --- | --- | --- | --- | --- |
| asyncapi/commerce-events-kafka | 9 | 1 | 8 | 0 | 11514 |
| asyncapi/orders-amqp-2.6 | 6 | 6 | 0 | 0 | 6527 |
| graphql/devtools-github | 12 | 5 | 0 | 7 | 30011 |
| graphql/ecommerce-shopify-admin | 12 | 7 | 0 | 5 | 27802 |
| grpc/otlp-trace-service | 3 | 3 | 0 | 0 | 1732 |
| grpc/route-guide | 3 | 3 | 0 | 0 | 4519 |
| rest/devtools-github | 12 | 1 | 10 | 1 | 30002 |
| rest/ecommerce-orders-3.1 | 9 | 8 | 1 | 0 | 5583 |
| rest/ecommerce-square | 12 | 7 | 5 | 0 | 17801 |
| rest/payments-paypal-orders | 9 | 2 | 7 | 0 | 17016 |
| rest/payments-stripe | 12 | 8 | 4 | 0 | 11136 |
| rest/social-discord | 9 | 4 | 5 | 0 | 20285 |
| rest/social-spotify | 9 | 2 | 7 | 0 | 25649 |
| rest/social-tmdb | 12 | 1 | 11 | 0 | 13835 |
| rest/travel-booking | 12 | 11 | 1 | 0 | 21111 |
| rest/travel-hotels | 6 | 6 | 0 | 0 | 9989 |
| soap/accounts-rpc | 6 | 6 | 0 | 0 | 2561 |
| soap/calculator | 9 | 9 | 0 | 0 | 7080 |

## Per API type

| API type | Calls | LLM | Repaired | Fallback | p95 ms |
| --- | --- | --- | --- | --- | --- |
| asyncapi | 15 | 7 | 8 | 0 | 11514 |
| graphql | 24 | 12 | 0 | 12 | 30005 |
| grpc | 6 | 6 | 0 | 0 | 4519 |
| rest | 102 | 50 | 51 | 1 | 21111 |
| soap | 15 | 15 | 0 | 0 | 7080 |

## Per domain

| Domain | Calls | LLM | Repaired | Fallback | p95 ms |
| --- | --- | --- | --- | --- | --- |
| devtools | 27 | 9 | 10 | 8 | 30005 |
| ecommerce | 48 | 29 | 14 | 5 | 23814 |
| generic | 18 | 18 | 0 | 0 | 7080 |
| payments | 21 | 10 | 11 | 0 | 16703 |
| social-media | 30 | 7 | 23 | 0 | 23645 |
| travel | 18 | 17 | 1 | 0 | 21111 |
