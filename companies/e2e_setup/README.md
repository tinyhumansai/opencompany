# E2E Setup Co

A fixture company with **no team**, used by the first-run company-setup
end-to-end lane (`frontend/test/e2e/company-setup.spec.ts`).

First-run setup offers itself only when a company's roster is empty — see
[`docs/spec/runtime/company-setup/overview.md`](../../docs/spec/runtime/company-setup/overview.md).
Every other company under `companies/` declares agents in its manifest, so none
of them can reach the flow. This one is the fresh-tenant shape.

Run the lane against it with:

```sh
PW_HOST_COMPANY=companies/e2e_setup npx playwright test company-setup
```

Adding an `[[agent]]`, a desk, or a workflow here would stop it being a first
run, and the spec would fail on the dialog never appearing.
