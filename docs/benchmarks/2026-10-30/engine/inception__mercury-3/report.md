# Mockingbird LLM evals

Model `inception/mercury-3` at `http://127.0.0.1:58482/r/inception__mercury-3~engine/api/v1`, started 2026-10-30T07:30:46.010152+00:00.

| Metric | Value |
| --- | --- |
| Calls | 162 |
| Schema-valid rate | 96.3% |
| First-pass rate | 46.9% |
| Repair rate | 49.4% |
| Fallback rate | 3.7% |
| p50 latency | 996 ms |
| p95 latency | 5817 ms |
| Prompt tokens per call | 1442.8 |
| Completion tokens per call | 947.3 |
| Streaming operations not evaluated | 3 |

## Per contract

| Contract | Calls | LLM | Repaired | Fallback | p95 ms |
| --- | --- | --- | --- | --- | --- |
| asyncapi/commerce-events-kafka | 9 | 0 | 9 | 0 | 834 |
| asyncapi/orders-amqp-2.6 | 6 | 6 | 0 | 0 | 4660 |
| graphql/devtools-github | 12 | 10 | 1 | 1 | 2852 |
| graphql/ecommerce-shopify-admin | 12 | 11 | 0 | 1 | 5817 |
| grpc/otlp-trace-service | 3 | 3 | 0 | 0 | 657 |
| grpc/route-guide | 3 | 3 | 0 | 0 | 548 |
| rest/devtools-github | 12 | 1 | 11 | 0 | 2515 |
| rest/ecommerce-orders-3.1 | 9 | 6 | 3 | 0 | 1148 |
| rest/ecommerce-square | 12 | 6 | 6 | 0 | 10554 |
| rest/payments-paypal-orders | 9 | 0 | 8 | 1 | 25881 |
| rest/payments-stripe | 12 | 1 | 11 | 0 | 938 |
| rest/social-discord | 9 | 0 | 9 | 0 | 1668 |
| rest/social-spotify | 9 | 0 | 7 | 2 | 42359 |
| rest/social-tmdb | 12 | 0 | 11 | 1 | 37383 |
| rest/travel-booking | 12 | 11 | 1 | 0 | 1862 |
| rest/travel-hotels | 6 | 3 | 3 | 0 | 1376 |
| soap/accounts-rpc | 6 | 6 | 0 | 0 | 720 |
| soap/calculator | 9 | 9 | 0 | 0 | 609 |

## Per API type

| API type | Calls | LLM | Repaired | Fallback | p95 ms |
| --- | --- | --- | --- | --- | --- |
| asyncapi | 15 | 6 | 9 | 0 | 4660 |
| graphql | 24 | 21 | 1 | 2 | 3331 |
| grpc | 6 | 6 | 0 | 0 | 657 |
| rest | 102 | 28 | 70 | 4 | 14251 |
| soap | 15 | 15 | 0 | 0 | 720 |

## Per domain

| Domain | Calls | LLM | Repaired | Fallback | p95 ms |
| --- | --- | --- | --- | --- | --- |
| devtools | 27 | 14 | 12 | 1 | 2515 |
| ecommerce | 48 | 29 | 18 | 1 | 5817 |
| generic | 18 | 18 | 0 | 0 | 720 |
| payments | 21 | 1 | 19 | 1 | 19710 |
| social-media | 30 | 0 | 27 | 3 | 37383 |
| travel | 18 | 14 | 4 | 0 | 1862 |
