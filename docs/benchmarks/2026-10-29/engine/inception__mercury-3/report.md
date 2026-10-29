# Mockingbird LLM evals

Model `inception/mercury-3` at `http://127.0.0.1:62384/r/inception__mercury-3~engine/api/v1`, started 2026-10-29T18:07:16.631604+00:00.

| Metric | Value |
| --- | --- |
| Calls | 162 |
| Schema-valid rate | 85.8% |
| First-pass rate | 40.1% |
| Repair rate | 45.7% |
| Fallback rate | 14.2% |
| p50 latency | 1099 ms |
| p95 latency | 4167 ms |
| Prompt tokens per call | 1330.1 |
| Completion tokens per call | 462.0 |

## Per contract

| Contract | Calls | LLM | Repaired | Fallback | p95 ms |
| --- | --- | --- | --- | --- | --- |
| asyncapi/commerce-events-kafka | 9 | 0 | 9 | 0 | 890 |
| asyncapi/orders-amqp-2.6 | 6 | 6 | 0 | 0 | 660 |
| graphql/devtools-github | 12 | 3 | 0 | 9 | 5415 |
| graphql/ecommerce-shopify-admin | 12 | 6 | 0 | 6 | 5217 |
| grpc/otlp-trace-service | 3 | 3 | 0 | 0 | 553 |
| grpc/route-guide | 3 | 3 | 0 | 0 | 806 |
| rest/devtools-github | 12 | 1 | 11 | 0 | 2953 |
| rest/ecommerce-orders-3.1 | 9 | 8 | 1 | 0 | 1150 |
| rest/ecommerce-square | 12 | 6 | 4 | 2 | 4314 |
| rest/payments-paypal-orders | 9 | 0 | 8 | 1 | 2869 |
| rest/payments-stripe | 12 | 1 | 11 | 0 | 1237 |
| rest/social-discord | 9 | 0 | 9 | 0 | 1889 |
| rest/social-spotify | 9 | 0 | 7 | 2 | 4201 |
| rest/social-tmdb | 12 | 1 | 10 | 1 | 2742 |
| rest/travel-booking | 12 | 12 | 0 | 0 | 1742 |
| rest/travel-hotels | 6 | 2 | 4 | 0 | 3207 |
| soap/accounts-rpc | 6 | 4 | 0 | 2 | 1048 |
| soap/calculator | 9 | 9 | 0 | 0 | 641 |

## Per API type

| API type | Calls | LLM | Repaired | Fallback | p95 ms |
| --- | --- | --- | --- | --- | --- |
| asyncapi | 15 | 6 | 9 | 0 | 890 |
| graphql | 24 | 9 | 0 | 15 | 5217 |
| grpc | 6 | 6 | 0 | 0 | 806 |
| rest | 102 | 31 | 65 | 6 | 2953 |
| soap | 15 | 13 | 0 | 2 | 1048 |

## Per domain

| Domain | Calls | LLM | Repaired | Fallback | p95 ms |
| --- | --- | --- | --- | --- | --- |
| devtools | 27 | 7 | 11 | 9 | 4777 |
| ecommerce | 48 | 26 | 14 | 8 | 4314 |
| generic | 18 | 16 | 0 | 2 | 1048 |
| payments | 21 | 1 | 19 | 1 | 2122 |
| social-media | 30 | 1 | 26 | 3 | 3573 |
| travel | 18 | 14 | 4 | 0 | 3207 |
