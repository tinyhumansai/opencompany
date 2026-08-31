# Tool grants

The three **tool-grant** routes (issue #1796), split out of
[`api-write-plane.md`](api-write-plane.md) to keep that file under the
repository's 500-line ceiling. Everything here is part of the console write
plane.

Connecting an integration stores a credential; it does not grant the tool
namespace, and only the first of those had a write path — so five connect
surfaces (Chargebee, PayPal, hosting, search, Composio) each ended in *"Add
`x` to `[tools].allow` in the company's manifest — it cannot be fixed from
this page."* The integration read **Connected** and reached nobody, and on a
hosted tenant with a read-only manifest snapshot there was nowhere else to go.

`GET` returns the grant list **in force**, the manifest's own list, what was
granted from the console and by whom, and `grantable` — the closed list of
namespaces the host will accept. `PUT {"namespace": "chargebee"}` grants one;
anything outside `CONSOLE_GRANTABLE_NAMESPACES` is a **`422`**. That list is
closed because this is the only overlay in the product that *widens* capability:
every entry is a namespace the catch-all `*` deliberately refuses to confer
**and** one the console holds a credential form for, so granting is the second
half of an action the operator already took against an account they already
hold. `shell`, `code` and `web` have no such form. The list is enforced again at
resolution, so a row that reached the store some other way confers nothing.

Granting what the manifest already grants stores no override — the console must
not claim credit for a seed grant, or a later `DELETE` would appear to revoke
one it cannot touch. `DELETE` withdraws one namespace (`?namespace=paypal`) or
every console grant, recomputing from the recovered seed (`seed_tool_allow`)
rather than subtracting from the folded list — which is what makes a manifest
grant untouchable *structurally* rather than by a guard. Both writes are
admin-only and attributed.

Unlike `[policy]`, the resolved value is **folded into the persisted manifest**
rather than merged at each read. `[tools].allow` is consulted at some three
dozen sites, and a parallel resolution would have to reach every one, with the
single missed site reproducing the bug the fix exists to close.
`ToolGrantsOverride` is stored separately so the seed stays recoverable
(`seed_tool_allow`) and the grant stays attributed. Three callers need that
inverse and must agree: the carry rule, this route's `manifestAllow`, and the
**export bundle**, whose `company.toml` becomes the restored company's seed.

**When a grant takes effect has three answers**, and the response reports which
one the caller got. Belts are wired per roster build, not read per call, so
storing a grant is not the same as delivering one:

| Cognition path | Answer | How |
|---|---|---|
| harness | **next turn** | `tool_grants_fingerprint` moves, `HarnessPool::ensure` rebuilds the roster |
| hosted / sidecar / echo | **now** | the runtime is rebuilt in place through the issue-#290 seam `PUT …/inference` uses |
| no rebuilder, or it failed | **restart** | the grant is stored; a failed rebuild is logged, not raised |

That third row is why this is not one constant: reporting "next turn" to a
company that will not pick the grant up until a restart would be the console
asserting reach the runtime does not deliver — #1796 one layer inside its own
fix. The override survives a rebuild unless the seed's `[tools]` itself changed;
here that rule bites harder than for `[policy]`, because a console grant
outliving a seed edit would be a runtime *widening* surviving its revocation in
version control.
