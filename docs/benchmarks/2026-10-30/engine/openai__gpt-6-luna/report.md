# Mockingbird LLM evals

Model `openai/gpt-6-luna` at `http://127.0.0.1:58482/r/openai__gpt-6-luna~engine/api/v1`, started 2026-10-30T07:34:02.080073+00:00.

| Metric | Value |
| --- | --- |
| Calls | 162 |
| Schema-valid rate | 100.0% |
| First-pass rate | 85.2% |
| Repair rate | 14.8% |
| Fallback rate | 0.0% |
| p50 latency | 2668 ms |
| p95 latency | 7240 ms |
| Prompt tokens per call | 912.4 |
| Completion tokens per call | 255.8 |
| Streaming operations not evaluated | 3 |

## Per contract

| Contract | Calls | LLM | Repaired | Fallback | p95 ms |
| --- | --- | --- | --- | --- | --- |
| asyncapi/commerce-events-kafka | 9 | 9 | 0 | 0 | 5740 |
| asyncapi/orders-amqp-2.6 | 6 | 6 | 0 | 0 | 1556 |
| graphql/devtools-github | 12 | 12 | 0 | 0 | 6019 |
| graphql/ecommerce-shopify-admin | 12 | 12 | 0 | 0 | 11731 |
| grpc/otlp-trace-service | 3 | 3 | 0 | 0 | 1355 |
| grpc/route-guide | 3 | 3 | 0 | 0 | 1558 |
| rest/devtools-github | 12 | 9 | 3 | 0 | 9317 |
| rest/ecommerce-orders-3.1 | 9 | 9 | 0 | 0 | 3250 |
| rest/ecommerce-square | 12 | 11 | 1 | 0 | 5038 |
| rest/payments-paypal-orders | 9 | 4 | 5 | 0 | 5547 |
| rest/payments-stripe | 12 | 11 | 1 | 0 | 3451 |
| rest/social-discord | 9 | 5 | 4 | 0 | 4050 |
| rest/social-spotify | 9 | 2 | 7 | 0 | 10721 |
| rest/social-tmdb | 12 | 9 | 3 | 0 | 7699 |
| rest/travel-booking | 12 | 12 | 0 | 0 | 5780 |
| rest/travel-hotels | 6 | 6 | 0 | 0 | 1904 |
| soap/accounts-rpc | 6 | 6 | 0 | 0 | 1693 |
| soap/calculator | 9 | 9 | 0 | 0 | 1977 |

## Per API type

| API type | Calls | LLM | Repaired | Fallback | p95 ms |
| --- | --- | --- | --- | --- | --- |
| asyncapi | 15 | 15 | 0 | 0 | 5740 |
| graphql | 24 | 24 | 0 | 0 | 9589 |
| grpc | 6 | 6 | 0 | 0 | 1558 |
| rest | 102 | 78 | 24 | 0 | 7699 |
| soap | 15 | 15 | 0 | 0 | 1977 |

## Per domain

| Domain | Calls | LLM | Repaired | Fallback | p95 ms |
| --- | --- | --- | --- | --- | --- |
| devtools | 27 | 24 | 3 | 0 | 7240 |
| ecommerce | 48 | 47 | 1 | 0 | 5740 |
| generic | 18 | 18 | 0 | 0 | 1977 |
| payments | 21 | 15 | 6 | 0 | 5530 |
| social-media | 30 | 16 | 14 | 0 | 10316 |
| travel | 18 | 18 | 0 | 0 | 5780 |
