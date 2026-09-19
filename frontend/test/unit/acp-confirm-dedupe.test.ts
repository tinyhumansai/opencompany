// @vitest-environment jsdom

import { describe, expect, it } from "vitest";

import { confirmAcpHarness } from "@/api/transport/desktop";

/**
 * Two surfaces now ask the same question at the same moment: the External
 * harnesses panel probes on open, and the agent page's harness picker probes
 * when its editor opens. Each `confirm` is a real subprocess plus a JSON-RPC
 * handshake — seconds, not milliseconds — so the same harness being asked
 * twice concurrently is two CLIs started to produce one answer.
 *
 * De-duplication, not caching: the second *concurrent* ask joins the first,
 * and an ask made after it settles starts a fresh probe. Readiness is a fact
 * about the machine right now — an adapter can be installed between two looks,
 * which is exactly what the Install button does — so a settled answer must
 * never be replayed.
 */

interface Bridge {
  calls: string[];
  release: () => void;
}

function installBridge(): Bridge {
  const calls: string[] = [];
  let unblock: (() => void) | null = null;
  const gate = new Promise<void>((resolve) => {
    unblock = resolve;
  });
  (window as unknown as { __TAURI__: unknown }).__TAURI__ = {
    core: {
      invoke: async (command: string, args?: { id?: string }) => {
        calls.push(`${command}:${args?.id ?? ""}`);
        await gate;
        return { readiness: { state: "ready" }, models: [] };
      },
      Channel: class {
        onmessage: ((message: string) => void) | null = null;
      },
    },
  };
  return { calls, release: () => unblock?.() };
}

function uninstallBridge(): void {
  delete (window as unknown as { __TAURI__?: unknown }).__TAURI__;
}

describe("confirming a coding harness twice at once", () => {
  it("starts one probe for two concurrent asks, and a fresh one after it settles", async () => {
    const bridge = installBridge();
    try {
      const first = confirmAcpHarness("claude");
      const second = confirmAcpHarness("claude");

      // Both callers are waiting on the same probe, so nothing has been
      // started twice — the assertion that fails without the in-flight map.
      expect(bridge.calls).toEqual(["oc_acp_confirm_harness:claude"]);

      bridge.release();
      const [a, b] = await Promise.all([first, second]);
      expect(a).toEqual({ readiness: { state: "ready" }, models: [], path: undefined });
      // Literally the same answer object: they shared one probe rather than
      // two that happened to agree.
      expect(b).toBe(a);
      expect(bridge.calls).toEqual(["oc_acp_confirm_harness:claude"]);

      // Settled, so the next ask is a new look at the machine rather than a
      // replay. An adapter installed in between has to be visible.
      await confirmAcpHarness("claude");
      expect(bridge.calls).toEqual([
        "oc_acp_confirm_harness:claude",
        "oc_acp_confirm_harness:claude",
      ]);
    } finally {
      bridge.release();
      uninstallBridge();
    }
  });

  it("keeps two different harnesses apart", async () => {
    const bridge = installBridge();
    try {
      const claude = confirmAcpHarness("claude");
      const codex = confirmAcpHarness("codex");
      expect(bridge.calls).toEqual([
        "oc_acp_confirm_harness:claude",
        "oc_acp_confirm_harness:codex",
      ]);
      bridge.release();
      await Promise.all([claude, codex]);
    } finally {
      bridge.release();
      uninstallBridge();
    }
  });
});
