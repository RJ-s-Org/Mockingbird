# Mockingbird LLM evals

Model `thinkingmachines/inkling@nojson` at `http://127.0.0.1:58482/r/thinkingmachines__inkling@nojson~engine/api/v1`, started 2026-10-30T08:01:09.381856+00:00.

| Metric | Value |
| --- | --- |
| Calls | 162 |
| Schema-valid rate | 97.5% |
| First-pass rate | 50.0% |
| Repair rate | 47.5% |
| Fallback rate | 2.5% |
| p50 latency | 4453 ms |
| p95 latency | 20555 ms |
| Prompt tokens per call | 1070.3 |
| Completion tokens per call | 319.4 |
| Streaming operations not evaluated | 3 |

## Per contract

| Contract | Calls | LLM | Repaired | Fallback | p95 ms |
| --- | --- | --- | --- | --- | --- |
| asyncapi/commerce-events-kafka | 9 | 1 | 8 | 0 | 5183 |
| asyncapi/orders-amqp-2.6 | 6 | 6 | 0 | 0 | 1753 |
| graphql/devtools-github | 12 | 12 | 0 | 0 | 12038 |
| graphql/ecommerce-shopify-admin | 12 | 12 | 0 | 0 | 16498 |
| grpc/otlp-trace-service | 3 | 3 | 0 | 0 | 538 |
| grpc/route-guide | 3 | 3 | 0 | 0 | 686 |
| rest/devtools-github | 12 | 0 | 12 | 0 | 20737 |
| rest/ecommerce-orders-3.1 | 9 | 5 | 4 | 0 | 3209 |
| rest/ecommerce-square | 12 | 8 | 3 | 1 | 21348 |
| rest/payments-paypal-orders | 9 | 1 | 7 | 1 | 34118 |
| rest/payments-stripe | 12 | 1 | 11 | 0 | 6736 |
| rest/social-discord | 9 | 0 | 7 | 2 | 14163 |
| rest/social-spotify | 9 | 1 | 8 | 0 | 51814 |
| rest/social-tmdb | 12 | 0 | 12 | 0 | 26034 |
| rest/travel-booking | 12 | 10 | 2 | 0 | 12522 |
| rest/travel-hotels | 6 | 3 | 3 | 0 | 9597 |
| soap/accounts-rpc | 6 | 6 | 0 | 0 | 4301 |
| soap/calculator | 9 | 9 | 0 | 0 | 4508 |

## Per API type

| API type | Calls | LLM | Repaired | Fallback | p95 ms |
| --- | --- | --- | --- | --- | --- |
| asyncapi | 15 | 7 | 8 | 0 | 5183 |
| graphql | 24 | 24 | 0 | 0 | 13855 |
| grpc | 6 | 6 | 0 | 0 | 686 |
| rest | 102 | 29 | 69 | 4 | 26034 |
| soap | 15 | 15 | 0 | 0 | 4508 |

## Per domain

| Domain | Calls | LLM | Repaired | Fallback | p95 ms |
| --- | --- | --- | --- | --- | --- |
| devtools | 27 | 15 | 12 | 0 | 20555 |
| ecommerce | 48 | 32 | 15 | 1 | 13855 |
| generic | 18 | 18 | 0 | 0 | 4508 |
| payments | 21 | 2 | 18 | 1 | 14663 |
| social-media | 30 | 1 | 27 | 2 | 41650 |
| travel | 18 | 13 | 5 | 0 | 12522 |
