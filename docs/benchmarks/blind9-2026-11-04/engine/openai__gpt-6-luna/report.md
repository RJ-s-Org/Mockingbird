# Mockingbird LLM evals

Model `openai/gpt-6-luna` at `http://127.0.0.1:52106/r/openai__gpt-6-luna~engine/api/v1`, started 2026-11-04T14:32:05.662626+00:00.

| Metric | Value |
| --- | --- |
| Calls | 162 |
| Schema-valid rate | 99.4% |
| First-pass rate | 82.7% |
| Repair rate | 16.7% |
| Fallback rate | 0.6% |
| p50 latency | 2278 ms |
| p95 latency | 8127 ms |
| Prompt tokens per call | 1412.8 |
| Completion tokens per call | 272.0 |
| Streaming operations not evaluated | 3 |

## Per contract

| Contract | Calls | LLM | Repaired | Fallback | p95 ms |
| --- | --- | --- | --- | --- | --- |
| asyncapi/commerce-events-kafka | 9 | 9 | 0 | 0 | 3545 |
| asyncapi/orders-amqp-2.6 | 6 | 6 | 0 | 0 | 1555 |
| graphql/devtools-github | 12 | 12 | 0 | 0 | 5192 |
| graphql/ecommerce-shopify-admin | 12 | 12 | 0 | 0 | 10999 |
| grpc/otlp-trace-service | 3 | 3 | 0 | 0 | 1533 |
| grpc/route-guide | 3 | 3 | 0 | 0 | 4292 |
| rest/devtools-github | 12 | 6 | 6 | 0 | 5930 |
| rest/ecommerce-orders-3.1 | 9 | 9 | 0 | 0 | 2904 |
| rest/ecommerce-square | 12 | 9 | 3 | 0 | 6525 |
| rest/payments-paypal-orders | 9 | 3 | 6 | 0 | 8738 |
| rest/payments-stripe | 12 | 12 | 0 | 0 | 3702 |
| rest/social-discord | 9 | 6 | 3 | 0 | 5222 |
| rest/social-spotify | 9 | 3 | 5 | 1 | 60010 |
| rest/social-tmdb | 12 | 8 | 4 | 0 | 10361 |
| rest/travel-booking | 12 | 12 | 0 | 0 | 3000 |
| rest/travel-hotels | 6 | 6 | 0 | 0 | 1927 |
| soap/accounts-rpc | 6 | 6 | 0 | 0 | 1898 |
| soap/calculator | 9 | 9 | 0 | 0 | 1235 |

## Per API type

| API type | Calls | LLM | Repaired | Fallback | p95 ms |
| --- | --- | --- | --- | --- | --- |
| asyncapi | 15 | 15 | 0 | 0 | 3545 |
| graphql | 24 | 24 | 0 | 0 | 5946 |
| grpc | 6 | 6 | 0 | 0 | 4292 |
| rest | 102 | 74 | 27 | 1 | 9179 |
| soap | 15 | 15 | 0 | 0 | 1898 |

## Per domain

| Domain | Calls | LLM | Repaired | Fallback | p95 ms |
| --- | --- | --- | --- | --- | --- |
| devtools | 27 | 21 | 6 | 0 | 5348 |
| ecommerce | 48 | 45 | 3 | 0 | 6161 |
| generic | 18 | 18 | 0 | 0 | 4292 |
| payments | 21 | 15 | 6 | 0 | 4740 |
| social-media | 30 | 17 | 12 | 1 | 21951 |
| travel | 18 | 18 | 0 | 0 | 3000 |
