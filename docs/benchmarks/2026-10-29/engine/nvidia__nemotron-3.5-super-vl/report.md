# Mockingbird LLM evals

Model `nvidia/nemotron-3.5-super-vl` at `http://127.0.0.1:62384/r/nvidia__nemotron-3.5-super-vl~engine/api/v1`, started 2026-10-29T18:14:04.950679+00:00.

| Metric | Value |
| --- | --- |
| Calls | 162 |
| Schema-valid rate | 85.8% |
| First-pass rate | 47.5% |
| Repair rate | 38.3% |
| Fallback rate | 14.2% |
| p50 latency | 2976 ms |
| p95 latency | 15790 ms |
| Prompt tokens per call | 1349.5 |
| Completion tokens per call | 498.1 |

## Per contract

| Contract | Calls | LLM | Repaired | Fallback | p95 ms |
| --- | --- | --- | --- | --- | --- |
| asyncapi/commerce-events-kafka | 9 | 2 | 7 | 0 | 5163 |
| asyncapi/orders-amqp-2.6 | 6 | 6 | 0 | 0 | 1285 |
| graphql/devtools-github | 12 | 4 | 0 | 8 | 19738 |
| graphql/ecommerce-shopify-admin | 12 | 7 | 0 | 5 | 29211 |
| grpc/otlp-trace-service | 3 | 3 | 0 | 0 | 492 |
| grpc/route-guide | 3 | 3 | 0 | 0 | 1447 |
| rest/devtools-github | 12 | 0 | 11 | 1 | 25351 |
| rest/ecommerce-orders-3.1 | 9 | 8 | 0 | 1 | 10035 |
| rest/ecommerce-square | 12 | 7 | 5 | 0 | 11398 |
| rest/payments-paypal-orders | 9 | 0 | 8 | 1 | 16281 |
| rest/payments-stripe | 12 | 4 | 8 | 0 | 4961 |
| rest/social-discord | 9 | 1 | 5 | 3 | 15790 |
| rest/social-spotify | 9 | 0 | 6 | 3 | 14682 |
| rest/social-tmdb | 12 | 0 | 11 | 1 | 13961 |
| rest/travel-booking | 12 | 12 | 0 | 0 | 13429 |
| rest/travel-hotels | 6 | 5 | 1 | 0 | 11623 |
| soap/accounts-rpc | 6 | 6 | 0 | 0 | 1936 |
| soap/calculator | 9 | 9 | 0 | 0 | 1121 |

## Per API type

| API type | Calls | LLM | Repaired | Fallback | p95 ms |
| --- | --- | --- | --- | --- | --- |
| asyncapi | 15 | 8 | 7 | 0 | 5163 |
| graphql | 24 | 11 | 0 | 13 | 25381 |
| grpc | 6 | 6 | 0 | 0 | 1447 |
| rest | 102 | 37 | 55 | 10 | 14682 |
| soap | 15 | 15 | 0 | 0 | 1936 |

## Per domain

| Domain | Calls | LLM | Repaired | Fallback | p95 ms |
| --- | --- | --- | --- | --- | --- |
| devtools | 27 | 7 | 11 | 9 | 19738 |
| ecommerce | 48 | 30 | 12 | 6 | 15580 |
| generic | 18 | 18 | 0 | 0 | 1936 |
| payments | 21 | 4 | 16 | 1 | 15471 |
| social-media | 30 | 1 | 22 | 7 | 14769 |
| travel | 18 | 17 | 1 | 0 | 13429 |
