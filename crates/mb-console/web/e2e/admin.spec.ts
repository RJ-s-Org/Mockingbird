import AxeBuilder from "@axe-core/playwright";
import { expect, test, type Page } from "@playwright/test";
import { fileURLToPath } from "node:url";

const ORDERS = fileURLToPath(new URL("../../../../fixtures/openapi/orders-3.0.yaml", import.meta.url));
const WS_API = "/api/v1/workspaces/default";
const DATA_PLANE = "http://127.0.0.1:18080";

async function expectAccessible(page: Page) {
  const results = await new AxeBuilder({ page }).withTags(["wcag2a", "wcag2aa", "wcag21a", "wcag21aa"]).analyze();
  const blocking = results.violations.filter((v) => v.impact === "serious" || v.impact === "critical");
  expect(blocking, JSON.stringify(blocking, null, 2)).toEqual([]);
}

function mainNav(page: Page, name: string) {
  return page.getByRole("navigation", { name: "Main" }).getByRole("link", { name, exact: true });
}

test("profiles, data-plane keys, audit and status", async ({ page, request }) => {
  const consoleErrors: string[] = [];
  page.on("console", (m) => {
    if (m.type() === "error") consoleErrors.push(m.text());
  });
  // A reused server may still have these from an earlier run.
  await request.delete(`${WS_API}/mocks/guarded`);
  await request.delete(`${WS_API}/llm-profiles/fast`);
  const keyName = `dp-${Date.now()}`;

  // 1. Profile `fast`, referring to its key by environment variable.
  await page.goto("/w/default/llm-profiles");
  await page.getByRole("button", { name: "New profile" }).click();
  const editor = page.getByRole("dialog", { name: "New profile" });
  await editor.getByLabel("Name").fill("fast");
  await editor.getByLabel("Base URL").fill("http://127.0.0.1:9/v1");
  await editor.getByLabel("Model").fill("gpt-mini");
  await editor.getByLabel("Secret ref").fill("env:MB_LLM_API_KEY");
  await expectAccessible(page);
  await editor.getByRole("button", { name: "Save" }).click();
  await expect(page.getByRole("status")).toContainText("Saved profile fast.");
  const profiles = page.getByRole("table", { name: "LLM profiles" });
  await expect(profiles.getByRole("cell", { name: "env:MB_LLM_API_KEY" })).toBeVisible();
  await expectAccessible(page);

  // 2. A data-plane key; its token is shown once.
  await mainNav(page, "API keys").click();
  await page.getByRole("button", { name: "New API key" }).click();
  const create = page.getByRole("dialog", { name: "New API key" });
  await create.getByLabel("Name").fill(keyName);
  await create.getByLabel("admin", { exact: true }).uncheck();
  await create.getByLabel("data-plane", { exact: true }).check();
  await expectAccessible(page);
  await create.getByRole("button", { name: "Create" }).click();
  const created = page.getByRole("dialog", { name: "API key created" });
  await expect(created.getByText("This token will not be shown again")).toBeVisible();
  const token = (await created.getByTestId("api-key-token").textContent()) ?? "";
  expect(token).toMatch(/^mbk_[a-z2-7]{12}_[A-Za-z0-9_-]{43}$/);
  await expectAccessible(page);
  await created.getByRole("button", { name: "Close" }).click();
  await expect(page.getByText(token)).toHaveCount(0);

  // 3. Mock `guarded` serving the fixture, behind data-plane api_key auth.
  await mainNav(page, "Mocks").click();
  await page.getByRole("button", { name: "New mock" }).click();
  const newMock = page.getByRole("dialog", { name: "New mock" });
  await newMock.getByLabel("Name").fill("guarded");
  await newMock.getByRole("button", { name: "Create" }).click();
  await expect(page).toHaveURL(/\/w\/default\/mocks\/guarded\/overview$/);
  await page.getByRole("navigation", { name: "Mock" }).getByRole("link", { name: "Contracts" }).click();
  await page.getByLabel("Contract file").setInputFiles(ORDERS);
  await page.getByRole("button", { name: "Upload" }).click();
  await expect(page.getByRole("status")).toContainText("Uploaded version 1 (active");
  await page.getByRole("navigation", { name: "Mock" }).getByRole("link", { name: "Overview" }).click();
  await page.getByLabel("Data-plane auth").selectOption("api_key");
  await page.getByRole("button", { name: "Save" }).click();
  await expect(page.getByRole("status")).toContainText("Saved.");

  // 4. Without a key 401; with the token 200.
  const anonymous = await request.get(`${DATA_PLANE}/guarded/orders`);
  expect(anonymous.status()).toBe(401);
  const keyed = await request.get(`${DATA_PLANE}/guarded/orders`, { headers: { "x-api-key": token } });
  expect(keyed.status()).toBe(200);

  // 5. A revoked key is refused.
  await mainNav(page, "API keys").click();
  const keys = page.getByRole("table", { name: "API keys" });
  await keys.getByRole("button", { name: `Revoke ${keyName}` }).click();
  await page.getByRole("dialog", { name: `Revoke key ${keyName}?` }).getByRole("button", { name: "Revoke" }).click();
  const revokedRow = keys.getByRole("row").filter({ has: page.getByRole("cell", { name: keyName, exact: true }) });
  await expect(revokedRow.getByText("revoked", { exact: true })).toBeVisible();
  const revoked = await request.get(`${DATA_PLANE}/guarded/orders`, { headers: { "x-api-key": token } });
  expect(revoked.status()).toBe(401);

  // 6. The audit log has the key and mock changes.
  await mainNav(page, "Audit").click();
  const audit = page.getByRole("table", { name: "Audit events" });
  for (const action of ["api_key.create", "api_key.revoke", "mock.update"]) {
    await expect(audit.getByRole("cell", { name: action, exact: true }).first()).toBeVisible();
  }
  await expect(page.getByRole("link", { name: "Export JSONL" })).toHaveAttribute("download", "");
  await expectAccessible(page);

  // 7. Status.
  await mainNav(page, "Status").click();
  await expect(page.getByText("ready", { exact: true })).toBeVisible();
  await expect(page.getByText("Metrics are private")).toHaveCount(0);
  await expect(page.getByTestId("requests-total")).not.toHaveText("0");

  // 8. Accessibility of the final page.
  await expectAccessible(page);
  expect(consoleErrors).toEqual([]);
});
