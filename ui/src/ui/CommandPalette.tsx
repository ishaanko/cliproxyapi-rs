import { useQueryClient } from "@tanstack/react-query";
import { useEffect, useMemo, useRef, useState } from "react";
import { clearKey } from "@/lib/auth";
import { qk } from "@/lib/queries";
import { NAV, go } from "@/lib/route";
import type { CredentialFile, ModelInfo } from "@/lib/types";
import { Kbd, cx } from "./primitives";

interface Item {
  id: string;
  group: string;
  label: string;
  hint?: string;
  keys?: string;
  run: () => void;
}

/** Subsequence match with a bonus for prefix and contiguous hits. Null when no match. */
function score(query: string, text: string): number | null {
  if (!query) return 0;
  const q = query.toLowerCase();
  const t = text.toLowerCase();
  const at = t.indexOf(q);
  if (at >= 0) return 1000 - at * 4 - (t.length - q.length);
  let ti = 0;
  let s = 0;
  for (const ch of q) {
    const found = t.indexOf(ch, ti);
    if (found < 0) return null;
    s += found === ti ? 6 : 1;
    ti = found + 1;
  }
  return s;
}

export function CommandPalette({ onClose, onHelp }: { onClose: () => void; onHelp: () => void }) {
  const qc = useQueryClient();
  const [query, setQuery] = useState("");
  const [active, setActive] = useState(0);
  const input = useRef<HTMLInputElement>(null);
  const list = useRef<HTMLDivElement>(null);

  const items = useMemo<Item[]>(() => {
    const out: Item[] = [];
    for (const p of NAV) out.push({ id: `go-${p.page}`, group: "Go to", label: p.label, keys: `g ${p.key}`, run: () => go(p.page) });
    out.push(
      { id: "add-credential", group: "Actions", label: "Add credential", keys: "login oauth upload", run: () => go("credentials", "add") },
      { id: "add-key", group: "Actions", label: "Add API key", keys: "client access", run: () => go("keys", "add") },
      { id: "add-provider", group: "Actions", label: "Add provider", keys: "claude codex gemini openai compatibility", run: () => go("providers", "claude", "add") },
      { id: "refresh", group: "Actions", label: "Refresh all data", run: () => void qc.invalidateQueries() },
      { id: "help", group: "Actions", label: "Keyboard shortcuts", keys: "?", run: onHelp },
      { id: "signout", group: "Actions", label: "Sign out", run: clearKey },
    );
    for (const c of qc.getQueryData<CredentialFile[]>(qk.credentials) ?? []) {
      out.push({
        id: `cred-${c.name}`,
        group: "Credentials",
        label: c.label || c.email || c.name,
        hint: c.provider,
        keys: c.name,
        run: () => go("credentials", c.name),
      });
    }
    const served = qc.getQueriesData<ModelInfo[]>({ queryKey: qk.served }).flatMap(([, d]) => d ?? []);
    for (const m of served) out.push({ id: `model-${m.id}`, group: "Models", label: m.id, hint: m.owned_by, run: () => go("models", m.id) });
    return out;
  }, [qc, onHelp]);

  const results = useMemo(() => {
    const scored: { item: Item; s: number }[] = [];
    for (const item of items) {
      const s = score(query, item.label) ?? (item.keys ? score(query, item.keys) : null);
      if (s !== null) scored.push({ item, s: s + (item.group === "Go to" ? 2 : 0) });
    }
    if (query) scored.sort((a, b) => b.s - a.s);
    return scored.slice(0, 40).map((x) => x.item);
  }, [items, query]);

  useEffect(() => input.current?.focus(), []);
  useEffect(() => setActive(0), [query]);
  useEffect(() => {
    list.current?.querySelector(`[data-idx="${active}"]`)?.scrollIntoView({ block: "nearest" });
  }, [active]);

  const choose = (item: Item | undefined) => {
    if (!item) return;
    onClose();
    // Run after close so route-driven dialogs mount over a settled page.
    queueMicrotask(item.run);
  };

  let lastGroup = "";
  return (
    <div className="fixed inset-0 z-50 flex items-start justify-center bg-black/70 pt-[14vh]" onMouseDown={(e) => e.target === e.currentTarget && onClose()}>
      <div role="dialog" aria-modal="true" aria-label="Command palette" className="enter-pop w-[min(560px,calc(100vw-32px))] overflow-hidden rounded-lg border border-line-strong bg-black shadow-[0_24px_80px_rgba(0,0,0,0.9)]">
        <div className="flex h-11 items-center border-b border-line px-4">
          <input
            ref={input}
            value={query}
            onChange={(e) => setQuery(e.target.value)}
            onKeyDown={(e) => {
              if (e.key === "ArrowDown" || (e.ctrlKey && e.key === "n")) {
                e.preventDefault();
                setActive((a) => Math.min(results.length - 1, a + 1));
              } else if (e.key === "ArrowUp" || (e.ctrlKey && e.key === "p")) {
                e.preventDefault();
                setActive((a) => Math.max(0, a - 1));
              } else if (e.key === "Enter") {
                e.preventDefault();
                choose(results[active]);
              } else if (e.key === "Escape") {
                e.preventDefault();
                onClose();
              }
            }}
            placeholder="Search pages, credentials, models, actions"
            aria-label="Command"
            className="h-full w-full bg-transparent text-[14px] placeholder:text-faint focus:outline-none"
            spellCheck={false}
            autoComplete="off"
          />
        </div>
        <div ref={list} className="max-h-[min(400px,50dvh)] overflow-y-auto py-1.5">
          {results.length === 0 && <div className="px-4 py-6 text-center text-[13px] text-muted">No results</div>}
          {results.map((item, i) => {
            const header = item.group !== lastGroup;
            lastGroup = item.group;
            return (
              <div key={item.id}>
                {header && <div className="px-4 pt-2 pb-1 text-[11.5px] text-faint">{item.group}</div>}
                <button
                  data-idx={i}
                  onMouseMove={() => setActive(i)}
                  onClick={() => choose(item)}
                  className={cx("flex h-8 w-full items-center justify-between px-4 text-left text-[13px]", i === active ? "bg-active" : "")}
                >
                  <span className="truncate">{item.label}</span>
                  <span className="flex items-center gap-2 text-muted">
                    {item.hint && <span className="text-[12px]">{item.hint}</span>}
                    {item.group === "Go to" && item.keys && (
                      <span className="flex gap-1">
                        {item.keys.split(" ").map((k) => (
                          <Kbd key={k}>{k}</Kbd>
                        ))}
                      </span>
                    )}
                  </span>
                </button>
              </div>
            );
          })}
        </div>
      </div>
    </div>
  );
}
