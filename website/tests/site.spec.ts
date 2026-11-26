import { expect, test, type Page } from "@playwright/test";

import { providers } from "../app/providers.generated";

const siteUrl = "https://bvolpato.github.io/omnisession/";
const heroTitle = "Switch agents. Keep the thread.";
const unixCommand = "curl -fsSL https://raw.githubusercontent.com/bvolpato/omnisession/main/install.sh | sh";
const windowsCommand = "irm https://raw.githubusercontent.com/bvolpato/omnisession/main/install.ps1 | iex";
const capabilityKeys = ["read_index", "clean_start", "same_provider_resume", "cross_provider_import"] as const;
const platformNames = [["linux", "Linux"], ["macos", "macOS"], ["windows", "Windows"]] as const;

test.use({ permissions: ["clipboard-read", "clipboard-write"] });

function platformScope(platforms: readonly string[]) {
  if (platforms.length === 0) return "Not guaranteed";
  return platformNames.filter(([id]) => platforms.includes(id)).map(([, name]) => name).join(" + ");
}

function collectRuntimeErrors(page: Page) {
  const errors: string[] = [];
  page.on("console", (message) => {
    if (message.type() === "error") errors.push(message.text());
  });
  page.on("pageerror", (error) => errors.push(error.message));
  return errors;
}

async function expectNoPageOverflow(page: Page) {
  const hasPageOverflow = await page.evaluate(() => document.documentElement.scrollWidth > document.documentElement.clientWidth);
  expect(hasPageOverflow).toBe(false);
}

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
  const runtimeErrors = collectRuntimeErrors(page);
  await page.addInitScript(() => {
    Object.defineProperty(navigator, "userAgent", {
      configurable: true,
      get: () => "Mozilla/5.0 (Windows NT 10.0; Win64; x64)",
    });
  });

  const response = await page.goto("./");
  expect(response?.ok()).toBe(true);
  await expect(page).toHaveTitle("OmniSession | Continue any session in any agent");
  await expect(page.locator('link[rel="canonical"]')).toHaveAttribute("href", siteUrl);
  await expect(page.locator('meta[property="og:image"]')).toHaveAttribute("content", `${siteUrl}og-image.png`);
  await expect(page.locator('meta[name="twitter:card"]')).toHaveAttribute("content", "summary_large_image");
  const socialImage = await page.request.get("og-image.png");
  expect(socialImage.ok()).toBe(true);
  expect(socialImage.headers()["content-type"]).toBe("image/png");

  await expect(page.getByRole("banner")).toBeVisible();
  await expect(page.getByRole("main")).toBeVisible();
  await expect(page.getByRole("contentinfo")).toBeVisible();
  await expect(page.getByRole("heading", { level: 1 })).toHaveText(heroTitle);

  const sections = [
    ["map", "Change here for any agent."],
    ["transfer", "What happens when you press Enter."],
    ["search", "Find the one you half remember."],
    ["safety", "Your originals stay original."],
    ["agents", `${providers.length} agents, platform by platform.`],
    ["install", "One binary. One line."],
    ["faq", "Questions."],
  ] as const;
  for (const [id, heading] of sections) {
    await expect(page.locator(`section#${id}`).getByRole("heading", { level: 2 })).toHaveText(heading);
  }
  await expect(page.locator("#transfer ol").getByRole("listitem")).toHaveCount(4);

  const supportRows = page.getByRole("table", { name: "Agent support by platform" }).getByRole("row");
  await expect(supportRows).toHaveCount(providers.length + 1);
  for (const [index, provider] of providers.entries()) {
    const row = supportRows.nth(index + 1);
    await expect(row.getByRole("rowheader")).toHaveText(provider.name);
    const cells = row.getByRole("cell");
    await expect(cells.nth(0)).toHaveText(provider.signal);
    for (const [capabilityIndex, key] of capabilityKeys.entries()) {
      await expect(cells.nth(capabilityIndex + 1).locator(".platform-scope")).toHaveText(platformScope(provider.capabilities[key]));
    }
  }

  const bodyText = await page.locator("body").innerText();
  expect(bodyText).not.toMatch(/FIRST MESSAGE|LATEST MESSAGE|VISIBLE TRAJECTORY/i);
  expect(bodyText).not.toMatch(/^\s*(user|assistant|human|system)\s*[:›>·]/im);

  await expectImagesLoaded(page);

  const hero = page.getByRole("region", { name: heroTitle });
  await hero.getByRole("button", { name: "Copy install command" }).click();
  await expect(hero.getByRole("button", { name: "Copy install command" })).toContainText("Copied");
  expect(await page.evaluate(() => navigator.clipboard.readText())).toBe(unixCommand);

  await page.getByRole("banner").getByRole("link", { name: "Install", exact: true }).click();
  await expect(page).toHaveURL(/#install$/);
  const install = page.locator("section#install");
  const unixTab = install.getByRole("tab", { name: "macOS / Linux" });
  const windowsTab = install.getByRole("tab", { name: "Windows x86-64 preview" });
  const commandPanel = install.getByRole("tabpanel");
  const copyButton = install.getByRole("button", { name: "Copy install command" });
  await expect(unixTab).toHaveAttribute("aria-selected", "true");
  await expect(commandPanel).toContainText(unixCommand);
  await expect(commandPanel).not.toContainText("install.ps1");
  await expect(install.locator(".install-note")).toContainText("keep provider commands native");

  await unixTab.focus();
  await page.keyboard.press("ArrowRight");
  await expect(windowsTab).toBeFocused();
  await expect(windowsTab).toHaveAttribute("aria-selected", "true");
  await expect(commandPanel).toContainText(windowsCommand);
  await expect(copyButton).toHaveText("Copy");
  await copyButton.click();
  await expect(copyButton).toContainText("Copied");
  expect(await page.evaluate(() => navigator.clipboard.readText())).toBe(windowsCommand);
  await expect(install.locator(".install-note")).toContainText("provider commands remain native");

  const faq = page.locator("section#faq");
  const firstQuestion = faq.locator("details").first();
  await expect(firstQuestion).not.toHaveAttribute("open", "");
  await firstQuestion.locator("summary").click();
  await expect(firstQuestion).toHaveAttribute("open", "");

  await expectNoPageOverflow(page);
  expect(runtimeErrors).toEqual([]);
});

test("mobile layout stays within viewport", async ({ page }) => {
  const runtimeErrors = collectRuntimeErrors(page);

  await page.setViewportSize({ width: 390, height: 844 });
  await page.emulateMedia({ colorScheme: "light", reducedMotion: "reduce" });
  await page.goto("./");
  await expectNoPageOverflow(page);
  await expect(page.getByRole("heading", { level: 1 })).toBeVisible();
  await expect(page.getByRole("region", { name: heroTitle }).getByRole("button", { name: "Copy install command" })).toBeVisible();
  await expect(page.getByRole("banner").getByRole("link", { name: "Install", exact: true })).toBeVisible();
  await expect(page.getByRole("table", { name: "Agent support by platform" }).getByRole("row")).toHaveCount(providers.length + 1);
  const install = page.locator("section#install");
  await install.getByRole("tab", { name: "Windows x86-64 preview" }).click();
  await expect(install.getByRole("tabpanel")).toContainText("install.ps1");
  await expectNoPageOverflow(page);
  expect(runtimeErrors).toEqual([]);
});

test("picker demo searches, changes agent, and leaves Tab free", async ({ page }) => {
  const runtimeErrors = collectRuntimeErrors(page);
  await page.emulateMedia({ reducedMotion: "reduce" });
  await page.goto("./");
  const demo = page.getByRole("group", { name: "Interactive omni demo with invented sessions" });
  const input = demo.getByRole("combobox");
  await input.focus();
  await page.keyboard.type("redis");
  await expect(demo.getByRole("option", { selected: true })).toContainText("Share rate-limit buckets across pods with Redis");

  await page.keyboard.press("Enter");
  await expect(demo.getByText("Where should this session open?")).toBeVisible();
  await page.keyboard.type("claude");
  await expect(demo.getByRole("option")).toHaveCount(1);
  await page.keyboard.press("Enter");
  await expect(demo.getByText("Importing 54 trajectory items into Claude...")).toBeVisible();
  await expect(demo.getByText("23 messages carried over, 31 tool records kept as history")).toBeVisible();

  await page.keyboard.press("Enter");
  await page.keyboard.type("qwen3.8");
  await expect(demo.getByText("Matched in conversation text")).toBeVisible();
  await page.keyboard.press("Tab");
  await expect(demo.getByRole("button", { name: "Play the tour" })).toBeFocused();
  expect(runtimeErrors).toEqual([]);
});

test("picker tour plays, pauses, and stops at the first input", async ({ page }) => {
  await page.goto("./");
  const demo = page.getByRole("group", { name: "Interactive omni demo with invented sessions" });
  await demo.scrollIntoViewIfNeeded();
  await demo.getByRole("button", { name: "Pause" }).click();
  const paused = await demo.getByRole("combobox").inputValue();
  await page.waitForTimeout(2500);
  await expect(demo.getByRole("combobox")).toHaveValue(paused);

  await demo.getByRole("button", { name: "Play the tour" }).click();
  await expect(demo.getByText("codex:4f7fac05… is unchanged.")).toBeVisible({ timeout: 20_000 });
});

test("route map reports the manifest route for the chosen agents", async ({ page }) => {
  await page.emulateMedia({ reducedMotion: "reduce" });
  await page.goto("./");
  const map = page.locator("section#map");
  for (const target of providers.filter((provider) => provider.capabilities.cross_provider_import.length > 0 && provider.id !== "codex")) {
    await map.locator(`input[name="route-to"][value="${target.id}"]`).check({ force: true });
    await expect(map.getByRole("heading", { level: 3 })).toHaveText(`Codex to ${target.name}`);
    await expect(map.locator(".route-sign dl")).toContainText(target.cross);
    await expect(map.locator(".route-sign dl")).toContainText(platformScope(target.capabilities.cross_provider_import));
  }
  await map.locator('input[name="route-from"][value="grok"]').check({ force: true });
  await map.locator('input[name="route-to"][value="opencode"]').check({ force: true });
  await map.getByRole("button", { name: "Copy route command" }).click();
  expect(await page.evaluate(() => navigator.clipboard.readText())).toBe("omni resume grok:<session-id> --in opencode");
});
