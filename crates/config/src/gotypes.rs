//! Go struct names of the legacy config tree, for yaml.v3's strict-decode messages
//! (`field X not found in type config.RoutingConfig`). Generated from the reference with reflection
//! over `legacyConfig`: per struct, the YAML keys whose value holds a struct (directly, or inside
//! a list `[]` / map `{}`). Leaves and types with a custom `UnmarshalYAML` are left out (yaml.v3
//! decodes those without strict field checks).

/// `(struct type, [(yaml key, shape)])`; a shape is zero or more `[]` / `{}` wrappers and a struct
/// type name.
pub(crate) static GO_TYPES: &[(&str, &[(&str, &str)])] = &[
    ("config.AntigravityConfig", &[
        ("connection-pool", "config.AntigravityConnectionPoolConfig"),
    ]),
    ("config.AntigravityConnectionPoolConfig", &[
    ]),
    ("config.ClaudeCodeConfig", &[
    ]),
    ("config.ClaudeConfig", &[
    ]),
    ("config.ClaudeHeaderDefaults", &[
    ]),
    ("config.ClaudeKey", &[
        ("models", "[]config.ClaudeModel"),
        ("request-scoped-errors", "[]config.RequestScopedErrorRule"),
        ("cloak", "config.CloakConfig"),
    ]),
    ("config.ClaudeModel", &[
        ("thinking", "registry.ThinkingSupport"),
    ]),
    ("config.ClientConfig", &[
        ("codex", "config.CodexClientConfig"),
    ]),
    ("config.CloakConfig", &[
    ]),
    ("config.CodexClientConfig", &[
    ]),
    ("config.CodexConfig", &[
    ]),
    ("config.CodexHeaderDefaults", &[
    ]),
    ("config.CodexKey", &[
        ("models", "[]config.CodexModel"),
        ("request-scoped-errors", "[]config.RequestScopedErrorRule"),
    ]),
    ("config.CodexModel", &[
        ("thinking", "registry.ThinkingSupport"),
    ]),
    ("config.CredentialInFlightConfig", &[
    ]),
    ("config.DevinConfig", &[
    ]),
    ("config.DiscoveryConfig", &[
        ("interfaces", "config.DiscoveryInterfacesConfig"),
    ]),
    ("config.DiscoveryInterfacesConfig", &[
    ]),
    ("config.GeminiKey", &[
        ("models", "[]config.GeminiModel"),
        ("request-scoped-errors", "[]config.RequestScopedErrorRule"),
    ]),
    ("config.GeminiModel", &[
        ("thinking", "registry.ThinkingSupport"),
    ]),
    ("config.OAuthModelAlias", &[
    ]),
    ("config.OAuthModelSetting", &[
    ]),
    ("config.OpenAICompatibility", &[
        ("api-key-entries", "[]config.OpenAICompatibilityAPIKey"),
        ("models", "[]config.OpenAICompatibilityModel"),
        ("request-scoped-errors", "[]config.RequestScopedErrorRule"),
    ]),
    ("config.OpenAICompatibilityAPIKey", &[
    ]),
    ("config.OpenAICompatibilityModel", &[
        ("thinking", "registry.ThinkingSupport"),
    ]),
    ("config.PayloadConfig", &[
        ("default", "[]config.PayloadRule"),
        ("default-raw", "[]config.PayloadRule"),
        ("override", "[]config.PayloadRule"),
        ("override-raw", "[]config.PayloadRule"),
        ("filter", "[]config.PayloadFilterRule"),
    ]),
    ("config.PayloadFilterRule", &[
        ("models", "[]config.PayloadModelRule"),
    ]),
    ("config.PayloadModelRule", &[
    ]),
    ("config.PayloadRule", &[
        ("models", "[]config.PayloadModelRule"),
    ]),
    ("config.PluginsConfig", &[
        ("store-auth", "[]pluginstore.AuthConfig"),
    ]),
    ("config.PprofConfig", &[
    ]),
    ("config.QuotaExceeded", &[
    ]),
    ("config.RemoteManagement", &[
    ]),
    ("config.RequestScopedErrorRule", &[
    ]),
    ("config.RoutingConfig", &[
    ]),
    ("config.StreamingConfig", &[
    ]),
    ("config.TLSConfig", &[
    ]),
    ("config.VertexCompatKey", &[
        ("models", "[]config.VertexCompatModel"),
    ]),
    ("config.VertexCompatModel", &[
        ("thinking", "registry.ThinkingSupport"),
    ]),
    ("config.XAIConfig", &[
    ]),
    ("config.legacyConfig", &[
        ("client", "config.ClientConfig"),
        ("claude-code", "config.ClaudeCodeConfig"),
        ("streaming", "config.StreamingConfig"),
        ("tls", "config.TLSConfig"),
        ("credential-in-flight", "config.CredentialInFlightConfig"),
        ("remote-management", "config.RemoteManagement"),
        ("plugins", "config.PluginsConfig"),
        ("pprof", "config.PprofConfig"),
        ("discovery", "config.DiscoveryConfig"),
        ("quota-exceeded", "config.QuotaExceeded"),
        ("routing", "config.RoutingConfig"),
        ("antigravity", "config.AntigravityConfig"),
        ("devin", "config.DevinConfig"),
        ("gemini-api-key", "[]config.GeminiKey"),
        ("interactions-api-key", "[]config.GeminiKey"),
        ("codex-api-key", "[]config.CodexKey"),
        ("xai-api-key", "[]config.CodexKey"),
        ("meta-api-key", "[]config.CodexKey"),
        ("xai", "config.XAIConfig"),
        ("codex", "config.CodexConfig"),
        ("codex-header-defaults", "config.CodexHeaderDefaults"),
        ("claude", "config.ClaudeConfig"),
        ("claude-api-key", "[]config.ClaudeKey"),
        ("claude-header-defaults", "config.ClaudeHeaderDefaults"),
        ("openai-compatibility", "[]config.OpenAICompatibility"),
        ("vertex-api-key", "[]config.VertexCompatKey"),
        ("oauth-model-alias", "{}[]config.OAuthModelAlias"),
        ("oauth-request-scoped-errors", "{}[]config.RequestScopedErrorRule"),
        ("oauth-settings", "{}[]config.OAuthModelSetting"),
        ("payload", "config.PayloadConfig"),
    ]),
    ("pluginstore.AuthConfig", &[
    ]),
    ("registry.ThinkingSupport", &[
    ]),
];
