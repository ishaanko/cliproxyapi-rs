import { useQuery, useQueryClient } from "@tanstack/react-query";
import { useEffect, useMemo, useRef, useState } from "react";
import { ApiError, api, saveBlob } from "@/lib/api";
import { NONE, clock, dateTime, fmtBytes, fmtCompact, fmtMs, maskKey, relTime } from "@/lib/format";
import { useRowNav } from "@/lib/hotkeys";
import { useRequestFeed } from "@/lib/queries";
import { go, href } from "@/lib/route";
import { errorText, toast } from "@/lib/toast";
import type { ErrorLogFile, LogsResponse, UsageEvent } from "@/lib/types";
import { Sheet, confirm } from "@/ui/overlays";
import {
  Button,
  EmptyState,
  ErrorState,
  IconButton,
  Input,
  KeyValue,
  LoadingRows,
  PageHeader,
  SearchInput,
  StatusDot,
  Switch,
  Tabs,
  cx,
} from "@/ui/primitives";

type Tab = "app" | "requests" | "errors";
const isTab = (s: string | undefined): s is Tab => s === "app" || s === "requests" || s === "errors";

export default function Logs({ sub }: { sub: string[] }) {
  const tab: Tab = isTab(sub[0]) ? sub[0] : "app";
  return (
    <>
      <PageHeader title="Logs" />
      <div className="shrink-0 border-b border-line">
        <Tabs
          value={tab}
          onChange={(t) => go("logs", t)}
          items={[
            { id: "app", label: "Application" },
            { id: "requests", label: "Requests" },
            { id: "errors", label: "Error files" },
          ]}
        />
      </div>
      {tab === "app" && <AppLog />}
      {tab === "requests" && <RequestLog />}
      {tab === "errors" && <ErrorFiles />}
    </>
  );
}

// Application log

interface Line {
  id: number;
  raw: string;
  time?: string;
  level?: string;
  source?: string;
  message: string;
}

const LINE_RE = /^\[(\d{4}-\d\d-\d\d) (\d\d:\d\d:\d\d)\] \[([^\]]*)\] \[(\w+)\s*\] \[([^\]]*)\] (.*)$/;
const MAX_LINES = 2500;
let lineId = 0;

function parseLine(raw: string): Line {
  const m = LINE_RE.exec(raw);
  if (!m) return { id: lineId++, raw, message: raw };
  return { id: lineId++, raw, time: m[2], level: m[4]?.toLowerCase(), source: m[5], message: m[6] ?? "" };
}

const levelColor: Record<string, string> = { debug: "text-faint", info: "text-muted", warn: "text-warn", warning: "text-warn", error: "text-bad", fatal: "text-bad", panic: "text-bad" };
const LEVELS = ["all", "info", "warn", "error"] as const;
type LevelFilter = (typeof LEVELS)[number];

function AppLog() {
  const [lines, setLines] = useState<Line[]>([]);
  const [live, setLive] = useState(true);
  const [level, setLevel] = useState<LevelFilter>("all");
  const [search, setSearch] = useState("");
  const [state, setState] = useState<"loading" | "ready" | "disabled" | "error">("loading");
  const [error, setError] = useState<unknown>(null);
  const cursor = useRef<string | undefined>(undefined);
  const scroller = useRef<HTMLDivElement>(null);
  const follow = useRef(true);
  const qc = useQueryClient();

  useEffect(() => {
    let stopped = false;
    let timer: ReturnType<typeof setTimeout> | undefined;
    const tick = async () => {
      if (stopped) return;
      if (document.visibilityState === "visible") {
        try {
          const res = await api.get<LogsResponse>("/observability/logs", cursor.current ? { cursor: cursor.current } : { limit: 500 });
          if (stopped) return;
          cursor.current = res["next-cursor"] || cursor.current;
          const fresh = res.lines.map(parseLine);
          setState("ready");
          if (res["cursor-reset"] || fresh.length > 0) {
            setLines((prev) => (res["cursor-reset"] ? fresh : [...prev, ...fresh].slice(-MAX_LINES)));
          }
        } catch (e) {
          if (stopped) return;
          if (e instanceof ApiError && e.status === 400) setState("disabled");
          else if (e instanceof ApiError && e.status === 503) setState("error");
          else {
            setState("error");
            setError(e);
          }
          setError(e);
        }
      }
      if (!stopped && live) timer = setTimeout(() => void tick(), 2000);
    };
    void tick();
    return () => {
      stopped = true;
      clearTimeout(timer);
    };
  }, [live]);

  const shown = useMemo(() => {
    const needle = search.trim().toLowerCase();
    const rank = { all: 0, info: 1, warn: 2, error: 3 }[level];
    return lines.filter((l) => {
      if (rank > 0) {
        const lv = l.level ?? "info";
        const r = lv.startsWith("err") || lv === "fatal" || lv === "panic" ? 3 : lv.startsWith("warn") ? 2 : lv === "debug" ? 0 : 1;
        if (r < rank) return false;
      }
      return !needle || l.raw.toLowerCase().includes(needle);
    });
  }, [lines, level, search]);

  // Stay pinned to the newest line unless the user scrolled up.
  useEffect(() => {
    const el = scroller.current;
    if (el && follow.current) el.scrollTop = el.scrollHeight;
  }, [shown]);

  async function clear() {
    const ok = await confirm({ title: "Clear logs", body: "Truncates main.log and deletes rotated files.", confirm: "Clear", danger: true });
    if (!ok) return;
    try {
      await api.del("/observability/logs");
      cursor.current = undefined;
      setLines([]);
      toast.ok("Logs cleared");
      void qc.invalidateQueries({ queryKey: ["logs"] });
    } catch (e) {
      toast.error(errorText(e));
    }
  }

  if (state === "disabled") {
    return (
      <EmptyState
        title="File logging is off"
        hint="Enable observability.logs.logging-to-file in the config to read logs here."
        action={
          <a href={href("config")} className="inline-flex h-7 items-center rounded-md border border-line-strong px-2.5 text-[13px] hover:bg-hover">
            Open config
          </a>
        }
      />
    );
  }
  if (state === "error" && lines.length === 0) return <ErrorState error={error} />;

  return (
    <>
      <div className="flex h-11 shrink-0 items-center gap-3 border-b border-line px-5">
        <SearchInput value={search} onChange={(e) => setSearch(e.target.value)} placeholder="Filter lines" aria-label="Filter log lines" className="w-60" />
        <div className="flex items-center gap-0.5">
          {LEVELS.map((l) => (
            <button
              key={l}
              onClick={() => setLevel(l)}
              className={cx("h-7 rounded-md px-2 text-[12.5px] capitalize transition-colors duration-100", level === l ? "bg-active text-fg" : "text-muted hover:text-fg")}
            >
              {l}
            </button>
          ))}
        </div>
        <div className="ml-auto flex items-center gap-3">
          <span className="num text-[12px] text-faint">{shown.length === lines.length ? `${lines.length} lines` : `${shown.length} of ${lines.length}`}</span>
          <label className="flex items-center gap-2 text-[12.5px] text-muted">
            Live
            <Switch checked={live} onChange={setLive} label="Live tail" />
          </label>
          <Button variant="danger" icon="trash" onClick={() => void clear()}>
            Clear
          </Button>
        </div>
      </div>
      <div
        ref={scroller}
        className="min-h-0 flex-1 overflow-auto py-1"
        onScroll={(e) => {
          const el = e.currentTarget;
          follow.current = el.scrollHeight - el.scrollTop - el.clientHeight < 40;
        }}
      >
        {state === "loading" && <LoadingRows rows={12} />}
        {state === "ready" && shown.length === 0 && <EmptyState title={lines.length ? "No matches" : "No log lines"} />}
        {shown.map((l) => (
          <div key={l.id} className="mono flex gap-4 px-5 text-[12px] leading-[20px] hover:bg-hover">
            <span className="w-[58px] shrink-0 text-faint">{l.time ?? ""}</span>
            <span className={cx("w-[44px] shrink-0 uppercase", levelColor[l.level ?? ""] ?? "text-muted")}>{l.level ?? ""}</span>
            <span className="hidden w-[150px] shrink-0 truncate text-faint lg:block">{l.source ?? ""}</span>
            <span className={cx("min-w-0 flex-1 break-all whitespace-pre-wrap", l.level ? "text-fg-2" : "pl-[1ch] text-muted")}>{l.message}</span>
          </div>
        ))}
      </div>
    </>
  );
}

// Request log (usage extension)

function RequestLog() {
  const feed = useRequestFeed(2000);
  const [search, setSearch] = useState("");
  const [failedOnly, setFailedOnly] = useState(false);
  const [open, setOpen] = useState<UsageEvent | null>(null);

  const rows = useMemo(() => {
    const needle = search.trim().toLowerCase();
    return (feed.data?.events ?? []).filter(
      (e) =>
        (!failedOnly || e.failed) &&
        (!needle || [e.model, e.alias, e.provider, e.source, e.endpoint, e.request_id, e.auth_index].some((s) => s?.toLowerCase().includes(needle))),
    );
  }, [feed.data, search, failedOnly]);
  const nav = useRowNav(rows, setOpen);

  if (feed.isLoading) return <LoadingRows rows={10} />;
  if (feed.data === null) {
    return (
      <EmptyState
        title="Request feed unavailable"
        hint="This server does not implement GET /v8/management/observability/requests. See ui/API_EXTENSIONS.md."
      />
    );
  }
  if (feed.isError) return <ErrorState error={feed.error} onRetry={() => void feed.refetch()} />;

  return (
    <>
      <div className="flex h-11 shrink-0 items-center gap-3 border-b border-line px-5">
        <SearchInput value={search} onChange={(e) => setSearch(e.target.value)} placeholder="Filter requests" aria-label="Filter requests" className="w-60" />
        <div className="flex items-center gap-0.5">
          {[false, true].map((f) => (
            <button
              key={String(f)}
              onClick={() => setFailedOnly(f)}
              className={cx("h-7 rounded-md px-2 text-[12.5px] transition-colors duration-100", failedOnly === f ? "bg-active text-fg" : "text-muted hover:text-fg")}
            >
              {f ? "Failed" : "All"}
            </button>
          ))}
        </div>
        <span className="num ml-auto text-[12px] text-faint">{rows.length} requests</span>
      </div>
      <div className="min-h-0 flex-1 overflow-auto">
        {rows.length === 0 ? (
          <EmptyState title={search || failedOnly ? "No matches" : "No requests yet"} />
        ) : (
          <table className="tbl">
            <thead>
              <tr>
                <th className="fit">Time</th>
                <th className="w-[20%]">Model</th>
                <th className="w-[20%]">Credential</th>
                <th className="hidden w-[16%] xl:table-cell">Endpoint</th>
                <th className="hidden w-[11%] lg:table-cell">Key</th>
                <th className="w-[12%] text-right">In / out</th>
                <th className="w-[8%] text-right">TTFT</th>
                <th className="w-[8%] text-right">Latency</th>
              </tr>
            </thead>
            <tbody>
              {rows.map((e, i) => (
                <tr key={e.seq} data-active={nav.index === i} data-row-index={i} className="clickable" onClick={() => setOpen(e)}>
                  <td className="fit num text-muted">
                    <span className="flex items-center gap-2">
                      <StatusDot tone={e.failed ? "bad" : "ok"} />
                      {clock(e.timestamp)}
                    </span>
                  </td>
                  <td className="mono text-[12px]">{e.alias || e.model}</td>
                  <td className="text-fg-2">
                    {e.provider} <span className="text-faint">{e.source && e.source !== e.provider ? e.source : ""}</span>
                  </td>
                  <td className="mono hidden text-[12px] text-muted xl:table-cell">{e.endpoint ?? NONE}</td>
                  <td className="mono hidden text-[12px] text-muted lg:table-cell">{e.api_key ? maskKey(e.api_key) : NONE}</td>
                  <td className="num text-right text-muted">{e.failed ? NONE : `${fmtCompact(e.tokens.input_tokens)} / ${fmtCompact(e.tokens.output_tokens)}`}</td>
                  <td className="num text-right text-muted">{fmtMs(e.ttft_ms)}</td>
                  <td className="num text-right text-muted">{fmtMs(e.latency_ms)}</td>
                </tr>
              ))}
            </tbody>
          </table>
        )}
      </div>
      {open && <RequestSheet event={open} onClose={() => setOpen(null)} />}
    </>
  );
}

function RequestSheet({ event: e, onClose }: { event: UsageEvent; onClose: () => void }) {
  return (
    <Sheet title={e.alias || e.model} onClose={onClose} width={640}>
      <div className="px-5 py-4">
        <KeyValue
          rows={[
            ["Result", <span key="r" className={cx("flex items-center gap-2", e.failed && "text-bad")}><StatusDot tone={e.failed ? "bad" : "ok"} />{e.failed ? "Failed" : "Success"}</span>],
            ["Time", `${dateTime(e.timestamp)}  (${relTime(e.timestamp)})`],
            ["Provider", e.provider],
            ["Credential", e.source || NONE],
            ["Endpoint", <span key="e" className="mono text-[12px]">{e.endpoint ?? NONE}</span>],
            ["Model", <span key="m" className="mono text-[12px]">{e.model}</span>],
            ["Tokens", <span key="t" className="num">{fmtCompact(e.tokens.input_tokens)} in, {fmtCompact(e.tokens.output_tokens)} out, {fmtCompact(e.tokens.cached_tokens)} cached</span>],
            ["Latency", <span key="l" className="num">{fmtMs(e.latency_ms)}  (first token {fmtMs(e.ttft_ms)})</span>],
            ["Stream", e.stream ? "yes" : "no"],
            ["Request id", <span key="i" className="mono text-[12px]">{e.request_id ?? NONE}</span>],
          ]}
        />
      </div>
      {e.request_id && <RequestFile id={e.request_id} />}
    </Sheet>
  );
}

function RequestFile({ id }: { id: string }) {
  const q = useQuery({
    queryKey: ["request-log", id],
    retry: false,
    queryFn: () => api.getText(`/observability/logs/requests/${encodeURIComponent(id)}`),
  });
  return (
    <div className="border-t border-line px-5 py-4">
      <h3 className="mb-2 text-[13px] font-medium">Request log</h3>
      {q.isLoading && <div className="text-muted">Loading</div>}
      {q.isError && <div className="text-[12.5px] text-muted">No log file for this request. Enable request-log to capture them.</div>}
      {q.data !== undefined && <pre className="mono max-h-[50dvh] overflow-auto rounded-md border border-line p-3 text-[12px] leading-[1.55] whitespace-pre-wrap text-fg-2">{q.data}</pre>}
    </div>
  );
}

// Error log files

function ErrorFiles() {
  const q = useQuery({
    queryKey: ["logs", "errors"],
    queryFn: async () => (await api.get<{ files: ErrorLogFile[] }>("/observability/logs/errors")).files,
    refetchInterval: 10_000,
  });
  const [view, setView] = useState<{ title: string; path: string } | null>(null);
  const [requestId, setRequestId] = useState("");
  const files = q.data ?? [];
  const nav = useRowNav(files, (f) => setView({ title: f.name, path: `/observability/logs/errors/${encodeURIComponent(f.name)}` }));

  return (
    <>
      <div className="flex h-11 shrink-0 items-center gap-2 border-b border-line px-5">
        <Input
          value={requestId}
          onChange={(e) => setRequestId(e.target.value)}
          onKeyDown={(e) => e.key === "Enter" && requestId.trim() && setView({ title: `Request ${requestId.trim()}`, path: `/observability/logs/requests/${encodeURIComponent(requestId.trim())}` })}
          placeholder="Open request log by id"
          aria-label="Request id"
          className="mono h-7 w-72 text-[12px]"
        />
        <Button disabled={!requestId.trim()} onClick={() => setView({ title: `Request ${requestId.trim()}`, path: `/observability/logs/requests/${encodeURIComponent(requestId.trim())}` })}>
          Open
        </Button>
      </div>
      <div className="min-h-0 flex-1 overflow-auto">
        {q.isLoading && <LoadingRows rows={5} />}
        {q.isError && <ErrorState error={q.error} onRetry={() => void q.refetch()} />}
        {q.isSuccess && files.length === 0 && <EmptyState title="No error files" hint="Failed requests are written here when request-log is off." />}
        {files.length > 0 && (
          <table className="tbl">
            <thead>
              <tr>
                <th>File</th>
                <th className="w-[12%] text-right">Size</th>
                <th className="w-[16%]">Modified</th>
                <th className="fit" />
              </tr>
            </thead>
            <tbody>
              {files.map((f, i) => (
                <tr key={f.name} data-active={nav.index === i} data-row-index={i} className="group clickable" onClick={() => setView({ title: f.name, path: `/observability/logs/errors/${encodeURIComponent(f.name)}` })}>
                  <td className="mono text-[12px]">{f.name}</td>
                  <td className="num text-right text-muted">{fmtBytes(f.size)}</td>
                  <td className="num text-muted">{relTime(f.modified * 1000)}</td>
                  <td className="fit" onClick={(e) => e.stopPropagation()}>
                    <IconButton
                      icon="download"
                      label="Download"
                      className="opacity-0 transition-opacity group-hover:opacity-100 focus-visible:opacity-100"
                      onClick={() => void api.getBlob(`/observability/logs/errors/${encodeURIComponent(f.name)}`).then((b) => saveBlob(b, f.name)).catch((e: unknown) => toast.error(errorText(e)))}
                    />
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
        )}
      </div>
      {view && <FileSheet title={view.title} path={view.path} onClose={() => setView(null)} />}
    </>
  );
}

function FileSheet({ title, path, onClose }: { title: string; path: string; onClose: () => void }) {
  const q = useQuery({ queryKey: ["log-file", path], retry: false, queryFn: () => api.getText(path) });
  return (
    <Sheet title={title} onClose={onClose} width={760}>
      <div className="p-5">
        {q.isLoading && <div className="text-muted">Loading</div>}
        {q.isError && <ErrorState error={q.error} />}
        {q.data !== undefined && <pre className="mono overflow-auto text-[12px] leading-[1.55] whitespace-pre-wrap text-fg-2">{q.data}</pre>}
      </div>
    </Sheet>
  );
}
