import AxeBuilder from "@axe-core/playwright";
import { expect, test } from "@playwright/test";

test("the shell opens the default workspace and is accessible", async ({ page }) => {
  await page.goto("/");
  await expect(page).toHaveURL(/\/w\/default\/mocks$/);

  const switcher = page.getByLabel("Workspace");
  await expect(switcher).toHaveValue("default");
  await expect(page.getByRole("navigation", { name: "Main" }).getByRole("link", { name: "Mocks" })).toBeVisible();
  // auth.mode none: the local user cannot sign out.
  await expect(page.getByRole("button", { name: "Sign out" })).toHaveCount(0);

  const results = await new AxeBuilder({ page }).withTags(["wcag2a", "wcag2aa", "wcag21a", "wcag21aa"]).analyze();
  const blocking = results.violations.filter((v) => v.impact === "serious" || v.impact === "critical");
  expect(blocking, JSON.stringify(blocking, null, 2)).toEqual([]);
});

test("an unknown workspace shows a not-found state", async ({ page }) => {
  await page.goto("/w/nowhere/mocks");
  await expect(page.getByRole("heading", { name: "Workspace not found" })).toBeVisible();
  await expect(page.getByLabel("Workspace")).toHaveValue("nowhere");
  await expect(page.getByRole("button", { name: "New mock" })).toHaveCount(0);
  await page.getByRole("link", { name: /Go to the .* workspace/ }).click();
  await expect(page).toHaveURL(/\/w\/default\/mocks$/);
  await expect(page.getByRole("button", { name: "New mock" })).toBeVisible();
});

test.describe("dark mode", () => {
  test.use({ colorScheme: "dark" });

  test("follows prefers-color-scheme, can be overridden, and stays accessible", async ({ page }) => {
    await page.goto("/w/default/mocks");
    await expect(page.locator("html")).toHaveClass(/\bdark\b/);
    const background = () =>
      page.evaluate(() => getComputedStyle(document.querySelector("main")!.parentElement!).backgroundColor);
    const dark = await background();

    const results = await new AxeBuilder({ page }).withTags(["wcag2a", "wcag2aa", "wcag21a", "wcag21aa"]).analyze();
    const blocking = results.violations.filter((v) => v.impact === "serious" || v.impact === "critical");
    expect(blocking, JSON.stringify(blocking, null, 2)).toEqual([]);

    // A manual choice wins over the system and survives a reload.
    await page.getByLabel("Theme").selectOption("light");
    await expect(page.locator("html")).not.toHaveClass(/\bdark\b/);
    expect(await background()).not.toBe(dark);
    await page.reload();
    await expect(page.getByLabel("Theme")).toHaveValue("light");
    await expect(page.locator("html")).not.toHaveClass(/\bdark\b/);
  });
});
