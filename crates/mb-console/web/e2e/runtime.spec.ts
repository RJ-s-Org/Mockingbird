import AxeBuilder from "@axe-core/playwright";
import { expect, test, type Page } from "@playwright/test";
import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";

const ORDERS = fileURLToPath(new URL("../../../../fixtures/openapi/orders-3.0.yaml", import.meta.url));
const MOCK_API = "/api/v1/workspaces/default/mocks";
const DATA_PLANE = "http://127.0.0.1:18080";

async function expectAccessible(page: Page) {
  const results = await new AxeBuilder({ page }).withTags(["wcag2a", "wcag2aa", "wcag21a", "wcag21aa"]).analyze();
  const blocking = results.violations.filter((v) => v.impact === "serious" || v.impact === "critical");
  expect(blocking, JSON.stringify(blocking, null, 2)).toEqual([]);
}

function mockTab(page: Page, name: string) {
  return page.getByRole("navigation", { name: "Mock" }).getByRole("link", { name, exact: true });
}

test("live requests, sessions and scenarios of a running mock", async ({ page, request }) => {
  const consoleErrors: string[] = [];
  page.on("console", (m) => {
    if (m.type() === "error") consoleErrors.push(m.text());
  });

  // 1. Mock `live` serving the orders fixture (a reused server may still have it).
  await request.delete(`${MOCK_API}/live`);
  expect((await request.post(MOCK_API, { data: { name: "live" } })).status()).toBe(201);
  const upload = await request.post(`${MOCK_API}/live/contracts?activate=true`, {
    headers: { "Content-Type": "application/yaml" },
    data: readFileSync(ORDERS, "utf8"),
  });
  expect(upload.status()).toBe(201);

  // 2. The Live requests tab, once its stream is connected.
  await page.goto("/w/default/mocks/live/overview");
  const stream = page.waitForResponse((r) => r.url().includes("/mocks/live/requests/stream"));
  await mockTab(page, "Live requests").click();
  expect((await stream).status()).toBe(200);
  const live = page.getByRole("table", { name: "Live requests" });
  await expect(live.getByText("Waiting for requests…")).toBeVisible();

  // 3-4. A served call shows up within 5 s (its path without the route prefix) and opens in the drawer.
  const served = await request.post(`${DATA_PLANE}/live/orders`, {
    headers: { "X-Mockingbird-Session": "s1" },
    data: { customerId: "c1", total: 12.5 },
  });
  expect(served.status()).toBe(201);
  const row = live.getByRole("button", { name: /^Open POST \/orders at / });
  await expect(row).toBeVisible({ timeout: 5_000 });
  await expect(live.getByRole("cell", { name: "s1", exact: true })).toBeVisible();
  await expectAccessible(page);
  await row.click();
  const drawer = page.getByRole("dialog", { name: "POST /orders" });
  await expect(drawer.getByRole("heading", { name: "Response" })).toBeVisible();
  await expect(drawer.locator("pre").first()).toContainText('"customerId": "c1"');
  await expect(drawer.locator("pre").last()).toContainText('"total": 12.5');
  await expectAccessible(page);
  await drawer.getByRole("button", { name: "Close" }).click();

  // 5. The session holds the created order; reset forgets it.
  await mockTab(page, "Sessions").click();
  const sessions = page.getByRole("table", { name: "Sessions" });
  const s1 = sessions.getByRole("row").filter({ has: page.getByRole("cell", { name: "s1", exact: true }) });
  await expect(s1).toBeVisible();
  const resources = Number(await s1.getByRole("cell").nth(1).textContent());
  expect(resources).toBeGreaterThanOrEqual(1);
  await expectAccessible(page);
  await s1.getByRole("button", { name: "Reset session s1" }).click();
  await page.getByRole("dialog", { name: "Reset session s1?" }).getByRole("button", { name: "Reset" }).click();
  await expect(s1).toHaveCount(0);

  // 6. A scenario with latency and forced fields.
  await mockTab(page, "Scenarios").click();
  await page.getByRole("button", { name: "New scenario" }).click();
  const editor = page.getByRole("dialog", { name: "New scenario" });
  await editor.getByLabel("Name").fill("slow");
  await editor.getByLabel("Latency (ms)").fill("100");
  await editor.getByLabel("Set (JSON object)").fill('{"status":"shipped"}');
  await expect(editor.getByLabel("Operations").locator("option")).toHaveCount(5);
  await expectAccessible(page);
  await editor.getByRole("button", { name: "Save" }).click();
  await expect(editor).toHaveCount(0);
  const slow = page
    .getByRole("table", { name: "Scenarios" })
    .getByRole("row")
    .filter({ has: page.getByRole("cell", { name: "slow", exact: true }) });
  await expect(slow.getByRole("cell", { name: "100 ms" })).toBeVisible();

  // 7. Accessibility of the final page.
  await expectAccessible(page);
  expect(consoleErrors).toEqual([]);
});
