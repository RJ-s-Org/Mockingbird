# Mockingbird LLM evals

Model `deepseek/deepseek-v4.1-flash-1017` at `http://127.0.0.1:61040/r/deepseek__deepseek-v4.1-flash-1017~engine/api/v1`, started 2026-11-05T18:12:48.589086+00:00.

| Metric | Value |
| --- | --- |
| Calls | 162 |
| Schema-valid rate | 100.0% |
| First-pass rate | 100.0% |
| Repair rate | 0.0% |
| Fallback rate | 0.0% |
| p50 latency | 2067 ms |
| p95 latency | 5471 ms |
| Prompt tokens per call | 1025.2 |
| Completion tokens per call | 379.6 |
| Streaming operations not evaluated | 3 |

## Per contract

| Contract | Calls | LLM | Repaired | Fallback | p95 ms |
| --- | --- | --- | --- | --- | --- |
| asyncapi/commerce-events-kafka | 9 | 9 | 0 | 0 | 1687 |
| asyncapi/orders-amqp-2.6 | 6 | 6 | 0 | 0 | 793 |
| graphql/devtools-github | 12 | 12 | 0 | 0 | 2937 |
| graphql/ecommerce-shopify-admin | 12 | 12 | 0 | 0 | 5520 |
| grpc/otlp-trace-service | 3 | 3 | 0 | 0 | 450 |
| grpc/route-guide | 3 | 3 | 0 | 0 | 2445 |
| rest/devtools-github | 12 | 12 | 0 | 0 | 5772 |
| rest/ecommerce-orders-3.1 | 9 | 9 | 0 | 0 | 2702 |
| rest/ecommerce-square | 12 | 12 | 0 | 0 | 4109 |
| rest/payments-paypal-orders | 9 | 9 | 0 | 0 | 3983 |
| rest/payments-stripe | 12 | 12 | 0 | 0 | 2410 |
| rest/social-discord | 9 | 9 | 0 | 0 | 4256 |
| rest/social-spotify | 9 | 9 | 0 | 0 | 7304 |
| rest/social-tmdb | 12 | 12 | 0 | 0 | 6689 |
| rest/travel-booking | 12 | 12 | 0 | 0 | 3854 |
| rest/travel-hotels | 6 | 6 | 0 | 0 | 2642 |
| soap/accounts-rpc | 6 | 6 | 0 | 0 | 1109 |
| soap/calculator | 9 | 9 | 0 | 0 | 594 |

## Per API type

| API type | Calls | LLM | Repaired | Fallback | p95 ms |
| --- | --- | --- | --- | --- | --- |
| asyncapi | 15 | 15 | 0 | 0 | 1687 |
| graphql | 24 | 24 | 0 | 0 | 4446 |
| grpc | 6 | 6 | 0 | 0 | 2445 |
| rest | 102 | 102 | 0 | 0 | 5772 |
| soap | 15 | 15 | 0 | 0 | 1109 |

## Per domain

| Domain | Calls | LLM | Repaired | Fallback | p95 ms |
| --- | --- | --- | --- | --- | --- |
| devtools | 27 | 27 | 0 | 0 | 5246 |
| ecommerce | 48 | 48 | 0 | 0 | 4109 |
| generic | 18 | 18 | 0 | 0 | 2445 |
| payments | 21 | 21 | 0 | 0 | 3275 |
| social-media | 30 | 30 | 0 | 0 | 7033 |
| travel | 18 | 18 | 0 | 0 | 3854 |
