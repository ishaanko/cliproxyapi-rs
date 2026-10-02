import { useSyncExternalStore } from "react";

// Hash routing: #/<page>[/<sub>[/<more>]]. Deep links, back button and reload all work
// without any server support.
export const PAGES = ["overview", "credentials", "keys", "providers", "models", "config", "logs"] as const;
export type Page = (typeof PAGES)[number];

export interface Route {
  page: Page;
  /** Remaining path segments, decoded. */
  sub: string[];
}

function parse(hash: string): Route {
  const parts = hash.replace(/^#\/?/, "").split("/").filter(Boolean).map(decodeURIComponent);
  const first = parts[0];
  const page = PAGES.find((p) => p === first) ?? "overview";
  return { page, sub: parts.slice(1) };
}

const cache = { hash: "\0", route: parse("") };
function snapshot(): Route {
  const h = window.location.hash;
  if (h !== cache.hash) {
    cache.hash = h;
    cache.route = parse(h);
  }
  return cache.route;
}

export function useRoute(): Route {
  return useSyncExternalStore(
    (cb) => {
      window.addEventListener("hashchange", cb);
      return () => window.removeEventListener("hashchange", cb);
    },
    snapshot,
    () => cache.route,
  );
}

export function href(page: Page, ...sub: string[]): string {
  return `#/${[page, ...sub.map(encodeURIComponent)].join("/")}`;
}

export function go(page: Page, ...sub: string[]) {
  window.location.hash = href(page, ...sub);
}
