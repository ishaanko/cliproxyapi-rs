// Subscription limit rows (Quotas page) and the compact cell used in lists.
import { activeCooldowns, cooldownRemaining } from "@/lib/credential";
import { NONE, fmtDuration, relTime } from "@/lib/format";
import { canCheck, tightest, useLimits, type UsageWindow } from "@/lib/quota";
import type { CredentialFile } from "@/lib/types";
import { DotMeter } from "./dots";
import { Button, cx } from "./primitives";

/** "2h 40m", "3d 4h", "12m": coarse time until an epoch ms. */
export function until(ms: number, now = Date.now()): string {
  const s = Math.max(0, Math.round((ms - now) / 1000));
  if (s < 3600) return `${Math.max(1, Math.round(s / 60))}m`;
  if (s < 86400) return `${Math.floor(s / 3600)}h ${Math.floor((s % 3600) / 60)}m`;
  return `${Math.floor(s / 86400)}d ${Math.floor((s % 86400) / 3600)}h`;
}

const toneText = (used: number) => (used >= 90 ? "text-bad" : used >= 75 ? "text-warn" : "text-fg");

/** One window: label, twenty-dot meter, percent used and when it resets. */
export function WindowRow({ w }: { w: UsageWindow }) {
  return (
    <div className="grid h-8 grid-cols-[minmax(0,1fr)_auto_52px_minmax(0,148px)] items-center gap-4 border-b border-line last:border-0">
      <span className="truncate text-fg-2">{w.label}</span>
      {w.used === null ? <span /> : <DotMeter used={w.used} label={`${w.label} used`} />}
      <span className={cx("num text-right", w.used === null ? "text-muted" : toneText(w.used))}>{w.used === null ? NONE : `${w.used}%`}</span>
      <span className="num truncate text-muted">{w.detail ?? (w.resetsAt ? `resets in ${until(w.resetsAt)}` : "")}</span>
    </div>
  );
}

/** List cell: an active cooldown wins; otherwise the window closest to its cap. */
export function QuotaCell({ f }: { f: CredentialFile }) {
  const { limits } = useLimits(f);
  const cds = activeCooldowns(f);
  if (cds.length > 0) {
    const soonest = Math.min(...cds.map((c) => cooldownRemaining(c)));
    const status = cds.find((c) => c.http_status)?.http_status;
    return <span className="num text-warn">{`cooldown ${fmtDuration(soonest)}${status ? ` (${status})` : ""}`}</span>;
  }
  const w = limits ? tightest(limits.windows) : undefined;
  if (!w || w.used === null) return <span className="text-faint">{NONE}</span>;
  return (
    <span className="flex items-center gap-2.5" title={limits?.windows.map((x) => `${x.label}: ${x.used ?? NONE}%`).join("\n")}>
      <DotMeter used={w.used} label={`${w.label} used`} d={3} gap={2} />
      <span className={cx("num", toneText(w.used))}>{w.used}%</span>
    </span>
  );
}

/** Every known window of a credential, then where the numbers came from (or why a check failed). */
export function LimitsList({ f }: { f: CredentialFile }) {
  const { limits, check } = useLimits(f);
  return (
    <div>
      {limits?.windows.map((w) => <WindowRow key={w.label} w={w} />)}
      {limits?.windows.length === 0 && <div className="flex h-8 items-center text-muted">The provider reported no limits</div>}
      <div className="flex h-7 items-center text-[12px] text-muted">
        {check.isError ? (
          <span className="text-bad">{check.error.message}</span>
        ) : limits ? (
          `${limits.live ? "Checked" : "From responses"} ${relTime(limits.at)}`
        ) : canCheck(f) ? (
          "No limits seen yet. Check asks the provider."
        ) : (
          "No limits seen yet"
        )}
      </div>
    </div>
  );
}

/** Asks the provider's usage endpoint now. Hidden for credentials without one. */
export function CheckButton({ f }: { f: CredentialFile }) {
  const { check } = useLimits(f);
  if (!canCheck(f)) return null;
  return (
    <Button onClick={() => void check.refetch()} disabled={check.isFetching}>
      {check.isFetching ? "Checking" : "Check"}
    </Button>
  );
}
