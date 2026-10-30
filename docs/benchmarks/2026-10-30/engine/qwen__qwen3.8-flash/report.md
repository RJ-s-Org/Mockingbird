# Mockingbird LLM evals

Model `qwen/qwen3.8-flash` at `http://127.0.0.1:58482/r/qwen__qwen3.8-flash~engine/api/v1`, started 2026-10-30T07:40:34.663213+00:00.

| Metric | Value |
| --- | --- |
| Calls | 162 |
| Schema-valid rate | 100.0% |
| First-pass rate | 63.0% |
| Repair rate | 37.0% |
| Fallback rate | 0.0% |
| p50 latency | 7893 ms |
| p95 latency | 19348 ms |
| Prompt tokens per call | 927.5 |
| Completion tokens per call | 510.3 |
| Streaming operations not evaluated | 3 |

## Per contract

| Contract | Calls | LLM | Repaired | Fallback | p95 ms |
| --- | --- | --- | --- | --- | --- |
| asyncapi/commerce-events-kafka | 9 | 2 | 7 | 0 | 5981 |
| asyncapi/orders-amqp-2.6 | 6 | 6 | 0 | 0 | 7128 |
| graphql/devtools-github | 12 | 12 | 0 | 0 | 16248 |
| graphql/ecommerce-shopify-admin | 12 | 11 | 1 | 0 | 23661 |
| grpc/otlp-trace-service | 3 | 3 | 0 | 0 | 4380 |
| grpc/route-guide | 3 | 3 | 0 | 0 | 2237 |
| rest/devtools-github | 12 | 2 | 10 | 0 | 18982 |
| rest/ecommerce-orders-3.1 | 9 | 9 | 0 | 0 | 11795 |
| rest/ecommerce-square | 12 | 9 | 3 | 0 | 17930 |
| rest/payments-paypal-orders | 9 | 1 | 8 | 0 | 14976 |
| rest/payments-stripe | 12 | 3 | 9 | 0 | 8901 |
| rest/social-discord | 9 | 6 | 3 | 0 | 20589 |
| rest/social-spotify | 9 | 3 | 6 | 0 | 24203 |
| rest/social-tmdb | 12 | 3 | 9 | 0 | 19348 |
| rest/travel-booking | 12 | 8 | 4 | 0 | 20087 |
| rest/travel-hotels | 6 | 6 | 0 | 0 | 10210 |
| soap/accounts-rpc | 6 | 6 | 0 | 0 | 6516 |
| soap/calculator | 9 | 9 | 0 | 0 | 5813 |

## Per API type

| API type | Calls | LLM | Repaired | Fallback | p95 ms |
| --- | --- | --- | --- | --- | --- |
| asyncapi | 15 | 8 | 7 | 0 | 7128 |
| graphql | 24 | 23 | 1 | 0 | 22753 |
| grpc | 6 | 6 | 0 | 0 | 4380 |
| rest | 102 | 50 | 52 | 0 | 20087 |
| soap | 15 | 15 | 0 | 0 | 6516 |

## Per domain

| Domain | Calls | LLM | Repaired | Fallback | p95 ms |
| --- | --- | --- | --- | --- | --- |
| devtools | 27 | 17 | 10 | 0 | 17404 |
| ecommerce | 48 | 37 | 11 | 0 | 17930 |
| generic | 18 | 18 | 0 | 0 | 6516 |
| payments | 21 | 4 | 17 | 0 | 13751 |
| social-media | 30 | 12 | 18 | 0 | 24000 |
| travel | 18 | 14 | 4 | 0 | 20087 |
