import tailwindcss from "@tailwindcss/vite";
import react from "@vitejs/plugin-react";
import { fileURLToPath } from "node:url";
import { defineConfig } from "vite";

// Dev proxy target. Point it at the real server or at `bun run shim` (adds the
// usage extension endpoints on top of the Go server).
const backend = process.env.CPA_BACKEND ?? "http://127.0.0.1:18318";

export default defineConfig({
  // Relative asset URLs so the same build works at `/` and `/management.html`.
  base: "./",
  plugins: [react(), tailwindcss()],
  resolve: { alias: { "@": fileURLToPath(new URL("./src", import.meta.url)) } },
  server: {
    port: 5173,
    proxy: {
      "/v8": backend,
      "/v1": backend,
      "/healthz": backend,
    },
  },
  build: { outDir: "dist", emptyOutDir: true, target: "es2022", chunkSizeWarningLimit: 900 },
});
