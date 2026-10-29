# Mockingbird LLM evals

Model `deepseek/deepseek-v4.1-flash` at `http://127.0.0.1:62384/r/deepseek__deepseek-v4.1-flash~engine/api/v1`, started 2026-10-29T18:07:16.631803+00:00.

| Metric | Value |
| --- | --- |
| Calls | 162 |
| Schema-valid rate | 93.8% |
| First-pass rate | 69.8% |
| Repair rate | 24.1% |
| Fallback rate | 6.2% |
| p50 latency | 2281 ms |
| p95 latency | 8687 ms |
| Prompt tokens per call | 1414.2 |
| Completion tokens per call | 496.2 |

## Per contract

| Contract | Calls | LLM | Repaired | Fallback | p95 ms |
| --- | --- | --- | --- | --- | --- |
| asyncapi/commerce-events-kafka | 9 | 6 | 3 | 0 | 3647 |
| asyncapi/orders-amqp-2.6 | 6 | 6 | 0 | 0 | 1077 |
| graphql/devtools-github | 12 | 7 | 0 | 5 | 11801 |
| graphql/ecommerce-shopify-admin | 12 | 9 | 0 | 3 | 16428 |
| grpc/otlp-trace-service | 3 | 3 | 0 | 0 | 839 |
| grpc/route-guide | 3 | 3 | 0 | 0 | 652 |
| rest/devtools-github | 12 | 1 | 10 | 1 | 6686 |
| rest/ecommerce-orders-3.1 | 9 | 9 | 0 | 0 | 2837 |
| rest/ecommerce-square | 12 | 8 | 4 | 0 | 4611 |
| rest/payments-paypal-orders | 9 | 5 | 4 | 0 | 5434 |
| rest/payments-stripe | 12 | 5 | 7 | 0 | 3363 |
| rest/social-discord | 9 | 7 | 2 | 0 | 6519 |
| rest/social-spotify | 9 | 6 | 2 | 1 | 8630 |
| rest/social-tmdb | 12 | 6 | 6 | 0 | 10298 |
| rest/travel-booking | 12 | 11 | 1 | 0 | 14211 |
| rest/travel-hotels | 6 | 6 | 0 | 0 | 9677 |
| soap/accounts-rpc | 6 | 6 | 0 | 0 | 4619 |
| soap/calculator | 9 | 9 | 0 | 0 | 893 |

## Per API type

| API type | Calls | LLM | Repaired | Fallback | p95 ms |
| --- | --- | --- | --- | --- | --- |
| asyncapi | 15 | 12 | 3 | 0 | 3647 |
| graphql | 24 | 16 | 0 | 8 | 13291 |
| grpc | 6 | 6 | 0 | 0 | 839 |
| rest | 102 | 64 | 36 | 2 | 8585 |
| soap | 15 | 15 | 0 | 0 | 4619 |

## Per domain

| Domain | Calls | LLM | Repaired | Fallback | p95 ms |
| --- | --- | --- | --- | --- | --- |
| devtools | 27 | 11 | 10 | 6 | 8687 |
| ecommerce | 48 | 38 | 7 | 3 | 9293 |
| generic | 18 | 18 | 0 | 0 | 4619 |
| payments | 21 | 10 | 11 | 0 | 5268 |
| social-media | 30 | 19 | 10 | 1 | 8630 |
| travel | 18 | 17 | 1 | 0 | 14211 |
