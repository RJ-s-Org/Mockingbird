# Mockingbird LLM evals

Model `nvidia/nemotron-3.5-lightning` at `http://127.0.0.1:51311/r/nvidia__nemotron-3.5-lightning~engine/api/v1`, started 2026-11-04T20:55:46.309427+00:00.

| Metric | Value |
| --- | --- |
| Calls | 162 |
| Schema-valid rate | 93.2% |
| First-pass rate | 77.8% |
| Repair rate | 15.4% |
| Fallback rate | 6.8% |
| p50 latency | 1340 ms |
| p95 latency | 60003 ms |
| Prompt tokens per call | 1099.7 |
| Completion tokens per call | 856.5 |
| Streaming operations not evaluated | 3 |

## Per contract

| Contract | Calls | LLM | Repaired | Fallback | p95 ms |
| --- | --- | --- | --- | --- | --- |
| asyncapi/commerce-events-kafka | 9 | 9 | 0 | 0 | 1335 |
| asyncapi/orders-amqp-2.6 | 6 | 6 | 0 | 0 | 568 |
| graphql/devtools-github | 12 | 11 | 0 | 1 | 60003 |
| graphql/ecommerce-shopify-admin | 12 | 10 | 0 | 2 | 60004 |
| grpc/otlp-trace-service | 3 | 3 | 0 | 0 | 728 |
| grpc/route-guide | 3 | 3 | 0 | 0 | 785 |
| rest/devtools-github | 12 | 9 | 2 | 1 | 60001 |
| rest/ecommerce-orders-3.1 | 9 | 3 | 5 | 1 | 60004 |
| rest/ecommerce-square | 12 | 10 | 2 | 0 | 55936 |
| rest/payments-paypal-orders | 9 | 7 | 2 | 0 | 2736 |
| rest/payments-stripe | 12 | 12 | 0 | 0 | 2475 |
| rest/social-discord | 9 | 2 | 4 | 3 | 87330 |
| rest/social-spotify | 9 | 9 | 0 | 0 | 4902 |
| rest/social-tmdb | 12 | 1 | 10 | 1 | 60004 |
| rest/travel-booking | 12 | 11 | 0 | 1 | 60007 |
| rest/travel-hotels | 6 | 6 | 0 | 0 | 3041 |
| soap/accounts-rpc | 6 | 6 | 0 | 0 | 611 |
| soap/calculator | 9 | 8 | 0 | 1 | 60003 |

## Per API type

| API type | Calls | LLM | Repaired | Fallback | p95 ms |
| --- | --- | --- | --- | --- | --- |
| asyncapi | 15 | 15 | 0 | 0 | 1335 |
| graphql | 24 | 21 | 0 | 3 | 60003 |
| grpc | 6 | 6 | 0 | 0 | 785 |
| rest | 102 | 70 | 25 | 7 | 60002 |
| soap | 15 | 14 | 0 | 1 | 60003 |

## Per domain

| Domain | Calls | LLM | Repaired | Fallback | p95 ms |
| --- | --- | --- | --- | --- | --- |
| devtools | 27 | 23 | 2 | 2 | 60001 |
| ecommerce | 48 | 38 | 7 | 3 | 60003 |
| generic | 18 | 17 | 0 | 1 | 60003 |
| payments | 21 | 19 | 2 | 0 | 2521 |
| social-media | 30 | 12 | 14 | 4 | 60004 |
| travel | 18 | 17 | 0 | 1 | 60007 |
