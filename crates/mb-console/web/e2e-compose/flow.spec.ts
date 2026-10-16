import { expect, test, type Page } from "@playwright/test";
import { fileURLToPath } from "node:url";

// The Compose e2e flow (spec §15), against the Compose stack in deploy/compose
// (see playwright.rauthy.config.ts). A HAR import is deterministic, so the
// offline stack needs no LLM.
const HAR = fileURLToPath(new URL("../../../../fixtures/smart/petstore.har", import.meta.url));
const DATA_PLANE = "http://localhost:8080";
const DEMO_EMAIL = "demo@localhost.test";
const DEMO_PASSWORD = "Demo-Passw0rd-123!";
const SESSION = "e2e";

function mockTab(page: Page, name: string) {
  return page.getByRole("navigation", { name: "Mock" }).getByRole("link", { name, exact: true });
}

/** Rauthy 0.37's two-step login: the e-mail first, then the password. */
async function logIn(page: Page) {
  await page.goto("/");
  await expect(page).toHaveURL(/^http:\/\/localhost:8081\/auth\/v1\//);
  const login = page.locator("form").filter({ has: page.getByLabel("E-Mail", { exact: true }) });
  await login.getByLabel("E-Mail", { exact: true }).fill(DEMO_EMAIL);
  await login.getByRole("button", { name: "Login", exact: true }).click();
  const password = login.getByLabel("Password", { exact: true });
  await expect(password).toBeVisible();
  await password.fill(DEMO_PASSWORD);
  await login.getByRole("button", { name: "Login", exact: true }).click();
  await expect(page).toHaveURL(/^http:\/\/localhost:9090\/w\/default\/mocks$/, { timeout: 30_000 });
}

test("log in, import a HAR, activate it, call it in a session and reset the session", async ({ page, request }) => {
  // The import alone may take up to 30 s.
  test.setTimeout(120_000);

  // 1. Log in through Rauthy.
  await logIn(page);

  // 2. Create the mock `flow`.
  await page.getByRole("button", { name: "New mock" }).click();
  const dialog = page.getByRole("dialog", { name: "New mock" });
  await dialog.getByLabel("Name").fill("flow");
  // Step 5 steers the session with X-Mockingbird-Session, which needs this on.
  await expect(dialog.getByLabel("Header overrides")).toBeChecked();
  await dialog.getByRole("button", { name: "Create" }).click();
  await expect(page).toHaveURL(/\/w\/default\/mocks\/flow\/overview$/);

  // 3. Import the petstore HAR recording.
  await mockTab(page, "Import").click();
  await page.getByLabel("File", { exact: true }).check();
  await page.getByLabel("Document file").setInputFiles(HAR);
  await page.getByRole("button", { name: "Start import" }).click();
  await expect(page).toHaveURL(/\/w\/default\/mocks\/flow\/contracts\/1\/draft$/, { timeout: 30_000 });
  await expect(page.getByText("Status: draft")).toBeVisible();

  // 4. Approve, then activate.
  await page.getByRole("button", { name: "Approve" }).click();
  await page.getByRole("dialog", { name: "Approve draft v1" }).getByRole("button", { name: "Approve" }).click();
  await expect(page.getByText("Status: approved")).toBeVisible();
  await page.getByRole("button", { name: "Activate" }).click();
  await expect(page.getByText("Status: active")).toBeVisible();

  // 5. The data plane serves the imported operations in session `e2e`.
  const headers = { "X-Mockingbird-Session": SESSION };
  const fetched = await request.get(`${DATA_PLANE}/flow/api/pets/1`, { headers });
  expect(fetched.status()).toBe(200);
  const created = await request.post(`${DATA_PLANE}/flow/api/pets`, {
    headers,
    data: { name: "Bella", status: "available" },
  });
  expect(created.status()).toBe(201);

  // 6. The Sessions tab shows session `e2e` holding at least one resource.
  await mockTab(page, "Sessions").click();
  const sessions = page.getByRole("table", { name: "Sessions" });
  const row = sessions.getByRole("row").filter({ has: page.getByRole("cell", { name: SESSION, exact: true }) });
  await expect(row).toBeVisible();
  const resources = Number(await row.getByRole("cell").nth(1).textContent());
  expect(resources).toBeGreaterThanOrEqual(1);

  // 7. Reset it; the session is gone.
  await row.getByRole("button", { name: `Reset session ${SESSION}` }).click();
  await page.getByRole("dialog", { name: `Reset session ${SESSION}?` }).getByRole("button", { name: "Reset" }).click();
  await expect(row).toHaveCount(0);
});
