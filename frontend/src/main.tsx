import { StrictMode } from "react";
import { createRoot } from "react-dom/client";
import { ThemeProvider } from "next-themes";

import { App } from "./App";
import { TooltipProvider } from "@/components/ui/tooltip";
import { Toaster } from "@/components/ui/sonner";
import "./index.css";

type DesktopConfig = { api_url: string; company: string; operator_email: string };

/**
 * The browser build has no native bridge and starts normally. In the packaged
 * Tauri build, redeem the loopback-only desktop invite before React mounts so
 * the existing console keeps its normal cookie-authenticated API contract.
 */
async function bootstrapDesktop(): Promise<void> {
  try {
    const { invoke } = await import("@tauri-apps/api/core");
    const config = await invoke<DesktopConfig>("desktop_config");
    const current = new URL(window.location.href);
    if (current.searchParams.has("code")) return;

    const response = await fetch(`${config.api_url}/api/v1/companies/${config.company}/auth/request`, {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({ email: config.operator_email }),
    });
    const result = (await response.json()) as { dev_code?: string };
    if (!response.ok || !result.dev_code) throw new Error("desktop login code unavailable");

    current.searchParams.set("api", config.api_url);
    current.searchParams.set("company", config.company);
    current.searchParams.set("code", result.dev_code);
    window.location.replace(current.toString());
  } catch {
    // `@tauri-apps/api` is absent in an ordinary web deployment; it must retain
    // the existing same-origin startup behavior.
  }
}

async function mount(): Promise<void> {
  await bootstrapDesktop();
  const root = document.getElementById("root");
  if (!root) throw new Error("missing #root element");
  createRoot(root).render(
    <StrictMode>
      <ThemeProvider attribute="class" defaultTheme="system" enableSystem disableTransitionOnChange>
        <TooltipProvider delay={200}>
          <App />
          <Toaster position="bottom-right" richColors closeButton />
        </TooltipProvider>
      </ThemeProvider>
    </StrictMode>,
  );
}

void mount();
