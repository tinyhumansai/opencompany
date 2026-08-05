# Authority on the write plane (issue #403)

`ScopedCompany` answers *may this principal talk to this company*, and stops
there — no role check. That is the right guard for the many writes this product
deliberately leaves open to any member: opening a task, posting to a discussion,
adding a teammate. It is the wrong guard on its own for a write that decides
something **for** the company, which is what `AdminScopedCompany` is for. It
composes `ScopedCompany` with `users::admin::require_admin`, so a route declares
the authority it needs in its signature rather than in a call a handler has to
remember to make.

**Who qualifies**, matching the two principals in
[`config.md`](../../spec/runtime/config.md): a human whose session says they
administer this company (a member gets `403`), or the machine principal entitled
to address it — the hosting control plane, which provisions the tenant and holds
its database credentials, so refusing it a route it already sits above would be
ceremony rather than a boundary. Neither gets anonymity: `AdminScopedCompany`
always names an actor (`User` + user id, or `System` + tenant), and the routes
that use it journal it.

**Which routes.** The line is *the company's outward identity* — what it reaches
the world as, and which third-party accounts its agents act through:

| Surface | Admin-scoped |
|---|---|
| `composio` | `PUT …/composio/token`, `POST …/composio/authorize` |
| `connections` (`oauth`) | `POST …/connections/{p}/start`, `POST …/connections/{p}/disconnect` |
| `inference` | `PUT …/inference`, `DELETE …/inference` |
| `smtp` | `PUT …/smtp`, `POST …/smtp/test` (the caller names the recipient) |
| `domain` | `PUT …/domain` |
| `channels` | `PUT`/`DELETE …/channels/telegram`, `POST …/channels/telegram/webhook` |
| `mcp` | `POST …/mcp/servers`, `PUT`/`DELETE …/mcp/servers/{name}`, `POST …/mcp/servers/{name}/oauth/start` |

Reads on those same surfaces stay open to any member: they carry a tier name and
non-secret routing, never a credential, and knowing *that* Gmail is connected is
what lets a member understand why an agent can read mail. So do the probes over
already-stored config — `POST …/inference/test`, `GET …/mcp/servers/{name}/tools`,
`POST …/mcp/servers/{name}/test`, `POST …/domain/verify` — which name no
destination of their own.

This is deliberately **not** a read-scope / write-scope split. "Write" and
"requires authority" are different questions here and the product answers them
differently on purpose; an extractor named for the HTTP verb would have to take
capabilities away from members that are meant to be theirs.

`server::ops::write_test` pins the whole table against a member session, so a
route joining this plane joins that list too.
