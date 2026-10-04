import { useQueryClient } from "@tanstack/react-query";
import { useMemo, useState } from "react";
import { api } from "@/lib/api";
import { activeCooldowns, credentialState, credentialTitle, planOf } from "@/lib/credential";
import { cap } from "@/lib/format";
import { canCheck, checkAll, signalWindows } from "@/lib/quota";
import { qk, useCredentials } from "@/lib/queries";
import { errorText, toast } from "@/lib/toast";
import type { CredentialFile } from "@/lib/types";
import { CheckButton, LimitsList } from "@/ui/limits";
import { Button, EmptyState, ErrorState, LoadingRows, PageHeader, StatusDot } from "@/ui/primitives";

/** Subscription limits of every account that reports them, grouped by provider. */
export default function Quotas() {
  const qc = useQueryClient();
  const q = useCredentials();
  const [checking, setChecking] = useState(false);

  // An account belongs here once it reported limits or can be checked.
  const groups = useMemo(() => {
    const by = new Map<string, CredentialFile[]>();
    for (const f of q.data ?? []) {
      if (!canCheck(f) && signalWindows(f.provider, f.quota).length === 0) continue;
      by.set(f.provider, [...(by.get(f.provider) ?? []), f]);
    }
    return [...by.entries()];
  }, [q.data]);
  const checkable = (q.data ?? []).filter(canCheck);

  async function runAll() {
    setChecking(true);
    await checkAll(qc, checkable);
    setChecking(false);
  }

  return (
    <>
      <PageHeader title="Quotas">
        {checkable.length > 0 && (
          <Button onClick={() => void runAll()} disabled={checking}>
            {checking ? "Checking" : "Check all"}
          </Button>
        )}
      </PageHeader>
      <div className="min-h-0 flex-1 overflow-y-auto">
        {q.isLoading && <LoadingRows />}
        {q.isError && <ErrorState error={q.error} onRetry={() => void q.refetch()} />}
        {q.isSuccess && groups.length === 0 && (
          <EmptyState title="No limits yet" hint="Claude, Codex and Devin accounts report their limits here after their first request." />
        )}
        <div className="max-w-[960px] px-5 pb-10">
          {groups.map(([provider, files]) => (
            <section key={provider} className="pt-4">
              <h2 className="flex h-9 items-center gap-2 border-b border-line text-[13px] font-medium">
                {cap(provider)}
                <span className="num text-[12px] font-normal text-faint">{files.length}</span>
              </h2>
              {files.map((f) => (
                <Account key={f.id} f={f} />
              ))}
            </section>
          ))}
        </div>
      </div>
    </>
  );
}

function Account({ f }: { f: CredentialFile }) {
  const qc = useQueryClient();
  const state = credentialState(f);
  const plan = planOf(f);
  const cooling = activeCooldowns(f).length > 0;

  async function reset() {
    try {
      await api.post("/routing/cooldown/reset", { auth_index: f.auth_index });
      toast.ok("Cooldown cleared");
      await qc.invalidateQueries({ queryKey: qk.credentials });
    } catch (e) {
      toast.error(errorText(e));
    }
  }

  return (
    <div className="border-b border-line py-3">
      <div className="flex min-h-8 items-center gap-3">
        <StatusDot tone={state.tone} />
        <div className="min-w-0 flex-1">
          <span className="font-medium">{credentialTitle(f)}</span>
          <span className="ml-2 text-muted">{[plan && cap(plan), state.label].filter(Boolean).join(" · ")}</span>
        </div>
        {cooling && (
          <Button variant="ghost" onClick={() => void reset()}>
            Reset cooldown
          </Button>
        )}
        <CheckButton f={f} />
      </div>
      <div className="pl-[19px]">
        <LimitsList f={f} />
      </div>
    </div>
  );
}
