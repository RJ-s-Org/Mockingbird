# Mockingbird LLM evals

Model `openai/gpt-6-luna` at `http://127.0.0.1:51311/r/openai__gpt-6-luna~engine/api/v1`, started 2026-11-04T20:55:46.309424+00:00.

| Metric | Value |
| --- | --- |
| Calls | 162 |
| Schema-valid rate | 98.1% |
| First-pass rate | 83.3% |
| Repair rate | 14.8% |
| Fallback rate | 1.9% |
| p50 latency | 2675 ms |
| p95 latency | 6887 ms |
| Prompt tokens per call | 1260.4 |
| Completion tokens per call | 234.5 |
| Streaming operations not evaluated | 3 |

## Per contract

| Contract | Calls | LLM | Repaired | Fallback | p95 ms |
| --- | --- | --- | --- | --- | --- |
| asyncapi/commerce-events-kafka | 9 | 8 | 1 | 0 | 3640 |
| asyncapi/orders-amqp-2.6 | 6 | 6 | 0 | 0 | 1636 |
| graphql/devtools-github | 12 | 12 | 0 | 0 | 6088 |
| graphql/ecommerce-shopify-admin | 12 | 12 | 0 | 0 | 7337 |
| grpc/otlp-trace-service | 3 | 3 | 0 | 0 | 1678 |
| grpc/route-guide | 3 | 3 | 0 | 0 | 1937 |
| rest/devtools-github | 12 | 9 | 3 | 0 | 9552 |
| rest/ecommerce-orders-3.1 | 9 | 9 | 0 | 0 | 2920 |
| rest/ecommerce-square | 12 | 9 | 3 | 0 | 8939 |
| rest/payments-paypal-orders | 9 | 3 | 6 | 0 | 4018 |
| rest/payments-stripe | 12 | 12 | 0 | 0 | 3880 |
| rest/social-discord | 9 | 4 | 3 | 2 | 60003 |
| rest/social-spotify | 9 | 3 | 5 | 1 | 60003 |
| rest/social-tmdb | 12 | 9 | 3 | 0 | 6871 |
| rest/travel-booking | 12 | 12 | 0 | 0 | 5032 |
| rest/travel-hotels | 6 | 6 | 0 | 0 | 2245 |
| soap/accounts-rpc | 6 | 6 | 0 | 0 | 2473 |
| soap/calculator | 9 | 9 | 0 | 0 | 2025 |

## Per API type

| API type | Calls | LLM | Repaired | Fallback | p95 ms |
| --- | --- | --- | --- | --- | --- |
| asyncapi | 15 | 14 | 1 | 0 | 3640 |
| graphql | 24 | 24 | 0 | 0 | 6088 |
| grpc | 6 | 6 | 0 | 0 | 1937 |
| rest | 102 | 76 | 23 | 3 | 8939 |
| soap | 15 | 15 | 0 | 0 | 2473 |

## Per domain

| Domain | Calls | LLM | Repaired | Fallback | p95 ms |
| --- | --- | --- | --- | --- | --- |
| devtools | 27 | 24 | 3 | 0 | 6088 |
| ecommerce | 48 | 44 | 4 | 0 | 5834 |
| generic | 18 | 18 | 0 | 0 | 2473 |
| payments | 21 | 15 | 6 | 0 | 3880 |
| social-media | 30 | 16 | 11 | 3 | 60003 |
| travel | 18 | 18 | 0 | 0 | 5032 |
