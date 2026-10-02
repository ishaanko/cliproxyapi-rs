/** Placeholder for missing values. Plain hyphen by design. */
export const NONE = "-";

const intFmt = new Intl.NumberFormat("en-US");
export const fmtInt = (n: number | undefined | null): string => (n == null ? NONE : intFmt.format(n));

/** 1234 -> 1.2k, 5_300_000 -> 5.3M */
export function fmtCompact(n: number | undefined | null): string {
  if (n == null) return NONE;
  const abs = Math.abs(n);
  if (abs < 1000) return String(Math.round(n));
  const units: [number, string][] = [
    [1e12, "T"],
    [1e9, "B"],
    [1e6, "M"],
    [1e3, "k"],
  ];
  for (const [v, s] of units) {
    if (abs >= v) {
      const x = n / v;
      return `${x >= 100 ? Math.round(x) : x.toFixed(1).replace(/\.0$/, "")}${s}`;
    }
  }
  return String(n);
}

export function fmtPct(ratio: number | undefined | null, digits = 1): string {
  if (ratio == null || Number.isNaN(ratio)) return NONE;
  const p = ratio * 100;
  return `${p >= 99.95 && p < 100 ? "99.9" : p.toFixed(p === 100 || p === 0 ? 0 : digits)}%`;
}

export function fmtBytes(n: number | undefined | null): string {
  if (n == null) return NONE;
  if (n < 1024) return `${n} B`;
  if (n < 1024 * 1024) return `${(n / 1024).toFixed(1)} KB`;
  return `${(n / 1024 / 1024).toFixed(1)} MB`;
}

export function fmtMs(ms: number | undefined | null): string {
  if (ms == null || ms <= 0) return NONE;
  if (ms < 1000) return `${Math.round(ms)}ms`;
  return `${(ms / 1000).toFixed(ms < 10_000 ? 2 : 1)}s`;
}

/** Seconds -> 4m12s / 1h05m / 2d3h */
export function fmtDuration(totalSeconds: number): string {
  const s = Math.max(0, Math.round(totalSeconds));
  if (s < 60) return `${s}s`;
  if (s < 3600) return `${Math.floor(s / 60)}m${String(s % 60).padStart(2, "0")}s`;
  if (s < 86400) return `${Math.floor(s / 3600)}h${String(Math.floor((s % 3600) / 60)).padStart(2, "0")}m`;
  return `${Math.floor(s / 86400)}d${Math.floor((s % 86400) / 3600)}h`;
}

/** "5m ago", "2h ago", "3d ago"; future times as "in 5m". */
export function relTime(iso: string | number | undefined | null, now = Date.now()): string {
  if (iso == null || iso === "") return NONE;
  const t = typeof iso === "number" ? iso : Date.parse(iso);
  if (Number.isNaN(t)) return NONE;
  const diff = Math.round((now - t) / 1000);
  const abs = Math.abs(diff);
  const body = abs < 5 ? "now" : fmtDuration(abs).replace(/(\d+m)\d+s$/, "$1").replace(/(\d+h)\d+m$/, "$1");
  if (body === "now") return "now";
  return diff >= 0 ? `${body} ago` : `in ${body}`;
}

/** HH:MM:SS in local time */
export function clock(iso: string | number): string {
  const d = new Date(iso);
  if (Number.isNaN(d.getTime())) return NONE;
  return d.toLocaleTimeString("en-GB", { hour12: false });
}

export function dateTime(iso: string | undefined | null): string {
  if (!iso) return NONE;
  const d = new Date(iso);
  if (Number.isNaN(d.getTime())) return NONE;
  return `${d.toLocaleDateString("en-CA")} ${d.toLocaleTimeString("en-GB", { hour12: false })}`;
}

/** sk-dev-client-aaaa1111 -> sk-dev-c...1111 */
export function maskKey(key: string): string {
  if (key.length <= 12) return `${key.slice(0, 3)}${"*".repeat(Math.max(0, key.length - 3))}`;
  return `${key.slice(0, 8)}...${key.slice(-4)}`;
}

/** Random client key: sk- plus 48 base62 characters. */
export function generateKey(): string {
  const alphabet = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789";
  const buf = new Uint8Array(48);
  crypto.getRandomValues(buf);
  return `sk-${Array.from(buf, (b) => alphabet[b % alphabet.length]).join("")}`;
}

export const cap = (s: string): string => (s ? s[0]!.toUpperCase() + s.slice(1) : s);

/** Modifier key label for shortcuts. */
export const MOD = typeof navigator !== "undefined" && /Mac|iPhone|iPad/.test(navigator.platform) ? "\u2318" : "Ctrl";

const isSemver = (v: string) => /^v?\d+\.\d+/.test(v);
/** "8.0.10" and "v8.0.10" -> "v8.0.10"; "dev" stays "dev". */
export const fmtVersion = (v: string): string => (isSemver(v) ? `v${v.replace(/^v/, "")}` : v);
export const sameVersion = (a: string, b: string): boolean => a.replace(/^v/, "") === b.replace(/^v/, "");
export { isSemver };
