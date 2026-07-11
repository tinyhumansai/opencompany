import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";

// The console is company-agnostic and talks to the OpenCompany operator API.
// In dev it proxies the API routes to a locally-running `opencompany serve`
// (default 127.0.0.1:8080), so the app is same-origin and needs no CORS.
// Override the target with OC_API_TARGET when the host runs elsewhere.
const API_TARGET = process.env.OC_API_TARGET ?? "http://127.0.0.1:8080";

export default defineConfig({
  plugins: [react()],
  server: {
    proxy: {
      "/api": { target: API_TARGET, changeOrigin: true },
      "/healthz": { target: API_TARGET, changeOrigin: true },
      "/spec": { target: API_TARGET, changeOrigin: true },
      "/tiny": { target: API_TARGET, changeOrigin: true },
    },
  },
});
