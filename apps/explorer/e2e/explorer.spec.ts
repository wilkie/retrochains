import { test, expect } from "@playwright/test";

test("compiles a fixture in-browser and verifies it byte-exactly", async ({ page }) => {
  await page.goto("/");
  await expect(page.getByText(/corpus explorer/)).toBeVisible();

  // Narrow to a known fixture and open it.
  await page.getByPlaceholder(/search id or source/).fill("2478-double-not");
  await page.getByText("scalar/2478-double-not-obj").click();

  // It should compile and report a byte-exact match against the golden.
  await expect(page.getByText(/byte-exact ✓/)).toBeVisible({ timeout: 20_000 });

  // The decompiled tab should surface recovered C. The decompiler names functions
  // `f0`, `f1`, … (never `main`), so that text is unique to the decompiled pane.
  await page.getByRole("tab", { name: "decompiled" }).click();
  await expect(page.getByText(/int f\d+\s*\(|recovery bailed on/)).toBeVisible();
});
