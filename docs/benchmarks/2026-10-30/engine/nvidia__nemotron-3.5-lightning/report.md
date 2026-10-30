# Mockingbird LLM evals

Model `nvidia/nemotron-3.5-lightning` at `http://127.0.0.1:58482/r/nvidia__nemotron-3.5-lightning~engine/api/v1`, started 2026-10-30T07:30:46.010155+00:00.

| Metric | Value |
| --- | --- |
| Calls | 162 |
| Schema-valid rate | 88.3% |
| First-pass rate | 37.0% |
| Repair rate | 51.2% |
| Fallback rate | 11.7% |
| p50 latency | 2846 ms |
| p95 latency | 9932 ms |
| Prompt tokens per call | 1662.0 |
| Completion tokens per call | 543.8 |
| Streaming operations not evaluated | 3 |

## Per contract

| Contract | Calls | LLM | Repaired | Fallback | p95 ms |
| --- | --- | --- | --- | --- | --- |
| asyncapi/commerce-events-kafka | 9 | 1 | 8 | 0 | 7384 |
| asyncapi/orders-amqp-2.6 | 6 | 6 | 0 | 0 | 1246 |
| graphql/devtools-github | 12 | 9 | 0 | 3 | 4442 |
| graphql/ecommerce-shopify-admin | 12 | 8 | 2 | 2 | 7232 |
| grpc/otlp-trace-service | 3 | 3 | 0 | 0 | 951 |
| grpc/route-guide | 3 | 3 | 0 | 0 | 958 |
| rest/devtools-github | 12 | 0 | 11 | 1 | 13840 |
| rest/ecommerce-orders-3.1 | 9 | 2 | 7 | 0 | 2246 |
| rest/ecommerce-square | 12 | 9 | 3 | 0 | 5661 |
| rest/payments-paypal-orders | 9 | 0 | 6 | 3 | 10282 |
| rest/payments-stripe | 12 | 0 | 12 | 0 | 5399 |
| rest/social-discord | 9 | 0 | 7 | 2 | 23432 |
| rest/social-spotify | 9 | 0 | 9 | 0 | 12546 |
| rest/social-tmdb | 12 | 1 | 10 | 1 | 12016 |
| rest/travel-booking | 12 | 3 | 4 | 5 | 9932 |
| rest/travel-hotels | 6 | 0 | 4 | 2 | 11999 |
| soap/accounts-rpc | 6 | 6 | 0 | 0 | 1251 |
| soap/calculator | 9 | 9 | 0 | 0 | 737 |

## Per API type

| API type | Calls | LLM | Repaired | Fallback | p95 ms |
| --- | --- | --- | --- | --- | --- |
| asyncapi | 15 | 7 | 8 | 0 | 7384 |
| graphql | 24 | 17 | 2 | 5 | 6317 |
| grpc | 6 | 6 | 0 | 0 | 958 |
| rest | 102 | 15 | 73 | 14 | 11999 |
| soap | 15 | 15 | 0 | 0 | 1251 |

## Per domain

| Domain | Calls | LLM | Repaired | Fallback | p95 ms |
| --- | --- | --- | --- | --- | --- |
| devtools | 27 | 12 | 11 | 4 | 13622 |
| ecommerce | 48 | 26 | 20 | 2 | 6317 |
| generic | 18 | 18 | 0 | 0 | 1251 |
| payments | 21 | 0 | 18 | 3 | 8080 |
| social-media | 30 | 1 | 26 | 3 | 12546 |
| travel | 18 | 3 | 8 | 7 | 11999 |
