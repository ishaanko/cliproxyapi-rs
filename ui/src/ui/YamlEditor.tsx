import { defaultKeymap, history, historyKeymap, indentWithTab } from "@codemirror/commands";
import { yaml as yamlLang } from "@codemirror/lang-yaml";
import { HighlightStyle, bracketMatching, indentOnInput, syntaxHighlighting } from "@codemirror/language";
import { linter, lintGutter, type Diagnostic } from "@codemirror/lint";
import { highlightSelectionMatches, searchKeymap } from "@codemirror/search";
import { EditorState } from "@codemirror/state";
import {
  EditorView,
  drawSelection,
  highlightActiveLine,
  highlightActiveLineGutter,
  keymap,
  lineNumbers,
} from "@codemirror/view";
import { tags as t } from "@lezer/highlight";
import { parseDocument } from "yaml";
import { useEffect, useRef } from "react";

export interface CursorInfo {
  line: number;
  col: number;
  lines: number;
}

/** Syntax errors from the `yaml` parser, as editor diagnostics and plain problems. */
export function yamlProblems(text: string): { from: number; to: number; line: number; message: string }[] {
  const doc = parseDocument(text, { prettyErrors: false });
  return doc.errors.map((e) => {
    const from = Math.min(e.pos[0], text.length);
    const to = Math.max(from, Math.min(e.pos[1], text.length));
    const line = e.linePos?.[0]?.line ?? text.slice(0, from).split("\n").length;
    return { from, to, line, message: e.message.split("\n")[0] ?? e.message };
  });
}

const theme = EditorView.theme(
  {
    "&": { height: "100%", backgroundColor: "#000", color: "#e8e8e8", fontSize: "12.5px" },
    ".cm-scroller": { fontFamily: "var(--font-mono)", lineHeight: "1.62", overflow: "auto" },
    ".cm-content": { caretColor: "#fff", padding: "10px 0" },
    ".cm-cursor, .cm-dropCursor": { borderLeftColor: "#fff", borderLeftWidth: "1.5px" },
    "&.cm-focused": { outline: "none" },
    "&.cm-focused .cm-selectionBackground, .cm-selectionBackground, ::selection": { backgroundColor: "rgba(110,168,255,0.28)" },
    ".cm-gutters": { backgroundColor: "#000", color: "#4a4a4a", border: "none", paddingLeft: "8px" },
    ".cm-lineNumbers .cm-gutterElement": { padding: "0 14px 0 6px", minWidth: "34px" },
    ".cm-activeLine": { backgroundColor: "#0b0b0b" },
    ".cm-activeLineGutter": { backgroundColor: "transparent", color: "#bdbdbd" },
    ".cm-selectionMatch": { backgroundColor: "rgba(255,255,255,0.1)" },
    ".cm-matchingBracket": { backgroundColor: "rgba(255,255,255,0.14)", outline: "none" },
    ".cm-lintRange-error": { backgroundImage: "none", textDecoration: "underline wavy #f2625d", textUnderlineOffset: "3px" },
    ".cm-gutter-lint": { width: "10px" },
    ".cm-lint-marker-error": { content: "none", width: "6px", height: "6px", borderRadius: "50%", background: "#f2625d" },
    ".cm-tooltip": { backgroundColor: "#000", border: "1px solid #2a2a2a", borderRadius: "6px", color: "#e8e8e8" },
    ".cm-tooltip-lint": { fontFamily: "var(--font-sans)", fontSize: "12px" },
    ".cm-panels": { backgroundColor: "#000", color: "#e8e8e8", borderTop: "1px solid #1a1a1a" },
    ".cm-panels input, .cm-panels button": { fontFamily: "var(--font-sans)", fontSize: "12px" },
    ".cm-panels input": { backgroundColor: "#000", border: "1px solid #2a2a2a", borderRadius: "4px", color: "#fff", padding: "2px 6px" },
    ".cm-panels button": { backgroundColor: "#0a0a0a", border: "1px solid #2a2a2a", borderRadius: "4px", color: "#e8e8e8", backgroundImage: "none" },
    ".cm-searchMatch": { backgroundColor: "rgba(229,168,59,0.25)" },
    ".cm-searchMatch-selected": { backgroundColor: "rgba(229,168,59,0.5)" },
  },
  { dark: true },
);

const highlight = HighlightStyle.define([
  { tag: [t.propertyName, t.definition(t.propertyName)], color: "#ffffff" },
  { tag: [t.string, t.special(t.string)], color: "#a5c8ff" },
  { tag: [t.number, t.bool, t.null, t.atom], color: "#d9a35b" },
  { tag: t.comment, color: "#5f5f5f", fontStyle: "italic" },
  { tag: [t.punctuation, t.separator, t.brace, t.squareBracket, t.operator], color: "#707070" },
  { tag: [t.labelName, t.typeName, t.meta, t.keyword], color: "#c4a1ff" },
]);

/**
 * YAML editor on CodeMirror 6. Uncontrolled: `initial` is read once, so remount with a
 * new `key` to replace the document. Reports edits, cursor and save requests.
 */
export function YamlEditor({
  initial,
  onChange,
  onCursor,
  onSave,
  jumpTo,
}: {
  initial: string;
  onChange: (text: string) => void;
  onCursor: (c: CursorInfo) => void;
  onSave: () => void;
  /** Line to scroll to and select; `nonce` re-triggers the same line. */
  jumpTo?: { line: number; nonce: number };
}) {
  const host = useRef<HTMLDivElement>(null);
  const view = useRef<EditorView | null>(null);
  const handlers = useRef({ onChange, onCursor, onSave });
  handlers.current = { onChange, onCursor, onSave };

  useEffect(() => {
    if (!host.current) return;
    const lintSource = (v: EditorView): Diagnostic[] =>
      yamlProblems(v.state.doc.toString()).map((p) => ({ from: p.from, to: p.to, severity: "error", message: p.message }));
    const report = (v: EditorView) => {
      const head = v.state.selection.main.head;
      const line = v.state.doc.lineAt(head);
      handlers.current.onCursor({ line: line.number, col: head - line.from + 1, lines: v.state.doc.lines });
    };
    const v = new EditorView({
      parent: host.current,
      state: EditorState.create({
        doc: initial,
        extensions: [
          lineNumbers(),
          highlightActiveLineGutter(),
          highlightActiveLine(),
          drawSelection(),
          history(),
          indentOnInput(),
          bracketMatching(),
          highlightSelectionMatches(),
          yamlLang(),
          syntaxHighlighting(highlight),
          lintGutter(),
          linter(lintSource, { delay: 250 }),
          EditorState.tabSize.of(2),
          keymap.of([
            { key: "Mod-s", preventDefault: true, run: () => (handlers.current.onSave(), true) },
            indentWithTab,
            ...searchKeymap,
            ...historyKeymap,
            ...defaultKeymap,
          ]),
          theme,
          EditorView.updateListener.of((u) => {
            if (u.docChanged) handlers.current.onChange(u.state.doc.toString());
            if (u.docChanged || u.selectionSet) report(u.view);
          }),
        ],
      }),
    });
    view.current = v;
    report(v);
    return () => {
      v.destroy();
      view.current = null;
    };
    // `initial` is intentionally read once per mount.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  useEffect(() => {
    const v = view.current;
    if (!v || !jumpTo) return;
    const line = v.state.doc.line(Math.min(Math.max(jumpTo.line, 1), v.state.doc.lines));
    v.dispatch({ selection: { anchor: line.from, head: line.to }, effects: EditorView.scrollIntoView(line.from, { y: "center" }) });
    v.focus();
  }, [jumpTo]);

  return <div ref={host} className="h-full min-h-0" />;
}
