// Starts Mockingbird (offline, with its proxy) and the storefront under the
// variables `mockingbird proxy env` prints, then drives both.
import { test, before, after } from "node:test";
import assert from "node:assert/strict";
import { spawn, execFileSync } from "node:child_process";
import { mkdtempSync } from "node:fs";
import http from "node:http";
import { tmpdir } from "node:os";
import { join } from "node:path";

const MB = process.env.MOCKINGBIRD_BIN ?? "mockingbird";
const [PORT, ADMIN, GRPC, PROXY] = [50080, 50090, 50051, 50888];
const SECRET = "whsec_test_juniper";
const APP = "http://127.0.0.1:3000";
let server, app;

async function waitFor(url, timeoutMs = 30000) {
  const deadline = Date.now() + timeoutMs;
  while (Date.now() < deadline) {
    try {
      if ((await fetch(url)).ok) return;
    } catch {}
    await new Promise((r) => setTimeout(r, 200));
  }
  throw new Error(`${url} did not answer`);
}

// POSTs a form to the mock's data plane under `host` (fetch cannot set Host).
function postForm(host, path, body) {
  return new Promise((resolve, reject) => {
    const req = http.request(
      {
        host: "127.0.0.1",
        port: PORT,
        path,
        method: "POST",
        headers: { host, "content-type": "application/x-www-form-urlencoded" },
      },
      (res) => {
        let text = "";
        res.on("data", (c) => (text += c));
        res.on("end", () =>
          res.statusCode === 200 ? resolve(JSON.parse(text)) : reject(new Error(`${res.statusCode} ${text}`)),
        );
      },
    );
    req.on("error", reject);
    req.end(body);
  });
}

before(async () => {
  const caDir = mkdtempSync(join(tmpdir(), "mb-ca-"));
  // The mock server gets no proxy variables of its own.
  const clean = Object.fromEntries(Object.entries(process.env).filter(([k]) => !/proxy/i.test(k)));
  const serverEnv = { ...clean, MB_CA_DIR: caDir, STRIPE_WEBHOOK_SECRET: SECRET };
  const args = ["serve", "-c", "mockingbird.yaml", "--offline",
    "--port", PORT, "--admin-port", ADMIN, "--grpc-port", GRPC, "--proxy-port", PROXY];
  server = spawn(MB, args.map(String), { env: serverEnv, stdio: ["ignore", "inherit", "inherit"] });
  await waitFor(`http://127.0.0.1:${ADMIN}/readyz`);
  const { vars } = JSON.parse(
    execFileSync(MB, ["proxy", "env", "--json", "--port", String(PROXY)], { env: serverEnv }),
  );
  app = spawn(process.execPath, ["server.js"], {
    env: { ...clean, ...vars, STRIPE_WEBHOOK_SECRET: SECRET, PORT: "3000" },
    stdio: ["ignore", "inherit", "inherit"],
  });
  await waitFor(`${APP}/orders`);
});

after(() => {
  app?.kill();
  server?.kill();
});

test("lists products from the Shopify Admin GraphQL API", async () => {
  const res = await fetch(`${APP}/products`);
  const body = await res.json();
  assert.equal(res.status, 200, JSON.stringify(body));
  assert.ok(Array.isArray(body) && body.length > 0, JSON.stringify(body));
  // Offline, the faker fills GraphQL IDs with words; with an LLM they are
  // gid://shopify/Product/... as the persona asks.
  assert.equal(typeof body[0].id, "string");
  assert.equal(typeof body[0].title, "string");
});

test("records an order from a verified Stripe webhook", async () => {
  // The checkout page would create this PaymentIntent with Stripe.js.
  const intent = await postForm("api.stripe.com", "/v1/payment_intents", "amount=3400&currency=usd&description=Monstera");
  assert.match(intent.id, /^pi_/);
  const deadline = Date.now() + 15000;
  let orders = [];
  while (Date.now() < deadline) {
    orders = await (await fetch(`${APP}/orders`)).json();
    if (orders.length > 0) break;
    await new Promise((r) => setTimeout(r, 200));
  }
  assert.ok(orders.length > 0, "no verified payment_intent.created webhook arrived");
  assert.match(orders[0].payment_intent, /^pi_/);
});
