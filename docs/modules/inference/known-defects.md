# Known defects in the design we are borrowing from

openhuman's provider surface is the model for this work. It is also a real
codebase with real bugs, and copying it faithfully means copying those too. Each
item below was found by reading openhuman at `5e543a76b`; each has a decision
attached, and the fix is part of the design rather than a follow-up.

## 1. A row carries no health signal

**There.** The connected-provider row renders brand mark, label, one derived
detail line, an on/off switch, and an overflow menu. There is no status field.
A provider whose key was revoked an hour ago looks identical to a working one;
the only signal is a banner above the list, rendered from a process-global
registry of 401/403s.

There is also no periodic check of any kind — verified, not merely unfound. The
only probes are at add-time, a manual button inside the routing dialog, and a
flows-only readiness check.

**Here.** A row shows its last known state, and the row is where it shows.

```
⬤  OpenRouter        •••• configured     ✓ ok            ▣ on   ⋯
⬤  Acme gateway      api.acme.dev/v1     ⚠ key rejected  ▣ on   ⋯
⬤  Ollama (local)    127.0.0.1:11434     ⚠ unreachable   ▣ on   ⋯
```

Sourced from what already happens rather than from a new poller:

- the add-time probe result
- the manual Test
- **the turn path** — a 401 already invalidates the credential cache; record the
  provider, the status and the time alongside it

Do not add a background poller. It costs a request per provider per interval
across every company on the host, to learn something the next real turn learns
for free. Record what the system already discovers.

Two rules carried over because they are right: latch the notice **once per
failure episode**, not once per retry — the repeating background loop that
produced ~9k events for 6 users is the reason — and **never cache a credential
failure against an endpoint**, because a 401 is an answer about the key that was
presented, not about the endpoint.

## 2. Removing a provider orphans its key

**There.** The switch removes the config entry and leaves `provider:<slug>` in
the auth store. Re-adding the provider silently reuses the old key. Undocumented,
and it reads as unintentional rather than deliberate.

**Here.** Deleting a provider clears its credential in the same operation. If the
clear fails, log it loudly — a silently failed key-clear orphans a secret on
disk, which is the shape of an incident, not a tidiness problem.

Note the local constraint: this store has **no delete**, and clearing is a write
of the empty string that reads back as unset. That is fine, but it means "cleared"
and "never set" are the same state — so the clear must actually be issued rather
than inferred.

## 3. There is no disabled state

**There.** The switch is binary: connected or removed. Turning a provider off
deletes it, losing its endpoint and scrubbing its routes. A temporary disable —
"stop billing this account this week" — is not expressible.

**Here.** `enabled: bool` on the record, distinct from deletion. A disabled
provider keeps its endpoint, its label and its credential, and is not a routing
target. Routes pointing at it behave as they do for a deleted one — reported,
not silently demoted.

## 4. The preset catalogue is duplicated by hand across the language boundary

**There.** 27 providers in Rust and the same list again in TypeScript with extra
presentation fields. Nothing generates one from the other and nothing tests that
they agree. A provider added to one and not the other half-works.

**Here, and worse already.** OpenCompany has *six* copies of provider and tier
knowledge, and two have already drifted: the OpenRouter attribution headers exist
in both `provider.rs` and `roster_build.rs` with different values, and the wire
provider union in `api/inference.ts` still carries `"managed"` after the host's
list dropped it.

The rework collapses these to one source with a generated or asserted mirror. If
generation is too heavy, a test that fails when the lists diverge is the minimum
— the current state is that nothing notices.

## 5. `default_model` is deprecated on write and load-bearing on read

**There.** The field is marked `skip_serializing` and documented as "never
emitted", yet the resolver reads it as the empty-model fallback *and* as the
abstract-tier remap target — and an error message tells the operator to *set* it.
The schema and the runtime disagree about whether the field exists.

**Here.** Do not introduce a field in that state. If a per-provider default model
is needed, it is a real field with a real write path. If it is not needed, it
does not exist.

## 6. A verified-but-unwired verifier

**There.** `verifyCloudProviderConnection` is carefully built, thoroughly
documented, never throws — and has no production caller. Meanwhile the add flow
uses a cheaper `/models` probe and the routing dialog calls a third path. Three
verification paths, one dormant.

**Here.** One probe, one caller, one classifier. If a second kind of check is
genuinely needed — a real completion versus a catalog listing — it is a
parameter, not a parallel function.

## 7. The page is named for something it no longer is

**There.** The Connections page lives in `pages/Skills.tsx`, with `/skills`
redirecting to `/connections`. Renamed concept, unrenamed file.

**Here.** Not a bug to inherit, but a warning: this rework renames "Inference" to
"LLM" on the rail. Rename the *label* and leave route ids alone — that is the
established rule in `connection-pages.ts`, and it is what keeps bookmarks working.

## 8. Mechanical file splits that defeat the module system

**There.** The routing factory is ~2,600 lines split into four `include!`d
"parts", as are the credential and middleware modules. These are file-size-lint
satisfaction, not design factoring: they break `cargo`'s module boundaries and
make line numbers meaningless across the logical unit.

**Here.** If a module grows past the point of comfort, split it on a
responsibility seam — resolver, storage, probe, routing — with real modules. Do
not `include!` halves of one file.

## What we are deliberately keeping, bugs and all

Not everything odd is wrong:

- **A catalog that publishes neither vocabulary yields no defaults.** Both
  possible answers are wrong at such an endpoint, but passing the tier name
  through is the *honest* one: the provider's 400 then names a string the
  operator configured and can find, instead of an id they never typed.
- **The legacy `managed` alias resolves rather than failing.** A stored runtime
  blob is data an operator cannot hand-edit, so hard-failing on a value the
  console itself once wrote would strand them.
- **An unknown provider in a stored blob fails loudly.** The opposite of the
  above, and correctly so: resolving one silently would attribute its spend to
  whatever the fallback happened to be.
