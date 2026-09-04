# WorkspaceStore (WS3 console-surface stores)

Split out of [`ports-console.md`](ports-console.md), which was over the
repository's 500-line ceiling. Part of the port contracts indexed by
[ports.md](ports.md).

### WorkspaceStore

The Obsidian-style note tree (`src/ports/workspace.rs`), seeded from the
company's `workspace/**` on first use and thereafter written by both the
operator (console/REST) and the company's agents (`workspace_create` /
`workspace_write`, plus `workspace_rename` / `workspace_delete` within
`agents/<agent-id>/` since issue #671).

```rust
pub trait WorkspaceStore: Send + Sync {
    async fn tree(&self, company: &CompanyId) -> Result<Vec<WorkspaceNode>>;
    async fn read(&self, company: &CompanyId, id: &str)
        -> Result<Option<(WorkspaceNode, String)>>;
    async fn read_capped(&self, company: &CompanyId, id: &str, max_bytes: u64)
        -> Result<Option<(WorkspaceNode, String, u64)>>;
    async fn write(&self, company: &CompanyId, id: &str, content: &str,
                   author: WorkspaceOrigin) -> Result<WorkspaceNode>;
    async fn create(&self, /* parent, name, kind, content */) -> Result<WorkspaceNode>;
    async fn adopt_or_create_folder(&self, company: &CompanyId, parent: Option<&str>,
                                    name: &str, origin: WorkspaceOrigin)
        -> Result<FolderClaim>;                  // Created | Adopted  (#759)
    async fn rename_move(&self, /* id, new_name, new_parent */) -> Result<WorkspaceNode>;
    async fn delete(&self, company: &CompanyId, id: &str) -> Result<bool>;
    async fn is_empty(&self, company: &CompanyId) -> Result<bool>;

    // Bytes (#553) — additive; the text path above is untouched.
    async fn create_binary(&self, company: &CompanyId, node: &WorkspaceNode,
                           bytes: &[u8]) -> Result<()>;
    async fn write_binary(&self, company: &CompanyId, id: &str, bytes: &[u8],
                          mime: Option<&str>, author: WorkspaceOrigin)
        -> Result<WorkspaceNode>;
    async fn read_bytes(&self, company: &CompanyId, id: &str)
        -> Result<Option<(WorkspaceNode, BlobStream)>>;
}
```

Nodes are folders or files (`NodeKind`); `[[wikilink]]` backlinks are derived
at read time by the GraphQL layer.

**No torn reads (#887).** A `read` concurrent with a `write` on the same node
returns one whole revision or the other — never an error, and never a prefix.
sqlite and mongodb get this from a row update and a document replace; `fs` gets
it from a tmp-file-plus-rename (`store::fs::write_atomic{,_bytes}`), which is
what it was missing. `conformance::assert_workspace_read_never_tears` holds all
three to it. The half that made this a data-integrity bug rather than a noisy
one: a prefix that happens to end on a codepoint boundary *decodes cleanly*, so
an agent grounds its answer in half a document with nothing failing anywhere.

**Bytes (#553).** A node holds prose or it holds a payload, never both. A
binary node is a `File` whose `mime`, `size` and `sha256` are all `Some`;
`mime` alone is the discriminator every surface keys off. `size` and `sha256`
are computed **by the store** from the bytes it persists (`blob_metadata`) and
are never accepted from a caller — a digest a caller supplies is an unverified
claim, which is the one thing a digest must not be.

The text path stays `String`, deliberately: ~17 call sites reference this port
and none of them (the backlink scan, the seeder, the GraphQL projection, the
agent tools) can do anything with a PNG. So a text `read` of a binary node
yields an **empty body**, the same answer a folder gives, and a text `write` to
one is refused rather than allowed to leave the recorded digest describing
bytes that are gone.

`read` has no ceiling, which is fine for every caller that wants the whole note
and wrong for one that will discard anything over a cap: it would have to
materialise the body to learn it must throw it away. `read_capped` is that
caller's read — it answers the body's true byte length and hands back the body
only while it fits, so the store withholds what would be discarded instead of
transferring it. Each backend measures in its own way (a `stat`, SQL
`length(CAST(content AS BLOB))`, an aggregation's `$strLenBytes`) and
`conformance::assert_workspace_read_capped` holds all three to the one contract.
A decorator forwards it; falling back to `read` would spend exactly the
allocation the cap exists to prevent. The binary half has never needed this —
`size` rides the node, so a caller decides on metadata and `read_bytes` is never
reached for an over-cap payload.

Writes buffer and reads stream. The asymmetry is what makes the quota
enforceable: `QuotaEnforcedWorkspace` (`src/runtime/workspace_quota.rs`) wraps
the store at the single assembly site, inside the announcer, and sees the full
size **before** anything is written — so a refused write leaves no partial
blob, no node and no orphan. It meters payloads only (prose is uncounted; the
threat model is media) against a per-file cap (`[workspace] max_blob_mb`,
default 64 MiB) and a per-company total (`[workspace] tree_quota_gb`,
unlimited by default), answering 413. The console's upload route holds a
separate 256 MiB body limit — a backstop against buffering an unbounded
request, deliberately above the cap so the store's refusal is the one an
operator sees (#647); see `storage.md`.

Backends: sqlite keeps the blob in the node's own row, so it cannot orphan one.
`FsOps` writes the file then the index — the benign order the text path already
uses. MongoDB needs **GridFS** (a payload cannot ride in a 16 MB BSON document)
and writes blob-first/document-second, deleting in the mirror order, so a crash
strands only a blob nothing references; `MongoStore::from_database` sweeps those
at boot. Tenancy in the shared bucket is a filter on `metadata.company_id` **and**
`metadata.node_id` for every read, delete and sweep.

**Folder claims (#759).** `adopt_or_create_folder` is the only way the publish
walk and the system-root scaffold create a folder. It is a store primitive rather
than a read plus a `create` because the read is honest about one instant and the
create acts on it later: two publishes needing `artifacts/<agent>/<task>/` both saw
it free, and sqlite and mongodb both created — leaving two folders under one
name. That state does not decay. Path resolution answers a duplicated name with
`Conflict`, so a race lasting microseconds refuses every later publish beneath
that path, for every agent, permanently.

Afterwards exactly one folder answers to `(parent, name)` among the nodes this
primitive governs, and the returned node is it. A `parent` of `None` is the
workspace root, so the roots are claimed by the same call as everything below
them. Adoption **preserves the original authorship stamp**: `origin` is used only
when this call is the one that creates. A file holding the name, or a
pre-existing ambiguity, is a `Conflict` — fail-closed, exactly as before.

`FolderClaim::{Created, Adopted}` exists for one consumer: `WorkspaceAnnouncer`
emits its node-created frame **only** on `Created`. A frame for a folder that was
already standing would tell an open console something appeared when nothing did,
on the adoption path nearly every publish takes.

It is deliberately not `swap_files`' compare-and-swap. That stages a payload and
makes the loser *fail*, which is right for a file — its bytes are a content claim
with one legitimate winner. A folder is a payload-free container claim: two
publishers wanting one folder want the same thing, so the loser must adopt and
carry on. That also dissolves loser cleanup, since a loser never owns a folder.

Backends decide it where each of them can: `FsOps` under the workspace-index lock
it already holds for every write (single-process per data dir by contract),
sqlite inside a `TransactionBehavior::Immediate` transaction (two stores can open
one file), and MongoDB with a second partial unique index — see `storage.md`. The
agent's own `workspace_create` tool is **not** routed through this: adopting there
would silently merge two intentionally separate hand-made folders.

**Search (#607).** Workspace search is a **company-layer helper**
(`company::workspace_search`) over the port's existing `tree` + `read`, not a
port method. A trait method would need five implementations *and* one agreed
definition of matching across three engines, and SQLite's FTS5 tokeniser,
MongoDB's `$text` stemming and a hand-rolled filesystem scan cannot be made to
agree without either forbidding the native indexes or accepting that the same
query answers differently per deployment. The helper is correct on all three
backends by construction; the cost it accepts is O(N) reads per query, the same
shape `workspace_links` already pays on every file open, and it is the named
place to add an index later.

A binary node is **matched by name and never content-scanned**. Its bytes are
not text, so scanning them would produce mojibake matches, waste I/O
proportional to the payload, and — on a streaming backend — pull a whole video
through memory to find nothing.

The port already makes the safe behaviour the default rather than a rule to
remember: a text `read` of a binary node returns an **empty body** on all three
backends, so a content scan built over `read`/`tree` finds nothing for one
automatically. It does not need to know binaries exist, and it cannot
accidentally index them. The helper states the rule anyway rather than
inheriting the silence, because "found nothing" and "not searchable this way"
are different answers and only one of them is true. A scan that wants to *say*
something about a payload uses `mime`/`size`/`sha256` off the node, which the
tree read already carries — the same fields `workspace_list` renders.
`read_bytes` is for serving a download and appears nowhere in a search path.

Paths in a hit come from `company::workspace_paths`, the same ancestor-chain
rules the agent tools' `PathIndex` uses, so a node search offers is always a
node `workspace_read` can open — an unaddressable node (dangling ancestor,
illegal name segment) is excluded from hits and from `total` alike.

`assert_workspace_binary_store` pins all of it across all three backends,
including a 17 MiB case that proves the BSON cap is not in play. Over HTTP:
`GET …/workspace/blob/{id}` streams the payload (`ETag` = sha256) and
`POST …/workspace/upload` takes multipart; a text-typed upload whose bytes
decode as UTF-8 is stored as a note instead, so an uploaded `.md` keeps its
editor and backlinks.

The blob route is the download of any **file** in the tree, so a prose note is
served from its body under the same neutralised headers — a note has no stored
digest, so it carries no `ETag`, and its `Content-Length` is the body's byte
length. A folder and an id naming nothing 404 identically.

An upload is still bounded by the company's `max_blob_mb` before that
text/binary choice (#665). The bound is a property of bytes entering through
the upload route, not their eventual representation; ordinary text writes stay
unmetered and text nodes still do not contribute to `tree_quota_gb`.

The filesystem backend mirrors logical names as real paths, so it refuses a
create, rename, or move that would land on a path another node already holds
(#666). It compares the whole resolved name chain rather than the sibling name,
because a tree can legitimately hold two folders under one name — the scaffold
finds such roots, declines to resolve them and leaves them standing — and their
equally-named children are not siblings by `parent_id` yet still resolve to one
path. The check runs under the workspace-index lock: a refused upload leaves the
existing node, payload, size, and digest unchanged. Id-keyed database backends
can continue to represent duplicate names without aliasing payloads.

**A stored `mime` does not decide how the blob route serves it (#667).** That
value is the uploader's declared `Content-Type`, or `mime_guess` on a published
deliverable's filename — a claim, not a property of the bytes — and the route is
same-origin with the console's `SameSite=Lax` session cookie, which a top-level
navigation sends. So `read_blob` classifies against a **closed list** instead:

| stored mime | served as | disposition |
| --- | --- | --- |
| raster image (`png`, `jpeg`, `gif`, `webp`, `avif`, `bmp`, `tiff`, `apng`, icon) or `application/pdf` | unchanged | `inline` |
| `image/svg+xml` | unchanged | `attachment` |
| anything else, including absent | `application/octet-stream` | `attachment` |

Every response also carries `X-Content-Type-Options: nosniff`, without which the
forced type is a suggestion. SVG is split out rather than folded into either
neighbour because it is an image *and* a document: an `<img>` will not decode it
without `image/svg+xml`, and inside an `<img>` the SVG spec's secure static mode
runs no script — but the same bytes at the top of a tab are a scripted document
on this origin. Keeping the type and forcing `attachment` serves both.

This lives on the **read** path deliberately. Sanitising on upload would leave
every payload already stored under a caller-chosen mime exploitable; classifying
at serve time covers those too. The console is unaffected either way — it fetches
blobs through `getBlob` and wraps them in an object URL, and `fetch` ignores
`Content-Disposition` entirely.

A repo-wide Content-Security-Policy is **not** part of this and is still absent;
its lack is not specific to this route and closing this one does not close that.

**Authorship (#326).** Every `WorkspaceNode` carries `created_by` and
`updated_by`, both a `WorkspaceOrigin` ∈ `seed | operator | agent{id}`. `write`
takes the author and stamps `updated_by`; `create` receives a fully-formed node
whose caller sets both. `rename_move` deliberately touches **neither**, so an
operator reorganising the tree cannot mask who wrote the body that is stored.
Backends persist the node as opaque JSON and both fields are
`#[serde(default)]`, so a node written before the fields existed loads as
`operator` — no column, no migration. `assert_workspace_store` pins the
round-trip, the write stamp, and the rename non-stamp across all three
backends.

**System workspace roots.** `company::workspace_scaffold` owns `agents/`,
`artifacts/`, `desks/`, and the operator-only `secrets/` subtree on different
schedules:

* `ensure_workspace_scaffold` adopt-or-creates the roots listed in
  `SYSTEM_ROOTS` — `agents`, `artifacts` and `secrets` — from one seam:
  `RuntimeBuilder::build` (boot). It takes no roster (a company with no agents
  still gets them), so an existing company picks them up on its next boot.
  `agents/` stays empty; `secrets/readme.md` explains that every agent workspace
  tool omits or refuses this subtree while operator APIs retain normal access,
  and `artifacts/readme.md` explains that an empty deliverable list is a real
  outcome rather than a gap. Idempotent.
* `ensure_agent_folder` / `ensure_artifact_folder` / `ensure_desk_folder`
  adopt-or-create `agents/<agent-id>/`, `artifacts/<agent-id>/` and
  `desks/<desk-id>/` **on demand**, returning the node id, called when that agent
  or desk first produces something. A folder means
  "this member produced something"; an eager folder per roster member would be
  a claim the tree cannot back. `workspace_create` calls the agent minter when
  an agent writes into its own home; #552's publish path calls the artifact
  minter.
* **`desks/` is not scaffolded** (#645). Both minters have always created an
  absent root on their way down, and `ensure_desk_folder` has no callers yet, so
  scaffolding the root gave every company a permanently empty folder
  advertising a feature nothing fills — the same promise-not-record shape #570
  removed at the member level. `ensure_desk_folder` now mints root and member
  folder together on first use. A `desks/` that already exists is untouched:
  the scaffold only ever looks up the names in `SYSTEM_ROOTS`, so a legacy
  company keeps its root, its contents and its authorship, un-warned.

Both are fail-closed: a name collision (a *file* of that name, or several nodes
sharing it) is never resolved by creating a duplicate that would make the path
permanently ambiguous. The scaffold warns and skips, since nothing waits on its
result; a minter returns the collision as an error, since its caller needs the
id. The agent/desks folders are organizational and attribution units only;
agents may create and write ordinary shared content anywhere. `secrets/` is
the operator-only exception on the workspace tool surface.

