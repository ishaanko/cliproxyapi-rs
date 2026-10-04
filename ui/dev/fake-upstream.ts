// Fake upstream (port 18400) used to generate real usage events through the server during
// UI development. Serves OpenAI chat completions, Anthropic messages and Codex responses;
// the last two answer with the quota headers real accounts send, so the server records
// limits per credential. Occasionally fails on purpose.
const port = Number(process.env.FAKE_UPSTREAM_PORT ?? 18400);

const sleep = (ms: number) => new Promise((r) => setTimeout(r, ms));
const now = () => Math.floor(Date.now() / 1000);

// Per upstream key: percent used of the short and weekly windows, and seconds until each resets.
// Keys not listed get a deterministic value derived from the key.
const usage: Record<string, { short: number; week: number; shortReset: number; weekReset: number }> = {
  "sk-ant-max": { short: 38, week: 52, shortReset: 2 * 3600 + 40 * 60, weekReset: 3 * 86400 },
  "sk-ant-pro": { short: 93, week: 64, shortReset: 25 * 60, weekReset: 30 * 3600 },
  "sk-codex-plus": { short: 8, week: 61, shortReset: 4 * 3600 + 13 * 60, weekReset: 27 * 3600 },
  "sk-codex-team": { short: 77, week: 27, shortReset: 2 * 3600, weekReset: 4 * 86400 },
};
const usageOf = (key: string) => {
  const u = usage[key];
  if (u) return u;
  const h = [...key].reduce((a, c) => (a * 31 + c.charCodeAt(0)) % 997, 7);
  return { short: h % 100, week: (h * 7) % 100, shortReset: 3600 + (h % 3) * 3600, weekReset: 86400 * (1 + (h % 6)) };
};
const keyOf = (req: Request) => (req.headers.get("x-api-key") ?? req.headers.get("authorization")?.replace(/^Bearer /, "") ?? "").trim();

function claudeHeaders(key: string): Record<string, string> {
  const u = usageOf(key);
  return {
    "anthropic-ratelimit-unified-status": u.short >= 100 ? "rejected" : "allowed",
    "anthropic-ratelimit-unified-5h-utilization": String(u.short / 100),
    "anthropic-ratelimit-unified-5h-reset": String(now() + u.shortReset),
    "anthropic-ratelimit-unified-5h-status": u.short >= 100 ? "rejected" : "allowed",
    "anthropic-ratelimit-unified-7d-utilization": String(u.week / 100),
    "anthropic-ratelimit-unified-7d-reset": String(now() + u.weekReset),
    "anthropic-ratelimit-unified-7d-status": "allowed",
  };
}

function codexHeaders(key: string): Record<string, string> {
  const u = usageOf(key);
  return {
    "x-codex-plan-type": "plus",
    "x-codex-primary-used-percent": String(u.short),
    "x-codex-primary-window-minutes": "300",
    "x-codex-primary-reset-after-seconds": String(u.shortReset),
    "x-codex-secondary-used-percent": String(u.week),
    "x-codex-secondary-window-minutes": "10080",
    "x-codex-secondary-reset-after-seconds": String(u.weekReset),
  };
}

const tokens = () => ({ prompt: 40 + Math.floor(Math.random() * 3200), completion: 20 + Math.floor(Math.random() * 900) });

/** A failure roll shared by every route: 4% overloaded, 3% rate limited. */
function failure(): Response | null {
  const roll = Math.random();
  if (roll < 0.04) return Response.json({ error: { message: "upstream overloaded" } }, { status: 500 });
  if (roll < 0.07) return Response.json({ error: { message: "rate limited" } }, { status: 429 });
  return null;
}

Bun.serve({
  port,
  async fetch(req) {
    const url = new URL(req.url);
    const body = (await req.json().catch(() => ({}))) as { model?: string; stream?: boolean };
    await sleep(60 + Math.random() * 700);
    const failed = failure();
    if (url.pathname.endsWith("/chat/completions")) {
      if (failed) return failed;
      const t = tokens();
      return Response.json({
        id: `chatcmpl-${crypto.randomUUID().slice(0, 12)}`,
        object: "chat.completion",
        created: now(),
        model: body.model ?? "gpt-4o-mini",
        choices: [{ index: 0, message: { role: "assistant", content: "ok" }, finish_reason: "stop" }],
        usage: { prompt_tokens: t.prompt, completion_tokens: t.completion, total_tokens: t.prompt + t.completion },
      });
    }
    if (url.pathname.endsWith("/v1/messages")) {
      const headers = claudeHeaders(keyOf(req));
      if (failed) return new Response(failed.body, { status: failed.status, headers: { ...headers, "content-type": "application/json" } });
      const t = tokens();
      const message = {
        id: `msg_${crypto.randomUUID().slice(0, 12)}`,
        type: "message",
        role: "assistant",
        model: body.model ?? "claude-sonnet-4-5",
        content: [{ type: "text", text: "ok" }],
        stop_reason: "end_turn",
        stop_sequence: null,
        usage: { input_tokens: t.prompt, output_tokens: t.completion },
      };
      if (!body.stream) return Response.json(message, { headers });
      const ev = (type: string, data: object) => `event: ${type}\ndata: ${JSON.stringify({ type, ...data })}\n\n`;
      const sse = [
        ev("message_start", { message: { ...message, content: [], stop_reason: null, usage: { input_tokens: t.prompt, output_tokens: 1 } } }),
        ev("content_block_start", { index: 0, content_block: { type: "text", text: "" } }),
        ev("content_block_delta", { index: 0, delta: { type: "text_delta", text: "ok" } }),
        ev("content_block_stop", { index: 0 }),
        ev("message_delta", { delta: { stop_reason: "end_turn", stop_sequence: null }, usage: { output_tokens: t.completion } }),
        ev("message_stop", {}),
      ].join("");
      return new Response(sse, { headers: { ...headers, "content-type": "text/event-stream" } });
    }
    if (url.pathname.endsWith("/responses")) {
      const headers = codexHeaders(keyOf(req));
      if (failed) return new Response(failed.body, { status: failed.status, headers: { ...headers, "content-type": "application/json" } });
      const t = tokens();
      const response = {
        id: `resp_${crypto.randomUUID().slice(0, 12)}`,
        object: "response",
        created_at: now(),
        status: "completed",
        model: body.model ?? "gpt-5",
        output: [{ type: "message", id: "msg_1", role: "assistant", status: "completed", content: [{ type: "output_text", text: "ok", annotations: [] }] }],
        usage: { input_tokens: t.prompt, output_tokens: t.completion, total_tokens: t.prompt + t.completion },
      };
      const sse = [
        `event: response.created\ndata: ${JSON.stringify({ type: "response.created", response: { ...response, status: "in_progress", output: [] } })}\n\n`,
        `event: response.completed\ndata: ${JSON.stringify({ type: "response.completed", response })}\n\n`,
      ].join("");
      return new Response(sse, { headers: { ...headers, "content-type": "text/event-stream" } });
    }
    return Response.json({ error: "not found" }, { status: 404 });
  },
});
console.log(`fake upstream on :${port}`);
