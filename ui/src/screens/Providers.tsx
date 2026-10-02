import { useQueryClient } from "@tanstack/react-query";
import { useMemo } from "react";
import { NONE, maskKey } from "@/lib/format";
import { useRowNav } from "@/lib/hotkeys";
import { qk, updateProviderGroup, useProviders } from "@/lib/queries";
import { go } from "@/lib/route";
import { errorText, toast } from "@/lib/toast";
import type { ProviderEntry } from "@/lib/types";
import { confirm } from "@/ui/overlays";
import { Button, EmptyState, ErrorState, IconButton, LoadingRows, PageHeader, Status, Tabs } from "@/ui/primitives";
import { ProviderSheet } from "./providers/ProviderSheet";

// Groups under `api-keys` in the v8 config tree, in display order.
export const GROUPS = [
  { id: "claude", label: "Claude" },
  { id: "codex", label: "Codex" },
  { id: "gemini", label: "Gemini" },
  { id: "interactions", label: "Gemini Interactions" },
  { id: "vertex", label: "Vertex" },
  { id: "xai", label: "xAI" },
  { id: "meta", label: "Meta" },
  { id: "openai-compatibility", label: "OpenAI compatible" },
] as const;
export type GroupId = (typeof GROUPS)[number]["id"];

const isGroup = (s: string | undefined): s is GroupId => GROUPS.some((g) => g.id === s);

export default function Providers({ sub }: { sub: string[] }) {
  const qc = useQueryClient();
  const q = useProviders();
  const group: GroupId = isGroup(sub[0]) ? sub[0] : "claude";
  const label = GROUPS.find((g) => g.id === group)?.label ?? group;
  const entries = useMemo(() => q.data?.[group] ?? [], [q.data, group]);

  const action = sub[1];
  const editIndex = action !== undefined && action !== "add" && Number.isInteger(Number(action)) ? Number(action) : null;
  const editing = action === "add" ? {} : editIndex !== null ? entries[editIndex] : undefined;

  async function remove(index: number) {
    const e = entries[index];
    if (!e) return;
    const ok = await confirm({ title: "Delete provider", body: `${entryTitle(e, index)} will be removed from the config.`, confirm: "Delete", danger: true });
    if (!ok) return;
    try {
      await updateProviderGroup(group, (list) => list.filter((_, i) => i !== index), { index, entry: e });
      toast.ok("Provider deleted");
      await qc.invalidateQueries({ queryKey: qk.providers });
    } catch (err) {
      toast.error(errorText(err));
    }
  }

  const nav = useRowNav(entries, (e) => go("providers", group, String(entries.indexOf(e))), {
    Backspace: (e) => void remove(entries.indexOf(e)),
    Delete: (e) => void remove(entries.indexOf(e)),
  });

  return (
    <>
      <PageHeader title="Providers">
        <Button variant="primary" icon="plus" onClick={() => go("providers", group, "add")}>
          Add
        </Button>
      </PageHeader>
      <div className="shrink-0 border-b border-line">
        <Tabs
          value={group}
          onChange={(g) => go("providers", g)}
          items={GROUPS.map((g) => ({ id: g.id, label: g.label, count: q.data?.[g.id]?.length ?? 0 }))}
        />
      </div>
      <div className="min-h-0 flex-1 overflow-auto">
        {q.isLoading && <LoadingRows rows={4} />}
        {q.isError && <ErrorState error={q.error} onRetry={() => void q.refetch()} />}
        {q.isSuccess && entries.length === 0 && (
          <EmptyState
            title={`No ${label} providers`}
            action={
              <Button variant="primary" icon="plus" onClick={() => go("providers", group, "add")}>
                Add {label}
              </Button>
            }
          />
        )}
        {entries.length > 0 && (
          <table className="tbl">
            <thead>
              <tr>
                <th className="w-[20%]">Name</th>
                <th className="w-[28%]">Base URL</th>
                <th className="w-[18%]">Keys</th>
                <th className="w-[8%] text-right">Models</th>
                <th className="w-[10%]">Prefix</th>
                <th className="w-[8%] text-right">Priority</th>
                <th className="w-[8%]">State</th>
                <th className="fit" />
              </tr>
            </thead>
            <tbody>
              {entries.map((e, i) => {
                const keys = e.keys ?? [];
                return (
                  <tr
                    key={i}
                    data-active={nav.index === i}
                    data-row-index={i}
                    className="group clickable"
                    onClick={() => {
                      nav.setIndex(i);
                      go("providers", group, String(i));
                    }}
                  >
                    <td className="font-medium">{entryTitle(e, i)}</td>
                    <td className="mono text-[12px] text-fg-2">{e["base-url"] || <span className="text-faint">default</span>}</td>
                    <td className="mono text-[12px] text-muted">
                      {keys.length === 0 ? NONE : maskKey(keys[0]?.["api-key"] ?? "")}
                      {keys.length > 1 && <span className="ml-1.5 text-faint">+{keys.length - 1}</span>}
                    </td>
                    <td className="num text-right">{e.models?.length ?? 0}</td>
                    <td className="text-muted">{e.prefix || NONE}</td>
                    <td className="num text-right text-muted">{e.priority ?? NONE}</td>
                    <td>
                      <Status tone={e.disabled ? "off" : "ok"}>{e.disabled ? "Disabled" : "Enabled"}</Status>
                    </td>
                    <td className="fit" onClick={(ev) => ev.stopPropagation()}>
                      <div className="flex justify-end gap-0.5 opacity-0 transition-opacity group-hover:opacity-100">
                        <IconButton icon="edit" label="Edit" onClick={() => go("providers", group, String(i))} />
                        <IconButton icon="trash" danger label="Delete" onClick={() => void remove(i)} />
                      </div>
                    </td>
                  </tr>
                );
              })}
            </tbody>
          </table>
        )}
      </div>
      {editing && (
        <ProviderSheet
          key={`${group}-${action}`}
          group={group}
          label={label}
          entry={editing}
          index={editIndex}
          onClose={() => go("providers", group)}
        />
      )}
    </>
  );
}

function entryTitle(e: ProviderEntry, index: number): string {
  return e.name || e["base-url"]?.replace(/^https?:\/\//, "") || `${index + 1}`;
}
