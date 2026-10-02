// Shapes of the management API (v8) responses this UI consumes. Fields the
// server may omit are optional. The usage extension types mirror API_EXTENSIONS.md.

export interface RecentBucket {
  time: string;
  success: number;
  failed: number;
}

export interface Cooldown {
  scope: string;
  model_key?: string;
  reason: string;
  retry_at: string;
  remaining_seconds: number;
  backoff_level?: number;
  http_status?: number;
}

export interface QuotaObservation {
  observed_at?: string;
  signals: Record<string, string>;
}

export interface CredentialFile {
  id: string;
  auth_index: string;
  name: string;
  type: string;
  provider: string;
  label?: string;
  status: string;
  status_message?: string;
  disabled: boolean;
  unavailable: boolean;
  runtime_only: boolean;
  source: "file" | "memory";
  size: number;
  success: number;
  failed: number;
  recent_requests?: RecentBucket[];
  quota?: QuotaObservation;
  model_quotas?: Record<string, QuotaObservation>;
  supports_quota?: boolean;
  email?: string;
  account?: string;
  account_type?: string;
  project_id?: string;
  created_at?: string;
  modtime?: string;
  updated_at?: string;
  last_refresh?: string;
  next_retry_after?: string;
  path?: string;
  id_token?: { plan_type?: string; chatgpt_account_id?: string };
  priority?: number;
  weight?: number;
  note?: string;
  prefix?: string;
  websockets?: boolean;
  request_retry?: number;
  cooldowns?: Cooldown[] | null;
}

export interface CredentialList {
  observed_at: string;
  files: CredentialFile[];
}

export interface ModelInfo {
  id: string;
  object?: string;
  created?: number;
  owned_by?: string;
  type?: string;
  display_name?: string;
  context_length?: number;
  max_completion_tokens?: number;
  thinking?: { min?: number; max?: number; levels?: string[]; zero_allowed?: boolean; dynamic_allowed?: boolean };
}

export interface ApiKeyUsageEntry {
  success: number;
  failed: number;
  recent_requests?: RecentBucket[];
}
/** provider -> "<base_url>|<api_key>" -> usage */
export type ApiKeyUsage = Record<string, Record<string, ApiKeyUsageEntry>>;

export interface OAuthStart {
  status: "ok";
  url: string;
  state: string;
  flow?: "device";
  user_code?: string;
  expires_in?: number;
}

export interface OAuthStatus {
  status: "ok" | "wait" | "error";
  error?: string;
}

export interface PluginInfo {
  id: string;
  supports_oauth?: boolean;
  oauth_provider?: string;
  effective_enabled?: boolean;
  metadata?: { name?: string } | null;
}

export interface LogsResponse {
  lines: string[];
  "line-count": number;
  "latest-timestamp": number;
  "next-cursor"?: string;
  "cursor-reset"?: boolean;
}

export interface ErrorLogFile {
  name: string;
  size: number;
  modified: number;
}

// Provider groups under `api-keys` in the v8 config tree.
export interface ProviderKey {
  "api-key": string;
  weight?: number | null;
  [k: string]: unknown;
}
export interface ProviderModel {
  name: string;
  alias?: string;
  [k: string]: unknown;
}
export interface ProviderEntry {
  name?: string;
  "base-url"?: string;
  prefix?: string;
  priority?: number;
  "proxy-url"?: string;
  disabled?: boolean;
  headers?: Record<string, string>;
  "excluded-models"?: string[];
  models?: ProviderModel[];
  keys?: ProviderKey[];
  [k: string]: unknown;
}
export type ProviderGroups = Partial<Record<string, ProviderEntry[]>>;

// Usage extension (additive endpoints, see API_EXTENSIONS.md).
export interface TokenTotals {
  input_tokens: number;
  output_tokens: number;
  reasoning_tokens: number;
  cached_tokens: number;
  total_tokens: number;
}
export interface UsageAgg {
  requests: number;
  failed: number;
  tokens: TokenTotals;
}
export interface UsageSummary {
  since: string;
  totals: UsageAgg;
  models: (UsageAgg & { model: string })[];
  credentials: (UsageAgg & { auth_index: string; provider?: string; source?: string })[];
  api_keys: (UsageAgg & { api_key: string })[];
  hourly: (UsageAgg & { hour: string })[];
}
export interface UsageEvent {
  seq: number;
  timestamp: string;
  latency_ms: number;
  ttft_ms?: number;
  source?: string;
  auth_index?: string;
  auth_type?: string;
  provider: string;
  model: string;
  alias?: string;
  endpoint?: string;
  api_key?: string;
  request_id?: string;
  failed: boolean;
  stream?: boolean;
  tokens: Partial<TokenTotals>;
}
export interface RequestsResponse {
  seq: number;
  capacity: number;
  events: UsageEvent[];
}
