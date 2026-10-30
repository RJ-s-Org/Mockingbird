# Mockingbird LLM evals

Model `anthropic/claude-haiku-5.5` at `http://127.0.0.1:58482/r/anthropic__claude-haiku-5.5~engine/api/v1`, started 2026-10-30T07:43:16.482426+00:00.

| Metric | Value |
| --- | --- |
| Calls | 162 |
| Schema-valid rate | 99.4% |
| First-pass rate | 73.5% |
| Repair rate | 25.9% |
| Fallback rate | 0.6% |
| p50 latency | 2267 ms |
| p95 latency | 6538 ms |
| Prompt tokens per call | 1631.6 |
| Completion tokens per call | 540.1 |
| Streaming operations not evaluated | 3 |

## Per contract

| Contract | Calls | LLM | Repaired | Fallback | p95 ms |
| --- | --- | --- | --- | --- | --- |
| asyncapi/commerce-events-kafka | 9 | 7 | 2 | 0 | 2267 |
| asyncapi/orders-amqp-2.6 | 6 | 6 | 0 | 0 | 880 |
| graphql/devtools-github | 12 | 10 | 2 | 0 | 6822 |
| graphql/ecommerce-shopify-admin | 12 | 12 | 0 | 0 | 6533 |
| grpc/otlp-trace-service | 3 | 3 | 0 | 0 | 1158 |
| grpc/route-guide | 3 | 3 | 0 | 0 | 797 |
| rest/devtools-github | 12 | 1 | 10 | 1 | 6760 |
| rest/ecommerce-orders-3.1 | 9 | 9 | 0 | 0 | 1959 |
| rest/ecommerce-square | 12 | 12 | 0 | 0 | 4937 |
| rest/payments-paypal-orders | 9 | 4 | 5 | 0 | 3553 |
| rest/payments-stripe | 12 | 7 | 5 | 0 | 2067 |
| rest/social-discord | 9 | 2 | 7 | 0 | 7506 |
| rest/social-spotify | 9 | 6 | 3 | 0 | 12532 |
| rest/social-tmdb | 12 | 5 | 7 | 0 | 5951 |
| rest/travel-booking | 12 | 12 | 0 | 0 | 4554 |
| rest/travel-hotels | 6 | 6 | 0 | 0 | 3660 |
| soap/accounts-rpc | 6 | 6 | 0 | 0 | 1024 |
| soap/calculator | 9 | 8 | 1 | 0 | 1802 |

## Per API type

| API type | Calls | LLM | Repaired | Fallback | p95 ms |
| --- | --- | --- | --- | --- | --- |
| asyncapi | 15 | 13 | 2 | 0 | 2267 |
| graphql | 24 | 22 | 2 | 0 | 6533 |
| grpc | 6 | 6 | 0 | 0 | 1158 |
| rest | 102 | 64 | 37 | 1 | 6738 |
| soap | 15 | 14 | 1 | 0 | 1802 |

## Per domain

| Domain | Calls | LLM | Repaired | Fallback | p95 ms |
| --- | --- | --- | --- | --- | --- |
| devtools | 27 | 14 | 12 | 1 | 6760 |
| ecommerce | 48 | 46 | 2 | 0 | 4465 |
| generic | 18 | 17 | 1 | 0 | 1802 |
| payments | 21 | 11 | 10 | 0 | 3117 |
| social-media | 30 | 13 | 17 | 0 | 7506 |
| travel | 18 | 18 | 0 | 0 | 4554 |
