package main

/*
#include <stdint.h>
#include <stdlib.h>

typedef struct {
	void* ptr;
	size_t len;
} cliproxy_buffer;

typedef int (*cliproxy_host_call_fn)(void*, const char*, const uint8_t*, size_t, cliproxy_buffer*);
typedef void (*cliproxy_host_free_fn)(void*, size_t);

typedef struct {
	uint32_t abi_version;
	void* host_ctx;
	cliproxy_host_call_fn call;
	cliproxy_host_free_fn free_buffer;
} cliproxy_host_api;

typedef int (*cliproxy_plugin_call_fn)(char*, uint8_t*, size_t, cliproxy_buffer*);
typedef void (*cliproxy_plugin_free_fn)(void*, size_t);
typedef void (*cliproxy_plugin_shutdown_fn)(void);

typedef struct {
	uint32_t abi_version;
	cliproxy_plugin_call_fn call;
	cliproxy_plugin_free_fn free_buffer;
	cliproxy_plugin_shutdown_fn shutdown;
} cliproxy_plugin_api;

extern int cliproxyPluginCall(char*, uint8_t*, size_t, cliproxy_buffer*);
extern void cliproxyPluginFree(void*, size_t);
extern void cliproxyPluginShutdown(void);

static const cliproxy_host_api* stored_host;

static void store_host_api(const cliproxy_host_api* host) {
	stored_host = host;
}

static int call_host_api(const char* method, const uint8_t* request, size_t request_len, cliproxy_buffer* response) {
	if (stored_host == NULL || stored_host->call == NULL) {
		return 1;
	}
	return stored_host->call(stored_host->host_ctx, method, request, request_len, response);
}

static void free_host_buffer(void* ptr, size_t len) {
	if (stored_host != NULL && stored_host->free_buffer != NULL && ptr != NULL) {
		stored_host->free_buffer(ptr, len);
	}
}
*/
import "C"

import (
	"encoding/json"
	"fmt"
	"net/http"
	"net/url"
	"strings"
	"unsafe"

	"github.com/router-for-me/CLIProxyAPI/v8/sdk/pluginabi"
	"github.com/router-for-me/CLIProxyAPI/v8/sdk/pluginapi"
)

// e2e-formats exercises plugin features the stock examples do not: a custom protocol format
// name carried by an executor and translated by a response translator, a model router for the
// Codex Alpha Search route, and the host.routing.reset_cooldown callback.
const (
	pluginName   = "e2e-formats"
	provider     = "xproto"
	customFormat = "x-acme-proto"
	alphaFormat  = "codex-alpha-search"
)

type envelope struct {
	OK     bool            `json:"ok"`
	Result json.RawMessage `json:"result,omitempty"`
	Error  *envelopeError  `json:"error,omitempty"`
}

type envelopeError struct {
	Code    string `json:"code"`
	Message string `json:"message"`
}

type registration struct {
	SchemaVersion uint32                 `json:"schema_version"`
	Metadata      pluginapi.Metadata     `json:"metadata"`
	Capabilities  registrationCapability `json:"capabilities"`
}

type registrationCapability struct {
	AuthProvider          bool                         `json:"auth_provider"`
	ModelRegistrar        bool                         `json:"model_registrar"`
	ModelProvider         bool                         `json:"model_provider"`
	ModelRouter           bool                         `json:"model_router"`
	Executor              bool                         `json:"executor"`
	ExecutorModelScope    pluginapi.ExecutorModelScope `json:"executor_model_scope"`
	ExecutorInputFormats  []string                     `json:"executor_input_formats,omitempty"`
	ExecutorOutputFormats []string                     `json:"executor_output_formats,omitempty"`
	ResponseTranslator    bool                         `json:"response_translator"`
	ManagementAPI         bool                         `json:"management_api"`
}

type identifierResponse struct {
	Identifier string `json:"identifier"`
}

type streamResponse struct {
	Headers http.Header                     `json:"headers,omitempty"`
	Chunks  []pluginapi.ExecutorStreamChunk `json:"chunks,omitempty"`
}

type managementRegistration struct {
	Resources []managementResource `json:"resources,omitempty"`
}

type managementResource struct {
	Path        string `json:"Path"`
	Menu        string `json:"Menu"`
	Description string `json:"Description"`
}

type managementRequest struct {
	Method string
	Path   string
	Query  url.Values
	Body   []byte
}

type managementResponse struct {
	StatusCode int         `json:"StatusCode"`
	Headers    http.Header `json:"Headers"`
	Body       []byte      `json:"Body"`
}

func main() {}

//export cliproxy_plugin_init
func cliproxy_plugin_init(host *C.cliproxy_host_api, plugin *C.cliproxy_plugin_api) C.int {
	if plugin == nil {
		return 1
	}
	C.store_host_api(host)
	plugin.abi_version = C.uint32_t(pluginabi.ABIVersion)
	plugin.call = C.cliproxy_plugin_call_fn(C.cliproxyPluginCall)
	plugin.free_buffer = C.cliproxy_plugin_free_fn(C.cliproxyPluginFree)
	plugin.shutdown = C.cliproxy_plugin_shutdown_fn(C.cliproxyPluginShutdown)
	return 0
}

//export cliproxyPluginCall
func cliproxyPluginCall(method *C.char, request *C.uint8_t, requestLen C.size_t, response *C.cliproxy_buffer) C.int {
	if response != nil {
		response.ptr = nil
		response.len = 0
	}
	if method == nil {
		writeResponse(response, errorEnvelope("invalid_method", "method is required"))
		return 1
	}
	var requestBytes []byte
	if request != nil && requestLen > 0 {
		requestBytes = C.GoBytes(unsafe.Pointer(request), C.int(requestLen))
	}
	raw, errHandle := handleMethod(C.GoString(method), requestBytes)
	if errHandle != nil {
		writeResponse(response, errorEnvelope("plugin_error", errHandle.Error()))
		return 1
	}
	writeResponse(response, raw)
	return 0
}

//export cliproxyPluginFree
func cliproxyPluginFree(ptr unsafe.Pointer, len C.size_t) {
	if ptr != nil {
		C.free(ptr)
	}
	_ = len
}

//export cliproxyPluginShutdown
func cliproxyPluginShutdown() {}

func handleMethod(method string, request []byte) ([]byte, error) {
	switch method {
	case pluginabi.MethodPluginRegister, pluginabi.MethodPluginReconfigure:
		return okEnvelope(pluginRegistration())
	case pluginabi.MethodModelRegister:
		return okEnvelope(pluginapi.ModelRegistrationResponse{Provider: provider, Models: models()})
	case pluginabi.MethodModelStatic, pluginabi.MethodModelForAuth:
		return okEnvelope(pluginapi.ModelResponse{Provider: provider, Models: models()})
	case pluginabi.MethodAuthIdentifier:
		return okEnvelope(identifierResponse{Identifier: provider})
	case pluginabi.MethodAuthParse:
		return okEnvelope(pluginapi.AuthParseResponse{Handled: true, Auth: pluginapi.AuthData{
			Provider:    provider,
			ID:          "xproto",
			FileName:    "xproto.json",
			Label:       "xproto",
			StorageJSON: append([]byte(nil), request...),
			Metadata:    map[string]any{"type": provider},
		}})
	case pluginabi.MethodExecutorIdentifier:
		return okEnvelope(identifierResponse{Identifier: provider})
	case pluginabi.MethodExecutorExecute:
		return executeNonStream(request)
	case pluginabi.MethodExecutorExecuteStream:
		return executeStream(request)
	case pluginabi.MethodResponseTranslate:
		return translateResponse(request)
	case pluginabi.MethodModelRoute:
		return routeModel(request)
	case pluginabi.MethodManagementRegister:
		return okEnvelope(managementRegistration{Resources: []managementResource{{
			Path:        "/reset",
			Menu:        "Reset cooldown",
			Description: "Calls host.routing.reset_cooldown for ?auth_index=.",
		}}})
	case pluginabi.MethodManagementHandle:
		return handleManagement(request)
	default:
		return errorEnvelope("unknown_method", "unknown method: "+method), nil
	}
}

func models() []pluginapi.ModelInfo {
	return []pluginapi.ModelInfo{{
		ID:                         "xproto-model",
		Object:                     "model",
		OwnedBy:                    provider,
		DisplayName:                "Custom Format Model",
		SupportedGenerationMethods: []string{"chat"},
		ContextLength:              8192,
		MaxCompletionTokens:        1024,
		UserDefined:                true,
	}}
}

func pluginRegistration() registration {
	return registration{
		SchemaVersion: pluginabi.SchemaVersion,
		Metadata: pluginapi.Metadata{
			Name:             pluginName,
			Version:          "0.1.0",
			Author:           "cliproxyapirust",
			GitHubRepository: "https://github.com/router-for-me/CLIProxyAPI",
			Logo:             "https://example.invalid/e2e-formats.png",
			ConfigFields:     []pluginapi.ConfigField{},
		},
		Capabilities: registrationCapability{
			AuthProvider:       true,
			ModelRegistrar:     true,
			ModelProvider:      true,
			ModelRouter:        true,
			Executor:           true,
			ExecutorModelScope: pluginapi.ExecutorModelScopeBoth,
			// The Alpha Search source format is declared so that routing to this executor passes
			// the readiness check (a custom format name has to survive that check).
			ExecutorInputFormats:  []string{"chat-completions", alphaFormat},
			ExecutorOutputFormats: []string{customFormat},
			ResponseTranslator:    true,
			ManagementAPI:         true,
		},
	}
}

// executeNonStream answers in the custom format and reports the formats the host sent.
func executeNonStream(raw []byte) ([]byte, error) {
	var req pluginapi.ExecutorRequest
	if errUnmarshal := json.Unmarshal(raw, &req); errUnmarshal != nil {
		return nil, errUnmarshal
	}
	payload := fmt.Sprintf("acme:format=%s,source=%s,model=%s,stream=%t", req.Format, req.SourceFormat, req.Model, req.Stream)
	return okEnvelope(pluginapi.ExecutorResponse{Payload: []byte(payload)})
}

func executeStream(raw []byte) ([]byte, error) {
	var req pluginapi.ExecutorRequest
	if errUnmarshal := json.Unmarshal(raw, &req); errUnmarshal != nil {
		return nil, errUnmarshal
	}
	first := fmt.Sprintf("acme:format=%s,source=%s,stream=%t", req.Format, req.SourceFormat, req.Stream)
	return okEnvelope(streamResponse{Chunks: []pluginapi.ExecutorStreamChunk{
		{Payload: []byte(first)},
		{Payload: []byte("acme:second")},
	}})
}

// translateResponse turns the custom format into a chat completion (or chunk) that names the
// formats the host passed, so the golden records them.
func translateResponse(raw []byte) ([]byte, error) {
	var req pluginapi.ResponseTransformRequest
	if errUnmarshal := json.Unmarshal(raw, &req); errUnmarshal != nil {
		return nil, errUnmarshal
	}
	if req.FromFormat != customFormat {
		return okEnvelope(pluginapi.PayloadResponse{})
	}
	text := fmt.Sprintf("from=%s to=%s stream=%t body=%s", req.FromFormat, req.ToFormat, req.Stream, string(req.Body))
	var out []byte
	if req.Stream {
		chunk := map[string]any{
			"id":      "xproto-stream",
			"object":  "chat.completion.chunk",
			"created": 0,
			"model":   req.Model,
			"choices": []any{map[string]any{"index": 0, "delta": map[string]any{"role": "assistant", "content": text}, "finish_reason": nil}},
		}
		body, errMarshal := json.Marshal(chunk)
		if errMarshal != nil {
			return nil, errMarshal
		}
		out = append([]byte("data: "), body...)
	} else {
		completion := map[string]any{
			"id":      "xproto-completion",
			"object":  "chat.completion",
			"created": 0,
			"model":   req.Model,
			"choices": []any{map[string]any{"index": 0, "message": map[string]any{"role": "assistant", "content": text}, "finish_reason": "stop"}},
			"usage":   map[string]any{"prompt_tokens": 1, "completion_tokens": 2, "total_tokens": 3},
		}
		body, errMarshal := json.Marshal(completion)
		if errMarshal != nil {
			return nil, errMarshal
		}
		out = body
	}
	return okEnvelope(pluginapi.PayloadResponse{Body: out})
}

// routeModel only answers Codex Alpha Search requests; every other route is left alone.
func routeModel(raw []byte) ([]byte, error) {
	var req pluginapi.ModelRouteRequest
	if errUnmarshal := json.Unmarshal(raw, &req); errUnmarshal != nil {
		return nil, errUnmarshal
	}
	if req.SourceFormat != alphaFormat {
		return okEnvelope(pluginapi.ModelRouteResponse{})
	}
	switch req.RequestedModel {
	case "alpha-route-model":
		return okEnvelope(pluginapi.ModelRouteResponse{Handled: true, TargetKind: pluginapi.ModelRouteTargetProvider, Target: "codex", TargetModel: " gpt-5.5 "})
	case "alpha-route-keep":
		return okEnvelope(pluginapi.ModelRouteResponse{Handled: true, TargetKind: pluginapi.ModelRouteTargetProvider, Target: " CODEX "})
	case "alpha-route-claude":
		return okEnvelope(pluginapi.ModelRouteResponse{Handled: true, TargetKind: pluginapi.ModelRouteTargetProvider, Target: "claude", TargetModel: "claude-sonnet-4-5"})
	case "alpha-route-self":
		return okEnvelope(pluginapi.ModelRouteResponse{Handled: true, TargetKind: pluginapi.ModelRouteTargetSelf})
	}
	return okEnvelope(pluginapi.ModelRouteResponse{})
}

// handleManagement resets the cooldown of ?auth_index= through the host callback and returns the
// callback result (or its error) as JSON.
func handleManagement(raw []byte) ([]byte, error) {
	var req managementRequest
	if len(raw) > 0 {
		if errUnmarshal := json.Unmarshal(raw, &req); errUnmarshal != nil {
			return nil, fmt.Errorf("decode management request: %w", errUnmarshal)
		}
	}
	authIndex := strings.TrimSpace(req.Query.Get("auth_index"))
	result, errCall := callHost(pluginabi.MethodHostRoutingResetCooldown, pluginapi.HostRoutingResetCooldownRequest{AuthIndex: authIndex})
	body := map[string]any{}
	if errCall != nil {
		body["ok"] = false
		body["error"] = errCall.Error()
	} else {
		body["ok"] = true
		body["result"] = json.RawMessage(result)
	}
	out, errMarshal := json.Marshal(body)
	if errMarshal != nil {
		return nil, errMarshal
	}
	return okEnvelope(managementResponse{
		StatusCode: http.StatusOK,
		Headers:    http.Header{"content-type": []string{"application/json"}},
		Body:       out,
	})
}

func callHost(method string, payload any) (json.RawMessage, error) {
	rawPayload, errMarshal := json.Marshal(payload)
	if errMarshal != nil {
		return nil, fmt.Errorf("marshal host callback payload %s: %w", method, errMarshal)
	}
	cMethod := C.CString(method)
	defer C.free(unsafe.Pointer(cMethod))

	var response C.cliproxy_buffer
	var requestPtr *C.uint8_t
	if len(rawPayload) > 0 {
		cPayload := C.CBytes(rawPayload)
		if cPayload == nil {
			return nil, fmt.Errorf("allocate host callback payload %s", method)
		}
		defer C.free(cPayload)
		requestPtr = (*C.uint8_t)(cPayload)
	}
	callCode := C.call_host_api(cMethod, requestPtr, C.size_t(len(rawPayload)), &response)
	var rawResponse []byte
	if response.ptr != nil && response.len > 0 {
		rawResponse = C.GoBytes(response.ptr, C.int(response.len))
	}
	if response.ptr != nil {
		C.free_host_buffer(response.ptr, response.len)
	}
	if len(rawResponse) == 0 {
		return nil, fmt.Errorf("host callback %s returned no response, code=%d", method, int(callCode))
	}
	var env envelope
	if errUnmarshal := json.Unmarshal(rawResponse, &env); errUnmarshal != nil {
		return nil, fmt.Errorf("decode host callback envelope %s: %w", method, errUnmarshal)
	}
	if !env.OK {
		if env.Error != nil {
			return nil, fmt.Errorf("%s: %s", env.Error.Code, env.Error.Message)
		}
		return nil, fmt.Errorf("host callback %s failed", method)
	}
	if callCode != 0 {
		return nil, fmt.Errorf("host callback %s returned code=%d", method, int(callCode))
	}
	return append(json.RawMessage(nil), env.Result...), nil
}

func okEnvelope(v any) ([]byte, error) {
	raw, errMarshal := json.Marshal(v)
	if errMarshal != nil {
		return nil, errMarshal
	}
	return json.Marshal(envelope{OK: true, Result: raw})
}

func errorEnvelope(code, message string) []byte {
	raw, _ := json.Marshal(envelope{OK: false, Error: &envelopeError{Code: code, Message: message}})
	return raw
}

func writeResponse(response *C.cliproxy_buffer, raw []byte) {
	if response == nil || len(raw) == 0 {
		return
	}
	ptr := C.CBytes(raw)
	if ptr == nil {
		return
	}
	response.ptr = ptr
	response.len = C.size_t(len(raw))
}
