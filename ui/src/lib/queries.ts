import { useMutation, useQuery, useQueryClient, type QueryClient } from "@tanstack/react-query";
import { ApiError, api, isMissingEndpoint } from "./api";
import type {
  ApiKeyUsage,
  CredentialFile,
  CredentialList,
  ModelInfo,
  PluginList,
  ProviderEntry,
  ProviderGroups,
  RequestsResponse,
  UsageEvent,
  UsageSummary,
} from "./types";

export const qk = {
  credentials: ["credentials"] as const,
  credentialModels: (name: string) => ["credential-models", name] as const,
  apiKeyUsage: ["api-key-usage"] as const,
  summary: ["usage-summary"] as const,
  requests: ["requests"] as const,
  clientKeys: ["client-keys"] as const,
  providers: ["providers"] as const,
  served: ["served-models"] as const,
  channel: (c: string) => ["channel-models", c] as const,
  latest: ["latest-version"] as const,
  plugins: ["plugins"] as const,
  health: ["health"] as const,
};

/** GET a node of the v8 config tree. A missing node reads as undefined. */
export async function getConfigNode<T>(path: string): Promise<T | undefined> {
  try {
    return await api.get<T>(`/config/${path}`);
  } catch (e) {
    if (e instanceof ApiError && e.status === 404) return undefined;
    throw e;
  }
}

/** Optional endpoint: null when this server does not implement it. */
async function optional<T>(run: () => Promise<T>): Promise<T | null> {
  try {
    return await run();
  } catch (e) {
    if (isMissingEndpoint(e)) return null;
    throw e;
  }
}

/** The server lists disk-only files without id, provider or counters; fill the gaps. */
function normalizeCredential(f: CredentialFile): CredentialFile {
  return {
    ...f,
    id: f.id || f.name,
    auth_index: f.auth_index ?? "",
    provider: f.provider || f.type || "unknown",
    status: f.status ?? "",
    disabled: f.disabled ?? false,
    unavailable: f.unavailable ?? false,
    runtime_only: f.runtime_only ?? false,
    source: f.source ?? "file",
    success: f.success ?? 0,
    failed: f.failed ?? 0,
  };
}

export function useCredentials() {
  return useQuery({
    queryKey: qk.credentials,
    queryFn: async () => (await api.get<CredentialList>("/credentials")).files.map(normalizeCredential),
    refetchInterval: 10_000,
  });
}

export function useApiKeyUsage() {
  return useQuery({
    queryKey: qk.apiKeyUsage,
    queryFn: () => api.get<ApiKeyUsage>("/observability/usage/api-keys"),
    refetchInterval: 15_000,
  });
}

export function useUsageSummary() {
  return useQuery({
    queryKey: qk.summary,
    queryFn: () => optional(() => api.get<UsageSummary>("/observability/usage/summary")),
    refetchInterval: 6_000,
  });
}

export interface RequestFeed {
  /** Cursor: the last event seq received. */
  seq: number;
  startedAt: string;
  events: UsageEvent[];
}

const FEED_MAX = 500;
const FEED_PAGES = 5;

/**
 * Recent requests, newest first. Pages through `after` using the last returned seq as
 * the cursor, so a burst larger than one page loses nothing. A new `started_at` or a
 * lower `seq` means the server restarted and the buffer is rebuilt. Data is null when
 * the server lacks the extension endpoint.
 */
export function useRequestFeed(intervalMs = 3000) {
  const qc = useQueryClient();
  return useQuery({
    queryKey: qk.requests,
    refetchInterval: intervalMs,
    queryFn: async (): Promise<RequestFeed | null> => {
      let feed = qc.getQueryData<RequestFeed | null>(qk.requests) ?? null;
      for (let page = 0; page < FEED_PAGES; page++) {
        const res = await optional(() =>
          api.get<RequestsResponse>("/observability/requests", { limit: FEED_MAX, after: feed?.seq }),
        );
        if (!res) return null;
        const reset = feed === null || res.started_at !== feed.startedAt || res.seq < feed.seq;
        const prevEvents = reset || feed === null ? [] : feed.events;
        const prevSeq = reset || feed === null ? res.seq : feed.seq;
        const last = res.events[res.events.length - 1];
        feed = {
          seq: last ? last.seq : prevSeq,
          startedAt: res.started_at,
          events: [...res.events].reverse().concat(prevEvents).slice(0, FEED_MAX),
        };
        if (!res.has_more) break;
      }
      return feed;
    },
  });
}

export function useClientKeys() {
  return useQuery({
    queryKey: qk.clientKeys,
    queryFn: async () => (await getConfigNode<string[]>("access/api-keys")) ?? [],
  });
}

export function useProviders() {
  return useQuery({
    queryKey: qk.providers,
    queryFn: async () => (await getConfigNode<ProviderGroups>("api-keys")) ?? {},
  });
}

/** Models as clients see them. Needs a client key, so the first configured one is used. */
export function useServedModels() {
  const keys = useClientKeys();
  const first = keys.data?.[0];
  return useQuery({
    queryKey: [...qk.served, first],
    enabled: first !== undefined,
    queryFn: async (): Promise<ModelInfo[]> => {
      const res = await fetch("/v1/models", { headers: { Authorization: `Bearer ${first}` }, cache: "no-store" });
      if (!res.ok) throw new ApiError(res.status, `GET /v1/models failed (${res.status})`);
      const body = (await res.json()) as { data?: ModelInfo[] };
      return body.data ?? [];
    },
  });
}

export function useChannelModels(channel: string | null) {
  return useQuery({
    queryKey: qk.channel(channel ?? ""),
    enabled: channel !== null,
    staleTime: 5 * 60_000,
    queryFn: async () => (await api.get<{ models: ModelInfo[] }>(`/routing/model-definitions/${channel}`)).models,
  });
}

export function useCredentialModels(name: string | null) {
  return useQuery({
    queryKey: qk.credentialModels(name ?? ""),
    enabled: name !== null,
    queryFn: async () => (await api.get<{ models: ModelInfo[] }>("/credentials/models", { name: name ?? "" })).models,
  });
}

export function useLatestVersion() {
  return useQuery({
    queryKey: qk.latest,
    staleTime: 30 * 60_000,
    retry: false,
    queryFn: async () => (await api.get<{ "latest-version": string }>("/server/latest-version"))["latest-version"],
  });
}

export function useHealth() {
  return useQuery({
    queryKey: qk.health,
    refetchInterval: 10_000,
    retry: false,
    queryFn: async () => {
      const res = await fetch("/healthz", { cache: "no-store" });
      return res.ok;
    },
  });
}

export function useOAuthPlugins() {
  return useQuery({
    queryKey: qk.plugins,
    staleTime: 60_000,
    queryFn: async () => {
      const res = await optional(() => api.get<PluginList>("/plugins"));
      return (res?.plugins ?? [])
        .filter((p) => p.supports_oauth && p.effective_enabled)
        .map((p) => ({ id: p.oauth_provider || p.id, name: p.metadata?.name || p.id }));
    },
  });
}

/** Mutation that invalidates the given query keys on success. */
export function useInvalidating<TVars, TData = unknown>(
  mutationFn: (v: TVars) => Promise<TData>,
  keys: readonly (readonly unknown[])[],
) {
  const qc = useQueryClient();
  return useMutation({
    mutationFn,
    onSuccess: () => invalidate(qc, keys),
  });
}

export function invalidate(qc: QueryClient, keys: readonly (readonly unknown[])[]) {
  return Promise.all(keys.map((queryKey) => qc.invalidateQueries({ queryKey })));
}

// Read-modify-write helpers for list-valued config nodes. They re-read the server
// state first so concurrent edits elsewhere are not clobbered.
export async function updateClientKeys(edit: (keys: string[]) => string[]): Promise<string[]> {
  const current = (await getConfigNode<string[]>("access/api-keys")) ?? [];
  const next = edit(current);
  await api.put("/config/access/api-keys", next);
  return next;
}

/**
 * Entries have no stable id, so index-based edits pass `expect` (index and the entry as
 * the UI last saw it). If the server list moved underneath, the write is refused.
 */
export async function updateProviderGroup(
  group: string,
  edit: (entries: ProviderEntry[]) => ProviderEntry[],
  expect?: { index: number; entry: ProviderEntry },
): Promise<void> {
  const current = (await getConfigNode<ProviderEntry[]>(`api-keys/${group}`)) ?? [];
  if (expect && JSON.stringify(current[expect.index]) !== JSON.stringify(expect.entry)) {
    throw new Error("This provider changed on the server. Reload and try again.");
  }
  const next = edit(current);
  if (next.length === 0) await api.del(`/config/api-keys/${group}`).catch((e: unknown) => {
    if (!(e instanceof ApiError && e.status === 404)) throw e;
  });
  else await api.put(`/config/api-keys/${group}`, next);
}
