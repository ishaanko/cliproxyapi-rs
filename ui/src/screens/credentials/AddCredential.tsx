import { useQueryClient } from "@tanstack/react-query";
import { useCallback, useEffect, useRef, useState, type DragEvent, type KeyboardEvent } from "react";
import { ApiError, api } from "@/lib/api";
import { fmtDuration } from "@/lib/format";
import { qk, useOAuthPlugins } from "@/lib/queries";
import type { OAuthStart, OAuthStatus } from "@/lib/types";
import { toast } from "@/lib/toast";
import { Dialog } from "@/ui/overlays";
import { Button, CopyButton, Input, StatusDot, cx } from "@/ui/primitives";
import { Icon } from "@/ui/icons";

interface Provider {
  id: string;
  label: string;
  provider: string;
  flow: "browser" | "device";
  query?: Record<string, string>;
  webui?: boolean;
}

// Built-in login providers of the v8 `/oauth/auth-url?provider=` endpoint.
const BUILTIN: Provider[] = [
  { id: "claude", label: "Claude", provider: "claude", flow: "browser", webui: true },
  { id: "codex", label: "Codex", provider: "codex", flow: "browser", webui: true },
  { id: "codex-device", label: "Codex, device code", provider: "codex", flow: "device", query: { flow: "device" } },
  { id: "antigravity", label: "Antigravity", provider: "antigravity", flow: "browser", webui: true },
  { id: "kimi", label: "Kimi", provider: "kimi", flow: "device" },
  { id: "kimi-ai", label: "Kimi.ai", provider: "kimi-ai", flow: "device" },
  { id: "xai", label: "xAI", provider: "xai", flow: "device" },
  { id: "devin", label: "Devin", provider: "devin", flow: "browser" },
  { id: "meta", label: "Meta", provider: "meta", flow: "device" },
];

type Step = { kind: "pick" } | { kind: "oauth"; provider: Provider } | { kind: "files" };

export function AddCredential({ onClose }: { onClose: () => void }) {
  const [step, setStep] = useState<Step>({ kind: "pick" });
  const plugins = useOAuthPlugins();
  const providers: Provider[] = [
    ...BUILTIN,
    ...(plugins.data ?? []).map((p): Provider => ({ id: `plugin-${p.id}`, label: p.name, provider: p.id, flow: "browser" })),
  ];

  if (step.kind === "oauth") {
    return <OAuthStep provider={step.provider} onClose={onClose} onBack={() => setStep({ kind: "pick" })} />;
  }
  if (step.kind === "files") return <FilesStep onClose={onClose} onBack={() => setStep({ kind: "pick" })} />;

  return (
    <Dialog title="Add credential" onClose={onClose} width={440}>
      <PickList
        providers={providers}
        onOAuth={(provider) => setStep({ kind: "oauth", provider })}
        onFiles={() => setStep({ kind: "files" })}
      />
    </Dialog>
  );
}

function PickList({ providers, onOAuth, onFiles }: { providers: Provider[]; onOAuth: (p: Provider) => void; onFiles: () => void }) {
  const root = useRef<HTMLDivElement>(null);
  const move = (e: KeyboardEvent) => {
    if (e.key !== "ArrowDown" && e.key !== "ArrowUp" && e.key !== "j" && e.key !== "k") return;
    const opts = [...(root.current?.querySelectorAll<HTMLButtonElement>("[data-opt]") ?? [])];
    const i = opts.indexOf(document.activeElement as HTMLButtonElement);
    const next = e.key === "ArrowDown" || e.key === "j" ? Math.min(opts.length - 1, i + 1) : Math.max(0, i - 1);
    e.preventDefault();
    opts[next]?.focus();
  };
  const row = "flex h-9 w-full items-center justify-between px-5 text-left text-[13px] transition-colors duration-100 hover:bg-hover focus-visible:bg-active focus-visible:outline-none";
  return (
    <div ref={root} onKeyDown={move} className="py-1">
      <div className="px-5 pt-2 pb-1 text-[11.5px] text-faint">Sign in</div>
      {providers.map((p, i) => (
        <button key={p.id} data-opt data-af={i === 0 ? true : undefined} className={row} onClick={() => onOAuth(p)}>
          <span>{p.label}</span>
          <span className="text-[12px] text-muted">{p.flow === "device" ? "Device code" : "Browser"}</span>
        </button>
      ))}
      <div className="px-5 pt-3 pb-1 text-[11.5px] text-faint">Files</div>
      <button data-opt className={row} onClick={onFiles}>
        <span>Upload auth JSON or Vertex service account</span>
        <Icon name="upload" size={13} className="text-muted" />
      </button>
    </div>
  );
}

// OAuth login: start, show the URL or device code, poll status, accept a pasted callback.

function OAuthStep({ provider, onClose, onBack }: { provider: Provider; onClose: () => void; onBack: () => void }) {
  const qc = useQueryClient();
  const [start, setStart] = useState<OAuthStart | null>(null);
  const [startError, setStartError] = useState<string | null>(null);
  const [status, setStatus] = useState<OAuthStatus["status"]>("wait");
  const [failure, setFailure] = useState<string | null>(null);
  const [pasted, setPasted] = useState("");
  const [submitting, setSubmitting] = useState(false);
  const stateRef = useRef<string | null>(null);
  const finished = useRef(false);

  useEffect(() => {
    let cancelled = false;
    api
      .get<OAuthStart>("/oauth/auth-url", {
        provider: provider.provider,
        ...provider.query,
        ...(provider.webui ? { is_webui: "true" } : {}),
      })
      .then((res) => {
        if (cancelled) {
          void api.del("/oauth/session", { state: res.state }).catch(() => undefined);
          return;
        }
        stateRef.current = res.state;
        setStart(res);
      })
      .catch((e: unknown) => !cancelled && setStartError(e instanceof Error ? e.message : String(e)));
    return () => {
      cancelled = true;
    };
  }, [provider]);

  // Poll until the server reports ok or error.
  useEffect(() => {
    if (!start) return;
    const timer = setInterval(() => {
      api
        .get<OAuthStatus>("/oauth/status", { state: start.state })
        .then((s) => {
          if (finished.current) return;
          if (s.status === "ok") {
            finished.current = true;
            setStatus("ok");
            toast.ok(`${provider.label} credential added`);
            void qc.invalidateQueries({ queryKey: qk.credentials });
          } else if (s.status === "error") {
            finished.current = true;
            setStatus("error");
            setFailure(s.error ?? "Authentication failed");
          }
        })
        .catch(() => undefined);
    }, 1500);
    return () => clearInterval(timer);
  }, [start, provider.label, qc]);

  // Abandoning a pending login releases its callback listener on the server.
  const cancelPending = useCallback(() => {
    if (stateRef.current && !finished.current) void api.del("/oauth/session", { state: stateRef.current }).catch(() => undefined);
  }, []);
  useEffect(() => cancelPending, [cancelPending]);

  const close = () => {
    cancelPending();
    finished.current = true;
    onClose();
  };

  async function submitCallback() {
    if (!start || !pasted.trim()) return;
    setSubmitting(true);
    const value = pasted.trim();
    const body = /^https?:\/\//.test(value) ? { state: start.state, redirect_url: value } : { state: start.state, code: value };
    try {
      await api.post("/oauth/callback", body);
      setPasted("");
    } catch (e) {
      toast.error(e instanceof ApiError ? e.message : "Callback rejected");
    } finally {
      setSubmitting(false);
    }
  }

  const device = start?.flow === "device";
  return (
    <Dialog
      title={provider.label}
      onClose={close}
      width={480}
      footer={
        status === "ok" ? (
          <Button variant="primary" onClick={close}>
            Done
          </Button>
        ) : (
          <>
            <Button onClick={onBack}>Back</Button>
            <Button onClick={close}>Cancel</Button>
          </>
        )
      }
    >
      <div className="grid grid-cols-[minmax(0,1fr)] gap-5 px-5 py-5">
        {startError && (
          <div className="grid grid-cols-[minmax(0,1fr)] gap-2">
            <div className="flex items-center gap-2 text-bad">
              <StatusDot tone="bad" />
              Could not start login
            </div>
            <div className="mono text-[12px] break-words text-muted">{startError}</div>
          </div>
        )}
        {!start && !startError && <div className="text-muted">Starting</div>}
        {start && (
          <>
            {device && start.user_code && (
              <div className="grid grid-cols-[minmax(0,1fr)] gap-2">
                <div className="text-[12px] text-muted">Enter this code</div>
                <div className="flex items-center gap-2">
                  <span className="mono text-[28px] leading-none font-medium tracking-[0.08em]">{start.user_code}</span>
                  <CopyButton text={start.user_code} label="Copy code" />
                </div>
              </div>
            )}
            <div className="grid grid-cols-[minmax(0,1fr)] gap-2">
              <div className="text-[12px] text-muted">{device && start.user_code ? "At" : "Open"}</div>
              <div className="flex items-center gap-1 rounded-md border border-line-strong py-1 pr-1 pl-2.5">
                <span className="mono min-w-0 flex-1 truncate text-[12px] text-fg-2" title={start.url}>
                  {start.url}
                </span>
                <CopyButton text={start.url} label="Copy URL" />
                <a
                  href={start.url}
                  target="_blank"
                  rel="noreferrer"
                  className="inline-flex h-7 items-center gap-1.5 rounded-md bg-white px-2.5 text-[13px] font-medium text-black transition-colors hover:bg-[#e6e6e6]"
                >
                  Open
                  <Icon name="external" size={12} />
                </a>
              </div>
            </div>
            {!device && status === "wait" && (
              <div className="grid grid-cols-[minmax(0,1fr)] gap-2">
                <div className="text-[12px] text-muted">If the final page does not load, paste its URL</div>
                <div className="flex gap-2">
                  <Input
                    value={pasted}
                    onChange={(e) => setPasted(e.target.value)}
                    onKeyDown={(e) => e.key === "Enter" && void submitCallback()}
                    placeholder="http://localhost:1455/auth/callback?code=..."
                    className="mono text-[12px]"
                  />
                  <Button onClick={() => void submitCallback()} disabled={submitting || !pasted.trim()}>
                    Submit
                  </Button>
                </div>
              </div>
            )}
            <LoginStatus status={status} failure={failure} expiresIn={start.expires_in} />
          </>
        )}
      </div>
    </Dialog>
  );
}

function LoginStatus({ status, failure, expiresIn }: { status: OAuthStatus["status"]; failure: string | null; expiresIn?: number }) {
  const [left, setLeft] = useState(expiresIn ?? null);
  useEffect(() => {
    if (expiresIn == null || status !== "wait") return;
    const started = Date.now();
    const t = setInterval(() => setLeft(Math.max(0, expiresIn - Math.round((Date.now() - started) / 1000))), 1000);
    return () => clearInterval(t);
  }, [expiresIn, status]);

  if (status === "ok") {
    return (
      <div className="flex items-center gap-2 text-ok" role="status">
        <StatusDot tone="ok" />
        Signed in, credential saved
      </div>
    );
  }
  if (status === "error") {
    return (
      <div className="grid gap-1" role="alert">
        <div className="flex items-center gap-2 text-bad">
          <StatusDot tone="bad" />
          Login failed
        </div>
        <div className="mono text-[12px] break-words text-muted">{failure}</div>
      </div>
    );
  }
  return (
    <div className="flex items-center justify-between text-[12.5px] text-muted" role="status">
      <span className="flex items-center gap-2">
        <StatusDot tone="warn" />
        Waiting for sign in
      </span>
      {left !== null && <span className="num">expires in {fmtDuration(left)}</span>}
    </div>
  );
}

// File upload: auth JSON files and Vertex service accounts.

interface UploadResult {
  name: string;
  ok: boolean;
  message?: string;
}

function FilesStep({ onClose, onBack }: { onClose: () => void; onBack: () => void }) {
  const qc = useQueryClient();
  const [results, setResults] = useState<UploadResult[]>([]);
  const [busy, setBusy] = useState(false);
  const [drag, setDrag] = useState(false);
  const authInput = useRef<HTMLInputElement>(null);
  const vertexInput = useRef<HTMLInputElement>(null);

  async function upload(files: File[], kind: "auth" | "vertex") {
    if (files.length === 0) return;
    setBusy(true);
    const out: UploadResult[] = [];
    for (const file of files) {
      const form = new FormData();
      form.append("file", file, file.name);
      try {
        if (kind === "vertex") await api.postForm("/oauth/import", form, { provider: "vertex" });
        else await api.postForm("/credentials", form);
        out.push({ name: file.name, ok: true });
      } catch (e) {
        out.push({ name: file.name, ok: false, message: e instanceof Error ? e.message : String(e) });
      }
    }
    setResults((prev) => [...out, ...prev]);
    setBusy(false);
    void qc.invalidateQueries({ queryKey: qk.credentials });
    const okCount = out.filter((r) => r.ok).length;
    if (okCount > 0) toast.ok(`${okCount} uploaded`);
  }

  const onDrop = (e: DragEvent) => {
    e.preventDefault();
    setDrag(false);
    void upload([...e.dataTransfer.files].filter((f) => f.name.endsWith(".json")), "auth");
  };

  return (
    <Dialog
      title="Upload credentials"
      onClose={onClose}
      width={480}
      footer={
        <>
          <Button onClick={onBack}>Back</Button>
          <Button variant="primary" onClick={onClose}>
            Done
          </Button>
        </>
      }
    >
      <div className="grid grid-cols-[minmax(0,1fr)] gap-4 px-5 py-5">
        <div
          onDragOver={(e) => {
            e.preventDefault();
            setDrag(true);
          }}
          onDragLeave={() => setDrag(false)}
          onDrop={onDrop}
          className={cx(
            "flex h-28 flex-col items-center justify-center gap-2 rounded-md border border-dashed text-[13px] transition-colors duration-100",
            drag ? "border-white bg-hover" : "border-line-strong",
          )}
        >
          <span className="text-muted">Drop .json files here</span>
          <div className="flex gap-2">
            <Button icon="upload" disabled={busy} onClick={() => authInput.current?.click()}>
              Auth JSON
            </Button>
            <Button icon="upload" disabled={busy} onClick={() => vertexInput.current?.click()}>
              Vertex service account
            </Button>
          </div>
          <input
            ref={authInput}
            type="file"
            accept=".json,application/json"
            multiple
            hidden
            onChange={(e) => {
              void upload([...(e.target.files ?? [])], "auth");
              e.target.value = "";
            }}
          />
          <input
            ref={vertexInput}
            type="file"
            accept=".json,application/json"
            hidden
            onChange={(e) => {
              void upload([...(e.target.files ?? [])], "vertex");
              e.target.value = "";
            }}
          />
        </div>
        {results.length > 0 && (
          <ul>
            {results.map((r, i) => (
              <li key={i} className="flex min-h-8 items-start gap-2 border-b border-line py-1.5 last:border-0">
                <StatusDot tone={r.ok ? "ok" : "bad"} className="mt-[6px]" />
                <span className="mono text-[12px]">{r.name}</span>
                {r.message && <span className="ml-auto max-w-[55%] text-right text-[12px] break-words text-bad">{r.message}</span>}
              </li>
            ))}
          </ul>
        )}
      </div>
    </Dialog>
  );
}
