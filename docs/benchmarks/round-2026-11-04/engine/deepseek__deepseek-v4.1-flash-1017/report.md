# Mockingbird LLM evals

Model `deepseek/deepseek-v4.1-flash-1017` at `http://127.0.0.1:54271/r/deepseek__deepseek-v4.1-flash-1017~engine/api/v1`, started 2026-11-04T18:24:18.758773+00:00.

| Metric | Value |
| --- | --- |
| Calls | 162 |
| Schema-valid rate | 100.0% |
| First-pass rate | 100.0% |
| Repair rate | 0.0% |
| Fallback rate | 0.0% |
| p50 latency | 1574 ms |
| p95 latency | 6980 ms |
| Prompt tokens per call | 1026.5 |
| Completion tokens per call | 379.5 |
| Streaming operations not evaluated | 3 |

## Per contract

| Contract | Calls | LLM | Repaired | Fallback | p95 ms |
| --- | --- | --- | --- | --- | --- |
| asyncapi/commerce-events-kafka | 9 | 9 | 0 | 0 | 1574 |
| asyncapi/orders-amqp-2.6 | 6 | 6 | 0 | 0 | 842 |
| graphql/devtools-github | 12 | 12 | 0 | 0 | 2292 |
| graphql/ecommerce-shopify-admin | 12 | 12 | 0 | 0 | 4439 |
| grpc/otlp-trace-service | 3 | 3 | 0 | 0 | 517 |
| grpc/route-guide | 3 | 3 | 0 | 0 | 3597 |
| rest/devtools-github | 12 | 12 | 0 | 0 | 14051 |
| rest/ecommerce-orders-3.1 | 9 | 9 | 0 | 0 | 1729 |
| rest/ecommerce-square | 12 | 12 | 0 | 0 | 2583 |
| rest/payments-paypal-orders | 9 | 9 | 0 | 0 | 2550 |
| rest/payments-stripe | 12 | 12 | 0 | 0 | 2379 |
| rest/social-discord | 9 | 9 | 0 | 0 | 2508 |
| rest/social-spotify | 9 | 9 | 0 | 0 | 5511 |
| rest/social-tmdb | 12 | 12 | 0 | 0 | 11738 |
| rest/travel-booking | 12 | 12 | 0 | 0 | 2887 |
| rest/travel-hotels | 6 | 6 | 0 | 0 | 1629 |
| soap/accounts-rpc | 6 | 6 | 0 | 0 | 606 |
| soap/calculator | 9 | 9 | 0 | 0 | 700 |

## Per API type

| API type | Calls | LLM | Repaired | Fallback | p95 ms |
| --- | --- | --- | --- | --- | --- |
| asyncapi | 15 | 15 | 0 | 0 | 1574 |
| graphql | 24 | 24 | 0 | 0 | 4154 |
| grpc | 6 | 6 | 0 | 0 | 3597 |
| rest | 102 | 102 | 0 | 0 | 10886 |
| soap | 15 | 15 | 0 | 0 | 700 |

## Per domain

| Domain | Calls | LLM | Repaired | Fallback | p95 ms |
| --- | --- | --- | --- | --- | --- |
| devtools | 27 | 27 | 0 | 0 | 11378 |
| ecommerce | 48 | 48 | 0 | 0 | 3389 |
| generic | 18 | 18 | 0 | 0 | 3597 |
| payments | 21 | 21 | 0 | 0 | 2379 |
| social-media | 30 | 30 | 0 | 0 | 10886 |
| travel | 18 | 18 | 0 | 0 | 2887 |
