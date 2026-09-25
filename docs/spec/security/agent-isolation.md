# Agent isolation: what confines an agent, and what does not

**Status: current, and deliberately uncomfortable.** The threat model filed as
issue #752 C0, kept after the bound-repository tier it was first written for was
removed: nothing in it depended on repositories. It exists to be read *before*
someone concludes from a closed child issue that the agent shell is contained.
It is not.

Scope: what confines an agent running inside one OpenCompany tenant, with an
emphasis on the agent that holds `shell`. Not in scope: authentication of
humans ([users.md](../runtime/users.md)), cross-tenant isolation in
shared-single-database mode ([storage.md](../runtime/storage.md), which states
its own limit), or the console's own authorization model
([api.md](../runtime/api.md)).

## The claim, in one paragraph

**One container per tenant is the only hard boundary OpenCompany has.**
Everything inside that container — the server process, every agent's workspace,
every stored credential, the shell an agent runs commands through — is one
uid, one filesystem and one network namespace. The controls described below are
real and worth having, but they are *policy applied to a cooperative
component*, not a boundary between an attacker and the thing they want. An
agent that has been prompt-injected is not a cooperative component.

## Attacker model

The attacker does not have an account. They write text that an agent reads:
an issue body, a pull-request description, a web page fetched by `web_fetch`,
an inbound email, a page an agent fetched,
a Composio trigger payload, the body of a registry-sourced `SKILL.md`. Any of those can carry instructions, and the agent
that reads them is the one holding the grants.

Assume, therefore: **the attacker can issue any tool call the agent is granted,
with any arguments.** The question this document answers is what that buys
them. It is not "can the model be tricked" — assume yes — but "what is on the
other side of the tool call when it is."

## What is actually enforced today

Each of these is real. None of them is a sandbox.

| Control | Mechanism | Where |
| --- | --- | --- |
| One container per tenant | The `opencompany-manager` control plane builds and runs a per-tenant container | superproject, not this repo |
| Database per tenant | `OPENCOMPANY_MONGODB_URI` is scoped to that tenant's database | [storage.md](../runtime/storage.md) |
| Tool grants are per agent, and `repo` needs naming | `grants_repo_explicit` — the catch-all `*` does **not** confer `repo`, `media`, `composio` or `search` | `src/company/types.rs` |
| Repo tools need a grant, a wired manager **and** a binding | Three of the four gates in `build_agent`, each fail-closed with a warning (the fourth is the row below) | `src/harness/build.rs` |
| Classic PATs refused at intake | `ghp_…` reads every repository the account can reach; the route refuses it and says how to make a fine-grained one | `src/server/ops/repos.rs` |
| A checkout cannot reach the mirror it came from | Full object copy over `file://`, no hardlinks, no `alternates`, every back-reference severed | `src/harness/repo.rs` |
| A push at a mirror is refused by the mirror | A `pre-receive` hook installed in every mirror | `install_push_refusal` |
| Checkouts do not survive a turn | Orphaned checkouts swept at boot, tenant-scoped | `sweep_orphaned_checkouts` |
| Shell without an audit logger is no shell | `shell_audit` returns `None` → the whole `shell` namespace is withheld | `src/harness/toolbelt.rs` |
| The audit sink is not in the agent's sandbox | Issue #752 C6 — the sink is the host-owned `companies/<slug>/audit/<agent>/`, so the file tools' `workspace_only` policy **refuses** it instead of permitting it | `src/store/layout.rs`, `src/harness/toolbelt.rs` |
| A command that could not be recorded does not run | `AuditedShellTool` appends the intent line and fsyncs it **before** delegating; an append failure refuses the call | `src/harness/audit.rs` |
| Copilot turns reach nothing | `ConfinedToolPolicy` denies every tool call by name, empty belt, empty memory | `src/harness/confine.rs` |
| Web tools reject private and metadata IPs | OpenHuman's `url_guard`, always, regardless of allowlist | `src/harness/toolbelt.rs` |
| Per-company web allowlist, when set | `[tools].web_allowed_domains`; empty means allow-any-public | `src/harness/toolbelt.rs` |

**The exec `SecurityPolicy` is advisory.** `workspace_only`,
`block_high_risk_commands` and `require_approval_for_medium_risk` are read by
filesystem tools and by a command classifier. `workspace_dir` and `action_dir`
set the shell's **working directory**. A working directory is not a jail.

### On the write tier's approval gate, which is not here yet

The repository **write** tier (#247, #734–#738) is in flight and is **not on
`main`** as this is written — there is no `repo_publish` in the tree. When it
lands, its approval gate is adequate for what it guards: a push is
low-frequency, high-consequence and irreversible, exactly the shape an approval
gate is for, and its constraints (host-generated branch name, never the default
branch, no force, all inside `RepoManager`) are structural rather than
prompt-reachable.

It is still not a shell boundary. It runs in the same process and gates a
*tool*. An injected agent moving data out does not call `repo_publish` — it
calls `shell` and runs `git push`, or `curl`. So the gate does not lower the
priority of anything in this document, and it must not be cited as if it did.

A suggested precondition, not a code dependency: **the write tier should not
reach a tenant whose pod lacks default-deny egress.**

## What is not enforced

### 1. There is no sandbox. The shell runs directly.

OpenHuman's `ShellTool` routes execution through a sandbox backend only when
the agent's sandbox mode is `Sandboxed`
(`vendor/openhuman/src/openhuman/tools/impl/system/shell.rs`). `SandboxMode`
defaults to `None`, and **OpenCompany never sets one** — grepping `src/` for
`SandboxMode`, `jail` or `landlock` returns doc comments and nothing else. Every
shell command an agent runs is a direct child of the server process, with the
server's uid, the server's filesystem and the server's network.

The `cwd_jail` / Landlock subsystem exists in `vendor/openhuman` but is neither
wired nor compiled: `sandbox-landlock` is absent from the tenant feature set in
`deploy-staging.yml`.

### 2. Wiring Landlock would not close the exfiltration channel.

From `vendor/openhuman/src/openhuman/sandbox/cwd_jail/README.md`, verbatim:
*"Landlock does not gate network at all."* Egress isolation in OpenHuman exists
only through the Docker sandbox backend's `--network`, which needs
docker-in-docker and is not available in a tenant pod.

So a filesystem jail is worth having and does nothing for token exfiltration on
its own. This is why C4 is secondary and why closing it would not close #752.

### 3. On `fs` and `sqlite`, secrets are plaintext on the container's disk.

`FsSecretStore` writes one plaintext file per secret under
`<data-dir>/companies/<slug>/secrets/` and says so in its own doc comment;
`sqlite` puts the same bytes in a database file on the same disk. Both are
readable by the uid the shell runs as.

Since #752 C3 this specific exposure is refused rather than documented: binding
a repository credential, booting a company whose roster grants `repo`, and
wiring the repo tools for an agent all fail closed unless
`OPENCOMPANY_STORAGE=mongodb`. That closes the *repository* credential on that
path. It does not make the filesystem safe — every other secret on an `fs` host
is still plaintext next to it, and nothing here touches the shell.

### 4. There is no egress policy.

A tenant pod can open a connection to any host that resolves. Nothing in this
repo can change that, and no test in this repo can demonstrate otherwise: a
manifest lint is not proof, a blocked packet is. This is C1, it lives in
`opencompany-microservice`, and it is the load-bearing control in the whole set.

### 5. There is no per-agent uid separation.

Every agent in a tenant is the same uid, in the same process. Agent A's
workspace is a directory agent B can `cat`. The grant system decides which
*tools* an agent is offered; it does not decide what the shell can reach once
one of them is `shell`.

### 6. The audit trail still does not survive a determined `shell`.

Narrowed by C6 (issue #775), and worth reading for what it did **not** buy.

It used to read: the shell audit log is `<workspace>/audit.log` — the agent's own
workspace. That was an understatement. The workspace is also the
`SecurityPolicy { workspace_only: true }` root the file tools enforce, so the
audit trail sat *inside the write sandbox*: rewriting the record of an agent's
own commands was a **policy-permitted** write through its ordinary file tools,
not something it had to reach for `shell` to do. The one fail-closed write
boundary this codebase has included the audit log as a legitimate target.

Two things changed:

- **The sink moved out.** It is now the host-owned
  `companies/<slug>/audit/<agent>/audit.log`, one directory per agent, resolved
  through `DataLayout::agent_audit_dir` and threaded to the harness as an
  explicit `audit_root`. The permitted write is now a refused one. This is a
  boundary change rather than a hardening: the same `file_write` call that
  truncated the trail now lands on an inert file inside the sandbox.
- **The gate moved earlier.** Withholding shell when the logger cannot
  *initialize* made the log's existence fail-closed at build time and nothing
  more; upstream's `emit_audit` is post-execution and warn-and-continue by
  explicit design ("audit must never block or fail a tool call"), so a sink that
  became unwritable mid-session yielded commands that ran with **zero** record.
  That is not a nuisance — an attacker holding `shell` can fill the volume on
  purpose, so warn-and-continue is a way to *mint unaudited shell*.
  `AuditedShellTool` appends the command's intent line, fsynced, before the
  command runs, and refuses the call if that append fails. A full disk becomes a
  shell outage, which is the safe direction and the same choice boot already
  makes on an unwritable journal root.

**What is still not enforced, and must not be claimed otherwise.** The shell
path is untouched: same uid, same filesystem, no sandbox (§1). A deliberate `rm`
against the host-side sink still succeeds, and so does a `>` at it. This is
**not** tamper-evidence, and hash chaining or signed segments in-container would
not make it so — one uid and one process means the attacker holds both the chain
and any key. Genuine tamper-evidence needs an external sink and belongs with C1.
Durability is container-lifetime, deliberately: losing a journal record corrupts
*behaviour*, while losing an audit record loses *evidence*, and evidence that
must outlive the container needs somewhere else to live anyway.

What C6 actually buys, stated exactly: the sanctioned write paths refuse the
sink, and the intent line of the command that destroys it is on disk before it
runs.

## Skills are text an agent reads

A skill is a `SKILL.md` document an agent can list, describe and read. See
[`docs/modules/skills.md`](../../modules/skills.md) for the subsystem. It is
named here because `grep -ci skill` returned `0` for this file, and for
[grants.md](../company-brain/grants.md), [approvals.md](../company-brain/approvals.md)
and [tools.md](../runtime/tools.md), until now — not because skills were judged
safe, but because nobody had written the judgement down.

**A skill confers no capability.** It is inert text. The three tools that reach
one — `list_skills`, `describe_skill`, `read_skill_resource` — are declared
`EffectGroup::Other, Reach::Nothing` in
[`policy/consequence.rs`](../../../crates/opencompany-core/src/policy/consequence.rs),
the same bucket as any other local metadata read, and that classification is
correct: they read a materialized directory under the agent's own workspace and
touch nothing else. Skill *execution* is not wired at all
([`skills.md`](../../modules/skills.md#execution-is-deliberately-not-wired)), so
there is no path from installing a skill to running code.

**The content is untrusted all the same.** A registry- or upload-sourced
`SKILL.md` — body, description, and every bundled resource — was authored by
someone other than the operator, and it reaches an agent's context verbatim
through `describe_skill` and `read_skill_resource`, and its description reaches
the persona prompt through the catalogue. This document already treats a fetched
page and a remote MCP tool description that way. A skill is the same class of
input and was simply never named.

### The load-bearing sentence

**If a skill's text persuades an agent to take an action, that action still
crosses every gate it would have crossed anyway.** The tool call is resolved by
the three-level grant — `[tools].allow ∩ desk.tools ∩ agent.tools`
([tools.md](../runtime/tools.md)) — and whatever the company's approval policy
would do with the effect, it does the same whether or not a skill argued for it
([approvals.md](../company-brain/approvals.md)). What that is depends on the
mode, and today on little else: `readonly` denies applicable effects and the
emergency stop denies ahead of every policy rule, while policy HITL is disabled,
so `supervised`, `auto`, `full` and `always_approve` are classification and audit
data and an effect parks when an agent calls `request_approval`. A skill changes
none of that. It cannot widen a grant, add
a tool, name a credential, or mark an effect pre-approved. It has no field for
any of those, and nothing reads it as configuration.

That is the whole reason skills are lower-risk than MCP servers *today*: an MCP
server is a capability whose grant must be scoped, while a skill is an argument
an agent may find persuasive. The bottleneck is the tool call, and the tool call
is already gated.

### Installing one is configuration, not an effect

Every skill write — install, uninstall, toggle, author — is gated
`AdminScopedCompany`, the same category as adding a teammate or adding an MCP
server. It is an operator deciding something about the company, not an agent
reaching across the trust boundary mid-cycle.

Skill writes are therefore **not** `ApprovalGate` checkpoints, and that is
correct rather than an omission: the gate exists for effects an agent produces,
and an admin configuring their own company is not one. Nothing about the admin
route makes installing a skill a privilege grant either — the grant tables are
elsewhere and a skill never touches them.

### What is not true today

Two properties a reader might assume from the above are **not** in place:

- **A skill reaches every agent in the company.** There is no per-agent or
  per-desk scope on a skill; `materialize` takes no agent id, so an installed
  skill lands in every agent's catalogue on the next cycle. Tools get a
  three-level grant and skills get nothing analogous. Per-agent scope is in
  flight (issue #2213); until it merges, "install for one teammate" cannot be
  expressed.
- **Nothing inspects installed content.** Neither the registry install nor the
  console author path scans a document for instruction-manipulation patterns,
  invisible or bidirectional code points, hard-coded credentials, or references
  to other tools. An install-time scan is in flight on a parallel branch; until
  it merges, the only thing standing between a hostile `SKILL.md` and an agent's
  prompt is the frontmatter newline collapse, which stops key injection and
  nothing else.

When execution ships, none of this section survives unchanged. A skill that runs
is a process, and it inherits every gap in this document — no sandbox, no egress
policy, no uid separation. The prerequisites are stated with the deferral in
[`skills.md`](../../modules/skills.md#execution-is-deliberately-not-wired):
configuration injected rather than self-loaded, spend metered through the
harness, and egress bounded. That last one is C1, which is still open.

## Residual capability after every control in #752 lands

**Read this section as if C0–C5 are already done, because that is when it
matters.** It is written in the present tense on purpose: nothing below is
fixed by the work #752 tracks.

One container per tenant remains the only hard boundary. Inside a tenant, a
prompt-injected agent holding `shell` that can reach an allowlisted host can
still:

1. **Exfiltrate any repository content it can read, to an attacker-controlled
   repository on that same allowlisted git host.** Default-deny egress (C1)
   allowlists the git host — it has to, or checkout and publish stop working.
   The agent pushes to `github.com/attacker/drop` instead of
   `github.com/acme/widgets`. Same host, same port, same protocol, same TLS
   certificate authority. Nothing in the allowlist distinguishes them.
2. **Cause writes anywhere the credential's scope reaches**, under the write
   tier. The approval gate constrains what `repo_publish` may do; it does not
   constrain what `git push` may do from a shell that holds the token.
3. **Read every secret the tenant holds that is on the container's disk**, and
   every agent's workspace in that tenant.
4. **Destroy its own audit trail — but only by naming it.** Since C6 the sink is
   host-owned and outside the write sandbox, so the file tools refuse it and
   there is no longer an *accidental* or merely-permitted path to it. What
   remains is a deliberate destructive shell command aimed at a host-owned
   path, whose own intent line was fsynced before it ran. The record of the
   erasure survives the erasure; the erasure still works.

What C1–C5 actually buy: the number of hosts reachable drops from "the
internet" to "a handful", raw sockets stop working (C2), the repository
credential stops being a file on disk (C3), the filesystem an escaped process
sees shrinks (C4), and a stolen token expires in an hour and reaches one
repository (C5). Each of those raises cost and narrows a channel. **None of
them puts a boundary between the agent and the credential, or between the agent
and its egress.**

The honest one-line summary, which belongs in any report on this work: *we
narrowed the channels and shortened the credential's life; we did not contain
the shell.*

## An unresolved tension: egress policy versus the web tools

Default-deny egress (C1) and `[tools].web_allowed_domains` pull in opposite
directions, and both are load-bearing.

- C1 wants the pod to reach a short, fixed list: the git host, the model API,
  the TinyHumans backend, DNS. Everything else refused at the network layer.
- The web tools want broad public reach. An empty `web_allowed_domains` means
  *allow any public host*, and that is the default because a company doing
  research, competitive analysis or customer support is expected to read the
  web. Narrowing it to a per-company allowlist is a real product cost: an agent
  that cannot open the page it was asked about is not doing the job.

These cannot both be maximal. A pod-level allowlist that admits the public web
so `web_fetch` keeps working is a pod-level allowlist that admits the attacker's
collection endpoint, and C1 has bought approximately nothing. A pod-level
allowlist tight enough to be a boundary breaks `web_fetch` for every company
that has not enumerated its domains in advance.

There is no resolution recorded here because none has been decided. The shapes
worth considering, none of them free:

- Route the web tools through an egress proxy inside the allowlist, and enforce
  the per-company domain list at the proxy rather than in-process. Moves the
  decision to a component the agent cannot talk around, and adds a component.
- Split the tiers: companies that hold a repository credential get the tight
  allowlist and lose open web; companies that want open web do not get `repo`.
  Honest, and it makes the two capabilities mutually exclusive.
- Accept broad egress and stop describing C1 as containment — treat it as
  blocking raw sockets and non-HTTP protocols only.

Whoever closes C1 must state which of these they chose, and what it costs.

## What must never be claimed

Not in a commit message, not in a PR body, not in a release note, not in this
directory:

- "The shell is sandboxed." It is not. No sandbox mode is set.
- "Egress is locked down", without naming the allowlist and conceding that
  exfiltration to an allowlisted host is unaffected.
- "The agent cannot reach the credential." On `fs` it is a file it can read; on
  `mongodb` it is in a process it is running inside.
- "The audit log is tamper-proof", or tamper-evident, or protected. C6 moved it
  out of the write sandbox and made an unrecordable command refuse to run. The
  shell can still delete the file. Say *that*.
- "#752 is fixed." #752 cannot be closed by work in this repo. C3 and C4 are
  the in-repo children; the load-bearing ones are C1, C2 and C5, and they are
  in `opencompany-microservice`.
- Any claim about egress justified by a passing test in this repository. No
  test here can demonstrate that a packet was blocked.
- "Skills are sandboxed", or "a skill runs in a sandbox". Nothing sandboxes a
  skill. Today one is never executed at all, and the day execution ships it will
  run under exactly the controls above.
- "Installing a skill is gated, so its content is trusted." The gate is on the
  operator's write, not on the document. See
  [Skills are text an agent reads](#skills-are-text-an-agent-reads).

## The children of #752

| ID | Scope | Repo | State |
| --- | --- | --- | --- |
| C0 | This document | opencompany | done |
| C1 | Default-deny egress for tenant pods | `opencompany-microservice` / k8s | open — **highest priority** |
| C2 | Pod `securityContext`: seccomp `RuntimeDefault`, drop all caps including `NET_RAW`, no privilege escalation | `opencompany-microservice` / k8s | open |
| C3 | Fail closed on the fs secret backend for `repo` | opencompany | done |
| C4 | Wire `cwd_jail` Landlock for the agent shell, plus a CI lane that builds the feature | opencompany | open — secondary |
| C5 | Short-lived single-repository installation tokens instead of a long-lived write PAT | `opencompany-microservice` | open — before the write tier ships broadly |
| C6 | Move the shell audit sink out of the agent workspace, and refuse a command whose intent could not be recorded | opencompany | done (issue #775) — see §6 for what it did *not* buy |

The write tier (#247, #734–#738) is a separate line of work, not a child of
this one — but see the precondition above.

## Related

- [storage.md](../runtime/storage.md) — backend selection, and the repository
  credential requirement C3 added.
- [ports-state.md](../runtime/ports-state.md) — the `SecretStore` contract.
- [company-brain/approvals.md](../company-brain/approvals.md) — the approval
  model the publish gate sits in.
- [../../modules/skills.md](../../modules/skills.md) — the skills subsystem: what
  a skill is, how the effective set is resolved, and why execution is deferred.
