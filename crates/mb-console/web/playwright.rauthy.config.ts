import { defineConfig, devices } from "@playwright/test";

// The full login flow against the Compose stack (deploy/compose): Rauthy on
// localhost:8081, Mockingbird's admin port on 9090 and data plane on 8080.
// Start the stack first; there is no webServer here. Runs only in the CI
// `e2e` job, which has Docker.
export default defineConfig({
  testDir: "e2e-rauthy",
  testMatch: "**/*.spec.ts",
  fullyParallel: false,
  workers: 1,
  forbidOnly: !!process.env.CI,
  reporter: process.env.CI ? [["list"], ["html", { open: "never" }]] : "list",
  use: {
    baseURL: "http://localhost:9090",
    trace: "retain-on-failure",
  },
  projects: [
    {
      name: "chromium",
      use: {
        ...devices["Desktop Chrome"],
        launchOptions: { executablePath: process.env.MB_E2E_CHROMIUM },
      },
    },
  ],
});
