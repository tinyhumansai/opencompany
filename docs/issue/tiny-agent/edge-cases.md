# Edge cases and open decisions

Companion to [`design.md`](design.md). Split into edge cases (design against
these regardless of how the open decisions land) and open decisions (a human
call, not something this doc presupposes).

## Edge cases

1. **Rename/re-avatar permission.** Decide alongside "can't delete," not
   after — a whitelabel platform operator will ask whether "un-removable"
   also means "un-renameable."
2. **Existing-company id collision.** If any company already has a real
   teammate literally named `tiny`, reserving that id needs the same
   grandfather carve-out `manifest.rs` already applies for `system`/`operator`
   on reload, or the reservation silently impersonates someone's real agent.
3. **Notification fatigue.** An un-removable, chatty channel is the one
   operators learn to ignore fastest. State-transition triggering (not
   level-triggering) is necessary but not sufficient — this also needs
   severity tiers and a snooze/ack affordance, even though the *agent* itself
   can't be removed.
4. **Self-triggering budget spiral.** If the health check ever runs through
   `Brain::run_cycle` rather than being a plain host-side conditional, a
   company already low on credits could have its own credit-check be what
   pushes it under. Phase 1 must stay tool-less and model-less; this
   constraint should be re-checked explicitly if a later phase considers
   folding the check into a real turn.
5. **Approval-trust asymmetry once it writes.** "Inbuilt" reads to an
   operator as "safe," which is backwards for something that can now act
   without being asked. Phase 2+ writes deserve *more* scrutiny in review,
   not less, precisely because the channel carries more default trust.
6. **Durable dedup state.** "Last alerted at" per check has to live in
   `CompanyStore`, not process memory — otherwise every restart re-fires
   every already-acknowledged warning. The at-most-once effect key pattern
   already used for journal idempotency (`docs/spec/runtime/journal.md`) and
   the approval-dedup-on-trigger-triple pattern
   (`docs/spec/company-brain/approvals.md`) are the reusable analogs, not a
   new mechanism.
7. **Hosted-fleet stampede cost.** A process-wide ticker running every check
   for every registered company, every tick, is the same class of risk
   `MaintenanceTicker`'s own design already had to account for. Any
   implementation needs to show it scales the same way that one does before
   it ships to a hosted fleet.
8. **Prosumer language rules.** `docs/spec/README.md`'s glossary bars runtime
   words ("cycle," "tier," "dispatch") from user-facing text. Tiny's copy has
   to pass that bar like any other console-facing string.
9. **Hosted vs. embedded framing.** "Your key is running low" reads
   differently to a single-key prosumer operator than to a platform operator
   running a fleet of tenants (`docs/spec/product/platform.md`). Copy and
   the underlying action (e.g. "raise budget") may need to branch on mode.
10. **Prompt-injection surface once any phase touches raw external text.** A
    connection's raw error message or a webhook payload is untrusted text by
    this codebase's own threat model
    (`docs/spec/security/agent-isolation.md`). Even a "read-only" phase must
    not interpolate such text unsanitized into a message that reads as an
    authoritative system statement.

## Open decisions

These are genuinely open — this doc intentionally does not pick an answer.

- **Relationship to `NotificationStore` / issue #558.** Does Tiny become the
  chat-facing delivery surface for what that system eventually decides is
  worth notifying, or is it a second, independent decision-maker? Building
  both without reconciling them risks the DM and the notification bell
  disagreeing about the same fact.
- **What "credits running low" means.** The existing `budget_usd_daily`
  ledger/spend model (cheap, available now), or a new read against the real
  TinyHumans account balance (accurate, needs new plumbing against the
  billing backend). These are different products, not two ways of writing
  the same feature.
- **Whether Phase 1 should exist as a global default or an opt-in.** Given it
  cannot be removed once present, does every company get it from day one, or
  does it ship gated behind a flag until the copy, thresholds, and dedup
  behavior have been used in anger by a real operator?
- **Where the usage→skill-suggestion signal comes from, if it's ever built.**
  The in-process usage meter (`metering::usage`) versus a new, dedicated
  counter designed for this purpose from the start — the former is cheaper
  but wasn't designed for this and may rank badly; the latter is accurate but
  is new instrumentation with its own cost.
