import { useQueryClient } from "@tanstack/react-query";
import { useMemo, useState } from "react";
import { NONE, fmtCompact, fmtInt, fmtPct, generateKey, maskKey } from "@/lib/format";
import { useRowNav } from "@/lib/hotkeys";
import { qk, updateClientKeys, useClientKeys, useUsageSummary } from "@/lib/queries";
import { go } from "@/lib/route";
import { errorText, toast } from "@/lib/toast";
import { Dialog, confirm } from "@/ui/overlays";
import { Button, CopyButton, EmptyState, ErrorState, IconButton, Input, LoadingRows, PageHeader, cx } from "@/ui/primitives";

export default function ApiKeys({ sub }: { sub: string[] }) {
  const qc = useQueryClient();
  const q = useClientKeys();
  const summary = useUsageSummary();
  const [revealed, setRevealed] = useState<ReadonlySet<string>>(new Set());

  const keys = useMemo(() => q.data ?? [], [q.data]);
  const usage = useMemo(() => new Map(summary.data?.api_keys.map((u) => [u.api_key, u])), [summary.data]);
  const hasUsage = summary.data != null;

  const toggleReveal = (k: string) =>
    setRevealed((prev) => {
      const next = new Set(prev);
      if (!next.delete(k)) next.add(k);
      return next;
    });

  async function remove(key: string) {
    const ok = await confirm({
      title: "Remove API key",
      body: `Clients using ${maskKey(key)} will be rejected.`,
      confirm: "Remove",
      danger: true,
    });
    if (!ok) return;
    try {
      await updateClientKeys((list) => list.filter((k) => k !== key));
      toast.ok("Key removed");
      await qc.invalidateQueries({ queryKey: qk.clientKeys });
    } catch (e) {
      toast.error(errorText(e));
    }
  }

  const nav = useRowNav(keys, (k) => toggleReveal(k), {
    c: (k) => void navigator.clipboard.writeText(k).then(() => toast.ok("Copied")),
    Backspace: (k) => void remove(k),
    Delete: (k) => void remove(k),
  });

  return (
    <>
      <PageHeader title="API keys">
        <Button variant="primary" icon="plus" onClick={() => go("keys", "add")}>
          Add
        </Button>
      </PageHeader>
      <div className="min-h-0 flex-1 overflow-auto">
        {q.isLoading && <LoadingRows rows={4} />}
        {q.isError && <ErrorState error={q.error} onRetry={() => void q.refetch()} />}
        {q.isSuccess && keys.length === 0 && (
          <EmptyState
            title="No client keys"
            hint="Without keys, the proxy accepts no client requests."
            action={
              <Button variant="primary" icon="plus" onClick={() => go("keys", "add")}>
                Add key
              </Button>
            }
          />
        )}
        {keys.length > 0 && (
          <table className="tbl">
            <thead>
              <tr>
                <th>Key</th>
                {hasUsage && (
                  <>
                    <th className="text-right">Requests</th>
                    <th className="text-right">Tokens</th>
                    <th className="text-right">Errors</th>
                  </>
                )}
                <th className="fit" />
              </tr>
            </thead>
            <tbody>
              {keys.map((k, i) => {
                const u = usage.get(k);
                const shown = revealed.has(k);
                return (
                  <tr key={k} data-active={nav.index === i} data-row-index={i} className="group clickable" onClick={() => nav.setIndex(i)}>
                    <td className="mono">{shown ? k : maskKey(k)}</td>
                    {hasUsage && (
                      <>
                        <td className="num text-right">{u ? fmtInt(u.requests) : NONE}</td>
                        <td className="num text-right text-muted">{u ? fmtCompact(u.tokens.total_tokens) : NONE}</td>
                        <td className={cx("num text-right", u?.failed ? "text-bad" : "text-faint")}>{u?.requests ? fmtPct(u.failed / u.requests) : NONE}</td>
                      </>
                    )}
                    <td className="fit" onClick={(e) => e.stopPropagation()}>
                      <div className="flex items-center justify-end gap-0.5">
                        <IconButton icon={shown ? "eyeOff" : "eye"} label={shown ? "Hide" : "Reveal"} onClick={() => toggleReveal(k)} />
                        <CopyButton text={k} label="Copy key" />
                        <IconButton icon="trash" danger label="Remove" onClick={() => void remove(k)} />
                      </div>
                    </td>
                  </tr>
                );
              })}
            </tbody>
          </table>
        )}
      </div>
      {sub[0] === "add" && <AddKey existing={keys} onClose={() => go("keys")} />}
    </>
  );
}

function AddKey({ existing, onClose }: { existing: string[]; onClose: () => void }) {
  const qc = useQueryClient();
  const [value, setValue] = useState(generateKey);
  const [busy, setBusy] = useState(false);
  const trimmed = value.trim();
  const duplicate = existing.includes(trimmed);

  async function add() {
    if (!trimmed || duplicate) return;
    setBusy(true);
    try {
      await updateClientKeys((list) => [...list, trimmed]);
      await qc.invalidateQueries({ queryKey: qk.clientKeys });
      void navigator.clipboard.writeText(trimmed).catch(() => undefined);
      toast.ok("Key added and copied");
      onClose();
    } catch (e) {
      toast.error(errorText(e));
      setBusy(false);
    }
  }

  return (
    <Dialog
      title="Add API key"
      onClose={onClose}
      width={480}
      footer={
        <>
          <Button onClick={onClose}>Cancel</Button>
          <Button variant="primary" disabled={busy || !trimmed || duplicate} onClick={() => void add()}>
            Add
          </Button>
        </>
      }
    >
      <div className="grid gap-2 px-5 py-5">
        <div className="flex gap-2">
          <Input
            data-af
            value={value}
            onChange={(e) => setValue(e.target.value)}
            onKeyDown={(e) => e.key === "Enter" && void add()}
            className="mono text-[12px]"
            aria-label="Key"
          />
          <Button icon="refresh" onClick={() => setValue(generateKey())}>
            Generate
          </Button>
        </div>
        <div className="h-4 text-[12px] text-bad">{duplicate ? "Key already exists" : ""}</div>
      </div>
    </Dialog>
  );
}
