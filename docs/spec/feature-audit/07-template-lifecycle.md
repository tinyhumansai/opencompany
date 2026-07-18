# Template Lifecycle

## Outcome

Operators can discover, inspect, install, customize, export, and safely upgrade
company templates without losing local decisions or accepting unreviewed
changes.

## Why this matters

The repository ships nineteen company definitions and validates their content,
but a productized catalog also needs stable identity, versioning,
compatibility, provenance, distribution, and upgrade behavior. A template is
more than its manifest: it includes workflows, skills, workspace seeds,
charter defaults, policies, and an explanation of the retained human role.

## Proposed capability

- Define a template package manifest with stable id, semantic version, schema
  compatibility, publisher, license, changelog, and content hashes.
- Package the complete company directory without executable code or secrets.
- Browse a local or remote catalog with capability, industry, risk, required
  connections, expected cost, and maturity metadata.
- Verify package integrity and optional publisher signatures before install.
- Record the exact source version and provenance for every launched company.
- Keep Operator changes in explicit overlay layers.
- Calculate upgrades as three-way diffs between the previous template, the new
  template, and the effective local company.
- Present each material change for review and allow partial adoption.
- Run validation and dry-run checks before applying an upgrade.
- Support rollback to the previous effective configuration.
- Export a portable company bundle with a clear choice of configuration-only
  or configuration plus durable state.

## Acceptance boundary

- Installing or upgrading never executes template content.
- Packages cannot contain secrets, absolute paths, or path traversal.
- Existing companies remain pinned until the Operator accepts an upgrade.
- Local overlays are not silently overwritten.
- An upgrade that changes policy, budget, discovery, or sellable skills gets
  explicit review regardless of compatibility classification.
- Rollback restores the previous effective configuration and provenance.
- Offline catalogs and direct directory installs remain supported.

## Likely implementation seams

- `companies/`, `src/company/`, and `opencompany check`
- `docs/spec/product/templates.md` and manifest compatibility rules
- export/import support and CompanyStore provenance
- content-validation CI and package-security tests
- a gallery, diff review, and upgrade history in the console

## Open questions

- Whether the first catalog is a Git repository, static index, or service.
- Signature and trust policy for community publishers.
- Which state families are portable across runtime versions and storage
  backends.
- How long older schema validators must remain available.
