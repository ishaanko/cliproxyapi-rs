import { useEffect, useRef, useSyncExternalStore, type ReactNode } from "react";
import { useToasts } from "@/lib/toast";
import { Button, IconButton, StatusDot, cx } from "./primitives";

// Native <dialog>: focus trap, Esc and top-layer stacking come from the platform.
// Parents mount these only while open, so state resets on every open.

function useDialog(onClose: () => void) {
  const ref = useRef<HTMLDialogElement>(null);
  useEffect(() => {
    const d = ref.current;
    if (!d || d.open) return;
    d.showModal();
    // React's autoFocus runs before the dialog is open, so elements opt in via data-af.
    (d.querySelector<HTMLElement>("[data-af]") ?? d).focus({ preventScroll: true });
  }, []);
  return {
    ref,
    onCancel: (e: React.SyntheticEvent) => {
      e.preventDefault();
      onClose();
    },
    onMouseDown: (e: React.MouseEvent<HTMLDialogElement>) => {
      if (e.target === e.currentTarget) onClose();
    },
  };
}

function Titlebar({ title, onClose }: { title: ReactNode; onClose: () => void }) {
  return (
    <div className="flex h-12 shrink-0 items-center justify-between border-b border-line pr-2 pl-5">
      <h2 className="min-w-0 truncate text-[14px] font-medium tracking-[-0.01em]">{title}</h2>
      <IconButton icon="x" label="Close" onClick={onClose} />
    </div>
  );
}

export function Dialog({
  title,
  onClose,
  children,
  footer,
  width = 520,
}: {
  title: ReactNode;
  onClose: () => void;
  children: ReactNode;
  footer?: ReactNode;
  width?: number;
}) {
  const d = useDialog(onClose);
  return (
    <dialog
      ref={d.ref}
      onCancel={d.onCancel}
      onMouseDown={d.onMouseDown}
      style={{ width: `min(${width}px, calc(100vw - 32px))` }}
      tabIndex={-1}
      className="modal m-auto outline-none max-h-[min(720px,calc(100dvh-48px))] overflow-hidden rounded-lg border border-line-strong bg-black p-0 text-fg shadow-[0_24px_80px_rgba(0,0,0,0.9)]"
    >
      <div className="flex max-h-[inherit] flex-col">
        <Titlebar title={title} onClose={onClose} />
        <div className="min-h-0 flex-1 overflow-y-auto">{children}</div>
        {footer && <div className="flex shrink-0 items-center justify-end gap-2 border-t border-line px-5 py-3">{footer}</div>}
      </div>
    </dialog>
  );
}

/** Right-hand drawer for details and forms. */
export function Sheet({
  title,
  onClose,
  children,
  footer,
  width = 560,
}: {
  title: ReactNode;
  onClose: () => void;
  children: ReactNode;
  footer?: ReactNode;
  width?: number;
}) {
  const d = useDialog(onClose);
  return (
    <dialog
      ref={d.ref}
      onCancel={d.onCancel}
      onMouseDown={d.onMouseDown}
      style={{ width: `min(${width}px, 100vw)` }}
      tabIndex={-1}
      className="sheet outline-none fixed inset-y-0 right-0 left-auto m-0 h-dvh max-h-dvh max-w-none overflow-hidden border-l border-line-strong bg-black p-0 text-fg"
    >
      <div className="flex h-full flex-col">
        <Titlebar title={title} onClose={onClose} />
        <div className="min-h-0 flex-1 overflow-y-auto">{children}</div>
        {footer && <div className="flex shrink-0 items-center justify-end gap-2 border-t border-line px-5 py-3">{footer}</div>}
      </div>
    </dialog>
  );
}

// Promise-based confirmation, rendered once by <ConfirmHost/>.

interface ConfirmRequest {
  title: string;
  body?: string;
  confirm: string;
  danger: boolean;
  resolve: (ok: boolean) => void;
}
let pending: ConfirmRequest | null = null;
const confirmListeners = new Set<() => void>();
function setPending(p: ConfirmRequest | null) {
  pending = p;
  for (const l of confirmListeners) l();
}

export function confirm(opts: { title: string; body?: string; confirm?: string; danger?: boolean }): Promise<boolean> {
  return new Promise((resolve) => {
    setPending({ title: opts.title, body: opts.body, confirm: opts.confirm ?? "Confirm", danger: opts.danger ?? false, resolve });
  });
}

export function ConfirmHost() {
  const req = useSyncExternalStore(
    (cb) => {
      confirmListeners.add(cb);
      return () => confirmListeners.delete(cb);
    },
    () => pending,
  );
  if (!req) return null;
  const done = (ok: boolean) => {
    req.resolve(ok);
    setPending(null);
  };
  return (
    <Dialog
      title={req.title}
      width={420}
      onClose={() => done(false)}
      footer={
        <>
          <Button onClick={() => done(false)}>Cancel</Button>
          <Button variant={req.danger ? "danger" : "primary"} data-af onClick={() => done(true)}>
            {req.confirm}
          </Button>
        </>
      }
    >
      {req.body && <p className="px-5 py-4 text-[13px] break-words text-fg-2">{req.body}</p>}
      {!req.body && <div className="h-1" />}
    </Dialog>
  );
}

export function Toaster() {
  const toasts = useToasts();
  return (
    <div className="pointer-events-none fixed right-4 bottom-4 z-[100] flex flex-col items-end gap-2">
      {toasts.map((t) => (
        <div
          key={t.id}
          role="status"
          className={cx(
            "enter-pop pointer-events-auto flex max-w-[420px] items-start gap-2.5 rounded-md border bg-black px-3 py-2 text-[13px] shadow-[0_8px_32px_rgba(0,0,0,0.8)]",
            t.kind === "error" ? "border-[#5a2422]" : "border-line-strong",
          )}
        >
          <StatusDot tone={t.kind === "error" ? "bad" : "ok"} className="mt-[6px]" />
          <span className="break-words">{t.text}</span>
        </div>
      ))}
    </div>
  );
}
