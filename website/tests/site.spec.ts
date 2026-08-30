import { expect, test, type Page } from "@playwright/test";

async function expectImagesLoaded(page: Page) {
  const images = page.locator("img");
  const imageCount = await images.count();

  for (let index = 0; index < imageCount; index += 1) {
    const image = images.nth(index);
    await image.scrollIntoViewIfNeeded();
    await expect
      .poll(
        () =>
          image.evaluate((element) => {
            if (!(element instanceof HTMLImageElement)) return "not-image";
            if (!element.complete) return "loading";
            return element.naturalWidth > 0 ? "loaded" : "broken";
          }),
        { message: `image ${index + 1}/${imageCount} did not load` },
      )
      .toBe("loaded");
  }
}

test("exported site loads and hydrates under GitHub Pages base path", async ({ page }) => {
  const runtimeErrors: string[] = [];
  page.on("console", (message) => {
    if (message.type() === "error") runtimeErrors.push(message.text());
  });
  page.on("pageerror", (error) => runtimeErrors.push(error.message));
  await page.addInitScript(() => {
    Object.defineProperty(navigator, "userAgent", {
      configurable: true,
      get: () => "Mozilla/5.0 (Windows NT 10.0; Win64; x64)",
    });
  });

  const response = await page.goto("./");
  expect(response?.ok()).toBe(true);
  await expect(page).toHaveTitle("OmniSession | Continue coding sessions across agents");
  await expect(page.getByRole("heading", { level: 1 })).toContainText("Continue the work");
  const supportRows = page.getByRole("table", { name: "Provider support" }).getByRole("row");
  await expect(supportRows).toHaveCount(10);
  await expect(supportRows.nth(1)).toContainText("Codex");
  await expect(supportRows.nth(2)).toContainText("Claude Code");
  const antigravity = supportRows.filter({ hasText: "Antigravity CLI" });
  await expect(antigravity).toContainText("Linux + macOS");
  await expect(antigravity).toContainText("Linux");
  await expect(antigravity).toContainText("Read Linux + macOS");
  await expect(antigravity).toContainText("Start Linux + macOS");
  await expect(supportRows.filter({ hasText: "Cursor IDE" })).toContainText("Start Not guaranteed");

  await expectImagesLoaded(page);

  await page.getByRole("link", { name: "Install omni" }).click();
  await expect(page).toHaveURL(/#install$/);
  const linuxTab = page.getByRole("tab", { name: "Linux / macOS" });
  const windowsTab = page.getByRole("tab", { name: "Windows x86-64 Preview" });
  const commandPanel = page.getByRole("tabpanel");
  await expect(linuxTab).toHaveAttribute("aria-selected", "true");
  await expect(commandPanel).toContainText("curl -fsSL");
  await expect(commandPanel).not.toContainText("install.ps1");
  await page.getByRole("button", { name: "Copy install command" }).click();
  await expect(page.getByRole("button", { name: "Copy install command" })).toContainText("Copied");
  await windowsTab.click();
  await expect(windowsTab).toHaveAttribute("aria-selected", "true");
  await expect(commandPanel).toContainText("irm https://raw.githubusercontent.com/bvolpato/omnisession/main/install.ps1 | iex");
  await expect(page.getByRole("button", { name: "Copy install command" })).toHaveText("Copy");
  await expect(page.getByText("provider fidelity remains provisional", { exact: false })).toBeVisible();
  const hasPageOverflow = await page.evaluate(() => document.documentElement.scrollWidth > document.documentElement.clientWidth);
  expect(hasPageOverflow).toBe(false);
  expect(runtimeErrors).toEqual([]);
});

test("mobile layout stays within viewport", async ({ page }) => {
  const runtimeErrors: string[] = [];
  page.on("console", (message) => {
    if (message.type() === "error") runtimeErrors.push(message.text());
  });
  page.on("pageerror", (error) => runtimeErrors.push(error.message));

  await page.setViewportSize({ width: 390, height: 844 });
  await page.goto("./");
  const hasPageOverflow = await page.evaluate(() => document.documentElement.scrollWidth > document.documentElement.clientWidth);
  expect(hasPageOverflow).toBe(false);
  await expect(page.getByRole("heading", { level: 1 })).toBeVisible();
  await expect(page.getByRole("button", { name: "Copy install command" })).toBeVisible();
  await page.getByRole("tab", { name: "Windows x86-64 Preview" }).click();
  await expect(page.getByRole("tabpanel")).toContainText("install.ps1");
  const hasWindowsPageOverflow = await page.evaluate(() => document.documentElement.scrollWidth > document.documentElement.clientWidth);
  expect(hasWindowsPageOverflow).toBe(false);
  expect(runtimeErrors).toEqual([]);
});
