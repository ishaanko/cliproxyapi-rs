import { cap } from "./format";
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
