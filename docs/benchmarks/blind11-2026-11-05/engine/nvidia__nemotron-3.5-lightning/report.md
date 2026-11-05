# Mockingbird LLM evals

Model `nvidia/nemotron-3.5-lightning` at `http://127.0.0.1:61040/r/nvidia__nemotron-3.5-lightning~engine/api/v1`, started 2026-11-05T18:12:48.589057+00:00.

| Metric | Value |
| --- | --- |
| Calls | 162 |
| Schema-valid rate | 100.0% |
| First-pass rate | 88.3% |
| Repair rate | 11.7% |
| Fallback rate | 0.0% |
| p50 latency | 1398 ms |
| p95 latency | 3703 ms |
| Prompt tokens per call | 1067.2 |
| Completion tokens per call | 542.9 |
| Streaming operations not evaluated | 3 |

## Per contract

| Contract | Calls | LLM | Repaired | Fallback | p95 ms |
| --- | --- | --- | --- | --- | --- |
| asyncapi/commerce-events-kafka | 9 | 9 | 0 | 0 | 2093 |
| asyncapi/orders-amqp-2.6 | 6 | 6 | 0 | 0 | 961 |
| graphql/devtools-github | 12 | 12 | 0 | 0 | 2289 |
| graphql/ecommerce-shopify-admin | 12 | 12 | 0 | 0 | 2264 |
| grpc/otlp-trace-service | 3 | 3 | 0 | 0 | 525 |
| grpc/route-guide | 3 | 3 | 0 | 0 | 435 |
| rest/devtools-github | 12 | 10 | 2 | 0 | 34677 |
| rest/ecommerce-orders-3.1 | 9 | 6 | 3 | 0 | 1797 |
| rest/ecommerce-square | 12 | 12 | 0 | 0 | 3003 |
| rest/payments-paypal-orders | 9 | 9 | 0 | 0 | 3750 |
| rest/payments-stripe | 12 | 12 | 0 | 0 | 1964 |
| rest/social-discord | 9 | 3 | 6 | 0 | 2368 |
| rest/social-spotify | 9 | 9 | 0 | 0 | 5511 |
| rest/social-tmdb | 12 | 4 | 8 | 0 | 3950 |
| rest/travel-booking | 12 | 12 | 0 | 0 | 3368 |
| rest/travel-hotels | 6 | 6 | 0 | 0 | 962 |
| soap/accounts-rpc | 6 | 6 | 0 | 0 | 1391 |
| soap/calculator | 9 | 9 | 0 | 0 | 986 |

## Per API type

| API type | Calls | LLM | Repaired | Fallback | p95 ms |
| --- | --- | --- | --- | --- | --- |
| asyncapi | 15 | 15 | 0 | 0 | 2093 |
| graphql | 24 | 24 | 0 | 0 | 2264 |
| grpc | 6 | 6 | 0 | 0 | 525 |
| rest | 102 | 83 | 19 | 0 | 3950 |
| soap | 15 | 15 | 0 | 0 | 1391 |

## Per domain

| Domain | Calls | LLM | Repaired | Fallback | p95 ms |
| --- | --- | --- | --- | --- | --- |
| devtools | 27 | 25 | 2 | 0 | 33636 |
| ecommerce | 48 | 45 | 3 | 0 | 2264 |
| generic | 18 | 18 | 0 | 0 | 1391 |
| payments | 21 | 21 | 0 | 0 | 2096 |
| social-media | 30 | 16 | 14 | 0 | 4231 |
| travel | 18 | 18 | 0 | 0 | 3368 |
