/*
 * oc-qa.js — the OpenCompany release parity harness (issue #987).
 *
 * Paste this whole file into the browser console of a **signed-in** operator
 * console, then call `OCQA.read()` or `OCQA.probe()`.
 *
 * ## Why a console script and not a test
 *
 * It rides the session cookie of the tab it is pasted into, so a release can be
 * checked against a real hosted tenant with no token to mint, distribute or
 * revoke. Nothing here is a substitute for the suite in `frontend/test/` or
 * `cargo test`: this answers "is the thing we deployed actually working for the
 * operator", which no in-repo test can, because the failures it is built around
 * are failures of the *deployment* (a stale `index.html`, an unwired channel, a
 * missing credential) rather than of the code.
 *
 * ## Zero dependencies, deliberately
 *
 * `fetch`, `console` and nothing else. Anything that has to be installed first
 * will not be run during an incident, which is the moment this is worth most.
 *
 * ## Three rules the checks obey
 *
 * 1. **A verdict travels with the value it judged.** Every row carries the
 *    reading the verdict was formed from, so a PASS can be checked rather than
 *    trusted. A harness whose output is a column of PASSes is unfalsifiable.
 *
 * 2. **Unreadable is never PASS.** A surface that 404s, errors, or is absent on
 *    this build reports `SKIP`, and `SKIP` counts as *untested* in the summary,
 *    not as passed. The 2026-08-18 pass recorded three checks as passing that
 *    had never run; that is the miss this rule exists to stop.
 *
 * 3. **A check is reviewed against the bug it missed.** `runVerdict` below
 *    exists because the first version of the workflow check folded
 *    `nodes[].status` alone and scored a run that delivered nothing as green —
 *    the same mistake issue #981 filed against the product. Extending the
 *    harness after a miss is not enough; the check that missed has to change.
 *
 * ## The two checks worth keeping even if the rest is deleted
 *
 * - `console-cache-headers` — caught #979. A cacheable `index.html` white-screens
 *   every returning user after a deploy, and there is nothing in the UI to look
 *   at, because the UI is the thing that failed to load.
 * - `workflow-deliveries` — caught #981. A run whose nodes are all `ok` can still
 *   have delivered nothing.
 *
 * ## Usage
 *
 *   OCQA.read()                       // 22 read-only checks, ~10s. Spends nothing.
 *   OCQA.read({ company: "acme" })    // pin a company in multi-company mode
 *
 *   OCQA.probe()                      // 5 checks, 4 of them live: real chat
 *                                     // turns, a real board card, real tokens.
 *                                     // Names the host it is about to act on
 *                                     // before it starts. The workflow run is
 *                                     // SKIP until you name one (see below).
 *   OCQA.probe({ workflow: "daily-release-readiness" })              // + a REAL run
 *   OCQA.probe({ workflow: "daily-release-readiness", dryRun: true })// + a rehearsal
 *
 *   OCQA.report()                     // last run, as Markdown to paste in an issue
 *   OCQA.report({ raw: true })        // ...including the tenant message text
 *
 * `probe()` never picks a workflow for you. A real run fires real deliveries —
 * a report into a channel, mail to a real address — so the fifth check reports
 * SKIP until you name the workflow you meant. Everything else in `probe()` is
 * live either way: it spends tokens on whatever tenant the tab is signed in to.
 *
 * Both entry points resolve to an array of rows and also print a table.
 */
(function (global) {
  "use strict";

  const VERSION = "1.0.0";

  const PASS = "PASS";
  const WARN = "WARN";
  const FAIL = "FAIL";
  const SKIP = "SKIP";

  /* ------------------------------------------------------------------ *
   * Transport
   * ------------------------------------------------------------------ */

  /**
   * One HTTP call, never throwing. Every failure mode — a non-2xx, a body that
   * is not JSON, a network error, a timeout — comes back as a value, because a
   * check that throws takes the other twenty-one with it.
   */
  async function http(path, options) {
    const opts = options || {};
    const controller = new AbortController();
    const timeoutMs = opts.timeoutMs || 20000;
    const timer = setTimeout(() => controller.abort(), timeoutMs);
    const started = Date.now();
    try {
      const res = await fetch(path, {
        method: opts.method || "GET",
        credentials: "include",
        cache: opts.cache || "default",
        signal: controller.signal,
        headers: opts.body ? { "content-type": "application/json" } : undefined,
        body: opts.body ? JSON.stringify(opts.body) : undefined,
      });
      const text = await res.text();
      let body = null;
      try {
        body = text ? JSON.parse(text) : null;
      } catch {
        body = null;
      }
      return {
        ok: res.ok,
        status: res.status,
        body,
        text,
        headers: res.headers,
        elapsedMs: Date.now() - started,
      };
    } catch (err) {
      return {
        ok: false,
        status: 0,
        body: null,
        text: "",
        headers: new Headers(),
        elapsedMs: Date.now() - started,
        error: err && err.name === "AbortError" ? `timed out after ${timeoutMs}ms` : String(err),
      };
    } finally {
      clearTimeout(timer);
    }
  }

  /**
   * The company route prefix.
   *
   * Single-company mode serves `/api/v1/company`; a multi-company host wants
   * `/api/v1/companies/{id}`. Resolved by asking rather than configured,
   * because the wrong prefix 404s every check and would read as twenty-two
   * SKIPs with no clue which knob was wrong.
   */
  async function resolveScope(company) {
    if (company) return `/api/v1/companies/${encodeURIComponent(company)}`;
    const single = await http("/api/v1/company");
    if (single.ok) return "/api/v1/company";
    const list = await http("/api/v1/companies");
    if (list.ok && Array.isArray(list.body) && list.body.length > 0) {
      return `/api/v1/companies/${encodeURIComponent(list.body[0].id)}`;
    }
    return null;
  }

  /* ------------------------------------------------------------------ *
   * Pure judgements — the parts worth unit-testing
   * ------------------------------------------------------------------ */

  /**
   * A run's verdict — **the host's when it sends one** (issue #981).
   *
   * `run.verdict` is now derived server-side and serialized on both run DTOs,
   * so the first line below is the whole of this function against any current
   * host. Everything after it is the fallback for a host predating #981, and
   * that fallback is a transcription of
   * `frontend/src/views/workflows/run-health.ts` **which must stay one**: two
   * independent definitions of "did this run succeed" is precisely the defect
   * issue #981 filed, and the harness owned the second copy — it scored a
   * delivery-failure run as PASS. `frontend/test/unit/qa-harness.test.ts` pins
   * the two together so a change to the console's reading breaks this file
   * loudly rather than silently re-greening a bad run.
   *
   * The fallback cannot simply be deleted now the host answers: this script is
   * pasted into a browser against whatever host is in front of the operator,
   * including one rolled back or older than this file.
   *
   * The order IS the check, and every arm below exists because the state it
   * names had been scoring green:
   * - `running` first: an unsettled run has neither succeeded nor failed, and
   *   both colours are claims the host has not made.
   * - `cancelled` before the delivery reads: a stop somebody asked for is not a
   *   fault, and a cancelled run has no deliveries to weigh.
   * - `stranded` (issue #1189) above BOTH of them: a run whose every gate has
   *   lost its card is the one state in which "go and decide it" is false, and
   *   `blocked` and `awaiting-approval` both say exactly that. Only the host can
   *   make the join — it needs the live approvals queue — so the fallback
   *   reads the host's own `strandedApprovals`, and treats its absence as "not
   *   reconciled" rather than as "nothing is stranded". A run only PARTLY
   *   stranded keeps its old verdict.
   * - `blocked` (issue #881) before the delivery reads: a run that stopped short
   *   at a gate carries no `error`, is not `cancelled`, is not `running` and
   *   routed no report — so before its own arm it fell through every check to
   *   "ok" and told the operator a pipeline that delivered nothing had
   *   succeeded.
   * - `undelivered` before `awaiting-approval`: a report that will not go out
   *   without a change outranks one waiting on a human.
   * - `awaiting-approval` reads [`awaitingCount`], not the delivery rows alone
   *   (issue #846). A run that paused at a `requires_approval` node never
   *   reached an `output` node, so its `deliveries` are empty and a
   *   delivery-only read scored the gated case — the common one — as clean.
   *
   * These are exactly the eight words the host's `WorkflowRunVerdict` uses, in
   * the same order — which is what makes the fallback and the answer
   * interchangeable rather than merely similar.
   */
  function runVerdict(run) {
    if (!run) return "unknown";
    if (run.verdict) return run.verdict;
    if (run.running === true) return "running";
    if (run.error) return "failed";
    if (run.cancelled) return "stopped";
    if (isStranded(run)) return "stranded";
    if (isBlocked(run)) return "blocked";
    if (undeliveredCount(run.deliveries) > 0) return "undelivered";
    if (awaitingCount(run) > 0) return "awaiting-approval";
    if (erroredNodeCount(run) > 0) return "degraded";
    return "ok";
  }

  /**
   * How many nodes finished in error while the run itself carried on (issue
   * #1865).
   *
   * `on_error: continue | route` and the iteration cap both leave a node
   * `error` without failing the run, so every reading above this one is
   * absent: no `error`, not cancelled, nothing parked, everything delivered.
   * Without this arm the ladder fell straight through to `ok` and the probe
   * reported PASS on a run that had a broken step in it — the same
   * false-success shape every other arm here exists to close, and the reason
   * the host's own ladder places `degraded` last, immediately before `Ok`.
   */
  function erroredNodeCount(run) {
    return (run.nodes || []).filter((n) => n && n.status === "error").length;
  }

  /**
   * Whether this run stopped short because a step is waiting on a person
   * (issue #881).
   */
  function isBlocked(run) {
    return (run.blockedNodes || []).length > 0;
  }

  /**
   * Whether nothing in the queue is waiting on this run any more, so no
   * decision can move it (issue #1189).
   *
   * `pendingApprovals` is a receipt of where the run stopped and cannot go
   * stale — but the question each entry points at can, and on one staging
   * tenant 34 of 60 runs were pointing at nothing. Only the host can reconcile
   * the two, because the join needs the live approvals queue; a host predating
   * #1189 sends no `strandedApprovals` at all, which reads here as "not
   * reconciled" and never as "nothing is stranded".
   *
   * Mirrors the host's rule rather than inventing a second one: it stopped for
   * somebody, EVERY gate lost its card, and no report is parked either. A
   * partly stranded run keeps its old verdict — something there really is still
   * decidable.
   */
  function isStranded(run) {
    const pending = (run.pendingApprovals || []).length;
    return (
      pending > 0 &&
      (run.strandedApprovals || 0) >= pending &&
      pendingCount(run.deliveries) === 0
    );
  }

  /**
   * Everything about this run that is **still** waiting on a person: the gates
   * it paused at that still have a card, plus the reports it parked (issues
   * #846, #1189).
   *
   * The two were never read together, and that is what let a run report success
   * while a human had not answered it. Issue #1189 added the other half: the
   * gates were counted raw, so a run whose cards the queue had lost went on
   * claiming somebody was being waited on. `strandedApprovals` is the host's
   * reconciliation; clamped at 0 because a negative would render as
   * "-1 awaiting approval", which is a worse failure than the one being fixed.
   */
  function awaitingCount(run) {
    const pending = (run.pendingApprovals || []).length;
    const live = Math.max(0, pending - (run.strandedApprovals || 0));
    return live + pendingCount(run.deliveries);
  }

  /**
   * Reports that did not land **and will not without a change**. `pending` is
   * excluded on purpose: it is a report parked for an operator's approval, so
   * counting it here would score a working approvals queue as a failure.
   *
   * Two `skipped` reasons are excluded too (issue #981), matching the host's
   * `is_undelivered` and the console's `isUndelivered`: `already-delivered` (an
   * earlier run in this approval lineage sent it, issue #438) and `dry-run` (a
   * test run attempted nothing, on purpose, issue #542). Both describe a report
   * whose fate is accounted for.
   *
   * `no-destination-configured` is deliberately NOT excluded: that report was
   * produced and then lost, with nothing accounting for it, which is exactly
   * what issue #925 added the row to make visible.
   *
   * An absent `reason` — a host predating issue #248 — counts, which is the
   * safe direction for a harness that is pasted against whatever host is in
   * front of the operator.
   */
  function undeliveredCount(deliveries) {
    return (deliveries || []).filter(isUndelivered).length;
  }

  /** Whether one delivery row is a report that did not go out. */
  function isUndelivered(d) {
    if (d.status === "sent" || d.status === "pending") return false;
    if (d.status !== "skipped") return true;
    return d.reason !== "already-delivered" && d.reason !== "dry-run";
  }

  /** Reports waiting on an operator's verdict rather than on a fix. */
  function pendingCount(deliveries) {
    return (deliveries || []).filter((d) => d.status === "pending").length;
  }

  /**
   * Judges one `Cache-Control` header. This is the whole of #979 in six lines.
   *
   * `kind: "html"` — the shell and every SPA-route fallback. It names the entry
   * bundle, so a browser allowed to reuse yesterday's copy asks for chunks that
   * are gone from the new image, the SPA fallback answers them with `index.html`,
   * the dynamic import throws, and the page is blank. **An absent header is a
   * FAIL, not a WARN**: absent means heuristic caching, which is the bug.
   *
   * `kind: "asset"` — content-hashed, so a long immutable lifetime is both safe
   * and the point. A missing header only costs revalidation round trips, so it
   * is a WARN.
   */
  function judgeCacheHeader(kind, header) {
    const value = (header || "").toLowerCase();
    if (kind === "html") {
      if (!value) {
        return { verdict: FAIL, note: "no cache-control: heuristic caching white-screens returning users after a deploy (#979)" };
      }
      if (value.includes("no-store") || value.includes("no-cache") || /max-age\s*=\s*0\b/.test(value)) {
        return { verdict: PASS, note: "revalidated, so the browser always learns the current entry name" };
      }
      return { verdict: FAIL, note: "cacheable shell: a returning browser keeps yesterday's entry bundle (#979)" };
    }
    if (!value) {
      return { verdict: WARN, note: "no cache-control on a hashed asset: every load revalidates for nothing" };
    }
    const maxAge = /max-age\s*=\s*(\d+)/.exec(value);
    if (value.includes("immutable") || (maxAge && Number(maxAge[1]) >= 86400)) {
      return { verdict: PASS, note: "hashed asset cached long, which is safe because the name changes with the bytes" };
    }
    return { verdict: WARN, note: "hashed asset cached briefly: safe, but the revalidation is pure cost" };
  }

  /** A compact "4h" / "12m" for an epoch-millis timestamp. */
  function age(atMillis, now) {
    if (!atMillis) return "n/a";
    const seconds = Math.max(0, Math.round(((now || Date.now()) - atMillis) / 1000));
    if (seconds < 60) return `${seconds}s`;
    if (seconds < 3600) return `${Math.round(seconds / 60)}m`;
    if (seconds < 86400) return `${Math.round(seconds / 3600)}h`;
    return `${Math.round(seconds / 86400)}d`;
  }

  /* ------------------------------------------------------------------ *
   * Row plumbing
   * ------------------------------------------------------------------ */

  function row(check, verdict, value, note, detail) {
    const r = { check, verdict, value: String(value), note: note || "" };
    // `detail` carries tenant content — a real agent reply, a real message.
    // It is worth seeing on the operator's own screen and is exactly what must
    // not be pasted into a public issue, so it lives in its own field and
    // `report()` withholds it unless asked for it.
    if (detail !== undefined && detail !== null && detail !== "") r.detail = String(detail);
    return r;
  }

  /**
   * Records a row and hands back what this check's caller should see.
   *
   * Exists because `return rows.push(r)` returns the array's new **length** — a
   * number a caller expecting a list will treat as data and then crash on,
   * taking every check after it down with it. Naming the empty value at each
   * early return is what keeps a check that could not read its surface from
   * poisoning the one downstream of it.
   */
  function push(rows, r, empty) {
    rows.push(r);
    return empty === undefined ? null : empty;
  }

  /**
   * The row for a surface that could not be read.
   *
   * Always `SKIP`, never `PASS`. A 404 on a feature-gated route is a perfectly
   * good answer to "does this build have it" and a terrible answer to "does it
   * work"; conflating the two is what let three checks be written up as passing
   * in the 2026-08-18 pass when they had never run.
   */
  function unread(check, res, what) {
    const why = res.error ? res.error : `HTTP ${res.status}`;
    return row(check, SKIP, why, `${what} could not be read — untested, not passed`);
  }

  /**
   * Whether the host answered "this deployment has no such machinery".
   *
   * A feature-gated surface reports `{"error": …, "code": "not_wired"}` rather
   * than pretending, and that is a different answer from a broken one: a build
   * with no workflow runner cannot run a workflow, and scoring it FAIL sends
   * somebody chasing a graph that is fine. It is `SKIP` — untested, not passed
   * and not failed.
   *
   * Matched on the typed `code` token, never on the prose (issue #248): the
   * message is free to be reworded, and a harness that greps it would go quiet
   * the day somebody did.
   */
  function notWired(res) {
    return !!(res.body && res.body.code === "not_wired");
  }

  /** A duration a human reads at a glance: `840ms`, `4.1s`. */
  function secs(ms) {
    return ms < 1000 ? `${ms}ms` : `${(ms / 1000).toFixed(1)}s`;
  }

  /** The host this run is judging, safe to read outside a browser (the unit sandbox). */
  function hostname() {
    return global.location ? global.location.host : "unknown";
  }

  /**
   * Runs one check, and contains a throw to that check's own row.
   *
   * `http()` never throws, which is what the transport docstring promises — but
   * that defence stops at the transport. A check still reads fields off the
   * body it got back, and a host whose shape has drifted hands back a 200 whose
   * fields are simply absent: `f.spentUsd.toFixed(2)` on a `/finances` that
   * answered `{}` is a `TypeError`, and without a boundary here it takes the
   * other twenty-one checks and the summary with it. Reporting *nothing* is
   * louder than a false green but no more useful, and shape drift on an older
   * tenant is precisely the deployment-era condition this tool exists to catch.
   *
   * The row is `SKIP`, never `FAIL`: a check that could not be evaluated has
   * not judged the surface, and the same rule applies to a check that threw as
   * to one that 404ed. It carries the message, because a harness bug is the
   * likeliest cause and it should be reportable without a debugger.
   */
  async function attempt(rows, check, fn, empty) {
    try {
      return await fn();
    } catch (err) {
      const why = (err && err.message) || String(err);
      rows.push(row(check, SKIP, `threw: ${why}`, `${check} could not be judged — untested, not passed`));
      return empty === undefined ? null : empty;
    }
  }

  /* ------------------------------------------------------------------ *
   * Read-only checks (22)
   * ------------------------------------------------------------------ */

  async function checkHost(rows) {
    const health = await http("/healthz", { cache: "no-store" });
    if (!health.ok) {
      rows.push(row("host", FAIL, `HTTP ${health.status || health.error}`, "the host did not answer /healthz — nothing below is meaningful"));
      return null;
    }
    const spec = await http("/spec");
    if (!spec.ok) {
      rows.push(row("host", WARN, `healthz ok in ${health.elapsedMs}ms, /spec HTTP ${spec.status}`, "no build identity: a pass cannot name what it judged"));
      return null;
    }
    const s = spec.body || {};
    const caps = (s.capabilities || []).join("+") || "rest only";
    rows.push(
      row(
        "host",
        PASS,
        `${s.name}@${s.version} · ${hostname()} · ${caps}`,
        `healthz ${health.elapsedMs}ms · storage ${s.storage || "unknown"} · instance ${String(s.instance_id || "").slice(0, 8)}`,
      ),
    );
    return s;
  }

  /**
   * The console's cache headers — the check that caught #979.
   *
   * Fetched with `cache: "reload"` so the browser's own cache cannot answer and
   * hide the header we came to read.
   */
  async function checkConsoleCacheHeaders(rows) {
    const shell = await http("/", { cache: "reload" });
    if (!shell.ok) {
      rows.push(unread("console-cache-headers", shell, "the console shell"));
      return;
    }
    const shellHeader = shell.headers.get("cache-control");
    const shellVerdict = judgeCacheHeader("html", shellHeader);
    rows.push(
      row("console-cache-headers", shellVerdict.verdict, `/ → cache-control: ${shellHeader || "(absent)"}`, shellVerdict.note),
    );

    // The SPA fallback is a separate response from a separate code path; a shell
    // that revalidates while `/some/route` does not is the same bug on the path
    // a returning user actually lands on.
    const fallback = await http("/__oc-qa-nonexistent-route", { cache: "reload" });
    if (fallback.ok) {
      const header = fallback.headers.get("cache-control");
      const verdict = judgeCacheHeader("html", header);
      rows.push(
        row("spa-fallback-cache-headers", verdict.verdict, `unknown route → cache-control: ${header || "(absent)"}`, verdict.note),
      );
    }

    const asset = /\/assets\/[A-Za-z0-9._-]+\.js/.exec(shell.text || "");
    if (!asset) {
      rows.push(row("asset-cache-headers", SKIP, "no /assets/*.js in the shell", "nothing to judge — a dev server or an unbuilt console"));
      return;
    }
    const assetRes = await http(asset[0], { cache: "reload" });
    if (!assetRes.ok) {
      rows.push(row("asset-cache-headers", FAIL, `${asset[0]} → HTTP ${assetRes.status}`, "the shell names an entry bundle the host does not serve — this is the white screen"));
      return;
    }
    const type = assetRes.headers.get("content-type") || "";
    if (!type.includes("javascript")) {
      rows.push(
        row("asset-cache-headers", FAIL, `${asset[0]} → ${type}`, "an asset answered by the SPA fallback: the entry bundle is stale (#979)"),
      );
      return;
    }
    const assetHeader = assetRes.headers.get("cache-control");
    const assetVerdict = judgeCacheHeader("asset", assetHeader);
    rows.push(row("asset-cache-headers", assetVerdict.verdict, `${asset[0]} → cache-control: ${assetHeader || "(absent)"}`, assetVerdict.note));
  }

  async function checkLifecycle(rows, scope) {
    const res = await http(scope);
    if (!res.ok) return push(rows, unread("company-lifecycle", res, "company status"));
    const c = res.body || {};
    // `emergency_paused` is deliberately independent of `lifecycle`: chat still
    // works while a company is stopped, so reading `lifecycle` alone reports a
    // stopped company as perfectly healthy.
    const stopped = c.emergency_paused === true;
    const verdict = c.lifecycle === "running" && !stopped ? PASS : stopped ? FAIL : WARN;
    rows.push(
      row(
        "company-lifecycle",
        verdict,
        `${c.name || c.id} · ${c.lifecycle}${stopped ? " · KILL SWITCH ENGAGED" : ""}`,
        stopped ? "new effects outside the Other group are being denied" : `${c.pending_approvals || 0} approvals pending`,
      ),
    );
    return c;
  }

  async function checkRoster(rows, scope) {
    const res = await http(`${scope}/team`);
    if (!res.ok) return push(rows, unread("roster", res, "the roster"), []);
    const team = res.body || [];
    rows.push(
      row(
        "roster",
        team.length > 0 ? PASS : FAIL,
        `${team.length} teammates: ${team.map((t) => t.id).join(", ") || "(none)"}`,
        team.length ? "" : "a company with no roster can answer nothing",
      ),
    );
    return team;
  }

  async function checkDesks(rows, scope) {
    const res = await http(`${scope}/desks`);
    if (!res.ok) return push(rows, unread("desks", res, "desks"), []);
    const desks = res.body || [];
    const empty = desks.filter((d) => (d.members || []).length === 0);
    rows.push(
      row(
        "desks",
        desks.length === 0 ? WARN : empty.length ? WARN : PASS,
        `${desks.length} desks: ${desks.map((d) => `${d.id}(${(d.members || []).length})`).join(", ") || "(none)"}`,
        empty.length ? `${empty.map((d) => d.id).join(", ")} have no members, so a hand-off to them lands nowhere` : "",
      ),
    );
    return desks;
  }

  async function checkTaskBoard(rows, scope) {
    const res = await http(`${scope}/tasks`);
    if (!res.ok) return push(rows, unread("task-board", res, "the task board"), []);
    const tasks = res.body || [];
    const byColumn = {};
    for (const t of tasks) byColumn[t.column] = (byColumn[t.column] || 0) + 1;
    const stuck = tasks.filter((t) => t.column === "in_progress" && Date.now() - t.updatedAt > 6 * 3600 * 1000);
    rows.push(
      row(
        "task-board",
        stuck.length ? WARN : PASS,
        `${tasks.length} cards · ${Object.entries(byColumn).map(([k, v]) => `${k}:${v}`).join(" ") || "(empty)"}`,
        stuck.length ? `${stuck.length} in_progress untouched for over 6h — a turn that died leaves exactly this trace (#983)` : "",
      ),
    );
    return tasks;
  }

  async function checkWorkspace(rows, scope) {
    const res = await http(`${scope}/workspace`);
    if (!res.ok) return push(rows, unread("workspace", res, "the workspace tree"), []);
    const nodes = res.body || [];
    const files = nodes.filter((n) => n.kind === "file");
    const newest = files.reduce((max, n) => Math.max(max, n.updatedAt || 0), 0);
    rows.push(
      row(
        "workspace",
        nodes.length ? PASS : WARN,
        `${nodes.length} nodes (${files.length} files) · newest ${age(newest)} ago`,
        nodes.length ? "" : "an empty workspace is fine on a fresh tenant and a red flag on a working one",
      ),
    );
    return nodes;
  }

  async function checkApprovals(rows, scope) {
    const res = await http(`${scope}/approvals`);
    if (!res.ok) {
      rows.push(unread("approvals-backlog", res, "the approvals queue"));
      return [];
    }
    const approvals = res.body || [];
    const oldest = approvals.reduce((min, a) => (min === 0 ? a.at_millis : Math.min(min, a.at_millis)), 0);
    const kinds = [...new Set(approvals.map((a) => a.kind))].join(", ");
    // Age, not depth, is the signal. A queue of three parked a minute ago is a
    // company working; a queue of one parked eight days ago is a company whose
    // operator stopped looking, and every effect behind it is frozen.
    const ageHours = oldest ? (Date.now() - oldest) / 3600000 : 0;
    rows.push(
      row(
        "approvals-backlog",
        approvals.length === 0 ? PASS : ageHours > 48 ? FAIL : ageHours > 4 ? WARN : PASS,
        `${approvals.length} parked${approvals.length ? ` (${kinds}) · oldest ${age(oldest)} ago` : ""}`,
        ageHours > 4 ? "everything behind this gate is frozen until somebody decides" : "",
      ),
    );
    return approvals;
  }

  /**
   * The company's approval tier.
   *
   * `{scope}/policy` is a real read surface (`src/server/ops/policy.rs`, #562):
   * `read_policy` takes only `ScopedCompany` and answers the tier the approval
   * gate itself reads — `effective_policy()`, resolved ahead of the manifest —
   * plus the manifest's tier and whether an operator override is producing it.
   * So this row verifies the single setting the approvals checks depend on
   * instead of only reporting what the gate is holding. That upgrades the queue
   * too: an empty queue now reads differently for `supervised` (nothing is
   * pending) than for `full` (nothing will ever park).
   */
  async function checkApprovalTier(rows, scope, approvals) {
    const caps = await http(`${scope}/capabilities`);
    const grants = caps.ok ? caps.body || {} : {};
    const policy = await http(`${scope}/policy`);
    if (!policy.ok) {
      // Older build without the read surface (#562 shipped after this path
      // stopped being guessable), or a scope where the route is not mounted.
      const observed = [];
      if (approvals && approvals.length) observed.push(`${approvals.length} effects currently parked`);
      if (grants.configured) observed.push(`plan ${grants.plan || "custom"}/${grants.period || "?"}`);
      rows.push(
        row(
          "approval-tier",
          SKIP,
          observed.join(" · ") || "no observable gate activity",
          `{scope}/policy did not answer (HTTP ${policy.status}) — the tier is unverified on this build`,
        ),
      );
      return;
    }
    const p = policy.body || {};
    const parked = approvals ? approvals.length : 0;
    const mode = p.mode || "?";
    const parts = [`mode ${mode}`];
    if (p.manifestMode && p.manifestMode !== mode) parts.push(`manifest ${p.manifestMode}`);
    if (p.overridden) parts.push(`overridden by ${p.setBy || "an operator"}`);
    parts.push(`${parked} parked`);
    const note = [];
    if (p.overridden) {
      note.push(`an override is in force (${p.setBy || "an operator"}${p.setAtMillis ? ` at ${new Date(p.setAtMillis).toISOString()}` : ""}) — the manifest tier is not what runs`);
    }
    if (mode === "full" && parked > 0) {
      rows.push(
        row(
          "approval-tier",
          WARN,
          parts.join(" · "),
          `\`full\` tier yet ${parked} effects are parked — the gate should not be holding anything`,
        ),
      );
      return;
    }
    if (mode === "full") note.push("full tier — no effect ever parks, so an empty queue is the norm, not a sign of no gate");
    else if (parked === 0) note.push(`\`${mode}\` tier with nothing pending — an empty queue means nothing is awaiting the gate`);
    rows.push(
      row("approval-tier", PASS, parts.join(" · "), note.join(" ")),
    );
  }

  async function checkConnections(rows, scope) {
    const res = await http(`${scope}/connections`);
    if (!res.ok) return push(rows, unread("manifest-connections", res, "connections"), []);
    const conns = res.body || [];
    const connected = conns.filter((c) => c.connected);
    const unverified = conns.filter((c) => c.unverified);
    rows.push(
      row(
        "manifest-connections",
        unverified.length ? WARN : PASS,
        `${connected.length}/${conns.length} connected: ${connected.map((c) => `${c.provider}${c.via ? `[${c.via.join("/")}]` : ""}`).join(", ") || "(none)"}`,
        unverified.length
          ? `${unverified.map((c) => c.provider).join(", ")} could not be checked — "not connected" here means "we do not know"`
          : "",
      ),
    );
    return conns;
  }

  async function checkComposio(rows, scope) {
    const status = await http(`${scope}/composio`);
    if (!status.ok) return push(rows, unread("composio-health", status, "Composio status"));
    const s = status.body || {};
    if (!s.inBuild) {
      return push(rows, row("composio-health", SKIP, "not in build", "the composio feature is not compiled into this image"));
    }
    if (!s.granted) {
      return push(rows, row("composio-health", PASS, "in build, not granted", "the company does not grant the composio namespace, so nothing is expected to work"));
    }
    const conns = await http(`${scope}/composio/connections`);
    const list = conns.ok ? conns.body || [] : [];
    const live = list.filter((c) => c.connected);
    // A `fallback` catalog is a degraded read wearing a healthy shape: eight
    // starter providers look exactly like the whole set (#397).
    const degraded = s.catalogSource === "fallback";
    rows.push(
      row(
        "composio-health",
        !conns.ok ? SKIP : degraded ? WARN : live.length ? PASS : WARN,
        `credential ${s.credentialSource} · catalog ${s.catalogSource}${s.openMode ? " · open mode" : ""} · ${live.length}/${list.length} toolkits connected`,
        degraded ? s.catalogNotice || "catalog could not be fetched; this list may be incomplete" : live.length ? "" : "granted with no live connection: every composio tool call will fail",
      ),
    );
  }

  async function checkMcp(rows, scope, spec, capabilities) {
    const res = await http(`${scope}/mcp/servers`);
    if (!res.ok) return push(rows, unread("mcp-servers", res, "MCP servers"), []);
    const servers = res.body || [];
    const enabled = servers.filter((s) => s.enabled);
    const unhealthy = enabled.filter((s) => s.health && s.health.status !== "ok");
    const unauthed = enabled.filter((s) => !s.authConfigured && s.health && s.health.authHint);
    // #567: the management routes ship in every build, so an operator can add a
    // server, store a token, watch it probe healthy — on an image that hands
    // agents no MCP tool at all. `undefined` is unknown, never "absent".
    const bridge = capabilities && capabilities.mcpInBuild;
    const bridgeNote = bridge === false ? " · AGENT BRIDGE NOT IN BUILD" : bridge === undefined ? " · bridge unknown" : "";
    rows.push(
      row(
        "mcp-servers",
        bridge === false && enabled.length ? FAIL : unhealthy.length ? FAIL : unauthed.length ? WARN : PASS,
        `${enabled.length}/${servers.length} enabled${bridgeNote} · ${enabled.map((s) => `${s.name}:${(s.health && s.health.status) || "unprobed"}`).join(", ") || "(none)"}`,
        bridge === false && enabled.length
          ? "servers configured and probing on a build with no agent-side bridge: no agent can call them"
          : unhealthy.length
            ? unhealthy.map((s) => `${s.name}: ${s.health.message}`).join(" | ")
            : unauthed.length
              ? `${unauthed.map((s) => s.name).join(", ")} report an auth hint with no stored credential`
              : "",
      ),
    );
    return servers;
  }

  async function checkInference(rows, scope) {
    const res = await http(`${scope}/inference`);
    if (!res.ok) return push(rows, unread("inference-provider", res, "inference status"));
    const i = res.body || {};
    // `restartRequired` is the transition every new operator makes: a company
    // built with no inference source runs the offline echo brain, and saving a
    // credential afterwards changes neither the brain nor the workflow runner.
    const echo = i.cognition === "echo";
    rows.push(
      row(
        "inference-provider",
        i.restartRequired ? FAIL : echo ? FAIL : i.keyConfigured || i.slug === "managed" ? PASS : WARN,
        `${i.provider}/${i.slug} · ${i.cognition} · source ${i.source} · key ${i.keyConfigured ? "stored" : "absent"}`,
        i.restartRequired
          ? "a stored config the running brain predates: only a restart puts it to work (#266)"
          : echo
            ? "the offline echo brain: agents will answer, and none of it is real work"
            : `metering ${i.usageMetering} · models ${Object.keys(i.models || {}).length}`,
      ),
    );
    return i;
  }

  /**
   * What this tenant is bound to — the template it was launched from and the
   * build it is serving.
   *
   * This is the check the whole pass rests on. A tenant on an old image reports
   * bugs `main` has already fixed, which is what made the first attempt at the
   * 2026-08-18 pass misleading. It cannot be automated into a verdict — no
   * endpoint reports a git SHA — so it prints the identity for a human to
   * compare against the commit under test, and stays `SKIP` until they do.
   */
  function checkRepoBinding(rows, spec, company) {
    const p = (company && company.template_provenance) || null;
    const build = spec ? `${spec.name}@${spec.version}` : "unknown build";
    rows.push(
      row(
        "repo-binding",
        SKIP,
        `${build} · template ${p ? `${p.source_id}${p.version ? `@${p.version}` : ""}` : "(none — raw manifest)"}`,
        "confirm by hand that this is the commit under test; a stale tenant reports bugs main has already fixed",
      ),
    );
  }

  async function checkWorkflows(rows, scope) {
    const res = await http(`${scope}/workflows`);
    if (!res.ok) {
      rows.push(unread("workflows", res, "workflows"));
      return [];
    }
    const flows = res.body || [];
    const paused = flows.filter((w) => w.enabled === false);
    rows.push(
      row(
        "workflows",
        flows.length ? PASS : WARN,
        `${flows.length} workflows${paused.length ? ` · ${paused.length} paused` : ""}: ${flows.map((w) => w.id).join(", ") || "(none)"}`,
        paused.length ? `paused: ${paused.map((w) => w.id).join(", ")} — saved and runnable by hand, skipped by the scheduler` : "",
      ),
    );
    return flows;
  }

  async function checkRunHistory(rows, scope) {
    const res = await http(`${scope}/workflows/runs?limit=50`);
    if (!res.ok) {
      rows.push(unread("run-history", res, "run history"));
      return [];
    }
    const runs = res.body || [];
    const tally = {};
    for (const r of runs) {
      const v = runVerdict(r);
      tally[v] = (tally[v] || 0) + 1;
    }
    const bad = (tally.failed || 0) + (tally.undelivered || 0);
    const newest = runs.reduce((max, r) => Math.max(max, r.atMillis || 0), 0);
    rows.push(
      row(
        "run-history",
        runs.length === 0 ? WARN : bad ? WARN : PASS,
        `${runs.length} runs · ${Object.entries(tally).map(([k, v]) => `${k}:${v}`).join(" ") || "(none)"} · newest ${age(newest)} ago`,
        runs.length === 0 ? "no run has ever been journaled here" : "",
      ),
    );
    return runs;
  }

  /**
   * Workflow deliveries — the check that caught #981.
   *
   * A run whose nodes are all `ok` can still have delivered nothing: a delivery
   * failure deliberately does not populate `error` or flip `nodes[].status`, so
   * folding node status is a green verdict on a dropped report. `deliveries[]`
   * is the only place this is visible, and nothing in the UI surfaces it beyond
   * a dot.
   */
  function checkDeliveries(rows, runs) {
    if (!runs || runs.length === 0) {
      rows.push(row("workflow-deliveries", SKIP, "no runs in history", "nothing to judge — untested, not passed"));
      return;
    }
    const all = runs.flatMap((r) => (r.deliveries || []).map((d) => ({ run: r, d })));
    const attempted = runs.filter((r) => (r.deliveries || []).length > 0);
    // Issue #981: the shared predicate, not a fourth transcription of it inside
    // this file. The status-only filter this replaces FAILED the whole check on
    // a `skipped`/`dry-run` row (a test run attempted nothing, on purpose) and
    // on a `skipped`/`already-delivered` one (an earlier run in the approval
    // lineage sent it) — so a company whose most recent runs were tests scored
    // a red row for a delivery path that is working.
    const dropped = all.filter(({ d }) => isUndelivered(d));
    const reasons = [...new Set(dropped.map(({ d }) => d.reason || d.status))];
    rows.push(
      row(
        "workflow-deliveries",
        dropped.length ? FAIL : all.length ? PASS : WARN,
        `${attempted.length}/${runs.length} runs attempted delivery · ${dropped.length}/${all.length} reports dropped${reasons.length ? ` (${reasons.join(", ")})` : ""}`,
        dropped.length
          ? dropped
              .slice(0, 3)
              .map(({ run, d }) => `${run.workflowId}/${d.node} → ${d.target || d.kind}: ${d.detail}`)
              .join(" | ")
          : all.length
            ? ""
            : "no run has ever attempted a delivery, so the delivery path is unexercised",
      ),
    );
  }

  async function checkDataHygiene(rows, scope) {
    const res = await http(`${scope}/memory/stats`);
    if (!res.ok) return push(rows, unread("data-hygiene", res, "memory stats"));
    const m = res.body || {};
    rows.push(
      row(
        "data-hygiene",
        PASS,
        `${m.totalItems || 0} items: ${m.facts || 0} facts · ${m.teammateMemory || 0} teammate memories · ${m.taskOutcomes || 0} outcomes · ${m.documentMemory || 0} document chunks · last write ${age(m.lastUpdatedAtMillis)} ago`,
        // `factsUpdatedAtMillis` sits at 0 for any company whose operator never
        // hand-authored a fact, so it is not the freshness signal.
        m.lastUpdatedAtMillis ? "" : "nothing has ever been remembered here",
      ),
    );
  }

  async function checkSkills(rows, scope) {
    const res = await http(`${scope}/skills`);
    if (!res.ok) return push(rows, unread("skills", res, "skills"));
    const skills = res.body || [];
    const on = skills.filter((s) => s.enabled);
    rows.push(
      row(
        "skills",
        skills.length ? PASS : WARN,
        `${on.length}/${skills.length} enabled: ${on.map((s) => s.id).join(", ") || "(none)"}`,
        skills.length ? "" : "no skills installed",
      ),
    );
  }

  async function checkUsageAndFinances(rows, scope) {
    const usage = await http(`${scope}/usage`);
    const finances = await http(`${scope}/finances`);
    if (!usage.ok && !finances.ok) {
      rows.push(unread("usage-finances", usage, "usage and finances"));
      return;
    }
    const u = usage.ok ? usage.body || {} : null;
    const f = finances.ok ? finances.body || {} : null;
    const totals = (u && u.totals) || {};
    // `/finances` is Phase 1 (`src/server/ops/finances.rs`), so an older tenant
    // can answer 200 with the figures simply absent. Reading them unguarded is
    // a `TypeError` on the one deployment this check exists to judge, and an
    // absent figure rendered as `$0.00` would be worse still: it reads as a
    // company that has spent nothing rather than one that did not say.
    const figures = ["spentUsd", "budgetUsd", "balanceUsd"];
    const missing = f ? figures.filter((k) => typeof f[k] !== "number") : figures;
    const priced = !!f && missing.length === 0;
    const overspent = priced && f.budgetUsd > 0 && f.spentUsd > f.budgetUsd;
    rows.push(
      row(
        "usage-finances",
        !u || !priced ? SKIP : overspent ? FAIL : PASS,
        `${totals.tokens || 0} tokens · $${(totals.costUsd || 0).toFixed(2)} · ${priced ? `spent $${f.spentUsd.toFixed(2)}/$${f.budgetUsd.toFixed(2)} · balance $${f.balanceUsd.toFixed(2)}` : "finances unread"}`,
        overspent
          ? "spend is past the manifest budget"
          : !u || !f
            ? "one of the two surfaces did not answer"
            : priced
              ? ""
              : `/finances answered without ${missing.join(", ")} — untested, not passed`,
      ),
    );
  }

  async function checkChatHistory(rows, scope) {
    const res = await http(`${scope}/chat/history`);
    if (!res.ok) return push(rows, unread("chat-history", res, "chat history"), []);
    const msgs = res.body || [];
    const newest = msgs.reduce((max, m) => Math.max(max, m.atMillis || 0), 0);
    const mine = msgs.filter((m) => m.mine).length;
    rows.push(
      row(
        "chat-history",
        msgs.length ? PASS : WARN,
        `${msgs.length} messages (${mine} operator) · newest ${age(newest)} ago`,
        msgs.length ? "" : "an empty transcript on a tenant that has been used means messages are not being journaled (#983)",
      ),
    );
    return msgs;
  }

  /**
   * The tool catalog the orchestrator actually holds.
   *
   * `requested` alone reports the opposite of the truth for a manifest agent
   * whose `tools` line is empty — that means the company's standard grant, not
   * "no tools" — so this reads `effective`, which is what the agent runs with.
   */
  async function checkToolCatalog(rows, scope, team) {
    if (!team || team.length === 0) {
      rows.push(row("tool-catalog", SKIP, "no roster", "nothing to read a grant from"));
      return;
    }
    const orchestrator = team.find((t) => t.isOrchestrator) || team[0];
    const res = await http(`${scope}/team/${encodeURIComponent(orchestrator.id)}`);
    if (!res.ok) return push(rows, unread("tool-catalog", res, "the orchestrator's grants"));
    const detail = res.body || {};
    const tools = detail.tools || {};
    const effective = tools.effective || [];
    rows.push(
      row(
        "tool-catalog",
        effective.length ? PASS : FAIL,
        `${orchestrator.id}: ${effective.length} effective (requested ${(tools.requested || []).length}, ceiling ${(tools.companyAllow || []).length})`,
        effective.length ? effective.slice(0, 12).join(", ") : "the orchestrator holds no tools, so it can do nothing but talk",
      ),
    );
  }

  /* ------------------------------------------------------------------ *
   * OCQA.read()
   * ------------------------------------------------------------------ */

  async function read(options) {
    const opts = options || {};
    const rows = [];
    const started = Date.now();

    // Every check is run through `attempt`, so one that throws on a body whose
    // shape has drifted costs its own row and not the whole pass. The fallback
    // handed back is the same empty value the check's own unreadable path
    // returns, so a throw cannot poison the check downstream of it either.
    const spec = await attempt(rows, "host", () => checkHost(rows));
    await attempt(rows, "console-cache-headers", () => checkConsoleCacheHeaders(rows));

    const scope = await resolveScope(opts.company);
    if (!scope) {
      rows.push(row("scope", FAIL, "no company", "neither /api/v1/company nor /api/v1/companies answered — sign in first"));
      return finish(rows, started, "read");
    }

    const company = await attempt(rows, "company-lifecycle", () => checkLifecycle(rows, scope));
    const team = await attempt(rows, "roster", () => checkRoster(rows, scope), []);
    await attempt(rows, "desks", () => checkDesks(rows, scope));
    await attempt(rows, "task-board", () => checkTaskBoard(rows, scope));
    await attempt(rows, "workspace", () => checkWorkspace(rows, scope));
    const approvals = await attempt(rows, "approvals-backlog", () => checkApprovals(rows, scope), []);
    await attempt(rows, "approval-tier", () => checkApprovalTier(rows, scope, approvals));
    await attempt(rows, "manifest-connections", () => checkConnections(rows, scope));
    await attempt(rows, "composio-health", () => checkComposio(rows, scope));
    const caps = await http(`${scope}/capabilities`);
    await attempt(rows, "mcp-servers", () => checkMcp(rows, scope, spec, caps.ok ? caps.body : null));
    await attempt(rows, "inference-provider", () => checkInference(rows, scope));
    await attempt(rows, "repo-binding", () => checkRepoBinding(rows, spec, company));
    await attempt(rows, "workflows", () => checkWorkflows(rows, scope));
    const runs = await attempt(rows, "run-history", () => checkRunHistory(rows, scope), []);
    await attempt(rows, "workflow-deliveries", () => checkDeliveries(rows, runs));
    await attempt(rows, "data-hygiene", () => checkDataHygiene(rows, scope));
    await attempt(rows, "skills", () => checkSkills(rows, scope));
    await attempt(rows, "usage-finances", () => checkUsageAndFinances(rows, scope));
    await attempt(rows, "chat-history", () => checkChatHistory(rows, scope));
    await attempt(rows, "tool-catalog", () => checkToolCatalog(rows, scope, team));

    return finish(rows, started, "read", scope);
  }

  /* ------------------------------------------------------------------ *
   * OCQA.probe() — five checks, four of them live, that spend real tokens
   * ------------------------------------------------------------------ */

  /**
   * A chat round trip.
   *
   * Given its own timeout well under the edge's 120s (#983), because the point
   * is to *measure* the round trip rather than inherit an unbounded wait. A
   * timeout here is a real finding with a number attached, not a hang.
   */
  async function probeChat(rows, scope, opts) {
    const text = opts.chatText || "QA probe: reply with one short sentence naming this company. Do not open a task.";
    const res = await http(`${scope}/chat`, { method: "POST", body: { text }, timeoutMs: opts.chatTimeoutMs || 90000 });
    if (!res.ok) {
      rows.push(
        row(
          "probe-chat",
          FAIL,
          `${res.error || `HTTP ${res.status}`} after ${secs(res.elapsedMs)}`,
          "the turn may still be running invisibly — check chat/history and the board before calling it lost (#983)",
        ),
      );
      return;
    }
    const replies = (res.body && res.body.responses) || [];
    const first = replies[0] ? String(replies[0].text || "").slice(0, 120) : "";
    rows.push(
      row(
        "probe-chat",
        replies.length === 0 ? FAIL : res.elapsedMs > 30000 ? WARN : PASS,
        `${secs(res.elapsedMs)} · ${replies.length} replies · ${first.length} chars`,
        replies.length === 0 ? "a 200 with no reply is a turn that answered nothing" : "",
        // The reply itself is real tenant content and `report()` is written to
        // be pasted into a public issue, so the verdict is formed from the
        // shape of the answer and the text rides in `detail`, which the report
        // withholds. It is still on the operator's own screen, where judging
        // whether the company answered *well* needs it.
        first ? `"${first}"` : "",
      ),
    );
  }

  /** One addressed message per desk — the routing every desk claims to support. */
  async function probeDesks(rows, scope, opts) {
    const res = await http(`${scope}/desks`);
    if (!res.ok) return push(rows, unread("probe-desks", res, "desks"));
    const desks = (res.body || []).filter((d) => (d.members || []).length > 0);
    if (desks.length === 0) return push(rows, row("probe-desks", SKIP, "no desks with members", "nothing to address"));
    const results = [];
    for (const desk of desks.slice(0, opts.maxDesks || 5)) {
      const r = await http(`${scope}/chat`, {
        method: "POST",
        body: { text: "QA probe: in one sentence, what does this desk do?", chat: desk.id },
        timeoutMs: opts.chatTimeoutMs || 90000,
      });
      const replies = (r.body && r.body.responses) || [];
      results.push({ desk: desk.id, ok: r.ok && replies.length > 0, took: secs(r.elapsedMs) });
    }
    const failed = results.filter((r) => !r.ok);
    rows.push(
      row(
        "probe-desks",
        failed.length ? FAIL : PASS,
        results.map((r) => `${r.desk}:${r.ok ? "" : "FAIL@"}${r.took}`).join(" · "),
        failed.length ? `${failed.map((r) => r.desk).join(", ")} did not answer` : "",
      ),
    );
  }

  /** Card create → visible on the board → delete. The board's whole write path. */
  async function probeBoardCard(rows, scope) {
    const title = `QA probe card ${new Date().toISOString()}`;
    const created = await http(`${scope}/tasks`, { method: "POST", body: { title, note: "Created by oc-qa.js. Safe to delete." } });
    if (!created.ok || !created.body || !created.body.id) {
      return push(rows, row("probe-board-card", FAIL, `create → ${created.error || `HTTP ${created.status}`}`, "the board cannot take a card"));
    }
    const id = created.body.id;
    const board = await http(`${scope}/tasks`);
    const visible = board.ok && (board.body || []).some((t) => t.id === id);
    const removed = await http(`${scope}/tasks/${encodeURIComponent(id)}`, { method: "DELETE" });
    rows.push(
      row(
        "probe-board-card",
        visible && removed.ok ? PASS : FAIL,
        `create ok · on board ${visible ? "yes" : "NO"} · delete ${removed.ok ? "ok" : `HTTP ${removed.status}`}`,
        visible ? (removed.ok ? "" : `probe card ${id} was left behind — delete it by hand`) : "a created card the board does not list",
      ),
    );
  }

  /**
   * One real workflow run, judged to a terminal state.
   *
   * Detached, then polled out of history: a synchronous run inherits the edge's
   * unbounded wait (#983), and the run's own duration is what we are measuring.
   * The verdict comes from `runVerdict`, so a run that delivered nothing scores
   * `undelivered` and not `ok` — the mistake the first version of this check made.
   */
  async function probeWorkflowRun(rows, scope, opts) {
    const list = await http(`${scope}/workflows`);
    if (!list.ok) return push(rows, unread("probe-workflow-run", list, "workflows"));
    const flows = list.body || [];
    // The target is never chosen for you. A real run fires real effects — a
    // report to a channel, mail to a real address — and `flows[0]` is whatever
    // the host happened to list first, which on a production tenant is a
    // stranger's workflow. Naming it is the operator saying which one; there is
    // no default that is safe to guess, so the unnamed case is SKIP.
    if (!opts.workflow) {
      return push(
        rows,
        row(
          "probe-workflow-run",
          SKIP,
          flows.length ? `${flows.length} workflows, none named` : "no workflows",
          'this check will not choose its own target — pass { workflow: "<id>" } to run one for real, or { workflow: "<id>", dryRun: true } to rehearse it',
        ),
      );
    }
    const target = flows.find((w) => w.id === opts.workflow);
    if (!target) {
      return push(rows, row("probe-workflow-run", SKIP, `no workflow "${opts.workflow}"`, "nothing to run"));
    }
    const body = { input: {} };
    if (opts.dryRun) body.dry_run = true;
    else body.detach = true;
    const started = Date.now();
    const res = await http(`${scope}/workflows/${encodeURIComponent(target.id)}/run`, { method: "POST", body, timeoutMs: 120000 });
    if (!res.ok) {
      if (notWired(res)) {
        return push(
          rows,
          row("probe-workflow-run", SKIP, `${target.id} → workflow execution not wired in this deployment`, "this build has no workflow runner — untested, not failed"),
        );
      }
      return push(rows, row("probe-workflow-run", FAIL, `${target.id} → ${res.error || `HTTP ${res.status}`}`, "the run was not accepted"));
    }

    // A host predating detach ignores the flag and answers the settled run, and
    // a host predating dry-run ignores that flag and runs FOR REAL. Both are
    // read back off the response, never assumed from what was asked.
    if (opts.dryRun && res.body && res.body.dryRun !== true) {
      rows.push(row("probe-workflow-run", WARN, `${target.id}: dryRun absent from the response`, "this host ignored the flag and ran for real — effects fired"));
    }
    if (res.body && res.body.detached !== true) {
      // Keep the facts the host sent. Rebuilding this from `deliveries`
      // alone discarded both `verdict` — the host's own answer, which
      // `runVerdict` prefers over its fallback ladder — and `nodes`, which is
      // the only evidence a step errored while the run carried on. A degraded
      // run then scored `ok` and the probe reported PASS (issue #1865).
      const settled = {
        deliveries: (res.body && res.body.deliveries) || [],
        nodes: (res.body && res.body.nodes) || [],
        verdict: res.body && res.body.verdict,
        error: null,
        running: false,
      };
      const verdict = runVerdict(settled);
      return push(rows, 
        row(
          "probe-workflow-run",
          verdict === "ok" ? PASS : verdict === "awaiting-approval" ? WARN : FAIL,
          `${target.id} → ${verdict} in ${secs(Date.now() - started)} · ${settled.deliveries.length} deliveries`,
          describeDeliveries(settled.deliveries),
        ),
      );
    }

    const runId = res.body.runId;
    const deadline = Date.now() + (opts.runTimeoutMs || 300000);
    let last = null;
    while (Date.now() < deadline) {
      await sleep(3000);
      const history = await http(`${scope}/workflows/runs?workflow=${encodeURIComponent(target.id)}&limit=20`);
      if (!history.ok) continue;
      last = (history.body || []).find((r) => r.runId === runId) || null;
      if (last && last.running !== true) break;
    }
    if (!last) {
      return push(rows, row("probe-workflow-run", SKIP, `${target.id} run ${runId} never appeared in history`, "untested — the run may still be walking its graph"));
    }
    const verdict = runVerdict(last);
    if (verdict === "running") {
      return push(rows, row("probe-workflow-run", FAIL, `${target.id} → still running after ${secs(Date.now() - started)}`, "did not reach a terminal state inside the probe window"));
    }
    rows.push(
      row(
        "probe-workflow-run",
        verdict === "ok" ? PASS : verdict === "awaiting-approval" ? WARN : FAIL,
        `${target.id} → ${verdict} in ${secs(Date.now() - started)} · ${(last.nodes || []).length} nodes · ${(last.deliveries || []).length} deliveries`,
        last.error || describeDeliveries(last.deliveries),
      ),
    );
  }

  function describeDeliveries(deliveries) {
    const dropped = (deliveries || []).filter((d) => d.status !== "sent" && d.status !== "pending");
    if (dropped.length === 0) return "";
    return dropped.map((d) => `${d.node} → ${d.target || d.kind}: ${d.reason || d.status} (${d.detail})`).join(" | ");
  }

  /** Did the probes park anything new at the gate? */
  async function probeApprovalDelta(rows, scope, before) {
    const res = await http(`${scope}/approvals`);
    if (!res.ok) return push(rows, unread("probe-approval-delta", res, "the approvals queue"));
    const after = res.body || [];
    const seen = new Set((before || []).map((a) => a.id));
    const fresh = after.filter((a) => !seen.has(a.id));
    rows.push(
      row(
        "probe-approval-delta",
        fresh.length ? WARN : PASS,
        `${fresh.length} newly parked${fresh.length ? `: ${fresh.map((a) => a.kind).join(", ")}` : ""}`,
        fresh.length ? "the probes left effects at the gate — resolve or deny them before leaving the tenant" : "",
      ),
    );
  }

  function sleep(ms) {
    return new Promise((resolve) => setTimeout(resolve, ms));
  }

  async function probe(options) {
    const opts = options || {};
    const rows = [];
    const started = Date.now();
    const scope = await resolveScope(opts.company);
    if (!scope) {
      rows.push(row("scope", FAIL, "no company", "sign in first"));
      return finish(rows, started, "probe");
    }
    // Named before anything fires. These probes spend real tokens and leave
    // real traces on whatever tenant the tab is signed in to, and the one thing
    // an operator cannot recover from is not having known which host that was.
    console.log(
      `%coc-qa ${VERSION} · probe → ${hostname()} · ${scope} — real turns, real effects, on this tenant.`,
      "color:#d97706;font-weight:bold",
    );
    const before = await http(`${scope}/approvals`);
    await attempt(rows, "probe-chat", () => probeChat(rows, scope, opts));
    await attempt(rows, "probe-desks", () => probeDesks(rows, scope, opts));
    await attempt(rows, "probe-board-card", () => probeBoardCard(rows, scope));
    await attempt(rows, "probe-workflow-run", () => probeWorkflowRun(rows, scope, opts));
    await attempt(rows, "probe-approval-delta", () => probeApprovalDelta(rows, scope, before.ok ? before.body : []));
    return finish(rows, started, "probe", scope);
  }

  /* ------------------------------------------------------------------ *
   * Output
   * ------------------------------------------------------------------ */

  let lastRun = null;

  function finish(rows, started, mode, scope) {
    const tally = { PASS: 0, WARN: 0, FAIL: 0, SKIP: 0 };
    for (const r of rows) tally[r.verdict] = (tally[r.verdict] || 0) + 1;
    lastRun = {
      mode,
      scope: scope || null,
      host: hostname(),
      atMillis: Date.now(),
      elapsedMs: Date.now() - started,
      tally,
      rows,
    };
    console.table(rows);
    console.log(
      `%coc-qa ${VERSION} · ${mode} · ${rows.length} checks in ${secs(Date.now() - started)} — ` +
        `${tally.PASS} pass, ${tally.WARN} warn, ${tally.FAIL} fail, ${tally.SKIP} untested`,
      tally.FAIL ? "color:#dc2626;font-weight:bold" : tally.WARN || tally.SKIP ? "color:#d97706" : "color:#059669",
    );
    if (tally.SKIP) {
      console.log("%cSKIP means untested, not passed. Do not write those up as green.", "color:#d97706");
    }
    console.log("OCQA.report() → Markdown for an issue.");
    return rows;
  }

  /**
   * The last run as a Markdown table, for pasting straight into an issue.
   *
   * Which is the reason for the redaction: this output is written to be moved
   * out of the tenant and into a public artifact, so the message text a probe
   * collected does not travel with it. `report({ raw: true })` opts back in for
   * a private write-up, and the table says when something was withheld rather
   * than quietly shortening a value.
   *
   * Ids — company, desks, workflows, delivery targets — are still in here. They
   * are what makes a FAIL actionable, and a report with them stripped names no
   * defect. Read the table before pasting it somewhere public.
   */
  function report(options) {
    if (!lastRun) return "No run yet. Call OCQA.read() first.";
    const raw = !!(options && options.raw);
    const withheld = lastRun.rows.filter((r) => r.detail).length;
    const lines = [
      `## oc-qa ${VERSION} — \`${lastRun.mode}\` on \`${lastRun.host}\``,
      "",
      `${new Date(lastRun.atMillis).toISOString()} · ${secs(lastRun.elapsedMs)} · ` +
        `${lastRun.tally.PASS} pass / ${lastRun.tally.WARN} warn / ${lastRun.tally.FAIL} fail / ${lastRun.tally.SKIP} untested`,
      "",
      "| Check | Verdict | Value judged | Note |",
      "| --- | --- | --- | --- |",
    ];
    for (const r of lastRun.rows) {
      const cell = (s) => String(s).replace(/\|/g, "\\|");
      const value = raw && r.detail ? `${r.value} · ${r.detail}` : r.value;
      lines.push(`| \`${r.check}\` | ${r.verdict} | ${cell(value)} | ${cell(r.note)} |`);
    }
    if (withheld && !raw) {
      lines.push("");
      lines.push(
        `_${withheld} row${withheld === 1 ? "" : "s"} carried tenant message text, withheld from this report. ` +
          "`OCQA.report({ raw: true })` includes it — do not paste that into a public issue._",
      );
    }
    const text = lines.join("\n");
    console.log(text);
    return text;
  }

  global.OCQA = {
    version: VERSION,
    read,
    probe,
    report,
    /** Pure judgements, exposed so `frontend/test/unit/qa-harness.test.ts` can pin them. */
    _internals: {
      runVerdict,
      undeliveredCount,
      isUndelivered,
      checkDeliveries,
      pendingCount,
      awaitingCount,
      isBlocked,
      judgeCacheHeader,
      age,
      notWired,
      secs,
    },
  };

  console.log(`oc-qa ${VERSION} loaded. OCQA.read() · OCQA.probe() · OCQA.report()`);
})(typeof globalThis !== "undefined" ? globalThis : this);
