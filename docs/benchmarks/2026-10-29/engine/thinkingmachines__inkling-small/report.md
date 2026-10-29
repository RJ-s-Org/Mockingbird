# Mockingbird LLM evals

Model `thinkingmachines/inkling-small` at `http://127.0.0.1:58357/r/thinkingmachines__inkling-small~engine/api/v1`, started 2026-10-29T19:47:01.777153+00:00.

| Metric | Value |
| --- | --- |
| Calls | 162 |
| Schema-valid rate | 83.3% |
| First-pass rate | 32.7% |
| Repair rate | 50.6% |
| Fallback rate | 16.7% |
| p50 latency | 2537 ms |
| p95 latency | 9982 ms |
| Prompt tokens per call | 1478.0 |
| Completion tokens per call | 617.4 |

## Per contract

| Contract | Calls | LLM | Repaired | Fallback | p95 ms |
| --- | --- | --- | --- | --- | --- |
| asyncapi/commerce-events-kafka | 9 | 0 | 7 | 2 | 4924 |
| asyncapi/orders-amqp-2.6 | 6 | 6 | 0 | 0 | 595 |
| graphql/devtools-github | 12 | 0 | 0 | 12 | 10914 |
| graphql/ecommerce-shopify-admin | 12 | 4 | 1 | 7 | 12101 |
| grpc/otlp-trace-service | 3 | 3 | 0 | 0 | 1122 |
| grpc/route-guide | 3 | 3 | 0 | 0 | 788 |
| rest/devtools-github | 12 | 0 | 11 | 1 | 9982 |
| rest/ecommerce-orders-3.1 | 9 | 7 | 2 | 0 | 4077 |
| rest/ecommerce-square | 12 | 6 | 6 | 0 | 3164 |
| rest/payments-paypal-orders | 9 | 0 | 9 | 0 | 7134 |
| rest/payments-stripe | 12 | 2 | 10 | 0 | 1913 |
| rest/social-discord | 9 | 0 | 6 | 3 | 7557 |
| rest/social-spotify | 9 | 0 | 8 | 1 | 12709 |
| rest/social-tmdb | 12 | 0 | 11 | 1 | 8815 |
| rest/travel-booking | 12 | 7 | 5 | 0 | 4455 |
| rest/travel-hotels | 6 | 0 | 6 | 0 | 7501 |
| soap/accounts-rpc | 6 | 6 | 0 | 0 | 1242 |
| soap/calculator | 9 | 9 | 0 | 0 | 654 |

## Per API type

| API type | Calls | LLM | Repaired | Fallback | p95 ms |
| --- | --- | --- | --- | --- | --- |
| asyncapi | 15 | 6 | 7 | 2 | 4924 |
| graphql | 24 | 4 | 1 | 19 | 10992 |
| grpc | 6 | 6 | 0 | 0 | 1122 |
| rest | 102 | 22 | 74 | 6 | 9420 |
| soap | 15 | 15 | 0 | 0 | 1242 |

## Per domain

| Domain | Calls | LLM | Repaired | Fallback | p95 ms |
| --- | --- | --- | --- | --- | --- |
| devtools | 27 | 3 | 11 | 13 | 10633 |
| ecommerce | 48 | 23 | 16 | 9 | 10092 |
| generic | 18 | 18 | 0 | 0 | 1242 |
| payments | 21 | 2 | 19 | 0 | 7024 |
| social-media | 30 | 0 | 25 | 5 | 11505 |
| travel | 18 | 7 | 11 | 0 | 7501 |
