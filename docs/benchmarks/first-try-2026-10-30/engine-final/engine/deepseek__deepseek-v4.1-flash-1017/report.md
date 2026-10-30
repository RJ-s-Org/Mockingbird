# Mockingbird LLM evals

Model `deepseek/deepseek-v4.1-flash-1017` at `http://127.0.0.1:60974/r/deepseek__deepseek-v4.1-flash-1017~engine/api/v1`, started 2026-10-30T22:35:58.667131+00:00.

| Metric | Value |
| --- | --- |
| Calls | 162 |
| Schema-valid rate | 99.4% |
| First-pass rate | 97.5% |
| Repair rate | 1.9% |
| Fallback rate | 0.6% |
| p50 latency | 2142 ms |
| p95 latency | 7630 ms |
| Prompt tokens per call | 1058.9 |
| Completion tokens per call | 504.0 |
| Streaming operations not evaluated | 3 |

## Per contract

| Contract | Calls | LLM | Repaired | Fallback | p95 ms |
| --- | --- | --- | --- | --- | --- |
| asyncapi/commerce-events-kafka | 9 | 9 | 0 | 0 | 2404 |
| asyncapi/orders-amqp-2.6 | 6 | 6 | 0 | 0 | 1044 |
| graphql/devtools-github | 12 | 12 | 0 | 0 | 8689 |
| graphql/ecommerce-shopify-admin | 12 | 12 | 0 | 0 | 14328 |
| grpc/otlp-trace-service | 3 | 3 | 0 | 0 | 672 |
| grpc/route-guide | 3 | 3 | 0 | 0 | 908 |
| rest/devtools-github | 12 | 11 | 0 | 1 | 4583 |
| rest/ecommerce-orders-3.1 | 9 | 9 | 0 | 0 | 2902 |
| rest/ecommerce-square | 12 | 12 | 0 | 0 | 5759 |
| rest/payments-paypal-orders | 9 | 9 | 0 | 0 | 5371 |
| rest/payments-stripe | 12 | 12 | 0 | 0 | 2360 |
| rest/social-discord | 9 | 9 | 0 | 0 | 3711 |
| rest/social-spotify | 9 | 9 | 0 | 0 | 11416 |
| rest/social-tmdb | 12 | 11 | 1 | 0 | 6925 |
| rest/travel-booking | 12 | 10 | 2 | 0 | 31602 |
| rest/travel-hotels | 6 | 6 | 0 | 0 | 11910 |
| soap/accounts-rpc | 6 | 6 | 0 | 0 | 1653 |
| soap/calculator | 9 | 9 | 0 | 0 | 877 |

## Per API type

| API type | Calls | LLM | Repaired | Fallback | p95 ms |
| --- | --- | --- | --- | --- | --- |
| asyncapi | 15 | 15 | 0 | 0 | 2404 |
| graphql | 24 | 24 | 0 | 0 | 8689 |
| grpc | 6 | 6 | 0 | 0 | 908 |
| rest | 102 | 98 | 3 | 1 | 8022 |
| soap | 15 | 15 | 0 | 0 | 1653 |

## Per domain

| Domain | Calls | LLM | Repaired | Fallback | p95 ms |
| --- | --- | --- | --- | --- | --- |
| devtools | 27 | 26 | 0 | 1 | 4707 |
| ecommerce | 48 | 48 | 0 | 0 | 5061 |
| generic | 18 | 18 | 0 | 0 | 1653 |
| payments | 21 | 21 | 0 | 0 | 4469 |
| social-media | 30 | 29 | 1 | 0 | 9404 |
| travel | 18 | 16 | 2 | 0 | 31602 |
