# Finance in the console

The operator-facing finance surface: one top-level **Finance** section with two
sub-pages — **Invoicing** (Chargebee, issue #788) and **Wallet** (PayPal, issue
#789) — where each provider is connected, verified, and exercised against the
real account without leaving the console.

**Status: implemented.** The host module is `src/server/ops/finance.rs`; the
console lives under `frontend/src/views/finance/`. This document remains the
reasoning behind both — read it before changing either.

## Why a section and not a settings tab

Chargebee and PayPal are configured today at `#/settings/billing`, in a single
`BillingView` that renders two credential cards. That is the right home for a
credential and the wrong home for everything else about money:

- A settings tab is a place an operator visits **once**. Invoices and a wallet
  balance are read repeatedly, and burying a daily read under Settings → Billing
  makes it a place nobody looks.
- The one page conflates two unrelated integrations. A PayPal read that fails
  already had to be prevented from taking the Chargebee form down with it (see
  the `Promise.allSettled` note in `BillingView`); two surfaces make that
  structural rather than a workaround.
- "Billing" is ambiguous in a product that is itself billed. An operator reading
  "Billing" reasonably expects *what OpenCompany charges me* — which is Settings
  → Usage. **Invoicing** is what the company charges its own customers, and
  naming it so removes the collision.

So: Settings → Billing keeps nothing. The credential forms move into the two new
sub-pages, beside the data they unlock, and the Settings row is retired.

### Relationship to the parked `FinancesView`

`frontend/src/views/FinancesView.tsx` exists and is parked (issue #302 unmounted
it from `NAV`; it remains routable per `console-routes.ts`). It renders a
**ledger projection** — `GET …/finances`, folded from the company ledger and the
manifest `[budget]` by `metering::finances_from` — not provider data. It is the
company's own internal accounting; Chargebee and PayPal are the outside world.

Both belong under Finance, as three sub-pages, not two:

| Sub-page | Source | Nature |
| --- | --- | --- |
| Overview | `GET …/finances` (**exists**) | ledger fold: balance, budget, spend by category |
| Invoicing | Chargebee, via new read routes | what customers owe and have paid |
| Wallet | PayPal, via new read routes | what is actually in the account |

Overview is the parked view re-hosted unchanged — it is the landing page, so the
section is never empty on a host with no provider configured.

## What exists today

**Backend, complete and tested:**

- `src/chargebee/` — `client` (REST v2, form-encoded, HTTP Basic) and `api`:
  `get_customer`, `create_customer`, `send_invoice`, `get_invoice`,
  `list_invoices`. Projections are `InvoiceSummary` / `CustomerSummary` in
  `src/chargebee/types.rs`. Every money field is `*_in_minor_units: i64`.
- `src/paypal/` — `client` and `api`: `get_wallet_balance` → `Vec<Balance>`,
  `list_transactions` → `Vec<Transaction>`. Money is a decimal **string**,
  deliberately never an `f64`.
- `src/server/ops/billing.rs` — the credential write-plane:
  `GET|PUT …/billing/chargebee`, `DELETE …/billing/chargebee/key`, and the
  PayPal equivalents. Returns `BillingStatus` / `PaypalStatus`: booleans plus
  the non-secret site slug and environment. Writes are admin-only and
  all-or-nothing (`write_all` rolls back).
- `src/server/hooks_chargebee.rs` — the inbound webhook.
- `src/harness/built_in/{chargebee,paypal}.rs` — the same `api` functions as
  agent tools, permission-gated (`chargebee_send_invoice` and
  `chargebee_create_customer` are `Execute`).

**Frontend:**

- `frontend/src/api/billing.ts` — `getBilling`/`saveBilling`/`clearBilling` and
  the PayPal trio.
- `frontend/src/views/BillingView.tsx` — both credential forms.

**The gap:** `chargebee::api` and `paypal::api` are reachable **only** from a
harness turn. There is no HTTP path by which the console can list an invoice or
read a balance. That is the whole of what this design adds on the host side.

## Host: the finance read plane

New module `src/server/ops/finance.rs`, merged in `server/ops/mod.rs` beside
`billing::router()`. It is a thin adapter: resolve the company's credentials
out of its `SecretStore`, build the provider client, call the existing `api`
function, serialize its existing projection. **No new business logic, no new
money types.** If a shape is wrong for the console it is wrong for the agent
too, and the fix belongs in `api`.

```text
GET    …/finance/chargebee/invoices?status=&customerEmail=&limit=
GET    …/finance/chargebee/invoices/:id
GET    …/finance/chargebee/customers?email=
POST   …/finance/chargebee/test
POST   …/finance/chargebee/invoices            (admin)
GET    …/finance/paypal/balance
GET    …/finance/paypal/transactions?since=&until=&limit=
POST   …/finance/paypal/test
```

`scoped(...)` gives both scope forms, as every other ops router does.

### Scope and method rules

- Reads are `ScopedCompany` — any signed-in member. An invoice list is not more
  sensitive than the ledger the Overview page already shows them.
- `POST …/test` is a **read** dressed as a POST: it is non-idempotent only in
  that it costs a provider round-trip, and making it a GET invites a browser or
  a link preview to fire it. It takes no body and returns `{ ok, detail }`,
  where `detail` names the site or environment that answered.

  There is no `ok: false`. A failed check is the provider's own failure and
  renders as the `502` below, carrying the provider's code and message —
  collapsing that into a `200 {ok: false}` would throw away the only part of the
  answer an operator can act on. And there is no `checkedAt`: the console knows
  when it clicked, so stamping a time on the host would mean a clock in a module
  that otherwise needs none.
- `POST …/invoices` is `AdminScopedCompany`. It bills a real customer real
  money. See **Testing without billing a customer** below.
- There is no `DELETE`. Voiding an invoice is a Chargebee operation with
  accounting consequences and no `api` function behind it; it stays out until
  somebody asks for it deliberately.

### Failure shape

Three failures are distinguishable and must stay so, because their remedies are
in three different places — the same reasoning that gave `BillingStatus` four
flags instead of a `connected` boolean:

| Condition | Status | Body |
| --- | --- | --- |
| feature not compiled in (`cfg!(feature = "chargebee")` false) | `501` | `{ code: "not_in_build" }` |
| credentials absent from the secret store | `409` | `{ code: "not_configured" }` |
| provider rejected or was unreachable | `502` | `{ code: "provider_error", provider, providerStatus, providerCode, message }` |
| an argument was rejected before the call (`status: 0`) | `400` | `{ code: "invalid_arguments", … }` |

The last row is the one the design missed and the implementation needed. The
`api` functions already mark a locally rejected argument — an empty invoice id, a
missing date window — with `status: 0`, meaning the call never left the process.
Rendering that as a `502` would send an operator to check PayPal's status page
over a date range they can fix themselves.

`OpenCompanyError::Chargebee`/`::Paypal` already carry `status`, `code` and
`message`; `provider_error` passes them through verbatim. PayPal's rewritten
"data for the given start date is not available" message
(`explain_unavailable_window`) reaches the console intact, which is the point of
having rewritten it — the console shows the remedy, not the riddle.

A `501` and a `409` are what the two pages render their empty states from, so
neither ever shows a spinner that resolves into nothing.

### What is deliberately not added

- **No caching, no polling.** Every page load is a live call. A cached invoice
  status is a wrong invoice status, and the volumes here are an operator
  clicking a tab, not a dashboard on a wall.
- **No provider data in the ledger fold.** `finances_from` stays ledger-only.
  Mixing a live provider read into a durable projection makes the Overview page
  fail when PayPal is slow, and makes the number irreproducible.
- **No PayPal writes.** `src/paypal/mod.rs` is explicit that `send_payment` was
  left out of #789 pending a scoping decision. Nothing here changes that: the
  Wallet page is read-only, and says so.

## Console: the Finance section

`finance` is already a member of `View` (as `finances`) in
`frontend/src/lib/console-routes.ts` and already has a `ROUTABLE` entry. Bringing
the section back is a `NAV` row plus sub-routing — the surface was parked, not
retired, and `console-routes.ts` is emphatic that those are different acts.

**Shape:** `FinanceSection.tsx`, modelled directly on `SettingsSection.tsx` —
sub-sidebar on `sm:` and up, a scrolling chip row below it, `aria-current="page"`
on the active entry, and the sub-page id as the hash's second segment.

```text
#/finances            → Overview   (the existing FinancesView, unchanged)
#/finances/invoicing  → Chargebee
#/finances/wallet     → PayPal
```

`NAV` gains one row between Workspace and Approvals: `{ view: "finances", label:
"Finance", icon: Wallet }`.

Each sub-page is rendered with `key={company ?? "self"}`, for the reason
`SettingsSection` already does it for Billing and Hosting: these pages carry
typed-but-unsaved credentials, and a company switch must remount rather than
carry a Chargebee key into another company's Save.

### Both provider pages share one layout

Each is a **connection panel** stacked over a **data panel**, and the connection
panel collapses to a single line once the provider is working. An operator
configures once and reads daily; a permanently expanded credential form taxes
every visit for a task performed once.

```text
┌─ Invoicing ───────────────────────────────────────────────┐
│ ▸ Chargebee · acme-test · Connected      [Test] [Manage]  │  ← collapsed
├───────────────────────────────────────────────────────────┤
│  Status  ▾ All   Customer ▾ Any            [Send invoice] │
│  ─────────────────────────────────────────────────────────│
│  INV-0042  alan@…   $1,250.00  Paid         due 3 Sep     │
│  INV-0041  ida@…      $480.00  Payment due  due 1 Sep  ↗  │
└───────────────────────────────────────────────────────────┘
```

Expanded, the connection panel is the existing `BillingView` card for that
provider, moved not rewritten. Its four-state reporting is the part worth
keeping intact: *no key/site*, *no webhook*, *not granted*, *not in build* fail
differently and none of them is fixed by the credential form. The collapsed line
shows the worst of the four, so "Connected" never hides "this company does not
grant `chargebee`, so no agent can use any of this".

Two corrections to that reporting came out of issue #1796:

- **The panel can now fix *not granted*.** The remedy used to end in "it cannot
  be fixed from this page", which was true and was the bug: nothing in the
  console could write `[tools].allow`, and on a hosted tenant the manifest is a
  read-only boot snapshot. `Health.grantNamespace` names the namespace and the
  panel renders a **Grant** control over `PUT …/tools/grants`
  ([tools.md](tools.md)). It is offered for *not granted* only — on a host built
  without the provider the grant would succeed and change nothing.
- **Precedence needs the configured-check first.** *not granted* outranking
  *not configured* is right, but only once there is a credential for the missing
  grant to be blocking — which is what `health.ts` always said and what its
  guards did not do. Testing `!granted` first put a company that had never
  touched Chargebee into the *not granted* arm, which asserts "Connected" and
  interpolates the site, rendering `Connected to null — but no teammate can use
  it`. Two claims in one line, both false. PayPal had the same ordering and
  printed a bare "Connected —" over a company with no client id at all.

### Invoicing (Chargebee)

- **List** — `InvoiceSummary` rows: id, customer, total, status badge, due date,
  and a `payment_url` link when one exists. Filters for status and customer
  email map straight onto `ListInvoicesArgs`.
- **Detail** — a sheet, not a route. It is a read of one row on the page behind
  it, and a sheet keeps the list's filter state without a route to restore.
  Shows line items, amount paid, amount due, and the payment link.
- **Send invoice** — a dialog over `POST …/finance/chargebee/invoices`, taking
  `SendInvoiceArgs`. Two things it must get right:
  - **Minor units.** The form takes dollars-and-cents and converts; the field is
    labelled with the currency and shows the minor-unit value it will send. The
    `*_in_minor_units` naming exists because "invoice Alan $100" becomes a $1.00
    invoice otherwise, and a UI that hides the unit re-opens exactly that hole.
  - **Idempotency.** The dialog mints a `idempotency_key` on open and sends it,
    so a double-clicked Send is one invoice. `InvoiceSummary` sets a replay flag
    when Chargebee returned an earlier invoice for the key — the toast says
    "already sent" rather than "sent", because a replayed response is otherwise
    byte-identical to a fresh one.
- **Webhook** — the panel shows `webhookUrl` with a copy button, exactly as
  `BillingView` does now, and says plainly when the host has no public URL that
  Chargebee could deliver to.

### Wallet (PayPal)

- **Balance** — one card per `Balance`, primary currency first, `available` and
  `withheld` rendered as the strings PayPal sent. No arithmetic, no `toFixed`,
  no summing across currencies.
- **Transactions** — `Transaction` rows: date, counterparty, note, signed
  amount, and a status chip decoding `S`/`P`/`V`/`D` into Success / Pending /
  Reversed / Denied. A raw `V` means nothing to an operator.
- **The lag is stated on the page, not discovered.** PayPal serves transaction
  data up to three hours behind and rejects a window ending inside that gap.
  The default range therefore ends three hours ago, the range picker will not
  select later, and the panel says why. This is the one place the console
  encodes provider behaviour rather than passing it through, and it is justified
  by `explain_unavailable_window` existing at all: the failure it describes is
  one an operator hits on their first click otherwise.
- **Environment.** A `sandbox` connection carries a persistent badge on the
  page, not only in the connection panel. Reading a sandbox balance and
  believing it is real money is the failure the badge exists to prevent.

### Testing things out

"Test" means three different depths, and the design offers all three because
they answer different questions:

1. **Test connection** — `POST …/finance/{provider}/test`. One cheap
   authenticated call (Chargebee: a 1-row invoice list; PayPal: a token fetch
   plus balance read). Answers *are these credentials live?* Result renders
   inline with the provider's own message on failure, never a generic "failed".
2. **Read something real** — the list and balance panels are themselves the
   test. An operator who can see their invoices knows the integration works, and
   this needs no extra affordance.
3. **Send a test invoice** — the Send dialog, aimed at yourself. Discussed next.

### Testing without billing a customer

Sending an invoice is the only destructive thing on either page, and there is no
undo route. Three guards, in order of how much they cost:

- **Sandbox is the default and is obvious.** Chargebee test sites and PayPal
  `sandbox` are the intended place to exercise this. The environment badge is
  loud on both pages; a `live` Chargebee site (no `-test` suffix) is stated in
  the Send dialog's confirm line.
- **The confirm line names the money and the recipient**, in the currency, at
  the magnitude that will actually be charged: "Invoice alan@example.com
  $1,250.00 USD on acme (live)." Not "Are you sure?".
- **Admin only.** `POST …/finance/chargebee/invoices` is `AdminScopedCompany`,
  matching `PUT …/billing/chargebee`. A member can read every invoice and raise
  none.

A "dry run" mode was considered and rejected: Chargebee has no preview endpoint
that returns a real `InvoiceSummary`, so a dry run would render a *fabricated*
invoice — which teaches the operator that the flow works while proving nothing
about whether it does.

## Files

| Path | Change |
| --- | --- |
| `src/server/ops/finance.rs` | new — the read plane above |
| `src/server/ops/mod.rs` | merge `finance::router()` |
| `frontend/src/api/finance.ts` | new — typed client for the routes |
| `frontend/src/views/finance/FinanceSection.tsx` | new — sub-router |
| `frontend/src/views/finance/InvoicingView.tsx` | new |
| `frontend/src/views/finance/WalletView.tsx` | new |
| `frontend/src/views/finance/ConnectionPanel.tsx` | new — the collapsed/expanded shell, shared |
| `frontend/src/views/finance/ChargebeeForm.tsx` | new — the credential form, lifted out of `BillingView` |
| `frontend/src/views/finance/PaypalForm.tsx` | new — likewise |
| `frontend/src/views/finance/SendInvoiceDialog.tsx` | new |
| `frontend/src/views/finance/health.ts` | new — the four-state precedence, pure |
| `frontend/src/views/finance/money.ts` | new — minor units, status decodes, the PayPal window; pure |
| `frontend/src/views/BillingView.tsx` | **deleted** — its two cards became the forms above |
| `frontend/src/views/SettingsSection.tsx` | drop the `billing` row |
| `frontend/src/components/app-shell.tsx` | `NAV` row + `sub` threaded to `FinanceSection` |

`frontend/src/api/billing.ts` is unchanged — the credential calls are the same
calls, made from a different page.

`docs/spec/runtime/api.md` gains nothing: it does not document the
`…/billing/…` routes either, and this file is the reference for both halves of
the surface.

## Testing

What was actually written, against the plan above:

- Host: `src/server/ops/finance.rs`'s own module, 7 tests in the default lane and
  8 with both features. The `501` case is asserted by a test compiled **only**
  on a build with neither feature, which is the only place that arm exists.
- Console: `finance-money.test.ts` (18), `finance-health.test.ts` (9),
  `finance-invoicing.test.ts` (6), `finance-company-switch.test.ts` (3).
- `scripts/ci/feature-lanes.txt` gained `server::ops::finance` on both the
  `chargebee` and `paypal` rows, and the matching filter is in ci.yml. Without
  it the two feature-gated tests here would be compiled by the lane and selected
  by nothing — the #770 pathology, arrived at from a new direction.

The original plan:

- **Host, per route:** not-in-build → `501`, unconfigured → `409`,
  provider 4xx → `502` carrying the provider's code, happy path → the
  projection. Plus the two scope assertions: a member may read, a member may not
  `POST …/invoices`. `src/server/ops/billing.rs`'s existing route-level tests are
  the model, including their `FailsOnOneKey` secret store.
- **Both providers need a fake transport.** The existing `api` tests already
  stand up one; the finance routes reuse it rather than mocking at the `api`
  boundary, so the adapter's own serialization is covered.
- **Feature lanes:** `chargebee` and `paypal` already have rows in
  `scripts/ci/feature-lanes.txt`. New gated tests must not turn a
  `compile-only` row into a lie — `scripts/ci/assert-feature-lanes.sh` fails if
  they do, so any row needing promotion changes in the same PR.
- **Console unit:** the minor-unit conversion (the dollars→cents function is
  tested directly, with `1250.00 → 125000` and `0.1 + 0.2` cases), the
  `S`/`P`/`V`/`D` decode, the three-hour window clamp, and the sub-page
  resolver — mirroring `test/unit/billing-view-branches.test.ts`.
- **E2E:** a company with no provider configured lands on Overview and shows
  both sub-pages in their `not_configured` state without an error toast.

## Deviations from the design

- **The invoice detail sheet was not built.** The list row carries the id, the
  customer, the total, the status and the payment link, which is what the design
  said the sheet was for minus the line items. A sheet showing three more fields
  is not worth a component until somebody asks for one; `GET
  …/finance/chargebee/invoices/{id}` exists and is tested, so it is a render away.
- **The customer lookup route has no UI.** `GET …/finance/chargebee/customers`
  is implemented and tested; the Invoicing page uses customer only as an invoice
  filter, which is open question 1 below, still open.
- **`live` detection is a heuristic.** Chargebee's API does not report whether a
  site is a test site, so `SendInvoiceDialog` treats a slug not ending in `-test`
  as live. Wrong in the safe direction: a test site named without the suffix gets
  a warning it did not need.

## Open questions

1. **Does Invoicing need a customers list of its own?** `get_customer` and
   `create_customer` exist. The design uses customer only as an invoice filter;
   a full customer surface duplicates Chargebee's own UI for little gain.
2. **What does the webhook do to the page?** A `payment_succeeded` delivery
   already raises a chat message. It could also invalidate the invoice list, but
   that needs a console event the section subscribes to, and the list is a
   manual refresh away.
3. **Issue #1337** — what a parked surface should say to an operator who arrives
   on one — applies to Overview if this section ships before the ledger fold has
   real data on a fresh company.
