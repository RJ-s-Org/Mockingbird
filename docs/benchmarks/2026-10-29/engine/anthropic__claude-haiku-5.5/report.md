# Mockingbird LLM evals

Model `anthropic/claude-haiku-5.5` at `http://127.0.0.1:62384/r/anthropic__claude-haiku-5.5~engine/api/v1`, started 2026-10-29T18:10:25.147077+00:00.

| Metric | Value |
| --- | --- |
| Calls | 162 |
| Schema-valid rate | 92.0% |
| First-pass rate | 72.8% |
| Repair rate | 19.1% |
| Fallback rate | 8.0% |
| p50 latency | 2288 ms |
| p95 latency | 8044 ms |
| Prompt tokens per call | 2325.3 |
| Completion tokens per call | 599.6 |

## Per contract

| Contract | Calls | LLM | Repaired | Fallback | p95 ms |
| --- | --- | --- | --- | --- | --- |
| asyncapi/commerce-events-kafka | 9 | 9 | 0 | 0 | 2180 |
| asyncapi/orders-amqp-2.6 | 6 | 6 | 0 | 0 | 930 |
| graphql/devtools-github | 12 | 8 | 0 | 4 | 8496 |
| graphql/ecommerce-shopify-admin | 12 | 9 | 0 | 3 | 13580 |
| grpc/otlp-trace-service | 3 | 3 | 0 | 0 | 824 |
| grpc/route-guide | 3 | 3 | 0 | 0 | 854 |
| rest/devtools-github | 12 | 2 | 10 | 0 | 6835 |
| rest/ecommerce-orders-3.1 | 9 | 9 | 0 | 0 | 2218 |
| rest/ecommerce-square | 12 | 8 | 2 | 2 | 8206 |
| rest/payments-paypal-orders | 9 | 6 | 2 | 1 | 5689 |
| rest/payments-stripe | 12 | 9 | 3 | 0 | 4138 |
| rest/social-discord | 9 | 2 | 6 | 1 | 9266 |
| rest/social-spotify | 9 | 6 | 1 | 2 | 12250 |
| rest/social-tmdb | 12 | 6 | 6 | 0 | 8044 |
| rest/travel-booking | 12 | 11 | 1 | 0 | 4654 |
| rest/travel-hotels | 6 | 6 | 0 | 0 | 4152 |
| soap/accounts-rpc | 6 | 6 | 0 | 0 | 1052 |
| soap/calculator | 9 | 9 | 0 | 0 | 1466 |

## Per API type

| API type | Calls | LLM | Repaired | Fallback | p95 ms |
| --- | --- | --- | --- | --- | --- |
| asyncapi | 15 | 15 | 0 | 0 | 2180 |
| graphql | 24 | 17 | 0 | 7 | 8496 |
| grpc | 6 | 6 | 0 | 0 | 854 |
| rest | 102 | 65 | 31 | 6 | 8114 |
| soap | 15 | 15 | 0 | 0 | 1466 |

## Per domain

| Domain | Calls | LLM | Repaired | Fallback | p95 ms |
| --- | --- | --- | --- | --- | --- |
| devtools | 27 | 13 | 10 | 4 | 7730 |
| ecommerce | 48 | 41 | 2 | 5 | 8171 |
| generic | 18 | 18 | 0 | 0 | 1466 |
| payments | 21 | 15 | 5 | 1 | 4138 |
| social-media | 30 | 14 | 13 | 3 | 11954 |
| travel | 18 | 17 | 1 | 0 | 4654 |
