#!/usr/bin/env -S uv run --quiet --script
# /// script
# requires-python = ">=3.11"
# dependencies = ["websockets>=13"]
# ///
"""Live differential test against real providers: Go (:18318) vs Rust (:18317).

Start the servers first with tools/live/ctl.sh start (see that file for the auth setup).

  tools/live/diff.py api [--filter SUBSTR]   same HTTP/SSE/websocket requests to both, diff shapes
  tools/live/diff.py agents [--filter S]     real Claude Code and Codex sessions through each proxy

Model output is nondeterministic, so bodies are compared by shape: keys, value types, and the
values of structural keys (type, role, finish_reason, model, ...). Streams compare the sequence
of event shapes with consecutive repeats collapsed. Error cases compare full bodies. A differing
case is re-run once; it only counts as a difference if it differs both times.
Writes tmp/live/results/<timestamp>/{report.md,<case>.json}.
"""

import argparse
import asyncio
import difflib
import http.client
import json
import os
import shutil
import socket
import subprocess
import sys
import time
from concurrent.futures import ThreadPoolExecutor
from dataclasses import dataclass, field
from pathlib import Path

import websockets

ROOT = Path(__file__).resolve().parents[2]
STATE = ROOT / "tmp" / "live"
SERVERS = {"go": 18318, "rust": 18317}
KEY = "live-test-key"
MGMT = "live-mgmt"
CLAUDE = "claude-haiku-4-5-20251001"
MUSE = "muse-spark-1.3"

# Keys whose values are structural and must match exactly.
KEEP = {
    "type", "object", "role", "finish_reason", "stop_reason", "native_finish_reason", "status",
    "model", "event", "name", "code", "param", "finishReason", "failed", "generate", "stream",
    "status_code", "source", "user_agent", "client_ip", "index", "output_index", "content_index",
    "tool_choice", "service_tier", "content-type",
}
# Keys that always vary.
VOLATILE = {"id", "created", "created_at", "timestamp", "request_id", "responseId", "msg_id",
            "item_id", "call_id", "tool_use_id", "signature", "encrypted_content", "auth_index",
            "access_token_sha256", "date", "x-request-id", "request-id", "cf-ray"}
DROP_HEADERS = {"date", "content-length", "x-request-id", "request-id", "cf-ray", "server",
                "set-cookie", "transfer-encoding", "connection", "keep-alive", "vary"}


def shape(v, key=None):
    """Structure of a JSON value: keys and types, with structural values kept."""
    if isinstance(v, dict):
        return {k: shape(v[k], k) for k in sorted(v) if k not in VOLATILE}
    if isinstance(v, list):
        out = []
        for item in v:
            s = shape(item, key)
            if not out or out[-1] != s:
                out.append(s)
        return out
    if isinstance(v, str):
        if key in KEEP:
            return v
        if key in ("arguments", "input") and v[:1] == "{":
            try:
                return shape(json.loads(v))
            except ValueError:
                pass
        return "str"
    if isinstance(v, bool) or v is None:
        return v
    if key in KEEP:
        return v
    return 0 if v == 0 else "num"


def exact(v):
    """Full value minus volatile keys, for deterministic responses (errors, counts)."""
    if isinstance(v, dict):
        return {k: exact(v[k]) for k in sorted(v) if k not in VOLATILE}
    if isinstance(v, list):
        return [exact(x) for x in v]
    return v


def parse_sse(raw: str):
    events = []
    for block in raw.replace("\r\n", "\n").split("\n\n"):
        name, data = None, []
        for line in block.split("\n"):
            if line.startswith("event:"):
                name = line[6:].strip()
            elif line.startswith("data:"):
                data.append(line[5:].strip())
        if name is None and not data:
            continue
        payload = "\n".join(data)
        try:
            payload = json.loads(payload)
        except ValueError:
            pass
        events.append({"event": name, "data": payload})
    return events


def collapse(seq):
    out = []
    for s in seq:
        if not out or out[-1] != s:
            out.append(s)
    return out


def final_text(body) -> str:
    """Best-effort assistant text from any of the response formats, for the report."""
    texts = []

    def walk(v, key=None):
        if isinstance(v, dict):
            for k, x in v.items():
                walk(x, k)
        elif isinstance(v, list):
            for x in v:
                walk(x, key)
        elif isinstance(v, str) and key in ("text", "content", "delta", "output_text") and v:
            texts.append(v)

    walk(body)
    return "".join(texts)[:200]


@dataclass
class Case:
    name: str
    path: str
    body: dict | None = None
    method: str = "POST"
    stream: bool = False
    ws: bool = False
    mode: str = "shape"  # shape | exact | models
    headers: dict = field(default_factory=dict)
    raw_body: str | None = None
    disconnect: bool = False  # close the socket after the first response line


def http_call(port: int, case: Case):
    conn = http.client.HTTPConnection("127.0.0.1", port, timeout=180)
    headers = {"Authorization": f"Bearer {KEY}", "Content-Type": "application/json"}
    headers.update(case.headers)
    data = case.raw_body if case.raw_body is not None else (
        json.dumps(case.body) if case.body is not None else None)
    t0 = time.monotonic()
    conn.request(case.method, case.path, body=data, headers=headers)
    resp = conn.getresponse()
    if case.disconnect:
        resp.fp.readline()
        conn.sock.shutdown(socket.SHUT_RDWR)
        conn.close()
        return {"status": resp.status, "headers": {}, "body": "disconnected",
                "ms": int((time.monotonic() - t0) * 1000)}
    raw = resp.read().decode("utf-8", "replace")
    ms = int((time.monotonic() - t0) * 1000)
    hdrs = {k.lower(): v for k, v in resp.getheaders()}
    ctype = hdrs.get("content-type", "")
    if "text/event-stream" in ctype:
        body = parse_sse(raw)
    else:
        try:
            body = json.loads(raw)
        except ValueError:
            body = raw
    return {"status": resp.status, "headers": hdrs, "body": body, "ms": ms}


async def ws_call_async(port: int, case: Case):
    events = []
    t0 = time.monotonic()
    async with websockets.connect(
        f"ws://127.0.0.1:{port}{case.path}",
        additional_headers={"Authorization": f"Bearer {KEY}"}, open_timeout=30,
    ) as ws:
        await ws.send(json.dumps(case.body))
        while True:
            msg = json.loads(await asyncio.wait_for(ws.recv(), 180))
            events.append({"event": None, "data": msg})
            if msg.get("type") in ("response.completed", "response.failed", "error"):
                break
    return {"status": 101, "headers": {}, "body": events,
            "ms": int((time.monotonic() - t0) * 1000)}


def call(port: int, case: Case):
    try:
        if case.ws:
            return asyncio.run(ws_call_async(port, case))
        return http_call(port, case)
    except Exception as e:  # report transport failures as results, they are diffs too
        return {"status": 0, "headers": {}, "body": f"{type(e).__name__}: {e}", "ms": 0}


def pop_usage(port: int):
    conn = http.client.HTTPConnection("127.0.0.1", port, timeout=10)
    conn.request("GET", "/v0/management/usage-queue?count=100",
                 headers={"Authorization": f"Bearer {MGMT}"})
    resp = conn.getresponse()
    data = json.loads(resp.read() or b"[]")
    for rec in data:  # header values vary per request; compare header names only
        if isinstance(rec.get("response_headers"), dict):
            rec["response_headers"] = sorted(rec["response_headers"])
    return data


def view(case: Case, r):
    """What gets compared for one server's result."""
    body = r["body"]
    if case.mode == "exact":
        b = exact(body)
    elif case.mode == "models":
        b = sorted(m["id"] for m in body.get("data", [])) if isinstance(body, dict) else body
    elif isinstance(body, list) and body and isinstance(body[0], dict) and "data" in body[0] \
            and (case.stream or case.ws):
        b = collapse([shape(e) for e in body])
    else:
        b = shape(body)
    hdrs = {k: (v.split(";")[0] if k == "content-type" else "*")
            for k, v in r["headers"].items()
            if k not in DROP_HEADERS and not k.startswith("anthropic-ratelimit")}
    return {"status": r["status"], "headers": hdrs, "body": b,
            "usage": [shape(u) for u in r.get("usage", [])]}


def run_pair(case: Case):
    for p in SERVERS.values():
        pop_usage(p)
    with ThreadPoolExecutor(2) as ex:
        futs = {s: ex.submit(call, p, case) for s, p in SERVERS.items()}
        res = {s: f.result() for s, f in futs.items()}
    time.sleep(3.0 if case.disconnect else 1.0)  # usage is published after the response ends
    for s, p in SERVERS.items():
        res[s]["usage"] = pop_usage(p)
    return res


def cases():
    msg = [{"role": "user", "content": "Reply with exactly the word: pong"}]
    weather_tool_claude = [{
        "name": "get_weather", "description": "Get weather for a city",
        "input_schema": {"type": "object", "properties": {"city": {"type": "string"}},
                         "required": ["city"]}}]
    weather_tool_oai = [{"type": "function", "function": {
        "name": "get_weather", "description": "Get weather for a city",
        "parameters": {"type": "object", "properties": {"city": {"type": "string"}},
                       "required": ["city"]}}}]
    weather_tool_resp = [{"type": "function", "name": "get_weather",
                          "description": "Get weather for a city",
                          "parameters": {"type": "object",
                                         "properties": {"city": {"type": "string"}},
                                         "required": ["city"]}}]
    ask_weather = [{"role": "user", "content": "What's the weather in Paris? Use the tool."}]
    out = [
        Case("models", "/v1/models", method="GET", mode="models"),
        Case("err-bad-key", "/v1/chat/completions", {"model": CLAUDE, "messages": msg},
             headers={"Authorization": "Bearer wrong"}, mode="exact"),
        Case("err-unknown-model", "/v1/chat/completions",
             {"model": "no-such-model", "messages": msg}, mode="exact"),
        Case("err-malformed-json", "/v1/chat/completions", raw_body="{not json", mode="exact"),
        Case("err-messages-unknown-model", "/v1/messages",
             {"model": "no-such-model", "max_tokens": 10, "messages": msg}, mode="exact"),
        Case("count-tokens-claude", "/v1/messages/count_tokens",
             {"model": CLAUDE, "messages": msg}, mode="exact"),
    ]
    for label, model in (("claude", CLAUDE), ("muse", MUSE)):
        out += [
            Case(f"messages-{label}", "/v1/messages",
                 {"model": model, "max_tokens": 64, "messages": msg}),
            Case(f"messages-{label}-stream", "/v1/messages",
                 {"model": model, "max_tokens": 64, "messages": msg, "stream": True}, stream=True),
            Case(f"messages-{label}-tool", "/v1/messages",
                 {"model": model, "max_tokens": 256, "messages": ask_weather,
                  "tools": weather_tool_claude,
                  "tool_choice": {"type": "tool", "name": "get_weather"}}),
            Case(f"messages-{label}-tool-stream", "/v1/messages",
                 {"model": model, "max_tokens": 256, "messages": ask_weather,
                  "tools": weather_tool_claude, "stream": True,
                  "tool_choice": {"type": "tool", "name": "get_weather"}}, stream=True),
            Case(f"chat-{label}", "/v1/chat/completions",
                 {"model": model, "max_tokens": 64, "messages": msg}),
            Case(f"chat-{label}-stream", "/v1/chat/completions",
                 {"model": model, "max_tokens": 64, "messages": msg, "stream": True,
                  "stream_options": {"include_usage": True}}, stream=True),
            Case(f"chat-{label}-tool", "/v1/chat/completions",
                 {"model": model, "max_tokens": 256, "messages": ask_weather,
                  "tools": weather_tool_oai,
                  "tool_choice": {"type": "function", "function": {"name": "get_weather"}}}),
            Case(f"chat-{label}-tool-stream", "/v1/chat/completions",
                 {"model": model, "max_tokens": 256, "messages": ask_weather,
                  "tools": weather_tool_oai, "stream": True,
                  "tool_choice": {"type": "function", "function": {"name": "get_weather"}}},
                 stream=True),
            Case(f"responses-{label}", "/v1/responses",
                 {"model": model, "max_output_tokens": 64, "input": msg}),
            Case(f"responses-{label}-stream", "/v1/responses",
                 {"model": model, "max_output_tokens": 64, "input": msg, "stream": True},
                 stream=True),
            Case(f"responses-{label}-tool", "/v1/responses",
                 {"model": model, "max_output_tokens": 256, "input": ask_weather,
                  "tools": weather_tool_resp,
                  "tool_choice": {"type": "function", "name": "get_weather"}}),
            Case(f"responses-{label}-ws", "/v1/responses",
                 {"type": "response.create", "model": model, "max_output_tokens": 64,
                  "input": msg}, ws=True),
            Case(f"gemini-{label}", f"/v1beta/models/{model}:generateContent",
                 {"contents": [{"role": "user", "parts": [{"text": msg[0]["content"]}]}],
                  "generationConfig": {"maxOutputTokens": 64}}),
            Case(f"gemini-{label}-stream", f"/v1beta/models/{model}:streamGenerateContent?alt=sse",
                 {"contents": [{"role": "user", "parts": [{"text": msg[0]["content"]}]}],
                  "generationConfig": {"maxOutputTokens": 64}}, stream=True),
        ]
    count = [{"role": "user", "content": "Count from 1 to 300, one number per line."}]
    out += [
        Case("disconnect-messages-claude-stream", "/v1/messages",
             {"model": CLAUDE, "max_tokens": 800, "stream": True, "messages": count},
             stream=True, disconnect=True),
        Case("disconnect-chat-muse-stream", "/v1/chat/completions",
             {"model": MUSE, "max_tokens": 800, "stream": True, "messages": count},
             stream=True, disconnect=True),
    ]
    out.append(Case("messages-claude-thinking-stream", "/v1/messages",
                    {"model": CLAUDE, "max_tokens": 2048, "stream": True,
                     "thinking": {"type": "enabled", "budget_tokens": 1024},
                     "messages": [{"role": "user", "content": "What is 17*23? Answer briefly."}]},
                    stream=True))
    return out


def udiff(a, b):
    return "\n".join(difflib.unified_diff(
        json.dumps(a, indent=1, sort_keys=True).splitlines(),
        json.dumps(b, indent=1, sort_keys=True).splitlines(), "go", "rust", lineterm="", n=2))


def cmd_api(args, outdir: Path):
    rows, diffs = [], []
    for case in cases():
        if args.filter and args.filter not in case.name:
            continue
        res = run_pair(case)
        v = {s: view(case, r) for s, r in res.items()}
        verdict = "same"
        if v["go"] != v["rust"]:
            first = udiff(v["go"], v["rust"])
            res = run_pair(case)
            v = {s: view(case, r) for s, r in res.items()}
            if v["go"] == v["rust"]:
                verdict = "same (retry)"
            else:
                verdict = "DIFF"
                second = udiff(v["go"], v["rust"])
                diffs.append((case.name, second, first != second))
        (outdir / f"{case.name}.json").write_text(json.dumps(res, indent=1))
        texts = {s: final_text(res[s]["body"]) if res[s]["status"] < 300 else "" for s in res}
        rows.append((case.name, res["go"]["status"], res["rust"]["status"], verdict,
                     res["go"]["ms"], res["rust"]["ms"], texts["go"], texts["rust"]))
        print(f"{verdict:13} {case.name:36} go={res['go']['status']} rust={res['rust']['status']}"
              f"  {res['go']['ms']}ms/{res['rust']['ms']}ms", flush=True)

    lines = ["# Live Go vs Rust differential (real upstreams)", "",
             f"Go `{(ROOT / 'tmp/GO_REF').read_text().strip() if (ROOT / 'tmp/GO_REF').exists() else '?'}`"
             f", Rust `{git_head()}`, {time.strftime('%Y-%m-%d %H:%M')}", "",
             "| case | go | rust | result | go ms | rust ms | go text | rust text |",
             "|---|---|---|---|---|---|---|---|"]
    for r in rows:
        t = [str(x).replace("|", "\\|").replace("\n", " ")[:60] for x in r]
        lines.append("| " + " | ".join(t) + " |")
    same = sum(1 for r in rows if r[3] != "DIFF")
    lines += ["", f"**{same} / {len(rows)} same**", ""]
    for name, d, flapped in diffs:
        lines += [f"## {name}" + (" (diff changed between attempts)" if flapped else ""), "",
                  "```diff", d, "```", ""]
    (outdir / "report.md").write_text("\n".join(lines))
    print(f"\n{same}/{len(rows)} same; report: {outdir / 'report.md'}")


FIB = "0 1 1 2 3 5 8 13 21 34"
TASK = ("Create a file fib.py that prints the first 10 Fibonacci numbers starting at 0, "
        "separated by single spaces, on one line. Run it with python3. "
        "Then reply with only the program's output.")


def run_agent(kind: str, server: str, model: str, outdir: Path):
    port = SERVERS[server]
    base = STATE / "agents" / f"{kind}-{server}-{model}"
    shutil.rmtree(base, ignore_errors=True)
    work = base / "work"
    work.mkdir(parents=True)
    env = {k: v for k, v in os.environ.items()
           if not k.startswith(("ANTHROPIC_", "OPENAI_", "CLAUDE_CODE_"))}
    if kind == "claude":
        env.update(ANTHROPIC_BASE_URL=f"http://127.0.0.1:{port}", ANTHROPIC_API_KEY=KEY,
                   CLAUDE_CONFIG_DIR=str(base / "config"))
        cmd = ["claude", "-p", TASK, "--model", model, "--output-format", "json",
               "--allowedTools", "Write", "Bash(python3:*)"]
    else:
        home = base / "codex-home"
        home.mkdir()
        (home / "config.toml").write_text(
            f'model = "{model}"\nmodel_provider = "cpa"\n\n[model_providers.cpa]\n'
            f'name = "cpa"\nbase_url = "http://127.0.0.1:{port}/v1"\n'
            f'env_key = "CPA_KEY"\nwire_api = "responses"\n')
        env.update(CODEX_HOME=str(home), CPA_KEY=KEY)
        cmd = ["codex", "exec", "--skip-git-repo-check", "--sandbox", "workspace-write",
               "-C", str(work), TASK]
    t0 = time.monotonic()
    try:
        p = subprocess.run(cmd, cwd=work, env=env, capture_output=True, text=True, timeout=600,
                           stdin=subprocess.DEVNULL)
        out, err, rc = p.stdout, p.stderr, p.returncode
    except subprocess.TimeoutExpired as e:
        out, err, rc = e.stdout or "", (e.stderr or "") + "\nTIMEOUT", -1
    secs = time.monotonic() - t0
    script = work / "fib.py"
    ran = ""
    if script.exists():
        ran = subprocess.run(["python3", str(script)], capture_output=True, text=True,
                             timeout=30).stdout.strip()
    ok = rc == 0 and ran == FIB and FIB in out
    usage = pop_usage(port)
    name = f"agent-{kind}-{server}-{model}"
    (outdir / f"{name}.log").write_text(
        f"$ {' '.join(cmd)}\nrc={rc} secs={secs:.1f}\n--- stdout\n{out}\n--- stderr\n{err}\n"
        f"--- fib.py output\n{ran}\n--- usage records: {len(usage)}\n"
        + json.dumps([shape(u) for u in usage], indent=1))
    return {"name": name, "ok": ok, "rc": rc, "secs": round(secs, 1), "file": script.exists(),
            "output_ok": ran == FIB, "requests": len(usage),
            "failed_requests": sum(1 for u in usage if u.get("failed"))}


def cmd_agents(args, outdir: Path):
    rows = []
    for kind in ("claude", "codex"):
        for model in (CLAUDE, MUSE):
            for server in SERVERS:
                if args.filter and args.filter not in f"{kind}-{server}-{model}":
                    continue
                r = run_agent(kind, server, model, outdir)
                rows.append(r)
                print(f"{'PASS' if r['ok'] else 'FAIL'}  {r['name']:52} rc={r['rc']} "
                      f"{r['secs']}s requests={r['requests']} failed={r['failed_requests']}",
                      flush=True)
    lines = ["# Live agent sessions through each proxy", "", f"Task: {TASK}", "",
             "| session | result | rc | secs | fib.py correct | proxied requests | failed |",
             "|---|---|---|---|---|---|---|"]
    for r in rows:
        lines.append(f"| {r['name']} | {'PASS' if r['ok'] else 'FAIL'} | {r['rc']} | {r['secs']} "
                     f"| {r['output_ok']} | {r['requests']} | {r['failed_requests']} |")
    (outdir / "agents.md").write_text("\n".join(lines) + "\n")
    print(f"\nreport: {outdir / 'agents.md'}")


def git_head():
    try:
        return subprocess.run(["git", "-C", str(ROOT), "rev-parse", "--short", "HEAD"],
                              capture_output=True, text=True).stdout.strip()
    except OSError:
        return "?"


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawTextHelpFormatter)
    ap.add_argument("what", choices=["api", "agents"])
    ap.add_argument("--filter")
    args = ap.parse_args()
    outdir = STATE / "results" / time.strftime("%Y%m%d-%H%M%S")
    outdir.mkdir(parents=True)
    (cmd_api if args.what == "api" else cmd_agents)(args, outdir)


if __name__ == "__main__":
    sys.exit(main())
