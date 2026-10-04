import { useQuery, useQueryClient } from "@tanstack/react-query";
import { useState, type ReactNode } from "react";
import { generateKey, maskKey } from "@/lib/format";
import { qk, updateClientKeys, useClientKeys } from "@/lib/queries";
import { errorText, toast } from "@/lib/toast";
import type { ModelInfo } from "@/lib/types";
import { Button, CopyButton, EmptyState, KeyValue, PageHeader, Select, Status, Tabs } from "@/ui/primitives";

const TOOLS = ["Claude Code", "Codex CLI", "Gemini CLI", "OpenAI SDK", "Anthropic SDK", "curl"] as const;
type Tool = (typeof TOOLS)[number];

const notes: Record<Tool, string> = {
  "Claude Code": "Run in the shell you start Claude Code from, or put both variables in the env block of ~/.claude/settings.json.",
  "Codex CLI": "Add the provider to ~/.codex/config.toml and export the key in your shell.",
  "Gemini CLI": "Run in the shell you start Gemini CLI from.",
  "OpenAI SDK": "Python. The JavaScript SDK takes the same baseURL and apiKey. Chat Completions and Responses both work.",
  "Anthropic SDK": "Python. No /v1 in the address: the SDK adds it.",
  curl: "Lists the models this key can reach. 401 means the key is wrong.",
};

/** Default model per tool until the user picks one: the tool's own family when the proxy serves it. */
const family: Record<Tool, string> = {
  "Claude Code": "claude",
  "Codex CLI": "gpt",
  "Gemini CLI": "gemini",
  "OpenAI SDK": "gpt",
  "Anthropic SDK": "claude",
  curl: "",
};

const q = (s: string) => JSON.stringify(s);
const sh = (s: string) => `'${s.replaceAll("'", `'\\''`)}'`;

function snippet(tool: Tool, base: string, key: string, model: string): string {
  switch (tool) {
    case "Claude Code":
      return `export ANTHROPIC_BASE_URL=${sh(base)}\nexport ANTHROPIC_AUTH_TOKEN=${sh(key)}\nclaude`;
    case "Codex CLI":
      return `# ~/.codex/config.toml\nmodel_provider = "cliproxy"\nmodel = ${q(model)}\n\n[model_providers.cliproxy]\nname = "CLIProxyAPI"\nbase_url = ${q(`${base}/v1`)}\nenv_key = "CLIPROXY_API_KEY"\nwire_api = "responses"\n\n# then, in your shell\nexport CLIPROXY_API_KEY=${sh(key)}`;
    case "Gemini CLI":
      return `export GOOGLE_GEMINI_BASE_URL=${sh(base)}\nexport GEMINI_API_KEY=${sh(key)}\ngemini`;
    case "OpenAI SDK":
      return `from openai import OpenAI\n\nclient = OpenAI(base_url=${q(`${base}/v1`)}, api_key=${q(key)})\nr = client.chat.completions.create(\n    model=${q(model)}, messages=[{"role": "user", "content": "Hello"}]\n)\nprint(r.choices[0].message.content)`;
    case "Anthropic SDK":
      return `import anthropic\n\nclient = anthropic.Anthropic(base_url=${q(base)}, api_key=${q(key)})\nr = client.messages.create(\n    model=${q(model)}, max_tokens=256, messages=[{"role": "user", "content": "Hello"}]\n)\nprint(r.content[0].text)`;
    case "curl":
      return `curl ${base}/v1/models \\\n  -H ${sh(`Authorization: Bearer ${key}`)}`;
  }
}

/** Asks this proxy for its model list with a client key, exactly as a tool would. */
function useKeyCheck(key: string | undefined) {
  return useQuery({
    queryKey: [...qk.served, "check", key],
    enabled: key !== undefined,
    retry: false,
    queryFn: async () => {
      const res = await fetch("/v1/models", { headers: { Authorization: `Bearer ${key}` }, cache: "no-store" });
      if (res.status === 401) throw new Error("The proxy rejected this key");
      if (!res.ok) throw new Error(`The proxy answered HTTP ${res.status}`);
      return ((await res.json()) as { data?: ModelInfo[] }).data?.map((m) => m.id) ?? [];
    },
  });
}

/** Copy-paste setup for coding tools and SDKs, with a live check of the chosen client key. */
export default function Tools() {
  const qc = useQueryClient();
  const keys = useClientKeys();
  const [pick, setPick] = useState(0);
  const [tool, setTool] = useState<Tool>("Claude Code");
  const [model, setModel] = useState("");
  const list = keys.data ?? [];
  const key = list[Math.min(pick, list.length - 1)];
  const check = useKeyCheck(key);
  const models = check.data ?? [];
  const chosen = models.includes(model) ? model : (models.find((m) => m.includes(family[tool])) ?? models[0] ?? "MODEL_ID");

  const base = window.location.origin;
  const local = /^(localhost|127\.|\[::1\])/.test(window.location.hostname);

  async function create() {
    try {
      await updateClientKeys((k) => [...k, generateKey()]);
      await qc.invalidateQueries({ queryKey: qk.clientKeys });
      toast.ok("Client key created");
    } catch (e) {
      toast.error(errorText(e));
    }
  }

  return (
    <>
      <PageHeader title="Use with tools" />
      <div className="min-h-0 flex-1 overflow-y-auto">
        {keys.isSuccess && list.length === 0 ? (
          <EmptyState
            title="No client key"
            hint="Tools send a client key to this proxy. Create one, then come back here for the setup."
            action={
              <Button variant="primary" icon="plus" onClick={() => void create()}>
                Create client key
              </Button>
            }
          />
        ) : (
          <div className="max-w-[860px]">
            <div className="px-5 pt-4">
              <KeyValue
                rows={[
                  [
                    "Address",
                    <span key="a" className="flex items-center gap-1">
                      <span className="mono">{base}</span>
                      <CopyButton text={base} />
                      {local && <span className="ml-2 text-[12px] text-muted">Only this computer can reach it</span>}
                    </span>,
                  ],
                  [
                    "OpenAI base",
                    <span key="o" className="flex items-center gap-1">
                      <span className="mono">{base}/v1</span>
                      <CopyButton text={`${base}/v1`} />
                    </span>,
                  ],
                  [
                    "Client key",
                    <span key="k" className="flex items-center gap-3 py-1">
                      {list.length > 1 ? (
                        <Select value={pick} onChange={(e) => setPick(Number(e.target.value))} className="h-7 w-52" aria-label="Client key">
                          {list.map((k, i) => (
                            <option key={k} value={i}>
                              {maskKey(k)}
                            </option>
                          ))}
                        </Select>
                      ) : (
                        <span className="mono">{key ? maskKey(key) : ""}</span>
                      )}
                      <span className="whitespace-nowrap">
                      {check.isFetching ? (
                        <Status tone="off">Testing</Status>
                      ) : check.isError ? (
                        <Status tone="bad">{check.error.message}</Status>
                      ) : check.isSuccess ? (
                        <Status tone={models.length ? "ok" : "warn"}>{models.length ? `Works, ${models.length} models` : "Works, but no models yet"}</Status>
                      ) : null}
                      </span>
                    </span>,
                  ],
                  ...(models.length
                    ? ([
                        [
                          "Model",
                          <span key="m" className="my-1 w-72">
                            <Select value={chosen} onChange={(e) => setModel(e.target.value)} className="h-7" aria-label="Model">
                              {models.map((m) => (
                                <option key={m}>{m}</option>
                              ))}
                            </Select>
                          </span>,
                        ],
                      ] as [string, ReactNode][])
                    : []),
                ]}
              />
            </div>
            <div className="mt-6 border-b border-line">
              <Tabs value={tool} onChange={setTool} items={TOOLS.map((t) => ({ id: t, label: t }))} />
            </div>
            <div className="px-5 pt-3">
              <p className="text-[12.5px] text-muted">{notes[tool]}</p>
              {key && (
                <>
                  <pre className="mono mt-3 overflow-x-auto rounded-md border border-line bg-raised p-4 leading-[1.6] text-fg-2" aria-label={`${tool} setup`}>
                    {snippet(tool, base, maskKey(key), chosen)}
                  </pre>
                  <div className="mt-3 flex items-center gap-3">
                    <Button
                      variant="primary"
                      icon="copy"
                      onClick={() =>
                        void navigator.clipboard.writeText(snippet(tool, base, key, chosen)).then(() => toast.ok("Copied with the full key"))
                      }
                    >
                      Copy setup
                    </Button>
                    <span className="text-[12px] text-muted">The copy has the full key; the page shows it masked.</span>
                  </div>
                </>
              )}
            </div>
          </div>
        )}
      </div>
    </>
  );
}
