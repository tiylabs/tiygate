import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";
import tailwindcss from "@tailwindcss/vite";
import path from "node:path";

// The SPA is mounted behind `/admin/ui` by the embedding server
// (rust-embed in tiygate-server). The `base` must match that prefix
// so all asset URLs resolve under it.
export default defineConfig({
  base: "/admin/ui/",
  plugins: [react(), tailwindcss()],
  resolve: {
    alias: {
      "@": path.resolve(__dirname, "src"),
    },
  },
  build: {
    outDir: "dist",
    emptyOutDir: true,
  },
  server: {
    port: 5173,
    // During local dev, proxy the admin API to a running gateway so
    // the SPA can talk to a real backend without CORS.
    proxy: {
      "/admin/v1": {
        target: "http://localhost:3000",
        changeOrigin: true,
      },
    },
  },
});
