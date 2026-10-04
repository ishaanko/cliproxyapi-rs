// Subscription limits per credential. Two sources feed one shape:
//  - passive: quota headers the server stored from the account's last upstream response
//    (`quota.signals` on /credentials), free and always present once the account served a request;
//  - live: a check the user starts, which asks the provider's usage endpoint through
//    /requests/api-call with the credential's own token ($TOKEN$ is substituted server-side).
// Payload parsing follows the formats documented by vayungodara/cliproxy-rs (MIT).
import { useQuery, type QueryClient } from "@tanstack/react-query";
import { api } from "./api";
import type { CredentialFile, QuotaObservation } from "./types";

export interface UsageWindow {
  label: string;
  /** Percent used, 0..100. Null for windows without a cap (extra usage with no limit). */
  used: number | null;
  /** Epoch ms. */
  resetsAt: number | null;
  /** Replaces the reset text, e.g. "$6.40 of $20.00". */
  detail?: string;
}

export interface Limits {
  windows: UsageWindow[];
  /** Epoch ms of the observation or check. */
  at: number;
  live: boolean;
}

type Json = Record<string, unknown>;
const obj = (v: unknown): Json | undefined => (v && typeof v === "object" && !Array.isArray(v) ? (v as Json) : undefined);
const num = (v: unknown): number | null => {
  const n = typeof v === "string" ? Number.parseFloat(v) : v;
  return typeof n === "number" && Number.isFinite(n) ? n : null;
};
const clamp = (n: number) => Math.round(Math.max(0, Math.min(100, n)) * 10) / 10;
const epochMs = (secs: number | null) => (secs && secs > 0 ? secs * 1000 : null);
const isoMs = (v: unknown) => (typeof v === "string" && v ? Date.parse(v) || null : null);

const windowLabel = (seconds: number | null, fallback: string) =>
  seconds === 18_000 ? "5-hour limit" : seconds === 604_800 ? "Weekly limit" : fallback;

/** Windows from the stored response headers. Expired windows are dropped: their allowance is back. */
export function signalWindows(provider: string, quota: QuotaObservation | undefined): UsageWindow[] {
  const sig = new Map(Object.entries(quota?.signals ?? {}).map(([k, v]) => [k.toLowerCase(), v]));
  const observed = isoMs(quota?.observed_at) ?? Date.now();
  const out: UsageWindow[] = [];
  if (provider === "claude") {
    for (const [key, label] of [
      ["5h", "Current session"],
      ["7d", "Current week"],
    ] as const) {
      const p = `anthropic-ratelimit-unified-${key}-`;
      const util = num(sig.get(`${p}utilization`));
      if (util === null) continue;
      const used = sig.get(`${p}status`) === "rejected" ? 100 : clamp(util * 100);
      out.push({ label, used, resetsAt: epochMs(num(sig.get(`${p}reset`))) });
    }
  } else if (provider === "codex") {
    for (const w of ["primary", "secondary"] as const) {
      const p = `x-codex-${w}-`;
      let used = num(sig.get(`${p}used-percent`));
      if (used === null) continue;
      if (w === "primary" && sig.get("x-codex-limit-reached") === "true") used = 100;
      const minutes = num(sig.get(`${p}window-minutes`));
      const after = num(sig.get(`${p}reset-after-seconds`));
      out.push({
        label: windowLabel(minutes === null ? null : minutes * 60, w === "primary" ? "Primary limit" : "Secondary limit"),
        used: clamp(used),
        resetsAt: epochMs(num(sig.get(`${p}reset-at`))) ?? (after !== null ? observed + after * 1000 : null),
      });
    }
  } else if (provider === "devin") {
    for (const w of ["daily", "weekly"] as const) {
      const left = num(sig.get(`${w}_quota_remaining_percent`));
      if (left === null) continue;
      out.push({ label: w === "daily" ? "Daily limit" : "Weekly limit", used: clamp(100 - left), resetsAt: isoMs(sig.get(`${w}_quota_reset_at`)) });
    }
  }
  const now = Date.now();
  return out.filter((w) => w.resetsAt === null || w.resetsAt > now);
}

// Claude's usage buckets, titled as Claude Code's /usage titles them. Unknown buckets are skipped.
const claudeBuckets: [string, string][] = [
  ["five_hour", "Current session"],
  ["seven_day", "Current week (all models)"],
  ["seven_day_sonnet", "Current week (Sonnet only)"],
  ["seven_day_opus", "Current week (Opus only)"],
];

function money(minor: number, places: number, currency: string): string {
  const amount = minor / 10 ** places;
  try {
    return new Intl.NumberFormat("en-US", { style: "currency", currency }).format(amount);
  } catch {
    return `${amount.toFixed(places)} ${currency}`;
  }
}

function claudeUsage(payload: Json): UsageWindow[] {
  const out: UsageWindow[] = [];
  const limits = Array.isArray(payload.limits) ? payload.limits.map(obj).filter((l) => l !== undefined) : [];
  if (limits.length) {
    for (const l of limits) {
      const used = num(l.percent);
      const model = obj(obj(l.scope)?.model)?.display_name;
      const label =
        l.kind === "session"
          ? "Current session"
          : l.kind === "weekly_all"
            ? "Current week (all models)"
            : l.kind === "weekly_scoped" && typeof model === "string"
              ? `Current week (${model} only)`
              : "";
      if (label && used !== null) out.push({ label, used: clamp(used), resetsAt: isoMs(l.resets_at) });
    }
  } else {
    for (const [key, label] of claudeBuckets) {
      const b = obj(payload[key]);
      const used = num(b?.utilization);
      if (used !== null) out.push({ label, used: clamp(used), resetsAt: isoMs(b?.resets_at) });
    }
  }
  const extra = obj(payload.extra_usage);
  const spent = num(extra?.used_credits);
  if (extra?.is_enabled === true && spent !== null) {
    const places = num(extra.decimal_places) ?? 2;
    const currency = typeof extra.currency === "string" && extra.currency ? extra.currency : "USD";
    const cap = num(extra.monthly_limit);
    out.push({
      label: "Extra usage",
      used: cap === null ? null : (num(extra.utilization) ?? (cap > 0 ? clamp((spent / cap) * 100) : 0)),
      resetsAt: null,
      detail: cap === null ? `${money(spent, places, currency)} spent, no limit` : `${money(spent, places, currency)} of ${money(cap, places, currency)}`,
    });
  }
  return out;
}

function codexUsage(payload: Json): UsageWindow[] {
  const out: UsageWindow[] = [];
  for (const [key, v] of Object.entries(obj(payload.rate_limit) ?? {})) {
    const w = obj(v);
    const used = num(w?.used_percent);
    if (!w || used === null) continue;
    const after = num(w.reset_after_seconds);
    out.push({
      label: windowLabel(num(w.limit_window_seconds), key === "primary_window" ? "Primary limit" : "Secondary limit"),
      used: clamp(used),
      resetsAt: epochMs(num(w.reset_at)) ?? (after !== null ? Date.now() + after * 1000 : null),
    });
  }
  return out;
}

const usageEndpoint: Record<string, { url: string; parse: (p: Json) => UsageWindow[] }> = {
  claude: { url: "https://api.anthropic.com/api/oauth/usage", parse: claudeUsage },
  codex: { url: "https://chatgpt.com/backend-api/wham/usage", parse: codexUsage },
};

/** A live check is possible: OAuth account of a provider with a known usage endpoint. */
export const canCheck = (f: CredentialFile) => !f.disabled && f.provider in usageEndpoint && f.source === "file";

async function fetchUsage(f: CredentialFile): Promise<Limits> {
  const ep = usageEndpoint[f.provider];
  if (!ep) throw new Error(`No usage endpoint for ${f.provider}`);
  const header: Record<string, string> = { Authorization: "Bearer $TOKEN$", "Content-Type": "application/json" };
  if (f.provider === "claude") header["anthropic-beta"] = "oauth-2025-04-20";
  const account = f.id_token?.chatgpt_account_id;
  if (f.provider === "codex" && account) header["Chatgpt-Account-Id"] = account;
  const r = await api.post<{ status_code: number; body: string }>("/requests/api-call", {
    authIndex: f.auth_index,
    method: "GET",
    url: ep.url,
    header,
  });
  if (r.status_code === 401 || r.status_code === 403) throw new Error(`Token rejected (HTTP ${r.status_code}). Refresh or reconnect the account.`);
  if (r.status_code < 200 || r.status_code >= 300) throw new Error(`Provider answered HTTP ${r.status_code}`);
  let payload: unknown;
  try {
    payload = JSON.parse(r.body);
  } catch {
    throw new Error("Provider answered with something other than JSON");
  }
  return { windows: ep.parse(obj(payload) ?? {}), at: Date.now(), live: true };
}

const checkKey = (f: CredentialFile) => ["quota-check", f.auth_index || f.name] as const;

/** A credential's limits: its latest live check in this tab, else what its responses reported. */
export function useLimits(f: CredentialFile) {
  const check = useQuery({ queryKey: checkKey(f), queryFn: () => fetchUsage(f), enabled: false, staleTime: Infinity, retry: false });
  const passive = signalWindows(f.provider, f.quota);
  const limits: Limits | null = check.data
    ? check.data
    : passive.length
      ? { windows: passive, at: isoMs(f.quota?.observed_at) ?? 0, live: false }
      : null;
  return { limits, check };
}

/** Checks one credential after another so the provider is not hammered. Failures stay on each row. */
export async function checkAll(qc: QueryClient, files: CredentialFile[]) {
  for (const f of files.filter(canCheck)) {
    await qc.fetchQuery({ queryKey: checkKey(f), queryFn: () => fetchUsage(f), staleTime: 0, retry: false }).catch(() => undefined);
  }
}

/** The window closest to its cap, for one-line summaries. */
export function tightest(windows: UsageWindow[]): UsageWindow | undefined {
  return windows.reduce<UsageWindow | undefined>((a, w) => (w.used !== null && (a?.used == null || w.used > a.used) ? w : a), undefined);
}
