# Driving the animation: idle, hover, replying

## Colors: v1 ships one fixed colorway

`handColor`/`skinColor` are the mascot's literal skin and hand color, not an
abstract UI accent — they're a different kind of thing from `TEAM_TONES`
(`frontend/src/lib/team.ts`), which tints a tile's background/initials.
Reusing a teammate's hashed `tone` hue directly as their mascot's *skin
color* risks looking wrong in a way no amount of engineering analysis can
settle — e.g. a `rose`-toned teammate getting a pink-skinned mascot — and
that's a call for whoever actually looks at it rendered, not something to
bake into v1 code sight unseen.

**v1 ships the `.riv` file's own defaults** (`handColor=#B4900B`,
`skinColor=#F7D145`) for every teammate that picks `mascot:animated`. No
palette-derivation logic, no new mapping table. Per-tone (or per-teammate)
color variation is a real, clearly-scoped v1.5+ follow-up once there's
something to look at.

## Hover: no new plumbing needed

Trivial everywhere it applies — a plain `onMouseEnter`/`onMouseLeave` on the
wrapping element, setting `MascotAvatar`'s `state` prop to `"hover"`. Applies
identically at both v1 call sites (the profile-sheet header, the
avatar-picker preview/tile).

## "Replying": real, but scoped down for v1

The only existing signal that honestly means "this agent is producing output
right now" is `status === "running"` on a `TurnStep`/`LiveRow`
(`frontend/src/lib/live-frame.ts`), set by `foldLiveFrame` when a
`tool_call` frame arrives and resolved to `ok`/`error`/`awaiting_approval`
on the matching `tool_result`. `app-shell.tsx` owns the fold (`foldLiveFrame`
called at its live-event stream handler) and the resulting state
(`liveStepsByThread`/`liveStepsByMessage`, referenced throughout that file),
passing `TurnStep[]` down as props. Two leaf components branch on
`status === "running"` today: `WorkingIndicator.tsx` (`runningStepLabel`
picks the last running step's label for the "X is working…" text) and
`StepTimeline.tsx` (pulses the step's icon via `animate-pulse`).

**The agent profile sheet has no subscription to any of this.** It's a
static summary panel (`agentProfile()` in `frontend/src/lib/agent-profile.ts`
builds a plain snapshot from an `AgentDetailDto`) with no live-turn state
flowing into it at all. Wiring a "replying" mascot state into the
profile-sheet hero would mean either lifting `liveStepsByThread`-shaped
state up to somewhere the sheet can reach it, or standing up a new
per-agent "is a turn open right now" subscription — real work, not a
one-line addition, and out of scope for v1.

**`ChatLiveReceipt.tsx` and `MessageTimeline.tsx`'s "still working" row are
both single-instance hero surfaces that already have this signal for
free** — `ChatLiveReceipt.tsx` already imports `runningStepLabel` from
`WorkingIndicator` and distinguishes `queued` from running state for its own
status dot. If "replying" ships at all in v1, it ships as the `MascotAvatar`
in one of *these* surfaces, not the profile sheet — same component, wired
into a place that already has the data, rather than new plumbing invented
to feed the profile sheet a signal it was never designed to carry.

**Recommendation: treat "replying" as out of scope for the first landed
version**, and revisit once idle/hover in the profile sheet and picker are
shipped and reviewed. It's a real, well-understood follow-up, not an
unsolved problem — just a separable one.

## Ruled out: presence

`frontend/src/hooks/use-presence.ts` is confirmed human-viewer-only —
keyed by `userId`, tracking who's looking at the console right now via
document-input idleness, with no concept of an agent or a running turn. Not
a candidate signal source for any mascot state.

## The state budget, mapped to the Number-input slots

Idle is the file's own default (`mascotAnimationNumber = 1`, confirmed from
the editor and, later, live in the runtime — it renders the mascot's cap).
The "4 slots, `glass1`-`glass4`" framing this section originally used was a
static-analysis guess later found wrong: the artboard actually has nine
costume animations (`open-questions.md` §1), of which `glass1`-`glass4` are
only one family. **`mascotAnimationNumber = 2` is confirmed live to render
headphones, not a `glass2` variant** — `STATE_NUMBERS` in
`mascot-avatar.tsx` maps `hover` to `2` on that confirmation, not a guess.
`3` (`replying`) is wired but its visual is unconfirmed; slots beyond that
are unused by v1. See `open-questions.md` §1 and §4 for what's actually
been watched play versus what's still assumed.
