import { useMutation, useQuery, useQueryClient, type QueryClient } from "@tanstack/react-query";
import { ApiError, api, isMissingEndpoint } from "./api";
import type {
  ApiKeyUsage,
  CredentialList,
  ModelInfo,
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

export function useCredentials() {
  return useQuery({
    queryKey: qk.credentials,
    queryFn: async () => (await api.get<CredentialList>("/credentials")).files,
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
  seq: number;
  events: UsageEvent[];
}

const FEED_MAX = 500;

/**
 * Recent requests, newest first. Polls with an `after` cursor and merges into the
 * cached feed. Data is null when the server lacks the extension endpoint.
 */
export function useRequestFeed(intervalMs = 3000) {
  const qc = useQueryClient();
  return useQuery({
    queryKey: qk.requests,
    refetchInterval: intervalMs,
    queryFn: async (): Promise<RequestFeed | null> => {
      const prev = qc.getQueryData<RequestFeed | null>(qk.requests);
      const res = await optional(() =>
        api.get<RequestsResponse>("/observability/requests", { limit: FEED_MAX, after: prev?.seq }),
      );
      if (!res) return null;
      if (!prev || res.seq < prev.seq) return { seq: res.seq, events: [...res.events].reverse() };
      if (res.events.length === 0) return prev;
      return { seq: res.seq, events: [...res.events].reverse().concat(prev.events).slice(0, FEED_MAX) };
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
      const res = await optional(() =>
        api.get<{ plugins: { id: string; supports_oauth?: boolean; oauth_provider?: string; metadata?: { name?: string } | null }[] }>("/plugins"),
      );
      return (res?.plugins ?? []).filter((p) => p.supports_oauth).map((p) => ({ id: p.oauth_provider || p.id, name: p.metadata?.name || p.id }));
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

export async function updateProviderGroup(group: string, edit: (entries: ProviderEntry[]) => ProviderEntry[]): Promise<void> {
  const current = (await getConfigNode<ProviderEntry[]>(`api-keys/${group}`)) ?? [];
  const next = edit(current);
  if (next.length === 0) await api.del(`/config/api-keys/${group}`).catch((e: unknown) => {
    if (!(e instanceof ApiError && e.status === 404)) throw e;
  });
  else await api.put(`/config/api-keys/${group}`, next);
}
