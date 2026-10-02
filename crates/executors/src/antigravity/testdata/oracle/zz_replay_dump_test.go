package executor

import (
	"bytes"
	"encoding/json"
	"fmt"
	"math/rand"
	"os"
	"testing"

	"github.com/router-for-me/CLIProxyAPI/v8/internal/util"
	"github.com/tidwall/gjson"
	"github.com/tidwall/sjson"
)

type replayDumpCase struct {
	Payload         json.RawMessage   `json:"payload"`
	UseSchemas      bool              `json:"use_schemas"`
	Applied         json.RawMessage   `json:"applied"`
	Changed         bool              `json:"changed"`
	ItemsFromReq    []json.RawMessage `json:"items_from_request"`
	Degraded        json.RawMessage   `json:"degraded"`
	DegradedCount   int               `json:"degraded_count"`
	Repaired        json.RawMessage   `json:"repaired"`
	RolesNormalized json.RawMessage   `json:"roles_normalized"`
	HasReserved     bool              `json:"has_reserved"`
}

func TestZZReplayDump(t *testing.T) {
	outPath := os.Getenv("REPLAY_OUT")
	if outPath == "" {
		t.Skip("REPLAY_OUT not set")
	}
	const turns = 6
	rs := rand.New(rand.NewSource(20261002))
	base := syntheticAntigravityReplayMixedPayload(turns)
	items := legacyAntigravityReasoningReplayItemsFromRequest(base)
	schemas := map[string]any{"lookup": map[string]any{"type": "object", "properties": map[string]any{
		"turn": map[string]any{"type": "integer"},
		"x":    map[string]any{"type": "string", "default": "d"},
	}}}

	var cases []replayDumpCase
	for caseIndex := 0; caseIndex < 500; caseIndex++ {
		payload := bytes.Clone(base)
		for range 1 + rs.Intn(5) {
			turn := rs.Intn(turns)
			modelIndex := 1 + turn*2
			responseIndex := modelIndex + 1
			part := rs.Intn(3)
			var err error
			switch rs.Intn(16) {
			case 0:
				payload, err = sjson.DeleteBytes(payload, fmt.Sprintf("request.contents.%d.parts.%d.thoughtSignature", modelIndex, part))
			case 1:
				payload, err = sjson.SetBytes(payload, fmt.Sprintf("request.contents.%d.parts.2.functionCall.args.turn", modelIndex), turn+50)
			case 2:
				payload, err = sjson.DeleteBytes(payload, fmt.Sprintf("request.contents.%d.parts.2.functionCall.id", modelIndex))
			case 3:
				payload, err = sjson.SetBytes(payload, fmt.Sprintf("request.contents.%d.parts.1.text", modelIndex), "drifted")
			case 4:
				payload, err = sjson.SetBytes(payload, fmt.Sprintf("request.contents.%d.role", responseIndex), "model")
			case 5:
				payload, err = sjson.DeleteBytes(payload, fmt.Sprintf("request.contents.%d.parts.%d", modelIndex, part))
			case 6:
				payload, err = sjson.SetBytes(payload, fmt.Sprintf("request.contents.%d.parts.0.thought", modelIndex), false)
			case 7:
				payload, err = sjson.SetBytes(payload, fmt.Sprintf("request.contents.%d.parts.2.functionCall.id", modelIndex), "call-0")
			case 8:
				payload, err = sjson.SetBytes(payload, "request.toolConfig.functionCallingConfig.mode", "ANY")
			case 9:
				args := fmt.Sprintf(`{"turn":%d}`, turn)
				stable := util.GeminiClaudeToolUseID(fmt.Sprintf("call-%d", turn), "lookup", args)
				payload, err = sjson.SetBytes(payload, fmt.Sprintf("request.contents.%d.parts.2.functionCall.id", modelIndex), stable)
				if err == nil {
					payload, err = sjson.SetBytes(payload, fmt.Sprintf("request.contents.%d.parts.0.functionResponse.id", responseIndex), stable)
				}
			case 10:
				for p := 0; p < 3; p++ {
					payload, _ = sjson.DeleteBytes(payload, fmt.Sprintf("request.contents.%d.parts.%d.thoughtSignature", modelIndex, p))
				}
			case 11:
				payload, err = sjson.SetBytes(payload, fmt.Sprintf("request.contents.%d.parts.2.functionCall.args.x", modelIndex), "d")
			case 12:
				payload, err = sjson.DeleteBytes(payload, fmt.Sprintf("request.contents.%d.parts.0.functionResponse.id", responseIndex))
			case 13:
				payload, err = sjson.SetBytes(payload, fmt.Sprintf("request.contents.%d.parts.0.functionResponse.name", responseIndex), "unknown")
			case 14:
				payload, err = sjson.SetBytes(payload, fmt.Sprintf("request.contents.%d.parts.2.thoughtSignature", modelIndex), "skip_thought_signature_validator")
			case 15:
				payload, err = sjson.SetBytes(payload, fmt.Sprintf("request.contents.%d.parts.%d.thought_signature", modelIndex, part), "snake-sig")
			}
			if err != nil {
				t.Fatalf("mutation: %v", err)
			}
		}
		useSchemas := caseIndex%2 == 1
		var sch map[string]any
		if useSchemas {
			sch = schemas
		}
		applied, changed := applyAntigravityReasoningReplayItems(payload, items, sch)
		degraded, count := degradeAntigravityClaudeToolProvenanceIDs(payload)
		cases = append(cases, replayDumpCase{
			Payload:         payload,
			UseSchemas:      useSchemas,
			Applied:         applied,
			Changed:         changed,
			ItemsFromReq:    toRaw(antigravityReasoningReplayItemsFromRequest(payload)),
			Degraded:        degraded,
			DegradedCount:   count,
			Repaired:        antigravityRepairUnsignedFirstFunctionCalls(payload),
			RolesNormalized: normalizeAntigravityGeminiFunctionResponseRoles(payload),
			HasReserved:     antigravityPayloadHasClaudeToolProvenanceID(payload),
		})
	}
	out := map[string]any{
		"base":    json.RawMessage(base),
		"items":   toRaw(items),
		"schemas": schemas,
		"cases":   cases,
	}
	b, err := json.Marshal(out)
	if err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(outPath, b, 0o644); err != nil {
		t.Fatal(err)
	}
	_ = gjson.Result{}
}

func toRaw(items [][]byte) []json.RawMessage {
	out := make([]json.RawMessage, 0, len(items))
	for _, it := range items {
		out = append(out, json.RawMessage(it))
	}
	return out
}
