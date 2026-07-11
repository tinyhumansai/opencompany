# Templates

A Template is a packaged company definition ready to launch — the productized
form of today's `examples/*` manifests. The Template Gallery is the
prosumer's step 3 ([prosumer.md](prosumer.md)).

## Anatomy

A template is a directory containing:

| Part | File | Content |
| --- | --- | --- |
| Manifest | `company.toml` | company metadata, roster, default policy/budget/place tables ([runtime/manifest.md](../runtime/manifest.md)) |
| Story | `README.md` | what the company produces, the team, **what you keep** (human role) |
| Charter defaults | in-manifest | mission/tone/never-do seeds the interview can override |
| Skill catalog | `[place].skills` | what the company could sell if taken public (ships `discoverable = false`) |
| Checkpoint list | `[policy]` | which effects always require the operator |

Templates contain **no code**. The example crates shrink to a manifest plus
a two-line `main` calling `opencompany::run_company(...)`, so `examples/`
and the gallery are the same artifact.

## The 18 launch templates

From `examples/`: venture studio, software company, startup accelerator,
venture capital, consultation firm, marketing agency, design studio, media
company, influencer business, game studio, game business, recruiting
company, enterprise sales, customer support, real-estate company, accounting
firm, law firm, pharma startup. Each README states the human role — e.g. the
marketing agency keeps *campaign review and sign-off*; the law firm keeps
*filings and client counsel*.

## Authoring and validation

- New templates are PRs adding a directory under `examples/`; CI runs
  `opencompany check` on every template manifest.
- Lint rules: unique agent ids; every `[place].skills` entry priced and
  described; `[policy]` present (a template without checkpoints is
  rejected); README states the human role; prosumer-language rules apply to
  README and descriptions.
- **Schema versioning**: the manifest carries an implicit schema version;
  additive keys are minor, semantic changes require a migration note and a
  `opencompany check --fix` path. Old templates keep working
  (compatibility rule in [runtime/manifest.md](../runtime/manifest.md)).

## Customization without forking

With [agentic setup](../agentic/setup.md), templates additionally serve as
the Architect's **priors**: it may start from one, blend several, or compose
from scratch, and the gallery remains the offline fallback when no brain is
reachable. Template lint rules apply to Blueprints unchanged.

An operator's changes (interview answers, charter edits, standing rules)
layer **over** the template with provenance
([charter.md](../company-brain/charter.md)) — the template underneath stays
pristine, so a template update can be offered as a diff ("the Marketing
Agency template improved its SEO teammate — apply?") instead of a merge
conflict. Applying a template update is an explicit operator action, never
automatic.
