# Mockingbird LLM evals

Model `nvidia/nemotron-3.5-lightning` at `http://127.0.0.1:62384/r/nvidia__nemotron-3.5-lightning~engine/api/v1`, started 2026-10-29T18:07:16.632234+00:00.

| Metric | Value |
| --- | --- |
| Calls | 162 |
| Schema-valid rate | 70.4% |
| First-pass rate | 29.6% |
| Repair rate | 40.7% |
| Fallback rate | 29.6% |
| p50 latency | 2317 ms |
| p95 latency | 8263 ms |
| Prompt tokens per call | 2333.8 |
| Completion tokens per call | 681.0 |

## Per contract

| Contract | Calls | LLM | Repaired | Fallback | p95 ms |
| --- | --- | --- | --- | --- | --- |
| asyncapi/commerce-events-kafka | 9 | 0 | 5 | 4 | 5094 |
| asyncapi/orders-amqp-2.6 | 6 | 6 | 0 | 0 | 1505 |
| graphql/devtools-github | 12 | 1 | 0 | 11 | 11255 |
| graphql/ecommerce-shopify-admin | 12 | 3 | 2 | 7 | 14609 |
| grpc/otlp-trace-service | 3 | 3 | 0 | 0 | 850 |
| grpc/route-guide | 3 | 3 | 0 | 0 | 659 |
| rest/devtools-github | 12 | 0 | 9 | 3 | 8804 |
| rest/ecommerce-orders-3.1 | 9 | 1 | 8 | 0 | 2356 |
| rest/ecommerce-square | 12 | 8 | 4 | 0 | 3541 |
| rest/payments-paypal-orders | 9 | 0 | 3 | 6 | 5026 |
| rest/payments-stripe | 12 | 2 | 7 | 3 | 2723 |
| rest/social-discord | 9 | 1 | 3 | 5 | 6903 |
| rest/social-spotify | 9 | 0 | 9 | 0 | 6137 |
| rest/social-tmdb | 12 | 1 | 11 | 0 | 9905 |
| rest/travel-booking | 12 | 3 | 4 | 5 | 8902 |
| rest/travel-hotels | 6 | 1 | 1 | 4 | 6037 |
| soap/accounts-rpc | 6 | 6 | 0 | 0 | 1101 |
| soap/calculator | 9 | 9 | 0 | 0 | 571 |

## Per API type

| API type | Calls | LLM | Repaired | Fallback | p95 ms |
| --- | --- | --- | --- | --- | --- |
| asyncapi | 15 | 6 | 5 | 4 | 5094 |
| graphql | 24 | 4 | 2 | 18 | 11541 |
| grpc | 6 | 6 | 0 | 0 | 850 |
| rest | 102 | 17 | 59 | 26 | 7546 |
| soap | 15 | 15 | 0 | 0 | 1101 |

## Per domain

| Domain | Calls | LLM | Repaired | Fallback | p95 ms |
| --- | --- | --- | --- | --- | --- |
| devtools | 27 | 4 | 9 | 14 | 10463 |
| ecommerce | 48 | 18 | 19 | 11 | 8023 |
| generic | 18 | 18 | 0 | 0 | 1101 |
| payments | 21 | 2 | 10 | 9 | 4796 |
| social-media | 30 | 2 | 23 | 5 | 6903 |
| travel | 18 | 4 | 5 | 9 | 8902 |
