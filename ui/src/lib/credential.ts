import { cap, fmtDuration } from "./format";
import type { Cooldown, CredentialFile } from "./types";
import type { Tone } from "@/ui/primitives";

export function credentialTitle(f: CredentialFile): string {
  return f.label || f.email || f.account || f.name;
}

export function planOf(f: CredentialFile): string | undefined {
  return f.id_token?.plan_type || undefined;
}

/** Seconds until a cooldown ends, from its absolute retry time. */
export function cooldownRemaining(c: Cooldown, now = Date.now()): number {
  const at = Date.parse(c.retry_at);
  return Number.isNaN(at) ? c.remaining_seconds : Math.max(0, Math.round((at - now) / 1000));
}

export function activeCooldowns(f: CredentialFile, now = Date.now()): Cooldown[] {
  return (f.cooldowns ?? []).filter((c) => cooldownRemaining(c, now) > 0);
}

export function credentialState(f: CredentialFile): { tone: Tone; label: string } {
  if (f.disabled || f.status === "disabled") return { tone: "off", label: "Disabled" };
  // The server reports cooldowns as unavailable with status "error", so check them first.
  if (activeCooldowns(f).length > 0 || f.unavailable) return { tone: "warn", label: "Cooldown" };
  if (f.status === "error") return { tone: "bad", label: "Error" };
  if (f.status === "active" || f.status === "") return { tone: "ok", label: "Active" };
  return { tone: "warn", label: cap(f.status) };
}

/** One-line quota or cooldown summary for list rows, or null when there is nothing to say. */
export function quotaSummary(f: CredentialFile): { text: string; tone: "warn" | "muted" } | null {
  const cds = activeCooldowns(f);
  if (cds.length > 0) {
    const soonest = Math.min(...cds.map((c) => cooldownRemaining(c)));
    const status = cds.find((c) => c.http_status)?.http_status;
    return { text: `cooldown ${fmtDuration(soonest)}${status ? ` (${status})` : ""}`, tone: "warn" };
  }
  const signals = Object.entries(f.quota?.signals ?? {});
  if (signals.length > 0) return { text: signals.slice(0, 2).map(([k, v]) => `${k} ${v}`).join("  "), tone: "muted" };
  return null;
}
