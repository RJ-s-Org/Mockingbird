import AxeBuilder from "@axe-core/playwright";
import { expect, test, type Page } from "@playwright/test";
import { fileURLToPath } from "node:url";

const HAR = fileURLToPath(new URL("../../../../fixtures/smart/petstore.har", import.meta.url));
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

function operationRow(page: Page, method: string, path: string) {
  return page
    .getByRole("table", { name: "Draft operations" })
    .getByRole("row")
    .filter({ has: page.getByRole("cell", { name: method, exact: true }) })
    .filter({ has: page.getByRole("cell", { name: path, exact: true }) });
}

test("import a HAR recording, review, edit, approve and activate the draft", async ({ page, request }) => {
  // The import alone may take up to 30 s.
  test.setTimeout(90_000);
  const consoleErrors: string[] = [];
  page.on("console", (m) => {
    if (m.type() === "error") consoleErrors.push(m.text());
  });

  // 1. Mock `har` (a reused server may still have it), then a HAR upload with detection.
  await request.delete(`${MOCK_API}/har`);
  await page.goto("/w/default/mocks");
  await page.getByRole("button", { name: "New mock" }).click();
  const dialog = page.getByRole("dialog", { name: "New mock" });
  await dialog.getByLabel("Name").fill("har");
  await dialog.getByRole("button", { name: "Create" }).click();
  await expect(page).toHaveURL(/\/w\/default\/mocks\/har\/overview$/);

  await mockTab(page, "Import").click();
  await page.getByLabel("File", { exact: true }).check();
  await page.getByLabel("Document file").setInputFiles(HAR);
  await expect(page.getByLabel("Kind")).toHaveValue("");
  await page.getByRole("button", { name: "Start import" }).click();

  // 2. The draft review page, with the recorded operation at high confidence.
  await expect(page).toHaveURL(/\/w\/default\/mocks\/har\/contracts\/1\/draft$/, { timeout: 30_000 });
  await expect(page.getByRole("heading", { name: "Draft v1" })).toBeVisible();
  await expect(page.getByText("Status: draft")).toBeVisible();
  const pet = operationRow(page, "GET", "/api/pets/{petId}");
  await expect(pet.getByText(/^High · \d+%$/)).toBeVisible();
  await expectAccessible(page);

  // 3. A preview of it is JSON.
  await pet.getByRole("button", { name: "Preview GET /api/pets/{petId}" }).click();
  const body = page.getByLabel("Preview body");
  await expect(body).toBeVisible();
  const text = (await body.textContent()) ?? "";
  expect(() => JSON.parse(text)).not.toThrow();
  await page.getByRole("button", { name: "Close preview" }).click();

  // 4. A summary edit is saved and marked.
  await pet.getByRole("button", { name: "Edit GET /api/pets/{petId}" }).click();
  const editor = page.getByRole("form", { name: "Edit GET /api/pets/{petId}" });
  await editor.getByLabel("Summary").fill("Fetch a pet");
  await editor.getByRole("button", { name: "Save" }).click();
  await expect(editor).toBeHidden();
  await expect(pet.getByRole("cell", { name: "Fetch a pet", exact: true })).toBeVisible();
  await expect(pet.getByText("edited", { exact: true })).toBeVisible();

  // 5. Approve, then activate.
  await page.getByRole("button", { name: "Approve" }).click();
  await page.getByRole("dialog", { name: "Approve draft v1" }).getByRole("button", { name: "Approve" }).click();
  await expect(page.getByText("Status: approved")).toBeVisible();
  await page.getByRole("button", { name: "Activate" }).click();
  await expect(page.getByText("Status: active")).toBeVisible();
  await expect(page.getByRole("link", { name: "Back to contracts" })).toBeVisible();
  await expectAccessible(page);

  // 6. The data plane serves the imported operation.
  const served = await request.get(`${DATA_PLANE}/har/api/pets/1`);
  expect(served.status()).toBe(200);

  // 7. Documentation needs an LLM, which the offline e2e server does not have:
  //    the server refuses the import up front, so the failure shows in the wizard.
  await mockTab(page, "Import").click();
  await page.getByLabel("Paste text").check();
  await page.getByLabel("Kind").selectOption("markdown");
  await page.getByLabel("Document text").fill("# Pets\n\n## GET /pets\n\nLists the pets.\n");
  await page.getByRole("button", { name: "Start import" }).click();
  await expect(page.getByRole("alert")).toContainText("LLM profile");

  // 8. The draft page is accessible when read-only, too.
  await page.goto("/w/default/mocks/har/contracts/1/draft");
  await expect(page.getByText("Status: active")).toBeVisible();
  await expectAccessible(page);

  // The refused Markdown import (step 7) is the only failed request.
  expect(consoleErrors.filter((e) => !/status of 422/.test(e))).toEqual([]);
});
