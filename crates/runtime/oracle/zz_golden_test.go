// Go side of the service differential test (see tests/service_golden.rs and gen_scenarios.py).
//
// It must live inside the reference module to reach the internal packages. Copy it to
// <CLIProxyAPI>/sdk/cliproxy/zz_golden_test.go and run from that checkout:
//
//	ZZ_IN=<repo>/crates/runtime/tests/fixtures/service_scenarios.json \
//	ZZ_OUT=<repo>/crates/runtime/tests/fixtures/service_golden.json \
//	go test ./sdk/cliproxy -run TestZZGolden -count=1
//
// For every scenario it loads the config, synthesizes config + file auths with a fixed fake auth
// dir, registers each auth's models in a clean global registry and dumps the auths, the per-auth
// models and the registry list payloads.
package cliproxy

import (
	"context"
	"encoding/json"
	"net/http"
	"net/http/httptest"
	"os"
	"path/filepath"
	"sort"
	"strings"
	"testing"
	"time"

	"github.com/gin-gonic/gin"
	"github.com/router-for-me/CLIProxyAPI/v8/internal/config"
	"github.com/router-for-me/CLIProxyAPI/v8/internal/registry"
	"github.com/router-for-me/CLIProxyAPI/v8/internal/watcher/synthesizer"
	"github.com/router-for-me/CLIProxyAPI/v8/sdk/api/handlers"
	"github.com/router-for-me/CLIProxyAPI/v8/sdk/api/handlers/claude"
	"github.com/router-for-me/CLIProxyAPI/v8/sdk/api/handlers/gemini"
	"github.com/router-for-me/CLIProxyAPI/v8/sdk/api/handlers/openai"
	coreauth "github.com/router-for-me/CLIProxyAPI/v8/sdk/cliproxy/auth"
)

const zzAuthDir = "/golden/auths"

type zzScenario struct {
	Name       string            `json:"name"`
	ConfigYAML string            `json:"config_yaml"`
	AuthFiles  map[string]string `json:"auth_files"`
	// DumpHandlers also records the /v1/models and /v1beta/models response bodies.
	DumpHandlers bool `json:"dump_handlers"`
}

// zzHandlerBodies serves the real model-list handlers over the current global registry.
func zzHandlerBodies() map[string]any {
	gin.SetMode(gin.TestMode)
	base := &handlers.BaseAPIHandler{}
	oa, cl, ge := openai.NewOpenAIAPIHandler(base), claude.NewClaudeCodeAPIHandler(base), gemini.NewGeminiAPIHandler(base)
	r := gin.New()
	r.GET("/openai", oa.OpenAIModels)
	r.GET("/claude", cl.ClaudeModels)
	r.GET("/gemini", ge.GeminiModels)
	r.GET("/gemini/*action", ge.GeminiGetHandler)
	get := func(path string) map[string]any {
		w := httptest.NewRecorder()
		r.ServeHTTP(w, httptest.NewRequest(http.MethodGet, path, nil))
		return map[string]any{"status": w.Code, "body": w.Body.String()}
	}
	out := map[string]any{"openai": get("/openai"), "claude": get("/claude"), "gemini": get("/gemini")}
	var list struct {
		Models []struct {
			Name string `json:"name"`
		} `json:"models"`
	}
	if body, ok := out["gemini"].(map[string]any)["body"].(string); ok && json.Unmarshal([]byte(body), &list) == nil && len(list.Models) > 0 {
		name := list.Models[0].Name
		out["gemini_name"] = name
		out["gemini_get_prefixed"] = get("/gemini/" + name)
		out["gemini_get_bare"] = get("/gemini/" + strings.TrimPrefix(name, "models/"))
	}
	out["gemini_get_missing"] = get("/gemini/models/no-such-model")
	return out
}

func zzAuth(a *coreauth.Auth) map[string]any {
	return map[string]any{
		"id":          a.ID,
		"provider":    a.Provider,
		"label":       a.Label,
		"prefix":      a.Prefix,
		"status":      string(a.Status),
		"disabled":    a.Disabled,
		"proxy_url":   a.ProxyURL,
		"file_name":   a.FileName,
		"attributes":  a.Attributes,
		"metadata":    a.Metadata,
		"index":       a.EnsureIndex(),
		"auth_kind":   a.AuthKind(),
		"source_kind": a.AuthSourceKind(),
	}
}

func zzModel(m *registry.ModelInfo, now int64) map[string]any {
	created := any(m.Created)
	if d := m.Created - now; d < 60 && d > -60 {
		created = "NOW"
	}
	out := map[string]any{
		"id":                          m.ID,
		"metadata_model_id":           m.MetadataModelID,
		"explicit_thinking":           m.ExplicitThinking,
		"explicit_input_modalities":   m.ExplicitInputModalities,
		"object":                      m.Object,
		"created":                     created,
		"owned_by":                    m.OwnedBy,
		"type":                        m.Type,
		"display_name":                m.DisplayName,
		"name":                        m.Name,
		"version":                     m.Version,
		"description":                 m.Description,
		"input_token_limit":           m.InputTokenLimit,
		"output_token_limit":          m.OutputTokenLimit,
		"supported_generation_methods": m.SupportedGenerationMethods,
		"context_length":              m.ContextLength,
		"max_context_length":          m.MaxContextLength,
		"max_completion_tokens":       m.MaxCompletionTokens,
		"supported_parameters":        m.SupportedParameters,
		"supported_input_modalities":  m.SupportedInputModalities,
		"supported_output_modalities": m.SupportedOutputModalities,
		"supports_web_search":         m.SupportsWebSearch,
		"support_configuration_update": m.SupportConfigurationUpdate,
		"thinking":                    m.Thinking,
		"user_defined":                m.UserDefined,
		"is_compat":                   m.IsCompat,
	}
	if m.NativeCapabilities != nil && m.NativeCapabilities.WebSearch != nil {
		out["native_web_search"] = *m.NativeCapabilities.WebSearch
	}
	return out
}

func TestZZGolden(t *testing.T) {
	in, out := os.Getenv("ZZ_IN"), os.Getenv("ZZ_OUT")
	if in == "" || out == "" {
		t.Skip("ZZ_IN / ZZ_OUT not set")
	}
	raw, err := os.ReadFile(in)
	if err != nil {
		t.Fatal(err)
	}
	var scenarios []zzScenario
	if err := json.Unmarshal(raw, &scenarios); err != nil {
		t.Fatal(err)
	}
	tmp := t.TempDir()
	results := make([]any, 0, len(scenarios))
	for _, sc := range scenarios {
		cfgPath := filepath.Join(tmp, sc.Name+".yaml")
		if err := os.WriteFile(cfgPath, []byte(sc.ConfigYAML), 0o600); err != nil {
			t.Fatal(err)
		}
		cfg, err := config.LoadConfig(cfgPath)
		if err != nil {
			t.Fatalf("%s: load config: %v", sc.Name, err)
		}
		cfg.AuthDir = zzAuthDir
		now := time.Now()
		ctx := &synthesizer.SynthesisContext{Config: cfg, AuthDir: zzAuthDir, Now: now, IDGenerator: synthesizer.NewStableIDGenerator()}

		var auths []*coreauth.Auth
		configAuths, errCfg := synthesizer.NewConfigSynthesizer().Synthesize(ctx)
		if errCfg != nil {
			t.Fatalf("%s: synthesize: %v", sc.Name, errCfg)
		}
		auths = append(auths, configAuths...)
		names := make([]string, 0, len(sc.AuthFiles))
		for name := range sc.AuthFiles {
			names = append(names, name)
		}
		sort.Strings(names)
		fileErrors := map[string]string{}
		for _, name := range names {
			if !strings.HasSuffix(strings.ToLower(name), ".json") || sc.AuthFiles[name] == "" {
				continue
			}
			generated, errFile := synthesizer.SynthesizeAuthFile(ctx, filepath.Join(zzAuthDir, name), []byte(sc.AuthFiles[name]))
			if errFile != nil {
				fileErrors[name] = errFile.Error()
				continue
			}
			auths = append(auths, generated...)
		}

		svc := &Service{cfg: cfg}
		reg := registry.GetGlobalRegistry()
		for _, a := range auths {
			svc.registerModelsForAuth(context.Background(), a)
		}
		nowUnix := time.Now().Unix()
		dumped := make([]any, 0, len(auths))
		providers := map[string][]string{}
		for _, a := range auths {
			// Full model info only where config, prefix, alias or exclusion rules reshape the
			// catalog; plain catalog registrations are compared by id (keeps the fixture small).
			full := strings.HasPrefix(a.Attributes["source"], "config:") || a.Prefix != "" ||
				a.Attributes["model_aliases"] != "" || a.Attributes["excluded_models"] != ""
			models := []any{}
			for _, m := range reg.GetModelsForClient(a.ID) {
				if full {
					models = append(models, zzModel(m, nowUnix))
				} else {
					models = append(models, m.ID)
				}
				if _, ok := providers[m.ID]; !ok {
					providers[m.ID] = reg.GetModelProviders(m.ID)
				}
			}
			entry := zzAuth(a)
			entry["models"] = models
			dumped = append(dumped, entry)
		}
		// Registry list payloads, reduced to the id (or Gemini name) of each entry.
		lists := map[string]any{}
		for _, h := range []string{"openai", "claude", "gemini"} {
			ids := []string{}
			for _, m := range reg.GetAvailableModels(h) {
				if id, ok := m["id"].(string); ok {
					ids = append(ids, id)
				} else if name, ok := m["name"].(string); ok {
					ids = append(ids, name)
				}
			}
			lists[h] = ids
		}
		var handlerBodies map[string]any
		if sc.DumpHandlers {
			handlerBodies = zzHandlerBodies()
		}
		for _, a := range auths {
			reg.UnregisterClient(a.ID)
		}
		results = append(results, map[string]any{
			"handlers":    handlerBodies,
			"name":        sc.Name,
			"auths":       dumped,
			"file_errors": fileErrors,
			"providers":   providers,
			"lists":       lists,
		})
	}

	gen := synthesizer.NewStableIDGenerator()
	var ids [][2]string
	for _, parts := range [][]string{{"a", "b"}, {"a", "b"}, {" a ", "b"}, {"a", "b", "c"}, {"a", "b"}} {
		id, token := gen.Next("k", parts...)
		ids = append(ids, [2]string{id, token})
	}
	body, err := json.Marshal(map[string]any{"scenarios": results, "id_gen": ids})
	if err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(out, body, 0o644); err != nil {
		t.Fatal(err)
	}
}
