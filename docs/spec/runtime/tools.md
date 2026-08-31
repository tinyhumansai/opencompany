# Tool grants and scoping

*How a company decides what each of its agents can reach.*

Terms: [glossary](../glossary.md). The approval gate — a separate axis, covering
whether a granted tool's call needs a human first — is
[company-brain/grants.md](../company-brain/grants.md).

---

## The three levels

A teammate's tool belt is resolved by intersecting three declarations, in order:

```
[tools].allow  ∩  desk.tools  ∩  agent.tools
```

Every level is **narrow-only**. There is no path through the resolution that
yields a grant `[tools].allow` does not already cover, which is what makes the
lower two levels safe to hand to an operator: the worst a desk or an agent
declaration can do is remove capability.

Each level is **optional**, and an **omitted** level is a pass-through rather
than a denial:

> An **absent** grant means **"inherit"**, not **"nothing"**.

An agent with no `tools` line holds its desk's ceiling; a desk with no `tools`
line imposes the company's.

Since issue #1804 the **agent** level draws a further distinction that the desk
level does not, because a grant is a three-state value there:

> At the **agent** level: **absent** (`None`) inherits; an **explicit empty
> list** (`[]`) is a deliberate **deny-all** (nothing); a **non-empty list**
> narrows.

This is a deliberate contract inversion: `[]` used to mean "inherit" and now
means "hold nothing". It lets an operator lock a single teammate down to no
tools without touching the company or desk ceiling. The **desk** level keeps the
older rule — an empty desk `tools` states no ceiling (full pass-through), never
a company-wide deny-all — so the union sharp edge there is unchanged.

Any surface that renders an absent agent grant as "no tools", or an explicit
empty agent grant as "inherit", has inverted the meaning — see `AgentToolsDto`
in `src/server/ops/team_agent.rs`, whose field docs carry the same warning.

Resolution lives in one function,
[`agent_scoped_grants`](../../../src/runtime/builder.rs), and every reader goes
through it: the harness that wires the belt, the console's agent card, and the
roster list. A second implementation anywhere is a bug — the console showing a
tool the gate refuses is the exact failure this single-source rule prevents.

### Levels in detail

**Company — `[tools].allow`.** The ceiling, and **the one place a capability is
turned off for a whole company**. It defaults to `globals/globals.toml`'s
`default_allow`:

```toml
default_allow = ["*", "workspace.*", "workspace.write", "media", "composio", "search", "mcp:*"]
```

Every capability in the list above is on by default, and dropping an entry from
this list is how it comes off — for every teammate at once, whatever their own
`tools` line asks for. `allow` **replaces** the default rather than extending
it, so a company that means to withhold one namespace writes the rest of the
list out and leaves that one off.

The default withholds the credential-gated `chargebee`, `hosting` and `paypal`
integrations, and `repo`. The first three are opt-in by name because each is a
company-specific third-party integration — `hosting` publishes the workspace to
the public internet and provisions databases the company pays for. `repo` is
withheld for a different reason, and not as a preference: a host on filesystem
storage refuses to boot a company whose allow-list names it, because a
repository credential would sit on that filesystem in plaintext. A
MongoDB-backed company that wants it adds `repo.*` here and on the teammates
that need it.

#### Granting a credential-gated namespace from the console (issue #1796)

`[tools].allow` is seed-authoritative: a rebuild re-persists it from
`company.toml`, and for `[tools]` that is a security property rather than an
implementation detail. That left the credential-gated integrations above with no
way in on a hosted tenant, where the manifest is a read-only boot snapshot baked
into the image — so a company could connect Chargebee from the console, see
**Connected**, and reach no teammate, with the page correctly reporting that it
"cannot be fixed from this page".

A connect surface can now add the grant itself, through `PUT …/tools/grants`
([the write plane](api-write-plane.md)). It is an attributed operator override
folded into the effective list, **not** a manifest write, and it is bounded two
ways:

- **A closed list.** `CONSOLE_GRANTABLE_NAMESPACES` is exactly the five the
  console holds a credential form for — `chargebee`, `composio`, `hosting`,
  `paypal`, `search`. Granting is the second half of an action the operator
  already took against an account they already hold. `shell`, `code` and `web`
  have no such form and are not grantable from any page.
- **Version control still wins.** A `[tools]` edit in `company.toml` clears
  every console grant on the next rebuild. This layer only ever *widens*, so a
  grant outliving a seed edit would be a runtime capability surviving the
  operator revoking it — the named harm the seed-wins rule exists to prevent.

Narrowing is unchanged and lives one level down: the console withdraws a
namespace it granted, and takes capability *away* through desk ceilings, never
by subtracting from the company's own list.

**Desk — `[[group_chat]].tools`.** A department's ceiling. A company organises
its teammates into desks — a finance desk, a creative desk — and this is where
"nobody on this desk reaches the web" is stated once instead of repeated on
every member and hoped for on the next member added.

**Agent — `agents/<id>.toml` `tools`.** The individual's request.

### Agents on several desks take the union

A teammate on more than one desk takes the **union** of those desks' ceilings
before the intersection with the company grant.

Union rather than intersection, because desk membership is additive: joining the
growth desk is how a marketer gains the ad tools. Intersecting would make each
extra desk silently *revoke* capability, so adding someone to a desk could break
work they were already doing.

The consequence, which MUST be understood before relying on a desk ceiling: **a
desk with no ceiling narrows nothing**, so a teammate on both a restricted desk
and an unrestricted one ends up unrestricted. A company that means to restrict a
teammate states the ceiling on every desk that teammate sits on, or states it on
the teammate. This is the safe direction — the widest it can resolve to is the
company grant itself — but it is not the intuitive one.

## The wildcard does not mean everything

`*` covers `files`, `docs`, `shell`, `code`, `web` and `subagent`. It
deliberately does **not** confer the explicit opt-in namespaces, each of which
must be named:

| Namespace | Why it must be named |
| --- | --- |
| `media` | Spends real money per generated image or video. |
| `composio` | Reaches the tenant's connected third-party accounts and moves real side effects — sends email, opens PRs. |
| `search` | The queries leave the building, and a call is billed — to the managed platform, or to the company's own provider account. See [search.md](search.md). |
| `mcp:*` | Reaches every `[[mcp_server]]` the company wired, each holding its own endpoint and credentials; a bare `*` must not hand a third-party server's tools to every teammate. |
| `chargebee` | Billing API, wired only against the company's own Chargebee credentials. |
| `paypal` | Wallet reads — a business's private figure, not a `*` wildcard's business. |
| `hosting` | Publishes the workspace to the public internet and provisions databases the company pays for. |
| `repo` | Materializes a third party's source inside a sandbox where the agent may also hold `shell`. |

`repo.write` is tighter still: only the exact string confers it. A bare `repo`
grant carries read access and nothing else, because read and write are separate
decisions — a company that adopted the read tier must not silently acquire
agents that push.

Each rule has its own predicate beside the manifest types
(`grants_media_explicit` and siblings in `src/company/types.rs`). Nothing may
re-derive these answers from the generic glob matcher: it reports `*` as
covering everything, which is right for the ordinary families and wrong for
every opt-in namespace above.

The predicates accept two spellings — the bare namespace (`search`) or a
`namespace.`-descendant (`search.web`) — plus, for the workspace pair, the exact
`workspace.write` token. A `*` **glued** to the namespace (`search*`,
`workspace.write*`) is neither: the write path stores a request glob verbatim,
and no predicate accepts the glued form, so the coverage check and the card
preview both report it as not applying. Write the broken form instead
(`search.*`, or `search.web`) when a sub-grant ask is meant.

## The catalog

`src/company/tool_catalog.rs` enumerates everything a company can grant —
built-in families, `[[mcp_server]]` entries, and `[tools.composio]` toolkits —
in one vocabulary, served at `GET {scope}/tools/catalog`.

It is a **projection, never a source of truth**. Every entry carries the exact
grant token an operator would write, and resolves it through the same matcher
the roster build uses. An entry advertising a grant the gate does not honour is
a bug in the catalog, not a new kind of permission; a test asserts the
round-trip.

Two flags on each entry exist because the naive rendering is wrong:

- `granted` — whether `[tools].allow` currently confers it, resolved through the
  per-namespace predicates above rather than the glob matcher.
- `coveredByWildcard` — whether `*` would confer it, so a console rendering `*`
  as "everything" can say which four families it does not in fact cover.

A disabled `[[mcp_server]]` is listed with `granted: false` rather than omitted:
an operator needs to see that the server exists and is switched off, which an
absent row cannot say.

## Where a company's MCP servers are declared

A company's effective servers merge in four layers, lowest precedence first:

1. **Default** — `[[default_mcp_server]]` in the instance `config.toml`, shipped
   to every company on the install (`docs/spec/runtime/config.md`).
2. **Bundle** — `companies/<name>/mcp.json`, in the `{"mcpServers": {…}}` shape
   every other MCP host uses. Parsed by `src/company/mcp_file.rs` and merged into
   `mcp_servers` by `CompanyManifest::from_located` **before** validation, so a
   bundle server is held to exactly the rules an inline entry is — HTTP
   transport only, no credential in the URL, unique name.
3. **Manifest** — `[[mcp_server]]` in `company.toml`. Layer 2 and 3 are the same
   layer once loaded: a name declared in **both** is a validation problem naming
   both files, refused rather than resolved by precedence, for the reason the
   roster refuses a bundle that declares agents twice — either precedence rule
   silently discards a declaration somebody wrote down.
4. **Runtime** — what an operator adds or overrides from the console.

The server's name is the `mcp.json` map key, so it cannot disagree with itself.
`url` and `endpoint` are both accepted; setting both to different values is
refused. A `$comment` key is ignored at file and server level, because JSON
carries no comments and a template's reasoning has to live somewhere.

An invalid **entry** is dropped with a logged reason rather than failing the
boot — an `mcp.json` copied from a vendor README usually carries a stdio
`command`, which hosted v1 does not support — while a malformed **file** is a
manifest problem. `content_test` is what makes either fatal for a shipped
template, and additionally requires every shipped server to be `https`,
described, documented in its bundle README, and disabled if it names an
`authSecret` (an enabled server that needs a token fails at an agent's first
tool call rather than here).

Removing a server from `mcp.json` takes effect on the next `serve`, which
re-parses the bundle — not on an in-place rebuild, which uses the persisted
manifest. That is the same behaviour as editing `company.toml`.

## Runtime overrides

An operator may narrow a desk from the console. The override is stored on the
company record (`overlay_desk_tools`) and read through
`CompanyRecord::effective_desk_tools`, never directly — the same
`overlay_* → effective_*` discipline the spend cap and approval tier follow, so
the write path and both read surfaces cannot drift.

Two properties are load-bearing:

**Version control wins when it speaks.** On a rebuild, a desk's override is
dropped if that desk's seed `tools` changed
(`carry_desk_tool_overrides`, `src/runtime/builder.rs`). A console override
outliving a seed that narrowed the desk would be a runtime widening surviving
the operator revoking it in version control — the failure the `[tools]` /
`[policy]` seed-wins rule exists to prevent, and a per-desk grant is squarely
within it. The check is **per desk**, not whole-block: editing the finance desk
says nothing about the creative desk.

**A change reaches the next turn, not the next restart.** A tool belt is wired
once per roster, not once per call, so desk scoping participates in the roster
staleness fingerprint (`desk_scope_fingerprint`, `src/harness/mod.rs`). The
fingerprint covers which desks exist, who sits on them, and each ceiling —
because seating a teammate on a restricted desk narrows its belt just as surely
as editing that desk's ceiling does.

## What this does not do

Grants decide which tools are **wired**. They do not decide whether a wired
tool's call proceeds — that is the approval gate
([company-brain/grants.md](../company-brain/grants.md)) — nor how much a
namespace may spend, which is the capability plan (`[plan]`), nor what an agent
can reach once it holds `shell`. On that last point see
[security/agent-isolation.md](../security/agent-isolation.md), which is blunt
about what is not enforced.
