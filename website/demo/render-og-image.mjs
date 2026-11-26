// Renders public/og-image.png in the style of the homepage. Build the site first, then run:
//   node demo/render-og-image.mjs
import { readFileSync } from "node:fs";
import { spawn } from "node:child_process";
import { setTimeout as sleep } from "node:timers/promises";

import { chromium } from "@playwright/test";

const server = spawn(process.execPath, ["tests/serve-static.mjs"], { stdio: "ignore" });
let browser;
try {
  await sleep(800);
  browser = await chromium.launch();
  const page = await browser.newPage({ viewport: { width: 1200, height: 630 }, colorScheme: "light", reducedMotion: "reduce" });
  // The homepage supplies fonts, line colors, and agent bullets. Without scripts, React keeps the new body.
  await page.route("**/*.js", (route) => route.abort());
  await page.goto("http://127.0.0.1:4173/omnisession/");
  const logo = readFileSync("public/logo.svg", "utf8");
  await page.evaluate((svg) => {
    const thread = document.querySelector(".thread")?.outerHTML ?? "";
    document.body.innerHTML = `
      <main class="card">
        <header>${svg}<strong>OmniSession</strong><span>omni</span></header>
        <h1>Switch agents.<span>Keep the thread.</span></h1>
        <p>Find any coding-agent session. Continue it in any agent.</p>
        ${thread}
      </main>`;
  }, logo);
  await page.addStyleTag({
    content: `
      body { width: 1200px; height: 630px; overflow: hidden; }
      .card { position: relative; width: 1200px; height: 630px; overflow: hidden; background: var(--page); }
      .card header { display: flex; align-items: center; gap: 18px; height: 92px; padding: 8px 64px 0; border-top: 8px solid var(--sign-ink); background: var(--sign); color: var(--sign-ink); font-size: 30px; box-shadow: inset 0 0 0 0; }
      .card header svg { width: 50px; height: 50px; }
      .card header strong { font-weight: 800; letter-spacing: -0.02em; }
      .card header span { margin-left: auto; color: var(--sign-ink-2); font-family: var(--font-mono); font-size: 26px; }
      .card h1 { margin: 50px 64px 0; font-size: 112px; }
      .card > p { margin: 24px 64px 0; color: var(--ink-2); font-size: 31px; }
      .card .thread { position: absolute; left: 64px; right: 0; bottom: 34px; margin: 0; }
      .card .thread-stop { font-size: 22px; }
      .card .thread-stop > span:last-child { font-size: 0; }
      .card .thread-stop strong { font-size: 23px; }
      .card .thread-stop .bullet { scale: 1.3; transform-origin: left center; box-shadow: 0 0 0 4px var(--page); }
      .card .thread-stop::before { top: 11px; height: 14px; }
      .card .thread-stop { gap: 16px; }
    `,
  });
  await page.evaluate(() => document.fonts.ready);
  await page.waitForTimeout(400);
  await page.screenshot({ path: "public/og-image.png" });
} finally {
  await browser?.close();
  server.kill();
}
