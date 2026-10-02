package executor

import (
	"context"
	"encoding/json"
	"io"
	"net/http"
	"net/http/httptest"
	"os"
	"strings"
	"sync"
	"testing"
	"time"

	"github.com/router-for-me/CLIProxyAPI/v8/internal/cache"
	"github.com/router-for-me/CLIProxyAPI/v8/internal/config"
	"github.com/router-for-me/CLIProxyAPI/v8/internal/runtime/executor/helps"
	cliproxyauth "github.com/router-for-me/CLIProxyAPI/v8/sdk/cliproxy/auth"
	cliproxyexecutor "github.com/router-for-me/CLIProxyAPI/v8/sdk/cliproxy/executor"
	sdktranslator "github.com/router-for-me/CLIProxyAPI/v8/sdk/translator"
)

type oracleUpstream struct {
	Status  int               `json:"status"`
	Headers map[string]string `json:"headers,omitempty"`
	Body    string            `json:"body"`
}

type oracleCase struct {
	Name             string            `json:"name"`
	Kind             string            `json:"kind"`
	Model            string            `json:"model"`
	SourceFormat     string            `json:"source_format"`
	ResponseFormat   string            `json:"response_format,omitempty"`
	Payload          json.RawMessage   `json:"payload"`
	OriginalPayload  json.RawMessage   `json:"original_payload,omitempty"`
	Alt              string            `json:"alt,omitempty"`
	Headers          map[string]string `json:"headers,omitempty"`
	Metadata         map[string]any    `json:"metadata,omitempty"`
	ReqMetadata      map[string]any    `json:"req_metadata,omitempty"`
	AuthID           string            `json:"auth_id,omitempty"`
	AuthAttributes   map[string]string `json:"auth_attributes,omitempty"`
	AuthMetadata     map[string]any    `json:"auth_metadata,omitempty"`
	SensitiveWords   []string          `json:"sensitive_words,omitempty"`
	CreditsEnabled   bool              `json:"credits_enabled,omitempty"`
	CreditsRequested bool              `json:"credits_requested,omitempty"`
	Upstream         oracleUpstream    `json:"upstream"`
	Expect           any               `json:"expect,omitempty"`
}

type oracleRequest struct {
	Method  string            `json:"method"`
	Path    string            `json:"path"`
	Query   string            `json:"query"`
	Headers map[string]string `json:"headers"`
	Body    json.RawMessage   `json:"body"`
}

type oracleError struct {
	Status       int    `json:"status"`
	Message      string `json:"message"`
	RetryAfterMs int64  `json:"retry_after_ms,omitempty"`
}

type oracleResult struct {
	Payload string       `json:"payload,omitempty"`
	Chunks  []string     `json:"chunks,omitempty"`
	Error   *oracleError `json:"error,omitempty"`
}

type oracleExpect struct {
	Request *oracleRequest `json:"request,omitempty"`
	Result  oracleResult   `json:"result"`
}

func oracleError2(err error) *oracleError {
	e := &oracleError{Message: err.Error()}
	type sc interface{ StatusCode() int }
	if s, ok := err.(sc); ok {
		e.Status = s.StatusCode()
	}
	type ra interface{ RetryAfter() *time.Duration }
	if r, ok := err.(ra); ok && r.RetryAfter() != nil {
		e.RetryAfterMs = r.RetryAfter().Milliseconds()
	}
	return e
}

func TestZZOracle(t *testing.T) {
	inPath := os.Getenv("ORACLE_IN")
	outPath := os.Getenv("ORACLE_OUT")
	if inPath == "" || outPath == "" {
		t.Skip("oracle env not set")
	}
	raw, err := os.ReadFile(inPath)
	if err != nil {
		t.Fatal(err)
	}
	var cases []oracleCase
	if err := json.Unmarshal(raw, &cases); err != nil {
		t.Fatal(err)
	}
	cache.ClearAntigravityReasoningReplayCache()
	for i := range cases {
		c := &cases[i]
		var mu sync.Mutex
		var captured *oracleRequest
		server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
			body, _ := io.ReadAll(r.Body)
			if strings.Contains(r.URL.Path, "loadCodeAssist") {
				w.WriteHeader(200)
				_, _ = w.Write([]byte(`{}`))
				return
			}
			hdr := map[string]string{}
			for _, k := range []string{"Content-Type", "Authorization", "User-Agent", "Accept-Encoding", "Connection", "X-Custom"} {
				if v := r.Header.Get(k); v != "" {
					hdr[strings.ToLower(k)] = v
				}
			}
			var bodyRaw json.RawMessage = body
			if !json.Valid(body) {
				bodyRaw, _ = json.Marshal(string(body))
			}
			mu.Lock()
			captured = &oracleRequest{Method: r.Method, Path: r.URL.Path, Query: r.URL.RawQuery, Headers: hdr, Body: bodyRaw}
			mu.Unlock()
			for k, v := range c.Upstream.Headers {
				w.Header().Set(k, v)
			}
			status := c.Upstream.Status
			if status == 0 {
				status = 200
			}
			w.WriteHeader(status)
			_, _ = w.Write([]byte(c.Upstream.Body))
		}))

		cfg := &config.Config{RequestRetry: 1}
		cfg.Antigravity.SensitiveWords = c.SensitiveWords
		cfg.QuotaExceeded.AntigravityCredits = c.CreditsEnabled
		ex := NewAntigravityExecutor(cfg)

		attrs := map[string]string{"base_url": server.URL}
		for k, v := range c.AuthAttributes {
			attrs[k] = v
		}
		meta := map[string]any{
			"access_token": "token-123",
			"expired":      time.Now().Add(24 * time.Hour).Format(time.RFC3339),
			"project_id":   "project-1",
		}
		for k, v := range c.AuthMetadata {
			meta[k] = v
		}
		auth := &cliproxyauth.Auth{ID: c.AuthID, Provider: "antigravity", Attributes: attrs, Metadata: meta}

		hdr := http.Header{}
		for k, v := range c.Headers {
			hdr.Set(k, v)
		}
		capsule, _ := helps.SealAntigravityCompaction("capsule summary text", "gemini-3.7-flash")
		payload := []byte(strings.ReplaceAll(string(c.Payload), "__CAPSULE__", capsule))
		opts := cliproxyexecutor.Options{
			SourceFormat:   sdktranslator.FromString(c.SourceFormat),
			ResponseFormat: sdktranslator.FromString(c.ResponseFormat),
			Stream:         c.Kind == "stream",
			Alt:            c.Alt,
			Headers:        hdr,
			Metadata:       c.Metadata,
		}
		if c.ResponseFormat == "" {
			opts.ResponseFormat = ""
		}
		if len(c.OriginalPayload) > 0 {
			opts.OriginalRequest = []byte(strings.ReplaceAll(string(c.OriginalPayload), "__CAPSULE__", capsule))
		}
		req := cliproxyexecutor.Request{Model: c.Model, Payload: payload, Metadata: c.ReqMetadata}
		ctx := context.Background()
		if c.CreditsRequested {
			ctx = cliproxyauth.WithAntigravityCredits(ctx)
		}

		var res oracleResult
		switch c.Kind {
		case "execute":
			resp, e := ex.Execute(ctx, auth, req, opts)
			if e != nil {
				res.Error = oracleError2(e)
			} else {
				res.Payload = string(resp.Payload)
			}
		case "count":
			resp, e := ex.CountTokens(ctx, auth, req, opts)
			if e != nil {
				res.Error = oracleError2(e)
			} else {
				res.Payload = string(resp.Payload)
			}
		case "stream":
			result, e := ex.ExecuteStream(ctx, auth, req, opts)
			if e != nil {
				res.Error = oracleError2(e)
			} else {
				for chunk := range result.Chunks {
					if chunk.Err != nil {
						res.Error = oracleError2(chunk.Err)
						break
					}
					res.Chunks = append(res.Chunks, string(chunk.Payload))
				}
			}
		}
		server.Close()
		mu.Lock()
		c.Expect = oracleExpect{Request: captured, Result: res}
		mu.Unlock()
	}
	out, err := json.MarshalIndent(cases, "", " ")
	if err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(outPath, out, 0o644); err != nil {
		t.Fatal(err)
	}
}
