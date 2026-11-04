# Mockingbird LLM evals

Model `nvidia/nemotron-3.5-lightning` at `http://127.0.0.1:54271/r/nvidia__nemotron-3.5-lightning~engine/api/v1`, started 2026-11-04T18:24:18.762920+00:00.

| Metric | Value |
| --- | --- |
| Calls | 162 |
| Schema-valid rate | 98.1% |
| First-pass rate | 74.7% |
| Repair rate | 23.5% |
| Fallback rate | 1.9% |
| p50 latency | 1568 ms |
| p95 latency | 51903 ms |
| Prompt tokens per call | 1286.9 |
| Completion tokens per call | 1645.5 |
| Streaming operations not evaluated | 3 |

## Per contract

| Contract | Calls | LLM | Repaired | Fallback | p95 ms |
| --- | --- | --- | --- | --- | --- |
| asyncapi/commerce-events-kafka | 9 | 9 | 0 | 0 | 4393 |
| asyncapi/orders-amqp-2.6 | 6 | 6 | 0 | 0 | 859 |
| graphql/devtools-github | 12 | 10 | 2 | 0 | 59148 |
| graphql/ecommerce-shopify-admin | 12 | 12 | 0 | 0 | 15257 |
| grpc/otlp-trace-service | 3 | 3 | 0 | 0 | 567 |
| grpc/route-guide | 3 | 3 | 0 | 0 | 518 |
| rest/devtools-github | 12 | 9 | 3 | 0 | 59511 |
| rest/ecommerce-orders-3.1 | 9 | 3 | 4 | 2 | 3212 |
| rest/ecommerce-square | 12 | 10 | 2 | 0 | 3831 |
| rest/payments-paypal-orders | 9 | 3 | 6 | 0 | 58362 |
| rest/payments-stripe | 12 | 11 | 0 | 1 | 60003 |
| rest/social-discord | 9 | 2 | 7 | 0 | 7597 |
| rest/social-spotify | 9 | 8 | 1 | 0 | 56835 |
| rest/social-tmdb | 12 | 0 | 12 | 0 | 48579 |
| rest/travel-booking | 12 | 11 | 1 | 0 | 54449 |
| rest/travel-hotels | 6 | 6 | 0 | 0 | 1311 |
| soap/accounts-rpc | 6 | 6 | 0 | 0 | 643 |
| soap/calculator | 9 | 9 | 0 | 0 | 1021 |

## Per API type

| API type | Calls | LLM | Repaired | Fallback | p95 ms |
| --- | --- | --- | --- | --- | --- |
| asyncapi | 15 | 15 | 0 | 0 | 4393 |
| graphql | 24 | 22 | 2 | 0 | 54499 |
| grpc | 6 | 6 | 0 | 0 | 567 |
| rest | 102 | 63 | 36 | 3 | 54449 |
| soap | 15 | 15 | 0 | 0 | 1021 |

## Per domain

| Domain | Calls | LLM | Repaired | Fallback | p95 ms |
| --- | --- | --- | --- | --- | --- |
| devtools | 27 | 22 | 5 | 0 | 59148 |
| ecommerce | 48 | 40 | 6 | 2 | 3831 |
| generic | 18 | 18 | 0 | 0 | 1021 |
| payments | 21 | 14 | 6 | 1 | 58362 |
| social-media | 30 | 10 | 20 | 0 | 48579 |
| travel | 18 | 17 | 1 | 0 | 54449 |
