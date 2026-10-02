// Dev seeding against the throwaway Go server: writes fake credential files into
// the temp auth dir and sends traffic through /v1 so usage events exist.
//   bun dev/seed.ts [authDir] [baseUrl] [requests]
import { mkdir, writeFile } from "node:fs/promises";

const authDir = process.argv[2] ?? "/tmp/cpa-ui-dev/auth";
const base = process.argv[3] ?? "http://127.0.0.1:18317";
const total = Number(process.argv[4] ?? 160);
const clientKeys = ["sk-dev-client-aaaa1111", "sk-dev-client-bbbb2222"];
const far = "2099-01-01T00:00:00Z";
const now = new Date().toISOString();

const jwt = (claims: Record<string, unknown>) => {
  const enc = (o: unknown) => Buffer.from(JSON.stringify(o)).toString("base64url");
  return `${enc({ alg: "none" })}.${enc(claims)}.sig`;
};

const files: Record<string, Record<string, unknown>> = {
  "claude-ada@example.com.json": {
    type: "claude", email: "ada@example.com", access_token: "sk-ant-oat01-fake", refresh_token: "sk-ant-ort01-fake",
    expired: far, last_refresh: now,
  },
  "claude-grace@example.com.json": {
    type: "claude", email: "grace@example.com", access_token: "sk-ant-oat01-fake2", refresh_token: "sk-ant-ort01-fake2",
    expired: far, last_refresh: now, disabled: true,
  },
  "codex-linus@example.com-plus.json": {
    type: "codex", email: "linus@example.com", account_id: "acct_1", access_token: "fake", refresh_token: "fake",
    id_token: jwt({ email: "linus@example.com", "https://api.openai.com/auth": { chatgpt_plan_type: "plus", chatgpt_account_id: "acct_1" } }),
    expired: far, last_refresh: now,
  },
  "codex-margaret@example.com-pro.json": {
    type: "codex", email: "margaret@example.com", account_id: "acct_2", access_token: "fake", refresh_token: "fake",
    id_token: jwt({ email: "margaret@example.com", "https://api.openai.com/auth": { chatgpt_plan_type: "pro", chatgpt_account_id: "acct_2" } }),
    expired: far, last_refresh: now, note: "team shared",
  },
  "antigravity-alan@example.com.json": {
    type: "antigravity", email: "alan@example.com", access_token: "ya29.fake", refresh_token: "1//fake",
    expires_in: 3599, timestamp: Date.now(), expired: far, project_id: "bold-tide-48213",
  },
  "kimi-dennis.json": {
    type: "kimi", email: "dennis@example.com", access_token: "fake", refresh_token: "fake", expired: far,
  },
};

await mkdir(authDir, { recursive: true });
for (const [name, body] of Object.entries(files)) {
  await writeFile(`${authDir}/${name}`, JSON.stringify(body, null, 2), { mode: 0o600 });
}
console.log(`wrote ${Object.keys(files).length} credential files to ${authDir}`);

const models = ["fast-mini", "fast-mini", "llama-70b"];
let ok = 0;
let bad = 0;
for (let i = 0; i < total; i++) {
  const res = await fetch(`${base}/v1/chat/completions`, {
    method: "POST",
    headers: { "content-type": "application/json", authorization: `Bearer ${clientKeys[i % 2]}` },
    body: JSON.stringify({ model: models[i % models.length], messages: [{ role: "user", content: "hi" }] }),
  }).catch(() => null);
  if (res?.ok) ok++;
  else bad++;
}
console.log(`sent ${total} requests, ${ok} ok, ${bad} failed`);
