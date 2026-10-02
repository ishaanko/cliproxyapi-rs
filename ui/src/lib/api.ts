import { useSyncExternalStore } from "react";
import { clearKey, getKey } from "./auth";

const BASE = "/v8/management";

export class ApiError extends Error {
  readonly status: number;
  readonly code?: string;
  constructor(status: number, message: string, code?: string) {
    super(message);
    this.name = "ApiError";
    this.status = status;
    this.code = code;
  }
}

/** True for the statuses a server without an optional endpoint answers with. */
export function isMissingEndpoint(e: unknown): boolean {
  return e instanceof ApiError && (e.status === 404 || e.status === 405 || e.status === 501);
}

// Server build info taken from X-CPA-* headers on every management response.
export interface ServerMeta {
  version: string | null;
  commit: string | null;
  buildDate: string | null;
}
let meta: ServerMeta = { version: null, commit: null, buildDate: null };
const metaListeners = new Set<() => void>();

function captureMeta(res: Response) {
  const version = res.headers.get("x-cpa-version");
  if (!version || version === meta.version) return;
  meta = { version, commit: res.headers.get("x-cpa-commit"), buildDate: res.headers.get("x-cpa-build-date") };
  for (const l of metaListeners) l();
}

export function useServerMeta(): ServerMeta {
  return useSyncExternalStore(
    (cb) => {
      metaListeners.add(cb);
      return () => metaListeners.delete(cb);
    },
    () => meta,
  );
}

type Query = Record<string, string | number | boolean | undefined>;

function url(path: string, query?: Query): string {
  const u = new URL(BASE + path, window.location.origin);
  for (const [k, v] of Object.entries(query ?? {})) if (v !== undefined) u.searchParams.set(k, String(v));
  return u.pathname + u.search;
}

async function errorFrom(res: Response): Promise<ApiError> {
  const text = await res.text().catch(() => "");
  let message = res.statusText || `HTTP ${res.status}`;
  let code: string | undefined;
  try {
    const body: unknown = JSON.parse(text);
    if (body && typeof body === "object") {
      const o = body as Record<string, unknown>;
      if (typeof o.message === "string" && o.message) message = o.message;
      else if (typeof o.error === "string") message = o.error;
      if (typeof o.error === "string") code = o.error;
    }
  } catch {
    if (text) message = text.slice(0, 300);
  }
  return new ApiError(res.status, message, code);
}

async function send(path: string, init: RequestInit & { query?: Query } = {}): Promise<Response> {
  const { query, headers, ...rest } = init;
  const h = new Headers(headers);
  const key = getKey();
  if (key) h.set("Authorization", `Bearer ${key}`);
  let res: Response;
  try {
    res = await fetch(url(path, query), { ...rest, headers: h, cache: "no-store" });
  } catch {
    throw new ApiError(0, "Server unreachable");
  }
  captureMeta(res);
  if (res.status === 401) {
    clearKey();
    throw new ApiError(401, "Invalid management key");
  }
  if (!res.ok) throw await errorFrom(res);
  return res;
}

const jsonInit = (method: string, body: unknown, query?: Query): RequestInit & { query?: Query } => ({
  method,
  query,
  headers: { "Content-Type": "application/json" },
  body: body === undefined ? undefined : JSON.stringify(body),
});

export const api = {
  async get<T>(path: string, query?: Query): Promise<T> {
    return (await send(path, { query })).json() as Promise<T>;
  },
  async getText(path: string, query?: Query): Promise<string> {
    return (await send(path, { query })).text();
  },
  async getBlob(path: string, query?: Query): Promise<Blob> {
    return (await send(path, { query })).blob();
  },
  async put<T = unknown>(path: string, body: unknown, query?: Query): Promise<T> {
    return (await send(path, jsonInit("PUT", body, query))).json() as Promise<T>;
  },
  async patch<T = unknown>(path: string, body: unknown, query?: Query): Promise<T> {
    return (await send(path, jsonInit("PATCH", body, query))).json() as Promise<T>;
  },
  async post<T = unknown>(path: string, body?: unknown, query?: Query): Promise<T> {
    return (await send(path, jsonInit("POST", body, query))).json() as Promise<T>;
  },
  async del<T = unknown>(path: string, query?: Query): Promise<T> {
    return (await send(path, { method: "DELETE", query })).json() as Promise<T>;
  },
  async putYaml(path: string, yaml: string): Promise<unknown> {
    return (await send(path, { method: "PUT", headers: { "Content-Type": "application/yaml" }, body: yaml })).json();
  },
  async postForm<T = unknown>(path: string, form: FormData, query?: Query): Promise<T> {
    return (await send(path, { method: "POST", body: form, query })).json() as Promise<T>;
  },
};

/** Download a blob under a file name via a temporary anchor. */
export function saveBlob(blob: Blob, filename: string) {
  const a = document.createElement("a");
  a.href = URL.createObjectURL(blob);
  a.download = filename;
  document.body.append(a);
  a.click();
  a.remove();
  setTimeout(() => URL.revokeObjectURL(a.href), 2000);
}

/** Verify a candidate key without storing it. Throws ApiError with a user-facing message. */
export async function verifyKey(key: string): Promise<void> {
  let res: Response;
  try {
    res = await fetch(url("/credentials", { page: 1, page_size: 1 }), { headers: { Authorization: `Bearer ${key}` }, cache: "no-store" });
  } catch {
    throw new ApiError(0, "Server unreachable");
  }
  captureMeta(res);
  if (res.ok) return;
  if (res.status === 401) throw new ApiError(401, "Invalid management key");
  if (res.status === 403) {
    const e = await errorFrom(res);
    throw new ApiError(403, e.message && e.message !== "Forbidden" ? e.message : "Management access denied", e.code);
  }
  if (res.status === 404) throw new ApiError(404, "Management API is not enabled");
  throw await errorFrom(res);
}
