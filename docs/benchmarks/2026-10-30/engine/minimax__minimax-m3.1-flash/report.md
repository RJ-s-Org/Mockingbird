# Mockingbird LLM evals

Model `minimax/minimax-m3.1-flash` at `http://127.0.0.1:58482/r/minimax__minimax-m3.1-flash~engine/api/v1`, started 2026-10-30T07:43:56.360413+00:00.

| Metric | Value |
| --- | --- |
| Calls | 162 |
| Schema-valid rate | 95.1% |
| First-pass rate | 58.6% |
| Repair rate | 36.4% |
| Fallback rate | 4.9% |
| p50 latency | 3256 ms |
| p95 latency | 28907 ms |
| Prompt tokens per call | 1335.6 |
| Completion tokens per call | 900.4 |
| Streaming operations not evaluated | 3 |

## Per contract

| Contract | Calls | LLM | Repaired | Fallback | p95 ms |
| --- | --- | --- | --- | --- | --- |
| asyncapi/commerce-events-kafka | 9 | 4 | 5 | 0 | 29535 |
| asyncapi/orders-amqp-2.6 | 6 | 6 | 0 | 0 | 3049 |
| graphql/devtools-github | 12 | 8 | 1 | 3 | 34399 |
| graphql/ecommerce-shopify-admin | 12 | 10 | 1 | 1 | 12324 |
| grpc/otlp-trace-service | 3 | 3 | 0 | 0 | 2399 |
| grpc/route-guide | 3 | 3 | 0 | 0 | 431 |
| rest/devtools-github | 12 | 4 | 6 | 2 | 28214 |
| rest/ecommerce-orders-3.1 | 9 | 9 | 0 | 0 | 16587 |
| rest/ecommerce-square | 12 | 9 | 3 | 0 | 40141 |
| rest/payments-paypal-orders | 9 | 0 | 9 | 0 | 9421 |
| rest/payments-stripe | 12 | 5 | 7 | 0 | 3950 |
| rest/social-discord | 9 | 1 | 7 | 1 | 60007 |
| rest/social-spotify | 9 | 3 | 6 | 0 | 46288 |
| rest/social-tmdb | 12 | 5 | 7 | 0 | 55089 |
| rest/travel-booking | 12 | 6 | 6 | 0 | 28636 |
| rest/travel-hotels | 6 | 5 | 0 | 1 | 26919 |
| soap/accounts-rpc | 6 | 5 | 1 | 0 | 3708 |
| soap/calculator | 9 | 9 | 0 | 0 | 6894 |

## Per API type

| API type | Calls | LLM | Repaired | Fallback | p95 ms |
| --- | --- | --- | --- | --- | --- |
| asyncapi | 15 | 10 | 5 | 0 | 29535 |
| graphql | 24 | 18 | 2 | 4 | 13623 |
| grpc | 6 | 6 | 0 | 0 | 2399 |
| rest | 102 | 47 | 51 | 4 | 31265 |
| soap | 15 | 14 | 1 | 0 | 6894 |

## Per domain

| Domain | Calls | LLM | Repaired | Fallback | p95 ms |
| --- | --- | --- | --- | --- | --- |
| devtools | 27 | 15 | 7 | 5 | 28214 |
| ecommerce | 48 | 38 | 9 | 1 | 29535 |
| generic | 18 | 17 | 1 | 0 | 6894 |
| payments | 21 | 5 | 16 | 0 | 8966 |
| social-media | 30 | 9 | 20 | 1 | 55089 |
| travel | 18 | 11 | 6 | 1 | 28636 |
