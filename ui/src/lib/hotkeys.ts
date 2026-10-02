import { useEffect, useRef, useState } from "react";

export function isTyping(target: EventTarget | null): boolean {
  if (!(target instanceof HTMLElement)) return false;
  return target.isContentEditable || target.closest("input, textarea, select, .cm-editor") !== null;
}

export function modalOpen(): boolean {
  return document.querySelector("dialog[open]") !== null;
}

/**
 * Keyboard selection for tables: j/k or arrows move, Enter opens, Escape clears.
 * Rows mark themselves with data-row-index so the active one scrolls into view.
 */
export function useRowNav<T>(rows: readonly T[], onOpen: (row: T) => void, extra?: Record<string, (row: T) => void>) {
  const [index, setIndex] = useState(-1);
  const latest = useRef({ rows, onOpen, extra, index });
  latest.current = { rows, onOpen, extra, index };

  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.metaKey || e.ctrlKey || e.altKey || isTyping(e.target) || modalOpen()) return;
      const { rows: r, onOpen: open, extra: ex, index: i } = latest.current;
      const row = r[i];
      if (e.key === "j" || e.key === "ArrowDown") {
        e.preventDefault();
        setIndex(Math.min(r.length - 1, i + 1));
      } else if (e.key === "k" || e.key === "ArrowUp") {
        e.preventDefault();
        setIndex(Math.max(0, i - 1));
      } else if (e.key === "Enter" && row !== undefined) {
        e.preventDefault();
        open(row);
      } else if (e.key === "Escape") {
        setIndex(-1);
      } else if (row !== undefined && ex?.[e.key]) {
        e.preventDefault();
        ex[e.key]?.(row);
      }
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, []);

  // Keep the selection valid when the list shrinks (filtering, deletes).
  useEffect(() => {
    if (index >= rows.length) setIndex(rows.length - 1);
  }, [rows.length, index]);

  useEffect(() => {
    if (index < 0) return;
    document.querySelector(`[data-row-index="${index}"]`)?.scrollIntoView({ block: "nearest" });
  }, [index]);

  return { index, setIndex };
}
