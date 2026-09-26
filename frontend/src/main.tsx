import { StrictMode } from "react";
import { createRoot } from "react-dom/client";
import { ThemeProvider } from "next-themes";
import { ErrorBoundary } from "@sentry/react";

import { App } from "./App";
import { CrashFallback } from "@/components/crash-fallback";
import { TooltipProvider } from "@/components/ui/tooltip";
import { Toaster } from "@/components/ui/sonner";
import { applyStoredAccentPreset } from "@/lib/accent-presets";
import { purgeStoredSmtpPasswords } from "@/lib/domain";
import { installExternalLinkOpener } from "@/lib/external-links";
import { startScrollActivity } from "@/lib/scroll-activity";
import { initSentry, isReporting } from "@/lib/sentry";
import { OpenPanelTracking } from "@/lib/openpanel";
import "./index.css";

/**
 * The desktop needs no pre-mount step.
 *
 * There used to be one here: it invoked a `desktop_config` command, redeemed a
 * `dev_code` and reloaded the page with `?api=&code=`. That command is not in
 * this build's `generate_handler!`, so it always threw and the `catch` swallowed
 * it — and its `fetch` straight out of the webview is what the Rust proxy and
 * the CSP both exist to avoid. The desktop now boots like any other console and
 * discovers its hosts through the connection registry (see `App`).
 */
function mount(): void {
  const root = document.getElementById("root");
  if (!root) throw new Error("missing #root element");
  createRoot(root).render(
    <>
      <OpenPanelTracking />
      <StrictMode>
        {/*
          Outermost, outside ThemeProvider and TooltipProvider, because the thing
          that crashes may be one of them — a boundary inside a provider cannot
          catch that provider's own throw, and the symptom is the white page this
          exists to replace. `CrashFallback` depends on no context for the same
          reason.
        */}
        <ErrorBoundary
          fallback={({ error, resetError, eventId }) => (
            <CrashFallback
              error={error}
              // Only when an event actually left. The SDK mints an id locally
              // whether or not a DSN is configured; see `isReporting`.
              eventId={isReporting() ? eventId : null}
              onReset={resetError}
            />
          )}
        >
          <ThemeProvider
            attribute="class"
            defaultTheme="system"
            enableSystem
            disableTransitionOnChange
          >
            <TooltipProvider delay={200}>
              <App />
              <Toaster position="bottom-right" richColors closeButton />
            </TooltipProvider>
          </ThemeProvider>
        </ErrorBoundary>
      </StrictMode>
    </>,
  );
}

// Crash reporting first, before anything else runs and well before the first
// render — a crash during the first render is exactly the one worth reporting,
// and a boundary armed after it would miss it. Silent unless
// `VITE_SENTRY_DSN` is set: no console warning, no network, nothing to notice.
// See `docs/spec/runtime/crash-reporting.md`.
initSentry();

// Started before the first render and never disposed: it is one capturing
// listener for the life of the document, and every scroll container in the app
// — including ones mounted much later — depends on it for the `data-scrolling`
// mark the themed scrollbars in `index.css` lift on. Outside React on purpose,
// so StrictMode's double-invoked effects cannot arm it twice.
startScrollActivity();

// Outward links reach the operator's browser in the desktop shell (issue
// #2282). Armed the same way and for the same reason as the scroll listener
// above: one capturing listener for the life of the document, outside React so
// StrictMode cannot arm it twice, and covering anchors mounted long after boot
// — including ones nobody has written yet. Inert in a browser, where an anchor
// already does the right thing.
installExternalLinkOpener();

// Deletes SMTP passwords the pre-#1460 console wrote to localStorage, before
// the first render and therefore before anything can read one back. At boot
// rather than in the Settings card because the operator is not obliged to open
// Settings again, and the credential has to be gone either way. A no-op on any
// browser that never stored one.
purgeStoredSmtpPasswords();

// Synchronous, and before `mount()`, so the chosen accent preset is already on
// `<html>` for React's first commit — see `accent-presets.ts` for why this
// cannot be an inline `<script>` (client-rendered scripts are inert) or a
// `next-themes`-style trick (this SPA has no server render for that trick to
// run during). `next-themes` itself still applies `.dark` from an effect, one
// commit later — a pre-existing flash this call does not fix (issue #2493,
// `docs/issues/accent-theme-presets/open-questions.md` Q8).
applyStoredAccentPreset();

mount();
