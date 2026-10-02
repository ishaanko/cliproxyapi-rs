// End-to-end run of the mutating flows against the (throwaway) server through the
// real UI. Writes ui/screenshots/e2e-report.txt as the repeatable artifact.
//   bun run build && bun dev/shim.ts &  bun dev/e2e.ts [baseUrl] [managementKey]
import { chromium, type Page } from "playwright";
import { mkdtemp, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";

const base = process.argv[2] ?? "http://127.0.0.1:18318";
const key = process.argv[3] ?? "dev-secret";
const report: string[] = [];

const browser = await chromium.launch({ executablePath: process.env.CHROMIUM_PATH });
const page = await (await browser.newContext({ viewport: { width: 1440, height: 900 } })).newPage();
await page.addInitScript((k) => localStorage.setItem("cpa.management-key", k), key);

async function api<T>(path: string): Promise<T> {
  const res = await fetch(`${base}/v8/management${path}`, { headers: { Authorization: `Bearer ${key}` } });
  if (!res.ok) throw new Error(`${path}: ${res.status}`);
  return res.json() as Promise<T>;
}

async function step(name: string, fn: () => Promise<void>) {
  try {
    await fn();
    report.push(`PASS ${name}`);
  } catch (e) {
    report.push(`FAIL ${name}: ${e instanceof Error ? e.message.split("\n")[0] : String(e)}`);
  }
}

page.on("dialog", (d) => void d.accept());

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

const go = async (hash: string) => {
  await page.goto(`${base}/#/overview`);
  await page.goto(`${base}/${hash}`);
};
const confirmDialog = (label: string) => page.locator("dialog[open] button", { hasText: label }).last().click();

async function expectEq<T>(actual: T, expected: T, what: string) {
  if (JSON.stringify(actual) !== JSON.stringify(expected)) throw new Error(`${what}: got ${JSON.stringify(actual)}, want ${JSON.stringify(expected)}`);
}

await step("api key: add via UI persists to config", async () => {
  await go("#/keys/add");
  await page.fill("dialog[open] input", "sk-e2e-test-key-0001");
  await page.click("dialog[open] button:has-text('Add')");
  await page.waitForSelector("td:has-text('sk-e2e-t')");
  const keys = await api<string[]>("/config/access/api-keys");
  await expectEq(keys.includes("sk-e2e-test-key-0001"), true, "key in config");
});

await step("api key: remove via UI", async () => {
  await page.locator("tr", { hasText: "sk-e2e-t" }).hover();
  await page.locator("tr", { hasText: "sk-e2e-t" }).getByLabel("Remove").click();
  await confirmDialog("Remove");
  await page.waitForSelector("td:has-text('sk-e2e-t')", { state: "detached" });
  const keys = await api<string[]>("/config/access/api-keys");
  await expectEq(keys.includes("sk-e2e-test-key-0001"), false, "key removed");
});

const dir = await mkdtemp(join(tmpdir(), "cpa-e2e-"));
const credFile = join(dir, "claude-e2e@example.com.json");
await writeFile(credFile, JSON.stringify({ type: "claude", email: "e2e@example.com", access_token: "x", refresh_token: "y", expired: "2099-01-01T00:00:00Z" }));

await step("credential: upload JSON appears in list", async () => {
  await go("#/credentials/add");
  await page.click("dialog[open] button:has-text('Upload auth JSON')");
  await page.locator("dialog[open] input[type=file]").first().setInputFiles(credFile);
  await page.waitForSelector("dialog[open] li:has-text('claude-e2e@example.com.json')");
  await page.click("dialog[open] button:has-text('Done')");
  await page.waitForSelector("td:has-text('e2e@example.com')");
});

await step("credential: disable toggle persists", async () => {
  await page.locator("tr", { hasText: "e2e@example.com" }).getByRole("switch").click();
  await page.waitForSelector("tr:has-text('e2e@example.com') >> text=Disabled");
  const list = await api<{ files: { name: string; disabled: boolean }[] }>("/credentials");
  await expectEq(list.files.find((f) => f.name === "claude-e2e@example.com.json")?.disabled, true, "disabled");
});

await step("credential: edit routing note via sheet", async () => {
  await page.locator("tr", { hasText: "e2e@example.com" }).click();
  await page.fill("dialog[open] input >> nth=3", "from e2e");
  await page.click("dialog[open] button:has-text('Save')");
  await page.waitForSelector("text=Saved");
  const list = await api<{ files: { name: string; note?: string }[] }>("/credentials");
  await expectEq(list.files.find((f) => f.name === "claude-e2e@example.com.json")?.note, "from e2e", "note");
});

await step("credential: delete via sheet", async () => {
  await page.click("dialog[open] button:has-text('Delete')");
  await confirmDialog("Delete");
  await page.waitForSelector("td:has-text('e2e@example.com')", { state: "detached" });
  const list = await api<{ files: { name: string }[] }>("/credentials");
  await expectEq(list.files.some((f) => f.name === "claude-e2e@example.com.json"), false, "file removed");
});

await step("oauth: claude login shows URL, cancel releases session", async () => {
  await go("#/credentials/add");
  await page.click("dialog[open] button:has-text('Claude')");
  await page.waitForSelector("dialog[open] a:has-text('Open')");
  const href = await page.getAttribute("dialog[open] a:has-text('Open')", "href");
  await expectEq(href?.startsWith("https://claude.ai/oauth/authorize"), true, "authorize url");
  const state = new URL(href ?? "").searchParams.get("state") ?? "";
  await page.click("dialog[open] button:has-text('Cancel')");
  await page.waitForTimeout(500);
  const status = await api<{ status: string; error?: string }>(`/oauth/status?state=${state}`);
  await expectEq(status.status, "error", "session gone after cancel");
});

await step("oauth: device flow shows user code", async () => {
  await go("#/credentials/add");
  await page.click("dialog[open] button:has-text('xAI')");
  await page.waitForSelector("dialog[open] .mono.text-\\[28px\\]");
  await page.click("dialog[open] button:has-text('Cancel')");
});

await step("provider: add claude entry via sheet, then delete", async () => {
  await go("#/providers/claude/add");
  await page.fill("dialog[open] input >> nth=0", "e2e-claude");
  await page.click("dialog[open] button:has-text('Add') >> nth=0");
  await page.fill("dialog[open] input[aria-label='API key']", "sk-ant-e2e-0001");
  await page.click("dialog[open] button:has-text('Save')");
  await page.waitForSelector("td:has-text('e2e-claude')");
  const groups = await api<{ claude?: { name: string }[] }>("/config/api-keys");
  await expectEq(groups.claude?.some((e) => e.name === "e2e-claude"), true, "in config");
  await page.locator("tr", { hasText: "e2e-claude" }).hover();
  await page.locator("tr", { hasText: "e2e-claude" }).getByLabel("Delete").click();
  await confirmDialog("Delete");
  await page.waitForSelector("td:has-text('e2e-claude')", { state: "detached" });
});

await step("config: invalid YAML blocks save and shows problem", async () => {
  await go("#/config");
  await page.click(".cm-content");
  await page.keyboard.press("Control+End");
  await page.keyboard.type("\nbroken: [unclosed");
  await page.waitForSelector("footer:has-text('syntax error')");
  await expectEq(await page.locator("button:has-text('Save')").first().isDisabled(), true, "save disabled");
});

await step("config: server-side rejection is surfaced with line", async () => {
  await go("#/config");
  await replaceValue(page, "request-retry", "abc");
  await page.keyboard.press("Control+s");
  await page.waitForSelector("ul li:has-text('cannot unmarshal')", { timeout: 5000 });
});

await step("config: valid edit saves, persists, and round-trips", async () => {
  for (const value of ["2", "0"]) {
    await go("#/config");
    await replaceValue(page, "request-retry", value);
    await page.keyboard.press("Control+s");
    await page.waitForSelector("text=Config saved");
    await expectEq(await api<number>("/config/routing/retry/request-retry"), Number(value), "request-retry");
  }
});

await step("logs: application log renders lines", async () => {
  await go("#/logs");
  await page.waitForSelector("div.mono:has-text('gin_logger')");
});

await browser.close();
const failed = report.filter((l) => l.startsWith("FAIL")).length;
report.push(`\n${report.length - failed} passed, ${failed} failed`);
const text = report.join("\n") + "\n";
await writeFile(join(import.meta.dir, "..", "screenshots", "e2e-report.txt"), text);
console.log(text);
process.exitCode = failed ? 1 : 0;
