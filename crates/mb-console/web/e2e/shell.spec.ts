import AxeBuilder from "@axe-core/playwright";
import { expect, test } from "@playwright/test";
import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { inflateSync } from "node:zlib";

const ORDERS = fileURLToPath(new URL("../../../../fixtures/openapi/orders-3.0.yaml", import.meta.url));

/** A minimal decoder for the 8-bit, non-interlaced RGB(A) PNGs Playwright takes. */
function decodePng(buf: Buffer) {
  let pos = 8;
  let width = 0;
  let height = 0;
  let channels = 4;
  const idat: Buffer[] = [];
  while (pos < buf.length) {
    const len = buf.readUInt32BE(pos);
    const type = buf.toString("ascii", pos + 4, pos + 8);
    const data = buf.subarray(pos + 8, pos + 8 + len);
    if (type === "IHDR") {
      width = data.readUInt32BE(0);
      height = data.readUInt32BE(4);
      channels = data[9] === 6 ? 4 : 3;
    } else if (type === "IDAT") idat.push(data);
    pos += 12 + len;
  }
  const raw = inflateSync(Buffer.concat(idat));
  const stride = width * channels;
  const out = Buffer.alloc(height * stride);
  for (let y = 0; y < height; y += 1) {
    const filter = raw[y * (stride + 1)]!;
    for (let x = 0; x < stride; x += 1) {
      const v = raw[y * (stride + 1) + 1 + x]!;
      const a = x >= channels ? out[y * stride + x - channels]! : 0;
      const b = y > 0 ? out[(y - 1) * stride + x]! : 0;
      const c = x >= channels && y > 0 ? out[(y - 1) * stride + x - channels]! : 0;
      let p = 0;
      if (filter === 1) p = a;
      else if (filter === 2) p = b;
      else if (filter === 3) p = (a + b) >> 1;
      else if (filter === 4) {
        const e = a + b - c;
        const [pa, pb, pc] = [Math.abs(e - a), Math.abs(e - b), Math.abs(e - c)];
        p = pa <= pb && pa <= pc ? a : pb <= pc ? b : c;
      }
      out[y * stride + x] = (v + p) & 0xff;
    }
  }
  return {
    width,
    pixel: (x: number, y: number): [number, number, number] => {
      const i = y * stride + x * channels;
      return [out[i]!, out[i + 1]!, out[i + 2]!];
    },
  };
}

/** WCAG relative luminance of an sRGB pixel. */
function luminance([r, g, b]: [number, number, number]): number {
  const lin = (v: number) => {
    const c = v / 255;
    return c <= 0.03928 ? c / 12.92 : ((c + 0.055) / 1.055) ** 2.4;
  };
  return 0.2126 * lin(r) + 0.7152 * lin(g) + 0.0722 * lin(b);
}


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

  test("selected options of a multi-select stay readable", async ({ page, request }) => {
    const api = "/api/v1/workspaces/default/mocks";
    await request.delete(`${api}/darkselect`);
    expect((await request.post(api, { data: { name: "darkselect" } })).status()).toBe(201);
    const upload = await request.post(`${api}/darkselect/contracts?activate=true`, {
      headers: { "Content-Type": "application/yaml" },
      data: readFileSync(ORDERS, "utf8"),
    });
    expect(upload.status()).toBe(201);

    await page.goto("/w/default/mocks/darkselect/scenarios");
    await expect(page.locator("html")).toHaveClass(/\bdark\b/);
    await page.getByRole("button", { name: "New scenario" }).click();
    const editor = page.getByRole("dialog", { name: "New scenario" });
    const operations = editor.getByLabel("Operations");
    await operations.selectOption(["createOrder", "getOrder"]);
    // Focus elsewhere: an unfocused listbox paints its selection in the inactive colours.
    await editor.getByLabel("Name").focus();
    // Read what is painted (the browser's selection colours are not in the computed
    // style): the option's left padding gives its background, its most contrasting
    // pixel its text colour.
    const png = decodePng(await page.screenshot());
    const scale = png.width / page.viewportSize()!.width;
    const backgrounds: Record<string, number> = {};
    for (const id of ["createOrder", "getOrder", "listOrders"]) {
      const box = (await operations.locator(`option[value="${id}"]`).boundingBox())!;
      const x0 = Math.round(box.x * scale);
      const y0 = Math.round(box.y * scale);
      const w = Math.round(box.width * scale);
      const h = Math.round(box.height * scale);
      const bg = luminance(png.pixel(x0 + 1, y0 + Math.floor(h / 2)));
      let best = 1;
      for (let y = y0; y < y0 + h; y += 1)
        for (let x = x0; x < x0 + w; x += 1) {
          const l = luminance(png.pixel(x, y));
          best = Math.max(best, (Math.max(l, bg) + 0.05) / (Math.min(l, bg) + 0.05));
        }
      expect(best, `${id}: painted text contrast`).toBeGreaterThanOrEqual(4.5);
      backgrounds[id] = bg;
    }
    // Selected rows must stand out from the rest (WCAG 1.4.11: 3:1 for UI state).
    const unselected = backgrounds.listOrders!;
    for (const id of ["createOrder", "getOrder"]) {
      const sel = backgrounds[id]!;
      const ratio = (Math.max(sel, unselected) + 0.05) / (Math.min(sel, unselected) + 0.05);
      expect(ratio, `${id}: selected background against unselected`).toBeGreaterThanOrEqual(3);
    }
    const results = await new AxeBuilder({ page }).include('[role="dialog"]').withTags(["wcag2a", "wcag2aa"]).analyze();
    const blocking = results.violations.filter((v) => v.impact === "serious" || v.impact === "critical");
    expect(blocking, JSON.stringify(blocking, null, 2)).toEqual([]);
    await request.delete(`${api}/darkselect`);
  });
});
