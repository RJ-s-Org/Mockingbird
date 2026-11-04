# Mockingbird LLM evals

Model `openai/gpt-6-luna` at `http://127.0.0.1:54271/r/openai__gpt-6-luna~engine/api/v1`, started 2026-11-04T18:24:18.758772+00:00.

| Metric | Value |
| --- | --- |
| Calls | 162 |
| Schema-valid rate | 100.0% |
| First-pass rate | 86.4% |
| Repair rate | 13.6% |
| Fallback rate | 0.0% |
| p50 latency | 2847 ms |
| p95 latency | 7163 ms |
| Prompt tokens per call | 1317.9 |
| Completion tokens per call | 275.6 |
| Streaming operations not evaluated | 3 |

## Per contract

| Contract | Calls | LLM | Repaired | Fallback | p95 ms |
| --- | --- | --- | --- | --- | --- |
| asyncapi/commerce-events-kafka | 9 | 9 | 0 | 0 | 4254 |
| asyncapi/orders-amqp-2.6 | 6 | 6 | 0 | 0 | 1592 |
| graphql/devtools-github | 12 | 12 | 0 | 0 | 6944 |
| graphql/ecommerce-shopify-admin | 12 | 12 | 0 | 0 | 8525 |
| grpc/otlp-trace-service | 3 | 3 | 0 | 0 | 1429 |
| grpc/route-guide | 3 | 3 | 0 | 0 | 2913 |
| rest/devtools-github | 12 | 9 | 3 | 0 | 7252 |
| rest/ecommerce-orders-3.1 | 9 | 9 | 0 | 0 | 3708 |
| rest/ecommerce-square | 12 | 11 | 1 | 0 | 11077 |
| rest/payments-paypal-orders | 9 | 4 | 5 | 0 | 4115 |
| rest/payments-stripe | 12 | 12 | 0 | 0 | 3588 |
| rest/social-discord | 9 | 6 | 3 | 0 | 7870 |
| rest/social-spotify | 9 | 3 | 6 | 0 | 13467 |
| rest/social-tmdb | 12 | 9 | 3 | 0 | 10292 |
| rest/travel-booking | 12 | 11 | 1 | 0 | 10214 |
| rest/travel-hotels | 6 | 6 | 0 | 0 | 1908 |
| soap/accounts-rpc | 6 | 6 | 0 | 0 | 1987 |
| soap/calculator | 9 | 9 | 0 | 0 | 1306 |

## Per API type

| API type | Calls | LLM | Repaired | Fallback | p95 ms |
| --- | --- | --- | --- | --- | --- |
| asyncapi | 15 | 15 | 0 | 0 | 4254 |
| graphql | 24 | 24 | 0 | 0 | 7163 |
| grpc | 6 | 6 | 0 | 0 | 2913 |
| rest | 102 | 80 | 22 | 0 | 7551 |
| soap | 15 | 15 | 0 | 0 | 1987 |

## Per domain

| Domain | Calls | LLM | Repaired | Fallback | p95 ms |
| --- | --- | --- | --- | --- | --- |
| devtools | 27 | 24 | 3 | 0 | 6944 |
| ecommerce | 48 | 47 | 1 | 0 | 7163 |
| generic | 18 | 18 | 0 | 0 | 2913 |
| payments | 21 | 16 | 5 | 0 | 4099 |
| social-media | 30 | 18 | 12 | 0 | 10292 |
| travel | 18 | 17 | 1 | 0 | 10214 |
