import { defineConfig, type PluginOption } from "vite";
import react from "@vitejs/plugin-react";
import tailwindcss from "@tailwindcss/vite";
import path from "node:path";

// Redirect /admin/ui → /admin/ui/ in dev server so the behaviour
// matches the production rust-embed handler (308 with trailing slash).
function trailingSlashPlugin(): PluginOption {
  return {
    name: "trailing-slash-redirect",
    configureServer(server) {
      server.middlewares.use((req, _res, next) => {
        const url = req.url ?? "";
        if (url === "/admin/ui" || url.startsWith("/admin/ui?")) {
          const qs = url.includes("?") ? url.slice(url.indexOf("?")) : "";
          _res.writeHead(308, { Location: `/admin/ui/${qs}` });
          _res.end();
          return;
        }
        next();
      });
    },
  };
}

// The SPA is mounted behind `/admin/ui` by the embedding server
// (rust-embed in tiygate-server). The `base` must match that prefix
// so all asset URLs resolve under it.
export default defineConfig({
  base: "/admin/ui/",
  plugins: [trailingSlashPlugin(), react(), tailwindcss()],
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
