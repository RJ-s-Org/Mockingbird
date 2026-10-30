# Mockingbird LLM evals

Model `openai/gpt-6-luna` at `http://127.0.0.1:60496/r/openai__gpt-6-luna~engine/api/v1`, started 2026-10-30T22:25:34.304272+00:00.

| Metric | Value |
| --- | --- |
| Calls | 162 |
| Schema-valid rate | 99.4% |
| First-pass rate | 82.1% |
| Repair rate | 17.3% |
| Fallback rate | 0.6% |
| p50 latency | 2722 ms |
| p95 latency | 6829 ms |
| Prompt tokens per call | 1245.8 |
| Completion tokens per call | 382.7 |
| Streaming operations not evaluated | 3 |

## Per contract

| Contract | Calls | LLM | Repaired | Fallback | p95 ms |
| --- | --- | --- | --- | --- | --- |
| asyncapi/commerce-events-kafka | 9 | 8 | 1 | 0 | 2748 |
| asyncapi/orders-amqp-2.6 | 6 | 6 | 0 | 0 | 2431 |
| graphql/devtools-github | 12 | 11 | 0 | 1 | 5878 |
| graphql/ecommerce-shopify-admin | 12 | 12 | 0 | 0 | 9662 |
| grpc/otlp-trace-service | 3 | 3 | 0 | 0 | 1575 |
| grpc/route-guide | 3 | 3 | 0 | 0 | 1756 |
| rest/devtools-github | 12 | 7 | 5 | 0 | 8539 |
| rest/ecommerce-orders-3.1 | 9 | 9 | 0 | 0 | 2892 |
| rest/ecommerce-square | 12 | 11 | 1 | 0 | 6085 |
| rest/payments-paypal-orders | 9 | 4 | 5 | 0 | 4558 |
| rest/payments-stripe | 12 | 12 | 0 | 0 | 2733 |
| rest/social-discord | 9 | 5 | 4 | 0 | 7892 |
| rest/social-spotify | 9 | 3 | 6 | 0 | 57572 |
| rest/social-tmdb | 12 | 8 | 4 | 0 | 6829 |
| rest/travel-booking | 12 | 12 | 0 | 0 | 5447 |
| rest/travel-hotels | 6 | 4 | 2 | 0 | 3069 |
| soap/accounts-rpc | 6 | 6 | 0 | 0 | 1919 |
| soap/calculator | 9 | 9 | 0 | 0 | 1552 |

## Per API type

| API type | Calls | LLM | Repaired | Fallback | p95 ms |
| --- | --- | --- | --- | --- | --- |
| asyncapi | 15 | 14 | 1 | 0 | 2748 |
| graphql | 24 | 23 | 0 | 1 | 6898 |
| grpc | 6 | 6 | 0 | 0 | 1756 |
| rest | 102 | 75 | 27 | 0 | 7249 |
| soap | 15 | 15 | 0 | 0 | 1919 |

## Per domain

| Domain | Calls | LLM | Repaired | Fallback | p95 ms |
| --- | --- | --- | --- | --- | --- |
| devtools | 27 | 21 | 5 | 1 | 7972 |
| ecommerce | 48 | 46 | 2 | 0 | 6085 |
| generic | 18 | 18 | 0 | 0 | 1919 |
| payments | 21 | 16 | 5 | 0 | 3871 |
| social-media | 30 | 16 | 14 | 0 | 7892 |
| travel | 18 | 16 | 2 | 0 | 5447 |
