import { useCallback, useEffect, useRef, useState } from "react";
import { useServerMeta } from "@/lib/api";
import { clearKey, useKey } from "@/lib/auth";
import { MOD, fmtVersion } from "@/lib/format";
import { isTyping, modalOpen } from "@/lib/hotkeys";
import { useClientKeys, useCredentials, useHealth } from "@/lib/queries";
import { NAV, go, href, useRoute, type Page } from "@/lib/route";
import ApiKeys from "@/screens/ApiKeys";
import Config from "@/screens/Config";
import Credentials from "@/screens/Credentials";
import Login from "@/screens/Login";
import Logs from "@/screens/Logs";
import Models from "@/screens/Models";
import Overview from "@/screens/Overview";
import Providers from "@/screens/Providers";
import Quotas from "@/screens/Quotas";
import Tools from "@/screens/Tools";
import { Brand } from "@/ui/Brand";
import { CommandPalette } from "@/ui/CommandPalette";
import { ConfirmHost, Dialog, Toaster } from "@/ui/overlays";
import { Icon } from "@/ui/icons";
import { Kbd, StatusDot, cx } from "@/ui/primitives";

export default function App() {
  const key = useKey();
  return (
    <>
      {key ? <Shell /> : <Login />}
      <Toaster />
      <ConfirmHost />
    </>
  );
}

function Shell() {
  const route = useRoute();
  const [palette, setPalette] = useState(false);
  const [help, setHelp] = useState(false);
  const openHelp = useCallback(() => setHelp(true), []);
  const chord = useRef<number>(0);

  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if ((e.metaKey || e.ctrlKey) && e.key.toLowerCase() === "k") {
        e.preventDefault();
        setPalette((p) => !p);
        return;
      }
      if (e.metaKey || e.ctrlKey || e.altKey || isTyping(e.target) || modalOpen()) return;
      if (e.key === "/") {
        const el = document.querySelector<HTMLInputElement>("[data-search]");
        if (el) {
          e.preventDefault();
          el.focus();
          el.select();
        }
        return;
      }
      if (e.key === "?") {
        setHelp(true);
        return;
      }
      if (chord.current && Date.now() - chord.current < 900) {
        chord.current = 0;
        const target = NAV.find((n) => n.key === e.key);
        if (target) {
          e.preventDefault();
          go(target.page);
        }
        return;
      }
      if (e.key === "g") chord.current = Date.now();
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, []);

  return (
    <div className="flex h-full flex-col md:flex-row">
      <Sidebar active={route.page} onPalette={() => setPalette(true)} />
      <main className="flex min-h-0 min-w-0 flex-1 flex-col">
        {route.page === "overview" && <Overview />}
        {route.page === "tools" && <Tools />}
        {route.page === "credentials" && <Credentials sub={route.sub} />}
        {route.page === "quotas" && <Quotas />}
        {route.page === "keys" && <ApiKeys sub={route.sub} />}
        {route.page === "providers" && <Providers sub={route.sub} />}
        {route.page === "models" && <Models sub={route.sub} />}
        {route.page === "config" && <Config />}
        {route.page === "logs" && <Logs sub={route.sub} />}
      </main>
      {palette && <CommandPalette onClose={() => setPalette(false)} onHelp={openHelp} />}
      {help && <Shortcuts onClose={() => setHelp(false)} />}
    </div>
  );
}

function Sidebar({ active, onPalette }: { active: Page; onPalette: () => void }) {
  const creds = useCredentials();
  const keys = useClientKeys();
  const health = useHealth();
  const meta = useServerMeta();
  const counts: Partial<Record<Page, number | undefined>> = { credentials: creds.data?.length, keys: keys.data?.length };
  const up = health.data === true;

  return (
    <aside className="flex shrink-0 flex-col border-b border-line md:w-[212px] md:border-r md:border-b-0">
      <div className="flex h-12 items-center px-4">
        <Brand />
      </div>
      <nav className="flex gap-0.5 overflow-x-auto px-2 pb-2 md:flex-1 md:flex-col md:overflow-visible md:pb-0">
        {NAV.map((n) => {
          const on = n.page === active;
          const count = counts[n.page];
          return (
            <a
              key={n.page}
              href={href(n.page)}
              aria-current={on ? "page" : undefined}
              className={cx(
                "group flex h-7 shrink-0 items-center justify-between rounded-md px-2.5 text-[13px] transition-colors duration-100",
                on ? "bg-active text-fg" : "text-muted hover:bg-hover hover:text-fg",
              )}
            >
              <span>{n.label}</span>
              <span className="flex items-center gap-1.5">
                {count !== undefined && <span className="num text-[12px] text-faint">{count}</span>}
                <span className="mono hidden text-[10.5px] text-faint opacity-0 transition-opacity group-hover:opacity-100 md:inline">g {n.key}</span>
              </span>
            </a>
          );
        })}
      </nav>
      <div className="hidden gap-1 border-t border-line p-2 md:grid">
        <button
          onClick={onPalette}
          className="flex h-7 items-center justify-between rounded-md px-2.5 text-[13px] text-muted transition-colors duration-100 hover:bg-hover hover:text-fg"
        >
          <span className="flex items-center gap-2">
            <Icon name="search" size={13} />
            Search
          </span>
          <span className="flex gap-1">
            <Kbd>{MOD}</Kbd>
            <Kbd>K</Kbd>
          </span>
        </button>
        <div className="flex h-7 items-center justify-between px-2.5 text-[12px] text-muted">
          <span className="flex items-center gap-2" title={meta.commit ? `commit ${meta.commit}` : undefined}>
            <StatusDot tone={health.isLoading ? "off" : up ? "ok" : "bad"} />
            <span className="mono">{meta.version ? fmtVersion(meta.version) : up ? "online" : "offline"}</span>
          </span>
          <button className="flex items-center gap-1.5 transition-colors duration-100 hover:text-fg" onClick={clearKey} title="Sign out">
            <Icon name="logout" size={13} />
            Sign out
          </button>
        </div>
      </div>
    </aside>
  );
}

const shortcuts: [string, string[]][] = [
  ["Command palette", [MOD, "K"]],
  ["Go to page", ["g", `then ${NAV.map((n) => n.key).join(" ")}`]],
  ["Focus search", ["/"]],
  ["Move in lists", ["j", "k"]],
  ["Open selected", ["Enter"]],
  ["Save config", [MOD, "S"]],
  ["Close dialog or clear", ["Esc"]],
  ["This list", ["?"]],
];

function Shortcuts({ onClose }: { onClose: () => void }) {
  return (
    <Dialog title="Keyboard shortcuts" onClose={onClose} width={420}>
      <ul className="px-5 py-2">
        {shortcuts.map(([label, keys]) => (
          <li key={label} className="flex h-9 items-center justify-between border-b border-line last:border-0">
            <span>{label}</span>
            <span className="flex items-center gap-1.5 text-[12px] text-muted">
              {keys.map((k) => (k.includes(" ") && k.startsWith("then") ? <span key={k} className="mono">{k}</span> : <Kbd key={k}>{k}</Kbd>))}
            </span>
          </li>
        ))}
      </ul>
    </Dialog>
  );
}
