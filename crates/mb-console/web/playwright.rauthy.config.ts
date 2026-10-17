import { defineConfig, devices } from "@playwright/test";

// The login flows against the Compose stack (deploy/compose): Rauthy on
// localhost:8081, Mockingbird's admin port on 9090 and data plane on 8080.
// Start the stack first; there is no webServer here. Runs only in the CI
// `e2e` job, which has Docker. e2e-rauthy/ holds the full login flow,
// e2e-compose/flow.spec.ts the Compose e2e flow (spec §15).
export default defineConfig({
  testDir: ".",
  testMatch: ["e2e-rauthy/**/*.spec.ts", "e2e-compose/flow.spec.ts"],
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
