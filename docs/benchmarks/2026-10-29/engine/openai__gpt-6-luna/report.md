# Mockingbird LLM evals

Model `openai/gpt-6-luna` at `http://127.0.0.1:62384/r/openai__gpt-6-luna~engine/api/v1`, started 2026-10-29T18:10:38.705491+00:00.

| Metric | Value |
| --- | --- |
| Calls | 162 |
| Schema-valid rate | 93.8% |
| First-pass rate | 74.1% |
| Repair rate | 19.8% |
| Fallback rate | 6.2% |
| p50 latency | 2702 ms |
| p95 latency | 9161 ms |
| Prompt tokens per call | 1331.4 |
| Completion tokens per call | 299.7 |

## Per contract

| Contract | Calls | LLM | Repaired | Fallback | p95 ms |
| --- | --- | --- | --- | --- | --- |
| asyncapi/commerce-events-kafka | 9 | 7 | 2 | 0 | 3350 |
| asyncapi/orders-amqp-2.6 | 6 | 6 | 0 | 0 | 2218 |
| graphql/devtools-github | 12 | 9 | 0 | 3 | 12091 |
| graphql/ecommerce-shopify-admin | 12 | 10 | 0 | 2 | 16471 |
| grpc/otlp-trace-service | 3 | 3 | 0 | 0 | 1271 |
| grpc/route-guide | 3 | 3 | 0 | 0 | 931 |
| rest/devtools-github | 12 | 8 | 4 | 0 | 10011 |
| rest/ecommerce-orders-3.1 | 9 | 6 | 1 | 2 | 4000 |
| rest/ecommerce-square | 12 | 10 | 2 | 0 | 4222 |
| rest/payments-paypal-orders | 9 | 3 | 6 | 0 | 5906 |
| rest/payments-stripe | 12 | 11 | 1 | 0 | 3270 |
| rest/social-discord | 9 | 3 | 3 | 3 | 3935 |
| rest/social-spotify | 9 | 0 | 9 | 0 | 10243 |
| rest/social-tmdb | 12 | 10 | 2 | 0 | 6294 |
| rest/travel-booking | 12 | 12 | 0 | 0 | 6585 |
| rest/travel-hotels | 6 | 4 | 2 | 0 | 6826 |
| soap/accounts-rpc | 6 | 6 | 0 | 0 | 1567 |
| soap/calculator | 9 | 9 | 0 | 0 | 1346 |

## Per API type

| API type | Calls | LLM | Repaired | Fallback | p95 ms |
| --- | --- | --- | --- | --- | --- |
| asyncapi | 15 | 13 | 2 | 0 | 3350 |
| graphql | 24 | 19 | 0 | 5 | 12091 |
| grpc | 6 | 6 | 0 | 0 | 1271 |
| rest | 102 | 67 | 30 | 5 | 8485 |
| soap | 15 | 15 | 0 | 0 | 1567 |

## Per domain

| Domain | Calls | LLM | Repaired | Fallback | p95 ms |
| --- | --- | --- | --- | --- | --- |
| devtools | 27 | 20 | 4 | 3 | 11203 |
| ecommerce | 48 | 39 | 5 | 4 | 4485 |
| generic | 18 | 18 | 0 | 0 | 1567 |
| payments | 21 | 14 | 7 | 0 | 5601 |
| social-media | 30 | 13 | 14 | 3 | 9452 |
| travel | 18 | 16 | 2 | 0 | 6826 |
