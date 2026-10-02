package executor

import (
	"context"
	"encoding/json"
	"fmt"
	"math/rand"
	"net/http"
	"os"
	"strings"
	"testing"

	"github.com/router-for-me/CLIProxyAPI/v8/internal/cache"
	"github.com/router-for-me/CLIProxyAPI/v8/internal/config"
	"github.com/router-for-me/CLIProxyAPI/v8/internal/runtime/executor/helps"
	cliproxyexecutor "github.com/router-for-me/CLIProxyAPI/v8/sdk/cliproxy/executor"
	"github.com/tidwall/gjson"
)

func rawJSON(b []byte) json.RawMessage { return json.RawMessage(b) }

func randSig(r *rand.Rand) string {
	return fmt.Sprintf("sig-%d", r.Intn(6))
}

func randPart(r *rand.Rand) map[string]any {
	part := map[string]any{}
	switch r.Intn(9) {
	case 0, 1:
		part["text"] = fmt.Sprintf("t%d", r.Intn(4))
	case 2, 3:
		part["text"] = fmt.Sprintf("th%d", r.Intn(4))
		part["thought"] = true
	case 4:
		// signature-only carrier
		if r.Intn(2) == 0 {
			part["thought"] = true
		}
		part["text"] = ""
	case 5, 6:
		fc := map[string]any{"name": []string{"a", "b"}[r.Intn(2)], "args": map[string]any{"n": r.Intn(3)}}
		if r.Intn(3) != 0 {
			fc["id"] = fmt.Sprintf("id-%d", r.Intn(4))
		}
		part["functionCall"] = fc
	case 7:
		part["text"] = fmt.Sprintf("t%d", r.Intn(4))
		part["thought"] = false
	case 8:
		part["inlineData"] = map[string]any{"mimeType": "image/png", "data": "AAA"}
	}
	if r.Intn(2) == 0 {
		switch r.Intn(4) {
		case 0, 1, 2:
			part["thoughtSignature"] = randSig(r)
		case 3:
			part["thought_signature"] = randSig(r)
		}
	}
	return part
}

type accCase struct {
	Request json.RawMessage `json:"request"`
	Lines   []string        `json:"lines"`
	Items   []json.RawMessage `json:"items"`
}

type streamCase struct {
	Stream string          `json:"stream"`
	Out    json.RawMessage `json:"out"`
}

type decisionCase struct {
	Body       string `json:"body"`
	Kind       string `json:"kind"`
	RetryMs    int64  `json:"retry_ms"`
	HasRetry   bool   `json:"has_retry"`
	Reason     string `json:"reason"`
	Explicit   bool   `json:"explicit_credits"`
	Injected   string `json:"injected,omitempty"`
}

type scopeCase struct {
	Headers   map[string]string `json:"headers"`
	Metadata  map[string]any    `json:"metadata"`
	ReqMeta   map[string]any    `json:"req_metadata"`
	Orig      json.RawMessage   `json:"orig,omitempty"`
	Payload   json.RawMessage   `json:"payload"`
	ReqBody   json.RawMessage   `json:"req_body,omitempty"`
	Source    string            `json:"source"`
	Key       string            `json:"key"`
	Valid     bool              `json:"valid"`
	SchemaNames []string        `json:"schema_names,omitempty"`
}

type schemaCase struct {
	Payload json.RawMessage `json:"payload"`
	Antigravity bool        `json:"antigravity"`
	Out     json.RawMessage `json:"out"`
	NeedsSan bool           `json:"needs_sanitization"`
}

type envelopeCase struct {
	Model   string          `json:"model"`
	Payload json.RawMessage `json:"payload"`
	Project string          `json:"project"`
	Derived string          `json:"derived"`
	Out     json.RawMessage `json:"out"`
}

type miscMisc struct {
	Words         []string          `json:"words"`
	Instruction   []json.RawMessage `json:"instructions"`
	Obfuscated    []json.RawMessage `json:"obfuscated"`
	Boundary      []json.RawMessage `json:"boundary_in"`
	BoundaryOut   []json.RawMessage `json:"boundary_out"`
	Leading       []json.RawMessage `json:"leading_out"`
	ClaudeReqs    []json.RawMessage `json:"claude_reqs"`
	ClaudeTokens  []int64           `json:"claude_tokens"`
	Compaction    json.RawMessage   `json:"compaction"`
	Grounding     json.RawMessage   `json:"grounding"`
}

func TestZZMiscDump(t *testing.T) {
	outPath := os.Getenv("MISC_OUT")
	if outPath == "" {
		t.Skip("MISC_OUT not set")
	}
	r := rand.New(rand.NewSource(77))
	ctx := context.Background()
	cache.ClearAntigravityReasoningReplayCache()

	// ---- accumulator
	bases := []string{
		`{"request":{"contents":[{"role":"user","parts":[{"text":"q"}]}]}}`,
		`{"request":{"systemInstruction":{"parts":[{"text":"s"}]},"contents":[{"role":"user","parts":[{"text":"q"}]},{"role":"model","parts":[{"text":"t1"},{"functionCall":{"name":"a","args":{"n":1},"id":"id-1"}}]}]}}`,
		string(syntheticAntigravityReplayMixedPayload(2)),
		`{"request":{"contents":[{"role":"user","parts":[{"text":"q"}]},{"role":"model","parts":[{"text":"t1","thoughtSignature":"sig-1"},{"text":"th1","thought":true,"thoughtSignature":"sig-2"}]}]}}`,
		`{"request":{"contents":[]}}`,
		`{"nothing":true}`,
	}
	var accCases []accCase
	for i := 0; i < 400; i++ {
		base := bases[r.Intn(len(bases))]
		scope := antigravityReasoningReplayScope{modelName: "m", sessionKey: fmt.Sprintf("session:acc-%d", i)}
		_, snap, _, _ := cache.GetAntigravityReasoningReplayItemsWithSnapshotRequired(ctx, scope.modelName, scope.sessionKey)
		scope.cacheSnapshot = snap
		acc := newAntigravityReasoningReplayAccumulator(scope, []byte(base))
		var lines []string
		nChunks := 1 + r.Intn(5)
		for c := 0; c < nChunks; c++ {
			var parts []any
			for p := 0; p < 1+r.Intn(3); p++ {
				parts = append(parts, randPart(r))
			}
			resp := map[string]any{"candidates": []any{map[string]any{"content": map[string]any{"role": "model", "parts": parts}}}}
			if c == nChunks-1 && r.Intn(6) != 0 {
				resp["candidates"].([]any)[0].(map[string]any)["finishReason"] = "STOP"
			}
			b, _ := json.Marshal(map[string]any{"response": resp})
			line := "data: " + string(b)
			lines = append(lines, line)
			acc.ObserveSSELine([]byte(line))
		}
		acc.Commit(ctx)
		items, _ := cache.GetAntigravityReasoningReplayItems(scope.modelName, scope.sessionKey)
		accCases = append(accCases, accCase{Request: rawJSON([]byte(base)), Lines: lines, Items: toRaw(items)})
	}

	// ---- convertStreamToNonStream
	var streamCases []streamCase
	ex := NewAntigravityExecutor(&config.Config{})
	for i := 0; i < 300; i++ {
		var sb strings.Builder
		for c := 0; c < 1+r.Intn(6); c++ {
			var parts []any
			for p := 0; p < r.Intn(4); p++ {
				parts = append(parts, randPart(r))
			}
			cand := map[string]any{"content": map[string]any{"parts": parts}}
			if r.Intn(3) == 0 {
				cand["content"].(map[string]any)["role"] = "model"
			}
			if r.Intn(4) == 0 {
				cand["finishReason"] = []string{"STOP", "MAX_TOKENS", ""}[r.Intn(3)]
			}
			resp := map[string]any{"candidates": []any{cand}}
			if r.Intn(3) == 0 {
				resp["modelVersion"] = "mv"
			}
			if r.Intn(3) == 0 {
				resp["responseId"] = fmt.Sprintf("rid-%d", r.Intn(3))
			}
			if r.Intn(3) == 0 {
				resp["usageMetadata"] = map[string]any{"promptTokenCount": r.Intn(100), "totalTokenCount": r.Intn(200)}
			}
			var line any = map[string]any{"response": resp}
			if r.Intn(8) == 0 {
				line = resp
			}
			if r.Intn(4) == 0 {
				line.(map[string]any)["traceId"] = fmt.Sprintf("tr-%d", r.Intn(3))
			}
			b, _ := json.Marshal(line)
			sb.Write(b)
			sb.WriteString("\n")
			if r.Intn(10) == 0 {
				sb.WriteString("not json\n\n")
			}
		}
		stream := sb.String()
		streamCases = append(streamCases, streamCase{Stream: stream, Out: rawJSON(ex.convertStreamToNonStream([]byte(stream)))})
	}
	streamCases = append(streamCases, streamCase{Stream: "", Out: rawJSON(ex.convertStreamToNonStream(nil))})

	// ---- 429 decisions
	bodies := []string{
		``, `not json`, `{}`,
		`{"error":{"status":"RESOURCE_EXHAUSTED","details":[{"@type":"type.googleapis.com/google.rpc.ErrorInfo","reason":"QUOTA_EXHAUSTED"}]}}`,
		`{"error":{"status":"resource_exhausted","details":[{"@type":"type.googleapis.com/google.rpc.ErrorInfo","reason":"rate_limit_exceeded"},{"@type":"type.googleapis.com/google.rpc.RetryInfo","retryDelay":"2.9s"}]}}`,
		`{"error":{"status":"RESOURCE_EXHAUSTED","details":[{"@type":"type.googleapis.com/google.rpc.ErrorInfo","reason":"RATE_LIMIT_EXCEEDED"},{"@type":"type.googleapis.com/google.rpc.RetryInfo","retryDelay":"3s"}]}}`,
		`{"error":{"status":"RESOURCE_EXHAUSTED","details":[{"@type":"type.googleapis.com/google.rpc.ErrorInfo","reason":"RATE_LIMIT_EXCEEDED"},{"@type":"type.googleapis.com/google.rpc.RetryInfo","retryDelay":"299s"}]}}`,
		`{"error":{"status":"RESOURCE_EXHAUSTED","details":[{"@type":"type.googleapis.com/google.rpc.ErrorInfo","reason":"RATE_LIMIT_EXCEEDED"},{"@type":"type.googleapis.com/google.rpc.RetryInfo","retryDelay":"300s"}]}}`,
		`{"error":{"status":"RESOURCE_EXHAUSTED","details":[{"@type":"type.googleapis.com/google.rpc.ErrorInfo","reason":"RATE_LIMIT_EXCEEDED"}]}}`,
		`{"error":{"status":"RESOURCE_EXHAUSTED","message":"Quota exhausted for model"}}`,
		`{"error":{"status":"RESOURCE_EXHAUSTED","message":"something else"}}`,
		`{"error":{"status":"UNAVAILABLE","details":[{"@type":"type.googleapis.com/google.rpc.ErrorInfo","reason":"QUOTA_EXHAUSTED"}]}}`,
		`{"error":{"status":"RESOURCE_EXHAUSTED","details":[{"@type":"type.googleapis.com/google.rpc.ErrorInfo","reason":"INSUFFICIENT_G1_CREDITS_BALANCE"}]}}`,
		`{"error":{"status":"RESOURCE_EXHAUSTED","details":[{"@type":"type.googleapis.com/google.rpc.ErrorInfo","reason":"X","metadata":{"quotaResetDelay":"1m30s"}}]}}`,
		`{"error":{"message":"Your quota will reset after 18s."}}`,
		`{"error":{"message":"Try again after 1h2m3s"}}`,
		`{"error":{"details":[{"@type":"type.googleapis.com/google.rpc.RetryInfo","retryDelay":"bogus"}]}}`,
		`{"error":{"status":"RESOURCE_EXHAUSTED","details":[{"@type":"type.googleapis.com/google.rpc.ErrorInfo","reason":"QUOTA_EXHAUSTED"},{"@type":"type.googleapis.com/google.rpc.ErrorInfo","reason":"RATE_LIMIT_EXCEEDED"}]}}`,
	}
	var decisions []decisionCase
	for _, b := range bodies {
		d := decideAntigravity429([]byte(b))
		dc := decisionCase{Body: b, Kind: string(d.kind), Reason: d.reason, Explicit: antigravityHasExplicitCreditsBalanceExhaustedReason([]byte(b))}
		if d.retryAfter != nil {
			dc.HasRetry = true
			dc.RetryMs = d.retryAfter.Milliseconds()
		}
		if inj := injectEnabledCreditTypes([]byte(b)); inj != nil {
			dc.Injected = string(inj)
		}
		decisions = append(decisions, dc)
	}

	// ---- scopes
	claudeSys := `{"model":"claude","system":[{"type":"text","text":"sys","cache_control":{"type":"ephemeral"}}],"metadata":{"user_id":"u_session_aa11-22"},"messages":[]}`
	claudeSys2 := `{"model":"claude","system":"sys","metadata":{"user_id":"{\"session_id\":\"json-sess\"}"},"messages":[]}`
	type sc struct {
		h    map[string]string
		m    map[string]any
		rm   map[string]any
		orig string
		pl   string
		src  string
	}
	scs := []sc{
		{nil, nil, nil, "", `{"sessionId":"s1"}`, "openai"},
		{nil, nil, nil, "", `{"request":{"sessionId":"s2"}}`, "openai"},
		{nil, nil, nil, "", `{"request":{"contents":[{"role":"user","parts":[{"text":"hello"}]}]}}`, "openai"},
		{nil, nil, nil, "", `{"request":{"contents":[]}}`, "openai"},
		{map[string]string{"Session-Id": "h1"}, nil, nil, "", `{}`, "openai"},
		{map[string]string{"Session_id": "h2"}, nil, nil, "", `{}`, "openai"},
		{nil, map[string]any{"execution_session_id": "e1"}, nil, "", `{}`, "openai"},
		{nil, nil, map[string]any{"execution_session_id": "e2"}, "", `{}`, "openai"},
		{nil, map[string]any{"derived_session_id": "d1"}, nil, "", `{}`, "openai"},
		{nil, nil, nil, `{"prompt_cache_key":"p1"}`, `{}`, "openai"},
		{nil, nil, nil, `{"session_id":"b1"}`, `{}`, "openai"},
		{nil, nil, nil, `{"metadata":{"session_id":"b2"}}`, `{}`, "openai"},
		{map[string]string{"Session-Id": "h1"}, map[string]any{"execution_session_id": "e1"}, nil, `{"prompt_cache_key":"p1"}`, `{}`, "openai"},
		{nil, map[string]any{"execution_session_id": "e1"}, nil, `{"prompt_cache_key":"p1"}`, `{}`, "openai"},
		{nil, nil, nil, claudeSys, claudeSys, "claude"},
		{map[string]string{"X-Claude-Code-Session-Id": "cc1"}, nil, nil, claudeSys, claudeSys, "claude"},
		{map[string]string{"X-Claude-Code-Session-Id": "cc1", "X-Claude-Code-Agent-Id": "ag9"}, nil, nil, claudeSys2, claudeSys2, "claude"},
		{nil, nil, nil, claudeSys2, claudeSys2, "claude"},
		{nil, nil, nil, "", `{"contents":[{"role":"user","parts":[{"text":"x"}]}]}`, "gemini"},
		{nil, nil, nil, "", ``, "openai"},
	}
	var scopes []scopeCase
	for _, s := range scs {
		hdr := http.Header{}
		for k, v := range s.h {
			hdr.Set(k, v)
		}
		req := cliproxyexecutor.Request{Model: "gemini-3.7-flash", Payload: []byte(s.pl), Metadata: s.rm}
		opts := cliproxyexecutor.Options{Headers: hdr, Metadata: s.m}
		if s.orig != "" {
			opts.OriginalRequest = []byte(s.orig)
		}
		scope := antigravityReasoningReplayScopeFromRequest(ctx, "gemini-3.7-flash", req, opts, []byte(s.pl))
		c := scopeCase{Headers: s.h, Metadata: s.m, ReqMeta: s.rm, Source: s.src, Key: scope.sessionKey, Valid: scope.valid()}
		if s.orig != "" {
			c.Orig = rawJSON([]byte(s.orig))
		}
		if json.Valid([]byte(s.pl)) {
			c.Payload = rawJSON([]byte(s.pl))
		} else {
			c.Payload = rawJSON([]byte(`""`))
		}
		scopes = append(scopes, c)
	}

	// ---- schema sanitization
	schemaPayloads := []string{
		`{"request":{"tools":[{"functionDeclarations":[{"name":"f","parameters":{"type":"object","properties":{"a":{"type":"string","minLength":2},"b":{"anyOf":[{"type":"integer"},{"type":"null"}]},"c":{"$ref":"#/$defs/X"}},"$defs":{"X":{"type":"object","properties":{"id":{"type":"string"}}}},"additionalProperties":false}}]}]}}`,
		`{"request":{"tools":[{"functionDeclarations":[{"name":"f","parametersJsonSchema":{"type":"object","properties":{"a":{"type":"string","enum":["x","y"]}}}}]}]}}`,
		`{"request":{"tools":[{"function_declarations":[{"name":"f","parameters_json_schema":{"type":"object","properties":{}},"response":{"type":"object","properties":{"r":{"type":"string","title":"R"}}}}]}]}}`,
		`{"request":{"tools":[{"functionDeclarations":[{"name":"empty","parameters":{"type":"object","properties":{}}}]}]}}`,
		`{"request":{"tools":[{"functionDeclarations":[{"name":"f","parameters":{"type":"object","properties":{"a":{"type":"string"}}}}]},{"googleSearch":{}}]}}`,
		`{"request":{"contents":[{"role":"model","parts":[{"functionCall":{"name":"f","args":{"title":"keep","format":"x","default":1,"const":2}}}]}],"generationConfig":{"responseSchema":{"type":"object","properties":{"a":{"type":"string","title":"A","default":"x"}},"additionalProperties":false,"nullable":true}}}}`,
		`{"request":{"generation_config":{"response_json_schema":{"type":"object","properties":{"a":{"enum":["a","b"],"type":"string"}}}}}}`,
		`{"request":{"generationConfig":{"responseJsonSchema":{"type":"object","properties":{"a":{"type":"integer","exclusiveMinimum":0}}},"responseMimeType":"application/json"}}}`,
		`{"request":{"tools":[{"functionDeclarations":[{"name":"f","parameters":{"type":"object","properties":{"p":{"allOf":[{"type":"object","properties":{"x":{"type":"string"}}},{"type":"object","properties":{"y":{"type":"string"}}}]},"q":{"type":["string","null"]},"r":{"const":"fixed"},"s":{"type":"array","items":{"type":"string"},"minItems":1}}}}]}]}}`,
		`{"request":{"contents":[]}}`,
		`{"request":{"tools":[]}}`,
	}
	var schemaCases []schemaCase
	for _, p := range schemaPayloads {
		for _, ag := range []bool{false, true} {
			out := sanitizeAntigravityRequestSchemas(p, ag)
			schemaCases = append(schemaCases, schemaCase{Payload: rawJSON([]byte(p)), Antigravity: ag, Out: rawJSON([]byte(out)), NeedsSan: antigravityRequestNeedsSchemaSanitization([]byte(p))})
		}
	}

	// ---- envelope
	envPayloads := []string{
		`{"request":{"contents":[{"role":"user","parts":[{"text":"hello"}]}]}}`,
		`{"request":{"contents":[{"role":"model","parts":[{"text":"x"}]},{"role":"user","parts":[{"text":"second"}]}]}}`,
		`{"request":{"sessionId":"keep-me","contents":[{"role":"user","parts":[{"text":"hello"}]}]},"requestType":"web_search"}`,
		`{"request":{"contents":[{"role":"user","parts":[{"text":"hello"}]}],"safetySettings":[{"a":1}]},"toolConfig":{"functionCallingConfig":{"mode":"ANY"}}}`,
		`{"request":{"toolConfig":{"keep":true},"contents":[{"role":"user","parts":[{"text":"hello"}]}]},"toolConfig":{"drop":true},"model":"old","userAgent":"x"}`,
	}
	var envCases []envelopeCase
	for _, p := range envPayloads {
		for _, model := range []string{"gemini-3.7-flash", "gemini-3.1-flash-image", "claude-sonnet-4-5-thinking"} {
			for _, derived := range []string{"", "-123"} {
				for _, project := range []string{"proj", ""} {
					out := geminiToAntigravity(model, []byte(p), project, derived)
					envCases = append(envCases, envelopeCase{Model: model, Payload: rawJSON([]byte(p)), Project: project, Derived: derived, Out: rawJSON(out)})
				}
			}
		}
	}

	// ---- misc helps
	var m miscMisc
	m.Words = []string{"secret", "Token", "x", "a​b", "  padded  "}
	instrs := []string{
		`{"request":{"systemInstruction":{"parts":[{"text":"my SECRET token is padded here"},{"text":"nothing"},{"inline":1}]}}}`,
		`{"request":{"system_instruction":"plain secret string"}}`,
		`{"request":{"systemInstruction":"Token Token"}}`,
		`{"request":{"contents":[]}}`,
	}
	matcher := helps.BuildSensitiveWordMatcher(m.Words)
	for _, in := range instrs {
		m.Instruction = append(m.Instruction, rawJSON([]byte(in)))
		m.Obfuscated = append(m.Obfuscated, rawJSON(helps.ObfuscateSensitiveWordsInSystemInstruction([]byte(in), matcher)))
	}
	boundaries := []string{
		`{"request":{"contents":[{"role":"model","parts":[{"text":"a"}]}]}}`,
		`{"request":{"contents":[{"role":"user","parts":[{"text":"a"}]},{"role":"model","parts":[{"text":"b"}]}]}}`,
		`{"request":{"contents":[{"role":"user","parts":[{"text":"a"}]},{"role":"model","parts":[{"functionResponse":{"name":"x","response":{}}}]}]}}`,
		`{"request":{"contents":[{"role":"assistant","parts":[{"text":"a"}]}]}}`,
		`{"request":{"contents":[]}}`,
		`{"request":{"contents":{"x":1}}}`,
		`{"request":{}}`,
	}
	for _, b := range boundaries {
		m.Boundary = append(m.Boundary, rawJSON([]byte(b)))
		m.BoundaryOut = append(m.BoundaryOut, rawJSON(helps.EnsureGeminiBoundaryUserContent([]byte(b), "request.contents")))
		m.Leading = append(m.Leading, rawJSON(helps.EnsureGeminiLeadingUserContent([]byte(b), "request.contents")))
	}
	claudeReqs := []string{
		`{"system":"sys prompt","messages":[{"role":"user","content":"hello world"},{"role":"assistant","content":[{"type":"text","text":"hi"},{"type":"tool_use","id":"t1","name":"bash","input":{"cmd":"ls  -la"}}]},{"role":"user","content":[{"type":"tool_result","tool_use_id":"t1","content":[{"type":"text","text":"out"}]}]}],"tools":[{"name":"bash","description":"run it","input_schema":{"type":"object","properties":{"cmd":{"type":"string"}}}}],"tool_choice":{"type":"auto"}}`,
		`{"messages":[{"role":"user","content":[{"type":"image","source":{"data":"x"}},{"type":"thinking","thinking":"hmm"},{"type":"document","title":"T","context":"C","source":{"type":"text","data":"D"}}]}]}`,
		`{"system":[{"type":"text","text":"a"},"b"],"messages":[]}`,
		`{"messages":[{"role":"user","content":[{"foo":"bar"},{"type":"mystery","text":"zzz"}]}]}`,
		`not json`, ``,
	}
	for _, c := range claudeReqs {
		m.ClaudeReqs = append(m.ClaudeReqs, rawJSON([]byte(fmt.Sprintf("%q", c))))
		n, err := helps.CountClaudeInputTokens([]byte(c))
		if err != nil {
			n = -1
		}
		m.ClaudeTokens = append(m.ClaudeTokens, n)
	}

	capsule, _ := helps.SealAntigravityCompaction("summary text <b>&", "gemini-x")
	expanded, _ := helps.ExpandAntigravityCompactionCapsules([]byte(`{"input":[{"type":"compaction","encrypted_content":"` + capsule + `"},{"type":"message","role":"user","content":"hi"}]}`))
	summaryPayload := helps.PrepareAntigravityCompactionSummaryPayload([]byte(`{"model":"m","stream":true,"tools":[1],"input":[{"type":"message","role":"user","content":"do"},{"type":"compaction_trigger"}],"metadata":{"a":1},"instructions":"i"}`), "m")
	summaryPayload2 := helps.PrepareAntigravityCompactionSummaryPayload([]byte(`{"model":"m","input":"just text"}`), "m")
	extract := func(s string) string {
		out, err := helps.ExtractAntigravitySummaryText([]byte(s))
		if err != nil {
			return "ERR:" + err.Error()
		}
		return out
	}
	m.Compaction, _ = json.Marshal(map[string]any{
		"capsule":   capsule,
		"expanded":  rawJSON(expanded),
		"summary1":  rawJSON(summaryPayload),
		"summary2":  rawJSON(summaryPayload2),
		"extract_responses": extract(`{"output":[{"type":"reasoning"},{"type":"message","content":[{"type":"output_text","text":"A"},{"type":"output_text","text":"B"}]},{"type":"message","content":"C"}]}`),
		"extract_gemini":    extract(`{"response":{"candidates":[{"content":{"parts":[{"text":"x","thought":true},{"text":"G1"},{"text":"G2"}]}}]}}`),
		"extract_claude":    extract(`{"content":[{"type":"text","text":"c1"},{"type":"tool_use"},{"type":"text","text":"c2"}]}`),
		"extract_chat":      extract(`{"choices":[{"message":{"content":"chat"}}]}`),
		"extract_none":      extract(`{"a":1}`),
		"has_trigger":       helps.HasResponsesCompactionTrigger([]byte(`{"input":[{"type":"compaction_trigger"}]}`)),
		"has_item":          helps.HasResponsesCompactionItem([]byte(`{"input":[{"type":"compaction"}]}`)),
		"has_item_neg":      helps.HasResponsesCompactionItem([]byte(`{"input":"x"}`)),
		"bad_unseal":        func() string { _, err := helps.ExpandAntigravityCompactionCapsules([]byte(`{"input":[{"type":"compaction","encrypted_content":"nope"}]}`)); return err.Error() }(),
	})
	_ = gjson.Result{}

	out := map[string]any{
		"acc": accCases, "streams": streamCases, "decisions": decisions, "scopes": scopes,
		"schemas": schemaCases, "envelopes": envCases, "misc": m,
	}
	b, err := json.Marshal(out)
	if err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(outPath, b, 0o644); err != nil {
		t.Fatal(err)
	}
}
