# Mockingbird LLM evals

Model `deepseek/deepseek-v4.1-flash-1017` at `http://127.0.0.1:52106/r/deepseek__deepseek-v4.1-flash-1017~engine/api/v1`, started 2026-11-04T14:32:05.662622+00:00.

| Metric | Value |
| --- | --- |
| Calls | 162 |
| Schema-valid rate | 99.4% |
| First-pass rate | 98.8% |
| Repair rate | 0.6% |
| Fallback rate | 0.6% |
| p50 latency | 2167 ms |
| p95 latency | 8931 ms |
| Prompt tokens per call | 1207.2 |
| Completion tokens per call | 408.5 |
| Streaming operations not evaluated | 3 |

## Per contract

| Contract | Calls | LLM | Repaired | Fallback | p95 ms |
| --- | --- | --- | --- | --- | --- |
| asyncapi/commerce-events-kafka | 9 | 9 | 0 | 0 | 2686 |
| asyncapi/orders-amqp-2.6 | 6 | 6 | 0 | 0 | 1313 |
| graphql/devtools-github | 12 | 12 | 0 | 0 | 2798 |
| graphql/ecommerce-shopify-admin | 12 | 12 | 0 | 0 | 7670 |
| grpc/otlp-trace-service | 3 | 3 | 0 | 0 | 2204 |
| grpc/route-guide | 3 | 2 | 0 | 1 | 60004 |
| rest/devtools-github | 12 | 12 | 0 | 0 | 12358 |
| rest/ecommerce-orders-3.1 | 9 | 9 | 0 | 0 | 7118 |
| rest/ecommerce-square | 12 | 12 | 0 | 0 | 10587 |
| rest/payments-paypal-orders | 9 | 9 | 0 | 0 | 13796 |
| rest/payments-stripe | 12 | 12 | 0 | 0 | 3735 |
| rest/social-discord | 9 | 9 | 0 | 0 | 4308 |
| rest/social-spotify | 9 | 8 | 1 | 0 | 31081 |
| rest/social-tmdb | 12 | 12 | 0 | 0 | 8993 |
| rest/travel-booking | 12 | 12 | 0 | 0 | 5584 |
| rest/travel-hotels | 6 | 6 | 0 | 0 | 12347 |
| soap/accounts-rpc | 6 | 6 | 0 | 0 | 1712 |
| soap/calculator | 9 | 9 | 0 | 0 | 5298 |

## Per API type

| API type | Calls | LLM | Repaired | Fallback | p95 ms |
| --- | --- | --- | --- | --- | --- |
| asyncapi | 15 | 15 | 0 | 0 | 2686 |
| graphql | 24 | 24 | 0 | 0 | 5563 |
| grpc | 6 | 5 | 0 | 1 | 60004 |
| rest | 102 | 101 | 1 | 0 | 10587 |
| soap | 15 | 15 | 0 | 0 | 5298 |

## Per domain

| Domain | Calls | LLM | Repaired | Fallback | p95 ms |
| --- | --- | --- | --- | --- | --- |
| devtools | 27 | 27 | 0 | 0 | 6072 |
| ecommerce | 48 | 48 | 0 | 0 | 7118 |
| generic | 18 | 17 | 0 | 1 | 60004 |
| payments | 21 | 21 | 0 | 0 | 4614 |
| social-media | 30 | 29 | 1 | 0 | 13415 |
| travel | 18 | 18 | 0 | 0 | 12347 |
