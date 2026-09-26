import { readFileSync } from "node:fs";
import { dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";

import { describe, expect, it } from "vitest";

/**
 * What pinning the Room rail costs `RoomView`, and the three things that pay it
 * (issue #2130).
 *
 * The rail is painted in the sidebar on every section now, and it is portalled
 * out of `RoomView` — so the shell keeps that view mounted on every route and
 * hands it `routeOpen`. Three consequences follow, and each one was found the
 * hard way or is one edit from being lost:
 *
 *   1. **A mounted view must not steer the route.** `RoomView` restores the
 *      remembered channel into the hash whenever the hash names no channel.
 *      Mounted everywhere, that fires on `#/workflows` and `#/connections` too
 *      — which name no second segment — and navigates the operator straight
 *      back out of the section they just opened. Found in a browser: clicking
 *      **Flows** landed on `#/chat/main`.
 *   2. **A mounted view must not paint over the page.** The transcript, its
 *      header and the members pane render only when `routeOpen`.
 *   3. **The dialogs must NOT be gated with them.** Their triggers are painted
 *      in the sidebar — the rail's "+" and its "New message" pencil — so they
 *      have to open from Company and Flows exactly as from Room. A portal moves
 *      the DOM node, not the component tree, which is what makes that possible
 *      at all, and putting them inside the `routeOpen` gate would quietly undo
 *      it: the trigger would still be on screen and would open nothing.
 *
 * A source guard rather than a render test, in the idiom of
 * `responsive-two-rail-band`: (1) needs a hash router and a mounted shell to
 * reproduce, and (3) is invisible below the level of the whole shell — the
 * dialogs are correct read on their own and wrong only once you know where
 * their triggers are painted. `section-rail-layout.test.ts` covers what the
 * rail and the content rail render.
 */

const here = dirname(fileURLToPath(import.meta.url));
const read = (rel: string) => readFileSync(resolve(here, "../../src", rel), "utf8");

describe("RoomView, mounted off its own route", () => {
  const chatView = read("views/RoomView.tsx");

  it("refuses to restore the remembered channel while another section is open", () => {
    // Anchored on the effect's own body rather than on `const restoredFor =
    // useRef`: since B-096 the ref is declared with the other long-lived refs,
    // well above the three early returns, while the effect that reads it moved
    // below `channel` — so slicing from the declaration now runs through other
    // hooks and finds their dependency arrays first.
    const body = chatView.indexOf("restoredFor.current = undefined;");
    expect(body).toBeGreaterThan(-1);
    const effect = chatView.slice(chatView.lastIndexOf("useEffect(() => {", body));
    // The guard, and that it is the FIRST thing the effect does — after the
    // `if (sub)` line it is already too late for a bare `#/workflows`.
    const guard = effect.indexOf("if (!routeOpen) return;");
    const subCheck = effect.indexOf("if (sub) {");
    expect(guard).toBeGreaterThan(-1);
    expect(subCheck).toBeGreaterThan(-1);
    expect(guard).toBeLessThan(subCheck);
    // And it is a dependency, so arriving on Room re-runs the restore rather
    // than skipping it for the life of the mount.
    expect(effect.slice(0, effect.indexOf("}, [") + 200)).toContain(
      "[routeOpen, scope, sub, channel, sections, onNavigate]",
    );
    // B-096 made this guard matter MORE, not less. The effect it replaced only
    // navigated when something was remembered (`if (remembered)`), so an
    // operator who had never opened a channel was accidentally spared; this one
    // always resolves — memory, else `channel.id` — and without the guard would
    // bounce every such operator out of Flows on the first paint.
    expect(effect).toContain("const remembered = readLastChannel(scope);");
    expect(effect).toContain("onNavigate(remembered && !archived ? remembered : channel.id);");
  });

  it("renders the transcript only on its own route", () => {
    expect(chatView).toContain("{routeOpen && (");
  });

  it("re-reads `?thread=` when Room is entered, and rewrites no other section's hash", () => {
    // Codex P2 on this PR. A task card's "Opened from chat" link goes
    // `#/tasks/<id>` → `#/chat/<channel>?thread=<id>`, and this view can
    // already be sitting on that very channel — the shell replays the last chat
    // segment while the address belongs to another section. Only `routeOpen`
    // and the query change, so an effect keyed on `channel?.id` alone never
    // fired: the thread did not open and the query was never consumed.
    //
    // It is a guard as well as a dependency, and the guard matters on its own:
    // the effect calls `replaceState` on whatever hash it finds, and off Room
    // that hash belongs to Company or Flows.
    const effect = chatView.slice(chatView.indexOf("const arrived = threadResolvedFor.current") - 400);
    expect(effect).toContain("if (!routeOpen || !channel?.id) return;");
    expect(effect.slice(0, effect.indexOf("}, [") + 60)).toContain(
      "[routeOpen, channel?.id, threadQuery]",
    );

    // And the query is reactive, which is what makes a SAME-channel link work:
    // `#/chat/general` → `#/chat/general?thread=h41` moves neither `sub` nor
    // `channel.id`, so an effect keyed on those alone never fires (CodeRabbit
    // review on #2130). `useHashView` parses only the path segments; this keeps
    // the subscription `useHashFlag` already uses for `?new`.
    expect(chatView).toContain('new URLSearchParams(query).get("thread")');
    expect(chatView).toContain('window.addEventListener("hashchange", apply)');
    // Carried with a nonce, so opening the SAME thread twice still fires: the
    // first open consumes the query, so the second link's parsed value is the
    // one already held and React would bail out of the re-render. Found in a
    // browser, where the third of three `?thread=` links did nothing.
    expect(chatView).toContain("nonce: prev.nonce + 1");
    // And one link opens one thread ONCE. Consuming fires no `hashchange`, so
    // the query keeps naming the thread it just opened — and
    // `useHashView.navigate` sets the hash and calls `setRoute` synchronously
    // while that event is still pending. So picking another channel re-ran this
    // effect for the new channel with the old query and reopened the thread
    // there, on a channel with no such parent, suppressing its live steps and
    // receipt. The correcting pass could not undo it: `threadResolvedFor`
    // already held the new channel, so `arrived` was false (Codex P2).
    expect(chatView).toContain(
      "threadQuery.value !== null && consumedThreadNonce.current !== threadQuery.nonce",
    );
    expect(chatView).toContain("consumedThreadNonce.current = threadQuery.nonce;");
    // Consuming is a `replaceState`, which fires no `hashchange` — so the value
    // stays put and the effect cannot loop on its own write. A hash change that
    // names no thread is somebody else's query moving, and only an ARRIVAL
    // closes an open panel.
    expect(effect).toContain("if (arrived) setOpenThreadId(null);");
  });

  it("keeps both sidebar-triggered dialogs outside that gate", () => {
    // Everything after the gate closes is what stays mounted off Room. Both
    // dialogs have to be in it: their triggers are the rail's "+" and its "New
    // message" pencil, which are painted in the sidebar on every section.
    const gate = chatView.indexOf("{routeOpen && (");
    const gateClose = chatView.indexOf("\n      )}\n", gate);
    const dialog = chatView.search(/<ChannelCreateDialog[\s/>]/);
    expect(gate).toBeGreaterThan(-1);
    expect(gateClose).toBeGreaterThan(gate);
    expect(dialog, "ChannelCreateDialog must mount after the routeOpen gate closes").toBeGreaterThan(
      gateClose,
    );
    // `NewMessageDialog` mounts inside `ChannelRail`, which is the portalled
    // node itself — so it rides along with the rail rather than needing a place
    // in this tail. Asserted from the rail's side so the pairing is stated
    // somewhere rather than assumed.
    expect(read("views/room/ChannelRail.tsx")).toMatch(/<NewMessageDialog[\s/>]/);
  });

  it("closes the Room-only dialogs on the way out, and only those", () => {
    // Codex P2 on this PR. `AddMemberDialog` opens from the members pane, which
    // is inside the `routeOpen` gate — but every dialog in this file sits
    // OUTSIDE that gate, because two of them have triggers painted in the
    // sidebar. Right for those two, wrong for this one: leaving Room on Back
    // used to unmount the view, and left an "Add agent" sheet standing over
    // Company once it stopped doing so.
    //
    // `BudgetDialog` was the second one here, and held the member it was opened
    // for so it came back still pointing at them. Per-agent caps are gone from
    // the console, and the dialog with them.
    const effect = chatView.slice(chatView.indexOf("if (routeOpen) return;"));
    expect(chatView).toContain("if (routeOpen) return;");
    expect(effect.slice(0, 200)).toContain("setAddOpen(false)");
    // And NOT the two the sidebar opens — closing those on the way out is the
    // whole thing this PR had to keep working from another section.
    expect(effect.slice(0, 200)).not.toContain("setChannelCreateOpen(false)");
  });

  it("stops the pinned rail claiming to be the current page off Room", () => {
    // Two nodes answering `aria-current="page"` is a page a screen reader
    // cannot locate you on. On `#/finances/wallet` there were exactly that: the
    // channel rail's open channel and the section rail's open sub-page. Off
    // Room the rail's mark is "where Room will take you back to", which is
    // `aria-current="true"` — a current item within a set, not a current page.
    expect(chatView).toContain("currentPage={routeOpen}");
    const rail = read("views/room/ChannelRail.tsx");
    expect(rail).toContain('const activeAria: "page" | "true" = currentPage ? "page" : "true";');
    // Every row shape reads the resolved value, so neither can drift.
    expect(rail.match(/aria-current=\{active \? activeAria : undefined\}/g) ?? []).toHaveLength(2);
    expect(rail).not.toContain('aria-current={active ? "page" : undefined}');
    // And a standalone rail — the unit tests, a rail beside its own transcript —
    // keeps saying `page` with nothing configured.
    expect(rail).toContain("currentPage = true,");
  });

  it("re-reads the roster, desks and directory on every entry to Room", () => {
    // Codex P2 on this PR. Every one of these was keyed on `[client, company]`
    // alone, which was sufficient while navigating away unmounted the view:
    // coming back was a mount, and a mount re-read everything. Add a teammate on
    // Company or delete a desk on the org chart now, and the pinned rail would
    // go on showing what it loaded once — in plain sight, because the rail is on
    // screen the whole time.
    expect(chatView).toContain("const [roomVisits, setRoomVisits] = useState(0);");
    // A `false → true` transition, and only after the mount. Counting
    // `routeOpen` being true at all counted the ordinary startup — the console
    // opens on `#/chat` — so every read ran twice in a row and `loadDesks`
    // dropped the pane back to its loading state a frame after it arrived
    // (Codex P2).
    expect(chatView).toContain("const entered = routeOpen && !wasRouteOpen.current;");
    expect(chatView).toContain("if (entered) setRoomVisits((n) => n + 1);");
    // Cognition, the roster, the viewer's people, the desks and the mention
    // directory: everything the rail, the composer and
    // the warning strip draw. Cognition is in the set even though it refreshes
    // on `visibilitychange` — that event is about the tab, not the route, and an
    // admin who fixes Inference and comes back has never hidden the tab.
    //
    // Deliberately NOT `reloadDirectory`'s own `useCallback`: that is a handle
    // other code calls after it changes something, not a read on a schedule, and
    // re-keying it would only churn its identity.
    expect(chatView.match(/\}, \[client, company, roomVisits\]\);/g) ?? []).toHaveLength(5);
    expect(chatView).toContain("const reloadDirectory = useCallback");

    // On ENTRY, not on `routeOpen` itself: that moves in both directions, so
    // leaving Room would spend a second round of reads on a section just left.
    // And not as a *gate* on the reads either — that would leave the rail empty
    // on a console loaded straight onto `#/company`, which is the one thing this
    // change exists to prevent.
    expect(chatView).not.toContain("}, [client, company, routeOpen]);");

    // Re-keying makes two of these overlap for the first time, so the roster
    // read needed the run token its neighbours already had (Codex P2). A console
    // that loads off Room and is taken into Room before the first `listTeam`
    // settles starts a second one beside it — and the failure path wrote
    // unconditionally, so a slow older rejection landing after a newer success
    // replaced a real roster with `[]`/`fromHost: false`: Direct messages gone
    // and "New channel" with them, until some later entry happened to fix it.
    expect(chatView).toContain("const ticket = ++rosterRead.current;");
    // Guarded in both directions — a stale rejection overwriting a fresh success
    // is the same bug with the sign flipped.
    const boot = chatView.slice(chatView.indexOf("const boot = useCallback"));
    const bootBody = boot.slice(0, boot.indexOf("}, [client, company, roomVisits]);"));
    expect(bootBody.match(/if \(!isCurrent\(\)\) return;/g) ?? []).toHaveLength(2);
    expect(bootBody).toContain("if (isCurrent()) setLoadingTeam(false);");
  });

  it("seeds the pinned rail's highlight from the channel Room will actually open", () => {
    // Codex P2 on this PR. A console loaded straight onto `#/company` has never
    // had `view === "chat"`, so a bare `null` left the rail highlighting the
    // first desk while clicking Room ran chat's own bare-route restoration and
    // landed somewhere else. A highlight has to name the destination it offers.
    const shell = read("components/app-shell.tsx");
    expect(shell).toContain("useState<string | null>(() => readLastChannel(scope))");
    // The same value chat restores from, read the same scoped way.
    expect(chatView).toContain("readLastChannel(scope)");
  });

  it("is mounted by the shell unconditionally, with routeOpen as the only gate", () => {
    const shell = read("components/app-shell.tsx");
    // The regression this replaces: `{view === "chat" && <RoomView …/>}`, which
    // unmounted the rail's owner the moment the operator left Room.
    expect(shell).not.toMatch(/\{view === "chat" && \(\s*<RoomView/);
    expect(shell).toContain('routeOpen={view === "chat"}');
    // And it is handed the CHAT segment, not the current view's. On
    // `#/connections/mcp` the live `sub` is `mcp`, which chat would resolve as
    // a channel id.
    expect(shell).toContain('sub={view === "chat" ? sub : chatSub}');
  });
});
