# 05 — Registry trust, scanning and updates

**Goal.** Content an operator did not write is checked before it can reach an
agent, an installed skill carries its provenance, and an install that has fallen
behind the registry says so.

**Why now.** The registry-poisoning record is public and recent: Snyk's
"ToxicSkills" scan of 3,984 skills found 76 confirmed-malicious payloads (verified);
Silverfort showed ClawHub's download counter could be inflated by anyone to rank a
malicious skill first (verified); Hermes's own scanner has open bypass reports
([`02`](02-industry-comparison.md)). OpenCompany's install path is
server-authoritative and pins a snapshot (`server/ops/skills.rs:286-306`), which is
a strong start — and does no inspection of what it pins.

## 5.0 The trust pipeline at a glance

Every text entry point goes through one pipeline. The right-hand loop is the drift
check sending an operator's `update` back through the scan. Every box is proposed.

```text
 UNTRUSTED TEXT ENTRY POINTS
 ┌────────────────┐  ┌────────────────┐  ┌────────────────┐  ┌────────────────┐
 │ registry       │  │ description +  │  │ bundled files  │  │ custom /       │
 │ SKILL.md body  │  │ category       │  │ (resources)    │  │ upload         │
 └────────┬───────┘  └────────┬───────┘  └────────┬───────┘  └────────┬───────┘
          │                   │                   │                   │
          └───────────────────┴─────────┬─────────┴───────────────────┘
                                        ▼
            ┌───────────────────────────────────────────────────────┐
            │ [NEW] SCAN  (one function, every entry point)         │
            │ size caps · Unicode-tag / bidi / zero-width strip     │◄─────┐
            │ instruction-shaped text · exfil shapes · secrets      │      │
            │ symlink / nested-archive containment                  │      │
            └───────────────────────────┬───────────────────────────┘      │
                                        │                                  │
                                        ▼                                  │
            ┌───────────────────────────────────────────────────────┐      │
            │ VERDICT     block vs warn: OPEN DECISION (not picked) │      │
            │ block : nothing is written                            │      │
            │ warn  : proceeds, operator sees the findings          │      │
            │ pass  : proceeds                                      │      │
            └───────────────────────────┬───────────────────────────┘      │
                                        │                                  │
                                        ▼                                  │
            ┌───────────────────────────────────────────────────────┐      │
            │ SANITISE what reaches the prompt                      │      │
            │ quoted data · code points stripped · length caps      │      │
            └───────────────────────────┬───────────────────────────┘      │
                                        │                                  │
                                        ▼                                  │
            ┌───────────────────────────────────────────────────────┐      │
            │ TRUST TIER: builtin | company | registry | custom     │      │
            │ PIN: sha256 digest + version + installer + time       │      │
            └───────────────────────────┬───────────────────────────┘      │
                                        │                                  │
                                        ▼                                  │
            ┌───────────────────────────────────────────────────────┐      │
            │ INSTALL: SkillStateStore.set  +  audit event          │      │
            │ (digest and actor, never the body)                    │      │
            └───────────────────────────┬───────────────────────────┘      │
                                        │ later                            │
                                        ▼                                  │
            ┌───────────────────────────────────────────────────────┐      │
            │ DRIFT CHECK  (on GET …/skills, per registry install)  │      │
            │ pinned digest + version  vs  live registry entry      │      │
            │ locally edited copy  -> modified; update refuses      ├──────┘
            │ library changed      -> updateAvailable               │
            └───────────────────────────────────────────────────────┘
 right-hand loop: operator runs update -> the new document is scanned again
```

- Entry points map to the table in §5.1: `install` `server/ops/skills.rs:307`,
  `create_custom` `:439`, loaders `company/skill_file.rs:156` and `:199`.
- Prompt-bound surfaces: catalogue `harness/built_in/skills.rs:169-193` and the
  files `read_skill_resource` returns (`:148-161`).
- The block-versus-warn behaviour is an OPEN DECISION ([`08`](08-rollout.md)); this
  diagram deliberately does not pick.
- Drift compares against the stored `version` (`company/skill_file.rs:25`, "nothing
  compares or orders it yet") and the pinned digest.

## 5.1 Where content enters, and what is checked today

| Entry | Code | Checked today |
| --- | --- | --- |
| Registry install | `install` (`server/ops/skills.rs:307`) | the slug exists in the library; **no content inspection** |
| Custom create | `create_custom` (`:439`) | assembled size ≤256 KiB (`:55`); frontmatter newlines collapsed (`:473`) |
| Bundle / global | `load_catalog_skills`, `load_dir_skills` (`company/skill_file.rs:156`, `:199`) | parse only; trusted as repo content |
| Client-supplied fallback | `install` with an **empty registry** (`:286-306` case 3) | nothing — the client authors the content |

The last row is the sharpest edge. It exists so hosted tenants (no
`skills_root`, `docs/spec/runtime/globals.md:26`) can still install. It means that
on exactly those hosts an admin's request body *is* the skill. That is acceptable
for an admin, but it must go through the same scan as everything else, and it
must be labelled `custom`, not `registry`, in provenance. **Today it is not:**
the fallback persists `source: SkillSource::Registry` (`server/ops/skills.rs:347`),
the same value a real library install gets, so a client-authored document is
indistinguishable from a library one in `SkillStateStore`.

## 5.2 Scan on install **and** on create/upload

One function, run at every entry in the table above except repo-committed bundles:
`scan_skill(&SkillDoc, &[Resource]) -> ScanReport`. It is the single place the
rules live (the same reason [`03`](03-prerequisites.md) puts validation in one
function). Claude's own upload dialog does the equivalent — "a security scan runs
when you save" ([`02`](02-industry-comparison.md), screenshot) — and Hermes scans
every hub install.

**What it inspects — all of it, not just the body.** The Hermes issues are the
lesson: descriptions and `DESCRIPTION.md` bypass the scanner and land verbatim in
the system prompt (#8884); cron-loaded content is never scanned (#3968); a
pattern scanner is defeated by runtime string construction (#7072). For us the
prompt-bound surfaces are the catalogue line (`harness/built_in/skills.rs:169-193`,
which interpolates name and description) and everything `read_skill_resource`
can return. So scan:

1. `name`, `description`, `category`, `version` (catalogue text);
2. the body;
3. **every bundled resource** the materializer will write (`skills.rs:78`), since
   `read_skill_resource` returns them.

**Checks** (start static and cheap; do not pretend it is a sandbox):

- size caps per file and in total, in addition to `MAX_SKILL_DOC_BYTES`;
- **Unicode tag characters and other invisible/control code points stripped or
  rejected** (CSA recommendation, verified), including bidi overrides and zero-width
  characters;
- instruction-shaped text aimed at the agent in fields that should be descriptive:
  "ignore previous instructions", role or system-prompt impersonation, requests to
  reveal secrets;
- shell/exfiltration shapes: `curl … | sh`, base64-piped execution, reads of
  credential paths, requests to a hard-coded external host;
- references to MCP servers or tools the skill does not declare (Anthropic's
  enterprise risk table lists "MCP references");
- hard-coded credentials;
- a resource that is an executable, an archive within an archive, or a symlink
  (containment — OpenClaw verifies symlink containment).

**Verdicts:** `pass`, `warn`, `block`. Whether `warn` proceeds and whether
`block` is overridable are **open decisions** ([`08`](08-rollout.md)). Hermes lets
`--force` override non-dangerous findings only; that is a reasonable default.

**Honest limits.** A static scan is bypassable — Hermes #7072 is the proof. The
scan is a filter that raises the cost of the cheap attacks and produces an audit
record, not a guarantee. The doc for operators must say so ([`03`](03-prerequisites.md)
§3.2), and the actual containment remains the tool-call gate.

## 5.3 Sanitize what reaches the prompt

Independent of the verdict, the catalogue must not be able to carry an instruction.
Render name and description as **quoted data** inside a fixed template, strip the
code points above, cap each field's length, and escape angle brackets and fence
markers (Anthropic sanitizes synced skills the same way: control characters
removed, angle brackets escaped — verified). Keep the existing newline collapse
(`skills.rs:473`). This closes Hermes #8884's shape structurally rather than by
detection.

## 5.4 Trust tiers and provenance

`SkillSource` is `Company | Registry | Custom` (`ports/skills_state.rs:19`).
Grow it into a visible trust label, computed, not stored separately:

| Tier | Meaning | Scan |
| --- | --- | --- |
| `builtin` | embedded global baseline (`companies/_globals/skills`) | CI |
| `company` | committed in the company's bundle | code review |
| `registry` | installed from the shared library, snapshot pinned | on install |
| `custom` | authored or uploaded in the console (includes the empty-registry fallback) | on save |

Show the tier on every row and detail panel ("from the shared registry · pinned
v1.2"), as Claude shows "from Anthropic". Record, per install: source, version,
**content digest** (SHA-256 of the pinned document), install time and installer.
That is Hermes's `.hub/lock.json` and OpenClaw's `.clawhub/lock.json`, and the
digest is what makes "unchanged" checkable.

## 5.5 Pinning and drift

Installs are already pinned: the snapshot is stored, and "a later library edit
does not rewrite an existing install" (`skills.rs:286-306`). What is missing is
the **signal**. `SkillDoc::version` is stored and "nothing compares or orders it
yet" (`company/skill_file.rs:25`); the comment says it exists for exactly this.

- On `GET …/skills`, for each `Registry` install, compare the pinned digest and
  version to the live library entry and return `updateAvailable: {from, to}`.
- **Never auto-update.** Claude's API defaults an omitted version to "latest",
  which lets a workspace member change production behaviour silently (verified);
  Claude Code marketplaces auto-update by default. Our default is the opposite:
  pinned until an admin acts.
- An explicit `POST …/skills/{slug}/update` re-runs the scan on the new document
  and shows a diff before applying.
- **Skip locally edited installs** (Hermes's `update` does): an install whose
  document no longer matches its recorded digest is `modified`, and update refuses
  rather than overwriting.
- Without a comparable `version` (it is free text), fall back to the digest and say
  "changed", not "newer". Do not invent an ordering.

## 5.6 Audit

Journal an event for install, update, uninstall, scope change, and every scan
verdict, carrying slug, tier, digest and actor but **not the document body** — the
same no-body rule `WorkflowUpdated` and `DeskHiveConfigured` follow
(`docs/spec/runtime/events.md`). The CSA note recommends an audit log of skill
changes (verified); the journal is already durable and exported. Operators then
have an answer to "when did this skill change, and who changed it?".

## 5.7 What not to build

- **No client-writable popularity.** Install counts in a Discover view are a social
  proof signal and the ClawHub incident is precisely that signal being spoofed.
  If a count is ever shown it must be computed server-side from the installs the
  host itself recorded, per host — and whether to show one at all is an open
  decision. Claude shows install counts on its Discover cards; that is a
  centrally-run marketplace, not a self-hosted registry.
- **No third-party registry by default.** The registry stays the host's shared
  library (`server/ops/skills.rs:364`). An OpenClaw-style `installPolicy` hook —
  run a trusted command before an install — is a reasonable optional extension for
  operators who want their own gate; do not build it in the first slice.
- **No signing yet.** Signing is on the CSA list; it needs a publisher and key
  story the shared library does not have. Record it as future work.

## 5.8 Tests

- Scan: each check has a fixture that trips it and one benign near-miss; a skill
  with a poisoned **description only** is caught (the Hermes #8884 shape); a
  poisoned **bundled resource only** is caught; invisible code points are stripped.
- Catalogue rendering: a description containing `\n\nSystem:` renders as one quoted
  line.
- Install: a registry install records digest and tier; the empty-registry fallback
  records `custom`; a broken configured library still returns `500`
  (`skills.rs:286-306`, existing behaviour preserved).
- Drift: changing the library entry flips `updateAvailable`; editing an installed
  custom copy flips it to `modified` and update refuses.
- Audit: the journal row exists and contains no body.
