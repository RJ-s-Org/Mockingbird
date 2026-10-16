import { expect, test, type Page } from "@playwright/test";
import { fileURLToPath } from "node:url";

// Runs against the Compose stack in deploy/compose (see playwright.rauthy.config.ts).
const ORDERS = fileURLToPath(new URL("../../../../fixtures/openapi/orders-3.0.yaml", import.meta.url));
const DATA_PLANE = "http://localhost:8080";
const DEMO_EMAIL = "demo@localhost.test";
const DEMO_PASSWORD = "Demo-Passw0rd-123!";

function mainNav(page: Page, name: string) {
  return page.getByRole("navigation", { name: "Main" }).getByRole("link", { name, exact: true });
}

function mockTab(page: Page, name: string) {
  return page.getByRole("navigation", { name: "Mock" }).getByRole("link", { name, exact: true });
}

test("log in through Rauthy, serve a mock and find it all in the audit log", async ({ page, request }) => {
  // 1. The console sends an anonymous browser to Rauthy.
  await page.goto("/");
  await expect(page).toHaveURL(/^http:\/\/localhost:8081\/auth\/v1\//);
  // Rauthy 0.37's login (frontend/src/routes/oidc/authorize/+page.svelte) is two
  // steps: submit the e-mail alone, and only for a password account does it then
  // show the password field. The passkey button outside the form is also named
  // "Login", so the submit button is looked up inside the form.
  const login = page.locator("form").filter({ has: page.getByLabel("E-Mail", { exact: true }) });
  await login.getByLabel("E-Mail", { exact: true }).fill(DEMO_EMAIL);
  await login.getByRole("button", { name: "Login", exact: true }).click();
  const password = login.getByLabel("Password", { exact: true });
  await expect(password).toBeVisible();
  await password.fill(DEMO_PASSWORD);
  await login.getByRole("button", { name: "Login", exact: true }).click();

  // 2. Back in the console, in the default workspace; create mock `e2e`.
  await expect(page).toHaveURL(/^http:\/\/localhost:9090\/w\/default\/mocks$/, { timeout: 30_000 });
  await page.getByRole("button", { name: "New mock" }).click();
  const dialog = page.getByRole("dialog", { name: "New mock" });
  await dialog.getByLabel("Name").fill("e2e");
  // Step 4 steers the session with X-Mockingbird-Session, which needs this on.
  await expect(dialog.getByLabel("Header overrides")).toBeChecked();
  await dialog.getByRole("button", { name: "Create" }).click();
  await expect(page).toHaveURL(/\/w\/default\/mocks\/e2e\/overview$/);

  // 3. Upload the orders contract (the first version is activated).
  await mockTab(page, "Contracts").click();
  await page.getByLabel("Contract file").setInputFiles(ORDERS);
  await page.getByRole("button", { name: "Upload" }).click();
  await expect(page.getByRole("status")).toContainText("Uploaded version 1 (active");

  // Open the live tail before calling, so the calls arrive on the stream.
  const stream = page.waitForResponse((r) => r.url().includes("/mocks/e2e/requests/stream"));
  await mockTab(page, "Live requests").click();
  expect((await stream).status()).toBe(200);
  const live = page.getByRole("table", { name: "Live requests" });

  // 4. The data plane serves the mock; a POST also gives session `e2e-s` state.
  const listed = await request.get(`${DATA_PLANE}/e2e/orders`);
  expect(listed.status()).toBe(200);
  const created = await request.post(`${DATA_PLANE}/e2e/orders`, {
    headers: { "X-Mockingbird-Session": "e2e-s" },
    data: { customerId: "c1", total: 12.5 },
  });
  expect(created.status()).toBe(201);

  // 5. Both calls show up in Live requests.
  await expect(live.getByRole("button", { name: /^Open GET \/orders at / })).toBeVisible({ timeout: 10_000 });
  await expect(live.getByRole("button", { name: /^Open POST \/orders at / })).toBeVisible({ timeout: 10_000 });

  // 6. Reset the session.
  await mockTab(page, "Sessions").click();
  const sessions = page.getByRole("table", { name: "Sessions" });
  const row = sessions.getByRole("row").filter({ has: page.getByRole("cell", { name: "e2e-s", exact: true }) });
  await expect(row).toBeVisible();
  await row.getByRole("button", { name: "Reset session e2e-s" }).click();
  await page.getByRole("dialog", { name: "Reset session e2e-s?" }).getByRole("button", { name: "Reset" }).click();
  await expect(row).toHaveCount(0);

  // 7. The audit log (all workspaces, since a login belongs to none).
  await mainNav(page, "Audit").click();
  await page.getByLabel("All workspaces").check();
  const audit = page.getByRole("table", { name: "Audit events" });
  for (const action of ["auth.login", "mock.create", "contract.upload", "session.reset"]) {
    await expect(audit.getByRole("cell", { name: action, exact: true }).first()).toBeVisible();
  }
});
