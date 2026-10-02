import { useQueryClient } from "@tanstack/react-query";
import { useState, type ReactNode } from "react";
import { api } from "@/lib/api";
import { activeCooldowns, cooldownRemaining, credentialState, credentialTitle, planOf } from "@/lib/credential";
import { NONE, dateTime, fmtBytes, fmtDuration, fmtInt, relTime } from "@/lib/format";
import { qk, useCredentialModels } from "@/lib/queries";
import { errorText, toast } from "@/lib/toast";
import type { CredentialFile } from "@/lib/types";
import { Sheet } from "@/ui/overlays";
import { Button, Field, Input, KeyValue, Spark, Status } from "@/ui/primitives";
import { deleteCredential, downloadCredential } from "./actions";

function Block({ title, children }: { title: string; children: ReactNode }) {
  return (
    <section className="border-b border-line px-5 py-4 last:border-0">
      <h3 className="mb-2 text-[13px] font-medium">{title}</h3>
      {children}
    </section>
  );
}

export function CredentialSheet({ file: f, onClose }: { file: CredentialFile; onClose: () => void }) {
  const qc = useQueryClient();
  const state = credentialState(f);
  const cooldowns = activeCooldowns(f);
  const models = useCredentialModels(f.name);
  const [busy, setBusy] = useState<string | null>(null);

  const refresh = () => qc.invalidateQueries({ queryKey: qk.credentials });

  async function run(label: string, fn: () => Promise<unknown>, done: string) {
    setBusy(label);
    try {
      await fn();
      toast.ok(done);
      await refresh();
    } catch (e) {
      toast.error(errorText(e));
    } finally {
      setBusy(null);
    }
  }

  return (
    <Sheet title={credentialTitle(f)} onClose={onClose}>
      <Block title="Details">
        <KeyValue
          rows={[
            ["Status", <Status key="s" tone={state.tone}>{state.label}</Status>],
            ["Provider", [f.provider, planOf(f)].filter(Boolean).join("  ")],
            ["File", <span key="f" className="mono text-[12px]">{f.name}</span>],
            ["Auth index", <span key="i" className="mono text-[12px]">{f.auth_index}</span>],
            ...(f.project_id ? ([["Project", <span key="p" className="mono text-[12px]">{f.project_id}</span>]] as [string, ReactNode][]) : []),
            ["Size", f.size ? fmtBytes(f.size) : NONE],
            ["Created", dateTime(f.created_at)],
            ["Refreshed", f.last_refresh ? `${dateTime(f.last_refresh)}  (${relTime(f.last_refresh)})` : NONE],
            ["Requests", <span key="r" className="num">{fmtInt(f.success)} ok{f.failed > 0 && <span className="ml-2 text-bad">{fmtInt(f.failed)} failed</span>}</span>],
            ...(f.recent_requests ? ([["Last 3h", <Spark key="sp" buckets={f.recent_requests} width={160} height={20} />]] as [string, ReactNode][]) : []),
          ]}
        />
        {f.status_message && <div className="mono mt-2 text-[12px] break-words text-warn">{f.status_message}</div>}
      </Block>

      {(cooldowns.length > 0 || Object.keys(f.quota?.signals ?? {}).length > 0 || Object.keys(f.model_quotas ?? {}).length > 0) && (
        <Block title="Quota and cooldown">
          {cooldowns.length > 0 && (
            <table className="tbl tbl-compact">
              <thead>
                <tr>
                  <th>Scope</th>
                  <th>Reason</th>
                  <th className="text-right">Remaining</th>
                </tr>
              </thead>
              <tbody>
                {cooldowns.map((c, i) => (
                  <tr key={i}>
                    <td className="mono text-[12px]">{c.model_key || c.scope}</td>
                    <td className="text-muted">{c.reason}{c.http_status ? ` (${c.http_status})` : ""}</td>
                    <td className="num text-right text-warn">{fmtDuration(cooldownRemaining(c))}</td>
                  </tr>
                ))}
              </tbody>
            </table>
          )}
          {Object.keys(f.quota?.signals ?? {}).length > 0 && (
            <KeyValue rows={Object.entries(f.quota?.signals ?? {}).map(([k, v]) => [k, <span key={k} className="num">{v}</span>])} />
          )}
          {Object.entries(f.model_quotas ?? {}).map(([model, q]) => (
            <div key={model} className="mt-3">
              <div className="mono mb-1 text-[12px] text-muted">{model}</div>
              <KeyValue rows={Object.entries(q.signals).map(([k, v]) => [k, <span key={k} className="num">{v}</span>])} />
            </div>
          ))}
        </Block>
      )}

      <Block title="Routing">
        <RoutingForm f={f} onSaved={refresh} />
      </Block>

      <Block title={`Models${models.data ? ` (${models.data.length})` : ""}`}>
        {models.isLoading && <div className="text-muted">Loading</div>}
        {models.isError && <div className="text-[12.5px] text-muted">Unavailable</div>}
        {models.data && models.data.length === 0 && <div className="text-[12.5px] text-muted">None registered</div>}
        {models.data && models.data.length > 0 && (
          <ul className="mono grid grid-cols-1 gap-x-4 sm:grid-cols-2">
            {models.data.map((m) => (
              <li key={m.id} className="flex h-7 items-center truncate border-b border-line text-[12px] text-fg-2" title={m.display_name || m.id}>
                {m.id}
              </li>
            ))}
          </ul>
        )}
      </Block>

      <div className="flex flex-wrap gap-2 px-5 py-4">
        <Button icon="refresh" disabled={busy !== null} onClick={() => void run("refresh", () => api.post("/credentials/refresh", { name: f.name }), "Token refreshed")}>
          Refresh token
        </Button>
        {(cooldowns.length > 0 || f.unavailable) && (
          <Button disabled={busy !== null} onClick={() => void run("cooldown", () => api.post("/routing/cooldown/reset", { auth_index: f.auth_index }), "Cooldown cleared")}>
            Clear cooldown
          </Button>
        )}
        {f.source === "file" && (
          <Button icon="download" onClick={() => void downloadCredential(f.name).catch((e: unknown) => toast.error(errorText(e)))}>
            Download
          </Button>
        )}
        <Button
          variant="danger"
          icon="trash"
          onClick={() =>
            void deleteCredential(f)
              .then(async (deleted) => {
                if (deleted) {
                  onClose();
                  await refresh();
                }
              })
              .catch((e: unknown) => toast.error(errorText(e)))
          }
        >
          Delete
        </Button>
      </div>
    </Sheet>
  );
}

// Editable routing metadata: sent as PATCH /credentials/fields. Empty clears the field.
function RoutingForm({ f, onSaved }: { f: CredentialFile; onSaved: () => Promise<unknown> }) {
  const initial = {
    priority: f.priority?.toString() ?? "",
    weight: f.weight?.toString() ?? "",
    // The list never reports prefix, so the field starts empty and means "unchanged".
    prefix: "",
    note: f.note ?? "",
  };
  const [form, setForm] = useState(initial);
  const [saving, setSaving] = useState(false);
  const dirty = (Object.keys(initial) as (keyof typeof initial)[]).some((k) => form[k] !== initial[k]);

  async function save() {
    const body: Record<string, unknown> = { name: f.name };
    for (const k of ["priority", "weight"] as const) {
      if (form[k] === initial[k]) continue;
      if (form[k].trim() === "") body[k] = null;
      else {
        const n = Number(form[k]);
        if (!Number.isInteger(n)) {
          toast.error(`${k} must be an integer`);
          return;
        }
        body[k] = n;
      }
    }
    if (form.prefix.trim() !== "") body.prefix = form.prefix.trim();
    if (form.note !== initial.note) body.note = form.note.trim() === "" ? null : form.note.trim();
    setSaving(true);
    try {
      await api.patch("/credentials/fields", body);
      toast.ok("Saved");
      await onSaved();
    } catch (e) {
      toast.error(errorText(e));
    } finally {
      setSaving(false);
    }
  }

  // The server ignores null for prefix, so clearing sends an empty string.
  async function clearPrefix() {
    setSaving(true);
    try {
      await api.patch("/credentials/fields", { name: f.name, prefix: "" });
      toast.ok("Prefix cleared");
      await onSaved();
    } catch (e) {
      toast.error(errorText(e));
    } finally {
      setSaving(false);
    }
  }

  const set = (k: keyof typeof form) => (e: React.ChangeEvent<HTMLInputElement>) => setForm({ ...form, [k]: e.target.value });
  return (
    <div className="grid gap-3">
      <div className="grid grid-cols-2 gap-3">
        <Field label="Priority">
          <Input inputMode="numeric" value={form.priority} onChange={set("priority")} placeholder="0" />
        </Field>
        <Field label="Weight">
          <Input inputMode="numeric" value={form.weight} onChange={set("weight")} placeholder="1" />
        </Field>
      </div>
      <Field label="Prefix">
        <div className="flex gap-2">
          <Input value={form.prefix} onChange={set("prefix")} placeholder="Unchanged, current value is not reported" />
          <Button disabled={saving} onClick={() => void clearPrefix()}>
            Clear
          </Button>
        </div>
      </Field>
      <Field label="Note">
        <Input value={form.note} onChange={set("note")} />
      </Field>
      <div>
        <Button variant="primary" disabled={!dirty || saving} onClick={() => void save()}>
          {saving ? "Saving" : "Save"}
        </Button>
      </div>
    </div>
  );
}
