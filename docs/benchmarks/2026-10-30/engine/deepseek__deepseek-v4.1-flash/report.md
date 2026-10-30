# Mockingbird LLM evals

Model `deepseek/deepseek-v4.1-flash` at `http://127.0.0.1:58482/r/deepseek__deepseek-v4.1-flash~engine/api/v1`, started 2026-10-30T07:38:43.245818+00:00.

| Metric | Value |
| --- | --- |
| Calls | 162 |
| Schema-valid rate | 98.8% |
| First-pass rate | 67.9% |
| Repair rate | 30.9% |
| Fallback rate | 1.2% |
| p50 latency | 3524 ms |
| p95 latency | 9347 ms |
| Prompt tokens per call | 984.9 |
| Completion tokens per call | 444.5 |
| Streaming operations not evaluated | 3 |

## Per contract

| Contract | Calls | LLM | Repaired | Fallback | p95 ms |
| --- | --- | --- | --- | --- | --- |
| asyncapi/commerce-events-kafka | 9 | 6 | 3 | 0 | 6270 |
| asyncapi/orders-amqp-2.6 | 6 | 6 | 0 | 0 | 5343 |
| graphql/devtools-github | 12 | 12 | 0 | 0 | 6070 |
| graphql/ecommerce-shopify-admin | 12 | 12 | 0 | 0 | 9972 |
| grpc/otlp-trace-service | 3 | 3 | 0 | 0 | 5893 |
| grpc/route-guide | 3 | 3 | 0 | 0 | 6687 |
| rest/devtools-github | 12 | 0 | 11 | 1 | 9423 |
| rest/ecommerce-orders-3.1 | 9 | 9 | 0 | 0 | 5572 |
| rest/ecommerce-square | 12 | 8 | 4 | 0 | 6309 |
| rest/payments-paypal-orders | 9 | 4 | 5 | 0 | 5577 |
| rest/payments-stripe | 12 | 1 | 11 | 0 | 2342 |
| rest/social-discord | 9 | 5 | 4 | 0 | 5014 |
| rest/social-spotify | 9 | 6 | 3 | 0 | 9927 |
| rest/social-tmdb | 12 | 2 | 9 | 1 | 11999 |
| rest/travel-booking | 12 | 12 | 0 | 0 | 15865 |
| rest/travel-hotels | 6 | 6 | 0 | 0 | 9020 |
| soap/accounts-rpc | 6 | 6 | 0 | 0 | 5010 |
| soap/calculator | 9 | 9 | 0 | 0 | 5235 |

## Per API type

| API type | Calls | LLM | Repaired | Fallback | p95 ms |
| --- | --- | --- | --- | --- | --- |
| asyncapi | 15 | 12 | 3 | 0 | 6270 |
| graphql | 24 | 24 | 0 | 0 | 9443 |
| grpc | 6 | 6 | 0 | 0 | 6687 |
| rest | 102 | 53 | 47 | 2 | 9412 |
| soap | 15 | 15 | 0 | 0 | 5235 |

## Per domain

| Domain | Calls | LLM | Repaired | Fallback | p95 ms |
| --- | --- | --- | --- | --- | --- |
| devtools | 27 | 15 | 11 | 1 | 8016 |
| ecommerce | 48 | 41 | 7 | 0 | 6309 |
| generic | 18 | 18 | 0 | 0 | 6687 |
| payments | 21 | 5 | 16 | 0 | 4876 |
| social-media | 30 | 13 | 16 | 1 | 9927 |
| travel | 18 | 18 | 0 | 0 | 15865 |
