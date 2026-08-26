import { expect, test } from "@playwright/test";

test("exported site loads and hydrates under GitHub Pages base path", async ({ page }) => {
  const runtimeErrors: string[] = [];
  page.on("console", (message) => {
    if (message.type() === "error") runtimeErrors.push(message.text());
  });
  page.on("pageerror", (error) => runtimeErrors.push(error.message));

  const response = await page.goto("./");
  expect(response?.ok()).toBe(true);
  await expect(page).toHaveTitle("OmniSession | Continue coding sessions across agents");
  await expect(page.getByRole("heading", { level: 1 })).toContainText("Continue the work");
  await expect(page.getByRole("table", { name: "Provider support" }).getByRole("row")).toHaveCount(10);

  const imagesLoaded = await page.locator("img").evaluateAll((images) =>
    images.every((image) => image instanceof HTMLImageElement && image.complete && image.naturalWidth > 0),
  );
  expect(imagesLoaded).toBe(true);

  await page.getByRole("link", { name: "Install omni" }).click();
  await expect(page).toHaveURL(/#install$/);
  await page.getByRole("button", { name: "Copy install command" }).click();
  await expect(page.getByRole("button", { name: "Copy install command" })).toContainText("Copied");
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
  expect(runtimeErrors).toEqual([]);
});
