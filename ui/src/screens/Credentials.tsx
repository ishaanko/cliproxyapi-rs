import { useQueryClient } from "@tanstack/react-query";
import { useMemo, useState } from "react";
import { api } from "@/lib/api";
import { credentialState, credentialTitle, planOf } from "@/lib/credential";
import { fmtInt, relTime } from "@/lib/format";
import { useRowNav } from "@/lib/hotkeys";
import { qk, useCredentials } from "@/lib/queries";
import { go } from "@/lib/route";
import { errorText, toast } from "@/lib/toast";
import type { CredentialFile } from "@/lib/types";
import { Button, EmptyState, ErrorState, IconButton, LoadingRows, PageHeader, SearchInput, Status, Switch, Tabs, cx } from "@/ui/primitives";
import { DotStrip } from "@/ui/dots";
import { QuotaCell } from "@/ui/limits";
import { AddCredential } from "./credentials/AddCredential";
import { deleteCredential, downloadCredential } from "./credentials/actions";
import { CredentialSheet } from "./credentials/CredentialSheet";

export default function Credentials({ sub }: { sub: string[] }) {
  const qc = useQueryClient();
  const q = useCredentials();
  const [search, setSearch] = useState("");
  const [provider, setProvider] = useState("all");

  const files = useMemo(() => q.data ?? [], [q.data]);
  const providers = useMemo(() => {
    const counts = new Map<string, number>();
    for (const f of files) counts.set(f.provider, (counts.get(f.provider) ?? 0) + 1);
    return [...counts.entries()].sort((a, b) => b[1] - a[1]);
  }, [files]);

  const rows = useMemo(() => {
    const needle = search.trim().toLowerCase();
    return files
      .filter((f) => provider === "all" || f.provider === provider)
      .filter(
        (f) =>
          !needle ||
          [f.label, f.email, f.name, f.provider, f.note, f.project_id, planOf(f)].some((s) => s?.toLowerCase().includes(needle)),
      );
  }, [files, search, provider]);

  const refresh = () => qc.invalidateQueries({ queryKey: qk.credentials });

  async function toggle(f: CredentialFile, enabled: boolean) {
    try {
      await api.patch("/credentials/status", { name: f.name, auth_index: f.auth_index, disabled: !enabled });
      await refresh();
    } catch (e) {
      toast.error(errorText(e));
    }
  }

  async function remove(f: CredentialFile) {
    try {
      if (await deleteCredential(f)) await refresh();
    } catch (e) {
      toast.error(errorText(e));
    }
  }

  const nav = useRowNav(rows, (f) => go("credentials", f.name), {
    e: (f) => void toggle(f, f.disabled),
    Backspace: (f) => void remove(f),
    Delete: (f) => void remove(f),
  });

  const adding = sub[0] === "add";
  const selected = !adding && sub[0] ? files.find((f) => f.name === sub[0]) : undefined;

  return (
    <>
      <PageHeader title="Credentials">
        <SearchInput value={search} onChange={(e) => setSearch(e.target.value)} placeholder="Filter" aria-label="Filter credentials" className="w-48" />
        <Button variant="primary" icon="plus" onClick={() => go("credentials", "add")}>
          Add
        </Button>
      </PageHeader>
      {providers.length > 1 && (
        <div className="shrink-0 border-b border-line">
          <Tabs
            value={provider}
            onChange={setProvider}
            items={[{ id: "all", label: "All", count: files.length }, ...providers.map(([id, count]) => ({ id, label: id, count }))]}
          />
        </div>
      )}
      <div className="min-h-0 flex-1 overflow-auto">
        {q.isLoading && <LoadingRows />}
        {q.isError && <ErrorState error={q.error} onRetry={() => void q.refetch()} />}
        {q.isSuccess && files.length === 0 && (
          <EmptyState
            title="No credentials"
            hint="Sign in to a provider or upload an auth JSON file."
            action={
              <Button variant="primary" icon="plus" onClick={() => go("credentials", "add")}>
                Add credential
              </Button>
            }
          />
        )}
        {q.isSuccess && files.length > 0 && rows.length === 0 && <EmptyState title="No matches" />}
        {rows.length > 0 && (
          <table className="tbl min-w-[960px]">
            <thead>
              <tr>
                <th className="w-[26%]">Account</th>
                <th className="w-[12%]">Provider</th>
                <th className="w-[11%]">Status</th>
                <th className="w-[16%]">Quota</th>
                <th className="w-[8%] text-right">Requests</th>
                <th className="fit">Last 3h</th>
                <th className="w-[8%]">Refreshed</th>
                <th className="fit" />
              </tr>
            </thead>
            <tbody>
              {rows.map((f, i) => (
                <Row
                  key={f.id}
                  f={f}
                  active={nav.index === i}
                  index={i}
                  onOpen={() => {
                    nav.setIndex(i);
                    go("credentials", f.name);
                  }}
                  onToggle={(v) => void toggle(f, v)}
                  onDelete={() => void remove(f)}
                />
              ))}
            </tbody>
          </table>
        )}
      </div>
      {adding && <AddCredential onClose={() => go("credentials")} />}
      {selected && <CredentialSheet file={selected} onClose={() => go("credentials")} />}
    </>
  );
}

function Row({
  f,
  active,
  index,
  onOpen,
  onToggle,
  onDelete,
}: {
  f: CredentialFile;
  active: boolean;
  index: number;
  onOpen: () => void;
  onToggle: (enabled: boolean) => void;
  onDelete: () => void;
}) {
  const state = credentialState(f);
  const plan = planOf(f);
  return (
    <tr data-active={active} data-row-index={index} className="clickable group" onClick={onOpen}>
      <td className={cx(f.disabled && "text-muted")}>
        <span className="font-medium">{credentialTitle(f)}</span>
        {f.note && <span className="ml-2 text-faint">{f.note}</span>}
      </td>
      <td className="text-fg-2">
        {f.provider}
        {plan && <span className="ml-1.5 text-faint">{plan}</span>}
      </td>
      <td title={f.status_message || undefined}>
        <Status tone={state.tone}>{state.label}</Status>
      </td>
      <td>
        <QuotaCell f={f} />
      </td>
      <td className="num text-right">
        {fmtInt(f.success + f.failed)}
        {f.failed > 0 && <span className="ml-1.5 text-bad">{f.failed}</span>}
      </td>
      <td className="fit">{f.recent_requests ? <DotStrip buckets={f.recent_requests} /> : null}</td>
      <td className="num text-muted">{relTime(f.last_refresh || f.updated_at)}</td>
      <td className="fit" onClick={(e) => e.stopPropagation()}>
        <div className="flex items-center justify-end gap-1">
          {f.source === "file" && (
            <IconButton icon="download" label="Download" className="opacity-0 transition-opacity group-hover:opacity-100 focus-visible:opacity-100" onClick={() => void downloadCredential(f.name).catch((e: unknown) => toast.error(errorText(e)))} />
          )}
          <IconButton icon="trash" danger label="Delete" className="opacity-0 transition-opacity group-hover:opacity-100 focus-visible:opacity-100" onClick={onDelete} />
          <span className="ml-1.5 inline-flex">
            <Switch checked={!f.disabled} onChange={onToggle} label={f.disabled ? "Enable" : "Disable"} />
          </span>
        </div>
      </td>
    </tr>
  );
}
