// Fake OpenAI-compatible upstream (port 18400) used to generate real usage events
// through the Go server during UI development. Occasionally fails on purpose.
const port = Number(process.env.FAKE_UPSTREAM_PORT ?? 18400);

const sleep = (ms: number) => new Promise((r) => setTimeout(r, ms));

Bun.serve({
  port,
  async fetch(req) {
    const url = new URL(req.url);
    if (url.pathname.endsWith("/chat/completions")) {
      const body = (await req.json().catch(() => ({}))) as { model?: string };
      await sleep(60 + Math.random() * 700);
      const roll = Math.random();
      if (roll < 0.04) return Response.json({ error: { message: "upstream overloaded" } }, { status: 500 });
      if (roll < 0.07) return Response.json({ error: { message: "rate limited" } }, { status: 429 });
      const prompt = 40 + Math.floor(Math.random() * 3200);
      const completion = 20 + Math.floor(Math.random() * 900);
      return Response.json({
        id: `chatcmpl-${crypto.randomUUID().slice(0, 12)}`,
        object: "chat.completion",
        created: Math.floor(Date.now() / 1000),
        model: body.model ?? "gpt-4o-mini",
        choices: [{ index: 0, message: { role: "assistant", content: "ok" }, finish_reason: "stop" }],
        usage: { prompt_tokens: prompt, completion_tokens: completion, total_tokens: prompt + completion },
      });
    }
    return Response.json({ error: "not found" }, { status: 404 });
  },
});
console.log(`fake upstream on :${port}`);
