import { defineConfig } from "@playwright/test";

export default defineConfig({
  testDir: "./tests",
  forbidOnly: Boolean(process.env.CI),
  retries: process.env.CI ? 2 : 0,
  reporter: process.env.CI ? "github" : "list",
  use: {
    baseURL: "http://127.0.0.1:4173/omnisession/",
    browserName: "chromium",
  },
  webServer: {
    command: "node tests/serve-static.mjs",
    url: "http://127.0.0.1:4173/omnisession/",
    reuseExistingServer: !process.env.CI,
  },
});
