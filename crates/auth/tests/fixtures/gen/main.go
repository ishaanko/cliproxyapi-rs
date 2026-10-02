// Fixture generator: writes credential files with the real Go token stores so the Rust crate can
// assert byte-for-byte compatibility (tests/go_fixtures.rs). Not built by cargo.
//
// Regenerate (from the repo root; needs the Go reference checkout at tmp/CLIProxyAPI):
//
//	REF=$PWD/tmp/CLIProxyAPI
//	printf '{"Replace":{"%s/cmd/authfixtures/main.go":"%s"}}' "$REF" "$PWD/crates/auth/tests/fixtures/gen/main.go" > /tmp/overlay.json
//	(cd $REF && GOPATH=$PWD/../gopath GOFLAGS=-mod=mod ../go-toolchain/bin/go run -overlay /tmp/overlay.json ./cmd/authfixtures $PWD/../../crates/auth/tests/fixtures/go $PWD/../../crates/auth/tests/fixtures/derived_cases.json $PWD/../../crates/auth/tests/fixtures/go_misc.json)
//
// The program is compiled inside the reference module through a go build overlay, so nothing is
// written into the reference checkout.
package main

import (
	"context"
	"crypto/rand"
	"crypto/rsa"
	"crypto/x509"
	"encoding/base64"
	"encoding/json"
	"encoding/pem"
	"fmt"
	"os"
	"strings"

	"github.com/router-for-me/CLIProxyAPI/v8/internal/auth/antigravity"
	"github.com/router-for-me/CLIProxyAPI/v8/internal/auth/claude"
	"github.com/router-for-me/CLIProxyAPI/v8/internal/auth/codex"
	"github.com/router-for-me/CLIProxyAPI/v8/internal/auth/devin"
	"github.com/router-for-me/CLIProxyAPI/v8/internal/auth/kimi"
	"github.com/router-for-me/CLIProxyAPI/v8/internal/auth/meta"
	"github.com/router-for-me/CLIProxyAPI/v8/internal/auth/vertex"
	"github.com/router-for-me/CLIProxyAPI/v8/internal/auth/xai"
	sdkauth "github.com/router-for-me/CLIProxyAPI/v8/sdk/auth"
	coreauth "github.com/router-for-me/CLIProxyAPI/v8/sdk/cliproxy/auth"
)

type derivedCase struct {
	Name        string            `json:"name"`
	Provider    string            `json:"provider"`
	ID          string            `json:"id"`
	FileName    string            `json:"file_name"`
	Attributes  map[string]string `json:"attributes"`
	Metadata    map[string]any    `json:"metadata"`
	Index       string            `json:"index"`
	ExpiresUnix *int64            `json:"expires_unix"`
	AuthKind    string            `json:"auth_kind"`
	SourceKind  string            `json:"source_kind"`
}

func jwt(claims string) string {
	enc := base64.RawURLEncoding.EncodeToString
	return enc([]byte(`{"alg":"none"}`)) + "." + enc([]byte(claims)) + ".sig"
}

// writeDerivedCases records the Go values of the derived fields (stable index, expiry, kinds) for a
// spread of inputs, as JSON for the Rust tests to replay.
func writeDerivedCases(path string) {
	cases := []*coreauth.Auth{
		{ID: "claude-a@b.json", Provider: "Claude", Attributes: map[string]string{"path": "/auth/dir/claude-a@b.json", "source": "/auth/dir/claude-a@b.json", "source_backend": "file"},
			Metadata: map[string]any{"type": "claude", "access_token": jwt(`{"exp":1900000000}`), "expired": "2001-01-01T00:00:00Z"}},
		{ID: "x.json", Provider: "codex", Attributes: map[string]string{"source": "/a/../a/b/./x.json"}, Metadata: map[string]any{"expired": "2030-01-02T03:04:05+02:00"}},
		{ID: "x.json", Provider: "kimi", Attributes: map[string]string{"path": "/a/x.json"}, Metadata: map[string]any{"type": " KIMI-ai ", "expired": "2030-01-02 10:00:00"}},
		{ID: "claude:abc", Provider: "claude", Attributes: map[string]string{"api_key": "sk", "base_url": "https://x", "auth_kind": "apikey", "source": "config:claude[abc]"}},
		{ID: "gemini:1", Provider: "gemini", Attributes: map[string]string{"api_key": "k", "base_url": " https://g "}},
		{ID: "openai-compatibility:n:1", Provider: "openai-compatibility", Attributes: map[string]string{"api_key": "k", "compat_name": "n", "base_url": "https://c"}},
		{ID: "codex:1", Provider: "codex", Attributes: map[string]string{"api_key": "k2"}},
		{ID: "plain-id", Provider: "weird"},
		{ID: "seeded", Provider: "claude", Attributes: map[string]string{"auth_index_seed": "my|seed"}},
		{ID: "meta.json", Provider: "meta", Attributes: map[string]string{"path": "/m/meta.json"}, Metadata: map[string]any{"access_token": "opaque", "expires_in": 3600, "timestamp": float64(1700000000000)}},
		{ID: "nested.json", Provider: "x", Attributes: map[string]string{"path": "/m/nested.json"}, Metadata: map[string]any{"token": map[string]any{"access_token": "t", "expiry": "2031-05-06T07:08:09Z"}}},
		{ID: "zero.json", Provider: "x", Attributes: map[string]string{"path": "/m/zero.json"}, Metadata: map[string]any{"expired": float64(0)}},
		{ID: "secs.json", Provider: "x", Attributes: map[string]string{"path": "/m/secs.json"}, Metadata: map[string]any{"expires_at": "1900000000"}},
		{ID: "ms.json", Provider: "x", Attributes: map[string]string{"path": "/m/ms.json"}, Metadata: map[string]any{"expire": float64(1900000000123)}},
		{ID: "none.json", Provider: "x", Attributes: map[string]string{"path": "/m/none.json"}, Metadata: map[string]any{"note": "nothing"}},
		{ID: "jwtstr.json", Provider: "x", Attributes: map[string]string{"path": "/m/jwtstr.json"}, Metadata: map[string]any{"accessToken": jwt(`{"exp":"1900000001"}`)}},
		{ID: "oauthkind.json", Provider: "x", Attributes: map[string]string{"path": "/m/oauthkind.json", "runtime_only": "true"}, Metadata: map[string]any{"email": "e@x"}},
		{ID: "runtime", Provider: "x", Attributes: map[string]string{"auth_kind": "OAuth2", "source": "config:x[1]"}},
	}
	out := make([]derivedCase, 0, len(cases))
	for i, a := range cases {
		a.FileName = a.ID
		var exp *int64
		if t, ok := a.ExpirationTime(); ok {
			u := t.Unix()
			if t.IsZero() {
				u = -62135596800
			}
			exp = &u
		}
		out = append(out, derivedCase{
			Name: fmt.Sprintf("case-%d", i), Provider: a.Provider, ID: a.ID, FileName: a.FileName,
			Attributes: a.Attributes, Metadata: a.Metadata, Index: a.EnsureIndex(), ExpiresUnix: exp,
			AuthKind: a.AuthKind(), SourceKind: a.AuthSourceKind(),
		})
	}
	data, err := json.MarshalIndent(out, "", "  ")
	if err != nil {
		fmt.Fprintln(os.Stderr, err)
		os.Exit(1)
	}
	if err := os.WriteFile(path, append(data, '\n'), 0o644); err != nil {
		fmt.Fprintln(os.Stderr, err)
		os.Exit(1)
	}
}

// writeMisc records Go outputs for URL building, credential file names and Vertex key
// normalization (with a throwaway RSA key, stored as DER only).
func writeMisc(path string) {
	pkce := &claude.PKCECodes{CodeVerifier: "verifier", CodeChallenge: "challenge-abc_123"}
	codexPKCE := &codex.PKCECodes{CodeVerifier: "verifier", CodeChallenge: "challenge-abc_123"}
	claudeURL, _, _ := claude.NewClaudeAuth(nil).GenerateAuthURL("state-1", pkce)
	codexURL, _ := codex.NewCodexAuth(nil).GenerateAuthURL("state-1", codexPKCE)
	devinSvc := devin.NewDevinAuthService(nil)

	key, err := rsa.GenerateKey(rand.Reader, 2048)
	if err != nil {
		panic(err)
	}
	pkcs1 := x509.MarshalPKCS1PrivateKey(key)
	pkcs8, err := x509.MarshalPKCS8PrivateKey(key)
	if err != nil {
		panic(err)
	}
	b64 := base64.StdEncoding.EncodeToString
	pemOf := func(kind string, der []byte) string {
		return string(pem.EncodeToMemory(&pem.Block{Type: kind, Bytes: der}))
	}
	normalize := func(pk string) string {
		out, err := vertex.NormalizeServiceAccountMap(map[string]any{"private_key": pk})
		if err != nil {
			panic(err)
		}
		return out["private_key"].(string)
	}
	mangled := "\x1b[0m-----BEGIN PRIVATE KEY-----" + b64(pkcs8) + "-----END PRIVATE KEY-----\r\n"
	body := func(p string) string {
		lines := strings.Split(strings.TrimSpace(p), "\n")
		return strings.Join(lines[1:len(lines)-1], "\n")
	}

	misc := map[string]any{
		"auth_urls": map[string]string{
			"claude":       claudeURL,
			"codex":        codexURL,
			"antigravity":  antigravity.NewAntigravityAuth(nil, nil).BuildAuthURL("state-1", ""),
			"antigravity_custom": antigravity.NewAntigravityAuth(nil, nil).BuildAuthURL("s t/1", "http://localhost:4000/oauth-callback"),
			"devin":        devinSvc.BuildAuthorizationURL("http://127.0.0.1:5000/callback", "challenge-abc_123", "state-1"),
			"devin_paste":  devinSvc.BuildAuthorizationURL("", "challenge-abc_123", "state-1"),
		},
		"file_names": map[string]string{
			"claude_legacy":   claude.CredentialFileName("a@b.com", "", ""),
			"claude_org":      claude.CredentialFileName(" a@b.com ", "org-1", "acc-1"),
			"claude_account":  claude.CredentialFileName("a@b.com", " ", "acc-1"),
			"codex_full":      codex.CredentialFileName("a@b.com", "Team Plus", "deadbeef", true),
			"codex_nohash":    codex.CredentialFileName("a@b.com", "free", "", true),
			"codex_noplan":    codex.CredentialFileName("a@b.com", " ", "deadbeef", true),
			"codex_bare":      codex.CredentialFileName("a@b.com", "", "", true),
			"codex_noprefix":  codex.CredentialFileName("a@b.com", "pro", "", false),
			"xai_email":       xai.CredentialFileName("a b@x.ai", "sub"),
			"xai_sub":         xai.CredentialFileName("", "sub/1 2"),
			"meta_email":      meta.CredentialFileName(" a+b@x.io ", "sub"),
			"meta_sub":        meta.CredentialFileName("", "sub"),
			"meta_none":       meta.CredentialFileName("", ""),
			"antigravity":     antigravity.CredentialFileName(" u@x.com "),
			"antigravity_none": antigravity.CredentialFileName(""),
		},
		"vertex": map[string]any{
			"pkcs1_der_b64":      b64(pkcs1),
			"pkcs8_der_b64":      b64(pkcs8),
			"expected_pem_body":  body(normalize(pemOf("RSA PRIVATE KEY", pkcs1))),
			"from_pkcs8_body":    body(normalize(pemOf("PRIVATE KEY", pkcs8))),
			"from_mangled_body":  body(normalize(mangled)),
		},
	}
	data, err := json.MarshalIndent(misc, "", "  ")
	if err != nil {
		panic(err)
	}
	if err := os.WriteFile(path, append(data, '\n'), 0o644); err != nil {
		panic(err)
	}
}

func main() {
	dir := os.Args[1]
	if len(os.Args) > 2 {
		writeDerivedCases(os.Args[2])
	}
	if len(os.Args) > 3 {
		writeMisc(os.Args[3])
	}
	store := sdkauth.NewFileTokenStore()
	store.SetBaseDir(dir)
	ctx := coreauth.WithAuthCreationIntent(context.Background())

	save := func(a *coreauth.Auth) {
		if _, err := store.Save(ctx, a); err != nil {
			fmt.Fprintln(os.Stderr, "save", a.ID, err)
			os.Exit(1)
		}
	}

	save(&coreauth.Auth{
		ID: "claude.json", FileName: "claude.json", Provider: "claude",
		Storage: &claude.ClaudeTokenStorage{
			AccessToken: "sk-ant-oat01-AT", RefreshToken: "sk-ant-ort01-RT", LastRefresh: "2026-10-01T12:00:00Z",
			Email: "user@example.com", AccountUUID: "acc-1", OrganizationUUID: "org-1", OrganizationName: "Org <One> & Co",
			DeviceIDs: []string{"abababababababababababababababababababababababababababababababab"},
			Expire:    "2026-10-01T20:00:00Z",
		},
		Metadata: map[string]any{
			"email": "user@example.com", "proxy-url": "socks5://127.0.0.1:1080", "priority": 5,
			"headers": map[string]any{"X-Custom": "v"}, "note": "a<b>&c",
		},
	})

	save(&coreauth.Auth{
		ID: "codex.json", FileName: "codex.json", Provider: "codex",
		Storage: &codex.CodexTokenStorage{
			IDToken: "idt", AccessToken: "at", RefreshToken: "rt", AccountID: "acct-1",
			LastRefresh: "2026-10-01T12:00:00Z", Email: "dev@example.com", Expire: "2026-10-01T20:00:00Z", PlanType: "plus",
		},
		Metadata: map[string]any{"email": "dev@example.com", "plan_type": "plus", "websockets": true, "weight": 3},
	})

	// Codex without a plan type: omitempty must drop it.
	save(&coreauth.Auth{
		ID: "codex-noplan.json", FileName: "codex-noplan.json", Provider: "codex",
		Storage:  &codex.CodexTokenStorage{AccessToken: "at", Email: "e@x"},
		Disabled: true,
		Metadata: map[string]any{},
	})

	save(&coreauth.Auth{
		ID: "xai.json", FileName: "xai.json", Provider: "xai",
		Storage: &xai.TokenStorage{
			AccessToken: "at", RefreshToken: "rt", IDToken: "idt", TokenType: "Bearer", ExpiresIn: 3600,
			Expire: "2026-10-01T13:00:00Z", LastRefresh: "2026-10-01T12:00:00Z", Email: "grok@x.ai", Subject: "sub-1",
			BaseURL: "https://api.x.ai/v1", TokenEndpoint: "https://auth.x.ai/oauth2/token",
		},
		Metadata: map[string]any{"email": "grok@x.ai", "label": "mine"},
	})

	save(&coreauth.Auth{
		ID: "kimi-ai.json", FileName: "kimi-ai.json", Provider: "kimi-ai",
		Storage: &kimi.KimiTokenStorage{
			AccessToken: "at", RefreshToken: "rt", TokenType: "Bearer", Scope: "kimi-code", DeviceID: "dev-1",
			Expired: "2026-10-01T13:00:00Z", Type: "kimi-ai",
		},
		Metadata: map[string]any{"timestamp": 1790000000000, "domain": "kimi.ai"},
	})

	save(&coreauth.Auth{
		ID: "vertex.json", FileName: "vertex.json", Provider: "vertex",
		Storage: &vertex.VertexCredentialStorage{
			ServiceAccount: map[string]any{"type": "service_account", "project_id": "proj", "client_email": "sa@proj.iam", "private_key": "placeholder\nnot-a-real-key\n"},
			ProjectID:      "proj", Email: "sa@proj.iam", Location: "us-central1", Prefix: "team",
		},
		Metadata: map[string]any{"label": "proj (sa@proj.iam)"},
	})

	save(&coreauth.Auth{
		ID: "meta.json", FileName: "meta.json", Provider: "meta",
		Storage: &meta.MetaTokenStorage{
			AccessToken: "key-1", DCAToken: "dca:tok", APIKey: "key-1", TokenType: "Bearer", ExpiresIn: 3600,
			DCAExpired: "2026-10-01T13:00:00Z", DCAExpiresAt: 1790000000, LastRefresh: "2026-10-01T12:00:00Z",
			BaseURL: "https://api.meta.ai/v1", Email: "m@x.com", Name: "M",
		},
		Metadata: map[string]any{"subs_tier_name": "pro", "is_subs_active": true, "access_token": "must-not-override", "note": "kept"},
	})

	// Metadata-only credential (antigravity style): compact, sorted, no trailing newline.
	save(&coreauth.Auth{
		ID: "antigravity-u@x.com.json", FileName: "antigravity-u@x.com.json", Provider: "antigravity",
		Metadata: map[string]any{
			"type": "antigravity", "access_token": "ya29.at", "refresh_token": "1//rt", "expires_in": 3599,
			"timestamp": 1790000000000, "expired": "2026-10-01T13:00:00Z", "email": "u@x.com", "project_id": "proj-9",
			"request-retry": 2, "excluded-models": []any{"a", "b"},
		},
	})
}
