# Mockingbird LLM evals

Model `nvidia/nemotron-3.5-lightning` at `http://127.0.0.1:52106/r/nvidia__nemotron-3.5-lightning~engine/api/v1`, started 2026-11-04T14:32:05.662632+00:00.

| Metric | Value |
| --- | --- |
| Calls | 162 |
| Schema-valid rate | 96.3% |
| First-pass rate | 83.3% |
| Repair rate | 13.0% |
| Fallback rate | 3.7% |
| p50 latency | 1973 ms |
| p95 latency | 16638 ms |
| Prompt tokens per call | 1587.1 |
| Completion tokens per call | 646.0 |
| Streaming operations not evaluated | 3 |

## Per contract

| Contract | Calls | LLM | Repaired | Fallback | p95 ms |
| --- | --- | --- | --- | --- | --- |
| asyncapi/commerce-events-kafka | 9 | 9 | 0 | 0 | 2280 |
| asyncapi/orders-amqp-2.6 | 6 | 6 | 0 | 0 | 942 |
| graphql/devtools-github | 12 | 12 | 0 | 0 | 1974 |
| graphql/ecommerce-shopify-admin | 12 | 12 | 0 | 0 | 11573 |
| grpc/otlp-trace-service | 3 | 3 | 0 | 0 | 3225 |
| grpc/route-guide | 3 | 3 | 0 | 0 | 643 |
| rest/devtools-github | 12 | 12 | 0 | 0 | 6756 |
| rest/ecommerce-orders-3.1 | 9 | 6 | 2 | 1 | 60004 |
| rest/ecommerce-square | 12 | 11 | 1 | 0 | 6189 |
| rest/payments-paypal-orders | 9 | 7 | 1 | 1 | 60004 |
| rest/payments-stripe | 12 | 10 | 2 | 0 | 11786 |
| rest/social-discord | 9 | 2 | 5 | 2 | 28669 |
| rest/social-spotify | 9 | 7 | 2 | 0 | 20421 |
| rest/social-tmdb | 12 | 9 | 3 | 0 | 16573 |
| rest/travel-booking | 12 | 5 | 5 | 2 | 17452 |
| rest/travel-hotels | 6 | 6 | 0 | 0 | 1694 |
| soap/accounts-rpc | 6 | 6 | 0 | 0 | 1149 |
| soap/calculator | 9 | 9 | 0 | 0 | 11087 |

## Per API type

| API type | Calls | LLM | Repaired | Fallback | p95 ms |
| --- | --- | --- | --- | --- | --- |
| asyncapi | 15 | 15 | 0 | 0 | 2280 |
| graphql | 24 | 24 | 0 | 0 | 11546 |
| grpc | 6 | 6 | 0 | 0 | 3225 |
| rest | 102 | 75 | 21 | 6 | 20421 |
| soap | 15 | 15 | 0 | 0 | 11087 |

## Per domain

| Domain | Calls | LLM | Repaired | Fallback | p95 ms |
| --- | --- | --- | --- | --- | --- |
| devtools | 27 | 27 | 0 | 0 | 5917 |
| ecommerce | 48 | 44 | 3 | 1 | 38642 |
| generic | 18 | 18 | 0 | 0 | 11087 |
| payments | 21 | 17 | 3 | 1 | 14258 |
| social-media | 30 | 18 | 10 | 2 | 20421 |
| travel | 18 | 11 | 5 | 2 | 17452 |
