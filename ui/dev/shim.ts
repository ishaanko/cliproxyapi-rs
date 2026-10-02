// Dev shim (port 18318). Proxies /v8, /v1 and /healthz to the Go server and adds the
// additive endpoints documented in ui/API_EXTENSIONS.md by draining the Go usage queue
// into an in-memory ring buffer plus aggregates. Also serves ui/dist at / and
// /management.html, i.e. what the Rust binary will do.
//   BACKEND=http://127.0.0.1:18317 MGMT_KEY=dev-secret SHIM_DEMO=1 bun dev/shim.ts
import { file } from "bun";
import { join, normalize } from "node:path";

const backend = process.env.BACKEND ?? "http://127.0.0.1:18317";
const mgmtKey = process.env.MGMT_KEY ?? "dev-secret";
const port = Number(process.env.SHIM_PORT ?? 18318);
const demo = process.env.SHIM_DEMO === "1";
// SHIM_NO_EXT=1 hides the extension endpoints to exercise the UI fallbacks.
const noExt = process.env.SHIM_NO_EXT === "1";
const distDir = join(import.meta.dir, "..", "dist");
const CAPACITY = 1000;

interface Tokens { input_tokens: number; output_tokens: number; reasoning_tokens: number; cached_tokens: number; total_tokens: number }
interface UsageEvent {
  seq: number;
  timestamp: string;
  latency_ms: number;
  ttft_ms: number;
  source: string;
  auth_index: string;
  auth_type: string;
  provider: string;
  model: string;
  alias: string;
  endpoint: string;
  api_key: string;
  request_id: string;
  failed: boolean;
  stream: boolean;
  tokens: Tokens;
}

const zeroTokens = (): Tokens => ({ input_tokens: 0, output_tokens: 0, reasoning_tokens: 0, cached_tokens: 0, total_tokens: 0 });
const ring: UsageEvent[] = [];
let seq = 0;

interface Agg { requests: number; failed: number; tokens: Tokens }
const newAgg = (): Agg => ({ requests: 0, failed: 0, tokens: zeroTokens() });
const totals = newAgg();
const models = new Map<string, Agg>();
const credentials = new Map<string, Agg & { provider: string; source: string }>();
const apiKeys = new Map<string, Agg>();
const hourly = new Map<number, Agg>();
const since = new Date().toISOString();

function bump(a: Agg, e: UsageEvent) {
  a.requests++;
  if (e.failed) a.failed++;
  for (const k of Object.keys(a.tokens) as (keyof Tokens)[]) a.tokens[k] += e.tokens?.[k] ?? 0;
}

function ingest(raw: Omit<UsageEvent, "seq">) {
  const e: UsageEvent = { ...raw, seq: ++seq };
  ring.push(e);
  if (ring.length > CAPACITY) ring.shift();
  bump(totals, e);
  const mk = e.alias || e.model || "unknown";
  bump(models.get(mk) ?? models.set(mk, newAgg()).get(mk)!, e);
  const ck = e.auth_index || "unknown";
  const c = credentials.get(ck) ?? credentials.set(ck, { ...newAgg(), provider: e.provider, source: e.source }).get(ck)!;
  bump(c, e);
  const ak = e.api_key || "unknown";
  bump(apiKeys.get(ak) ?? apiKeys.set(ak, newAgg()).get(ak)!, e);
  const hour = Math.floor(new Date(e.timestamp).getTime() / 3_600_000) * 3_600_000;
  bump(hourly.get(hour) ?? hourly.set(hour, newAgg()).get(hour)!, e);
}

async function drain() {
  try {
    const res = await fetch(`${backend}/v8/management/observability/usage/queue?count=500`, {
      headers: { authorization: `Bearer ${mgmtKey}` },
    });
    if (!res.ok) return;
    const items = (await res.json()) as Omit<UsageEvent, "seq">[];
    for (const it of items) ingest(it);
  } catch {
    /* backend down; retry next tick */
  }
}
setInterval(drain, 1000);

// SHIM_DEMO=1: synthetic history so charts and tables have shape. Uses the real
// credentials of the backend so auth_index values line up with /credentials.
const modelsByProvider: Record<string, string[]> = {
  claude: ["claude-sonnet-4-5", "claude-opus-4-1", "claude-haiku-4-5"],
  codex: ["gpt-5-codex", "gpt-5"],
  antigravity: ["gemini-2.5-pro", "gemini-3-pro-preview"],
  kimi: ["kimi-k2-thinking", "kimi-k2.5"],
  "fake-upstream": ["fast-mini", "llama-70b"],
};

async function seedDemo() {
  const pick = <T,>(xs: T[]): T => xs[Math.floor(Math.random() * xs.length)]!;
  const res = await fetch(`${backend}/v8/management/credentials`, { headers: { authorization: `Bearer ${mgmtKey}` } }).catch(() => null);
  const body = res?.ok ? ((await res.json()) as { files: { auth_index: string; provider: string; email?: string; label?: string; disabled?: boolean }[] }) : { files: [] };
  const creds = body.files
    .filter((f) => !f.disabled)
    .map((f) => ({ idx: f.auth_index, provider: f.provider, source: f.email || f.label || f.provider }));
  creds.push({ idx: "5b19d7c40e66ab01", provider: "fake-upstream", source: "fake-upstream" });
  const keys = ["sk-dev-client-aaaa1111", "sk-dev-client-bbbb2222"];
  const nowMs = Date.now();
  const events: Omit<UsageEvent, "seq">[] = [];
  for (let i = 0; i < 900; i++) {
    const age = Math.pow(Math.random(), 1.4) * 24 * 3_600_000;
    const c = pick(creds);
    const input = 200 + Math.floor(Math.random() * 14000);
    const output = 30 + Math.floor(Math.random() * 2500);
    const failed = Math.random() < 0.045;
    events.push({
      timestamp: new Date(nowMs - age).toISOString(),
      latency_ms: 300 + Math.floor(Math.random() * 9000),
      ttft_ms: 120 + Math.floor(Math.random() * 1400),
      source: c.source,
      auth_index: c.idx,
      auth_type: c.provider === "fake-upstream" ? "apikey" : "oauth",
      provider: c.provider,
      model: pick(modelsByProvider[c.provider] ?? ["unknown"]),
      alias: "",
      endpoint: pick(["POST /v1/chat/completions", "POST /v1/messages", "POST /v1/responses"]),
      api_key: pick(keys),
      request_id: crypto.randomUUID().slice(0, 8),
      failed,
      stream: Math.random() < 0.7,
      tokens: failed
        ? zeroTokens()
        : { input_tokens: input, output_tokens: output, reasoning_tokens: Math.random() < 0.3 ? Math.floor(output / 3) : 0, cached_tokens: Math.floor(input * Math.random() * 0.6), total_tokens: input + output },
    });
  }
  events.sort((a, b) => a.timestamp.localeCompare(b.timestamp));
  for (const e of events) ingest(e);
  console.log(`demo: ${events.length} synthetic events`);
}
if (demo) await seedDemo();

function json(data: unknown, status = 200) {
  return Response.json(data, { status, headers: { "cache-control": "no-store" } });
}

function authorized(req: Request): boolean {
  const bearer = req.headers.get("authorization")?.replace(/^Bearer\s+/i, "");
  return (bearer ?? req.headers.get("x-management-key")) === mgmtKey;
}

function summary() {
  const nowHour = Math.floor(Date.now() / 3_600_000) * 3_600_000;
  const buckets = [];
  for (let h = nowHour - 23 * 3_600_000; h <= nowHour; h += 3_600_000) {
    const a = hourly.get(h) ?? newAgg();
    buckets.push({ hour: new Date(h).toISOString(), ...a });
  }
  const rows = <T extends Agg>(m: Map<string, T>, key: string) =>
    [...m.entries()].map(([k, v]) => ({ [key]: k, ...v })).sort((a, b) => b.requests - a.requests);
  return {
    since,
    totals,
    models: rows(models, "model"),
    credentials: rows(credentials, "auth_index"),
    api_keys: rows(apiKeys, "api_key"),
    hourly: buckets,
  };
}

async function proxy(req: Request, url: URL): Promise<Response> {
  const target = new URL(url.pathname + url.search, backend);
  const headers = new Headers(req.headers);
  headers.delete("host");
  const init: RequestInit = { method: req.method, headers, redirect: "manual" };
  if (req.method !== "GET" && req.method !== "HEAD") init.body = await req.arrayBuffer();
  try {
    const res = await fetch(target, init);
    const out = new Headers(res.headers);
    out.delete("content-encoding");
    out.delete("content-length");
    return new Response(res.body, { status: res.status, headers: out });
  } catch {
    return json({ error: "backend unreachable" }, 502);
  }
}

async function serveStatic(pathname: string): Promise<Response> {
  const rel = pathname === "/" || pathname === "/management.html" ? "index.html" : normalize(pathname).replace(/^(\.\.[/\\])+/, "");
  const f = file(join(distDir, rel));
  if (await f.exists()) return new Response(f);
  return new Response("not found", { status: 404 });
}

Bun.serve({
  port,
  async fetch(req) {
    const url = new URL(req.url);
    const p = url.pathname;
    if (!noExt && p === "/v8/management/observability/requests" && req.method === "GET") {
      if (!authorized(req)) return json({ error: "invalid management key" }, 401);
      const limit = Math.min(Math.max(Number(url.searchParams.get("limit") ?? 100) || 100, 1), CAPACITY);
      const afterParam = url.searchParams.get("after");
      const events = afterParam === null ? ring.slice(-limit) : ring.filter((e) => e.seq > Number(afterParam)).slice(0, limit);
      return json({ seq, capacity: CAPACITY, events });
    }
    if (!noExt && p === "/v8/management/observability/usage/summary" && req.method === "GET") {
      if (!authorized(req)) return json({ error: "invalid management key" }, 401);
      return json(summary());
    }
    if (p.startsWith("/v8/") || p.startsWith("/v1/") || p === "/healthz") return proxy(req, url);
    return serveStatic(p);
  },
});
console.log(`shim on :${port} -> ${backend}${demo ? " (demo)" : ""}`);
