// End-to-end run of the quota, overview and tools screens against the Rust server, with
// traffic from dev/fake-upstream.ts. Writes ui/screenshots/quotas-e2e-report.txt plus one
// screenshot per screen as the repeatable artifact.
//
// OAuth accounts call the real provider hosts, so two responses are injected at the browser
// and nothing leaves the machine: quota signals on the seeded OAuth accounts (in the exact
// canonical-header form the server reports) and /requests/api-call answers for "Check".
//   bun dev/fake-upstream.ts &  cliproxy --config <dev config> &  bun dev/seed.ts <authDir> <base> 0
//   bun dev/e2e-quotas.ts [baseUrl] [managementKey]
import { chromium, type Page, type Route } from "playwright";
import { writeFile } from "node:fs/promises";
import { join } from "node:path";

const base = process.argv[2] ?? "http://127.0.0.1:18530";
const key = process.argv[3] ?? "dev-secret";
const out = join(import.meta.dir, "..", "screenshots");
const report: string[] = [];
const clientKey = "sk-dev-client-aaaa1111";

async function step(name: string, fn: () => Promise<void>) {
  try {
    await fn();
    report.push(`PASS ${name}`);
  } catch (e) {
    report.push(`FAIL ${name}: ${e instanceof Error ? e.message.split("\n")[0] : String(e)}`);
  }
}
function expect(cond: boolean, what: string) {
  if (!cond) throw new Error(what);
}

// Real traffic so totals, the dot chart and the request feed have data.
const models = ["fake-claude", "fake-gpt", "llama-70b"];
let sent = 0;
await Promise.all(
  Array.from({ length: 6 }, async (_, w) => {
    for (let i = w; i < 120; i += 6) {
      await fetch(`${base}/v1/chat/completions`, {
        method: "POST",
        headers: { "content-type": "application/json", authorization: `Bearer ${i % 2 ? clientKey : "sk-dev-client-bbbb2222"}` },
        body: JSON.stringify({ model: models[i % models.length], messages: [{ role: "user", content: "hi" }] }),
      }).catch(() => null);
      sent++;
    }
  }),
);
report.push(`INFO sent ${sent} requests through ${base}`);

const now = Math.floor(Date.now() / 1000);
const observed = new Date().toISOString();
// Signals as the server stores them: Go-canonical header names, string values.
const signals: Record<string, Record<string, string>> = {
  "ada@example.com": {
    "Anthropic-Ratelimit-Unified-5h-Utilization": "0.38",
    "Anthropic-Ratelimit-Unified-5h-Reset": String(now + 2 * 3600 + 40 * 60 + 30),
    "Anthropic-Ratelimit-Unified-5h-Status": "allowed",
    "Anthropic-Ratelimit-Unified-7d-Utilization": "0.52",
    "Anthropic-Ratelimit-Unified-7d-Reset": String(now + 3 * 86400 + 60),
  },
  "linus@example.com": {
    "X-Codex-Primary-Used-Percent": "8",
    "X-Codex-Primary-Window-Minutes": "300",
    "X-Codex-Primary-Reset-After-Seconds": String(4 * 3600 + 13 * 60 + 30),
    "X-Codex-Secondary-Used-Percent": "61",
    "X-Codex-Secondary-Window-Minutes": "10080",
    "X-Codex-Secondary-Reset-After-Seconds": String(27 * 3600 + 13 * 60 + 30),
  },
  "margaret@example.com": {
    "X-Codex-Primary-Used-Percent": "93",
    "X-Codex-Primary-Window-Minutes": "300",
    "X-Codex-Primary-Reset-After-Seconds": "1500",
    // Expired weekly window: its allowance is back, so the UI must drop it.
    "X-Codex-Secondary-Used-Percent": "99",
    "X-Codex-Secondary-Window-Minutes": "10080",
    "X-Codex-Secondary-Reset-At": String(now - 60),
  },
};

interface Cred {
  email?: string;
  auth_index: string;
  quota?: { observed_at?: string; signals: Record<string, string> };
}
const real = (await (await fetch(`${base}/v8/management/credentials`, { headers: { Authorization: `Bearer ${key}` } })).json()) as { files: Cred[] };
const indexOf = (email: string) => real.files.find((f) => f.email === email)?.auth_index ?? "";
report.push(`INFO server lists ${real.files.length} credentials`);

// Usage endpoint answers for "Check", keyed by auth_index.
const usage: Record<string, { status_code: number; body: string }> = {
  [indexOf("ada@example.com")]: {
    status_code: 200,
    body: JSON.stringify({
      limits: [
        { group: "a", kind: "session", percent: 41, resets_at: new Date(Date.now() + 2.5 * 3600e3).toISOString() },
        { group: "a", kind: "weekly_all", percent: 55, resets_at: new Date(Date.now() + 3 * 86400e3).toISOString() },
        { group: "a", kind: "weekly_scoped", percent: 71, scope: { model: { display_name: "Opus" } }, resets_at: new Date(Date.now() + 3 * 86400e3).toISOString() },
      ],
      extra_usage: { is_enabled: true, used_credits: 640, monthly_limit: 2000, decimal_places: 2, currency: "USD" },
    }),
  },
  [indexOf("linus@example.com")]: {
    status_code: 200,
    body: JSON.stringify({
      rate_limit: {
        primary_window: { used_percent: 9, limit_window_seconds: 18000, reset_after_seconds: 15000 },
        secondary_window: { used_percent: 62, limit_window_seconds: 604800, reset_after_seconds: 97000 },
      },
    }),
  },
  [indexOf("margaret@example.com")]: { status_code: 401, body: '{"error":"invalid token"}' },
};
const apiCalls: { authIndex: string; url: string; header: Record<string, string> }[] = [];

const browser = await chromium.launch({ executablePath: process.env.CHROMIUM_PATH });
const ctx = await browser.newContext({ viewport: { width: 1440, height: 900 }, deviceScaleFactor: 2, colorScheme: "dark" });
await ctx.grantPermissions(["clipboard-read", "clipboard-write"], { origin: base });
const page = await ctx.newPage();
await page.addInitScript((k) => localStorage.setItem("cpa.management-key", k), key);
const errors: string[] = [];
page.on("pageerror", (e) => errors.push(`pageerror: ${e.message}`));
page.on("console", (m) => m.type() === "error" && errors.push(`console: ${m.text()}`));

await page.route("**/v8/management/credentials*", async (route: Route) => {
  if (route.request().method() !== "GET") return route.continue();
  const res = await route.fetch();
  const body = (await res.json()) as { files: Cred[] };
  for (const f of body.files) {
    const s = f.email ? signals[f.email] : undefined;
    if (s) f.quota = { observed_at: observed, signals: s };
  }
  await route.fulfill({ response: res, json: body });
});
await page.route("**/v8/management/requests/api-call", async (route: Route) => {
  const req = route.request().postDataJSON() as { authIndex: string; url: string; header: Record<string, string> };
  apiCalls.push(req);
  const answer = usage[req.authIndex] ?? { status_code: 404, body: "{}" };
  await route.fulfill({ json: { ...answer, header: {} } });
});

async function open(hash: string) {
  await page.goto(`${base}/management.html${hash}`);
  await page.waitForLoadState("networkidle");
  await page.waitForTimeout(400);
}
const shot = (name: string) => page.screenshot({ path: join(out, `quotas-e2e-${name}.png`), fullPage: false });
const account = (p: Page, email: string) => p.locator("div.border-b", { has: p.getByText(email, { exact: true }) }).last();
const windowRow = (p: Page, email: string, label: string) => account(p, email).locator("div.grid", { hasText: label }).first();

await step("overview: dot chart has 24 hourly columns of 6 dots", async () => {
  await open("#/overview");
  const dots = await page.locator('svg[aria-label="Requests over time"] circle').count();
  expect(dots === 24 * 6, `expected 144 dots, got ${dots}`);
  const lit = await page.locator('svg[aria-label="Requests over time"] circle:not([fill="#1f1f1f"])').count();
  expect(lit > 0, "no lit dots after real traffic");
});
await step("overview: routing line names strategy, retries and affinity", async () => {
  const text = (await page.getByText(/^Routing/).first().innerText()).replace(/\s+/g, " ");
  expect(text.includes("fill first") && text.includes("2 retries") && text.includes("session affinity"), `got "${text}"`);
});
await step("overview: accounts grouped by provider with tightest limit", async () => {
  const section = page.locator("section", { has: page.getByRole("heading", { name: "Accounts" }) });
  for (const p of ["claude 2", "codex 2", "antigravity 1", "kimi 1"]) expect((await section.locator("td", { hasText: new RegExp(`^${p}$`) }).count()) === 1, `no ${p} group`);
  const ada = section.locator("tr", { hasText: "ada@example.com" });
  expect((await ada.getByText("52%").count()) === 1, "ada should show its weekly 52% as tightest");
  const margaret = section.locator("tr", { hasText: "margaret@example.com" });
  expect((await margaret.locator(".text-bad", { hasText: "93%" }).count()) === 1, "margaret 93% should be red");
});
await shot("overview");

await step("quotas: only accounts with limits or a usage endpoint, grouped", async () => {
  await open("#/quotas");
  const heads = await page.locator("main h2").allInnerTexts();
  const names = heads.map((h) => h.replace(/\s*\d+$/, "").trim());
  expect(JSON.stringify(names) === JSON.stringify(["Claude", "Codex"]), `groups ${JSON.stringify(names)}`);
});
await step("quotas: passive Claude windows parsed from unified headers", async () => {
  const s = (await windowRow(page, "ada@example.com", "Current session").innerText()).replace(/\s+/g, " ");
  expect(s.includes("38%") && s.includes("resets in 2h 40m"), `session row "${s}"`);
  const w = (await windowRow(page, "ada@example.com", "Current week").innerText()).replace(/\s+/g, " ");
  expect(w.includes("52%") && w.includes("resets in 3d 0h"), `week row "${w}"`);
  expect((await account(page, "ada@example.com").getByText(/From responses (now|\d+s ago)/).count()) === 1, "source line");
});
await step("quotas: Codex windows, red at 93%, expired window dropped", async () => {
  const p = (await windowRow(page, "linus@example.com", "5-hour limit").innerText()).replace(/\s+/g, " ");
  expect(p.includes("8%") && p.includes("resets in 4h 13m"), `linus 5h "${p}"`);
  const wk = (await windowRow(page, "linus@example.com", "Weekly limit").innerText()).replace(/\s+/g, " ");
  expect(wk.includes("61%") && wk.includes("resets in 1d 3h"), `linus weekly "${wk}"`);
  const m = account(page, "margaret@example.com");
  expect((await m.locator(".text-bad", { hasText: "93%" }).count()) === 1, "margaret 93% not red");
  expect((await m.getByText("Weekly limit").count()) === 0, "expired weekly window still shown");
  const meter = m.locator('svg[role="meter"]').first();
  expect((await meter.getAttribute("aria-valuenow")) === "93", "meter value");
});
await shot("quotas");

await step("quotas: Check asks the usage endpoint with the credential token", async () => {
  await account(page, "ada@example.com").getByRole("button", { name: "Check" }).click();
  await page.getByText("Current week (Opus only)").waitFor({ timeout: 5000 });
  const call = apiCalls.find((c) => c.authIndex === indexOf("ada@example.com"));
  expect(call?.url === "https://api.anthropic.com/api/oauth/usage", `url ${call?.url}`);
  expect(call?.header.Authorization === "Bearer $TOKEN$", "token placeholder");
  expect(call?.header["anthropic-beta"] === "oauth-2025-04-20", "oauth beta header");
  const extra = (await windowRow(page, "ada@example.com", "Extra usage").innerText()).replace(/\s+/g, " ");
  expect(extra.includes("$6.40 of $20.00") && extra.includes("32%"), `extra row "${extra}"`);
  expect((await account(page, "ada@example.com").getByText(/Checked (now|\d+s ago)/).count()) === 1, "checked line");
});
await step("quotas: Check all runs each account and shows a rejected token", async () => {
  await page.getByRole("button", { name: "Check all" }).click();
  await page.getByText(/Token rejected \(HTTP 401\)/).waitFor({ timeout: 5000 });
  const linus = (await windowRow(page, "linus@example.com", "5-hour limit").innerText()).replace(/\s+/g, " ");
  expect(linus.includes("9%"), `linus live 5h "${linus}"`);
  const codexCall = apiCalls.find((c) => c.authIndex === indexOf("linus@example.com"));
  expect(codexCall?.header["Chatgpt-Account-Id"] === "acct_1", "codex account header");
  expect(!apiCalls.some((c) => c.authIndex === indexOf("grace@example.com")), "disabled account was checked");
});
await shot("quotas-checked");

await step("credentials: quota column shows the tightest window", async () => {
  await open("#/credentials");
  const row = page.locator("tr", { hasText: "margaret@example.com" });
  expect((await row.locator(".text-bad", { hasText: "93%" }).count()) === 1, "margaret quota cell");
});
await shot("credentials");

await step("tools: client key works against /v1/models", async () => {
  await open("#/tools");
  await page.getByText(/Works, \d+ models/).waitFor({ timeout: 5000 });
});
await step("tools: snippet masks the key, copy carries the full key", async () => {
  const pre = await page.locator("pre").innerText();
  expect(pre.includes(`export ANTHROPIC_BASE_URL='${base}'`), "base url in snippet");
  expect(pre.includes("sk-dev-c...1111") && !pre.includes(clientKey), "key not masked");
  await page.getByRole("button", { name: "Copy setup" }).click();
  const clip = await page.evaluate(() => navigator.clipboard.readText());
  expect(clip.includes(clientKey), "clipboard lacks the full key");
  await page.getByRole("tab", { name: "Codex CLI" }).click();
  expect((await page.getByRole("tab", { name: "Codex CLI" }).getAttribute("aria-selected")) === "true", "Codex tab not selected");
  expect((await page.getByRole("tab", { name: "Claude Code" }).getAttribute("aria-selected")) === "false", "Claude tab still selected");
  const codex = await page.locator("pre").innerText();
  expect(codex.includes(`base_url = "${base}/v1"`) && codex.includes('wire_api = "responses"'), "codex config");
});
await shot("tools");

await step("no page or console errors", async () => {
  expect(errors.length === 0, errors.join(" | "));
});

await browser.close();
const fails = report.filter((l) => l.startsWith("FAIL")).length;
report.push(`${fails === 0 ? "OK" : "FAILED"}: ${report.filter((l) => l.startsWith("PASS")).length} passed, ${fails} failed`);
await writeFile(join(out, "quotas-e2e-report.txt"), `${report.join("\n")}\n`);
console.log(report.join("\n"));
process.exit(fails === 0 ? 0 : 1);
