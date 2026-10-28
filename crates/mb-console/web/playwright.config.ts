import { defineConfig, devices } from "@playwright/test";

const baseURL = "http://127.0.0.1:19090";

export default defineConfig({
  testDir: "e2e",
  testMatch: "**/*.spec.ts",
  fullyParallel: false,
  workers: 1,
  forbidOnly: !!process.env.CI,
  reporter: process.env.CI ? [["list"], ["html", { open: "never" }]] : "list",
  use: {
    baseURL,
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
  webServer: {
    command:
      "cargo run -q --manifest-path ../../../Cargo.toml -p mockingbird-cli -- serve -c e2e/mockingbird.e2e.yaml --offline",
    url: `${baseURL}/readyz`,
    timeout: 600_000,
    reuseExistingServer: !process.env.CI,
    stdout: "pipe",
    stderr: "pipe",
  },
});
