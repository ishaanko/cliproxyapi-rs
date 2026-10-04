// Dot graphics shared by Overview, Credentials and Quotas. Static SVG only: nothing animates.
import { useState, type ReactNode } from "react";
import type { RecentBucket } from "@/lib/types";

const UNLIT = "#1f1f1f";
/** Lit dot shades, dimmest first. */
const LEVELS = ["#3d3d3d", "#707070", "#a8a8a8", "#ffffff"] as const;
const BAD = "var(--color-bad)";
const WARN = "var(--color-warn)";

/** Shade for a count relative to the busiest bucket. */
const level = (n: number, max: number) => LEVELS[Math.min(3, Math.floor((n / max) * 3.999))] ?? LEVELS[3];

/** Twenty dots for a percent used. Turns orange from 75% and red from 90%. */
export function DotMeter({ used, label, d = 5, gap = 3 }: { used: number; label: string; d?: number; gap?: number }) {
  const lit = Math.round(used / 5);
  const fill = used >= 90 ? BAD : used >= 75 ? WARN : "#fff";
  const step = d + gap;
  return (
    <svg
      width={20 * step - gap}
      height={d}
      role="meter"
      aria-label={label}
      aria-valuenow={used}
      aria-valuemin={0}
      aria-valuemax={100}
      className="block shrink-0"
    >
      {Array.from({ length: 20 }, (_, i) => (
        <circle key={i} cx={i * step + d / 2} cy={d / 2} r={d / 2} fill={i < lit ? fill : UNLIT} />
      ))}
    </svg>
  );
}

/** One row of request buckets, oldest first. Shade is volume; a bucket with failures is red. */
export function DotStrip({ buckets, d = 4, gap = 2 }: { buckets: RecentBucket[]; d?: number; gap?: number }) {
  const max = Math.max(1, ...buckets.map((b) => b.success + b.failed));
  const step = d + gap;
  const total = buckets.reduce((s, b) => s + b.success + b.failed, 0);
  const failed = buckets.reduce((s, b) => s + b.failed, 0);
  return (
    <svg
      width={Math.max(0, buckets.length * step - gap)}
      height={d}
      role="img"
      aria-label={`${total} recent requests${failed ? `, ${failed} failed` : ""}`}
      className="block shrink-0"
    >
      {buckets.map((b, i) => {
        const n = b.success + b.failed;
        return <circle key={i} cx={i * step + d / 2} cy={d / 2} r={d / 2} fill={b.failed ? BAD : n ? level(n, max) : UNLIT} />;
      })}
    </svg>
  );
}

export interface Column {
  label: string;
  requests: number;
  failed: number;
  /** Extra text shown while the column is hovered. */
  note?: string;
}

/**
 * Dot bar chart: each column lights dots bottom-up in proportion to its requests, and its top
 * lit dot is red when the bucket had failures. Hovering a column names it in the caption line.
 */
export function DotBars({ columns, rows = 6, d = 12, gap = 8, caption }: { columns: Column[]; rows?: number; d?: number; gap?: number; caption: ReactNode }) {
  const [hover, setHover] = useState<number | null>(null);
  const max = Math.max(1, ...columns.map((c) => c.requests));
  const step = d + gap;
  const width = Math.max(0, columns.length * step - gap);
  const height = rows * step - gap;
  const active = hover === null ? undefined : columns[hover];
  return (
    <div>
      <div className="num flex h-5 items-center gap-3 text-[12px]" aria-live="polite">
        {active ? (
          <>
            <span className="text-fg">{active.label}</span>
            <span>{active.requests.toLocaleString()} requests</span>
            {active.failed > 0 && <span className="text-bad">{active.failed.toLocaleString()} failed</span>}
            {active.note && <span className="text-muted">{active.note}</span>}
          </>
        ) : (
          caption
        )}
      </div>
      <svg width={width} height={height} className="mt-2 block" role="img" aria-label="Requests over time" onMouseLeave={() => setHover(null)}>
        {columns.map((c, i) => {
          const lit = c.requests ? Math.max(1, Math.round((c.requests / max) * rows)) : 0;
          const x = i * step + d / 2;
          return (
            <g key={i} onMouseEnter={() => setHover(i)}>
              <rect x={i * step - gap / 2} y={0} width={step} height={height} fill="transparent" />
              {Array.from({ length: rows }, (_, j) => {
                const fromBottom = rows - j;
                const on = fromBottom <= lit;
                const fill = !on ? UNLIT : fromBottom === lit && c.failed ? BAD : hover === i ? "#fff" : "#cfcfcf";
                return <circle key={j} cx={x} cy={j * step + d / 2} r={d / 2} fill={fill} />;
              })}
            </g>
          );
        })}
      </svg>
      <div className="num mt-1.5 flex justify-between text-[11px] text-faint" style={{ width }}>
        <span>{columns[0]?.label}</span>
        <span>now</span>
      </div>
    </div>
  );
}
