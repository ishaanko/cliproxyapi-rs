import type { SVGProps } from "react";

// 16px grid, 1.4 stroke, currentColor. Paths only; no icon library.
const paths = {
  plus: "M8 3.2v9.6M3.2 8h9.6",
  x: "M4 4l8 8M12 4l-8 8",
  search: "M7 11.2a4.2 4.2 0 1 0 0-8.4 4.2 4.2 0 0 0 0 8.4zM10.2 10.2l3.3 3.3",
  trash: "M3 4.5h10M6.4 4.5V3h3.2v1.5M4.4 4.5l.6 8.3h6l.6-8.3M6.8 7v3.8M9.2 7v3.8",
  download: "M8 2.5v7.2M5 7l3 3 3-3M3 13h10",
  upload: "M8 10.5V3.3M5 5.8l3-3 3 3M3 13h10",
  copy: "M5.6 5.6h6.8v6.8H5.6zM3.6 10.4V3.6h6.8",
  check: "M3.4 8.4l3 3 6.2-7",
  external: "M9 3h4v4M13 3L7.4 8.6M6.6 4H3.5v8.5H12V9.4",
  refresh: "M13 8a5 5 0 1 1-1.6-3.7M13 2.8v2.9h-2.9",
  chevron: "M6 3.5L10.5 8 6 12.5",
  chevronDown: "M3.5 6L8 10.5 12.5 6",
  eye: "M1.8 8S4 3.8 8 3.8 14.2 8 14.2 8 12 12.2 8 12.2 1.8 8 1.8 8zM8 9.8a1.8 1.8 0 1 0 0-3.6 1.8 1.8 0 0 0 0 3.6z",
  eyeOff: "M2.5 2.5l11 11M6.6 4.1c.5-.2.9-.3 1.4-.3 4 0 6.2 4.2 6.2 4.2s-.7 1.3-1.9 2.4M4.2 5.6C2.8 6.8 1.8 8 1.8 8s2.2 4.2 6.2 4.2c.8 0 1.5-.2 2.1-.4",
  logout: "M6.2 3H3.5v10h2.7M10 5.2L12.8 8 10 10.8M12.8 8H6.4",
  edit: "M3 13l.6-2.8 6.9-6.9a1.2 1.2 0 0 1 1.7 0l.5.5a1.2 1.2 0 0 1 0 1.7L5.8 12.4 3 13z",
  warn: "M8 2.8l5.6 9.8H2.4L8 2.8zM8 6.8v2.6M8 11v.2",
  file: "M4 2.5h5l3 3v8H4zM9 2.5v3h3",
  command: "M5.5 5.5v-1a1.5 1.5 0 1 0-1.5 1.5h8a1.5 1.5 0 1 0-1.5-1.5v7a1.5 1.5 0 1 0 1.5-1.5H4a1.5 1.5 0 1 0 1.5 1.5v-6",
} as const;

export type IconName = keyof typeof paths;

export function Icon({ name, size = 14, ...rest }: { name: IconName; size?: number } & Omit<SVGProps<SVGSVGElement>, "name">) {
  return (
    <svg
      width={size}
      height={size}
      viewBox="0 0 16 16"
      fill="none"
      stroke="currentColor"
      strokeWidth="1.4"
      strokeLinecap="round"
      strokeLinejoin="round"
      aria-hidden="true"
      {...rest}
    >
      <path d={paths[name]} />
    </svg>
  );
}
