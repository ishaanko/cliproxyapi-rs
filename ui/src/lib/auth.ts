import { useSyncExternalStore } from "react";

const STORAGE_KEY = "cpa.management-key";
const listeners = new Set<() => void>();

function emit() {
  for (const l of listeners) l();
}

export function getKey(): string | null {
  try {
    return localStorage.getItem(STORAGE_KEY);
  } catch {
    return null;
  }
}

export function setKey(key: string) {
  localStorage.setItem(STORAGE_KEY, key);
  emit();
}

export function clearKey() {
  localStorage.removeItem(STORAGE_KEY);
  emit();
}

/** Current management key, or null when signed out. */
export function useKey(): string | null {
  return useSyncExternalStore(
    (cb) => {
      listeners.add(cb);
      window.addEventListener("storage", cb);
      return () => {
        listeners.delete(cb);
        window.removeEventListener("storage", cb);
      };
    },
    getKey,
    () => null,
  );
}
