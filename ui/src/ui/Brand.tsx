export function Brand({ large }: { large?: boolean }) {
  const s = large ? 22 : 16;
  return (
    <div className="flex items-center gap-2.5">
      <svg width={s} height={s} viewBox="0 0 16 16" aria-hidden="true">
        <rect width="16" height="16" rx="3.5" fill="#fff" />
        <path d="M5 4.8L9.2 8 5 11.2M9.8 11.2h2" stroke="#000" strokeWidth="1.5" strokeLinecap="round" strokeLinejoin="round" fill="none" />
      </svg>
      <span className={large ? "text-[17px] font-medium tracking-[-0.02em]" : "text-[13.5px] font-medium tracking-[-0.01em]"}>CLIProxyAPI</span>
    </div>
  );
}
