# Server Module

The server module owns the Axum HTTP surface. The base routes are:

- `GET /healthz`
- `GET /spec`
- `GET /tiny`

Operator chat and approvals live under `/api/v1/...` (see `server::operator`),
and feedback under `server::feedback`. Add future API routes as focused handler
groups rather than wiring behavior directly in the binary entrypoint.

## tiny.place A2A inbound + discovery (`tinyplace` feature)

Behind the `tinyplace` feature the server mounts the agent-to-agent surface
(`server::a2a`). With the feature off, none of these routes exist and the
default build links no crypto.

| Route | Purpose |
| --- | --- |
| `POST /a2a/{handle}` | JSON-RPC `tasks/send` from a counterparty agent |
| `GET  /a2a/{handle}` | the company's Agent Card (directory record) |
| `GET  /a2a/{handle}/skill.md` | human/agent-readable priced-skill catalog |
| `GET  /.well-known/agent-card.json` | the sole company's card (prosumer) |
| `GET  /companies/{handle}/.well-known/agent-card.json` | a named company's card |

`POST /a2a/{handle}` enforces the trust boundary in a fixed order before any
work reaches cognition:

1. Resolve a **discoverable** company (`[place].discoverable = true` with a
   matching `[company].handle`); a miss is `404`.
2. Verify the SIWX `Authorization` header (skew window + single-use replay
   protection via a host-global nonce cache). A bad/missing header is `401`.
3. For a skill priced above `0.00`, require a valid x402 authorization; without
   one the response is a `402` challenge naming the amount and the company's
   own tiny.place address.
4. Sanitize the counterparty payload (a minimal promptguard pass — control
   characters are stripped) before it becomes an `A2aTaskReceived` event and
   drives exactly one cycle. Paying customers run under the same approval gates
   as any other stimulus.

An unreachable tiny.place backend maps to `503`; any other transport failure is
`502`.

## Enable discovery for all companies

Every company declares its own discoverability in its manifest:

```toml
[company]
name = "Acme SEO"
handle = "acme"

[place]
discoverable = true
skills = [{ id = "seo.audit", price_usd = "25.00", description = "Full audit" }]
```

To opt **every** loaded company into going public regardless of its manifest,
pass `serve --discoverable`. It marks each company discoverable and synthesizes
a `@handle` (a slug of the company name) when one is missing, so Agent Card
generation and validation succeed:

```bash
cargo run --features tinyplace --bin opencompany -- \
  serve --discoverable \
  --company examples/agentic_law_firm \
  --company examples/agentic_marketing_agency
```

At boot each discoverable company runs the going-public flow (lifecycle step 3):
load-or-generate the Ed25519 keypair, `ensure_registered`, then publish the
Agent Card — all best-effort. An unreachable tiny.place degrades the company to
"private" with a warning and never blocks or fails boot.

Relevant configuration:

- `TINYPLACE_API_URL` — tiny.place economy base URL (default
  `https://api.tiny.place`).
- `OPENCOMPANY_PUBLIC_URL` — public host base embedded in published Agent Card
  endpoints. When unset, the endpoint falls back to `http://{bind}`.
