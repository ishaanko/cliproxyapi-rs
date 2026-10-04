import {
  forwardRef,
  useEffect,
  useRef,
  useState,
  type ButtonHTMLAttributes,
  type InputHTMLAttributes,
  type ReactNode,
  type SelectHTMLAttributes,
  type TextareaHTMLAttributes,
} from "react";
import { Icon, type IconName } from "./icons";

export const cx = (...parts: (string | false | null | undefined)[]): string => parts.filter(Boolean).join(" ");

// Buttons

type Variant = "primary" | "default" | "ghost" | "danger";

const variants: Record<Variant, string> = {
  primary: "bg-white text-black hover:bg-[#e6e6e6] active:bg-[#d4d4d4] border border-transparent",
  default: "border border-line-strong text-fg hover:bg-hover hover:border-[#3d3d3d] active:bg-active",
  ghost: "border border-transparent text-muted hover:text-fg hover:bg-hover active:bg-active",
  danger: "border border-line-strong text-bad hover:bg-[#140707] hover:border-[#5a2422] active:bg-[#1d0a0a]",
};

export const Button = forwardRef<
  HTMLButtonElement,
  ButtonHTMLAttributes<HTMLButtonElement> & { variant?: Variant; icon?: IconName; kbd?: string }
>(function Button({ variant = "default", icon, kbd, className, children, type = "button", ...rest }, ref) {
  return (
    <button
      ref={ref}
      type={type}
      className={cx(
        "inline-flex h-7 shrink-0 items-center justify-center gap-1.5 rounded-md px-2.5 text-[13px] font-medium transition-colors duration-100 select-none disabled:pointer-events-none disabled:opacity-40",
        variants[variant],
        className,
      )}
      {...rest}
    >
      {icon && <Icon name={icon} size={13} />}
      {children}
      {kbd && <Kbd tone={variant === "primary" ? "dark" : "default"}>{kbd}</Kbd>}
    </button>
  );
});

export function IconButton({
  icon,
  label,
  className,
  danger,
  ...rest
}: ButtonHTMLAttributes<HTMLButtonElement> & { icon: IconName; label: string; danger?: boolean }) {
  return (
    <button
      type="button"
      title={label}
      aria-label={label}
      className={cx(
        "inline-flex size-7 shrink-0 items-center justify-center rounded-md text-muted transition-colors duration-100 hover:bg-active disabled:pointer-events-none disabled:opacity-40",
        danger ? "hover:text-bad" : "hover:text-fg",
        className,
      )}
      {...rest}
    >
      <Icon name={icon} size={14} />
    </button>
  );
}

export function Kbd({ children, tone = "default" }: { children: ReactNode; tone?: "default" | "dark" }) {
  return (
    <kbd
      className={cx(
        "mono inline-flex h-[17px] min-w-[17px] items-center justify-center rounded-[4px] border px-1 text-[10.5px] leading-none font-normal",
        tone === "dark" ? "border-black/20 text-black/55" : "border-line-strong text-muted",
      )}
    >
      {children}
    </kbd>
  );
}

// Fields

const fieldBase =
  "w-full rounded-md border border-line-strong bg-black px-2.5 text-[13px] text-fg placeholder:text-faint transition-colors duration-100 hover:border-[#3d3d3d] focus:border-[#6a6a6a] disabled:opacity-50";

export const Input = forwardRef<HTMLInputElement, InputHTMLAttributes<HTMLInputElement>>(function Input({ className, ...rest }, ref) {
  return <input ref={ref} spellCheck={false} autoComplete="off" className={cx(fieldBase, "h-8", className)} {...rest} />;
});

export const Textarea = forwardRef<HTMLTextAreaElement, TextareaHTMLAttributes<HTMLTextAreaElement>>(function Textarea(
  { className, ...rest },
  ref,
) {
  return <textarea ref={ref} spellCheck={false} className={cx(fieldBase, "mono min-h-20 resize-y py-2 leading-[1.5]", className)} {...rest} />;
});

export function Select({ className, children, ...rest }: SelectHTMLAttributes<HTMLSelectElement>) {
  return (
    <select className={cx(fieldBase, "h-8 appearance-none pr-7", className)} {...rest}>
      {children}
    </select>
  );
}

export function Field({ label, children, hint }: { label: string; children: ReactNode; hint?: string }) {
  return (
    <label className="grid gap-1.5">
      <span className="text-[12px] text-muted">{label}</span>
      {children}
      {hint && <span className="text-[11.5px] text-faint">{hint}</span>}
    </label>
  );
}

/** Search box. Focus with `/` from anywhere (picked up via data-search). */
export const SearchInput = forwardRef<HTMLInputElement, InputHTMLAttributes<HTMLInputElement>>(function SearchInput(
  { className, ...rest },
  ref,
) {
  return (
    <div className={cx("relative", className)}>
      <Icon name="search" size={13} className="pointer-events-none absolute top-1/2 left-2.5 -translate-y-1/2 text-faint" />
      <Input ref={ref} data-search className="h-7 pr-7 pl-7" {...rest} />
      <span className="pointer-events-none absolute top-1/2 right-1.5 -translate-y-1/2">
        <Kbd>/</Kbd>
      </span>
    </div>
  );
});

export function Switch({ checked, onChange, label, disabled }: { checked: boolean; onChange: (v: boolean) => void; label: string; disabled?: boolean }) {
  return (
    <button
      type="button"
      role="switch"
      aria-checked={checked}
      aria-label={label}
      title={label}
      disabled={disabled}
      onClick={(e) => {
        e.stopPropagation();
        onChange(!checked);
      }}
      className={cx(
        "relative inline-block h-[16px] w-[28px] shrink-0 rounded-full border transition-colors duration-150 disabled:opacity-40",
        checked ? "border-white bg-white" : "border-line-strong bg-black hover:border-[#4a4a4a]",
      )}
    >
      <span
        className={cx(
          "absolute top-[2px] left-[2px] size-[10px] rounded-full transition-transform duration-150",
          checked ? "translate-x-[12px] bg-black" : "bg-[#6a6a6a]",
        )}
      />
    </button>
  );
}

// Status

export type Tone = "ok" | "warn" | "bad" | "off";
const dotTone: Record<Tone, string> = {
  ok: "bg-ok",
  warn: "bg-warn",
  bad: "bg-bad",
  off: "border border-faint bg-transparent",
};
const textTone: Record<Tone, string> = { ok: "text-fg", warn: "text-warn", bad: "text-bad", off: "text-muted" };

export function StatusDot({ tone, className }: { tone: Tone; className?: string }) {
  return <span className={cx("inline-block size-[7px] shrink-0 rounded-full", dotTone[tone], className)} />;
}

export function Status({ tone, children }: { tone: Tone; children: ReactNode }) {
  return (
    <span className={cx("inline-flex items-center gap-2", textTone[tone])}>
      <StatusDot tone={tone} />
      {children}
    </span>
  );
}

// Navigation within a page

export function Tabs<T extends string>({
  value,
  onChange,
  items,
}: {
  value: T;
  onChange: (v: T) => void;
  items: { id: T; label: string; count?: number }[];
}) {
  return (
    <div role="tablist" className="flex items-end gap-5 overflow-x-auto px-5">
      {items.map((t) => (
        <button
          key={t.id}
          role="tab"
          aria-selected={t.id === value}
          onClick={() => onChange(t.id)}
          className={cx(
            "relative -mb-px h-9 shrink-0 border-b px-0 text-[13px] transition-colors duration-100",
            t.id === value ? "border-white text-fg" : "border-transparent text-muted hover:text-fg",
          )}
        >
          {t.label}
          {t.count !== undefined && <span className="num ml-1.5 text-[12px] text-faint">{t.count}</span>}
        </button>
      ))}
    </div>
  );
}

// Layout

export function PageHeader({ title, children }: { title: string; children?: ReactNode }) {
  return (
    <header className="flex h-12 shrink-0 items-center justify-between gap-4 border-b border-line px-5">
      <h1 className="text-[14px] font-medium tracking-[-0.01em]">{title}</h1>
      <div className="flex items-center gap-2">{children}</div>
    </header>
  );
}

export function Section({ title, right, children, className }: { title: string; right?: ReactNode; children: ReactNode; className?: string }) {
  return (
    <section className={cx("min-w-0", className)}>
      <div className="flex h-9 items-center justify-between">
        <h2 className="text-[13px] font-medium">{title}</h2>
        {right}
      </div>
      {children}
    </section>
  );
}

export function EmptyState({ title, hint, action }: { title: string; hint?: string; action?: ReactNode }) {
  return (
    <div className="flex flex-col items-center justify-center gap-3 px-6 py-16 text-center">
      <div className="text-[13px] font-medium">{title}</div>
      {hint && <div className="max-w-[44ch] text-[12.5px] text-muted">{hint}</div>}
      {action}
    </div>
  );
}

export function ErrorState({ error, onRetry }: { error: unknown; onRetry?: () => void }) {
  const message = error instanceof Error ? error.message : String(error);
  return (
    <div className="flex flex-col items-center justify-center gap-3 px-6 py-16 text-center">
      <div className="flex items-center gap-2 text-[13px] font-medium text-bad">
        <StatusDot tone="bad" />
        Request failed
      </div>
      <div className="mono max-w-[60ch] text-[12px] break-words text-muted">{message}</div>
      {onRetry && (
        <Button icon="refresh" onClick={onRetry}>
          Retry
        </Button>
      )}
    </div>
  );
}

/** Static placeholder rows while a query loads (no motion). */
export function LoadingRows({ rows = 6 }: { rows?: number }) {
  return (
    <div className="px-5 py-2" aria-busy="true">
      {Array.from({ length: rows }, (_, i) => (
        <div key={i} className="flex h-[38px] items-center border-b border-line">
          <div className="h-2 rounded-sm bg-[#141414]" style={{ width: `${30 + ((i * 17) % 40)}%` }} />
        </div>
      ))}
    </div>
  );
}

export function CopyButton({ text, label = "Copy" }: { text: string; label?: string }) {
  const [done, setDone] = useState(false);
  const timer = useRef<ReturnType<typeof setTimeout>>(undefined);
  useEffect(() => () => clearTimeout(timer.current), []);
  return (
    <IconButton
      icon={done ? "check" : "copy"}
      label={done ? "Copied" : label}
      className={done ? "text-ok hover:text-ok" : undefined}
      onClick={(e) => {
        e.stopPropagation();
        void navigator.clipboard.writeText(text).then(() => {
          setDone(true);
          clearTimeout(timer.current);
          timer.current = setTimeout(() => setDone(false), 1200);
        });
      }}
    />
  );
}

export function KeyValue({ rows }: { rows: [string, ReactNode][] }) {
  return (
    <dl className="grid grid-cols-[112px_1fr]">
      {rows.map(([k, v]) => (
        <div key={k} className="contents">
          <dt className="flex min-h-8 items-center border-b border-line text-[12px] text-muted">{k}</dt>
          <dd className="flex min-h-8 min-w-0 items-center border-b border-line break-all">{v}</dd>
        </div>
      ))}
    </dl>
  );
}
