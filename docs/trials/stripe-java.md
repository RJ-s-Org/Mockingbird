# Trial: Stripe accept-a-payment, Java server

[stripe-samples/accept-a-payment](https://github.com/stripe-samples/accept-a-payment) at `1a57c3b`, `custom-payment-flow/server/java`: Spark Java 2.9, `stripe-java` 31.2.0, `java-dotenv`. The same sample's Node server was the first field trial; this run puts the Java server on the same mock and the same flows, and re-runs the Node server beside it.

| | |
| --- | --- |
| API mocked | Stripe (`api.stripe.com`): `POST /v1/payment_intents`, `GET /v1/payment_intents/{intent}`; `payment_intent.created` webhooks to the app |
| Contract | Stripe's `spec3.json` (the Node trial's copy, `stripe-spec3-v2500.json`, unmodified) |
| Model | OpenRouter `deepseek/deepseek-v4.1-flash`, default routing |
| Runtime | OpenJDK 27.0.1 (Homebrew), Maven 3.9 |
| Source edits | None. `.env` (git-ignored) copied from the Node server for the run and deleted afterwards |
| Result | 8/8 creates and 4/4 retrieves; 0 fallbacks; creates 1.0–2.9 s |

## Config and commands

`mockingbird-b1/mockingbird.yaml` is the Node trial's config with new ports and an absolute SQLite path:

```yaml
llm:
  default_profile: openrouter
  profiles:
    openrouter:
      api_key: env:OPENROUTER_API_KEY
      base_url: https://openrouter.ai/api/v1
      model: deepseek/deepseek-v4.1-flash
storage:
  url: sqlite:///Users/rjjain/Documents/Restia/mockingbird-trials/accept-a-payment/mockingbird-b1/mockingbird.db
server: {port: 45080, admin_port: 45090, grpc_port: 45051}
proxy: {enabled: true, port: 45888}
mocks:
  stripe:
    contract: ../custom-payment-flow/server/node/mockingbird-contracts/stripe-spec3-v2500.json
    route_prefix: /
    hosts: [api.stripe.com]
    webhooks:
      - url: http://127.0.0.1:4242/webhook
        signing: stripe
        secret: env:STRIPE_WEBHOOK_SECRET     # every write sends <object>.created
    persona: "Stripe API in test mode. Return complete Stripe objects with all top-level fields. Ids use Stripe prefixes (pi_, cus_, pm_) ..."
```

```bash
cd custom-payment-flow/server/java
mvn -q -DskipTests package                     # outside the proxy: Maven Central is not mocked
set -a; . ./.env; set +a
mockingbird run -c ../../../mockingbird-b1/mockingbird.yaml -- \
  sh -c '$JAVA -jar target/sample-jar-with-dependencies.jar & ../../../mockingbird-b1/flows.sh; kill $!'
```

`flows.sh` creates eight PaymentIntents through `POST /create-payment-intent` (card USD/EUR/GBP, iDEAL, SEPA Debit, Link, Bancontact) and follows four of them through `GET /payment/next`, which retrieves the intent and redirects to `/success`.

## What happened

**`JAVA_TOOL_OPTIONS` was enough.** The JVM printed `Picked up JAVA_TOOL_OPTIONS: -Dhttps.proxyHost=127.0.0.1 -Dhttps.proxyPort=45888 … -Djavax.net.ssl.trustStore=…/truststore.jks`, and stripe-java's `HttpURLConnection` went through the proxy and trusted the CA.

| Server | Creates | Retrieves | Sources | Prompt tokens |
| --- | --- | --- | --- | --- |
| Java | 8/8 in 1.0–2.9 s (median 1.6 s) | 4/4 from `state`, 2–6 ms | 6 `llm`, 2 `repaired`, 4 `state` | 140k (17.4k per create) |
| Node (re-run, same config) | 8/8 in 1.3–2.9 s (median 1.8 s) | 4/4 from `state`, 0–28 ms | 5 `llm`, 3 `repaired`, 4 `state` | 140k |
| Node, 2026-10-28 proxy trial | 8/8 in 3.2–7.6 s (median 3.7 s) | 4/4 from `state` | 6 `llm`, 2 `repaired`, 4 `state` | 140k |

The creates are about twice as fast as in the proxy trial, from L1's output budget and OpenRouter's `reasoning: {enabled: false}`. Prompts did not shrink: Stripe's `PaymentIntent` schema is 17k tokens of every prompt.

**Webhooks.** 16 `payment_intent.created` deliveries: the Node server answered 8 × 200 (signature checked by `stripe.webhooks.constructEvent`); the Java server answered 8 × 400. That is the sample's own logic: after `Webhook.constructEvent` succeeds, its `switch` answers 400 for any event type except `payment_intent.succeeded` and `payment_intent.payment_failed`. A `succeeded` event would need a confirm step the sample leaves to Stripe.js.

`verify --no-fallbacks --no-unmapped` passes for both.

## Notes

- The relative `storage.url` gotcha (Open since the proxy trials) hit again: running from `server/java` with `sqlite://./mockingbird-b1/…` failed with `unable to open database file`; the config now uses an absolute path.
- The jar was built before `mockingbird run`, so Maven's downloads never went through the proxy and the request log holds only the app's calls.
