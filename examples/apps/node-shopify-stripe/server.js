// Juniper Plants storefront backend: lists products from the Shopify Admin
// GraphQL API and records orders from verified Stripe webhooks.
//
// It talks to https://juniper-plants.myshopify.com like any Shopify app; the
// tests run it under `mockingbird proxy env` (Node's NODE_USE_ENV_PROXY sends
// fetch through the proxy, NODE_EXTRA_CA_CERTS trusts its CA).
import http from "node:http";
import { createAdminApiClient } from "@shopify/admin-api-client";
import Stripe from "stripe";

const shopify = createAdminApiClient({
  storeDomain: process.env.SHOPIFY_STORE ?? "juniper-plants.myshopify.com",
  apiVersion: "2026-10",
  accessToken: process.env.SHOPIFY_ACCESS_TOKEN ?? "shpat_test123",
});
// Only used to verify webhook signatures; it makes no API calls.
const stripe = new Stripe(process.env.STRIPE_API_KEY ?? "sk_test_123");
const orders = [];

const PRODUCTS = `#graphql
  query Products($first: Int!) {
    products(first: $first) {
      nodes { id title handle vendor }
    }
  }`;

async function products() {
  const { data, errors } = await shopify.request(PRODUCTS, { variables: { first: 5 } });
  if (errors) throw new Error(errors.message ?? JSON.stringify(errors));
  return data.products.nodes;
}

function readBody(req) {
  return new Promise((resolve, reject) => {
    const chunks = [];
    req.on("data", (c) => chunks.push(c));
    req.on("end", () => resolve(Buffer.concat(chunks)));
    req.on("error", reject);
  });
}

function send(res, status, body) {
  res.writeHead(status, { "content-type": "application/json" });
  res.end(JSON.stringify(body));
}

export const server = http.createServer(async (req, res) => {
  try {
    if (req.method === "GET" && req.url === "/products") {
      return send(res, 200, await products());
    }
    if (req.method === "POST" && req.url === "/stripe/webhook") {
      const raw = await readBody(req);
      let event;
      try {
        event = stripe.webhooks.constructEvent(
          raw,
          req.headers["stripe-signature"],
          process.env.STRIPE_WEBHOOK_SECRET,
        );
      } catch (e) {
        return send(res, 400, { error: e.message });
      }
      if (event.type === "payment_intent.created") {
        const intent = event.data.object;
        orders.push({ payment_intent: intent.id, amount: intent.amount, currency: intent.currency });
      }
      return send(res, 200, { received: true });
    }
    if (req.method === "GET" && req.url === "/orders") {
      return send(res, 200, orders);
    }
    send(res, 404, { error: "not found" });
  } catch (e) {
    send(res, 502, { error: e.message });
  }
});

if (import.meta.url === `file://${process.argv[1]}`) {
  const port = Number(process.env.PORT ?? 3000);
  server.listen(port, "127.0.0.1", () => console.log(`storefront on http://127.0.0.1:${port}`));
}
