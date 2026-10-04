# Management UI

React, TypeScript (strict), Vite, Tailwind v4, bun. Dark only. Talks to the v8
management API (`/v8/management/...`); optional additions are specified in
[API_EXTENSIONS.md](API_EXTENSIONS.md) and degrade gracefully when absent.

```
bun install
bun run typecheck
bun run build        # static site in ui/dist (relative asset URLs, hash routing)
```

`ui/dist` is what the Rust binary embeds and serves at `/` and `/management.html`.

## Develop

Run a throwaway Go server (own config and auth dir, never `~/.cli-proxy-api`),
then either use the Vite dev server or the shim:

```
bun dev/shim.ts            # :18318, proxies to :18317, adds the extension endpoints, serves dist/
CPA_BACKEND=http://127.0.0.1:18318 bun run dev
```

`SHIM_DEMO=1` seeds synthetic usage history, `SHIM_NO_EXT=1` hides the extension
endpoints. `dev/fake-upstream.ts` and `dev/seed.ts` generate real traffic and
credential files for the Go server.

## Verify

```
bun dev/e2e.ts     # mutating flows through the real UI, writes screenshots/e2e-report.txt
bun dev/shots.ts   # every screen into screenshots/*.png
```

Quotas, Overview and Use with tools against the Rust server (debug builds serve
`ui/dist` from disk, so rebuilding the UI needs no server rebuild):

```
bun dev/fake-upstream.ts &
cargo run -p cpa-server --bin cliproxy -- --config ui/dev/rust.yaml &
bun dev/seed.ts /tmp/cpa-ui-rust/auth http://127.0.0.1:18530 0
bun dev/e2e-quotas.ts   # writes screenshots/quotas-e2e-report.txt and quotas-e2e-*.png
```

All need Playwright's Chromium; set `CHROMIUM_PATH` to use another build.

## Keys

`Ctrl/Cmd K` palette, `g` then `o t c q k p m y l` to navigate, `/` search, `j k Enter`
in tables, `e` toggles the selected credential, `Backspace` deletes the selected row,
`Ctrl/Cmd S` saves config, `?` lists them.
