# Mockingbird LLM evals

Model `deepseek/deepseek-v4.1-flash-1017` at `http://127.0.0.1:62384/r/deepseek__deepseek-v4.1-flash-1017~engine/api/v1`, started 2026-10-29T18:20:13.846597+00:00.

| Metric | Value |
| --- | --- |
| Calls | 162 |
| Schema-valid rate | 93.8% |
| First-pass rate | 63.6% |
| Repair rate | 30.2% |
| Fallback rate | 6.2% |
| p50 latency | 2586 ms |
| p95 latency | 11384 ms |
| Prompt tokens per call | 1354.6 |
| Completion tokens per call | 474.4 |

## Per contract

| Contract | Calls | LLM | Repaired | Fallback | p95 ms |
| --- | --- | --- | --- | --- | --- |
| asyncapi/commerce-events-kafka | 9 | 5 | 4 | 0 | 3056 |
| asyncapi/orders-amqp-2.6 | 6 | 6 | 0 | 0 | 816 |
| graphql/devtools-github | 12 | 8 | 0 | 4 | 17501 |
| graphql/ecommerce-shopify-admin | 12 | 9 | 0 | 3 | 18208 |
| grpc/otlp-trace-service | 3 | 3 | 0 | 0 | 1489 |
| grpc/route-guide | 3 | 3 | 0 | 0 | 1111 |
| rest/devtools-github | 12 | 1 | 10 | 1 | 14447 |
| rest/ecommerce-orders-3.1 | 9 | 9 | 0 | 0 | 7339 |
| rest/ecommerce-square | 12 | 7 | 5 | 0 | 8825 |
| rest/payments-paypal-orders | 9 | 4 | 5 | 0 | 6051 |
| rest/payments-stripe | 12 | 2 | 10 | 0 | 3812 |
| rest/social-discord | 9 | 5 | 3 | 1 | 9127 |
| rest/social-spotify | 9 | 6 | 2 | 1 | 17745 |
| rest/social-tmdb | 12 | 2 | 10 | 0 | 8598 |
| rest/travel-booking | 12 | 12 | 0 | 0 | 10078 |
| rest/travel-hotels | 6 | 6 | 0 | 0 | 9133 |
| soap/accounts-rpc | 6 | 6 | 0 | 0 | 1372 |
| soap/calculator | 9 | 9 | 0 | 0 | 1036 |

## Per API type

| API type | Calls | LLM | Repaired | Fallback | p95 ms |
| --- | --- | --- | --- | --- | --- |
| asyncapi | 15 | 11 | 4 | 0 | 3056 |
| graphql | 24 | 17 | 0 | 7 | 17501 |
| grpc | 6 | 6 | 0 | 0 | 1489 |
| rest | 102 | 54 | 45 | 3 | 10078 |
| soap | 15 | 15 | 0 | 0 | 1372 |

## Per domain

| Domain | Calls | LLM | Repaired | Fallback | p95 ms |
| --- | --- | --- | --- | --- | --- |
| devtools | 27 | 12 | 10 | 5 | 14447 |
| ecommerce | 48 | 36 | 9 | 3 | 10458 |
| generic | 18 | 18 | 0 | 0 | 1372 |
| payments | 21 | 6 | 15 | 0 | 4889 |
| social-media | 30 | 13 | 15 | 2 | 13435 |
| travel | 18 | 18 | 0 | 0 | 10078 |
