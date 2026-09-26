# Roadblocks and risks

Found by reading the code on `upstream/main` at `cb81b500c`, 2026-09-25. Each
entry says what was **verified** (and how) separately from what is
**inferred**. Ordered by how much it would cost to discover late.

---

## R1. `next-themes`' anti-flash script is inert in this SPA — dark mode already flashes

**Verified.** `next-themes@0.4.6` prevents a wrong-mode flash by rendering an
inline `<script>` via `dangerouslySetInnerHTML`
(`frontend/node_modules/next-themes/dist/index.mjs`, the memoised component
`_`). That only runs when the HTML is server-rendered. The console mounts with
`createRoot(root).render(...)` (`frontend/src/main.tsx:28-65`), and React
creates client-rendered `<script>` elements inert on purpose. The class is
therefore applied by the provider's `useEffect`, after the first commit.

The codebase already knows: `frontend/src/components/crash-fallback.tsx:21-27`
("`next-themes` stamps `class="dark"` on `<html>` from an **effect**") and
`frontend/index.html:107-111` ("for them the handover can still flash").

**Consequence for this feature:** copying the `next-themes` pattern would
produce a component that looks correct and does nothing. Use the synchronous
pre-mount call in `architecture.md` §6.

**Adjacent opportunity (not in scope unless the operator says so):** the same
pre-mount function could apply `.dark` from the `"theme"` key, fixing today's
flash for operators whose explicit choice differs from their OS. `next-themes`
would then re-apply the same class — harmless. See `open-questions.md` Q8.

## R2. Specificity trap: a preset's semantic override leaks into the other mode

**Verified by the cascade rules against `index.css`.** `.dark` is a single
class, specificity (0,1,0) (`index.css:445`). A preset selector
`[data-accent-preset="x"]` is also (0,1,0) and, sitting later in the file, wins
the tie on source order — so if a preset block sets `--primary-foreground`, that
value applies **in dark mode too**, overriding `.dark`'s
`--primary-foreground: var(--surface-dark-bg)` (`index.css:468`).

**Rule:** the unqualified preset block may set only the ten `--brand-N`
primitives, which are theme-independent by design (`docs/design-system/color.md:46-48`).
Any semantic override goes in a `:not(.dark)` / `.dark` pair
(`architecture.md` §2). A unit test enforces it (`test-plan.md` U3).

## R3. Unhashed static files are served `immutable` for a year

**Verified.** `cache_console_response`
(`crates/opencompany-core/src/server/routes.rs:78-93`) gives every non-HTML
console response `public, max-age=31536000, immutable`. That is right for
Vite's content-hashed `/assets/*`, and wrong for anything copied unhashed from
`frontend/public/`. A boot script placed there would be frozen in every browser
that ever loaded it.

**Consequence:** rules out a `public/accent-boot.js`. The chosen design puts
the boot code in the hashed bundle.

**Adjacent finding, pre-existing (inferred impact, not reproduced):**
`frontend/public/openpanel-init.js` is exactly such a file — unhashed, loaded
by `index.html`, and served `immutable`. A change to it would not reach
returning browsers for up to a year. Worth its own issue; not this feature's.

## R4. A swatch that uses `bg-primary` will show the page's preset, not its own

**Verified by the CSS model plus a built stylesheet.** A custom property that
references another (`--primary: var(--brand-500)`) is resolved on the element
where it is **declared** — `<html>` — and descendants inherit the *computed*
result. Setting `data-accent-preset="lagoon"` on a swatch `<span>` re-declares
`--brand-500` there, but `--primary` inside it is still the value `<html>`
computed. `.bg-primary` emits `var(--primary)` and so paints the page's preset.

`.bg-brand-500` emits `var(--brand-500)` directly (verified:
`.bg-brand-500{background-color:var(--brand-500)}` in a built
`frontend/dist/assets/index-*.css`), so it resolves on the swatch and is
correct.

**Rule:** swatches paint with `bg-brand-500 dark:bg-brand-400`, never with a
semantic utility. The same reasoning is why `data-accent-preset` for the whole
app must be on `<html>`, the same element as `:root` and `.dark` — put it on
`#root` or `<body>` and every semantic token keeps violet.

## R5. The brand doctrine says violet is "the one hue the product owns"

**Verified.** `index.css:47-50`: "Brand — violet. The one hue the product owns.
Reserved for interaction and identity". `docs/brand/README.md:92`: "Violet
`#7153F0`. It means *interactive or ours*". The styleguide repeats it as copy
(`frontend/src/views/StyleguideView.tsx:351`, `:442`, `:545`), and so does
`frontend/src/views/UsageView.tsx:63`.

A preset system makes the interaction hue a personal choice. That is a brand
decision before it is an engineering one, and the copy above becomes wrong for
anyone on a non-default preset. `open-questions.md` Q2. Mitigating fact: the
logo tile is neutral `#191919` (`docs/brand/README.md:231`, `index.html:31`),
so presets do not fight the mark.

## R6. `--brand-` prefix is shared with third-party colours

**Verified.** `--brand-discord*`, `--brand-chargebee`, `--brand-paypal-*`
(`index.css:403-442`, `.dark` overrides at `:454-456`) are consumed by
`FeedbackView.tsx`, `title-bar-utilities.tsx`, `ChargebeePitch.tsx`,
`paypal-icon.tsx` and others. Any code or test that reasons about "all
`--brand-*`" — a preset-completeness test, a codemod, a styleguide loop — will
sweep them in. Match `--brand-(50|[1-9]00)\b` explicitly.

## R7. The CI token gate has a gap an implementer could fall into

**Verified by reading `scripts/ci/assert-design-tokens.sh`.** Check 4 greps
only `#[0-9a-fA-F]\{6\}`. An `oklch(…)`, `rgb(…)`, `hsl(…)` or 3-digit `#abc`
literal in a `.ts`/`.tsx` file passes. It also does not scan `.css` at all
(`INCLUDES=(--include=*.tsx --include=*.ts)`).

**Consequence:** "it passed CI" is not evidence the registry kept colours out
of TypeScript. The design keeps them in `index.css` on principle. Tightening
the gate (e.g. adding `oklch\(` for `frontend/src/**/*.ts*`) is cheap and
reasonable, but is a separate change — today's tree has zero hits for it, so it
would land clean.

## R8. Neutrals and the hover tint carry violet at low chroma

**Verified.** The surfaces and ink sit at hue ~286° (`index.css:84-97`,
`:150-160`; `docs/brand/README.md:110-115`, "Neutrals carry the brand"). The
most visible is the active rung: `--surface-light-active` `#ECE9FC` at chroma
0.0256 (`index.css:90`) backs `--accent`, the hover tint on every menu row
(`index.css:232`). In dark, `--surface-dark-active` (`index.css:97`) is also
`--secondary`, `--input`, `--chrome-border` and `--sidebar-accent`
(`:469`, `:496`, `:504`, `:555`).

Under a green preset, hover rows and the dark active nav row stay faintly
lavender. Overriding the rung per preset would reach dark inputs and borders
too — a wide blast radius. v1 leaves neutrals fixed; `open-questions.md` Q4.

## R9. The backend seam exists but is per-company, admin-visible and racy

**Verified.** `PATCH …/auth/me` (`crates/opencompany-core/src/server/users/routes.rs:82`,
`:1091`) and `UserRecord` (`crates/opencompany-core/src/ports/users.rs:61`)
could carry a preference with no migration (JSON blob in every store). But:

1. **Per company.** Every read and write is keyed by `runtime.id()`. The
   desktop connects to several hosts from one webview; a server-stored accent
   would differ per company, while the Appearance page defines the choice as
   per browser (`AppearanceView.tsx:4-9`).
2. **Admin-visible.** Admins read user records through `UserSummary`
   (`crates/opencompany-core/src/server/users/admin.rs`, struct near `:130`). A
   new field must be kept out of it deliberately.
3. **Whole-record replace.** `upsert_user` writes the entire record, and the
   handler's own comment names the residual read-to-write gap
   (`routes.rs:1126`). A preference save could resurrect a stale `role` or
   `status` an admin just changed.
4. **Refused during `must_change_password`** (`routes.rs:1110`).

None blocks v1, which is `localStorage` only. All four shape the future work.

## R10. Brand-derived chart and graph colours collide with Slack-style hues

**Verified values, inferred impact.** `--chart-1` and `--kg-accent` follow the
brand ramp (`index.css:324`, `:532`, `:811`, `:832`). The other chart slots are
cyan/green/amber/pink (`:325-328`), and the graph's `--kg-warn` (amber) marks
people while `var(--accent)` marks AI agents
(`frontend/src/views/overview/kg/KnowledgeGraph.tsx:56`, `:1719`, `:2197`).
A Jade preset would make chart series 1 and 3 both green; a Clementine or
Banana preset would make AI agents and people the same amber. Resolved in
`architecture.md` §4 by pinning both to a fixed primitive.

## R11. Common Slack-style hues fail white-on-500 at 4.5:1

**Verified by measurement** (formula at `docs/design-system/color.md:11-17`):
white on green `#16A34A` 3.30, amber `#D97706` 3.19, teal `#0D9488` 3.74; blue
`#2563EB` passes at 5.17. Presets in warm and green families must use a darker
500 (which dulls them) or a dark foreground via the mode-qualified override.
This is why the contrast test (`test-plan.md` U4) is in v1, not future work.

## R12. The default brand already misses 4.5:1 on two grounds

**Verified by measurement, 2026-09-25.** `brand-500` measures 4.68:1 on the
canvas, but **4.20:1** on `--accent` `#ECE9FC` and **4.22:1** on `--chrome`
`#EBEBF4`, where the sidebar sits. If the contrast test asserts "primary on
every ground it appears on", today's default fails it. The test must assert
the documented pairs (`architecture.md` §9) and this gap should be recorded,
not silently widened. Separate follow-up.

## R13. Contrast numbers disagree between the CSS and the docs

**Verified.** `index.css:464-466` says white on `brand-500` "measures 4.70:1";
`docs/design-system/color.md:52` says 5.00:1, and 5.00 is what the documented
formula returns. `color.md:55` gives `brand-700` on the active row as 6.91:1,
which is correct for `#ECE9FC`, but the sidebar now puts it on `brand-100`
(6.29:1, `index.css:336-340`). Anyone vetting presets against "the documented
bar" will find two bars. Fix the comments when the preset docs are written.

## R14. Tests that slice `index.css` by string can be fooled

**Verified.** `test/unit/shell-chrome-tokens.test.ts:37-52` finds blocks with
`indexCss.indexOf(\`${selector} {\`)` — the **first** occurrence. A preset
written as `[data-accent-preset="x"].dark {` contains the substring `.dark {`;
if it ever sat above the real `.dark` block, those tests would read the
preset's block instead. Keeping the preset section at the end of the file
avoids it; test U3 pins that order.

## R15. Existing theme e2e depends on the Appearance page's height

**Verified.** `test/e2e/theme-toggle-visible.spec.ts:54` runs at a 150px-tall
viewport and asserts the Theme card sits below the fold (`:111`). Adding an
Accent card **below** it keeps that true; adding it **above** would change
what the spec measures. Keep Theme first.
