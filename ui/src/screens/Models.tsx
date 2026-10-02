import { useMemo, useState } from "react";
import { NONE, fmtCompact } from "@/lib/format";
import { useRowNav } from "@/lib/hotkeys";
import { useChannelModels, useClientKeys, useServedModels } from "@/lib/queries";
import { href } from "@/lib/route";
import { toast } from "@/lib/toast";
import type { ModelInfo } from "@/lib/types";
import { EmptyState, ErrorState, LoadingRows, PageHeader, SearchInput, Tabs } from "@/ui/primitives";

// `served` is what /v1/models returns to clients. The rest are static catalogs per channel.
const CHANNELS = ["claude", "codex", "gemini", "gemini-interactions", "vertex", "aistudio", "antigravity", "kimi", "xai", "devin", "meta"];

function thinking(m: ModelInfo): string {
  const t = m.thinking;
  if (!t) return NONE;
  if (t.levels?.length) return t.levels.join(" ");
  if (t.max) return `${fmtCompact(t.min ?? 0)} to ${fmtCompact(t.max)}`;
  return NONE;
}

export default function Models({ sub }: { sub: string[] }) {
  const [tab, setTab] = useState("served");
  const [search, setSearch] = useState(sub[0] ?? "");
  const keys = useClientKeys();
  const served = useServedModels();
  const channel = useChannelModels(tab === "served" ? null : tab);
  const q = tab === "served" ? served : channel;

  const rows = useMemo(() => {
    const needle = search.trim().toLowerCase();
    const all = q.data ?? [];
    return all.filter((m) => !needle || [m.id, m.display_name, m.owned_by].some((s) => s?.toLowerCase().includes(needle)));
  }, [q.data, search]);

  const copy = (m: ModelInfo) => void navigator.clipboard.writeText(m.id).then(() => toast.ok(`Copied ${m.id}`));
  const nav = useRowNav(rows, copy);
  const noKeys = tab === "served" && keys.isSuccess && keys.data.length === 0;

  return (
    <>
      <PageHeader title="Models">
        <SearchInput value={search} onChange={(e) => setSearch(e.target.value)} placeholder="Search models" aria-label="Search models" className="w-64" />
      </PageHeader>
      <div className="shrink-0 border-b border-line">
        <Tabs
          value={tab}
          onChange={setTab}
          items={[{ id: "served", label: "Served", count: tab === "served" ? served.data?.length : undefined }, ...CHANNELS.map((c) => ({ id: c, label: c }))]}
        />
      </div>
      <div className="min-h-0 flex-1 overflow-auto">
        {noKeys && (
          <EmptyState
            title="No client key"
            hint="Listing served models needs one client key."
            action={
              <a href={href("keys", "add")} className="inline-flex h-7 items-center rounded-md bg-white px-2.5 text-[13px] font-medium text-black hover:bg-[#e6e6e6]">
                Add key
              </a>
            }
          />
        )}
        {!noKeys && q.isLoading && <LoadingRows />}
        {!noKeys && q.isError && <ErrorState error={q.error} onRetry={() => void q.refetch()} />}
        {!noKeys && q.isSuccess && rows.length === 0 && <EmptyState title={search ? "No matches" : "No models"} />}
        {rows.length > 0 && tab === "served" && (
          <table className="tbl">
            <thead>
              <tr>
                <th className="w-[50%]">Model</th>
                <th className="w-[30%]">Owner</th>
                <th>Created</th>
              </tr>
            </thead>
            <tbody>
              {rows.map((m, i) => (
                <tr key={m.id} data-active={nav.index === i} data-row-index={i} className="clickable" onClick={() => copy(m)}>
                  <td className="mono">{m.id}</td>
                  <td className="text-muted">{m.owned_by ?? NONE}</td>
                  <td className="num text-muted">{m.created ? new Date(m.created * 1000).toLocaleDateString("en-CA") : NONE}</td>
                </tr>
              ))}
            </tbody>
          </table>
        )}
        {rows.length > 0 && tab !== "served" && (
          <table className="tbl">
            <thead>
              <tr>
                <th className="w-[34%]">Model</th>
                <th className="w-[24%]">Name</th>
                <th className="w-[10%] text-right">Context</th>
                <th className="w-[10%] text-right">Max output</th>
                <th>Thinking</th>
              </tr>
            </thead>
            <tbody>
              {rows.map((m, i) => (
                <tr key={m.id} data-active={nav.index === i} data-row-index={i} className="clickable" onClick={() => copy(m)}>
                  <td className="mono">{m.id}</td>
                  <td className="text-fg-2">{m.display_name ?? NONE}</td>
                  <td className="num text-right text-muted">{fmtCompact(m.context_length)}</td>
                  <td className="num text-right text-muted">{fmtCompact(m.max_completion_tokens)}</td>
                  <td className="mono text-[12px] text-muted">{thinking(m)}</td>
                </tr>
              ))}
            </tbody>
          </table>
        )}
      </div>
    </>
  );
}
