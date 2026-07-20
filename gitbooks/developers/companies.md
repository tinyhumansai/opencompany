---
description: A company is a manifest plus docs — data, not code.
---

# Authoring companies

Every folder under `companies/` is a **business type — data, not code.** The
single configurable host instantiates any of them; a business is a manifest
plus its docs, never its own program. Adding a business is a new folder, not a
new crate.

## Anatomy of a company

Each folder follows the same shape:

| Part | File | Content |
| --- | --- | --- |
| Manifest | `company.toml` | The roster of agents, their responsibilities, the output, and the moments reserved for the human — the machine-readable definition the host loads. |
| Story | `README.md` | What the company does and produces, in plain language, including **what the human keeps.** |

The behavior lives entirely in the host and the vendored runtimes; each
definition just configures it. The operator console is a separate,
company-agnostic app under `frontend/` — one UI for every company.

## What goes in the manifest

A `company.toml` seeds the whole company:

- **Roster** — the agents and their distinct mandates.
- **Charter defaults** — mission, tone, and never-do seeds.
- **`[policy]`** — the checkpoints: which effects always require the operator
  (spend, send, sign, publish). A definition with no checkpoints is rejected.
- **`[place].skills`** — what the company *could* sell if taken
  [public](../overview/tiny-place.md) (ships `discoverable = false`), each
  entry priced and described.

## Author, validate, launch

```sh
opencompany check companies/agentic_marketing_agency        # validate
opencompany serve --company companies/agentic_marketing_agency   # launch
```

`opencompany check` reports any problems in plain language. Lint rules enforce:
unique agent ids; every sellable skill priced and described; `[policy]`
present; the README states the human role; and prosumer-language rules on the
README and descriptions (no runtime internals like "agent graph" or "dispatch"
leaking into operator-facing text).

## Customization without forking

An operator's changes — interview answers, charter edits, standing rules —
layer **over** a template with provenance. The template underneath stays
pristine, so a template update can be offered as a diff ("the Marketing Agency
template improved its SEO teammate — apply?") instead of a merge conflict.
Applying a template update is always an explicit action, never automatic.

## Run several together

Bring up the host + console with the hot-reloading demo launcher:

```sh
./scripts/launch-demo.sh marketing up
./scripts/launch-demo.sh marketing down       # stop; keep data
./scripts/launch-demo.sh marketing down -v    # also delete persistent data
```

`./scripts/list-demos.sh` lists every accepted company directory name and the
short aliases for the common demos.
