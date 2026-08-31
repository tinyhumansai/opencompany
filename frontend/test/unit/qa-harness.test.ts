/**
 * The QA harness's judgements, pinned (issue #987).
 *
 * `qa/oc-qa.js` is pasted into a browser console, so nothing imports it and
 * nothing type-checks it. Two things about it are worth a gate anyway:
 *
 * 1. **It parses.** A syntax error is discovered by an operator mid-incident,
 *    which is the worst possible moment and the only moment it is ever run.
 *
 * 2. **Its run verdict agrees with the console's.** The harness owns a
 *    transcription of `run-health.ts`, and a second definition of "did this run
 *    succeed" is precisely the defect issue #981 filed against the product —
 *    where the console's TypeScript held the only verdict and every API client
 *    folding `nodes[].status` scored a dropped report as green. The harness made
 *    exactly that mistake and scored a delivery-failure run as PASS. Pinning the
 *    two together is what stops a change to the console's reading from silently
 *    re-greening a bad run in the harness.
 *
 * The script is evaluated in a `vm` sandbox rather than imported: it is an IIFE
 * that assigns `globalThis.OCQA`, with no `export`, because an `export` would
 * make it unpasteable — which is the one property it must not lose.
 */

import { readFileSync } from "node:fs";
import { dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { createContext, runInNewContext } from "node:vm";
import { describe, expect, it } from "vitest";

import {
  isUndelivered as consoleIsUndelivered,
  runTone,
  verdictOf,
} from "@/views/workflows/run-health";
import type {
  DeliveryReport,
  WorkflowBlockedNode,
  WorkflowRunOutcome,
} from "@/api/workflows";

const here = dirname(fileURLToPath(import.meta.url));
const SCRIPT = resolve(here, "../../../qa/oc-qa.js");

/** One reported check, as the script emits it. */
interface Row {
  check: string;
  verdict: "PASS" | "WARN" | "FAIL" | "SKIP";
  value: string;
  note: string;
  /** Tenant content, held apart from `value` so `report()` can withhold it. */
  detail?: string;
}

/** A minimal `Response` stand-in — the four things `http()` reads. */
function response(status: number, body: unknown, headers: Record<string, string> = {}) {
  return {
    ok: status >= 200 && status < 300,
    status,
    text: async () => (body === undefined ? "" : JSON.stringify(body)),
    headers: new Headers(headers),
  };
}

/** Loads `oc-qa.js` into a sandbox and hands back its `_internals`. */
function loadHarness(fetchImpl?: (path: string, init?: { method?: string }) => Promise<unknown>) {
  const source = readFileSync(SCRIPT, "utf8");
  const sandbox: Record<string, unknown> = {
    console: { log: () => {}, table: () => {} },
    fetch:
      fetchImpl ??
      (async () => {
        throw new Error("no network in this test");
      }),
    setTimeout,
    clearTimeout,
    AbortController,
    Headers: globalThis.Headers,
    location: { host: "test" },
  };
  createContext(sandbox);
  runInNewContext(source, sandbox);
  const ocqa = sandbox.OCQA as {
    version: string;
    read: (options?: { company?: string }) => Promise<Row[]>;
    probe: (options?: { company?: string; workflow?: string; dryRun?: boolean }) => Promise<Row[]>;
    report: (options?: { raw?: boolean }) => string;
    _internals: {
      runVerdict: (run: unknown) => string;
      undeliveredCount: (d: DeliveryReport[]) => number;
      isUndelivered: (d: DeliveryReport) => boolean;
      checkDeliveries: (
        rows: { check: string; verdict: string; value: string }[],
        runs: unknown[],
      ) => void;
      pendingCount: (d: DeliveryReport[]) => number;
      awaitingCount: (run: unknown) => number;
      isBlocked: (run: unknown) => boolean;
      judgeCacheHeader: (kind: string, header: string | null) => { verdict: string; note: string };
      age: (atMillis: number, now?: number) => string;
      notWired: (res: { body: unknown }) => boolean;
      secs: (ms: number) => string;
    };
  };
  return ocqa;
}

/** A run outcome with only the fields the verdict reads. */
function run(overrides: Partial<WorkflowRunOutcome>): WorkflowRunOutcome {
  return {
    seq: 1,
    atMillis: 1_700_000_000_000,
    workflowId: "daily-release-readiness",
    scheduled: false,
    deliveries: [],
    pendingApprovals: [],
    ...overrides,
  } as WorkflowRunOutcome;
}

function delivery(
  status: DeliveryReport["status"],
  reason?: string,
): DeliveryReport {
  return { node: "report", kind: "channel", target: "operator", status, detail: "", reason };
}

/** The console's label for a run, mapped to the harness's verdict word. */
const TONE_TO_VERDICT: Record<string, string> = {
  running: "running",
  failed: "failed",
  stopped: "stopped",
  stranded: "stranded",
  blocked: "blocked",
  "not delivered": "undelivered",
  "awaiting approval": "awaiting-approval",
  degraded: "degraded",
  ok: "ok",
};

/** A node the run stopped short at, waiting on a person (issue #881). */
function blocked(nodeId: string): WorkflowBlockedNode {
  return { nodeId, tools: ["send_email"] };
}

describe("oc-qa.js loads", () => {
  it("parses and exposes both entry points", () => {
    const ocqa = loadHarness();
    expect(typeof ocqa.read).toBe("function");
    expect(typeof ocqa.probe).toBe("function");
    expect(typeof ocqa.report).toBe("function");
    expect(ocqa.version).toMatch(/^\d+\.\d+\.\d+$/);
  });
});

describe("read() against a host whose surfaces do not answer", () => {
  /**
   * Rule 2 of the harness, end to end: **unreadable is never PASS.**
   *
   * The 2026-08-18 pass wrote three checks up as green that had never run, and
   * a pure-function test cannot catch that — the mistake lives in the plumbing,
   * where a 404 body decays to `{}` and every field read off it comes back
   * `undefined`. So this drives the real `read()` over a host that answers
   * `/healthz`, `/spec` and the company status and 404s everything else, and
   * asserts nothing downstream claims to have passed.
   */
  const reachable: Record<string, unknown> = {
    "/healthz": { status: "ok" },
    "/spec": {
      name: "opencompany",
      version: "0.1.0",
      capabilities: ["rest", "graphql"],
      storage: "memory",
      instance_id: "abcdef0123456789",
    },
    "/api/v1/company": { id: "acme", name: "Acme", lifecycle: "running", pending_approvals: 0 },
  };

  async function runRead(): Promise<Row[]> {
    const ocqa = loadHarness(async (path: string) =>
      path in reachable ? response(200, reachable[path]) : response(404, { error: "not found" }),
    );
    return ocqa.read();
  }

  it("emits exactly the 22 checks the docs claim", async () => {
    // Pins the number in `qa/README.md` and `qa/MASTER-QA.md` to the code. The
    // cache-header check expands to three rows on a host that actually serves a
    // console; here the shell 404s, so it contributes its single SKIP.
    const rows = await runRead();
    expect(rows).toHaveLength(22);
    expect(new Set(rows.map((r) => r.check)).size).toBe(22);
  });

  it("passes only what it could actually read", async () => {
    const rows = await runRead();
    const passed = rows.filter((r) => r.verdict === "PASS").map((r) => r.check);
    expect(passed.sort()).toEqual(["company-lifecycle", "host"]);
  });

  it("reports every unreadable surface as untested rather than as passed", async () => {
    const rows = await runRead();
    const unreadable = rows.filter((r) => r.check !== "host" && r.check !== "company-lifecycle");
    // `repo-binding` is SKIP by construction — the build binding needs a human.
    // `approval-tier` is SKIP here because this host 404s `{scope}/policy`; the
    // real pass over a readable policy is pinned in the describe below.
    for (const r of unreadable) {
      expect(r.verdict, `${r.check} judged a surface it never read`).toBe("SKIP");
    }
  });

  it("never renders an absent value as a confident zero", async () => {
    // A 404 body decays to `{}`, so a check that formats `body.count` prints
    // `undefined` or `0` and reads as a real, healthy reading. Every SKIP row
    // must say why instead.
    const rows = await runRead();
    for (const r of rows.filter((x) => x.verdict === "SKIP")) {
      expect(r.note, `${r.check} skipped without saying why`).not.toBe("");
    }
  });
});

describe("read() can verify the approval tier", () => {
  // `{scope}/policy` is a real read surface (`src/server/ops/policy.rs`, #562):
  // a GET that `read_policy` answers with the tier actually in force, well
  // before the approval gate's own resolver rebuilt the roster. These lock
  // that the row grades it a real PASS instead of always SKIP, and that it
  // tells a `supervised` tenant with nothing pending apart from a `full` one.
  function policyHost(extra: Record<string, unknown>) {
    const reachable: Record<string, unknown> = {
      "/healthz": { status: "ok" },
      "/spec": {
        name: "opencompany",
        version: "0.1.0",
        capabilities: ["rest", "graphql"],
        storage: "memory",
        instance_id: "abcdef0123456789",
      },
      "/api/v1/company": { id: "acme", name: "Acme", lifecycle: "running", pending_approvals: 0 },
      ...extra,
    };
    const ocqa = loadHarness(async (path: string) =>
      path in reachable ? response(200, reachable[path]) : response(404, { error: "not found" }),
    );
    return ocqa;
  }

  const policyDto = {
    mode: "supervised",
    alwaysApprove: [],
    autoApproveUnderUsd: null,
    approvalTtlHours: 24,
    manifestMode: "full",
    manifestAlwaysApprove: [],
    manifestAutoApproveUnderUsd: null,
    manifestApprovalTtlHours: null,
    overridden: true,
    setBy: "alice@acme",
    setAtMillis: 1_700_000_000_000,
    tiers: [],
    takesEffect: "on the next turn",
  };

  it("passes approval-tier with the tier in force when {scope}/policy answers", async () => {
    const ocqa = policyHost({ "/api/v1/company/policy": policyDto });
    const rows = await ocqa.read();
    const tier = rows.find((r) => r.check === "approval-tier");
    expect(tier).toBeDefined();
    expect(tier!.verdict).toBe("PASS");
    expect(tier!.value).toContain("mode supervised");
    expect(tier!.value).toContain("manifest full");
    expect(tier!.value).toContain("overridden by alice@acme");
  });

  it("distinguishes a full-tier tenant with nothing parked from a missing gate", async () => {
    const ocqa = policyHost({
      "/api/v1/company/policy": {
        mode: "full",
        alwaysApprove: [],
        autoApproveUnderUsd: null,
        approvalTtlHours: 24,
        manifestMode: "full",
        manifestAlwaysApprove: [],
        manifestAutoApproveUnderUsd: null,
        manifestApprovalTtlHours: null,
        overridden: false,
        tiers: [],
        takesEffect: "on the next turn",
      },
    });
    const rows = await ocqa.read();
    const tier = rows.find((r) => r.check === "approval-tier");
    expect(tier!.verdict).toBe("PASS");
    expect(tier!.value).toContain("mode full");
    expect(tier!.note).toContain("no effect ever parks");
  });

  it("warns when a full tier is nonetheless holding parked effects", async () => {
    const ocqa = policyHost({
      "/api/v1/company/policy": {
        mode: "full",
        alwaysApprove: [],
        autoApproveUnderUsd: null,
        approvalTtlHours: 24,
        manifestMode: "full",
        manifestAlwaysApprove: [],
        manifestAutoApproveUnderUsd: null,
        manifestApprovalTtlHours: null,
        overridden: false,
        tiers: [],
        takesEffect: "on the next turn",
      },
      "/api/v1/company/approvals": [{ id: "a1", kind: "chase", at_millis: 1_700_000_000_000 }],
    });
    const rows = await ocqa.read();
    const tier = rows.find((r) => r.check === "approval-tier");
    expect(tier!.verdict).toBe("WARN");
    expect(tier!.value).toContain("1 parked");
  });
});

describe("runVerdict agrees with the console's runTone", () => {
  /**
   * Every arm of the precedence order, including the two that only exist
   * because leaving them out reads as success: a run still in flight and a run
   * somebody stopped both fall through to green without their own arm.
   */
  const cases: Array<{ name: string; run: WorkflowRunOutcome; label: string }> = [
    { name: "still running", run: run({ running: true }), label: "running" },
    {
      // #1865: `on_error: continue | route` and the iteration cap leave a node
      // in error without failing the run, so every reading above this one is
      // absent — no error, not cancelled, nothing parked, everything
      // delivered. Before the harness grew this arm it fell through to `ok`
      // and the release probe reported PASS on a run with a broken step.
      name: "a node errored while the run carried on",
      run: run({
        nodes: [
          { nodeId: "fetch", status: "ok" },
          { nodeId: "summarise", status: "error" },
        ],
      } as Partial<WorkflowRunOutcome>),
      label: "degraded",
    },
    {
      // Degraded sits last for a reason: anything the operator can still act
      // on outranks it.
      name: "a gate outranks a degraded node",
      run: run({
        blockedNodes: [blocked("approve")],
        nodes: [{ nodeId: "summarise", status: "error" }],
      } as Partial<WorkflowRunOutcome>),
      label: "blocked",
    },
    {
      name: "running outranks a failure it has not hit yet",
      run: run({ running: true, error: "boom" }),
      label: "running",
    },
    { name: "errored", run: run({ error: "node exploded" }), label: "failed" },
    {
      name: "stopped, judged before its deliveries",
      run: run({ cancelled: true, deliveries: [delivery("failed")] } as Partial<WorkflowRunOutcome>),
      label: "stopped",
    },
    {
      // #881: no error, not cancelled, not running, and no report routed — so
      // before this arm existed it fell through to green and told the operator
      // a pipeline that delivered nothing had succeeded.
      name: "stopped short at a gate",
      run: run({ blockedNodes: [blocked("approve")] }),
      label: "blocked",
    },
    {
      name: "blocked is judged before the delivery rows",
      run: run({ blockedNodes: [blocked("approve")], deliveries: [delivery("failed")] }),
      label: "blocked",
    },
    {
      // #1189: the shape that scored `awaiting-approval` forever. Every gate
      // has lost its card, so both readings below it — "blocked" and "awaiting
      // approval" — tell the operator to go and decide something that is not
      // there.
      name: "every gate has lost its card",
      run: run({
        pendingApprovals: ["fetch_bbc", "fetch_espn"],
        strandedApprovals: 2,
      } as Partial<WorkflowRunOutcome>),
      label: "stranded",
    },
    {
      name: "stranded outranks blocked",
      run: run({
        blockedNodes: [blocked("approve")],
        pendingApprovals: ["approve"],
        strandedApprovals: 1,
      } as Partial<WorkflowRunOutcome>),
      label: "stranded",
    },
    {
      // The negative: one gate is still decidable, so the run is still awaiting
      // and the per-node count carries the loss.
      name: "a partly stranded run is still awaiting",
      run: run({
        pendingApprovals: ["fetch_bbc", "fetch_espn"],
        strandedApprovals: 1,
      } as Partial<WorkflowRunOutcome>),
      label: "awaiting approval",
    },
    {
      name: "every node ok but the report was dropped (#981)",
      run: run({ deliveries: [delivery("failed")] }),
      label: "not delivered",
    },
    {
      name: "skipped counts as undelivered",
      run: run({ deliveries: [delivery("skipped")] }),
      label: "not delivered",
    },
    {
      name: "denied counts as undelivered",
      run: run({ deliveries: [delivery("denied")] }),
      label: "not delivered",
    },
    {
      name: "undelivered outranks pending",
      run: run({ deliveries: [delivery("pending"), delivery("failed")] }),
      label: "not delivered",
    },
    {
      name: "parked for approval is not a failure",
      run: run({ deliveries: [delivery("pending")] }),
      label: "awaiting approval",
    },
    {
      // #846: the gated shape. It never reached an output node, so `deliveries`
      // is empty and a delivery-only read scored it as a clean run.
      name: "paused at a gate, having routed no report at all",
      run: run({ pendingApprovals: ["approve"] }),
      label: "awaiting approval",
    },
    { name: "delivered", run: run({ deliveries: [delivery("sent")] }), label: "ok" },
    { name: "nothing to deliver", run: run({}), label: "ok" },
  ];

  for (const c of cases) {
    it(c.name, () => {
      const { runVerdict } = loadHarness()._internals;
      // Both halves asserted: the console still reads it the way this table
      // says, AND the harness agrees with the console. Asserting only the
      // second would pass vacuously if both drifted together.
      expect(runTone(c.run).label).toBe(c.label);
      expect(runVerdict(c.run)).toBe(TONE_TO_VERDICT[c.label]);
    });
  }

  it("folds gates and parked reports into one waiting-on-a-person count (#846)", () => {
    const { awaitingCount } = loadHarness()._internals;
    // Either half alone is a reading that scored a waiting run as finished.
    expect(awaitingCount(run({ pendingApprovals: ["a"], deliveries: [] }))).toBe(1);
    expect(awaitingCount(run({ pendingApprovals: [], deliveries: [delivery("pending")] }))).toBe(1);
    expect(
      awaitingCount(run({ pendingApprovals: ["a", "b"], deliveries: [delivery("pending")] })),
    ).toBe(3);
    expect(awaitingCount(run({}))).toBe(0);
  });

  it("counts deliveries the same way the console does", () => {
    const { undeliveredCount, pendingCount } = loadHarness()._internals;
    const deliveries = [
      delivery("sent"),
      delivery("pending"),
      delivery("failed"),
      delivery("skipped"),
    ];
    expect(undeliveredCount(deliveries)).toBe(2);
    expect(pendingCount(deliveries)).toBe(1);
  });

  /**
   * Issue #981's second half. The harness is pasted against whatever host is in
   * front of the operator, so its fallback has to move rung-for-rung with the
   * host's `is_undelivered` and the console's `isUndelivered` — otherwise the
   * three readings of the same rows diverge exactly where nobody is checking.
   */
  it("excuses the same two skipped reasons the console and the host excuse", () => {
    const { isUndelivered, undeliveredCount } = loadHarness()._internals;
    expect(isUndelivered(delivery("skipped", "dry-run"))).toBe(false);
    expect(isUndelivered(delivery("skipped", "already-delivered"))).toBe(false);
    // The deliberate non-move: this report was produced and then lost.
    expect(isUndelivered(delivery("skipped", "no-destination-configured"))).toBe(
      true,
    );
    // A host predating issue #248 records no reason; an unreadable one counts.
    expect(isUndelivered(delivery("skipped"))).toBe(true);
    // The exemptions are scoped to a skip.
    expect(isUndelivered(delivery("failed", "dry-run"))).toBe(true);
    expect(
      undeliveredCount([
        delivery("skipped", "dry-run"),
        delivery("skipped", "already-delivered"),
        delivery("skipped", "no-destination-configured"),
      ]),
    ).toBe(1);
  });

  it("agrees with the console's isUndelivered on every reason", () => {
    const { isUndelivered } = loadHarness()._internals;
    for (const row of [
      delivery("sent", "channel-posted"),
      delivery("pending", "parked-for-approval"),
      delivery("skipped", "dry-run"),
      delivery("skipped", "already-delivered"),
      delivery("skipped", "no-destination-configured"),
      delivery("skipped", "recipient-not-established"),
      delivery("denied", "email-not-granted"),
      delivery("failed", "channel-not-wired"),
    ]) {
      expect(isUndelivered(row), `${row.status}/${row.reason}`).toBe(
        consoleIsUndelivered(row),
      );
    }
  });

  /**
   * The `workflow-deliveries` row is what an operator running the harness
   * actually reads, and it owned a NINTH copy of the filter. A company whose
   * most recent runs were **test** runs — the safest thing an operator can do —
   * scored a red FAIL for a delivery path that is working perfectly.
   */
  it("does not fail the delivery check on accounted-for skips", () => {
    const { checkDeliveries } = loadHarness()._internals;
    const rows: { check: string; verdict: string; value: string }[] = [];
    checkDeliveries(rows, [
      {
        workflowId: "digest",
        deliveries: [
          delivery("skipped", "dry-run"),
          delivery("skipped", "already-delivered"),
          delivery("sent", "channel-posted"),
        ],
      },
    ]);
    const r = rows.find((x) => x.check === "workflow-deliveries");
    expect(r?.verdict).toBe("PASS");
    expect(r?.value).toContain("0/3 reports dropped");
  });

  it("still fails it for a report that was produced and lost", () => {
    const { checkDeliveries } = loadHarness()._internals;
    const rows: { check: string; verdict: string; value: string }[] = [];
    checkDeliveries(rows, [
      {
        workflowId: "digest",
        deliveries: [
          delivery("failed", "channel-not-wired"),
          // The deliberate non-move (issue #925).
          delivery("skipped", "no-destination-configured"),
        ],
      },
    ]);
    const r = rows.find((x) => x.check === "workflow-deliveries");
    expect(r?.verdict).toBe("FAIL");
    expect(r?.value).toContain("2/2 reports dropped");
  });

  it("reports an unknown run as unknown rather than ok", () => {
    const { runVerdict } = loadHarness()._internals;
    expect(runVerdict(null)).toBe("unknown");
  });
});

/**
 * The host owns the verdict now (issue #981, part 2).
 *
 * The table above still exercises the ladder, because none of its runs carries
 * a `verdict` — that is the fallback path, kept for a host predating this. What
 * the cases here pin is that the fallback stays a fallback: when the host does
 * answer, both readers take its word and neither re-derives one of its own.
 */
describe("the host's verdict is what both readers use", () => {
  it("is taken verbatim by the console and by the harness", () => {
    const { runVerdict } = loadHarness()._internals;
    const delivered = run({
      deliveries: [delivery("sent")],
      verdict: "undelivered",
    });
    // Deliberately contradictory: every local fact says `ok`. If either reader
    // still owns a definition, it answers `ok` here.
    expect(runVerdict(delivered)).toBe("undelivered");
    expect(runTone(delivered).label).toBe("not delivered");
    expect(runTone(delivered).dot).toContain("status-failed");
  });

  it("falls back to the ladder when the host sends none", () => {
    const { runVerdict } = loadHarness()._internals;
    const dropped = run({ deliveries: [delivery("failed")] });
    expect(dropped.verdict).toBeUndefined();
    expect(runVerdict(dropped)).toBe("undelivered");
    expect(runTone(dropped).label).toBe("not delivered");
  });

  it("ignores a word it cannot read rather than painting it green", () => {
    // `verdict` is host-controlled and a host may grow an eighth word. Falling
    // back to "ok" for one we cannot read would be the single arm nothing on
    // screen could contradict — so an unknown word drops to the ladder, which
    // reads the rows the host also sent.
    const dropped = run({
      deliveries: [delivery("failed")],
      verdict: "quantum-superposition" as never,
    });
    expect(runTone(dropped).label).toBe("not delivered");
    expect(runTone(dropped).dot).toContain("status-failed");
    expect(verdictOf(dropped)).toBe("undelivered");
  });

  it("agrees with the ladder on every word, so the two are interchangeable", () => {
    const { runVerdict } = loadHarness()._internals;
    for (const verdict of [
      "running",
      "failed",
      "stopped",
      "blocked",
      "undelivered",
      "awaiting-approval",
      "ok",
    ] as const) {
      const settled = run({ verdict });
      expect(runVerdict(settled)).toBe(verdict);
      expect(TONE_TO_VERDICT[runTone(settled).label]).toBe(verdict);
    }
  });
});

describe("judgeCacheHeader — the #979 reading", () => {
  it("fails an HTML response with no cache-control at all", () => {
    // The bug exactly: no header means heuristic caching, so the returning
    // browser keeps yesterday's shell. An absent header must not be a WARN.
    const { judgeCacheHeader } = loadHarness()._internals;
    expect(judgeCacheHeader("html", null).verdict).toBe("FAIL");
    expect(judgeCacheHeader("html", "").verdict).toBe("FAIL");
  });

  it("fails an HTML response the browser is allowed to reuse", () => {
    const { judgeCacheHeader } = loadHarness()._internals;
    expect(judgeCacheHeader("html", "public, max-age=3600").verdict).toBe("FAIL");
  });

  it("passes an HTML response that revalidates", () => {
    const { judgeCacheHeader } = loadHarness()._internals;
    for (const header of ["no-cache", "no-store", "public, max-age=0, must-revalidate"]) {
      expect(judgeCacheHeader("html", header).verdict, header).toBe("PASS");
    }
  });

  it("passes a hashed asset cached long, and only warns when it is not", () => {
    const { judgeCacheHeader } = loadHarness()._internals;
    expect(judgeCacheHeader("asset", "public, max-age=31536000, immutable").verdict).toBe("PASS");
    expect(judgeCacheHeader("asset", "public, max-age=86400").verdict).toBe("PASS");
    // Never a FAIL: a short-lived hashed asset costs a revalidation, not a
    // white screen. Only the shell can break the app by being cached.
    expect(judgeCacheHeader("asset", null).verdict).toBe("WARN");
    expect(judgeCacheHeader("asset", "public, max-age=60").verdict).toBe("WARN");
  });
});

describe("notWired — a feature absent from the build is untested, not failed", () => {
  /**
   * Found by running the harness against a real default-feature host: it
   * answered `POST …/workflows/{id}/run` with
   * `404 {"code":"not_wired"}` — "this deployment has no workflow runner" —
   * and the probe scored it FAIL, which would send somebody chasing a graph
   * that was fine.
   */
  it("recognises the typed code", () => {
    const { notWired } = loadHarness()._internals;
    expect(notWired({ body: { error: "workflow execution is not wired", code: "not_wired" } })).toBe(
      true,
    );
  });

  it("does not match on the prose, only the code (#248)", () => {
    // The message is free to be reworded; a check that grepped it would go
    // quiet the day somebody did, and silently start scoring absent features
    // as failures again.
    const { notWired } = loadHarness()._internals;
    expect(notWired({ body: { error: "workflow execution is not wired in this deployment" } })).toBe(
      false,
    );
    expect(notWired({ body: { error: "boom", code: "internal" } })).toBe(false);
    expect(notWired({ body: null })).toBe(false);
  });
});

describe("secs", () => {
  it("keeps a sub-second reading legible instead of rounding it to 0.0s", () => {
    // The live run printed every probe as "0.0s", which reads as a broken
    // clock rather than a fast host — and latency is one of the values the
    // chat probe's verdict is formed from.
    const { secs } = loadHarness()._internals;
    expect(secs(0)).toBe("0ms");
    expect(secs(42)).toBe("42ms");
    expect(secs(999)).toBe("999ms");
    expect(secs(1000)).toBe("1.0s");
    expect(secs(6040)).toBe("6.0s");
    expect(secs(121_000)).toBe("121.0s");
  });
});

describe("age", () => {
  it("reports absent timestamps as n/a rather than as 'just now'", () => {
    // `memoryStats.factsUpdatedAtMillis` and friends are `0` when nothing has
    // been written. Rendering that as a fresh timestamp would report an empty
    // company as an active one.
    const { age } = loadHarness()._internals;
    expect(age(0)).toBe("n/a");
  });

  it("scales from seconds to days", () => {
    const { age } = loadHarness()._internals;
    const now = 1_700_000_000_000;
    expect(age(now - 30_000, now)).toBe("30s");
    expect(age(now - 5 * 60_000, now)).toBe("5m");
    expect(age(now - 4 * 3_600_000, now)).toBe("4h");
    expect(age(now - 3 * 86_400_000, now)).toBe("3d");
  });
});

describe("a check that throws is contained to its own row", () => {
  /**
   * The inverse of "unreadable is never PASS": unreadable must not be able to
   * report *nothing at all*.
   *
   * `http()` promises never to throw, and it keeps that promise — but the
   * promise stops at the transport. A check still reads fields off the body it
   * was handed, and a host whose shape has drifted answers 200 with them
   * absent. Before the boundary, `read()` had no `try`, so one such body threw
   * out of the whole function: no rows, no summary, nothing printed. Shape
   * drift on an older tenant is exactly the deployment-era condition this
   * harness exists to catch, so it is the last place that can afford to go
   * silent.
   */
  const drifted: Record<string, unknown> = {
    "/healthz": { status: "ok" },
    "/spec": { name: "opencompany", version: "0.1.0", capabilities: ["rest"], storage: "memory" },
    "/api/v1/company": { id: "acme", name: "Acme", lifecycle: "running", pending_approvals: 0 },
    // 200s whose payload is an object where the check expects an array: the
    // roster reads `team.map`, which is `undefined` here.
    "/api/v1/company/team": {},
  };

  async function runDrifted(): Promise<Row[]> {
    const ocqa = loadHarness(async (path: string) =>
      path in drifted ? response(200, drifted[path]) : response(404, { error: "not found" }),
    );
    return ocqa.read();
  }

  it("still returns every other check", async () => {
    const rows = await runDrifted();
    expect(rows).toHaveLength(22);
  });

  it("reports the thrower as untested, naming it and saying what threw", async () => {
    const rows = await runDrifted();
    const roster = rows.find((r) => r.check === "roster");
    // SKIP and not FAIL: a check that could not be evaluated has not judged the
    // surface, the same as one that 404ed.
    expect(roster?.verdict).toBe("SKIP");
    expect(roster?.value).toMatch(/^threw: /);
  });

  it("does not let the throw poison the check downstream of it", async () => {
    // `tool-catalog` is handed the roster's return value. The boundary hands
    // back the same empty list the roster's own unreadable path returns, so the
    // downstream check reports its own verdict rather than a second throw.
    const rows = await runDrifted();
    const tools = rows.find((r) => r.check === "tool-catalog");
    expect(tools?.verdict).toBe("SKIP");
    expect(tools?.value).not.toMatch(/^threw: /);
  });
});

describe("usage-finances against a host that answers without the figures", () => {
  /**
   * The concrete instance the boundary above was found through, fixed at the
   * source too: `/finances` is Phase 1 (`src/server/ops/finances.rs`), so an
   * older tenant can answer `200 {}`. `f.spentUsd.toFixed(2)` on that is a
   * `TypeError`.
   */
  const reachable: Record<string, unknown> = {
    "/healthz": { status: "ok" },
    "/spec": { name: "opencompany", version: "0.1.0", capabilities: ["rest"], storage: "memory" },
    "/api/v1/company": { id: "acme", name: "Acme", lifecycle: "running", pending_approvals: 0 },
    "/api/v1/company/usage": { totals: { tokens: 1200, costUsd: 3.5 } },
    "/api/v1/company/finances": {},
  };

  async function financesRow(): Promise<Row | undefined> {
    const ocqa = loadHarness(async (path: string) =>
      path in reachable ? response(200, reachable[path]) : response(404, { error: "not found" }),
    );
    const rows = await ocqa.read();
    return rows.find((r) => r.check === "usage-finances");
  }

  it("reads it as untested rather than throwing", async () => {
    const row = await financesRow();
    expect(row?.verdict).toBe("SKIP");
    // Not the boundary catching it — the check itself declining to judge.
    expect(row?.value).not.toMatch(/^threw: /);
  });

  it("names the fields that were missing, and never prints an absent figure as $0.00", async () => {
    // A budget rendered as `$0.00` reads as a company that has spent nothing,
    // which is a confident answer to a question the host did not answer.
    const row = await financesRow();
    expect(row?.note).toContain("spentUsd");
    expect(row?.value).toContain("finances unread");
    expect(row?.value).not.toContain("balance $0.00");
  });
});

describe("probe() will not choose a workflow to run for real", () => {
  /**
   * A real run fires real deliveries — a report into a channel, mail to a real
   * address. The first version took `flows[0]`, which on a production tenant is
   * whichever workflow the host happened to list first: a stranger's. There is
   * no default that is safe to guess, so an unnamed target is SKIP.
   */
  const tenant: Record<string, unknown> = {
    "/healthz": { status: "ok" },
    "/spec": { name: "opencompany", version: "0.1.0", capabilities: ["rest"], storage: "memory" },
    "/api/v1/company": { id: "acme", name: "Acme", lifecycle: "running", pending_approvals: 0 },
    "/api/v1/company/workflows": [{ id: "somebody-elses-workflow" }, { id: "daily-release-readiness" }],
    "/api/v1/company/desks": [],
    "/api/v1/company/approvals": [],
    "/api/v1/company/chat": { responses: [{ text: "Acme." }] },
  };

  function tenantHarness() {
    const sent: string[] = [];
    const ocqa = loadHarness(async (path: string, init?: { method?: string }) => {
      const method = init?.method || "GET";
      sent.push(`${method} ${path}`);
      // The board is method-aware so the card probe behaves as it does against a
      // real host — a POST creates and a GET lists — rather than throwing and
      // taking the workflow check with it, which would make these assertions
      // pass for the wrong reason.
      if (path === "/api/v1/company/tasks") {
        return response(200, method === "POST" ? { id: "card-1" } : [{ id: "card-1" }]);
      }
      if (path === "/api/v1/company/tasks/card-1") return response(200, { ok: true });
      return path in tenant ? response(200, tenant[path]) : response(404, { error: "not found" });
    });
    return { ocqa, sent };
  }

  it("leaves the rest of the probe working, so the skip is the only thing missing", async () => {
    // Guards the fixture as much as the code: if the board probe threw here,
    // the assertions below would be measuring a dead probe rather than a
    // declined workflow target.
    const { ocqa } = tenantHarness();
    const rows = await ocqa.probe();
    expect(rows.find((r) => r.check === "probe-board-card")?.verdict).toBe("PASS");
    expect(rows.find((r) => r.check === "probe-chat")?.verdict).toBe("PASS");
  });

  it("skips the run, and fires no POST at any workflow, when none is named", async () => {
    const { ocqa, sent } = tenantHarness();
    const rows = await ocqa.probe();
    const row = rows.find((r) => r.check === "probe-workflow-run");
    expect(row?.verdict).toBe("SKIP");
    // The load-bearing assertion: not merely reported as skipped, but no run
    // request left the page.
    expect(sent.filter((s) => s.includes("/run"))).toEqual([]);
  });

  it("says how to run one, so the skip is a door rather than a dead end", async () => {
    const { ocqa } = tenantHarness();
    const rows = await ocqa.probe();
    const row = rows.find((r) => r.check === "probe-workflow-run");
    expect(row?.note).toContain("workflow");
    expect(row?.note).toContain("dryRun");
  });

  it("runs the one that is named, and only that one", async () => {
    const { ocqa, sent } = tenantHarness();
    await ocqa.probe({ workflow: "daily-release-readiness", dryRun: true });
    const runs = sent.filter((s) => s.includes("/run"));
    expect(runs).toHaveLength(1);
    expect(runs[0]).toContain("daily-release-readiness");
    expect(runs[0]).not.toContain("somebody-elses-workflow");
  });

  it("skips a name the host does not have rather than falling back to the first", async () => {
    const { ocqa, sent } = tenantHarness();
    const rows = await ocqa.probe({ workflow: "no-such-workflow" });
    const row = rows.find((r) => r.check === "probe-workflow-run");
    expect(row?.verdict).toBe("SKIP");
    expect(sent.filter((s) => s.includes("/run"))).toEqual([]);
  });
});

describe("report() withholds tenant message text", () => {
  /**
   * `report()` exists to be pasted into a GitHub issue, and `probe-chat`
   * collected a real agent reply. The verdict is formed from the shape of the
   * answer so the table still judges something checkable, and the reply itself
   * rides in `detail`, which the report drops unless asked.
   */
  const tenant: Record<string, unknown> = {
    "/healthz": { status: "ok" },
    "/spec": { name: "opencompany", version: "0.1.0", capabilities: ["rest"], storage: "memory" },
    "/api/v1/company": { id: "acme", name: "Acme", lifecycle: "running", pending_approvals: 0 },
    "/api/v1/company/desks": [],
    "/api/v1/company/approvals": [],
    "/api/v1/company/workflows": [],
    "/api/v1/company/chat": { responses: [{ text: "Acme, a maker of anvils, per the manifest." }] },
  };

  async function probed() {
    const ocqa = loadHarness(async (path: string, init?: { method?: string }) => {
      if (path === "/api/v1/company/tasks") {
        return response(200, (init?.method || "GET") === "POST" ? { id: "card-1" } : [{ id: "card-1" }]);
      }
      if (path === "/api/v1/company/tasks/card-1") return response(200, { ok: true });
      return path in tenant ? response(200, tenant[path]) : response(404, { error: "not found" });
    });
    const rows = await ocqa.probe();
    return { ocqa, rows };
  }

  it("keeps the reply out of the row's own value, judging its shape instead", async () => {
    const { rows } = await probed();
    const chat = rows.find((r) => r.check === "probe-chat");
    expect(chat?.value).not.toContain("anvils");
    expect(chat?.value).toContain("1 replies");
    // Still on the operator's own screen: judging whether the company answered
    // *well* needs the words.
    expect(chat?.detail).toContain("anvils");
  });

  it("omits it from the Markdown, and says that it did", async () => {
    const { ocqa } = await probed();
    const text = ocqa.report();
    expect(text).not.toContain("anvils");
    expect(text).toContain("withheld");
  });

  it("includes it only when explicitly asked", async () => {
    const { ocqa } = await probed();
    expect(ocqa.report({ raw: true })).toContain("anvils");
  });
});
