import { defineConfig, devices } from "@playwright/test";

// Boots the Vite dev server and drives the explorer in a real browser. Needs the
// browser binaries (`pnpm exec playwright install chromium`) and the wasm modules
// + manifest built. Run with `pnpm run test:e2e`.
export default defineConfig({
  testDir: "./e2e",
  timeout: 60_000,
  use: { baseURL: "http://localhost:5173", trace: "on-first-retry" },
  webServer: {
    command: "npm run dev",
    url: "http://localhost:5173",
    reuseExistingServer: !process.env.CI,
    timeout: 120_000,
  },
  projects: [{ name: "chromium", use: { ...devices["Desktop Chrome"] } }],
});
