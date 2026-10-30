# Mockingbird LLM evals

Model `nvidia/nemotron-3.5-lightning` at `http://127.0.0.1:60496/r/nvidia__nemotron-3.5-lightning~engine/api/v1`, started 2026-10-30T22:25:34.304266+00:00.

| Metric | Value |
| --- | --- |
| Calls | 162 |
| Schema-valid rate | 90.7% |
| First-pass rate | 76.5% |
| Repair rate | 14.2% |
| Fallback rate | 9.3% |
| p50 latency | 3636 ms |
| p95 latency | 60003 ms |
| Prompt tokens per call | 1309.3 |
| Completion tokens per call | 403.1 |
| Streaming operations not evaluated | 3 |

## Per contract

| Contract | Calls | LLM | Repaired | Fallback | p95 ms |
| --- | --- | --- | --- | --- | --- |
| asyncapi/commerce-events-kafka | 9 | 8 | 1 | 0 | 7941 |
| asyncapi/orders-amqp-2.6 | 6 | 5 | 0 | 1 | 5168 |
| graphql/devtools-github | 12 | 12 | 0 | 0 | 50400 |
| graphql/ecommerce-shopify-admin | 12 | 12 | 0 | 0 | 9512 |
| grpc/otlp-trace-service | 3 | 2 | 1 | 0 | 12419 |
| grpc/route-guide | 3 | 3 | 0 | 0 | 1255 |
| rest/devtools-github | 12 | 4 | 1 | 7 | 86184 |
| rest/ecommerce-orders-3.1 | 9 | 9 | 0 | 0 | 5730 |
| rest/ecommerce-square | 12 | 12 | 0 | 0 | 12496 |
| rest/payments-paypal-orders | 9 | 5 | 2 | 2 | 60007 |
| rest/payments-stripe | 12 | 12 | 0 | 0 | 2272 |
| rest/social-discord | 9 | 2 | 7 | 0 | 48890 |
| rest/social-spotify | 9 | 5 | 4 | 0 | 24450 |
| rest/social-tmdb | 12 | 11 | 1 | 0 | 10850 |
| rest/travel-booking | 12 | 2 | 6 | 4 | 60003 |
| rest/travel-hotels | 6 | 5 | 0 | 1 | 21046 |
| soap/accounts-rpc | 6 | 6 | 0 | 0 | 2008 |
| soap/calculator | 9 | 9 | 0 | 0 | 2031 |

## Per API type

| API type | Calls | LLM | Repaired | Fallback | p95 ms |
| --- | --- | --- | --- | --- | --- |
| asyncapi | 15 | 13 | 1 | 1 | 7941 |
| graphql | 24 | 24 | 0 | 0 | 43955 |
| grpc | 6 | 5 | 1 | 0 | 12419 |
| rest | 102 | 67 | 21 | 14 | 60004 |
| soap | 15 | 15 | 0 | 0 | 2031 |

## Per domain

| Domain | Calls | LLM | Repaired | Fallback | p95 ms |
| --- | --- | --- | --- | --- | --- |
| devtools | 27 | 18 | 2 | 7 | 60008 |
| ecommerce | 48 | 46 | 1 | 1 | 8237 |
| generic | 18 | 18 | 0 | 0 | 2031 |
| payments | 21 | 17 | 2 | 2 | 60002 |
| social-media | 30 | 18 | 12 | 0 | 24450 |
| travel | 18 | 7 | 6 | 5 | 60003 |
