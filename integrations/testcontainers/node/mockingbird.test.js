import assert from "node:assert/strict";
import { after, before, test } from "node:test";
import { MockingbirdContainer } from "./mockingbird.js";

const fixtures = new URL("../fixtures/", import.meta.url).pathname;
let mocks;

before(async () => {
  mocks = await new MockingbirdContainer().withConfigDir(fixtures).start();
});
after(() => mocks?.stop());

test("a charge is created and the mock records the call", async () => {
  const res = await fetch(`${mocks.baseUrl("payments")}/v1/charges`, {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify({ amount: 1999, currency: "eur" }),
  });
  assert.equal(res.status, 201);
  const charge = await res.json();
  assert.equal(typeof charge.id, "string");

  const log = await mocks.requests("payments");
  assert.ok(log.some((r) => r.method === "POST" && r.request.body.amount === 1999));
});

test("a scenario declines the card", async () => {
  const res = await fetch(`${mocks.baseUrl("payments")}/v1/charges`, {
    method: "POST",
    headers: { "content-type": "application/json", "x-mockingbird-scenario": "card-declined" },
    body: JSON.stringify({ amount: 1999, currency: "eur" }),
  });
  assert.equal(res.status, 402);
});
