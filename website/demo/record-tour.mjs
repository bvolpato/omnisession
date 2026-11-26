// Records the homepage demo as the README GIF. Build the site first, then run:
//   node demo/record-tour.mjs
// Needs ffmpeg. Each key press becomes one frame, so a run always gives the same timing.
import { execFileSync, spawn } from "node:child_process";
import { mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { setTimeout as sleep } from "node:timers/promises";

import { chromium } from "@playwright/test";

const output = "public/demo/omni-tour.gif";
const url = "http://127.0.0.1:4173/omnisession/";

const steps = [
  { hold: 1.6 },
  ...[..."redis"].map((key) => ({ key, hold: 0.14 })),
  { hold: 1.5 },
  { key: "Enter", hold: 1.2 },
  { key: "ArrowDown", hold: 0.35 },
  { key: "ArrowDown", hold: 1.6 },
  { key: "Enter", hold: 0.5 },
  { line: "Importing", hold: 0.8 },
  { line: "Verifying", hold: 0.8 },
  { line: "Imported and verified", hold: 0.7 },
  { line: "Created and verified", hold: 0.9 },
  { line: "carried over", hold: 4.5 },
];

const frames = mkdtempSync(join(tmpdir(), "omni-tour-"));
const server = spawn(process.execPath, ["tests/serve-static.mjs"], { stdio: "ignore" });
let browser;
try {
  await sleep(800);
  browser = await chromium.launch();
  const page = await browser.newPage({ viewport: { width: 1320, height: 1300 }, colorScheme: "dark", reducedMotion: "reduce" });
  await page.goto(url);
  await page.evaluate(() => document.fonts.ready);
  const demo = page.locator(".demo-window");
  await demo.getByRole("combobox").focus();
  await page.addStyleTag({ content: ".demo-window { outline: 0 !important; box-shadow: none !important; border-radius: 0 !important; } .demo-live { visibility: hidden; }" });

  const list = [];
  for (const [index, step] of steps.entries()) {
    if (step.key) await page.keyboard.press(step.key);
    if (step.line) await demo.getByText(step.line).first().waitFor();
    const file = join(frames, `${String(index).padStart(3, "0")}.png`);
    await demo.screenshot({ path: file });
    list.push(`file '${file}'`, `duration ${step.hold}`);
  }
  list.push(list.at(-2));
  const script = join(frames, "frames.txt");
  writeFileSync(script, list.join("\n"));
  execFileSync("ffmpeg", [
    "-y", "-loglevel", "error", "-f", "concat", "-safe", "0", "-i", script,
    "-vf", "fps=20,mpdecimate,split[a][b];[a]palettegen=max_colors=96:stats_mode=full[p];[b][p]paletteuse=dither=none",
    "-fps_mode", "vfr", "-loop", "0", "-final_delay", "450", output,
  ]);
} finally {
  await browser?.close();
  server.kill();
  rmSync(frames, { recursive: true, force: true });
}
