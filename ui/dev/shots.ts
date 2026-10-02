// Captures every screen into ui/screenshots/*.png against the shim (serves ui/dist).
//   bun run build && bun dev/shim.ts &  bun dev/shots.ts [baseUrl] [managementKey]
import { chromium, type Page } from "playwright";
import { join } from "node:path";

const base = process.argv[2] ?? "http://127.0.0.1:18318";
// DEGRADED=1: only capture the screens that change without the usage extension (run against SHIM_NO_EXT=1).
const degraded = process.env.DEGRADED === "1";
const key = process.argv[3] ?? "dev-secret";
const out = join(import.meta.dir, "..", "screenshots");

// Set CHROMIUM_PATH when the Playwright-pinned browser revision is not installed.
const browser = await chromium.launch({ executablePath: process.env.CHROMIUM_PATH });
const ctx = await browser.newContext({ viewport: { width: 1440, height: 900 }, deviceScaleFactor: 2, colorScheme: "dark" });
const page = await ctx.newPage();
page.on("dialog", (d) => void d.accept());
const errors: string[] = [];
page.on("pageerror", (e) => errors.push(`pageerror: ${e.message}`));
page.on("console", (m) => m.type() === "error" && errors.push(`console: ${m.text()}`));


/** Select a `key: value` line in the config editor via its search panel and replace the value. */
async function replaceValue(page: Page, key: string, value: string) {
  await page.click(".cm-content");
  await page.keyboard.press("Control+f");
  await page.keyboard.type(`${key}: `);
  await page.keyboard.press("Enter");
  await page.keyboard.press("Escape");
  await page.keyboard.press("ArrowRight");
  await page.keyboard.press("Shift+End");
  await page.keyboard.type(value);
}

async function shot(name: string, settle = 500) {
  await page.waitForTimeout(settle);
  await page.screenshot({ path: join(out, `${name}.png`) });
  console.log(`shot ${name}`);
}

async function open(hash: string, settle = 700) {
  // Same-page hash changes keep component state; bounce through overview first.
  await page.evaluate(() => (location.hash = "#/overview"));
  await page.waitForTimeout(100);
  await page.goto(`${base}/${hash}`);
  await page.waitForTimeout(settle);
}

if (degraded) {
  await page.goto(base);
  await page.evaluate((k) => localStorage.setItem("cpa.management-key", k), key);
  await page.reload();
  await open("#/overview", 1500);
  await shot("10-overview-no-extension", 600);
  await open("#/logs/requests", 1200);
  await shot("10b-requests-no-extension", 300);
  await browser.close();
  process.exit(0);
}

// 1. Login
await page.goto(base);
await page.evaluate(() => localStorage.clear());
await page.reload();
await page.waitForSelector("input[type=password]");
await shot("01-login", 300);
await page.fill("input[type=password]", "wrong-key");
await page.press("input[type=password]", "Enter");
await page.waitForSelector("[role=alert]:not(:empty)");
await shot("01b-login-error", 200);
await page.fill("input[type=password]", key);
await page.press("input[type=password]", "Enter");
await page.waitForSelector("aside nav");

// 2. Overview
await open("#/overview", 1500);
await shot("02-overview", 600);

// 3. Credentials
await open("#/credentials", 900);
await shot("03-credentials");
await page.keyboard.press("j");
await page.keyboard.press("j");
await shot("03b-credentials-keyboard", 200);
await open("#/credentials/codex-margaret@example.com-pro.json", 1000);
await shot("03c-credential-detail");
await open("#/credentials/add", 600);
await shot("03d-credentials-add");
await page.click("text=xAI");
await page.waitForSelector("text=Enter this code");
await shot("03e-credentials-add-device", 400);
await open("#/credentials/add", 500);
await page.click("text=Upload auth JSON");
await shot("03f-credentials-upload", 300);

// 4. API keys
await open("#/keys", 800);
await shot("04-api-keys");
await open("#/keys/add", 500);
await shot("04b-api-keys-add", 300);

// 5. Providers
await open("#/providers/openai-compatibility", 800);
await shot("05-providers");
await open("#/providers/openai-compatibility/0", 800);
await shot("05b-provider-edit");
await open("#/providers/claude", 600);
await shot("05c-providers-empty");

// 6. Models
await open("#/models", 1000);
await shot("06-models");
await open("#/models", 600);
await page.click("[role=tab]:has-text('claude')");
await shot("06b-models-catalog", 800);

// 7. Config
await open("#/config", 1200);
await shot("07-config");
await page.click(".cm-content");
await page.keyboard.press("Control+End");
await page.keyboard.type("\nbroken: [unclosed\n  - nope: : :");
await shot("07b-config-error", 800);

await open("#/config", 1000);
await replaceValue(page, "request-retry", "abc");
await page.keyboard.press("Control+s");
await page.waitForSelector("ul li:has-text('cannot unmarshal')");
await shot("07c-config-server-error", 400);

// 8. Logs
await open("#/logs", 1500);
await shot("08-logs");
await open("#/logs/requests", 1500);
await shot("08b-logs-requests");
await page.click("tbody tr:nth-child(2)");
await shot("08c-logs-request-detail", 700);
await open("#/logs/errors", 800);
await shot("08d-logs-errors");

// Command palette and help
await open("#/overview", 800);
await page.keyboard.press("Control+k");
await page.keyboard.type("cred");
await shot("09-command-palette", 300);
await page.keyboard.press("Escape");
await page.keyboard.press("?");
await shot("09b-shortcuts", 300);

// Narrow viewport
await page.setViewportSize({ width: 820, height: 900 });
await open("#/credentials", 900);
await shot("11-narrow-credentials", 300);
await open("#/overview", 1000);
await shot("11b-narrow-overview", 300);

await browser.close();
if (errors.length) {
  console.log("browser errors:\n" + errors.join("\n"));
  process.exitCode = 1;
}
