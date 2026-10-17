import AxeBuilder from "@axe-core/playwright";
import { expect, test, type Page } from "@playwright/test";
import { fileURLToPath } from "node:url";

const ORDERS = fileURLToPath(new URL("../../../../fixtures/openapi/orders-3.0.yaml", import.meta.url));

async function expectAccessible(page: Page) {
  const results = await new AxeBuilder({ page }).withTags(["wcag2a", "wcag2aa", "wcag21a", "wcag21aa"]).analyze();
  const blocking = results.violations.filter((v) => v.impact === "serious" || v.impact === "critical");
  expect(blocking, JSON.stringify(blocking, null, 2)).toEqual([]);
}

function versionRow(page: Page, version: number) {
  return page
    .getByRole("table", { name: "Contract versions" })
    .getByRole("row")
    .filter({ has: page.getByRole("cell", { name: `v${version}`, exact: true }) });
}

test("create a mock, upload, activate and roll back contract versions", async ({ page }) => {
  const consoleErrors: string[] = [];
  page.on("console", (m) => {
    if (m.type() === "error") consoleErrors.push(m.text());
  });

  await page.goto("/w/default/mocks");
  await page.getByRole("button", { name: "New mock" }).click();
  const dialog = page.getByRole("dialog", { name: "New mock" });
  await dialog.getByLabel("Name").fill("pets");
  await dialog.getByRole("button", { name: "Create" }).click();
  await expect(page).toHaveURL(/\/w\/default\/mocks\/pets\/overview$/);
  await expect(page.getByLabel("Route prefix")).toHaveValue("/pets");

  await page.getByRole("navigation", { name: "Mock" }).getByRole("link", { name: "Contracts" }).click();
  await page.getByLabel("Contract file").setInputFiles(ORDERS);
  await page.getByRole("button", { name: "Upload" }).click();
  // The first version is activated regardless of the checkbox.
  await expect(page.getByRole("status")).toContainText("Uploaded version 1 (active");
  await expect(versionRow(page, 1).getByRole("cell", { name: "active", exact: true })).toBeVisible();

  await page.getByLabel("Contract file").setInputFiles(ORDERS);
  await page.getByLabel("Activate after upload").check();
  await page.getByRole("button", { name: "Upload" }).click();
  await expect(page.getByRole("status")).toContainText("Uploaded version 2 (active");
  await expect(versionRow(page, 2).getByRole("cell", { name: "active", exact: true })).toBeVisible();
  await expect(versionRow(page, 1).getByRole("cell", { name: "approved", exact: true })).toBeVisible();

  await versionRow(page, 1).getByRole("button", { name: "View operations of version 1" }).click();
  const drawer = page.getByRole("dialog", { name: "Operations of version 1" });
  await expect(drawer.getByRole("cell", { name: "listOrders", exact: true })).toBeVisible();
  await expectAccessible(page);
  await drawer.getByRole("button", { name: "Close" }).click();

  await versionRow(page, 1).getByRole("button", { name: "Roll back to version 1" }).click();
  await expect(versionRow(page, 1).getByRole("cell", { name: "active", exact: true })).toBeVisible();
  await expect(versionRow(page, 2).getByRole("button", { name: "Activate version 2" })).toBeVisible();
  await expect(page.getByText("Active version v1")).toBeVisible();
  await expectAccessible(page);

  // The mock declared in the config file is read-only.
  await page.goto("/w/default/mocks/files/overview");
  // One notice for the settings form, one for the chaos and limits editor.
  const notices = page.getByText("Managed by config file");
  await expect(notices).toHaveCount(2);
  for (const notice of await notices.all()) await expect(notice).toBeVisible();
  await expect(page.getByLabel("Persona")).toBeDisabled();
  await expect(page.getByRole("button", { name: "Save", exact: true })).toBeDisabled();
  await expect(page.getByRole("button", { name: "Save chaos and limits" })).toBeDisabled();
  await expect(page.getByRole("button", { name: "Delete mock" })).toBeDisabled();
  await expectAccessible(page);

  await page.goto("/w/default/mocks");
  await expect(page.getByRole("table", { name: "Mocks" }).getByRole("link", { name: "pets" })).toBeVisible();
  await expectAccessible(page);

  expect(consoleErrors).toEqual([]);
});
