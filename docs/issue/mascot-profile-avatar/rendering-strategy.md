# Rendering strategy: where the live canvas goes

## The call-site audit

`TeammateAvatar` (`frontend/src/components/teammate-avatar.tsx`) renders in
roughly 30 places across the console. They split cleanly into two groups.

**Mass-render surfaces** — many instances mounted at once, mostly small
(`size-4` to `size-9`): the avatar-picker's own flavour grid, episode
conversation chips, team member cards, round-band seat rows, the DM channel
list, the `@mention` picker, transcript step sub-lists, thread reply rows,
channel members panes, the channel-create member picker, the main chat
gutter (one per message row), reply facepiles, the comms graph (one node per
agent), an agent's session/log list, and — the highest-count case — the org
chart, where every seat, every "on the roster" dropdown row, and every
desk-membership chip renders one. Mounting an independent WASM/canvas
instance at every one of these, simultaneously, on a busy screen would be a
real performance problem: dozens of Rive runtimes initializing at once for
tiles too small to show the animation meaningfully anyway (most are under
36px).

**Single/hero surfaces** — at most one or two on screen: the agent profile
sheet header (`size-12`), the current user's own avatar button, the
avatar-picker's "current selection" preview (`size-14`, separate from the
flavour grid), a DM channel header face (`size-6` — small despite being a
hero spot), the "live" chat receipt row, the transcript's one-time intro
mark, the "still working" indicator row, and the agent detail page's hero
avatar (`size-14`).

## The decision

`mascot:animated` resolves to `null` from `staticAvatarSrc`
(`frontend/src/lib/avatar.ts`) — the same thing an unrecognized or
not-yet-resolved reference does today. `TeammateAvatar`'s `AvatarTile`
already draws the tone tile with initials underneath, and only paints an
`<img>` on top when `src` resolves to something; with `mascot:` returning
`null`, every mass-render surface above keeps working completely unchanged,
today's tests included — **none of it needs to know a third avatar form
exists.**

A live `<MascotAvatar>` (a new component, not a `TeammateAvatar` variant)
is mounted explicitly at exactly two call sites for v1:

- `frontend/src/components/agent-profile-sheet.tsx` — the header avatar,
  where the `TeammateAvatar` tag currently sits inside the `SheetHeader`
  block (`className="size-12 rounded-xl text-sm"`, `data-testid="agent-profile-avatar"`).
  Swap it for `<MascotAvatar>` when `profile.avatar === "mascot:animated"`,
  `<TeammateAvatar>` otherwise.
- `frontend/src/components/avatar-picker.tsx` — two spots: the "current
  selection" preview (`data-testid="avatar-preview"`, `size-14`) needs the
  same conditional swap, and the flavour grid needs a **12th tile**
  alongside the 11 `TINY_FLAVOURS` swatches, rendering `<MascotAvatar>` at
  idle so the option is visibly previewable before it's chosen. Selecting it
  calls `onChange("mascot:animated")`, exactly like a flavour click does
  today — no new state-management pattern needed in the picker.

**Not required for v1, but architecturally free to add later** because it's
the same hero shape: `frontend/src/views/team/AgentDetailView.tsx`, which
has its own `size-14` hero avatar (`data-testid="agent-avatar"`, two render
paths — editable and static variants of the same slot).

## Test impact

Checked every test file that touches an avatar: nothing in `frontend/test`
references the avatar-picker's `avatar-flavours` / `avatar-flavour-<x>` /
`avatar-preview` testids outside `avatar-picker.tsx` itself, and no spec
imports `AvatarPicker` at all. The tests that do assert on avatar `<img>`
markup — `chat-thread-avatar` (unit + e2e), `chat-channel-intro`,
`company-cards.spec.ts`'s `agent-avatar` check — all target other
components' testids on the *static-tile* path, which is unchanged by this
work. **This is additive: nothing existing needs to change to keep passing.**
New tests for the picker's 12th tile and the profile-sheet swap are new
coverage, not test repairs.

## Bundle and load strategy

No Rive dependency exists in `frontend/package.json` today — `@rive-app/react-canvas`
is net new, and so is the ~1.8&nbsp;MB `.riv` asset. The console already has
an established pattern for isolating exactly this kind of weight:
`lazy(() => import("@/path").then((m) => ({ default: m.X })))`, used eleven
times today (`StandaloneStyleguide` for `recharts`, `WorkflowsView` for
`@xyflow/react`, `Joyride` for `react-joyride`, plus `ObservatoryView`,
`WorkspaceView`, `MemoryView`, `FinanceSection`, `PagesView`, `UsageView`,
`KnowledgeGraph`), each wrapped in `<Suspense>` with the existing
`route-loading.tsx` fallback (or a tile-shaped inline fallback, so the two
hero slots above don't jump layout while the chunk loads).

`vite.config.ts` has no manual Rollup chunk-splitting config
(`build.rollupOptions.output.manualChunks` is absent entirely) — every
existing split comes from these `lazy()` boundaries, not build config, so
`MascotAvatar` should follow the identical pattern rather than introduce a
new splitting mechanism. Concretely: `MascotAvatar` itself is the
`lazy()`-loaded module, pulling in `@rive-app/react-canvas` and the `.riv`
asset URL as its own dependencies — the two call sites above import
`MascotAvatar` lazily, not `@rive-app/react-canvas` directly.

## Accessibility

`prefers-reduced-motion` needs a real answer, not a skip: `MascotAvatar`
should freeze on the idle frame (or fall back to the static tile entirely)
when the media query matches, the same accessibility carve-out an animated
GIF avatar already has to consider per `docs/spec/runtime/avatars.md`
("GIFs are first-class... a moving one is more recognisable, not less" —
that argument assumes the viewer can tolerate motion, which reduced-motion
says they can't).
