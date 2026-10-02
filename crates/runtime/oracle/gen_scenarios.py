#!/usr/bin/env python3
"""Writes ../tests/fixtures/service_scenarios.json: the inputs of the Go/Rust synthesizer and
model-registration differential test (see zz_golden_test.go and ../tests/service_golden.rs)."""
import base64
import json
import os


def jwt(payload):
    enc = lambda o: base64.urlsafe_b64encode(json.dumps(o).encode()).rstrip(b"=").decode()
    return f"{enc({'alg': 'none'})}.{enc(payload)}.sig"


plus = jwt({"email": "p@x", "https://api.openai.com/auth": {"chatgpt_plan_type": "plus", "chatgpt_account_id": "acc"}})

S = []

S.append({"name": "gemini_keys", "config_yaml": """
gemini-api-key:
  - api-key: "AIza-1"
    prefix: " team "
    base-url: "https://g.example.com/"
    proxy-url: "socks5://127.0.0.1:1080"
    priority: 5
    weight: 3
    headers:
      X-B: " two "
      X-A: one
    excluded-models: ["Gemini-2.5-Flash*", " gemini-2.0* "]
    models:
      - name: gemini-2.5-pro
        alias: pro
        display-name: "Pro Alias"
        thinking: {min: 128, max: 32768, zero-allowed: true}
      - name: gemini-2.5-flash
        alias: pro
      - name: gemini-2.5-flash-lite
        max-context-length: 5000
    disable-cooling: true
    request-retry: 2
    request-scoped-errors:
      - status: 400
        match: ["bad"]
        action: stop
  - base-url: "https://only-base.example.com"
  - api-key: "AIza-3"
    excluded-models: ["gemini-2.5-pro"]
interactions-api-key:
  - api-key: "int-1"
    base-url: "https://int.example.com"
    prefix: ix
    request-retry: -1
"""})

S.append({"name": "claude_keys", "config_yaml": """
claude-api-key:
  - api-key: sk-ant-1
    base-url: https://claude.example.com
    prefix: c1
    fingerprint-profile: " Claude-Code-CLI "
    rebuild-mid-system-message: true
    excluded-models: ["claude-3-5*"]
    models:
      - {name: claude-sonnet-4-5, alias: sonnet, display-name: Sonnet X, max-context-length: 500000, is-compat: true, thinking: {levels: [Low, "", NONE, auto, low]}}
      - {name: "claude-opus-4-1(high)", alias: ""}
    headers: {X-Token: abc}
    priority: -2
    weight: 0
  - api-key: sk-ant-2
    excluded-models: ["claude-haiku*", "*opus*"]
    weight: 7
  - api-key: sk-ant-2
    base-url: https://claude2.example.com
    proxy-url: direct
"""})

S.append({"name": "codex_xai_meta_keys", "config_yaml": """
codex-api-key:
  - api-key: sk-codex-1
    base-url: https://codex.example.com/v1
    websockets: true
    alpha-search: true
    disable-codex-cloaking: false
    models:
      - {name: gpt-5-codex, alias: codexy, display-name: Codex Y, support-configuration-update: true}
      - {name: gpt-5, alias: gpt-5}
  - api-key: sk-codex-2
    base-url: https://codex2.example.com/v1
    proxy-url: direct
    excluded-models: ["gpt-image*"]
xai-api-key:
  - api-key: xai-1
    base-url: https://xai.example.com/v1
    websockets: true
    models: [{name: grok-4, alias: g4}]
  - api-key: xai-2
    base-url: https://xai2.example.com/v1
meta-api-key:
  - api-key: meta-1
    base-url: https://meta.example.com/v1
    models: [{name: muse-spark, alias: spark}]
  - api-key: meta-2
    base-url: https://meta2.example.com/v1
    excluded-models: ["muse*"]
"""})

S.append({"name": "openai_compat", "config_yaml": """
openai-compatibility:
  - name: OpenRouter
    prefix: or
    base-url: https://openrouter.ai/api/v1
    priority: 3
    headers: {HTTP-Referer: x}
    disable-cooling: false
    request-retry: 1
    request-scoped-errors: [{status: 429, match: [limit], action: continue}]
    api-key-entries:
      - {api-key: or-key-1, weight: 5}
      - {api-key: or-key-2, proxy-url: "http://proxy:8080"}
    models:
      - {name: openai/gpt-5, alias: gpt5, thinking: {levels: [minimal, High, high]}}
      - {name: anthropic/claude-sonnet-4.5, alias: gpt5}
      - {name: img-model, alias: img, image: true, input-modalities: [Text, " ", text, IMAGE], output-modalities: [image]}
      - {name: plain, display-name: Plain Display, max-context-length: 4096, is-compat: true}
  - name: keyless
    base-url: https://keyless.example.com
    models: [{name: m1, alias: k1}]
  - name: off
    disabled: true
    base-url: https://off.example.com
    api-key-entries: [{api-key: x}]
  - name: nomodels
    base-url: https://nomodels.example.com
    api-key-entries: [{api-key: nm}]
"""})

S.append({"name": "vertex_keys", "config_yaml": """
vertex-api-key:
  - api-key: v-1
    base-url: https://vertex.example.com
    prefix: vx
    models: [{name: gemini-2.5-pro, alias: vpro}]
    excluded-models: ["x*"]
    headers: {X-V: "1"}
  - api-key: v-2
"""})

files = {
    "claude-a@b.com.json": {"type": "claude", "email": "a@b.com", "access_token": "t", "refresh_token": "r", "expired": "2099-01-01T00:00:00Z", "prefix": "/team/", "priority": "4", "weight": 2, "headers": {"X-H": " v "}, "note": " hello ", "excluded_models": ["claude-3-5*"], "model_aliases": [{"name": "claude-sonnet-4-5", "alias": "sonnet-alias", "fork": True, "display-name": "Sonnet Alias"}], "fingerprint-profile": "Claude-Code-CLI", "proxy_url": "http://p:1"},
    "codex-plus.json": {"type": "codex", "email": "c@x", "id_token": plus},
    "codex-explicit.json": {"type": "codex", "plan_type": " team "},
    "codex-badjwt.json": {"type": "codex", "id_token": "garbage"},
    "codex-noplan.json": {"type": "codex", "email": "n@x"},
    "kimi.json": {"type": "kimi", "domain": "https://www.kimi.ai", "email": "k@x"},
    "kimi-com.json": {"type": "kimi.com", "base_url": "https://api.kimi.com/coding/"},
    "gemini-legacy.json": {"type": "gemini", "email": "g@x"},
    "gemini-cli.json": {"type": "gemini-cli"},
    "foo.json": {"type": "Foo ", "email": "f@x"},
    "disabled.json": {"type": "antigravity", "disabled": True, "email": "d@x"},
    "agy.json": {"type": "antigravity", "email": "agy@x", "prefix": "a/b", "model-aliases": [{"name": "gemini-3-pro-preview", "alias": "g3"}, {"name": "x", "alias": "X"}, {"name": "x", "alias": "x"}], "excluded-models": ["gemini-3-flash*", " "]},
    "vertex.json": {"type": "vertex", "project_id": "p", "email": "v@x"},
    "xai.json": {"type": "xai", "email": "x@x", "priority": 3.9},
    "meta.json": {"type": "meta", "email": "m@x"},
    "devin.json": {"type": "devin", "email": "d@x"},
    "aistudio.json": {"type": "aistudio", "email": "as@x"},
    "weights.json": {"type": "codex", "plan_type": "free", "weight": "1000000", "priority": "abc"},
    "no-type.json": {"email": "x"},
    "UPPER.JSON": {"type": "claude", "email": "upper@x"},
}
raw = {k: json.dumps(v) for k, v in files.items()}
raw["bad-weight.json"] = json.dumps({"type": "claude", "weight": "abc"})
raw["too-heavy.json"] = json.dumps({"type": "claude", "weight": 1000001})
raw["invalid.json"] = "{not json"
raw["array.json"] = "[1]"
raw["empty.json"] = ""
raw["notes.txt"] = "ignored"
S.append({"name": "auth_files", "config_yaml": "debug: false\n", "auth_files": raw})

S.append({"name": "oauth_config_rules", "config_yaml": """
force-model-prefix: true
oauth-excluded-models:
  Claude: ["claude-3-5*", "*haiku*"]
  codex: ["gpt-5*mini"]
oauth-model-alias:
  claude:
    - {name: claude-sonnet-4-5, alias: son, fork: true}
    - {name: claude-opus-4-1, alias: opus, display-name: "Opus!"}
  codex:
    - {name: gpt-5, alias: g5}
oauth-settings:
  claude:
    - {name: claude-sonnet-4-5, max-context-length: 123456}
  codex:
    - {name: g5, alias: g5, max-context-length: 99}
claude-api-key:
  - api-key: sk-conf
    prefix: kp
    models: [{name: claude-sonnet-4-5, alias: son2}]
""", "auth_files": {
    "claude-p.json": json.dumps({"type": "claude", "email": "p@x", "prefix": "team"}),
    "claude-q.json": json.dumps({"type": "claude", "email": "q@x", "excluded-models": ["claude-opus*"]}),
    "codex-p.json": json.dumps({"type": "codex", "plan_type": "pro", "prefix": "cx"}),
    "agy-p.json": json.dumps({"type": "antigravity", "prefix": "ag"}),
}})

out = os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "tests", "fixtures", "service_scenarios.json")
with open(out, "w") as f:
    json.dump(S, f, indent=1)
print("wrote", os.path.normpath(out))
