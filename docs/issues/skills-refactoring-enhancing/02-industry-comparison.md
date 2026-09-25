# 02 — How other agents handle skills

Researched 2026-09-21 from primary sources where reachable, not from a model's
memory (its training data ends in January 2026, well before these products'
current state). Every claim carries a tag:

- **verified** — read on the cited primary page or source file;
- **inferred** — deduced from context or a secondary source;
- **snippet-only** — seen only in a search-result summary, page not opened;
- **unconfirmed** — reported somewhere but not established.

Where two research passes covered the same project, the weaker tag wins.

## What the industry converged on

Verified (agentskills.io, `/home` and `/specification`): the **Agent Skills**
format — a directory with a required `SKILL.md`, frontmatter with required `name`
(≤64 chars, lowercase letters/digits/hyphens, must match the directory) and
`description` (≤1024 chars), optional `license`, `compatibility` (≤500),
`metadata`, and an experimental `allowed-tools`; optional `scripts/`,
`references/`, `assets/`; three-stage progressive disclosure (metadata always
loaded, body on activation, resources on demand). A validator, `skills-ref
validate`, ships with the spec. Claude, Codex, Gemini CLI, Cursor, Goose,
OpenHands, OpenCode, Hermes and OpenClaw all claim the format; the count was not
independently tallied (verified as a claim on the standard's page, not counted).
The `.agents/skills/` path convention is shared by Codex, Gemini CLI and Cursor
(verified per product docs).

**OpenCompany matches the format** and is stricter than everyone on execution
(none). It is looser than everyone on scoping, drift and consent.

## Comparison

| | Format | Loading | Scoping | Scripts/execution | Distribution | Consent / trust |
|---|---|---|---|---|---|---|
| **Claude Code / API** | SKILL.md + Claude-specific keys | metadata always, body on trigger | enterprise > personal > project; plugin-namespaced; SDK per-session allowlist | scripts via bash; `allowed-tools` pre-approves one turn | plugin marketplaces; sha/version/digest pins | permission rules `Skill(name)`; enterprise review checklist |
| **Codex CLI** | SKILL.md + `agents/openai.yaml` | explicit `$skill` or implicit | cwd, repo, user, admin, built-in | scripts supported; sandbox link **unstated** | plugins | per-skill `allow_implicit_invocation` |
| **Gemini CLI** | SKILL.md | model calls `activate_skill` | built-in, extension, user, workspace | bundled code (inferred) | `gemini skills install <git-url>` | **user confirmation on activate** |
| **Cursor** | SKILL.md + `paths`, `disable-model-invocation` | auto or `/name` | project, user, nested dir, `paths` glob | agents run `scripts/`; approval **unstated** | plugin marketplace | admin can disable cloud sync |
| **OpenClaw** | SKILL.md + `metadata.openclaw` | XML entry per skill in prompt | 7 tiers; **per-agent allowlists** | scripts gated on bins/env/os | ClawHub, semver, lockfile | scans, reports, `installPolicy` |
| **Hermes** | SKILL.md + `platforms`, toolset conditions | 3-level progressive disclosure | project > user > external > bundled; no per-agent allowlist found | sandboxed scripts | hub with trust tiers | install scan; `skills.write_approval` |
| **OpenHands** | agentskills.io | keyword/path triggers | repo, project, user, public | "skills cannot grant permissions" | review-before-install warning | instructions only |
| **Goose** | agentskills.io | — | global, project, plugin | — | — | permission gating **not described** |
| **OpenCompany** | SKILL.md | catalogue always in prompt, read tools | **company-wide only** | **none (read-only)** | admin install from shared registry | admin-only writes; no scan |

## Claude (Anthropic)

Sources: code.claude.com/docs/en/skills, platform.claude.com/docs/en/agents-and-tools/agent-skills/overview and `/enterprise`, code.claude.com/docs/en/plugin-marketplaces and `/agent-sdk/skills`, agentskills.io, github.com/anthropics/skills. All fetched 2026-09-21.

- **verified** Loading: about 100 tokens of metadata per skill always in the prompt; body (<5k tokens) on trigger; resources on demand; script code never enters context, only its output. Claude Code re-attaches each invoked skill's first 5,000 tokens after compaction within a shared 25,000-token budget. API requests accept at most 20 skills.
- **verified** Scoping: enterprise > personal > project; plugin skills namespaced `plugin:skill`. A skill cannot be scoped to a subagent from the skill side; a subagent's `skills` field preloads instead. The Agent SDK's `skills` option is a per-session allowlist (names, `"all"`, or `[]`); unlisted skills are hidden from the model **but their files stay readable through Read and Bash**.
- **verified** Execution: scripts run through bash and are rated "High" risk in the docs. `allowed-tools` pre-approves for one turn only, then clears; it does not restrict other tools; deny and ask rules override it. Permission rules can target skills: `Skill(name)`, `Skill(name *)`. Workspace trust does **not** gate a project skill's `allowed-tools`.
- **verified** Distribution: marketplaces via `marketplace.json`; plugins pinnable by git `ref`/`sha`, `version`, or SHA-256 archive digest (a mismatch fails the install); auto-update on by default. Enterprise: `strictKnownMarketplaces`, `blockedMarketplaces`. The `/v1/skills` API treats an omitted `version` as "latest", so a workspace member can change production behaviour silently.
- **verified** Security guidance: use trusted sources only; the enterprise page gives a risk-tier table (scripts, instruction manipulation, MCP references, network patterns, hardcoded credentials, filesystem scope), an 8-step review checklist and author/reviewer separation. Enterprise content scanning covers claude.ai and Cowork uploads, **not** Skills API uploads or pre-existing skills. Synced skills are sanitized (control characters removed, angle brackets escaped, `!` shell injection disabled).
- **snippet-only** Academic work on skill prompt injection (arXiv 2510.26328, "SkillJect", "MalSkillBench").
- **verified** Management UI in Claude Code: a `/skills` menu grouped by source; a `skillOverrides` setting with `on`, `name-only`, `user-invocable-only`, `off`; `/skill-doctor` reports per-skill context cost and never-invoked skills.

### The Claude Settings › Skills flow (from operator screenshots, 2026-09-21)

Five screenshots of claude.ai/desktop Settings › Customize › Skills:

1. **Your skills** tab — grouped by source ("From Anthropic & Partners · 22"); each row shows icon, name, a category badge (Productivity, Enterprise Search) or "New", "from Anthropic · description", last-edited date and a ⋮ menu; search ("skills and plugins"), Filter, Sort (default "Last edited").
2. **Discover** tab — a featured banner (e.g. "Data") with an Add button, and a "For you" grid of cards with install counts ("8.3M installs") and a `+` button.
3. **Add** menu — Upload skill, Create a skill, Create with Claude, Record your screen, Watch the intro video.
4. **Upload skill** — "Add a skill to your workspace. A security scan runs when you save." Drag-and-drop, several files at once; `.md` must carry name and description in YAML, `.zip`/`.skill` must contain `SKILL.md`; a "Security scan — runs when you save" row; Save disabled until a file is chosen.
5. **Create a skill** — name (placeholder `weekly-status-report`), description (placeholder "Generate weekly status reports from recent work. Use when asked for updates or progress summaries."), a line-numbered instruction editor, "+ Add file", a "Draft" badge, Cancel/Create.

**What the screenshots do not show:** what clicking a row opens (detail view,
per-skill toggle, delete, version); whether Discover cards are single skills or
plugin bundles (the search box says "skills and plugins", so bundles is an
inference); and the scope — this dialog says "your workspace" while the docs
research found claude.ai skills are per-user only. That may depend on the plan;
it was not resolved. None of the five screens offers per-agent scoping, which
lives in the SDK and Claude Code, so it need not be in a first UI slice.

## Codex CLI

Source: learn.chatgpt.com/docs/build-skills (redirect from developers.openai.com/codex/skills).

- **verified** A directory with `SKILL.md` (`name`, `description`), optional `scripts/`, `references/`, `assets/`, and `agents/openai.yaml` for UI metadata and policy.
- **verified** Explicit (`$skill`) or implicit invocation; `policy.allow_implicit_invocation: false` disables implicit matching.
- **verified** Scanned in order: `$CWD/.agents/skills`, `$REPO_ROOT/.agents/skills`, `$HOME/.agents/skills`, `/etc/codex/skills` (admin), built-ins. `~/.codex/config.toml` can disable a skill without deleting it.
- **unconfirmed** How scripts interact with the sandbox and approval model — the fetched page did not say. No first-party security notes were on the page. Versioning is not documented there.

## Gemini CLI

Source: geminicli.com/docs/cli/skills.

- **verified** Name and description of every enabled skill are injected into the system prompt; on a match the model calls `activate_skill`, and the **user then sees a confirmation prompt** naming the skill and the directory it gains access to; only after approval is the body injected. This is the strongest consent model of the three.
- **verified** Precedence, low to high: built-in, extension, user (`~/.gemini/skills/` or `~/.agents/skills/`), workspace (`.gemini/skills/` or `.agents/skills/`); `.agents/` beats `.gemini/` within a tier.
- **verified** `gemini skills install <git-url> --consent`, `uninstall --scope`, `/skills link|enable|disable`.
- **snippet-only** Frontmatter fields (`name`, `description`) and that bundled code is gated by the activation prompt plus normal tool approval.

## Cursor, Windsurf, Cline, Roo, Continue

Source for Cursor: cursor.com/docs/skills.

- **verified** (Cursor) `SKILL.md` with `name`, `description`, optional `paths` (glob restriction), `disable-model-invocation`, `icon`, `color`, `metadata`. Auto-discovered or `/skill-name`; nested directories such as `apps/web/.cursor/skills/` scope a skill to that subtree. Skills ship in plugins via `.cursor-plugin/marketplace.json` and team marketplaces; admins can disable cloud sync org-wide. Agents run `scripts/` when invoked; sandbox and approval behaviour is **unconfirmed**.
- **snippet-only** (third-party posts) Windsurf reads `.windsurf/skills/`, Cline `.cline/skills/`; Roo has mode-scoped rule folders; Continue's documented paths are for `rules/`, not skills.
- **unconfirmed** One blog reports that Roo Code shut down on 2026-05-15. No primary source was reached (the Roo docs fetch hit a redirect that was not followed). Do not rely on it.

## OpenClaw

Sources: docs.openclaw.ai/tools/skills, `/clawhub/skill-format`, `/clawhub`.

- **verified** Portable name rule (1–64 lowercase letters, digits, hyphens; matches the directory); runtime requirements under `metadata.openclaw`.
- **verified** Each eligible skill adds an XML entry (~24 tokens) to the system prompt; over `skills.limits.maxSkillsPromptChars` it drops descriptions before names. Snapshot at session start; a 250 ms-debounced watcher refreshes on change.
- **verified** Seven-level precedence, workspace first. **Per-agent allowlists** (`agents.entries.<id>.skills`) are final: a non-empty list does not merge with defaults.
- **verified** Skills bundle scripts via `{baseDir}`; gating is `requires.bins`, `anyBins`, `env`, `config`, plus an `os` filter; unmet means silently ineligible.
- **verified** Registry: semver plus `latest` tags; `openclaw skills update` covers ClawHub installs; the `clawhub` CLI records versions in `.clawhub/lock.json`; publishing needs a GitHub account past an age threshold; scans run on releases; users can report and moderators act; `security.installPolicy` can run a trusted policy command before installs. No hash pinning or signing is named on the overview page.
- **unconfirmed** Agent-authored skills: the loading order has a per-agent "workshop-skills" directory, but what writes it was not read.

## Hermes Agent (Nous Research)

Source: hermes-agent.nousresearch.com/docs/user-guide/features/skills.

- **verified** SKILL.md plus `name`, `description` (≤60 chars), `version`, optional `platforms`, `metadata.hermes.*`; claims agentskills.io compatibility. Three-level progressive disclosure (`skills_list()`, `skill_view(name)`, `skill_view(name, path)`).
- **verified** Precedence project-local > `~/.hermes/skills` > external dirs > bundled; conditional visibility via `requires_toolsets` / `fallback_for_toolsets`. **No per-agent allowlist found** (absence not proven).
- **verified** Hub installs carry trust tiers (builtin, trusted, community) and every install is scanned for exfiltration, injection, destructive commands and supply-chain signals; state in `.hub/lock.json`, `quarantine/`, `audit.log`; `hermes skills check` reports upstream drift and `update` skips locally edited skills.
- **verified** The `skill_manage` tool lets the agent create, patch and delete skills; `skills.write_approval: true` stages writes for `approve|reject`. **unconfirmed:** the flag's default.
- **verified (as open issues, not opened)** #8884 descriptions and `DESCRIPTION.md` bypass injection scanning and go verbatim into the system prompt; #7072 the scanner is bypassable with dynamic imports and runtime string construction; #3968 skill content loaded by cron jobs is never scanned. **snippet-only:** a CSA note titled "9 CVEs in 4 days" for Hermes.

## OpenHands and Goose

Sources: docs.openhands.dev/overview/skills; goose-docs.ai/docs/guides/context-engineering/using-skills/.

- **verified** OpenHands: repo, project, user, public scopes; keyword- and path-triggered skills; "skills cannot grant permissions" — instructions only; warns to review before installing.
- **verified** Goose: global, project, plugin scopes; skills are separate from recipes. **unconfirmed:** permission gating (the page does not describe it).
- No primary source was found for Aider, LangGraph, CrewAI or AutoGen skill systems. Nothing is claimed about them.

## Security research

- **verified** Snyk "ToxicSkills" (2026-02-05): 3,984 skills scanned from ClawHub and skills.sh; 13.4% had a critical issue and 36.82% some flaw; 76 confirmed malicious payloads; 91% of the confirmed-malicious also used prompt injection. snyk.io/blog/toxicskills-malicious-ai-agent-skills-clawhub/
- **verified** Silverfort (disclosed 2026-03-16): an unauthenticated, unrate-limited `increment` mutation let anyone inflate ClawHub download counts and put a malicious skill at #1; fixed in under 24 hours.
- **snippet-only** Koi Security ("ClawHavoc"): 341 malicious skills of 2,857 audited, 335 using fake prerequisites to install Atomic Stealer. Palo Alto Unit 42: five malicious skills.
- **verified** Cloud Security Alliance research note (2026-05-06) recommends content-hash verification, approved registries only, Unicode-tag stripping, signing, least-privilege egress and audit logs of skill changes. labs.cloudsecurityalliance.org/research/csa-research-note-skill-md-agent-context-poisoning-20260506/
- **verified** arXiv 2605.11418: semantic attacks on registries work across Claude Code, Codex and OpenClaw; no numbers beyond "high success rates" were captured.
- **inferred** The mechanism common to all of these: a malicious skill inherits the agent's permissions.

## What to copy, what to avoid

Copy: OpenClaw-style **per-agent allowlists where a non-empty list is final**
(`04`); Hermes's **drift check that skips local edits** and a lockfile recording
installed source and version (`05`); trust tiers applied to registry content
(`05`); Anthropic's risk-tier table and the CSA recommendations as the missing
threat-model section (`03`); the spec's `name` rules and a validator (`03`); a
`/skill-doctor`-style report of context cost and never-used skills, using the
`version` already stored (`06`); Gemini's **consent on activation** as an option
if execution ever ships (`07`).

Avoid: scanning bodies only — descriptions and catalogue text reach the prompt
too; ranking or displaying **client-writable popularity counters**; defaulting to
"latest"; silent non-loading when requirements are unmet without telling the
operator; per-user-only skills with no admin control; and wiring execution before
its metering and egress seam exists (`07`).
