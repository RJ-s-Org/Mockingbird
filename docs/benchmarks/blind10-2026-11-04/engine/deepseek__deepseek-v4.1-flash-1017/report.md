# Mockingbird LLM evals

Model `deepseek/deepseek-v4.1-flash-1017` at `http://127.0.0.1:51311/r/deepseek__deepseek-v4.1-flash-1017~engine/api/v1`, started 2026-11-04T20:55:46.309441+00:00.

| Metric | Value |
| --- | --- |
| Calls | 162 |
| Schema-valid rate | 100.0% |
| First-pass rate | 100.0% |
| Repair rate | 0.0% |
| Fallback rate | 0.0% |
| p50 latency | 1834 ms |
| p95 latency | 6797 ms |
| Prompt tokens per call | 1026.5 |
| Completion tokens per call | 405.7 |
| Streaming operations not evaluated | 3 |

## Per contract

| Contract | Calls | LLM | Repaired | Fallback | p95 ms |
| --- | --- | --- | --- | --- | --- |
| asyncapi/commerce-events-kafka | 9 | 9 | 0 | 0 | 2477 |
| asyncapi/orders-amqp-2.6 | 6 | 6 | 0 | 0 | 432 |
| graphql/devtools-github | 12 | 12 | 0 | 0 | 6456 |
| graphql/ecommerce-shopify-admin | 12 | 12 | 0 | 0 | 7281 |
| grpc/otlp-trace-service | 3 | 3 | 0 | 0 | 508 |
| grpc/route-guide | 3 | 3 | 0 | 0 | 4712 |
| rest/devtools-github | 12 | 12 | 0 | 0 | 8815 |
| rest/ecommerce-orders-3.1 | 9 | 9 | 0 | 0 | 2160 |
| rest/ecommerce-square | 12 | 12 | 0 | 0 | 6015 |
| rest/payments-paypal-orders | 9 | 9 | 0 | 0 | 3389 |
| rest/payments-stripe | 12 | 12 | 0 | 0 | 1834 |
| rest/social-discord | 9 | 9 | 0 | 0 | 3489 |
| rest/social-spotify | 9 | 9 | 0 | 0 | 5871 |
| rest/social-tmdb | 12 | 12 | 0 | 0 | 7120 |
| rest/travel-booking | 12 | 12 | 0 | 0 | 8183 |
| rest/travel-hotels | 6 | 6 | 0 | 0 | 8039 |
| soap/accounts-rpc | 6 | 6 | 0 | 0 | 908 |
| soap/calculator | 9 | 9 | 0 | 0 | 615 |

## Per API type

| API type | Calls | LLM | Repaired | Fallback | p95 ms |
| --- | --- | --- | --- | --- | --- |
| asyncapi | 15 | 15 | 0 | 0 | 2477 |
| graphql | 24 | 24 | 0 | 0 | 6456 |
| grpc | 6 | 6 | 0 | 0 | 4712 |
| rest | 102 | 102 | 0 | 0 | 8039 |
| soap | 15 | 15 | 0 | 0 | 908 |

## Per domain

| Domain | Calls | LLM | Repaired | Fallback | p95 ms |
| --- | --- | --- | --- | --- | --- |
| devtools | 27 | 27 | 0 | 0 | 8522 |
| ecommerce | 48 | 48 | 0 | 0 | 5644 |
| generic | 18 | 18 | 0 | 0 | 4712 |
| payments | 21 | 21 | 0 | 0 | 2984 |
| social-media | 30 | 30 | 0 | 0 | 6797 |
| travel | 18 | 18 | 0 | 0 | 8183 |
