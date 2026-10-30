# Mockingbird LLM evals

Model `deepseek/deepseek-v4.1-flash-1017` at `http://127.0.0.1:58482/r/deepseek__deepseek-v4.1-flash-1017~engine/api/v1`, started 2026-10-30T07:30:46.012966+00:00.

| Metric | Value |
| --- | --- |
| Calls | 162 |
| Schema-valid rate | 99.4% |
| First-pass rate | 66.0% |
| Repair rate | 33.3% |
| Fallback rate | 0.6% |
| p50 latency | 1883 ms |
| p95 latency | 7277 ms |
| Prompt tokens per call | 1014.3 |
| Completion tokens per call | 430.9 |
| Streaming operations not evaluated | 3 |

## Per contract

| Contract | Calls | LLM | Repaired | Fallback | p95 ms |
| --- | --- | --- | --- | --- | --- |
| asyncapi/commerce-events-kafka | 9 | 6 | 3 | 0 | 2773 |
| asyncapi/orders-amqp-2.6 | 6 | 6 | 0 | 0 | 2128 |
| graphql/devtools-github | 12 | 12 | 0 | 0 | 7669 |
| graphql/ecommerce-shopify-admin | 12 | 11 | 1 | 0 | 10125 |
| grpc/otlp-trace-service | 3 | 3 | 0 | 0 | 778 |
| grpc/route-guide | 3 | 3 | 0 | 0 | 521 |
| rest/devtools-github | 12 | 0 | 11 | 1 | 8470 |
| rest/ecommerce-orders-3.1 | 9 | 9 | 0 | 0 | 1542 |
| rest/ecommerce-square | 12 | 6 | 6 | 0 | 7955 |
| rest/payments-paypal-orders | 9 | 3 | 6 | 0 | 5309 |
| rest/payments-stripe | 12 | 3 | 9 | 0 | 2343 |
| rest/social-discord | 9 | 5 | 4 | 0 | 4236 |
| rest/social-spotify | 9 | 5 | 4 | 0 | 7277 |
| rest/social-tmdb | 12 | 2 | 10 | 0 | 7239 |
| rest/travel-booking | 12 | 12 | 0 | 0 | 7473 |
| rest/travel-hotels | 6 | 6 | 0 | 0 | 5812 |
| soap/accounts-rpc | 6 | 6 | 0 | 0 | 1460 |
| soap/calculator | 9 | 9 | 0 | 0 | 851 |

## Per API type

| API type | Calls | LLM | Repaired | Fallback | p95 ms |
| --- | --- | --- | --- | --- | --- |
| asyncapi | 15 | 12 | 3 | 0 | 2773 |
| graphql | 24 | 23 | 1 | 0 | 9444 |
| grpc | 6 | 6 | 0 | 0 | 778 |
| rest | 102 | 51 | 50 | 1 | 7239 |
| soap | 15 | 15 | 0 | 0 | 1460 |

## Per domain

| Domain | Calls | LLM | Repaired | Fallback | p95 ms |
| --- | --- | --- | --- | --- | --- |
| devtools | 27 | 15 | 11 | 1 | 8163 |
| ecommerce | 48 | 38 | 10 | 0 | 7955 |
| generic | 18 | 18 | 0 | 0 | 1460 |
| payments | 21 | 6 | 15 | 0 | 4128 |
| social-media | 30 | 12 | 18 | 0 | 7239 |
| travel | 18 | 18 | 0 | 0 | 7473 |
