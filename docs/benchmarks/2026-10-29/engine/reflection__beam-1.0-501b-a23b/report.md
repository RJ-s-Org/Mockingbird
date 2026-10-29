# Mockingbird LLM evals

Model `reflection/beam-1.0-501b-a23b` at `http://127.0.0.1:58357/r/reflection__beam-1.0-501b-a23b~engine/api/v1`, started 2026-10-29T19:34:43.110368+00:00.

| Metric | Value |
| --- | --- |
| Calls | 162 |
| Schema-valid rate | 51.2% |
| First-pass rate | 46.9% |
| Repair rate | 4.3% |
| Fallback rate | 48.8% |
| p50 latency | 4883 ms |
| p95 latency | 11546 ms |
| Prompt tokens per call | 463.9 |
| Completion tokens per call | 507.2 |

## Per contract

| Contract | Calls | LLM | Repaired | Fallback | p95 ms |
| --- | --- | --- | --- | --- | --- |
| asyncapi/commerce-events-kafka | 9 | 8 | 1 | 0 | 8558 |
| asyncapi/orders-amqp-2.6 | 6 | 6 | 0 | 0 | 3438 |
| graphql/devtools-github | 12 | 5 | 0 | 7 | 11809 |
| graphql/ecommerce-shopify-admin | 12 | 5 | 0 | 7 | 11971 |
| grpc/otlp-trace-service | 3 | 3 | 0 | 0 | 4883 |
| grpc/route-guide | 3 | 3 | 0 | 0 | 2263 |
| rest/devtools-github | 12 | 1 | 1 | 10 | 8793 |
| rest/ecommerce-orders-3.1 | 9 | 8 | 0 | 1 | 12069 |
| rest/ecommerce-square | 12 | 3 | 1 | 8 | 12600 |
| rest/payments-paypal-orders | 9 | 0 | 0 | 9 | 13740 |
| rest/payments-stripe | 12 | 8 | 3 | 1 | 8508 |
| rest/social-discord | 9 | 3 | 1 | 5 | 9453 |
| rest/social-spotify | 9 | 0 | 0 | 9 | 9361 |
| rest/social-tmdb | 12 | 5 | 0 | 7 | 11389 |
| rest/travel-booking | 12 | 3 | 0 | 9 | 12601 |
| rest/travel-hotels | 6 | 3 | 0 | 3 | 10024 |
| soap/accounts-rpc | 6 | 4 | 0 | 2 | 9560 |
| soap/calculator | 9 | 8 | 0 | 1 | 3301 |

## Per API type

| API type | Calls | LLM | Repaired | Fallback | p95 ms |
| --- | --- | --- | --- | --- | --- |
| asyncapi | 15 | 14 | 1 | 0 | 8558 |
| graphql | 24 | 10 | 0 | 14 | 11809 |
| grpc | 6 | 6 | 0 | 0 | 4883 |
| rest | 102 | 34 | 6 | 62 | 12069 |
| soap | 15 | 12 | 0 | 3 | 9560 |

## Per domain

| Domain | Calls | LLM | Repaired | Fallback | p95 ms |
| --- | --- | --- | --- | --- | --- |
| devtools | 27 | 9 | 1 | 17 | 9217 |
| ecommerce | 48 | 30 | 2 | 16 | 12093 |
| generic | 18 | 15 | 0 | 3 | 9560 |
| payments | 21 | 8 | 3 | 10 | 11546 |
| social-media | 30 | 8 | 1 | 21 | 9492 |
| travel | 18 | 6 | 0 | 12 | 12601 |
