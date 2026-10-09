/**
 * Watches one MCP server's browser sign-in from the console side.
 *
 * The host's OAuth callback lands in another tab and sends nothing back, so the
 * console learns about success only by probing the server until it reports
 * `ok`. Each watch owns its own stop flag rather than sharing a component-wide
 * "unmounted" marker, so a StrictMode mount/cleanup/mount cycle cannot silence
 * a watch started after it.
 */
import type { McpHealth } from "@/api/types";

/** How long the console keeps probing before it shows the timed-out state. */
export const SIGN_IN_DEADLINE_MS = 5 * 60_000;

/** Gap between two automatic probes. */
export const SIGN_IN_POLL_MS = 2_000;

export interface SignInWatchOptions {
  probe: () => Promise<McpHealth>;
  onProbe: (health: McpHealth) => void;
  onConnected: (health: McpHealth) => void;
  onTimeout: () => void;
  deadlineMs?: number;
  intervalMs?: number;
  now?: () => number;
}

export interface SignInWatch {
  /** Probe right away, also after the deadline passed (focus, "Check now"). */
  checkNow: () => void;
  stop: () => void;
  readonly stopped: boolean;
  readonly timedOut: boolean;
}

/** Start watching; the first automatic probe runs after one interval. */
export function watchSignIn(options: SignInWatchOptions): SignInWatch {
  const now = options.now ?? Date.now;
  const interval = options.intervalMs ?? SIGN_IN_POLL_MS;
  const deadline = now() + (options.deadlineMs ?? SIGN_IN_DEADLINE_MS);
  let timer: ReturnType<typeof setTimeout> | undefined;
  let stopped = false;
  let timedOut = false;
  let inFlight = false;

  const clear = () => {
    if (timer !== undefined) clearTimeout(timer);
    timer = undefined;
  };

  const arm = () => {
    clear();
    if (stopped || timedOut) return;
    timer = setTimeout(() => void tick(), interval);
  };

  const tick = async () => {
    timer = undefined;
    if (stopped) return;
    if (!timedOut && now() > deadline) {
      timedOut = true;
      options.onTimeout();
      return;
    }
    if (inFlight) return arm();
    inFlight = true;
    let health: McpHealth | undefined;
    try {
      health = await options.probe();
    } catch {
      health = undefined;
    } finally {
      inFlight = false;
    }
    if (stopped) return;
    if (health) {
      options.onProbe(health);
      if (health.status === "ok") {
        stopped = true;
        clear();
        options.onConnected(health);
        return;
      }
    }
    arm();
  };

  arm();

  return {
    checkNow: () => {
      if (stopped || inFlight) return;
      clear();
      void tick();
    },
    stop: () => {
      stopped = true;
      clear();
    },
    get stopped() {
      return stopped;
    },
    get timedOut() {
      return timedOut;
    },
  };
}
