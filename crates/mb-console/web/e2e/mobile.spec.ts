import { expect, test, type Page } from "@playwright/test";

// A phone-sized viewport: the console must never scroll sideways as a page.
test.use({ viewport: { width: 390, height: 844 } });

const ROUTES = [
  "/w/default/mocks",
  "/w/default/mocks/files/overview",
  "/w/default/mocks/files/contracts",
  "/w/default/mocks/files/import",
  "/w/default/mocks/files/scenarios",
  "/w/default/mocks/files/sessions",
  "/w/default/mocks/files/requests",
  "/w/default/mocks/files/channels",
  "/w/default/llm-profiles",
  "/w/default/api-keys",
  "/w/default/audit",
  "/status",
  "/w/nowhere/mocks",
];

async function expectNoHorizontalScroll(page: Page) {
  const { scrollWidth, innerWidth } = await page.evaluate(() => ({
    scrollWidth: document.documentElement.scrollWidth,
    innerWidth: window.innerWidth,
  }));
  expect(scrollWidth, `page is ${scrollWidth}px wide in a ${innerWidth}px viewport`).toBeLessThanOrEqual(innerWidth);
}

for (const route of ROUTES) {
  test(`no horizontal page scroll at 390px on ${route}`, async ({ page }) => {
    await page.goto(route);
    await expect(page.getByRole("navigation", { name: "Main" })).toBeVisible();
    await expect(page.getByText("Loading…")).toHaveCount(0);
    await expectNoHorizontalScroll(page);
  });
}

test("the header controls and the new mock dialog fit at 390px", async ({ page }) => {
  await page.goto("/w/default/mocks");
  const viewport = page.viewportSize()?.width ?? 390;
  for (const control of [page.getByLabel("Workspace"), page.getByRole("button", { name: "New workspace" })]) {
    const box = await control.boundingBox();
    expect(box).not.toBeNull();
    expect((box?.x ?? 0) + (box?.width ?? 0)).toBeLessThanOrEqual(viewport);
  }

  await page.getByRole("button", { name: "New mock" }).click();
  const dialog = page.getByRole("dialog", { name: "New mock" });
  await expect(dialog).toBeVisible();
  const box = await dialog.boundingBox();
  expect((box?.x ?? 0) >= 0 && (box?.x ?? 0) + (box?.width ?? 0) <= viewport).toBe(true);
  await expectNoHorizontalScroll(page);
});
