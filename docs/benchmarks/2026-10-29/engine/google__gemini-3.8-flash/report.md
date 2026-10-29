# Mockingbird LLM evals

Model `google/gemini-3.8-flash` at `http://127.0.0.1:58357/r/google__gemini-3.8-flash~engine/api/v1`, started 2026-10-29T19:34:43.109735+00:00.

| Metric | Value |
| --- | --- |
| Calls | 162 |
| Schema-valid rate | 67.3% |
| First-pass rate | 66.0% |
| Repair rate | 1.2% |
| Fallback rate | 32.7% |
| p50 latency | 9488 ms |
| p95 latency | 21294 ms |
| Prompt tokens per call | 740.9 |
| Completion tokens per call | 710.2 |

## Per contract

| Contract | Calls | LLM | Repaired | Fallback | p95 ms |
| --- | --- | --- | --- | --- | --- |
| asyncapi/commerce-events-kafka | 9 | 9 | 0 | 0 | 13010 |
| asyncapi/orders-amqp-2.6 | 6 | 6 | 0 | 0 | 5788 |
| graphql/devtools-github | 12 | 7 | 0 | 5 | 42266 |
| graphql/ecommerce-shopify-admin | 12 | 6 | 0 | 6 | 28978 |
| grpc/otlp-trace-service | 3 | 3 | 0 | 0 | 10377 |
| grpc/route-guide | 3 | 3 | 0 | 0 | 3212 |
| rest/devtools-github | 12 | 4 | 1 | 7 | 24759 |
| rest/ecommerce-orders-3.1 | 9 | 9 | 0 | 0 | 8821 |
| rest/ecommerce-square | 12 | 5 | 0 | 7 | 21294 |
| rest/payments-paypal-orders | 9 | 3 | 0 | 6 | 15850 |
| rest/payments-stripe | 12 | 9 | 1 | 2 | 16242 |
| rest/social-discord | 9 | 7 | 0 | 2 | 11779 |
| rest/social-spotify | 9 | 1 | 0 | 8 | 25977 |
| rest/social-tmdb | 12 | 9 | 0 | 3 | 14628 |
| rest/travel-booking | 12 | 6 | 0 | 6 | 27113 |
| rest/travel-hotels | 6 | 5 | 0 | 1 | 14163 |
| soap/accounts-rpc | 6 | 6 | 0 | 0 | 6873 |
| soap/calculator | 9 | 9 | 0 | 0 | 3625 |

## Per API type

| API type | Calls | LLM | Repaired | Fallback | p95 ms |
| --- | --- | --- | --- | --- | --- |
| asyncapi | 15 | 15 | 0 | 0 | 13010 |
| graphql | 24 | 13 | 0 | 11 | 28978 |
| grpc | 6 | 6 | 0 | 0 | 10377 |
| rest | 102 | 58 | 2 | 42 | 16242 |
| soap | 15 | 15 | 0 | 0 | 6873 |

## Per domain

| Domain | Calls | LLM | Repaired | Fallback | p95 ms |
| --- | --- | --- | --- | --- | --- |
| devtools | 27 | 14 | 1 | 12 | 24759 |
| ecommerce | 48 | 35 | 0 | 13 | 24413 |
| generic | 18 | 18 | 0 | 0 | 6873 |
| payments | 21 | 12 | 1 | 8 | 15850 |
| social-media | 30 | 17 | 0 | 13 | 14628 |
| travel | 18 | 11 | 0 | 7 | 27113 |
