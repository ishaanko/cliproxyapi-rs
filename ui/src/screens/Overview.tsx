import { useQuery } from "@tanstack/react-query";
import { useMemo, type ReactNode } from "react";
import { useServerMeta } from "@/lib/api";
import { NONE, clock, fmtCompact, fmtInt, fmtMs, fmtPct, fmtVersion, isSemver, maskKey, sameVersion } from "@/lib/format";
import { credentialState, credentialTitle, planOf } from "@/lib/credential";
import {
  getConfigNode,
  useApiKeyUsage,
  useCredentials,
  useHealth,
  useLatestVersion,
  useRequestFeed,
  useUsageSummary,
} from "@/lib/queries";
import { go, href } from "@/lib/route";
import type { CredentialFile, RecentBucket, UsageAgg, UsageEvent } from "@/lib/types";
import { DotBars, DotStrip, type Column } from "@/ui/dots";
import { QuotaCell } from "@/ui/limits";
import { Button, EmptyState, PageHeader, Section, StatusDot, cx } from "@/ui/primitives";

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

  const columns = useMemo<Column[]>(() => (sum ? hourlyColumns(sum.hourly) : bucketColumns(files, apiUsage.data ?? {})), [sum, files, apiUsage.data]);
  const peak = Math.max(0, ...columns.map((c) => c.requests));

  const active = files.filter((f) => !f.disabled && !f.unavailable).length;
  const cooling = files.filter((f) => !f.disabled && (f.unavailable || (f.cooldowns?.length ?? 0) > 0)).length;
  const up = health.data === true;
  const newer = latest.data && meta.version && isSemver(meta.version) && !sameVersion(latest.data, meta.version) ? latest.data : null;

  return (
    <>
      <PageHeader title="Overview">
        <Button variant="primary" icon="plus" onClick={() => go("credentials", "add")}>
          Connect account
        </Button>
      </PageHeader>
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
        <RoutingLine />

        <div className="grid gap-x-8 px-5 pb-8 xl:grid-cols-2">
          <div className="min-w-0">
            <Section title="Activity" right={<span className="text-[12px] text-muted">{sum ? "Last 24 hours" : "Last 3 hours"}</span>} className="pt-3">
              <DotBars columns={columns} caption={<span className="text-faint">{peak === 0 ? "No activity" : `Peak ${fmtInt(peak)} per ${sum ? "hour" : "10 minutes"}`}</span>} />
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
            <Section title="Accounts" right={<a href={href("quotas")} className="text-[12px] text-muted transition-colors hover:text-fg">Quotas</a>} className="pt-5">
              <Accounts files={files} summary={sum?.credentials} />
            </Section>
            <Section title="Models" className="pt-5">
              {sum ? <ModelsTable rows={sum.models.slice(0, 8)} /> : <Unavailable />}
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

function hourlyColumns(hourly: { hour: string; requests: number; failed: number; tokens: { total_tokens: number } }[]): Column[] {
  return hourly.map((h) => ({
    label: new Date(h.hour).toLocaleTimeString("en-GB", { hour: "2-digit", minute: "2-digit", hour12: false }),
    requests: h.requests,
    failed: h.failed,
    note: `${fmtCompact(h.tokens.total_tokens)} tokens`,
  }));
}

// Sum the per-credential and per-key 10 minute buckets the base API reports.
function bucketColumns(files: CredentialFile[], usage: Record<string, Record<string, { recent_requests?: RecentBucket[] }>>): Column[] {
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

interface RoutingNode {
  strategy?: string;
  "session-affinity"?: boolean;
  retry?: { "request-retry"?: number };
}

/** One line naming how requests are spread over accounts. */
function RoutingLine() {
  const routing = useQuery({ queryKey: ["config", "routing"], queryFn: () => getConfigNode<RoutingNode>("routing"), staleTime: 30_000 });
  if (!routing.isSuccess) return null;
  const r = routing.data ?? {};
  const retries = r.retry?.["request-retry"];
  const parts = [retries !== undefined && `${retries} ${retries === 1 ? "retry" : "retries"}`, r["session-affinity"] && "session affinity"].filter(Boolean);
  return (
    <div className="flex h-9 items-center gap-1.5 border-b border-line px-5 text-[12.5px] text-muted">
      Routing <span className="text-fg">{(r.strategy || "round-robin").replaceAll("-", " ")}</span>
      {parts.map((p) => (
        <span key={String(p)}>· {p}</span>
      ))}
    </div>
  );
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

/** Accounts grouped by provider, busiest first: state, last 3h, tightest limit, requests. */
function Accounts({ files, summary }: { files: CredentialFile[]; summary?: (UsageAgg & { auth_index: string })[] }) {
  const agg = new Map(summary?.map((s) => [s.auth_index, s]));
  const reqs = (f: CredentialFile) => agg.get(f.auth_index)?.requests ?? f.success + f.failed;
  const failedOf = (f: CredentialFile) => agg.get(f.auth_index)?.failed ?? f.failed;
  const groups = new Map<string, CredentialFile[]>();
  for (const f of [...files].sort((a, b) => reqs(b) - reqs(a))) groups.set(f.provider, [...(groups.get(f.provider) ?? []), f]);
  if (files.length === 0) return <EmptyState title="No accounts" hint="Connect an account to start serving requests." />;
  return (
    <table className="tbl tbl-compact">
      <tbody>
        {[...groups.entries()].map(([provider, list]) => [
          <tr key={provider}>
            <td colSpan={4} className="text-[12px] text-muted">
              {provider} <span className="num text-faint">{list.length}</span>
            </td>
          </tr>,
          ...list.map((f) => (
            <tr key={f.id} className={cx("clickable", f.disabled && "opacity-50")} onClick={() => go("credentials", f.name)}>
              <td className="max-w-0!">
                <span className="flex min-w-0 items-center gap-2.5">
                  <StatusDot tone={credentialState(f).tone} />
                  <span className="truncate text-fg-2">{credentialTitle(f)}</span>
                  {planOf(f) && <span className="shrink-0 text-faint">{planOf(f)}</span>}
                </span>
              </td>
              <td className="fit">{f.recent_requests ? <DotStrip buckets={f.recent_requests} d={3} gap={2} /> : null}</td>
              <td className="fit">
                <QuotaCell f={f} />
              </td>
              <td className="fit num text-right">
                {fmtInt(reqs(f))}
                {failedOf(f) > 0 && <span className="ml-1.5 text-bad">{failedOf(f)}</span>}
              </td>
            </tr>
          )),
        ])}
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
