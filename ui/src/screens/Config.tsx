import { useQuery, useQueryClient } from "@tanstack/react-query";
import { useEffect, useMemo, useRef, useState } from "react";
import { ApiError, api } from "@/lib/api";
import { toast } from "@/lib/toast";
import { confirm } from "@/ui/overlays";
import { Button, ErrorState, LoadingRows, PageHeader, StatusDot } from "@/ui/primitives";
import { YamlEditor, yamlProblems, type CursorInfo } from "@/ui/YamlEditor";
import { MOD } from "@/lib/format";

interface Problem {
  line?: number;
  message: string;
  source: "yaml" | "server";
}

/** Servers phrase locations as "line 12" or "yaml: line 12:"; pick the first one. */
function lineFromMessage(message: string): number | undefined {
  const m = /line (\d+)/i.exec(message);
  return m?.[1] ? Number(m[1]) : undefined;
}

export default function Config() {
  const q = useQuery({
    queryKey: ["config-yaml"],
    queryFn: () => api.getText("/config.yaml"),
    staleTime: Infinity,
    refetchOnWindowFocus: false,
  });
  const qc = useQueryClient();
  const [version, setVersion] = useState(0);

  if (q.isLoading) return (
    <>
      <PageHeader title="Config" />
      <LoadingRows rows={10} />
    </>
  );
  if (q.isError || q.data === undefined) return (
    <>
      <PageHeader title="Config" />
      <ErrorState error={q.error} onRetry={() => void q.refetch()} />
    </>
  );
  return (
    <Editor
      key={version}
      saved={q.data}
      onReload={async () => {
        await q.refetch();
        setVersion((v) => v + 1);
        // Everything else may have changed with the file.
        void qc.invalidateQueries({ predicate: (query) => query.queryKey[0] !== "config-yaml" });
      }}
    />
  );
}

function Editor({ saved, onReload }: { saved: string; onReload: () => Promise<void> }) {
  const [text, setText] = useState(saved);
  const [cursor, setCursor] = useState<CursorInfo>({ line: 1, col: 1, lines: saved.split("\n").length });
  const [serverError, setServerError] = useState<string | null>(null);
  const [saving, setSaving] = useState(false);
  const [jump, setJump] = useState<{ line: number; nonce: number }>();
  const textRef = useRef(text);
  textRef.current = text;

  const dirty = text !== saved;
  const dirtyRef = useRef(dirty);
  dirtyRef.current = dirty;

  const problems = useMemo<Problem[]>(() => {
    const list: Problem[] = yamlProblems(text).map((p) => ({ line: p.line, message: p.message, source: "yaml" }));
    if (serverError) list.push({ line: lineFromMessage(serverError), message: serverError, source: "server" });
    return list;
  }, [text, serverError]);
  const syntaxErrors = problems.filter((p) => p.source === "yaml").length;

  useEffect(() => {
    const warn = (e: BeforeUnloadEvent) => {
      if (dirtyRef.current) e.preventDefault();
    };
    window.addEventListener("beforeunload", warn);
    return () => window.removeEventListener("beforeunload", warn);
  }, []);

  async function save() {
    if (!dirtyRef.current) return;
    const current = textRef.current;
    if (yamlProblems(current).length > 0) {
      toast.error("Fix YAML errors before saving");
      return;
    }
    setSaving(true);
    setServerError(null);
    try {
      await api.putYaml("/config.yaml", current);
      toast.ok("Config saved");
      await onReload();
    } catch (e) {
      const message = e instanceof ApiError ? e.message : String(e);
      setServerError(message);
      const line = lineFromMessage(message);
      if (line !== undefined) setJump({ line, nonce: Date.now() });
      setSaving(false);
    }
  }

  // Ctrl/Cmd+S also works outside the editor, e.g. with focus on the toolbar.
  const saveRef = useRef(save);
  saveRef.current = save;
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if ((e.metaKey || e.ctrlKey) && e.key.toLowerCase() === "s") {
        e.preventDefault();
        void saveRef.current();
      }
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, []);

  async function reload() {
    if (dirty && !(await confirm({ title: "Discard changes", body: "Reload the config from the server and drop local edits.", confirm: "Discard", danger: true }))) return;
    await onReload();
  }

  return (
    <>
      <PageHeader title="Config">
        {dirty && (
          <span className="mr-1 flex items-center gap-2 text-[12px] text-muted">
            <StatusDot tone="warn" />
            Modified
          </span>
        )}
        <Button icon="refresh" onClick={() => void reload()}>
          Reload
        </Button>
        <Button variant="primary" disabled={!dirty || saving || syntaxErrors > 0} onClick={() => void save()} kbd={`${MOD}S`}>
          {saving ? "Saving" : "Save"}
        </Button>
      </PageHeader>
      <div className="min-h-0 flex-1">
        <YamlEditor initial={saved} onChange={setText} onCursor={setCursor} onSave={() => void save()} jumpTo={jump} />
      </div>
      {problems.length > 0 && (
        <ul className="max-h-36 shrink-0 overflow-y-auto border-t border-line">
          {problems.map((p, i) => (
            <li key={i}>
              <button
                disabled={p.line === undefined}
                onClick={() => p.line !== undefined && setJump({ line: p.line, nonce: Date.now() })}
                className="flex min-h-7 w-full items-start gap-2.5 px-5 py-1 text-left text-[12.5px] transition-colors enabled:hover:bg-hover"
              >
                <StatusDot tone="bad" className="mt-[6px]" />
                <span className="mono shrink-0 text-muted">{p.line !== undefined ? `L${p.line}` : p.source}</span>
                <span className="min-w-0 whitespace-pre-wrap break-words text-fg-2">{p.message}</span>
              </button>
            </li>
          ))}
        </ul>
      )}
      <footer className="num flex h-7 shrink-0 items-center justify-between border-t border-line px-5 text-[12px] text-muted">
        <span className="flex items-center gap-2">
          <StatusDot tone={syntaxErrors > 0 ? "bad" : "ok"} />
          {syntaxErrors > 0 ? `${syntaxErrors} syntax ${syntaxErrors === 1 ? "error" : "errors"}` : "Valid YAML"}
        </span>
        <span>
          Ln {cursor.line}, Col {cursor.col} <span className="ml-3 text-faint">{cursor.lines} lines</span>
        </span>
      </footer>
    </>
  );
}
