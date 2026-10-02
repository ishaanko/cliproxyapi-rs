import { useQueryClient } from "@tanstack/react-query";
import { useState } from "react";
import { qk, updateProviderGroup } from "@/lib/queries";
import { errorText, toast } from "@/lib/toast";
import type { ProviderEntry, ProviderKey, ProviderModel } from "@/lib/types";
import { Sheet } from "@/ui/overlays";
import { Button, Field, IconButton, Input, Switch, Tabs, Textarea } from "@/ui/primitives";

// Form view of one provider entry. Unknown fields ride along untouched so saving
// never drops settings this form does not know about; the JSON tab edits them.

type Mode = "form" | "json";

const headersToText = (h: Record<string, string> | undefined) =>
  Object.entries(h ?? {})
    .map(([k, v]) => `${k}: ${v}`)
    .join("\n");

function textToHeaders(text: string): Record<string, string> | undefined {
  const out: Record<string, string> = {};
  for (const line of text.split("\n")) {
    const i = line.indexOf(":");
    if (i <= 0) continue;
    const k = line.slice(0, i).trim();
    if (k) out[k] = line.slice(i + 1).trim();
  }
  return Object.keys(out).length ? out : undefined;
}

const lines = (text: string): string[] | undefined => {
  const out = text.split("\n").map((s) => s.trim()).filter(Boolean);
  return out.length ? out : undefined;
};

/** Parses an entry from JSON text; rejects anything but a plain object. */
function parseEntry(text: string): ProviderEntry {
  const value: unknown = JSON.parse(text);
  if (typeof value !== "object" || value === null || Array.isArray(value)) throw new Error("expected a JSON object");
  return value as ProviderEntry;
}

/** Integer in range, or undefined for empty text. Throws with a field-specific message. */
function parseInteger(text: string, label: string, min: number, max: number): number | undefined {
  const t = text.trim();
  if (t === "") return undefined;
  const n = Number(t);
  if (!Number.isInteger(n) || n < min || n > max) throw new Error(`${label} must be an integer from ${min} to ${max}`);
  return n;
}

/** Drop empty values so the saved YAML stays tidy. */
function clean(e: ProviderEntry): ProviderEntry {
  const out: ProviderEntry = { ...e };
  for (const k of ["name", "base-url", "prefix", "proxy-url"] as const) if (!out[k]?.trim()) delete out[k];
  if (out.priority === undefined || Number.isNaN(out.priority)) delete out.priority;
  if (!out.disabled) delete out.disabled;
  const keys = (out.keys ?? []).filter((k) => k["api-key"].trim());
  if (keys.length) out.keys = keys;
  else delete out.keys;
  const models = (out.models ?? [])
    .filter((m) => m.name.trim())
    .map(({ alias, ...rest }): ProviderModel => (alias?.trim() ? { ...rest, alias } : rest));
  if (models.length) out.models = models;
  else delete out.models;
  if (!out["excluded-models"]?.length) delete out["excluded-models"];
  if (!out.headers || Object.keys(out.headers).length === 0) delete out.headers;
  return out;
}

export function ProviderSheet({
  group,
  label,
  entry,
  index,
  onClose,
}: {
  group: string;
  label: string;
  entry: ProviderEntry;
  index: number | null;
  onClose: () => void;
}) {
  const qc = useQueryClient();
  const [draft, setDraft] = useState<ProviderEntry>(() => structuredClone(entry));
  const [mode, setMode] = useState<Mode>("form");
  const [json, setJson] = useState("");
  const [headers, setHeaders] = useState(() => headersToText(entry.headers));
  const [priorityText, setPriorityText] = useState(() => entry.priority?.toString() ?? "");
  const [weightText, setWeightText] = useState<string[]>(() => (entry.keys ?? []).map((k) => k.weight?.toString() ?? ""));
  const [excluded, setExcluded] = useState(() => (entry["excluded-models"] ?? []).join("\n"));
  const [error, setError] = useState<string | null>(null);
  const [saving, setSaving] = useState(false);
  const isNew = index === null;

  const patch = (p: Partial<ProviderEntry>) => setDraft((d) => ({ ...d, ...p }));
  const withText = (): ProviderEntry => ({ ...draft, headers: textToHeaders(headers), "excluded-models": lines(excluded) });

  function switchMode(next: Mode) {
    setError(null);
    if (next === "json") {
      setJson(JSON.stringify(clean(withText()), null, 2));
      setMode("json");
      return;
    }
    try {
      const parsed = parseEntry(json);
      setDraft(parsed);
      setPriorityText(parsed.priority?.toString() ?? "");
      setWeightText((parsed.keys ?? []).map((k) => k.weight?.toString() ?? ""));
      setHeaders(headersToText(parsed.headers));
      setExcluded((parsed["excluded-models"] ?? []).join("\n"));
      setMode("form");
    } catch (e) {
      setError(`Invalid JSON: ${errorText(e)}`);
    }
  }

  async function save() {
    let next: ProviderEntry;
    try {
      if (mode === "json") next = parseEntry(json);
      else {
        const full = withText();
        next = clean({
          ...full,
          priority: parseInteger(priorityText, "Priority", -1_000_000, 1_000_000),
          keys: (full.keys ?? []).map((k, i) => ({ ...k, weight: parseInteger(weightText[i] ?? "", `Key ${i + 1} weight`, 0, 1_000_000) })),
        });
      }
    } catch (e) {
      setError(mode === "json" ? `Invalid JSON: ${errorText(e)}` : errorText(e));
      return;
    }
    setSaving(true);
    setError(null);
    try {
      await updateProviderGroup(
        group,
        (list) => (index === null ? [...list, next] : list.map((x, i) => (i === index ? next : x))),
        index === null ? undefined : { index, entry },
      );
      await qc.invalidateQueries({ queryKey: qk.providers });
      toast.ok("Saved");
      onClose();
    } catch (e) {
      setError(errorText(e));
      setSaving(false);
    }
  }

  const keys = draft.keys ?? [];
  const models = draft.models ?? [];
  const setKey = (i: number, p: Partial<ProviderKey>) => patch({ keys: keys.map((k, j) => (j === i ? { ...k, ...p } : k)) });
  const setModel = (i: number, p: Partial<ProviderModel>) => patch({ models: models.map((m, j) => (j === i ? { ...m, ...p } : m)) });

  return (
    <Sheet
      title={`${isNew ? "Add" : "Edit"} ${label}`}
      onClose={onClose}
      width={600}
      footer={
        <>
          {error && <div className="mr-auto min-w-0 flex-1 text-[12px] break-words text-bad" role="alert">{error}</div>}
          <Button onClick={onClose}>Cancel</Button>
          <Button variant="primary" disabled={saving} onClick={() => void save()}>
            {saving ? "Saving" : "Save"}
          </Button>
        </>
      }
    >
      <div className="border-b border-line">
        <Tabs
          value={mode}
          onChange={switchMode}
          items={[
            { id: "form", label: "Form" },
            { id: "json", label: "JSON" },
          ]}
        />
      </div>
      {mode === "json" ? (
        <div className="p-5">
          <Textarea value={json} onChange={(e) => setJson(e.target.value)} className="min-h-[420px] text-[12px]" aria-label="Entry JSON" />
        </div>
      ) : (
        <div className="grid gap-4 p-5">
          <div className="grid grid-cols-2 gap-3">
            <Field label="Name">
              <Input value={draft.name ?? ""} onChange={(e) => patch({ name: e.target.value })} data-af />
            </Field>
            <Field label="Prefix">
              <Input value={draft.prefix ?? ""} onChange={(e) => patch({ prefix: e.target.value })} placeholder="team" />
            </Field>
          </div>
          <Field label="Base URL">
            <Input value={draft["base-url"] ?? ""} onChange={(e) => patch({ "base-url": e.target.value })} placeholder="default" className="mono text-[12px]" />
          </Field>
          <div className="grid grid-cols-2 gap-3">
            <Field label="Proxy URL">
              <Input value={draft["proxy-url"] ?? ""} onChange={(e) => patch({ "proxy-url": e.target.value })} placeholder="direct" className="mono text-[12px]" />
            </Field>
            <Field label="Priority">
              <Input inputMode="numeric" value={priorityText} onChange={(e) => setPriorityText(e.target.value)} placeholder="0" />
            </Field>
          </div>
          {group === "openai-compatibility" && (
            <div className="flex items-center justify-between">
              <span>Disabled</span>
              <Switch checked={draft.disabled === true} onChange={(v) => patch({ disabled: v })} label="Disabled" />
            </div>
          )}

          <ListEditor
            title="Keys"
            onAdd={() => {
              patch({ keys: [...keys, { "api-key": "" }] });
              setWeightText([...weightText, ""]);
            }}
          >
            {keys.map((k, i) => (
              <div key={i} className="flex gap-2">
                <Input value={k["api-key"]} onChange={(e) => setKey(i, { "api-key": e.target.value })} placeholder="api key" className="mono text-[12px]" aria-label="API key" />
                <Input
                  value={weightText[i] ?? ""}
                  onChange={(e) => setWeightText(weightText.map((w, j) => (j === i ? e.target.value : w)))}
                  placeholder="weight"
                  inputMode="numeric"
                  className="w-20"
                  aria-label="Weight"
                />
                <IconButton icon="x" label="Remove key" onClick={() => {
                    patch({ keys: keys.filter((_, j) => j !== i) });
                    setWeightText(weightText.filter((_, j) => j !== i));
                  }}
                />
              </div>
            ))}
          </ListEditor>

          <ListEditor title="Models" onAdd={() => patch({ models: [...models, { name: "" }] })}>
            {models.map((m, i) => (
              <div key={i} className="flex gap-2">
                <Input value={m.name} onChange={(e) => setModel(i, { name: e.target.value })} placeholder="upstream name" className="mono text-[12px]" aria-label="Model name" />
                <Input value={m.alias ?? ""} onChange={(e) => setModel(i, { alias: e.target.value })} placeholder="alias" className="mono text-[12px]" aria-label="Alias" />
                <IconButton icon="x" label="Remove model" onClick={() => patch({ models: models.filter((_, j) => j !== i) })} />
              </div>
            ))}
          </ListEditor>

          <Field label="Excluded models, one per line">
            <Textarea value={excluded} onChange={(e) => setExcluded(e.target.value)} className="min-h-16 text-[12px]" />
          </Field>
          <Field label="Headers, Name: value per line">
            <Textarea value={headers} onChange={(e) => setHeaders(e.target.value)} className="min-h-16 text-[12px]" />
          </Field>
        </div>
      )}
    </Sheet>
  );
}

function ListEditor({ title, onAdd, children }: { title: string; onAdd: () => void; children: React.ReactNode }) {
  return (
    <div className="grid gap-2">
      <div className="flex items-center justify-between">
        <span className="text-[12px] text-muted">{title}</span>
        <button className="text-[12px] text-muted transition-colors hover:text-fg" onClick={onAdd}>
          Add
        </button>
      </div>
      {children}
    </div>
  );
}
