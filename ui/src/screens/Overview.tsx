import { useMemo, useState, type ReactNode } from "react";
import { useServerMeta } from "@/lib/api";
import { NONE, clock, fmtCompact, fmtInt, fmtMs, fmtPct, fmtVersion, isSemver, maskKey, sameVersion } from "@/lib/format";
import {
  useApiKeyUsage,
  useCredentials,
  useHealth,
  useLatestVersion,
  useRequestFeed,
  useUsageSummary,
} from "@/lib/queries";
import { href } from "@/lib/route";
import type { CredentialFile, RecentBucket, UsageAgg, UsageEvent } from "@/lib/types";
import { EmptyState, PageHeader, Section, Spark, StatusDot, cx } from "@/ui/primitives";

interface Bar {
  label: string;
  requests: number;
  failed: number;
  tokens?: number;
}

export default function Overview() {
  const creds = useCredentials();
  const summary = useUsageSummary();
  const apiUsage = useApiKeyUsage();
  const feed = useRequestFeed();
  const health = useHealth();
  const latest = useLatestVersion();
  const meta = useServerMeta();

  const files = creds.data ?? [];
  const sum = summary.data ?? null;

  // Without the usage extension, totals fall back to what the base API tracks.
  const fallback = useMemo(() => {
    let success = 0;
    let failed = 0;
    for (const f of files) {
      success += f.success;
      failed += f.failed;
    }
    for (const group of Object.values(apiUsage.data ?? {})) {
      for (const u of Object.values(group)) {
        success += u.success;
        failed += u.failed;
      }
    }
    return { requests: success + failed, failed };
  }, [files, apiUsage.data]);

  const requests = sum ? sum.totals.requests : fallback.requests;
  const failed = sum ? sum.totals.failed : fallback.failed;
  const tokens = sum?.totals.tokens;

  const bars = useMemo<Bar[]>(() => (sum ? hourlyBars(sum.hourly) : bucketBars(files, apiUsage.data ?? {})), [sum, files, apiUsage.data]);

  const active = files.filter((f) => !f.disabled && !f.unavailable).length;
  const cooling = files.filter((f) => !f.disabled && (f.unavailable || (f.cooldowns?.length ?? 0) > 0)).length;
  const up = health.data === true;
  const newer = latest.data && meta.version && isSemver(meta.version) && !sameVersion(latest.data, meta.version) ? latest.data : null;

  return (
    <>
      <PageHeader title="Overview" />
      <div className="min-h-0 flex-1 overflow-y-auto">
        <div className="grid grid-cols-2 border-b border-line md:grid-cols-3 xl:grid-cols-6">
          <Stat label="Requests" value={fmtCompact(requests)} meta={failed > 0 ? <span className="text-bad">{fmtInt(failed)} failed</span> : "0 failed"} />
          <Stat label="Success rate" value={requests ? fmtPct((requests - failed) / requests) : NONE} />
          <Stat label="Input tokens" value={tokens ? fmtCompact(tokens.input_tokens) : NONE} meta={tokens ? `${fmtCompact(tokens.cached_tokens)} cached` : undefined} />
          <Stat label="Output tokens" value={tokens ? fmtCompact(tokens.output_tokens) : NONE} meta={tokens && tokens.reasoning_tokens ? `${fmtCompact(tokens.reasoning_tokens)} reasoning` : undefined} />
          <Stat
            label="Credentials"
            value={creds.isSuccess ? `${active}/${files.length}` : NONE}
            meta={cooling > 0 ? <span className="text-warn">{cooling} cooling</span> : "all available"}
          />
          <Stat
            label="Server"
            value={
              <span className="flex items-center gap-2">
                <StatusDot tone={health.isLoading ? "off" : up ? "ok" : "bad"} />
                <span>{health.isLoading ? NONE : up ? "Online" : "Offline"}</span>
              </span>
            }
            meta={
              <span className="mono">
                {meta.version ? fmtVersion(meta.version) : NONE}
                {newer && <span className="ml-2 text-accent">{newer} available</span>}
              </span>
            }
          />
        </div>

        <div className="grid gap-x-8 px-5 pb-8 xl:grid-cols-[minmax(0,1.45fr)_minmax(0,1fr)]">
          <div className="min-w-0">
            <Section title="Activity" right={<span className="text-[12px] text-muted">{sum ? "Last 24 hours" : "Last 3 hours"}</span>} className="pt-3">
              <BarChart bars={bars} />
            </Section>
            <Section title="Recent requests" right={<a href={href("logs", "requests")} className="text-[12px] text-muted transition-colors hover:text-fg">View all</a>} className="pt-5">
              {feed.data === null || feed.isError ? (
                <EmptyState title="No request feed" hint="This server does not provide /observability/requests." />
              ) : (
                <RecentRequests events={feed.data?.events.slice(0, 12) ?? []} loading={feed.isLoading} />
              )}
            </Section>
          </div>

          <div className="min-w-0">
            <Section title="Models" className="pt-3">
              {sum ? <ModelsTable rows={sum.models.slice(0, 8)} /> : <Unavailable />}
            </Section>
            <Section title="Credentials" right={<a href={href("credentials")} className="text-[12px] text-muted transition-colors hover:text-fg">Manage</a>} className="pt-5">
              <CredentialUsage files={files} summary={sum?.credentials} />
            </Section>
            <Section title="API keys" right={<a href={href("keys")} className="text-[12px] text-muted transition-colors hover:text-fg">Manage</a>} className="pt-5">
              {sum ? <KeyUsage rows={sum.api_keys} /> : <Unavailable />}
            </Section>
          </div>
        </div>
      </div>
    </>
  );
}

function Unavailable() {
  return <div className="flex h-24 items-center text-[12.5px] text-faint">Needs /observability/usage/summary</div>;
}

function Stat({ label, value, meta }: { label: string; value: ReactNode; meta?: ReactNode }) {
  return (
    <div className="border-line px-5 py-4 md:border-l md:first:border-l-0 [&:nth-child(odd)]:border-l-0 md:[&:nth-child(odd)]:border-l">
      <div className="text-[12px] text-muted">{label}</div>
      <div className="num mt-1 text-[22px] leading-[1.15] font-medium tracking-[-0.02em]">{value}</div>
      <div className="mt-1 h-4 text-[12px] text-muted">{meta}</div>
    </div>
  );
}

function hourlyBars(hourly: { hour: string; requests: number; failed: number; tokens: { total_tokens: number } }[]): Bar[] {
  return hourly.map((h) => ({
    label: new Date(h.hour).toLocaleTimeString("en-GB", { hour: "2-digit", minute: "2-digit", hour12: false }),
    requests: h.requests,
    failed: h.failed,
    tokens: h.tokens.total_tokens,
  }));
}

// Sum the per-credential and per-key 10 minute buckets the base API reports.
function bucketBars(files: CredentialFile[], usage: Record<string, Record<string, { recent_requests?: RecentBucket[] }>>): Bar[] {
  const acc = new Map<string, { success: number; failed: number }>();
  const add = (list: RecentBucket[] | undefined) => {
    for (const b of list ?? []) {
      const cur = acc.get(b.time) ?? { success: 0, failed: 0 };
      cur.success += b.success;
      cur.failed += b.failed;
      acc.set(b.time, cur);
    }
  };
  for (const f of files) add(f.recent_requests);
  for (const g of Object.values(usage)) for (const u of Object.values(g)) add(u.recent_requests);
  return [...acc.entries()].map(([time, v]) => ({ label: time.split("-")[0] ?? time, requests: v.success + v.failed, failed: v.failed }));
}

function BarChart({ bars }: { bars: Bar[] }) {
  const [hover, setHover] = useState<number | null>(null);
  const max = Math.max(0, ...bars.map((b) => b.requests));
  const niceMax = niceCeil(Math.max(1, max));
  const H = 132;
  const n = Math.max(1, bars.length);
  const active = hover !== null ? bars[hover] : undefined;
  const labelEvery = n > 12 ? Math.ceil(n / 6) : 1;

  return (
    <div>
      <div className="num flex h-5 items-center gap-3 text-[12px]" aria-live="polite">
        {active ? (
          <>
            <span className="text-fg">{active.label}</span>
            <span>{fmtInt(active.requests)} requests</span>
            {active.failed > 0 && <span className="text-bad">{fmtInt(active.failed)} failed</span>}
            {active.tokens !== undefined && <span className="text-muted">{fmtCompact(active.tokens)} tokens</span>}
          </>
        ) : (
          <span className="text-faint">{max === 0 ? "No activity" : `Peak ${fmtInt(max)}`}</span>
        )}
      </div>
      <div className="relative mt-1.5 pl-8">
        {[1, 0.5, 0].map((f) => (
          <div key={f} className="pointer-events-none absolute right-0 left-8 border-t border-line" style={{ top: `${(1 - f) * H}px` }}>
            <span className="num absolute -top-[7px] -left-8 w-6 text-right text-[11px] text-faint">{fmtCompact(Math.round(niceMax * f))}</span>
          </div>
        ))}
        <div className="relative flex items-end gap-[3px]" style={{ height: H }} onMouseLeave={() => setHover(null)}>
          {bars.map((b, i) => {
            const h = (b.requests / niceMax) * H;
            const hf = b.requests ? (b.failed / b.requests) * h : 0;
            return (
              <div key={i} className="relative flex h-full flex-1 items-end" onMouseEnter={() => setHover(i)}>
                <div className="w-full" style={{ height: Math.max(b.requests ? 2 : 0, h) }}>
                  <div className={cx("w-full", hover === i ? "bg-white" : "bg-[#9a9a9a]")} style={{ height: `calc(100% - ${hf}px)` }} />
                  {hf > 0 && <div className="w-full bg-bad" style={{ height: hf }} />}
                </div>
              </div>
            );
          })}
        </div>
        <div className="num mt-1.5 flex gap-[3px] text-[11px] text-faint">
          {bars.map((b, i) => (
            <div key={i} className="flex-1 overflow-visible whitespace-nowrap">
              {i % labelEvery === 0 ? b.label : ""}
            </div>
          ))}
        </div>
      </div>
    </div>
  );
}

function niceCeil(n: number): number {
  if (n <= 4) return 4;
  const pow = 10 ** Math.floor(Math.log10(n));
  for (const m of [1, 1.5, 2, 2.5, 3, 4, 5, 6, 8, 10]) if (m * pow >= n) return m * pow;
  return 10 * pow;
}

function RecentRequests({ events, loading }: { events: UsageEvent[]; loading: boolean }) {
  if (!loading && events.length === 0) return <EmptyState title="No requests yet" />;
  return (
    <table className="tbl tbl-compact -mt-px">
      <thead>
        <tr>
          <th className="fit">Time</th>
          <th>Model</th>
          <th>Credential</th>
          <th className="hidden 2xl:table-cell">Endpoint</th>
          <th className="text-right">Tokens</th>
          <th className="text-right">Latency</th>
        </tr>
      </thead>
      <tbody>
        {events.map((e) => (
          <tr key={e.seq}>
            <td className="fit num text-muted">
              <span className="flex items-center gap-2">
                <StatusDot tone={e.failed ? "bad" : "ok"} />
                {clock(e.timestamp)}
              </span>
            </td>
            <td className="mono text-[12px]">{e.alias || e.model}</td>
            <td className="text-muted">
              {e.provider} <span className="text-faint">{e.source && e.source !== e.provider ? e.source : ""}</span>
            </td>
            <td className="mono hidden text-[12px] text-muted 2xl:table-cell">{e.endpoint?.replace(/^POST /, "")}</td>
            <td className="num text-right text-muted">{e.failed ? NONE : `${fmtCompact(e.tokens.input_tokens)} / ${fmtCompact(e.tokens.output_tokens)}`}</td>
            <td className="num text-right text-muted">{fmtMs(e.latency_ms)}</td>
          </tr>
        ))}
      </tbody>
    </table>
  );
}

function errRate(a: UsageAgg): string {
  return a.requests ? fmtPct(a.failed / a.requests) : NONE;
}

function ModelsTable({ rows }: { rows: (UsageAgg & { model: string })[] }) {
  if (rows.length === 0) return <EmptyState title="No usage yet" />;
  return (
    <table className="tbl tbl-compact">
      <thead>
        <tr>
          <th>Model</th>
          <th className="text-right">Requests</th>
          <th className="text-right">Tokens</th>
          <th className="text-right">Errors</th>
        </tr>
      </thead>
      <tbody>
        {rows.map((r) => (
          <tr key={r.model}>
            <td className="mono text-[12px]">{r.model}</td>
            <td className="num text-right">{fmtInt(r.requests)}</td>
            <td className="num text-right text-muted">{fmtCompact(r.tokens.total_tokens)}</td>
            <td className={cx("num text-right", r.failed ? "text-bad" : "text-faint")}>{errRate(r)}</td>
          </tr>
        ))}
      </tbody>
    </table>
  );
}

function CredentialUsage({ files, summary }: { files: CredentialFile[]; summary?: (UsageAgg & { auth_index: string })[] }) {
  const agg = new Map(summary?.map((s) => [s.auth_index, s]));
  const reqs = (f: CredentialFile) => agg.get(f.auth_index)?.requests ?? f.success + f.failed;
  const failedOf = (f: CredentialFile) => agg.get(f.auth_index)?.failed ?? f.failed;
  const rows = [...files].sort((a, b) => reqs(b) - reqs(a)).slice(0, 8);
  if (rows.length === 0) return <EmptyState title="No credentials" />;
  return (
    <table className="tbl tbl-compact">
      <thead>
        <tr>
          <th>Account</th>
          <th className="text-right">Requests</th>
          {summary && <th className="text-right">Tokens</th>}
          <th className="text-right">Last 3h</th>
        </tr>
      </thead>
      <tbody>
        {rows.map((f) => (
          <tr key={f.id} className={f.disabled ? "opacity-50" : undefined}>
            <td>
              <span className="text-fg-2">{f.label || f.email || f.name}</span> <span className="text-faint">{f.provider}</span>
            </td>
            <td className="num text-right">
              {fmtInt(reqs(f))}
              {failedOf(f) > 0 && <span className="ml-1.5 text-bad">{failedOf(f)}</span>}
            </td>
            {summary && <td className="num text-right text-muted">{agg.has(f.auth_index) ? fmtCompact(agg.get(f.auth_index)?.tokens.total_tokens) : NONE}</td>}
            <td className="fit">
              <div className="flex justify-end">{f.recent_requests ? <Spark buckets={f.recent_requests} width={72} /> : null}</div>
            </td>
          </tr>
        ))}
      </tbody>
    </table>
  );
}

function KeyUsage({ rows }: { rows: (UsageAgg & { api_key: string })[] }) {
  if (rows.length === 0) return <EmptyState title="No usage yet" />;
  return (
    <table className="tbl tbl-compact">
      <thead>
        <tr>
          <th>Key</th>
          <th className="text-right">Requests</th>
          <th className="text-right">Tokens</th>
          <th className="text-right">Errors</th>
        </tr>
      </thead>
      <tbody>
        {rows.slice(0, 6).map((r) => (
          <tr key={r.api_key}>
            <td className="mono text-[12px]">{maskKey(r.api_key)}</td>
            <td className="num text-right">{fmtInt(r.requests)}</td>
            <td className="num text-right text-muted">{fmtCompact(r.tokens.total_tokens)}</td>
            <td className={cx("num text-right", r.failed ? "text-bad" : "text-faint")}>{errRate(r)}</td>
          </tr>
        ))}
      </tbody>
    </table>
  );
}
