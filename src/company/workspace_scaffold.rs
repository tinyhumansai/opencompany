//! The workspace's system roots — `agents/`, `desks/`, and `secrets/` — and the
//! content the runtime owns beneath them.
//!
//! Before this module an agent had nowhere in the shared tree that was
//! recognisably *its own*: everything it produced landed in its private
//! per-agent sandbox or on a task artifact, neither of which the operator or
//! another agent browses. `agents/` gives each roster member a named place in
//! the one tree both sides read, so "where did the CMO put the launch brief"
//! has an answer a human can navigate to. `desks/` is the same idea one level
//! up, for work a desk produces rather than one teammate (issue #552 wires the
//! producer).
//!
//! # One eager root, everything else lazy
//!
//! Provisioning runs on deliberately different schedules, and the line between
//! them is whether anything actually writes there yet:
//!
//! * `agents/` is scaffolding. [`ensure_workspace_scaffold`] lays it down on
//!   every boot, empty, whether or not the company has a roster — it is part of
//!   what a workspace *is*, the same way the template-seeded `playbooks/` and
//!   `standards/` are, and it has a real producer behind it: the persona brief
//!   steers every agent to write beneath it, so an operator opening the
//!   Workspace tab on a brand-new company is being shown where things are about
//!   to appear rather than a void.
//! * `desks/` is **not** scaffolded (issue #645). It was, until it turned out
//!   nothing writes into it: issue #552's publish path is still unwired, so
//!   [`ensure_desk_folder`] has no callers and every company carried a
//!   permanently empty root advertising a feature it does not yet have. An
//!   eager root nobody fills is the same promise-not-record mistake as an eager
//!   folder per roster member, one level up. It is therefore minted *whole* —
//!   root and member folder in one call — by [`ensure_desk_folder`], so it
//!   appears exactly when a desk first has something to put in it.
//! * `secrets/` is operator-only scaffolding. It is laid down eagerly with a
//!   `readme.md` explaining that agent workspace tools omit the entire subtree.
//! * A **member folder** was never scaffolding either; it is a container for
//!   something. `agents/<agent-id>/` and `desks/<desk-id>/` are minted on demand
//!   — by [`ensure_agent_folder`] / [`ensure_desk_folder`], at the moment that
//!   agent or desk first produces a task, artifact or note. An eager folder per
//!   roster member fills the tree with empty directories for teammates who have
//!   never done anything, which is noise that grows with the roster and tells
//!   the operator nothing.
//!
//! Dropping the root changes nothing about how a desk folder is reached: the
//! minter has always created an absent root on its way down, so it is the same
//! one call it always was.
//!
//! # What this is, and what it very deliberately is not
//!
//! It is an **organizational and attribution unit**, identified by path. It is
//! **not** a permission boundary. Agents write anywhere in the tree — that is
//! the settled design (a `workspace_write` has always been able to overwrite
//! any note, and gating *create* while *overwrite* stays free would protect
//! nothing, since overwriting is the strictly more destructive of the two).
//! What keeps the tree tidy is steering — the persona brief names
//! `agents/<your id>/` as the default home for anything an agent produces —
//! plus the authorship stamps from issue #326, which make it visible after the
//! fact who put what where. Containment lives one level up, in company tenancy,
//! the explicit `workspace` write grant, the CAS token, and policy parking.
//!
//! # Fail-closed adoption
//!
//! Identity is by path, and nothing in the [`WorkspaceStore`] port enforces
//! unique sibling names, so every lookup here is check-then-act. Ambiguity
//! always resolves the same way: **never guess and never overwrite**.
//!
//! * Exactly one folder carrying the name → adopt it as-is, authorship and all.
//! * A *file* carrying the name, or several nodes carrying it → refuse to touch
//!   it. Creating a rival would make the path permanently ambiguous, which the
//!   tool layer's resolver then refuses for every agent (see
//!   `harness::workspace_tools`).
//!
//! How a refusal is *reported* differs by caller, because the callers differ:
//!
//! * [`ensure_workspace_scaffold`] runs at boot with nobody waiting on a
//!   result, so it warns and skips — a convenience folder must not take down a
//!   boot, and the next boot retries. A tree read that fails still propagates:
//!   that is the store being broken, not the tree being odd.
//! * [`ensure_agent_folder`] / [`ensure_desk_folder`] are called *by a producer
//!   that needs the id back*, so there is nothing honest to fail soft to. They
//!   return the collision as an error and let the caller decide.
//!
//! Every function here is idempotent, which is what lets the scaffold run on
//! every boot and a minter run on every publish without accumulating anything.

use crate::Result;
use crate::error::OpenCompanyError;
use crate::ports::types::CompanyId;
use crate::ports::workspace::{NodeKind, WorkspaceNode, WorkspaceOrigin, WorkspaceStore};
use crate::ports::{generate_id, now_millis};

use super::workspace_names::kebab_name_or;

/// The reserved root folder holding one subfolder per agent that has produced
/// something.
///
/// A literal, because identity here is by path: this is the name the persona
/// brief tells agents to look for and the name issue #552's published
/// deliverables land under.
///
/// Lowercase, like every other name the runtime mints — see
/// [`workspace_names`](super::workspace_names). It was `Agents`, and a company
/// that ran an older build still has that folder: [`find`] matches a root name
/// case-insensitively so the legacy spelling is *adopted* rather than joined by
/// a lowercase twin, which would split one agent's home in two.
pub const AGENTS_ROOT: &str = "agents";

/// The reserved root folder holding one subfolder per desk that has produced
/// something.
///
/// Not scaffolded at boot — see [`SYSTEM_ROOTS`] and [`ensure_desk_folder`].
pub const DESKS_ROOT: &str = "desks";

/// The reserved root folder holding one subfolder per agent-authored
/// dashboard page: `pages/<slug>/`.
///
/// Not scaffolded at boot, for the same reason [`DESKS_ROOT`] is not: nothing
/// writes here until an agent creates its first page. Named here — rather
/// than only inside `harness::pages_tools`, which compiles only under the
/// `openhuman` feature — because [`crate::server::ops::pages`] (always
/// compiled) needs the identical root name to serve what
/// `harness::pages_tools::pages_tools` wrote. One literal, two callers.
pub const PAGES_ROOT: &str = "pages";

/// The page manifest node's name inside `pages/<slug>/`.
pub const PAGE_MANIFEST_NAME: &str = "page.toml";
/// The page source node's name inside `pages/<slug>/`.
pub const PAGE_SOURCE_NAME: &str = "page.tsx";
/// The compiled page node's name inside `pages/<slug>/`.
pub const PAGE_COMPILED_NAME: &str = "page.compiled.mjs";
/// The mime [`crate::server::ops::pages`] serves [`PAGE_COMPILED_NAME`] as.
pub const PAGE_COMPILED_MIME: &str = "application/javascript";

/// The operator-only workspace subtree.
///
/// Agents never receive this root or anything beneath it through their
/// workspace list, read, search, or write tools. The operator surfaces still
/// use the full workspace store, so notes here remain ordinarily browsable and
/// editable in the console.
pub const SECRETS_ROOT: &str = "secrets";

/// The name of the note provisioned inside [`SECRETS_ROOT`] on first boot.
///
/// Lowercase for the same reason every other minted name is — and matched
/// case-insensitively by [`find`], so a legacy `README.md` is adopted rather
/// than joined by a second copy.
pub const SECRETS_README_NAME: &str = "readme.md";

/// The note provisioned inside [`SECRETS_ROOT`] on first boot.
pub const SECRETS_README: &str = "# Workspace secrets\n\nStore private operator notes and secret values in this folder. Everything under `secrets/` is hidden from agent workspace tools, including listing, reading, searching, and writing. Operators can still browse and edit these notes in the Workspace view.\n\nDo not treat this folder as an application credential store: use the Connections and inference settings for credentials that OpenCompany must inject into tools or providers.\n";

/// The reserved root folder holding every deliverable an agent published:
/// `artifacts/<agent-id>/<task-id>/<source…>`.
///
/// Scaffolded eagerly, unlike [`DESKS_ROOT`], because it has a producer wired
/// today — [`crate::company::artifact_mirror::materialize`] files every
/// `publish_artifact` beneath it — and because the question it answers ("what
/// has this company actually produced?") is one an operator asks before the
/// first answer exists. An empty `artifacts/` with a note saying what will
/// appear there is a better answer than no folder at all.
///
/// Deliverables used to land under `{AGENTS_ROOT}/<agent-id>/…`, which filed
/// them by *who* rather than by *what*: an agent's folder is also its scratch
/// home, so a published spec sat in the same list as its working notes and
/// neither the operator nor another agent could tell which was which. Filing by
/// kind first and author second keeps the attribution — the agent id is still
/// the next segment — and makes the deliverable list a place rather than a
/// query.
///
/// **A projection, not the record.** The artifact chain is authoritative and
/// holds the version history; a node here carries the current body only. See
/// [`crate::company::artifact_mirror`].
/// Lowercase, like every other name the runtime mints — see
/// [`workspace_names`](super::workspace_names). [`find`] matches a root name
/// case-insensitively, so a company that ran a build spelling it `Artifacts`
/// adopts that folder rather than gaining a lowercase twin beside it.
pub const ARTIFACTS_ROOT: &str = "artifacts";

/// The name of the note provisioned inside [`ARTIFACTS_ROOT`] on first boot.
///
/// Lowercase, and matched case-insensitively by [`find`], for the same reasons
/// [`SECRETS_README_NAME`] is.
pub const ARTIFACTS_README_NAME: &str = "readme.md";

/// The note provisioned inside [`ARTIFACTS_ROOT`] on first boot.
pub const ARTIFACTS_README: &str = "# Deliverables\n\nEvery file an agent published with `publish_artifact`, filed as `artifacts/<agent>/<task>/<path>`. A run that published nothing leaves nothing here — that is a real outcome, not a gap: plenty of work (a question answered, a check run) produces no file.\n\nEach note here is the **current** body of a deliverable. Its version history, and who revised each version, live on the artifact record the card's Artifacts tab shows.\n\nEditing a note here is recorded against that history as a human edit, which is deliberate — the gap between what the agent produced and what a person had to fix is the point. Do not hand-create files here: nothing links them to a deliverable, and they will read as artifacts that are not.\n";

/// The system roots the runtime lays down eagerly, on every boot.
///
/// Deliberately *not* derived from the manifest: `agents/` exists because a
/// workspace has it, not because a particular company has agents.
///
/// [`DESKS_ROOT`] is deliberately absent (issue #645). Nothing writes into it
/// yet, so scaffolding it gave every company a permanently empty root; it is
/// minted on first use instead. It is a root either way — this list is about
/// *when* a root appears, not which names are reserved. [`ARTIFACTS_ROOT`] is
/// present on the other side of exactly that test: it has a producer wired
/// today, so an operator who opens it before the first publish is being shown
/// where deliverables are about to appear rather than a void.
///
/// Kept an array, and kept public, so a caller that has to tell scaffolding
/// apart from content — the re-seed tests, a future console filter — can ask
/// rather than hard-code the names, and so promoting a root back to eager stays
/// a one-line change.
pub const SYSTEM_ROOTS: [&str; 3] = [AGENTS_ROOT, ARTIFACTS_ROOT, SECRETS_ROOT];

/// Whether a logical workspace path belongs to the operator-only subtree.
///
/// This is case-insensitive on the root segment so a colliding `Secrets` node
/// cannot become an accidental agent-visible twin. Descendants are tested by
/// segments rather than string prefix, so `secrets-old/` remains ordinary
/// shared workspace content.
pub fn is_agent_hidden_path(path: &str) -> bool {
    path.trim()
        .trim_start_matches('/')
        .split('/')
        .next()
        .is_some_and(|root| root.eq_ignore_ascii_case(SECRETS_ROOT))
}

/// Adopt-or-create the eagerly-scaffolded roots ([`SYSTEM_ROOTS`]) for
/// `company`.
///
/// Two `tree()` reads (the second resolves the README parent), then only the
/// creates that are actually missing. Safe to
/// call on every boot: it depends on nothing but the company id, so an existing
/// company picks the roots up the next time it starts.
///
/// What it creates is stamped [`WorkspaceOrigin::Seed`] — scaffolding the
/// runtime lays down, authored by no operator and no agent. `secrets/` receives
/// one explanatory `readme.md`; member folders beneath `agents/` remain lazy
/// (see [`ensure_agent_folder`]). `desks/` is not created here at all, and an
/// existing one is never even looked at: this walks
/// [`SYSTEM_ROOTS`] by name, so a legacy company's `Desks/` (from before issue
/// #645, or hand-made by an operator) is left exactly as it stands, contents
/// and authorship included.
///
/// Errors from the tree read propagate; a failed or ambiguous *create* warns
/// and moves on, and the next boot retries it.
pub async fn ensure_workspace_scaffold(
    store: &dyn WorkspaceStore,
    company: &CompanyId,
) -> Result<()> {
    let nodes = store.tree(company).await?;

    for root in SYSTEM_ROOTS {
        // Each root is resolved independently, so one colliding name never
        // withholds another. The loop is the contract rather than an accident
        // of arity — it read the same when there were two.
        match find(&nodes, None, root) {
            Found::Folder(_) => {}
            Found::Collision(why) => tracing::warn!(
                company = %company,
                "[workspace] {why}; not provisioning the `{root}` root"
            ),
            Found::Free => {
                // Through the store primitive, so a boot racing a publish (or
                // two tenant replicas booting together) adopts rather than
                // duplicating the root — see `ensure_member_folder`. The
                // warn-and-continue reporting is unchanged: a convenience folder
                // must not take down a boot.
                if let Err(e) = store
                    .adopt_or_create_folder(company, None, root, WorkspaceOrigin::Seed)
                    .await
                {
                    tracing::warn!(
                        company = %company,
                        error = %e,
                        "[workspace] could not create the `{root}` root; will retry on the next boot"
                    );
                }
            }
        }
    }

    // Both `secrets/` and `artifacts/` are useful before anything is in them,
    // and each note explains its own boundary at the place an operator meets
    // it: what agents cannot see, and what "no deliverables" actually means.
    // Refresh the tree after claiming roots so a newly-created root has an id to
    // parent its note beneath. As with the roots, collisions fail closed and
    // retry later.
    //
    // A table rather than two copies of the block: the two notes differ only in
    // which root they sit under and what they say, and a second hand-written
    // copy is where the divergence starts.
    let nodes = store.tree(company).await?;
    for (root, note, body) in [
        (SECRETS_ROOT, SECRETS_README_NAME, SECRETS_README),
        (ARTIFACTS_ROOT, ARTIFACTS_README_NAME, ARTIFACTS_README),
    ] {
        let root_id = match find(&nodes, None, root) {
            Found::Folder(id) => id,
            Found::Collision(why) => {
                tracing::warn!(
                    company = %company,
                    "[workspace] {why}; not provisioning `{root}/{note}`"
                );
                continue;
            }
            Found::Free => continue,
        };
        match find(&nodes, Some(root_id.as_str()), note) {
            Found::Folder(_) | Found::Collision(_) => tracing::warn!(
                company = %company,
                "[workspace] `{root}/{note}` is not one unambiguous note; leaving it untouched"
            ),
            Found::Free => {
                let readme = WorkspaceNode {
                    id: generate_id(),
                    name: note.to_string(),
                    kind: NodeKind::File,
                    parent_id: Some(root_id),
                    updated_at_millis: now_millis(),
                    created_by: WorkspaceOrigin::Seed,
                    updated_by: WorkspaceOrigin::Seed,
                    mime: None,
                    size: None,
                    sha256: None,
                    adopted: false,
                };
                if let Err(error) = store.create(company, &readme, Some(body)).await {
                    tracing::warn!(
                        company = %company,
                        %error,
                        "[workspace] could not create `{root}/{note}`; will retry on the next boot"
                    );
                }
            }
        }
    }

    Ok(())
}

/// Adopt-or-create `agents/<agent_id>/`, returning its node id.
///
/// The lazy half of the feature: call this at the moment `agent_id` first
/// produces something that needs a home, not when it joins the roster. Creates
/// the `Agents` root too if the scaffold has not run (or could not create it),
/// so one call is enough to get a usable parent id.
///
/// The folder is stamped [`WorkspaceOrigin::Agent`] for the agent it belongs
/// to, so the console can say whose folder it is without parsing the path.
///
/// Idempotent: a second call on the same agent returns the same id and creates
/// nothing.
pub async fn ensure_agent_folder(
    store: &dyn WorkspaceStore,
    company: &CompanyId,
    agent_id: &str,
) -> Result<String> {
    Ok(ensure_agent_folder_tracked(store, company, agent_id)
        .await?
        .0)
}

/// [`ensure_agent_folder`], additionally reporting whether *this* call minted
/// the member folder (issue #1801).
///
/// The bool is `true` only when `agents/<agent_id>/` did not exist and this call
/// created it; an adopted or already-present folder answers `false`. A caller
/// that has to undo a half-finished operation — a note create that then failed,
/// which would otherwise leave the freshly-minted home standing empty — uses it
/// to know which folder it, and only it, brought into existence, so
/// [`rollback_empty_minted_folders`] can remove that folder and nothing it
/// merely found. The eagerly-scaffolded root is deliberately never reported: an
/// empty `agents/` is ordinary boot scaffolding, not this call's to undo.
pub async fn ensure_agent_folder_tracked(
    store: &dyn WorkspaceStore,
    company: &CompanyId,
    agent_id: &str,
) -> Result<(String, bool)> {
    let agent_id = agent_id.trim();
    ensure_member_folder(
        store,
        company,
        AGENTS_ROOT,
        agent_id,
        WorkspaceOrigin::Agent {
            id: agent_id.to_string(),
        },
    )
    .await
}

/// Adopt-or-create `artifacts/<agent_id>/`, returning its node id.
///
/// The deliverable half of [`ensure_agent_folder`], and lazy for the same
/// reason: a teammate that has published nothing gets no folder, so the list
/// under `artifacts/` is a record of who has produced something rather than a
/// copy of the roster. The root above it *is* eager (see [`SYSTEM_ROOTS`]) —
/// the root says the company has somewhere to put deliverables, a member folder
/// says this teammate delivered.
///
/// Stamped [`WorkspaceOrigin::Agent`] for the publishing agent, so the console
/// can attribute the folder without parsing the path.
///
/// Idempotent: a second call on the same agent returns the same id and creates
/// nothing.
pub async fn ensure_artifact_folder(
    store: &dyn WorkspaceStore,
    company: &CompanyId,
    agent_id: &str,
) -> Result<String> {
    Ok(ensure_artifact_folder_tracked(store, company, agent_id)
        .await?
        .0)
}

/// [`ensure_artifact_folder`], additionally reporting whether *this* call minted
/// the member folder (issue #1801) — the deliverable twin of
/// [`ensure_agent_folder_tracked`], with the same contract on the bool.
pub async fn ensure_artifact_folder_tracked(
    store: &dyn WorkspaceStore,
    company: &CompanyId,
    agent_id: &str,
) -> Result<(String, bool)> {
    let agent_id = agent_id.trim();
    ensure_member_folder(
        store,
        company,
        ARTIFACTS_ROOT,
        agent_id,
        WorkspaceOrigin::Agent {
            id: agent_id.to_string(),
        },
    )
    .await
}

/// Adopt-or-create `desks/<desk_id>/`, returning its node id.
///
/// [`ensure_agent_folder`]'s counterpart for a desk — call it when a desk first
/// produces an artifact. Nothing calls it yet; issue #552's publish path is the
/// first producer.
///
/// Unlike `agents/`, the `desks/` root is not scaffolded at boot (issue #645),
/// so this mints the root as well when it is missing. That is the point rather
/// than a fallback: `desks/` appears the first time a desk has something to put
/// in it, instead of standing empty in every company that never uses one.
///
/// Both the root and the member folder are stamped [`WorkspaceOrigin::Seed`]
/// rather than an author, because a desk is not one: [`WorkspaceOrigin`] names
/// the seed, the operator, or a single agent, and claiming
/// `Agent { id: <desk-id> }` would attribute the folder to a teammate that does
/// not exist. A lazily-minted root carries the same stamp the boot scaffold
/// used to give it, so nothing downstream can tell the two apart. The desk's
/// *contents* still carry the real agent that wrote each of them.
pub async fn ensure_desk_folder(
    store: &dyn WorkspaceStore,
    company: &CompanyId,
    desk_id: &str,
) -> Result<String> {
    Ok(ensure_member_folder(
        store,
        company,
        DESKS_ROOT,
        desk_id.trim(),
        WorkspaceOrigin::Seed,
    )
    .await?
    .0)
}

/// The shared body of [`ensure_agent_folder`] and [`ensure_desk_folder`]:
/// resolve `root`, then resolve `id` beneath it, creating what is missing.
///
/// # The tree read is a fast path; the store decides (issue #759)
///
/// [`find`] answering `Free` describes the instant the tree was read, and the
/// create used to act on it afterwards. Two agents first producing something at
/// once therefore both saw `agents/` free — or both saw `agents/<id>/` free —
/// and both created, leaving two folders under one name. Nothing repairs that:
/// [`find`] answers a duplicated name with `Collision` from then on, so a race
/// lasting microseconds refuses that agent's folder forever.
///
/// Both creates now go through [`WorkspaceStore::adopt_or_create_folder`], which
/// resolves the contention inside the store. That also makes the stale snapshot
/// harmless: a caller whose read predates another's create adopts the folder
/// that exists rather than minting a rival, and — because the root claim returns
/// the *winner's* id — the member folder beneath it is claimed under the same
/// parent either way.
///
/// Returns the folder's id and whether *this* call minted the **member** folder
/// (issue #1801). The bool is `true` only in the arm that reaches
/// [`WorkspaceStore::adopt_or_create_folder`] with a [`FolderClaim::Created`]
/// result; adoption of an existing or legacy folder answers `false`. A freshly
/// minted *root* is never folded into that signal — an empty `agents/` is
/// scaffolding, not the empty member folder a failed write leaves behind.
///
/// [`FolderClaim::Created`]: crate::ports::workspace::FolderClaim::Created
async fn ensure_member_folder(
    store: &dyn WorkspaceStore,
    company: &CompanyId,
    root: &str,
    id: &str,
    origin: WorkspaceOrigin,
) -> Result<(String, bool)> {
    // The id becomes a node name, and a name carrying a separator renders an
    // ambiguous or traversal-shaped path. The `fs` backend refuses such names
    // outright and the sqlite/mongodb backends do not, so the guard lives here
    // rather than being assumed of the store.
    if !is_legal_segment(id) {
        return Err(OpenCompanyError::InvalidRequest(format!(
            "`{id}` is not a legal workspace path segment, so it cannot name a folder under \
             `{root}/`"
        )));
    }

    let nodes = store.tree(company).await?;

    let root_id = match find(&nodes, None, root) {
        Found::Folder(id) => id,
        Found::Free => {
            store
                .adopt_or_create_folder(company, None, root, WorkspaceOrigin::Seed)
                .await?
                .into_node()
                .id
        }
        Found::Collision(why) => return Err(OpenCompanyError::Conflict(why)),
    };

    // The folder is named by the lowercase-dashed rule, not by the id verbatim:
    // a roster id is snake_case (`page_builder`), and every other name in the
    // tree is dashed. Falls back to the id when the id normalizes to nothing,
    // which `is_legal_segment` above has already proved is a usable segment.
    let name = kebab_name_or(id, id);

    match find(&nodes, Some(&root_id), &name) {
        Found::Folder(existing) => Ok((existing, false)),
        Found::Collision(why) => Err(OpenCompanyError::Conflict(why)),
        // Nothing carries the canonical name. Before minting it, look for the
        // folder an older build made under the id verbatim — `page_builder`
        // rather than `page-builder`, which `find` does not match because the
        // difference is a character, not a case. Adopting it keeps one member's
        // work in one folder across the upgrade; creating beside it would split
        // the member's history in two and report neither half as incomplete.
        Found::Free => match legacy_alias(&nodes, &root_id, id, &name) {
            Some(Found::Folder(existing)) => Ok((existing, false)),
            Some(Found::Collision(why)) => Err(OpenCompanyError::Conflict(why)),
            _ => {
                let claim = store
                    .adopt_or_create_folder(company, Some(&root_id), &name, origin)
                    .await?;
                // Adoption still hands back a node; only a genuine mint is a
                // rollback candidate, so the flag rides `was_created` rather
                // than "we took the create arm".
                let created = claim.was_created();
                Ok((claim.into_node().id, created))
            }
        },
    }
}

/// Best-effort removal of folders a single create or publish freshly minted,
/// run after a later write in that same operation failed and would otherwise
/// leave them standing empty (issue #1801).
///
/// The seams that mint a member folder — `agents/<id>/`, `artifacts/<id>/`, a
/// task folder — do so *before* the note or payload that gives the folder a
/// reason to exist. When that write then fails (a store error, a quota refusal),
/// the folder is what the Tidy(#700)/Repair(#759) buttons later have to sweep.
/// This closes the non-race half of that at the source: the caller passes the
/// ids it, and only it, brought into existence, and each is removed **only while
/// still structurally empty on a fresh tree read**.
///
/// The still-empty guard is the whole safety of it. A folder that gained a child
/// in the window — a concurrent publisher that adopted it, a note that did land
/// — is left exactly as it stands and never recursively deleted out from under
/// whoever filled it. Removal is child-first, so a member folder is cleared
/// before a folder it was the only occupant of, letting an intermediate folder
/// this same operation minted fall empty and be removed in turn. A reserved
/// system root ([`SYSTEM_ROOTS`]) is refused outright even if handed in: an
/// empty root is boot scaffolding, and the next boot re-lays it regardless.
///
/// The guard is enforced by [`WorkspaceStore::delete_if_empty`], not by the
/// `tree()` read below — that read only picks removal order (child-first) and
/// filters out reserved roots. Deciding go/no-go from it directly would be
/// exactly the bug review found on this PR: a concurrent adopter can land a
/// child in the window between this `tree()` call and a later `delete()`, and
/// an unconditional `delete` would sweep that child away with the folder. Each
/// `delete_if_empty` call instead re-checks the store's *current* state, which
/// is also why no bookkeeping is needed across iterations of the loop below —
/// once a child folder is actually removed, the store's own state reflects
/// that for its parent's check, unlike this function's now-stale `nodes` read.
///
/// Best-effort by design: it is undoing a path that has already failed, so a
/// tree read or delete that itself errors is swallowed — the caller is already
/// returning the original error, and a folder that survives cleanup is exactly
/// the pre-fix state the Repair button still covers.
pub(crate) async fn rollback_empty_minted_folders(
    store: &dyn WorkspaceStore,
    company: &CompanyId,
    minted: &[String],
) {
    if minted.is_empty() {
        return;
    }
    let nodes = match store.tree(company).await {
        Ok(nodes) => nodes,
        Err(_) => return,
    };

    // Deepest first, so a member/task folder is cleared before any folder it was
    // the sole occupant of. A reserved root is dropped here rather than deleted.
    // Order only — the actual emptiness decision is the store's, made fresh
    // inside `delete_if_empty` below, never from this snapshot.
    let mut targets: Vec<&String> = minted
        .iter()
        .filter(|id| !is_reserved_root(&nodes, id))
        .collect();
    targets.sort_by_key(|id| std::cmp::Reverse(node_depth(&nodes, id)));

    for id in targets {
        let _ = store.delete_if_empty(company, id).await;
    }
}

/// Whether `id` names one of the eagerly-scaffolded roots at the workspace root
/// — the nodes [`rollback_empty_minted_folders`] must never remove.
fn is_reserved_root(nodes: &[WorkspaceNode], id: &str) -> bool {
    nodes.iter().any(|node| {
        node.id == id
            && node.parent_id.is_none()
            && SYSTEM_ROOTS
                .iter()
                .any(|root| node.name.eq_ignore_ascii_case(root))
    })
}

/// The length of `id`'s parent chain within `nodes` — its depth from the
/// workspace root, used to order [`rollback_empty_minted_folders`] child-first.
fn node_depth(nodes: &[WorkspaceNode], id: &str) -> usize {
    let mut depth = 0usize;
    let mut current = nodes.iter().find(|node| node.id == id);
    while let Some(node) = current {
        match node.parent_id.as_deref() {
            Some(parent) => {
                depth += 1;
                current = nodes.iter().find(|node| node.id == parent);
            }
            None => break,
        }
    }
    depth
}

/// The pre-lowercase-dashed spelling of a member folder, when there is one to
/// look for.
///
/// `None` when the id already *is* its canonical name, so the caller mints the
/// canonical folder without a second lookup. Separated out because "adopt what
/// the last build wrote" is a distinct decision from "create what this build
/// writes", and folding it into the match arm above hid that.
fn legacy_alias(
    nodes: &[WorkspaceNode],
    root_id: &str,
    id: &str,
    canonical: &str,
) -> Option<Found> {
    (id != canonical).then(|| find(nodes, Some(root_id), id))
}

/// What a lookup for one named node under one parent found.
///
/// `pub(crate)` alongside [`find`], for the one other module that has to resolve
/// a system root: [`workspace_sweep`](crate::company::workspace_sweep). A sweep
/// that removes folders *under* `agents/` has to agree with the scaffold about
/// which node that root is, and about when there isn't one — a second lookup
/// with its own idea of "the `Agents` folder" could adopt a node this module
/// refuses to touch, and then delete beneath it.
pub(crate) enum Found {
    /// Exactly one folder carries the name — adopt it, by id.
    Folder(String),
    /// Nothing carries the name; it is free to create.
    Free,
    /// A *file* carries the name, or several nodes do. Never resolvable, with
    /// the reason phrased for a log line or an error body.
    Collision(String),
}

/// Look for a node named `name` whose parent is `parent` (`None` = the
/// workspace root).
///
/// `pub(crate)` so the fail-closed adoption rule above has exactly one
/// implementation. See [`Found`].
///
/// # The name match is case-insensitive
///
/// Because the names this module mints changed case: `Agents` became `agents`
/// when the lowercase-dashed rule landed
/// ([`workspace_names`](super::workspace_names)). A case-*sensitive* lookup
/// would answer `Free` for every company created before that, and the create
/// beneath it would mint a second root — so one agent's home would be under
/// `agents/` and its next deliverable under `agents/`, with neither view
/// complete and nothing reporting the split. Matching case-insensitively adopts
/// the folder that is already there, whichever spelling it carries.
///
/// It also closes the twin the other way round: a company whose operator made a
/// `Secrets/` folder by hand no longer gets a rival `secrets/` beside it. Two
/// nodes whose names differ only in case are now one ambiguous name, which is
/// [`Found::Collision`] — the fail-closed answer this module gives every
/// ambiguity, rather than a coin flip between them.
pub(crate) fn find(nodes: &[WorkspaceNode], parent: Option<&str>, name: &str) -> Found {
    let matches: Vec<&WorkspaceNode> = nodes
        .iter()
        .filter(|node| node.parent_id.as_deref() == parent && node.name.eq_ignore_ascii_case(name))
        .collect();

    match matches.as_slice() {
        [one] if one.kind == NodeKind::Folder => Found::Folder(one.id.clone()),
        [_] => Found::Collision(format!(
            "`{name}` already exists as a file, not a folder, so it is left alone"
        )),
        [] => Found::Free,
        many => Found::Collision(format!(
            "{count} nodes are named `{name}`, so the path is ambiguous",
            count = many.len()
        )),
    }
}

/// Whether `name` is usable as a single workspace path segment.
///
/// Mirrors the `fs` backend's `reject_unsafe_name` and the agent tool layer's
/// `is_legal_segment`. Duplicated rather than shared because this module is in
/// the default build and the tool layer links only under the `openhuman`
/// feature; the rule is three lines and a shared home for it would drag the
/// whole harness into every build.
fn is_legal_segment(name: &str) -> bool {
    !name.is_empty()
        && name != "."
        && name != ".."
        && !name.contains('/')
        && !name.contains('\\')
        && !name.contains('\0')
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::store::FsOps;

    fn agent(id: &str) -> WorkspaceOrigin {
        WorkspaceOrigin::Agent { id: id.to_string() }
    }

    async fn store() -> (tempfile::TempDir, Arc<dyn WorkspaceStore>) {
        let dir = tempfile::tempdir().expect("tempdir");
        let ops: Arc<dyn WorkspaceStore> = Arc::new(FsOps::new(dir.path()));
        (dir, ops)
    }

    /// Seeds root folders that share `name` by writing the workspace index
    /// directly.
    ///
    /// The filesystem store refuses to *create* two siblings under one name,
    /// because on that backend they would resolve to one path (issue #666).
    /// The trees below are the ones that check what the scaffold does when it
    /// nevertheless *finds* an ambiguous root — an index written before that
    /// refusal existed, or one an id-keyed backend can still represent legally.
    /// So the state is written rather than requested: going through `create`
    /// would only re-assert the store's refusal and never reach the scaffold.
    async fn seed_duplicate_roots(
        dir: &std::path::Path,
        company: &CompanyId,
        name: &str,
        ids: &[&str],
    ) {
        let index: std::collections::HashMap<String, WorkspaceNode> = ids
            .iter()
            .map(|id| {
                (
                    (*id).to_string(),
                    WorkspaceNode {
                        id: (*id).to_string(),
                        name: name.to_string(),
                        kind: NodeKind::Folder,
                        parent_id: None,
                        updated_at_millis: 1,
                        created_by: WorkspaceOrigin::Operator,
                        updated_by: WorkspaceOrigin::Operator,
                        mime: None,
                        size: None,
                        sha256: None,
                        adopted: false,
                    },
                )
            })
            .collect();
        let bundle = crate::store::Bundle::new(dir.to_path_buf(), company);
        tokio::fs::create_dir_all(bundle.workspace_dir())
            .await
            .expect("workspace dir");
        tokio::fs::write(
            bundle.workspace_index_json(),
            serde_json::to_vec(&index).expect("index json"),
        )
        .await
        .expect("seed index");
    }

    /// A node's rendered `parent/child` path, for readable assertions.
    fn path_of(nodes: &[WorkspaceNode], node: &WorkspaceNode) -> String {
        match &node.parent_id {
            None => node.name.clone(),
            Some(parent) => match nodes.iter().find(|n| &n.id == parent) {
                Some(p) => format!("{}/{}", path_of(nodes, p), node.name),
                None => node.name.clone(),
            },
        }
    }

    fn paths(nodes: &[WorkspaceNode]) -> Vec<String> {
        let mut out: Vec<String> = nodes.iter().map(|n| path_of(nodes, n)).collect();
        out.sort();
        out
    }

    async fn tree_paths(ws: &Arc<dyn WorkspaceStore>, company: &CompanyId) -> Vec<String> {
        paths(&ws.tree(company).await.unwrap())
    }

    fn scaffold_paths() -> Vec<&'static str> {
        vec![
            "agents",
            "artifacts",
            "artifacts/readme.md",
            "secrets",
            "secrets/readme.md",
        ]
    }

    /// The scaffold has an empty agent root plus the operator-only secrets
    /// folder and its explanatory note. It never creates roster member folders
    /// or the unused `desks/` root.
    #[tokio::test]
    async fn it_provisions_one_empty_system_root() {
        let (_dir, ws) = store().await;
        let company = CompanyId::new("acme");

        ensure_workspace_scaffold(ws.as_ref(), &company)
            .await
            .unwrap();

        let nodes = ws.tree(&company).await.unwrap();
        assert_eq!(
            paths(&nodes),
            scaffold_paths(),
            "`desks/` has no producer, so boot must not lay it down"
        );
        for node in nodes.iter().filter(|node| node.kind == NodeKind::Folder) {
            assert_eq!(
                node.created_by,
                WorkspaceOrigin::Seed,
                "{} is runtime scaffolding, not anybody's writing",
                node.name
            );
        }
        // Both notes, by path: two roots now carry a `readme.md`, so a
        // find-by-name would assert against whichever the store happened to
        // return first and pass while one of them held the other's text.
        for (path, expected) in [
            ("secrets/readme.md", SECRETS_README),
            ("artifacts/readme.md", ARTIFACTS_README),
        ] {
            let readme = nodes
                .iter()
                .find(|node| path_of(&nodes, node) == path)
                .unwrap_or_else(|| panic!("{path} is missing from the scaffold"));
            let (_, body) = ws.read(&company, &readme.id).await.unwrap().unwrap();
            assert_eq!(body, expected, "{path}");
        }
    }

    /// The scaffold takes no roster and asks for none: a company with no agents
    /// at all still gets the shape of its workspace. (This reverses the earlier
    /// eager design, where an empty roster deliberately created nothing —
    /// there, a root with no children was a stray; here it is the point.)
    #[tokio::test]
    async fn a_company_with_no_roster_still_gets_the_agents_root() {
        let (_dir, ws) = store().await;
        let company = CompanyId::new("solo");

        ensure_workspace_scaffold(ws.as_ref(), &company)
            .await
            .unwrap();

        assert_eq!(tree_paths(&ws, &company).await, scaffold_paths());
    }

    /// The deliverables root is scaffolded; a teammate's folder beneath it is
    /// not, and appears only when that teammate publishes.
    ///
    /// The asymmetry is the whole design: the root says the company has
    /// somewhere to put deliverables, a member folder says *this* teammate
    /// delivered. An eager folder per roster member would make the second claim
    /// on behalf of teammates that have produced nothing.
    #[tokio::test]
    async fn an_artifact_folder_is_minted_on_demand_beneath_a_scaffolded_root() {
        let (_dir, ws) = store().await;
        let company = CompanyId::new("acme");

        ensure_workspace_scaffold(ws.as_ref(), &company)
            .await
            .unwrap();
        assert!(
            !tree_paths(&ws, &company)
                .await
                .contains(&"artifacts/cmo".to_string()),
            "boot must not mint a folder for a teammate that has published nothing"
        );

        let first = ensure_artifact_folder(ws.as_ref(), &company, "cmo")
            .await
            .unwrap();
        let second = ensure_artifact_folder(ws.as_ref(), &company, "cmo")
            .await
            .unwrap();
        assert_eq!(first, second, "a second call minted a rival folder");

        let nodes = ws.tree(&company).await.unwrap();
        let mine = nodes.iter().find(|node| node.id == first).unwrap();
        assert_eq!(path_of(&nodes, mine), "artifacts/cmo");
        assert_eq!(mine.kind, NodeKind::Folder);
        assert_eq!(mine.created_by, agent("cmo"));
        assert!(
            !nodes
                .iter()
                .any(|node| path_of(&nodes, node) == "agents/cmo"),
            "publishing must not also mint the agent's scratch home"
        );
    }

    /// The property that lets this run on every boot.
    #[tokio::test]
    async fn it_is_idempotent() {
        let (_dir, ws) = store().await;
        let company = CompanyId::new("acme");

        for _ in 0..3 {
            ensure_workspace_scaffold(ws.as_ref(), &company)
                .await
                .unwrap();
        }

        assert_eq!(tree_paths(&ws, &company).await, scaffold_paths());
    }

    /// An operator-made `Agents/` folder is adopted as-is rather than
    /// duplicated — identity is by path, so a second root would make every
    /// `agents/...` path permanently ambiguous.
    #[tokio::test]
    async fn an_existing_root_folder_is_adopted() {
        let (_dir, ws) = store().await;
        let company = CompanyId::new("acme");
        ws.create(
            &company,
            &WorkspaceNode {
                id: "hand-made".to_string(),
                name: AGENTS_ROOT.to_string(),
                kind: NodeKind::Folder,
                parent_id: None,
                updated_at_millis: 1,
                created_by: WorkspaceOrigin::Operator,
                updated_by: WorkspaceOrigin::Operator,
                mime: None,
                size: None,
                sha256: None,
                adopted: false,
            },
            None,
        )
        .await
        .unwrap();

        ensure_workspace_scaffold(ws.as_ref(), &company)
            .await
            .unwrap();

        let nodes = ws.tree(&company).await.unwrap();
        assert_eq!(paths(&nodes), scaffold_paths());
        let root = nodes.iter().find(|n| n.name == AGENTS_ROOT).unwrap();
        assert_eq!(root.id, "hand-made", "the operator's folder must be reused");
        assert_eq!(
            root.created_by,
            WorkspaceOrigin::Operator,
            "adoption must not rewrite the operator's authorship"
        );
    }

    /// Fail-closed: a root *file* named `Agents` is a collision this module has
    /// no honest way to resolve, so it leaves it alone rather than shadowing
    /// the operator's note with a rival folder of the same name — and creates
    /// nothing else in its place.
    #[tokio::test]
    async fn a_root_file_is_left_alone_rather_than_shadowed() {
        let (_dir, ws) = store().await;
        let company = CompanyId::new("acme");
        ws.create(
            &company,
            &WorkspaceNode {
                id: "note".to_string(),
                name: AGENTS_ROOT.to_string(),
                kind: NodeKind::File,
                parent_id: None,
                updated_at_millis: 1,
                created_by: WorkspaceOrigin::Operator,
                updated_by: WorkspaceOrigin::Operator,
                mime: None,
                size: None,
                sha256: None,
                adopted: false,
            },
            Some("# not a folder"),
        )
        .await
        .unwrap();

        ensure_workspace_scaffold(ws.as_ref(), &company)
            .await
            .unwrap();

        let nodes = ws.tree(&company).await.unwrap();
        assert_eq!(
            paths(&nodes),
            scaffold_paths(),
            "the collision must not be shadowed; unrelated scaffold still provisions"
        );
        assert_eq!(
            nodes.iter().find(|n| n.name == AGENTS_ROOT).unwrap().kind,
            NodeKind::File,
            "the operator's note must not be shadowed by a folder of the same name"
        );
    }

    /// Several root nodes sharing a reserved name is the other unresolvable
    /// shape: adding a third would make it worse, so nothing is created.
    #[tokio::test]
    async fn several_nodes_sharing_a_root_name_are_left_alone() {
        let (dir, ws) = store().await;
        let company = CompanyId::new("acme");
        seed_duplicate_roots(dir.path(), &company, AGENTS_ROOT, &["dup-a", "dup-b"]).await;

        ensure_workspace_scaffold(ws.as_ref(), &company)
            .await
            .unwrap();

        let nodes = ws.tree(&company).await.unwrap();
        assert_eq!(
            nodes.iter().filter(|n| n.name == AGENTS_ROOT).count(),
            2,
            "an ambiguous root must not gain a third candidate"
        );
        assert_eq!(
            paths(&nodes),
            vec![
                "agents",
                "agents",
                "artifacts",
                "artifacts/readme.md",
                "secrets",
                "secrets/readme.md"
            ],
            "only the unrelated secrets scaffold may be created beside the collision"
        );
    }

    /// The tree is company-scoped: scaffolding one company leaves another's
    /// workspace untouched.
    #[tokio::test]
    async fn scaffolding_is_per_company() {
        let (_dir, ws) = store().await;
        let acme = CompanyId::new("acme");
        let other = CompanyId::new("other");

        ensure_workspace_scaffold(ws.as_ref(), &acme).await.unwrap();

        assert!(ws.is_empty(&other).await.unwrap());
    }

    // -- the lazy minters ---------------------------------------------------

    /// The property #552's publish path depends on: minting on every publish
    /// must be free after the first one, and must hand back the *same* parent
    /// id so two deliverables land in one folder rather than two.
    #[tokio::test]
    async fn ensure_agent_folder_is_idempotent_and_stable() {
        let (_dir, ws) = store().await;
        let company = CompanyId::new("acme");
        ensure_workspace_scaffold(ws.as_ref(), &company)
            .await
            .unwrap();

        let first = ensure_agent_folder(ws.as_ref(), &company, "ceo")
            .await
            .unwrap();
        let second = ensure_agent_folder(ws.as_ref(), &company, "ceo")
            .await
            .unwrap();

        assert_eq!(first, second, "a second call minted a rival folder");
        assert_eq!(
            tree_paths(&ws, &company).await,
            vec![
                "agents",
                "agents/ceo",
                "artifacts",
                "artifacts/readme.md",
                "secrets",
                "secrets/readme.md"
            ]
        );
        let nodes = ws.tree(&company).await.unwrap();
        let ceo = nodes.iter().find(|n| n.name == "ceo").unwrap();
        assert_eq!(ceo.kind, NodeKind::Folder);
        assert_eq!(ceo.created_by, agent("ceo"));
    }

    /// One agent producing something must not conjure folders for the rest of
    /// the roster — that is the whole difference from the eager design.
    #[tokio::test]
    async fn minting_one_agent_folder_leaves_the_roster_alone() {
        let (_dir, ws) = store().await;
        let company = CompanyId::new("acme");
        ensure_workspace_scaffold(ws.as_ref(), &company)
            .await
            .unwrap();

        ensure_agent_folder(ws.as_ref(), &company, "cmo")
            .await
            .unwrap();

        assert_eq!(
            tree_paths(&ws, &company).await,
            vec![
                "agents",
                "agents/cmo",
                "artifacts",
                "artifacts/readme.md",
                "secrets",
                "secrets/readme.md"
            ]
        );
    }

    /// A minter is also its own repair path: it creates the root when the
    /// scaffold never ran, so a boot whose create fail-softed still ends up
    /// with a usable `agents/` the first time an agent produces anything.
    #[tokio::test]
    async fn ensure_agent_folder_creates_the_root_it_needs() {
        let (_dir, ws) = store().await;
        let company = CompanyId::new("acme");

        let id = ensure_agent_folder(ws.as_ref(), &company, "ceo")
            .await
            .unwrap();

        let nodes = ws.tree(&company).await.unwrap();
        assert_eq!(paths(&nodes), vec!["agents", "agents/ceo"]);
        let root = nodes.iter().find(|n| n.name == AGENTS_ROOT).unwrap();
        assert_eq!(root.created_by, WorkspaceOrigin::Seed);
        assert_eq!(nodes.iter().find(|n| n.id == id).unwrap().name, "ceo");
    }

    /// An operator's hand-made `Agents/ceo` is adopted, not duplicated.
    #[tokio::test]
    async fn ensure_agent_folder_adopts_an_existing_folder() {
        let (_dir, ws) = store().await;
        let company = CompanyId::new("acme");
        ensure_workspace_scaffold(ws.as_ref(), &company)
            .await
            .unwrap();
        let root_id = ws
            .tree(&company)
            .await
            .unwrap()
            .into_iter()
            .find(|n| n.name == AGENTS_ROOT)
            .unwrap()
            .id;
        ws.create(
            &company,
            &WorkspaceNode {
                id: "hand-made".to_string(),
                name: "ceo".to_string(),
                kind: NodeKind::Folder,
                parent_id: Some(root_id),
                updated_at_millis: 1,
                created_by: WorkspaceOrigin::Operator,
                updated_by: WorkspaceOrigin::Operator,
                mime: None,
                size: None,
                sha256: None,
                adopted: false,
            },
            None,
        )
        .await
        .unwrap();

        let id = ensure_agent_folder(ws.as_ref(), &company, "ceo")
            .await
            .unwrap();

        assert_eq!(id, "hand-made");
        assert_eq!(
            ws.tree(&company)
                .await
                .unwrap()
                .iter()
                .find(|n| n.id == "hand-made")
                .unwrap()
                .created_by,
            WorkspaceOrigin::Operator,
            "adoption must not rewrite the operator's authorship"
        );
    }

    /// The minter has a caller waiting on an id, so a collision it cannot
    /// resolve is an error rather than a warn-and-carry-on — there is no id to
    /// hand back and pretending otherwise would strand the caller's write.
    #[tokio::test]
    async fn a_colliding_member_file_is_an_error_not_a_silent_skip() {
        let (_dir, ws) = store().await;
        let company = CompanyId::new("acme");
        ensure_workspace_scaffold(ws.as_ref(), &company)
            .await
            .unwrap();
        let root_id = ws
            .tree(&company)
            .await
            .unwrap()
            .into_iter()
            .find(|n| n.name == AGENTS_ROOT)
            .unwrap()
            .id;
        ws.create(
            &company,
            &WorkspaceNode {
                id: "ceo-note".to_string(),
                name: "ceo".to_string(),
                kind: NodeKind::File,
                parent_id: Some(root_id),
                updated_at_millis: 1,
                created_by: WorkspaceOrigin::Operator,
                updated_by: WorkspaceOrigin::Operator,
                mime: None,
                size: None,
                sha256: None,
                adopted: false,
            },
            Some("# notes about the ceo"),
        )
        .await
        .unwrap();

        let err = ensure_agent_folder(ws.as_ref(), &company, "ceo")
            .await
            .expect_err("a colliding note must not resolve to a folder id");
        assert!(err.to_string().contains("ceo"), "{err}");
        assert_eq!(
            ws.tree(&company)
                .await
                .unwrap()
                .iter()
                .find(|n| n.name == "ceo")
                .unwrap()
                .kind,
            NodeKind::File,
            "the operator's note must not be shadowed by a folder of the same name"
        );
    }

    /// An id that is not a legal path segment would render an unaddressable or
    /// traversal-shaped path, so it is refused before anything is created.
    #[tokio::test]
    async fn an_illegal_id_is_refused_and_creates_nothing() {
        let (_dir, ws) = store().await;
        let company = CompanyId::new("acme");

        for id in ["../escape", "", ".", "a/b", "a\\b"] {
            ensure_agent_folder(ws.as_ref(), &company, id)
                .await
                .expect_err("`{id}` is not a legal path segment");
        }

        assert!(ws.is_empty(&company).await.unwrap());
    }

    /// Issue #1839: a folder a rival adopted survives that rival's rollback.
    ///
    /// The residual half of #1801 removes a folder one caller minted and then
    /// failed to write beneath. But a second caller can adopt the same folder in
    /// the window — `adopt_or_create_folder` hands it back and stamps the lease —
    /// and the minter's `rollback_empty_minted_folders` must then leave it
    /// standing, because the adopter is about to write into it. The still-empty
    /// guard alone could not tell the two apart; the lease is what does.
    #[tokio::test]
    async fn rollback_leaves_an_adopted_folder_but_sweeps_an_unadopted_one() {
        let (_dir, ws) = store().await;
        let company = CompanyId::new("acme");

        // The minter creates `agents/cmo/` — its id is what a failed write would
        // roll back.
        let (adopted_id, created) = ensure_agent_folder_tracked(ws.as_ref(), &company, "cmo")
            .await
            .unwrap();
        assert!(created, "the first call minted the folder");

        // A rival publisher adopts the very same folder, taking the lease.
        let root_id = ws
            .tree(&company)
            .await
            .unwrap()
            .into_iter()
            .find(|n| n.name == AGENTS_ROOT)
            .unwrap()
            .id;
        let claim = ws
            .adopt_or_create_folder(&company, Some(&root_id), "cmo", agent("cmo"))
            .await
            .unwrap();
        assert!(!claim.was_created(), "the rival adopted, it did not mint");
        assert!(claim.node().adopted, "adoption took the lease");

        // A second minted folder nobody adopts is the genuine #1801 leak.
        let (leaked_id, _) = ensure_agent_folder_tracked(ws.as_ref(), &company, "cto")
            .await
            .unwrap();

        // The minter's write failed; it rolls back both folders it minted.
        rollback_empty_minted_folders(
            ws.as_ref(),
            &company,
            &[adopted_id.clone(), leaked_id.clone()],
        )
        .await;

        let names: Vec<String> = ws
            .tree(&company)
            .await
            .unwrap()
            .into_iter()
            .map(|n| n.name)
            .collect();
        assert!(
            names.contains(&"cmo".to_string()),
            "an adopted empty folder must survive the minter's rollback: {names:?}"
        );
        assert!(
            !names.contains(&"cto".to_string()),
            "but an unadopted empty minted folder is still swept: {names:?}"
        );
    }

    /// The desk minter is the same shape one root over — and since issue #645
    /// it is the *only* thing that ever creates `desks/`. Deliberately run with
    /// no scaffold at all: the first call must mint the root and the member
    /// folder together, which is what lets boot stop laying down an empty root
    /// nothing was filling.
    ///
    /// The root it mints stamps `Seed`, exactly as the boot scaffold used to,
    /// so no consumer can tell a lazily-minted root from the old eager one. The
    /// desk folder stamps `Seed` too, because a desk is not an agent and
    /// `WorkspaceOrigin` has no way to name one.
    #[tokio::test]
    async fn ensure_desk_folder_mints_the_desks_root_on_first_use() {
        let (_dir, ws) = store().await;
        let company = CompanyId::new("acme");

        let first = ensure_desk_folder(ws.as_ref(), &company, "creative_studio")
            .await
            .unwrap();
        let second = ensure_desk_folder(ws.as_ref(), &company, "creative_studio")
            .await
            .unwrap();

        assert_eq!(first, second, "a second call minted a rival folder");
        assert_eq!(
            tree_paths(&ws, &company).await,
            vec!["desks", "desks/creative-studio"],
            "the root appears with its first occupant, and brings nothing else"
        );
        let nodes = ws.tree(&company).await.unwrap();
        let desk = nodes.iter().find(|n| n.id == first).unwrap();
        assert_eq!(desk.kind, NodeKind::Folder);
        assert_eq!(desk.created_by, WorkspaceOrigin::Seed);
        let root = nodes.iter().find(|n| n.name == DESKS_ROOT).unwrap();
        assert_eq!(root.kind, NodeKind::Folder);
        assert_eq!(
            root.created_by,
            WorkspaceOrigin::Seed,
            "a lazily-minted root must carry the stamp boot used to give it"
        );
    }

    /// The migration story for every company that booted before issue #645: its
    /// `desks/` root already exists, and the scaffold must leave it completely
    /// alone rather than notice it is no longer managed and tidy it away.
    ///
    /// The scaffold only ever looks up the names in `SYSTEM_ROOTS`, so a
    /// `desks/` node is not even inspected — id, authorship and contents all
    /// survive untouched.
    #[tokio::test]
    async fn a_pre_existing_desks_root_survives_the_scaffold_untouched() {
        let (_dir, ws) = store().await;
        let company = CompanyId::new("legacy");
        ws.create(
            &company,
            &WorkspaceNode {
                id: "legacy-desks".to_string(),
                name: DESKS_ROOT.to_string(),
                kind: NodeKind::Folder,
                parent_id: None,
                updated_at_millis: 1,
                created_by: WorkspaceOrigin::Operator,
                updated_by: WorkspaceOrigin::Operator,
                mime: None,
                size: None,
                sha256: None,
                adopted: false,
            },
            None,
        )
        .await
        .unwrap();

        ensure_workspace_scaffold(ws.as_ref(), &company)
            .await
            .unwrap();

        let nodes = ws.tree(&company).await.unwrap();
        assert_eq!(
            paths(&nodes),
            vec![
                "agents",
                "artifacts",
                "artifacts/readme.md",
                "desks",
                "secrets",
                "secrets/readme.md"
            ],
            "dropping `desks/` from the scaffold must not delete an existing one"
        );
        let desks = nodes.iter().find(|n| n.name == DESKS_ROOT).unwrap();
        assert_eq!(desks.id, "legacy-desks", "the existing root must be kept");
        assert_eq!(
            desks.created_by,
            WorkspaceOrigin::Operator,
            "an unmanaged root's authorship must not be rewritten"
        );
    }

    /// The un-managed counterpart to `several_nodes_sharing_a_root_name_are_
    /// left_alone`: duplicate `Desks` nodes are not a collision the scaffold
    /// has to resolve any more, they are simply none of its business — and the
    /// root it *does* manage still provisions beside them.
    #[tokio::test]
    async fn duplicate_desks_nodes_do_not_disturb_the_scaffold() {
        let (dir, ws) = store().await;
        let company = CompanyId::new("acme");
        seed_duplicate_roots(dir.path(), &company, DESKS_ROOT, &["dup-a", "dup-b"]).await;

        ensure_workspace_scaffold(ws.as_ref(), &company)
            .await
            .unwrap();

        let nodes = ws.tree(&company).await.unwrap();
        assert_eq!(
            nodes.iter().filter(|n| n.name == DESKS_ROOT).count(),
            2,
            "an unmanaged name must be neither deduplicated nor added to"
        );
        assert_eq!(
            nodes.iter().filter(|n| n.name == AGENTS_ROOT).count(),
            1,
            "an odd name elsewhere is no reason to withhold a managed root"
        );
    }

    /// The two roots stay independent: minting a desk folder does not reach
    /// into `agents/`, and vice versa.
    #[tokio::test]
    async fn the_two_roots_do_not_leak_into_each_other() {
        let (_dir, ws) = store().await;
        let company = CompanyId::new("acme");

        ensure_agent_folder(ws.as_ref(), &company, "shared")
            .await
            .unwrap();
        ensure_desk_folder(ws.as_ref(), &company, "shared")
            .await
            .unwrap();

        assert_eq!(
            tree_paths(&ws, &company).await,
            vec!["agents", "agents/shared", "desks", "desks/shared"]
        );
    }

    /// The names the scaffold mints follow the workspace naming rule, so a
    /// fresh company's tree is uniform from the first boot rather than mixing
    /// `Agents/` with `playbooks/` the moment anybody puts something in it.
    #[tokio::test]
    async fn the_scaffolded_names_are_lowercase_and_dashed() {
        let (_dir, ws) = store().await;
        let company = CompanyId::new("acme");
        ensure_workspace_scaffold(ws.as_ref(), &company)
            .await
            .unwrap();
        ensure_agent_folder(ws.as_ref(), &company, "page_builder")
            .await
            .unwrap();

        let paths = tree_paths(&ws, &company).await;
        assert!(
            paths.contains(&"agents/page-builder".to_string()),
            "a snake_case roster id should mint a dashed folder: {paths:?}"
        );
        for path in &paths {
            assert_eq!(
                *path,
                crate::company::workspace_names::kebab_path(path),
                "the scaffold minted a name outside the rule: {path}"
            );
        }
    }

    /// A company created before the rule has `Agents/`, and must not grow a
    /// second lowercase root beside it — that would put one agent's home in two
    /// places, with neither view complete.
    #[tokio::test]
    async fn a_legacy_capitalised_root_is_adopted_not_duplicated() {
        let (_dir, ws) = store().await;
        let company = CompanyId::new("acme");
        let legacy = ws
            .adopt_or_create_folder(&company, None, "Agents", WorkspaceOrigin::Operator)
            .await
            .unwrap()
            .into_node()
            .id;

        ensure_workspace_scaffold(ws.as_ref(), &company)
            .await
            .unwrap();
        let home = ensure_agent_folder(ws.as_ref(), &company, "ceo")
            .await
            .unwrap();

        let nodes = ws.tree(&company).await.unwrap();
        assert_eq!(
            nodes
                .iter()
                .filter(|n| n.parent_id.is_none() && n.name.eq_ignore_ascii_case("agents"))
                .count(),
            1,
            "the legacy root should be adopted, not joined by a twin: {:?}",
            paths(&nodes)
        );
        assert_eq!(
            nodes.iter().find(|n| n.id == home).unwrap().parent_id,
            Some(legacy),
            "the member folder belongs under the root that already existed"
        );
    }

    /// The other half of the same upgrade: the member folder itself was named
    /// by the roster id verbatim, which differs from its dashed form by a
    /// character rather than by case, so `find` cannot see it.
    #[tokio::test]
    async fn a_legacy_member_folder_named_by_the_raw_id_is_adopted() {
        let (_dir, ws) = store().await;
        let company = CompanyId::new("acme");
        ensure_workspace_scaffold(ws.as_ref(), &company)
            .await
            .unwrap();
        let root_id = ws
            .tree(&company)
            .await
            .unwrap()
            .into_iter()
            .find(|n| n.name == AGENTS_ROOT)
            .unwrap()
            .id;
        let legacy = ws
            .adopt_or_create_folder(
                &company,
                Some(&root_id),
                "page_builder",
                agent("page_builder"),
            )
            .await
            .unwrap()
            .into_node()
            .id;

        let adopted = ensure_agent_folder(ws.as_ref(), &company, "page_builder")
            .await
            .unwrap();

        assert_eq!(adopted, legacy, "one agent, one folder, across the upgrade");
        let nodes = ws.tree(&company).await.unwrap();
        assert!(
            !nodes.iter().any(|n| n.name == "page-builder"),
            "a rival dashed folder would split the agent's work: {:?}",
            paths(&nodes)
        );
    }

    // -- the created-vs-adopted signal and the compensating rollback (#1801) --

    /// The tracked minter reports whether *this* call created the member folder:
    /// `true` the first time, `false` once it is only adopting what stands. That
    /// signal is what lets a failed write know which folder it, and only it,
    /// brought into existence.
    #[tokio::test]
    async fn ensure_agent_folder_tracked_reports_created_then_adopted() {
        let (_dir, ws) = store().await;
        let company = CompanyId::new("acme");
        ensure_workspace_scaffold(ws.as_ref(), &company)
            .await
            .unwrap();

        let (first, created) = ensure_agent_folder_tracked(ws.as_ref(), &company, "ceo")
            .await
            .unwrap();
        assert!(created, "the first call mints the member folder");

        let (second, created_again) = ensure_agent_folder_tracked(ws.as_ref(), &company, "ceo")
            .await
            .unwrap();
        assert!(!created_again, "a second call adopts rather than minting");
        assert_eq!(first, second, "and hands back the same folder");
    }

    /// A minted folder that never received the write it was made for is swept
    /// when the caller rolls back — leaving no empty `agents/<id>/` for the
    /// Repair button. The reserved root it hangs off is scaffolding and stays.
    #[tokio::test]
    async fn rollback_removes_a_minted_folder_that_stayed_empty() {
        let (_dir, ws) = store().await;
        let company = CompanyId::new("acme");
        ensure_workspace_scaffold(ws.as_ref(), &company)
            .await
            .unwrap();

        let (home, created) = ensure_agent_folder_tracked(ws.as_ref(), &company, "ceo")
            .await
            .unwrap();
        assert!(created);
        assert!(
            tree_paths(&ws, &company)
                .await
                .contains(&"agents/ceo".to_string())
        );

        rollback_empty_minted_folders(ws.as_ref(), &company, &[home]).await;

        let paths = tree_paths(&ws, &company).await;
        assert!(
            !paths.contains(&"agents/ceo".to_string()),
            "an empty minted folder must be swept when its write never landed: {paths:?}"
        );
        assert!(
            paths.contains(&"agents".to_string()),
            "the scaffolded root must survive the rollback: {paths:?}"
        );
    }

    /// The over-deletion guard: a folder that gained a child in the window — a
    /// concurrent create, or the very write the caller thought had failed — is
    /// left exactly as it stands, and its child is never deleted out from under
    /// it by a recursive sweep.
    #[tokio::test]
    async fn rollback_keeps_a_minted_folder_that_gained_a_child() {
        let (_dir, ws) = store().await;
        let company = CompanyId::new("acme");
        ensure_workspace_scaffold(ws.as_ref(), &company)
            .await
            .unwrap();
        let (home, _) = ensure_agent_folder_tracked(ws.as_ref(), &company, "ceo")
            .await
            .unwrap();

        ws.create(
            &company,
            &WorkspaceNode {
                id: "kept-note".to_string(),
                name: "brief.md".to_string(),
                kind: NodeKind::File,
                parent_id: Some(home.clone()),
                updated_at_millis: 1,
                created_by: WorkspaceOrigin::Operator,
                updated_by: WorkspaceOrigin::Operator,
                mime: None,
                size: None,
                sha256: None,
                adopted: false,
            },
            Some("# keep me"),
        )
        .await
        .unwrap();

        rollback_empty_minted_folders(ws.as_ref(), &company, &[home]).await;

        let paths = tree_paths(&ws, &company).await;
        assert!(
            paths.contains(&"agents/ceo".to_string()),
            "a folder that gained a child must survive: {paths:?}"
        );
        assert!(
            paths.contains(&"agents/ceo/brief.md".to_string()),
            "and its child must not be deleted out from under it: {paths:?}"
        );
    }

    /// A store double that, the first time `tree` is called, hands back a
    /// snapshot exactly like the real one below it — and then, *after*
    /// capturing that snapshot but before returning it, writes a child into
    /// `inject_child_under` directly against the wrapped store. This puts the
    /// wrapped store one write ahead of whatever the caller does with the
    /// snapshot it receives — precisely the shape of the race review found: a
    /// concurrent adopter's write landing in the window between a `tree()`
    /// read and a later `delete()` built from it.
    ///
    /// Every other method forwards straight through; only `tree`'s first call
    /// carries the injected write, so a second `tree()` call (e.g. inside
    /// `delete_if_empty`'s own fresh check) sees it, but the snapshot handed
    /// to the *caller* of the first call never does.
    struct InjectChildAfterFirstTree {
        inner: Arc<dyn WorkspaceStore>,
        inject_child_under: String,
        injected: std::sync::atomic::AtomicBool,
    }

    #[async_trait::async_trait]
    impl WorkspaceStore for InjectChildAfterFirstTree {
        async fn tree(&self, company: &CompanyId) -> Result<Vec<WorkspaceNode>> {
            let snapshot = self.inner.tree(company).await?;
            if !self
                .injected
                .swap(true, std::sync::atomic::Ordering::SeqCst)
            {
                self.inner
                    .create(
                        company,
                        &WorkspaceNode {
                            id: "raced-in-note".to_string(),
                            name: "raced-in.md".to_string(),
                            kind: NodeKind::File,
                            parent_id: Some(self.inject_child_under.clone()),
                            updated_at_millis: 1,
                            created_by: WorkspaceOrigin::Operator,
                            updated_by: WorkspaceOrigin::Operator,
                            mime: None,
                            size: None,
                            sha256: None,
                            adopted: false,
                        },
                        Some("landed mid-rollback"),
                    )
                    .await
                    .expect("inject concurrent child");
            }
            Ok(snapshot)
        }

        async fn read(
            &self,
            company: &CompanyId,
            id: &str,
        ) -> Result<Option<(WorkspaceNode, String)>> {
            self.inner.read(company, id).await
        }
        async fn read_capped(
            &self,
            company: &CompanyId,
            id: &str,
            max_bytes: u64,
        ) -> Result<Option<(WorkspaceNode, String, u64)>> {
            self.inner.read_capped(company, id, max_bytes).await
        }

        async fn write(
            &self,
            company: &CompanyId,
            id: &str,
            content: &str,
            author: WorkspaceOrigin,
        ) -> Result<WorkspaceNode> {
            self.inner.write(company, id, content, author).await
        }

        async fn create(
            &self,
            company: &CompanyId,
            node: &WorkspaceNode,
            content: Option<&str>,
        ) -> Result<()> {
            self.inner.create(company, node, content).await
        }

        async fn adopt_or_create_folder(
            &self,
            company: &CompanyId,
            parent: Option<&str>,
            name: &str,
            origin: WorkspaceOrigin,
        ) -> Result<crate::ports::workspace::FolderClaim> {
            self.inner
                .adopt_or_create_folder(company, parent, name, origin)
                .await
        }

        async fn create_binary(
            &self,
            company: &CompanyId,
            node: &WorkspaceNode,
            bytes: &[u8],
        ) -> Result<WorkspaceNode> {
            self.inner.create_binary(company, node, bytes).await
        }

        async fn write_binary(
            &self,
            company: &CompanyId,
            id: &str,
            bytes: &[u8],
            mime: Option<&str>,
            author: WorkspaceOrigin,
        ) -> Result<WorkspaceNode> {
            self.inner
                .write_binary(company, id, bytes, mime, author)
                .await
        }

        async fn read_bytes(
            &self,
            company: &CompanyId,
            id: &str,
        ) -> Result<Option<(WorkspaceNode, crate::ports::workspace::BlobStream)>> {
            self.inner.read_bytes(company, id).await
        }

        async fn rename_move(
            &self,
            company: &CompanyId,
            id: &str,
            name: Option<&str>,
            parent: Option<Option<&str>>,
        ) -> Result<WorkspaceNode> {
            self.inner.rename_move(company, id, name, parent).await
        }

        async fn swap_files(
            &self,
            company: &CompanyId,
            expected_id: Option<&str>,
            replacement_id: &str,
            name: &str,
        ) -> Result<Option<WorkspaceNode>> {
            self.inner
                .swap_files(company, expected_id, replacement_id, name)
                .await
        }

        async fn delete(&self, company: &CompanyId, id: &str) -> Result<bool> {
            self.inner.delete(company, id).await
        }

        // Forwarded explicitly, exactly like the production decorators
        // (`WorkspaceAnnouncer`, `QuotaEnforcedWorkspace`, `DerivedGuardWorkspace`)
        // — so this test exercises the wrapped store's own `delete_if_empty`
        // (here, `FsOps`'s single-lock override) rather than the default trait
        // method re-deriving the check at this wrapper's level.
        async fn delete_if_empty(&self, company: &CompanyId, id: &str) -> Result<bool> {
            self.inner.delete_if_empty(company, id).await
        }

        async fn is_empty(&self, company: &CompanyId) -> Result<bool> {
            self.inner.is_empty(company).await
        }
    }

    /// The race itself: a child lands under a minted-but-empty folder in the
    /// window between `rollback_empty_minted_folders`'s own `tree()` read and
    /// the `delete` it would have issued from that stale read. Before the
    /// fix, `rollback_empty_minted_folders` decided emptiness from that same
    /// stale snapshot and called the unconditional `delete`, which recursed
    /// through the folder and erased the concurrently-landed child with it —
    /// this test fails on that code, asserting the child survives. After the
    /// fix, the decision is `delete_if_empty`'s own fresh re-check, which sees
    /// the child and refuses to remove the folder.
    #[tokio::test]
    async fn rollback_does_not_erase_a_child_that_lands_mid_rollback() {
        let (_dir, real) = store().await;
        let company = CompanyId::new("acme");
        ensure_workspace_scaffold(real.as_ref(), &company)
            .await
            .unwrap();
        let (home, _) = ensure_agent_folder_tracked(real.as_ref(), &company, "ceo")
            .await
            .unwrap();

        let racy: Arc<dyn WorkspaceStore> = Arc::new(InjectChildAfterFirstTree {
            inner: real.clone(),
            inject_child_under: home.clone(),
            injected: std::sync::atomic::AtomicBool::new(false),
        });

        rollback_empty_minted_folders(racy.as_ref(), &company, &[home]).await;

        let paths = tree_paths(&real, &company).await;
        assert!(
            paths.contains(&"agents/ceo".to_string()),
            "a folder a concurrent write landed a child into mid-rollback must survive: {paths:?}"
        );
        assert!(
            paths.contains(&"agents/ceo/raced-in.md".to_string()),
            "the concurrently-landed child must not be erased by the rollback: {paths:?}"
        );
    }

    /// A reserved root is never a rollback target even when handed in: an empty
    /// `agents/` is ordinary boot scaffolding, and the next boot re-lays it.
    #[tokio::test]
    async fn rollback_never_removes_a_reserved_root() {
        let (_dir, ws) = store().await;
        let company = CompanyId::new("acme");
        let root = ws
            .adopt_or_create_folder(&company, None, AGENTS_ROOT, WorkspaceOrigin::Seed)
            .await
            .unwrap()
            .into_node()
            .id;

        rollback_empty_minted_folders(ws.as_ref(), &company, &[root]).await;

        assert!(
            tree_paths(&ws, &company)
                .await
                .contains(&"agents".to_string()),
            "an empty reserved root must never be swept"
        );
    }
}
