import { useSyncExternalStore } from "react";

export interface Toast {
  id: number;
  kind: "ok" | "error";
  text: string;
}

let toasts: Toast[] = [];
let nextId = 1;
const listeners = new Set<() => void>();

function set(next: Toast[]) {
  toasts = next;
  for (const l of listeners) l();
}

function push(kind: Toast["kind"], text: string) {
  const id = nextId++;
  set([...toasts.slice(-3), { id, kind, text }]);
  setTimeout(() => set(toasts.filter((t) => t.id !== id)), kind === "error" ? 6000 : 2600);
}

export const toast = {
  ok: (text: string) => push("ok", text),
  error: (text: string) => push("error", text),
};

export function useToasts(): Toast[] {
  return useSyncExternalStore(
    (cb) => {
      listeners.add(cb);
      return () => listeners.delete(cb);
    },
    () => toasts,
  );
}

export function errorText(e: unknown): string {
  return e instanceof Error ? e.message : String(e);
}
