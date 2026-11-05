# Mockingbird LLM evals

Model `openai/gpt-6-luna` at `http://127.0.0.1:61040/r/openai__gpt-6-luna~engine/api/v1`, started 2026-11-05T18:12:48.589058+00:00.

| Metric | Value |
| --- | --- |
| Calls | 162 |
| Schema-valid rate | 98.8% |
| First-pass rate | 85.2% |
| Repair rate | 13.6% |
| Fallback rate | 1.2% |
| p50 latency | 2999 ms |
| p95 latency | 8134 ms |
| Prompt tokens per call | 1292.0 |
| Completion tokens per call | 251.5 |
| Streaming operations not evaluated | 3 |

## Per contract

| Contract | Calls | LLM | Repaired | Fallback | p95 ms |
| --- | --- | --- | --- | --- | --- |
| asyncapi/commerce-events-kafka | 9 | 9 | 0 | 0 | 3440 |
| asyncapi/orders-amqp-2.6 | 6 | 6 | 0 | 0 | 1614 |
| graphql/devtools-github | 12 | 12 | 0 | 0 | 5924 |
| graphql/ecommerce-shopify-admin | 12 | 11 | 0 | 1 | 60003 |
| grpc/otlp-trace-service | 3 | 3 | 0 | 0 | 1370 |
| grpc/route-guide | 3 | 3 | 0 | 0 | 1558 |
| rest/devtools-github | 12 | 7 | 5 | 0 | 8134 |
| rest/ecommerce-orders-3.1 | 9 | 9 | 0 | 0 | 6036 |
| rest/ecommerce-square | 12 | 10 | 2 | 0 | 5068 |
| rest/payments-paypal-orders | 9 | 4 | 5 | 0 | 7075 |
| rest/payments-stripe | 12 | 12 | 0 | 0 | 12446 |
| rest/social-discord | 9 | 6 | 3 | 0 | 5441 |
| rest/social-spotify | 9 | 3 | 5 | 1 | 60003 |
| rest/social-tmdb | 12 | 10 | 2 | 0 | 7557 |
| rest/travel-booking | 12 | 12 | 0 | 0 | 6353 |
| rest/travel-hotels | 6 | 6 | 0 | 0 | 1841 |
| soap/accounts-rpc | 6 | 6 | 0 | 0 | 2071 |
| soap/calculator | 9 | 9 | 0 | 0 | 1936 |

## Per API type

| API type | Calls | LLM | Repaired | Fallback | p95 ms |
| --- | --- | --- | --- | --- | --- |
| asyncapi | 15 | 15 | 0 | 0 | 3440 |
| graphql | 24 | 23 | 0 | 1 | 9488 |
| grpc | 6 | 6 | 0 | 0 | 1558 |
| rest | 102 | 79 | 22 | 1 | 8134 |
| soap | 15 | 15 | 0 | 0 | 2071 |

## Per domain

| Domain | Calls | LLM | Repaired | Fallback | p95 ms |
| --- | --- | --- | --- | --- | --- |
| devtools | 27 | 22 | 5 | 0 | 7639 |
| ecommerce | 48 | 45 | 2 | 1 | 8507 |
| generic | 18 | 18 | 0 | 0 | 2071 |
| payments | 21 | 16 | 5 | 0 | 7075 |
| social-media | 30 | 19 | 10 | 1 | 10603 |
| travel | 18 | 18 | 0 | 0 | 6353 |
