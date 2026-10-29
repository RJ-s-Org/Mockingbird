# Mockingbird LLM evals

Model `z-ai/glm-5.3-flash` at `http://127.0.0.1:62384/r/z-ai__glm-5.3-flash~engine/api/v1`, started 2026-10-29T18:15:28.834719+00:00.

| Metric | Value |
| --- | --- |
| Calls | 162 |
| Schema-valid rate | 83.3% |
| First-pass rate | 49.4% |
| Repair rate | 34.0% |
| Fallback rate | 16.7% |
| p50 latency | 7044 ms |
| p95 latency | 30002 ms |
| Prompt tokens per call | 1316.7 |
| Completion tokens per call | 901.3 |

## Per contract

| Contract | Calls | LLM | Repaired | Fallback | p95 ms |
| --- | --- | --- | --- | --- | --- |
| asyncapi/commerce-events-kafka | 9 | 6 | 2 | 1 | 18858 |
| asyncapi/orders-amqp-2.6 | 6 | 6 | 0 | 0 | 2701 |
| graphql/devtools-github | 12 | 4 | 0 | 8 | 30009 |
| graphql/ecommerce-shopify-admin | 12 | 6 | 1 | 5 | 36701 |
| grpc/otlp-trace-service | 3 | 3 | 0 | 0 | 5245 |
| grpc/route-guide | 3 | 3 | 0 | 0 | 4291 |
| rest/devtools-github | 12 | 1 | 10 | 1 | 26921 |
| rest/ecommerce-orders-3.1 | 9 | 6 | 2 | 1 | 7404 |
| rest/ecommerce-square | 12 | 8 | 4 | 0 | 14696 |
| rest/payments-paypal-orders | 9 | 3 | 4 | 2 | 16962 |
| rest/payments-stripe | 12 | 7 | 5 | 0 | 18738 |
| rest/social-discord | 9 | 1 | 4 | 4 | 30004 |
| rest/social-spotify | 9 | 1 | 6 | 2 | 41385 |
| rest/social-tmdb | 12 | 2 | 10 | 0 | 22767 |
| rest/travel-booking | 12 | 5 | 5 | 2 | 30005 |
| rest/travel-hotels | 6 | 3 | 2 | 1 | 30633 |
| soap/accounts-rpc | 6 | 6 | 0 | 0 | 4709 |
| soap/calculator | 9 | 9 | 0 | 0 | 2473 |

## Per API type

| API type | Calls | LLM | Repaired | Fallback | p95 ms |
| --- | --- | --- | --- | --- | --- |
| asyncapi | 15 | 12 | 2 | 1 | 18858 |
| graphql | 24 | 10 | 1 | 13 | 30009 |
| grpc | 6 | 6 | 0 | 0 | 5245 |
| rest | 102 | 37 | 52 | 13 | 30002 |
| soap | 15 | 15 | 0 | 0 | 4709 |

## Per domain

| Domain | Calls | LLM | Repaired | Fallback | p95 ms |
| --- | --- | --- | --- | --- | --- |
| devtools | 27 | 8 | 10 | 9 | 27452 |
| ecommerce | 48 | 32 | 9 | 7 | 29670 |
| generic | 18 | 18 | 0 | 0 | 4709 |
| payments | 21 | 10 | 9 | 2 | 18090 |
| social-media | 30 | 4 | 20 | 6 | 30004 |
| travel | 18 | 8 | 7 | 3 | 30633 |
