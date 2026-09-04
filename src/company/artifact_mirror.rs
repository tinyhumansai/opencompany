//! The seam between a task artifact and the shared workspace tree (issue #552,
//! folding in #327's missing push channel).
//!
//! # The problem this closes
//!
//! `publish_artifact` used to drain into the [`ArtifactStore`] and stop. An
//! artifact is reachable from exactly one place — the Artifacts tab of one
//! card — so a deliverable an agent explicitly published was invisible to the
//! operator browsing the workspace and to every *other* agent, whose only view
//! of shared company state is the note tree. "The CMO wrote the launch brief"
//! had no answer anyone could navigate to.
//!
//! # Two surfaces, one truth
//!
//! A published deliverable now lives twice, and the split is deliberate:
//!
//! * The **artifact chain is authoritative**. It holds the full version
//!   history, the authorship of each revision, and therefore
//!   [`ArtifactRecord::human_edit_diff`] — the one quality datum the artifact
//!   port exists to produce.
//! * The **workspace node is a projection** holding the *current* body only.
//!   It is what makes the deliverable browsable and readable by teammates.
//!
//! The rejected alternative was to make the node the storage and have the
//! artifact reference it. That would push versioning down into
//! [`WorkspaceStore`] across all three backends, turn every artifact read into
//! a two-store join, and re-open the `(task_id, source)` identity contract that
//! #244 settled. A projection costs one extra write; the inversion costs the
//! port.
//!
//! **The invariant**: `node.body == chain.latest().body` after any successful
//! write on either surface.
//!
//! # Ordering: chain first, wherever there is a choice
//!
//! Every path here writes the chain before the node when it can. The two
//! failure modes are not symmetric:
//!
//! * Chain ahead of node — a stale node. Visible, harmless, and self-healing:
//!   the next write on either surface reconciles it.
//! * Node ahead of chain — an edit to a published deliverable that the version
//!   history never recorded. That is silent, permanent, and corrupts
//!   `human_edit_diff`, which is the exact rot the artifact port was built to
//!   prevent.
//!
//! So a failed mirror is logged and tolerated in the first direction and
//! avoided in the second. One path cannot have it: the agent's
//! `workspace_write` tool must complete its compare-and-swap before it knows
//! the write landed at all, so there the node necessarily moves first. It is
//! the narrowest window available rather than a different policy.
//!
//! # The guarantee is owed to deliverables, not to every note
//!
//! "Avoided in the second direction" is a promise about *published* nodes, and
//! it costs something to keep: the reverse lookup runs on every save, so a
//! strict reading would make an unreachable artifact store refuse edits to
//! ordinary notes too — notes with no chain to corrupt, on a save that
//! otherwise never touches that store. That trades the whole tree's
//! availability for a guarantee none of it is owed.
//!
//! [`mirror_node_edit`] therefore separates *cannot record* from *cannot tell*
//! (see [`MirrorOutcome`]). Its callers keep failing closed once a node is
//! known to be a deliverable, and choose for themselves what an unanswerable
//! store means. The console `PUT` takes the availability side and says so in
//! its own doc, including what that costs when the store is down.
//!
//! # Why this module is in the default build
//!
//! [`mirror_node_edit`] has three callers across two layers — the console's
//! workspace `PUT` and artifact-append routes (`src/server/ops/`, always
//! compiled) and the agent's `workspace_write` tool (`src/harness/`, compiled
//! only under the `openhuman` feature). The shared half therefore cannot live
//! in the harness, or the default build could not reach it.

use sha2::{Digest, Sha256};

use crate::Result;
use crate::error::OpenCompanyError;
use crate::ports::artifacts::{ArtifactAuthor, ArtifactRecord, ArtifactStore};
use crate::ports::now_millis;
use crate::ports::types::CompanyId;
use crate::ports::workspace::{NodeKind, WorkspaceNode, WorkspaceOrigin, WorkspaceStore};

use super::workspace_names::{MAX_NAME_BYTES, kebab_name, kebab_name_or};
use super::workspace_scaffold::{ensure_artifact_folder_tracked, rollback_empty_minted_folders};

/// One publish, as [`materialize`] needs it.
///
/// A struct rather than seven positional parameters: five of the fields are
/// `&str`, so a call site that transposed `task_id` and `source` would compile
/// perfectly and file every deliverable in the wrong folder.
#[derive(Debug, Clone, Copy)]
pub struct PublishTarget<'a> {
    /// The agent that published this file — the owner of the `agents/<id>/`
    /// folder it lands under, and the authorship stamped on every node created
    /// or written along the way.
    pub agent_id: &'a str,
    /// The card the publish belongs to. Its id is the immutable half of the
    /// folder name beneath the agent's, so two tasks by one agent cannot
    /// collide on a common filename — and so the folder stays findable by the
    /// id an operator holds.
    pub task_id: &'a str,
    /// The card's human title, when the caller has one (issue #1687).
    ///
    /// The readable half of that folder's name. `None` — a caller with no
    /// board record to hand — names the folder by the id alone, which is what
    /// every folder was called before this.
    pub task_title: Option<&'a str>,
    /// The normalized workspace-relative path the agent published, e.g.
    /// `specs/launch.md`. Interior segments become folders.
    pub source: &'a str,
    /// What to store — the file's text, or its bytes (issue #553). Text lands
    /// as an ordinary note; bytes land as a binary node, which is what stopped
    /// a paid image generation from becoming a dangling digest.
    pub payload: MirrorPayload<'a>,
    /// The node the previous version of this artifact was mirrored into, when
    /// there was one. Reused if it still resolves; see [`materialize`].
    pub existing_node_id: Option<&'a str>,
}

/// One card-less workflow-run artifact.
///
/// Runs cannot use [`PublishTarget`] directly because its second path segment
/// is a task id and a workflow agent node has no card. This target preserves
/// the same author/source/payload contract while giving the mirror the two ids
/// that make the destination unique within a run.
#[derive(Debug, Clone, Copy)]
pub struct RunTarget<'a> {
    /// The roster agent whose sandbox produced the file.
    pub agent_id: &'a str,
    /// The workflow run that owns the capture.
    pub run_id: &'a str,
    /// The graph node whose turn wrote it.
    pub node_id: &'a str,
    /// The normalized path relative to that agent's workspace.
    pub source: &'a str,
    /// The captured file body.
    pub payload: MirrorPayload<'a>,
}

/// What [`materialize`] is being asked to put in the tree.
///
/// Borrowed rather than owned: the drain already holds the bytes it read, and a
/// copy of a 200 MiB video to cross one function boundary would be the single
/// largest allocation on the publish path.
#[derive(Debug, Clone, Copy)]
pub enum MirrorPayload<'a> {
    /// Prose — an editable, diffable, backlinkable note.
    Text(&'a str),
    /// Opaque bytes, with the media type the publisher inferred.
    Bytes {
        /// The file's contents, written verbatim.
        bytes: &'a [u8],
        /// The media type to store the node under.
        mime: &'a str,
    },
}

/// What a publish left in the tree: the node holding it, and — for bytes — the
/// digest **the store computed** while writing it (issue #668).
///
/// The digest is `None` for prose, whose version body is the content itself, so
/// there is nothing a hash would add. For bytes it is the only thing that tells
/// two versions of one deliverable apart, and it comes back from
/// [`WorkspaceStore::create_binary`] / [`write_binary`](WorkspaceStore::write_binary)
/// rather than being computed here — see those methods for why the provenance
/// matters more than the value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mirrored {
    /// The workspace node now holding the deliverable.
    pub node_id: String,
    /// The store's `sha256` of the bytes it wrote, when the payload was bytes.
    pub sha256: Option<String>,
}

/// Put `target`'s body into the shared tree and return what it left there.
///
/// The layout is `artifacts/<agent-id>/<task-title>.<task-id>/<source…>`, the
/// task folder named by [`task_folder_name`] — readable half first, id last so
/// the folder is still findable by the id an operator holds (issue #1687). The
/// agent's folder
/// beneath that root is minted on demand by
/// [`ensure_artifact_folder`](super::workspace_scaffold::ensure_artifact_folder)
/// — member folders appear the first time somebody publishes something, so this
/// must **call** it rather than assume it exists.
///
/// It used to be `agents/<agent-id>/<task-id>/…`, which filed a deliverable in
/// the same folder as its author's scratch notes. Nothing migrates: a record
/// carrying an `existing_node_id` still revises the node it already has, so a
/// company that published before this change keeps its old nodes and its
/// console deep links, and only new paths land under `artifacts/`. A migration
/// would have to move nodes an operator may have organised by hand, to fix
/// something that is untidy rather than wrong.
///
/// # Interior path segments become folders
///
/// `specs/launch.md` lands as `…/<task-id>/specs/launch.md`, not as a file
/// literally named `specs/launch.md`. Flattening to the basename would make
/// `specs/a.md` and `docs/a.md` — two genuinely different deliverables of one
/// task — collide on one node and overwrite each other.
///
/// # Re-publish reuses the node, unless the operator removed it
///
/// `existing_node_id` is reused when it still resolves to a file, so a second
/// publish of the same path revises the note the operator has been reading
/// rather than opening a rival beside it. When it is absent (a pre-#552 record)
/// or no longer resolves (the operator deleted it, and deletions stick), a
/// fresh node is materialized and the *new* version carries the new id. Older
/// versions keep the id of the node that actually held them — honest history,
/// the same shape as `run_id`.
///
/// # Ambiguity is refused, never guessed
///
/// Identity here is by path and no backend enforces unique sibling names, so
/// every lookup is check-then-act. A name carried by a node of the wrong kind,
/// or by more than one node, is a [`Conflict`](OpenCompanyError::Conflict)
/// rather than a coin flip — the same fail-closed rule
/// [`workspace_scaffold`](super::workspace_scaffold) applies one level up.
pub async fn materialize(
    workspace: &dyn WorkspaceStore,
    company: &CompanyId,
    target: PublishTarget<'_>,
) -> Result<Mirrored> {
    // The cheap path, and the common one on a re-publish: the node from last
    // time still exists, so revise it in place and keep every reference to it
    // (the console's deep link, an operator's bookmark) working.
    //
    // A node whose *shape* changed — a markdown draft re-exported as a PDF, or
    // a PDF replaced by prose — cannot be revised in place, because neither
    // write path will convert one kind of node into the other (and the store
    // refuses if asked). Falling through to the path resolution below mints a
    // fresh node of the right kind, which is the same answer this function
    // already gives when the operator deleted the old one: the new version
    // carries the new id, older versions keep the id that actually held them.
    if let Some(existing) = target.existing_node_id
        && let Some((node, _)) = workspace.read(company, existing).await?
        && node.kind == NodeKind::File
        && node.is_binary() == matches!(target.payload, MirrorPayload::Bytes { .. })
    {
        let sha256 = write_payload(workspace, company, existing, target).await?;
        return Ok(Mirrored {
            node_id: existing.to_string(),
            sha256,
        });
    }

    // From here a publish may mint folders before the write that justifies
    // them exists. Track the ones this call freshly created so that a write
    // which then fails does not leave an empty `artifacts/<agent>/…` skeleton
    // standing (issue #1801) — the residual, non-race half of the empty folders
    // the Tidy(#700)/Repair(#759) buttons otherwise have to sweep. The cleanup
    // runs only on error, and removes only a folder that is still empty, so a
    // concurrent publisher that adopted one of these is never disturbed.
    let mut minted: Vec<String> = Vec::new();
    match materialize_fresh(workspace, company, target, &mut minted).await {
        Ok(mirrored) => Ok(mirrored),
        Err(err) => {
            rollback_empty_minted_folders(workspace, company, &minted).await;
            Err(err)
        }
    }
}

/// The path-resolving, folder-minting half of [`materialize`], for a deliverable
/// with no reusable node.
///
/// Split out so [`materialize`] can undo the folders it minted when the write
/// that would have filled them fails (issue #1801). Every freshly created folder
/// id is pushed onto `minted`: the agent folder here, plus each task or interior
/// folder [`resolve_task_folder`] and [`resolve_folder`] mint. The caller cleans
/// them up only on error and only while still empty — see
/// [`rollback_empty_minted_folders`].
async fn materialize_fresh(
    workspace: &dyn WorkspaceStore,
    company: &CompanyId,
    target: PublishTarget<'_>,
    minted: &mut Vec<String>,
) -> Result<Mirrored> {
    let segments = split_source(target.source)?;
    let (dirs, filename) = segments
        .split_last()
        .map(|(last, rest)| (rest, last.as_str()))
        .expect("split_source rejects an empty path");

    let (agent_folder, created) =
        ensure_artifact_folder_tracked(workspace, company, target.agent_id).await?;
    if created {
        minted.push(agent_folder.clone());
    }

    // One tree read, then a walk that keeps its own view current: each folder
    // this creates is pushed onto `nodes`, so a `specs/deep/note.md` resolves
    // its second segment against the first segment it just minted rather than
    // against a snapshot that predates it.
    let mut nodes = workspace.tree(company).await?;
    let mut parent = agent_folder;
    parent = resolve_task_folder(
        workspace,
        company,
        &mut nodes,
        minted,
        &parent,
        target.task_id,
        target.task_title,
        target.agent_id,
    )
    .await?;
    for name in dirs.iter().map(String::as_str) {
        parent = resolve_folder(
            workspace,
            company,
            &mut nodes,
            minted,
            &parent,
            name,
            target.agent_id,
        )
        .await?;
    }

    match resolve_file(&nodes, &parent, filename)? {
        // A node is already there under this exact path — an earlier publish
        // whose id we lost, or a note the agent wrote by hand. Revising it is
        // the only non-destructive answer: minting a rival would leave the path
        // permanently ambiguous, which the tool layer's resolver then refuses
        // for every agent.
        Some(id) => {
            // The same shape guard as above: a node already at this path whose
            // kind disagrees with what is being published cannot be revised,
            // because no write path converts one kind into the other (and the
            // store refuses if asked). It has to be replaced.
            let replace = match workspace.read(company, &id).await? {
                Some((node, _)) => {
                    node.is_binary() != matches!(target.payload, MirrorPayload::Bytes { .. })
                }
                None => false,
            };
            if replace {
                return replace_payload(workspace, company, &parent, filename, &id, target).await;
            }
            let sha256 = write_payload(workspace, company, &id, target).await?;
            Ok(Mirrored {
                node_id: id,
                sha256,
            })
        }
        // Nothing at this path — *as of the read above*. Issue #697: that read
        // is not a claim, so two first publishes of one deliverable both land
        // here and, before this, both created. Two nodes, one name.
        //
        // The state does not decay. `resolve_file` answers a duplicated name
        // with `Conflict`, so a race that lasted microseconds refuses every
        // future publish to that deliverable, for every agent, until somebody
        // edits the tree by hand.
        None => create_first(workspace, company, &parent, filename, target).await,
    }
}

/// A workflow node id's path segment within a run's artifact folder.
///
/// `kebab_name_or` is not injective — `write_up` and `write-up` both normalize
/// to `write-up` — but workflow validation only requires raw node ids to be
/// unique, not their kebab form. Two nodes that collide there would otherwise
/// resolve to the same `materialize_run` destination, and the later capture
/// would silently overwrite the earlier node's output. Appending a short
/// stable hash of the RAW id (computed before normalization) makes the
/// segment collision-resistant while keeping the kebab prefix for
/// readability in the workspace tree.
fn run_node_segment(node_id: &str) -> String {
    let kebab = kebab_name_or(node_id, node_id);
    let digest = Sha256::digest(node_id.as_bytes());
    let mut suffix = String::with_capacity(8);
    for byte in digest.iter().take(4) {
        use std::fmt::Write as _;
        let _ = write!(suffix, "{byte:02x}");
    }
    format!("{kebab}-{suffix}")
}

/// Files a card-less workflow-node output into the shared workspace tree.
///
/// The layout is `artifacts/<agent>/runs/<run>/<node>-<hash>/<source…>`.
/// Reusing [`materialize`] keeps the same path validation, conflict handling,
/// binary storage, and atomic create semantics as task artifacts while the
/// `runs` segment prevents a run id from being mistaken for a task id.
pub async fn materialize_run(
    workspace: &dyn WorkspaceStore,
    company: &CompanyId,
    target: RunTarget<'_>,
) -> Result<Mirrored> {
    let run = kebab_name_or(target.run_id, target.run_id);
    let node = run_node_segment(target.node_id);
    let source = format!("{run}/{node}/{}", target.source);
    materialize(
        workspace,
        company,
        PublishTarget {
            agent_id: target.agent_id,
            task_id: "runs",
            task_title: None,
            source: &source,
            payload: target.payload,
            existing_node_id: None,
        },
    )
    .await
}

/// Publishes a deliverable to a path that nothing occupies yet, and loses
/// rather than duplicates if that stops being true (issue #697).
///
/// # Why this is not just `create_payload`
///
/// It was, and that is the defect. `resolve_file` returning `None` is a
/// statement about the instant it read the tree; a plain create acts on it
/// later, and two publishers that both read "free" both created. The window is
/// small and the damage is permanent, which is the worst combination — nothing
/// cleans up after it and every later publish is refused.
///
/// # The shape is `replace_payload`'s, deliberately
///
/// Stage under a name no publish can produce and [`resolve_file`] will never
/// match, then ask the store to install it conditionally. The only difference
/// is what the caller expects to find: a republish names the node it supersedes,
/// a first publish asserts the name is still free. One primitive answers both
/// (see [`WorkspaceStore::swap_files`]), which is what keeps the loser-cleanup
/// rule — consume the staged node, payload included — in one place rather than
/// two that drift.
///
/// Staging costs the same quota it costs a republish: the payload is charged
/// while it is staged, so a company at its ceiling can be refused here. That is
/// the trade #662 already argued, and a refusal leaves nothing behind.
async fn create_first(
    workspace: &dyn WorkspaceStore,
    company: &CompanyId,
    parent: &str,
    filename: &str,
    target: PublishTarget<'_>,
) -> Result<Mirrored> {
    let staged_name = format!("{filename}.publishing-{}", crate::ports::generate_id());
    let staged = create_payload(
        workspace,
        company,
        Some(parent.to_string()),
        &staged_name,
        target,
    )
    .await?;

    match workspace
        .swap_files(company, None, &staged.node_id, filename)
        .await
    {
        Ok(Some(node)) => Ok(Mirrored {
            node_id: node.id,
            sha256: node.sha256,
        }),
        Ok(None) => Err(OpenCompanyError::Conflict(format!(
            "the deliverable at `{filename}` was created by another publish while this one was \
             being prepared; nothing was overwritten — publish again to revise it"
        ))),
        Err(err) => {
            // The same reasoning as the republish path: an indeterminate store
            // error may or may not have committed, so the staging id is logged
            // for recovery rather than deleted.
            tracing::error!(
                company = %company,
                staged = %staged.node_id,
                name = %staged_name,
                error = %err,
                "[publish] the store could not decide the staged first publish; its id is logged \
                 for recovery rather than deleted after an indeterminate write"
            );
            Err(err)
        }
    }
}

/// Overwrites `node_id` with whatever `target` carries, on the matching path.
async fn write_payload(
    workspace: &dyn WorkspaceStore,
    company: &CompanyId,
    node_id: &str,
    target: PublishTarget<'_>,
) -> Result<Option<String>> {
    match target.payload {
        MirrorPayload::Text(body) => {
            workspace
                .write(company, node_id, body, origin(target.agent_id))
                .await?;
            Ok(None)
        }
        MirrorPayload::Bytes { bytes, mime } => {
            let node = workspace
                .write_binary(company, node_id, bytes, Some(mime), origin(target.agent_id))
                .await?;
            Ok(node.sha256)
        }
    }
}

/// Replaces the node at a path with one of the **other** kind — prose becoming
/// bytes, or the reverse — without a window in which the deliverable does not
/// exist (issue #662).
///
/// # Why not delete-then-create
///
/// That is what this used to do, and it turned a *refused* publish into a
/// destructive one. `create_payload` fails for designed reasons — over
/// `max_blob_mb`, over `tree_quota_gb`, a store error — and quota refusal is an
/// intended outcome of the same work that introduced this path. When it failed,
/// the old deliverable had already been deleted and nothing restored it: the
/// operator was left with an artifact record pointing at a node id that no
/// longer resolved, and the previous deliverable — which was fine — destroyed by
/// a publish that did not succeed.
///
/// # The staging window costs quota, and that changes who succeeds
///
/// The replacement is minted while the superseded node still exists, so both
/// payloads are charged against `tree_quota_gb` until the swap. Delete-first
/// freed the old bytes before asking for the new ones. A company close to its
/// quota republishing a large deliverable is therefore **refused where it
/// previously succeeded** — the end state of a successful publish is unchanged,
/// but which publishes succeed is not.
///
/// That is the right trade, and it is the same one the rest of this doc argues:
/// a refusal leaves the previous deliverable intact and is recoverable by
/// raising the quota or deleting something, where the old behaviour destroyed
/// it. Recorded here because the operator-visible symptom — a quota error on a
/// republish of something that already fits — is otherwise unexplainable.
///
/// The old code argued the deletion was safe because "its history lives on the
/// artifact chain". That holds when the replacement lands and not otherwise, and
/// nothing distinguished the two. It is also **false for a binary**, whose
/// artifact version records neither content nor digest (issue #668) — so the
/// chain recovers nothing. Minting first removes the need for that argument
/// rather than working around it.
///
/// # Why the replacement is staged under another name
///
/// The obvious create-then-delete — mint the replacement at the final path,
/// then remove the old node — briefly puts **two** nodes at one path, and if the
/// delete then fails they stay there. [`resolve_file`] answers a duplicated name
/// with `Conflict`, so that state does not decay: it refuses every future
/// publish to that path, for every agent. Staging under a name nothing resolves
/// keeps the path unambiguous at every instant.
///
/// # The store owns the compare-and-swap
///
/// * **The create fails** — the common case, and the one this issue is about.
///   Nothing has changed: the old deliverable is intact, the path still resolves
///   to it, and the error propagates. Publishing is refused rather than
///   destructive.
/// * **Another publisher wins** — [`WorkspaceStore::swap_files`] consumes this
///   publisher's staging node and returns `None`; the caller receives a conflict
///   and the final path still names exactly the winner.
/// * **The store fails** — the old node remains the compare-and-swap authority.
///   The staging id is logged rather than blindly deleted: a distributed store
///   error can be an indeterminate response to a committed write, and deleting
///   that id could destroy the successful replacement.
async fn replace_payload(
    workspace: &dyn WorkspaceStore,
    company: &CompanyId,
    parent: &str,
    filename: &str,
    superseded: &str,
    target: PublishTarget<'_>,
) -> Result<Mirrored> {
    // A name no publish can produce and `resolve_file` will never match, so the
    // path keeps resolving to exactly one node while the swap is in flight.
    let staged_name = format!("{filename}.publishing-{}", crate::ports::generate_id());
    let staged = create_payload(
        workspace,
        company,
        Some(parent.to_string()),
        &staged_name,
        target,
    )
    .await?;

    match workspace
        // `Some`, emphatically: this is a republish, and it must lose if the
        // node it expected to supersede is no longer the one at the path.
        // `None` here would mean "install only if the name is free", which for
        // a path that is by definition occupied would refuse every republish.
        .swap_files(company, Some(superseded), &staged.node_id, filename)
        .await
    {
        Ok(Some(node)) => Ok(Mirrored {
            node_id: node.id,
            sha256: node.sha256,
        }),
        Ok(None) => Err(OpenCompanyError::Conflict(format!(
            "the deliverable at `{filename}` was replaced by another publish while this one \
             was being prepared; nothing was overwritten — publish again"
        ))),
        Err(err) => {
            tracing::error!(
                company = %company,
                staged = %staged.node_id,
                name = %staged_name,
                error = %err,
                "[publish] the store could not decide the staged replacement; its id is logged \
                 for recovery rather than deleted after an indeterminate write"
            );
            Err(err)
        }
    }
}

/// Creates a fresh node of the right kind holding `target`'s payload.
async fn create_payload(
    workspace: &dyn WorkspaceStore,
    company: &CompanyId,
    parent: Option<String>,
    filename: &str,
    target: PublishTarget<'_>,
) -> Result<Mirrored> {
    let mut node = WorkspaceNode {
        id: crate::ports::generate_id(),
        name: filename.to_string(),
        kind: NodeKind::File,
        parent_id: parent,
        updated_at_millis: now_millis(),
        created_by: origin(target.agent_id),
        updated_by: origin(target.agent_id),
        mime: None,
        size: None,
        sha256: None,
        adopted: false,
    };
    match target.payload {
        MirrorPayload::Text(body) => {
            workspace.create(company, &node, Some(body)).await?;
            Ok(Mirrored {
                node_id: node.id,
                sha256: None,
            })
        }
        MirrorPayload::Bytes { bytes, mime } => {
            node.mime = Some(mime.to_string());
            let stamped = workspace.create_binary(company, &node, bytes).await?;
            Ok(Mirrored {
                node_id: stamped.id,
                sha256: stamped.sha256,
            })
        }
    }
}

/// Record an edit to `node_id` on the artifact chain that owns it, when one
/// does.
///
/// The reverse lookup that keeps the two surfaces from diverging: a workspace
/// node the operator (or an agent) rewrites may be a *published deliverable*,
/// and an edit to one that never reached the version history is exactly the
/// silent corruption the artifact port exists to prevent.
///
/// Answers [`MirrorOutcome::Ordinary`] — and touches nothing — when `node_id`
/// names an ordinary note. Most of the tree is ordinary notes, so this is the
/// common answer and deliberately not an error.
///
/// # Two failures, told apart on purpose
///
/// The lookup and the append fail for different reasons and are not returned
/// alike. A failed **append** is an `Err`: the store answered, so this node is
/// known to be a published deliverable, and the caller must not write the node
/// behind a version that was never recorded. A failed **lookup** is
/// [`MirrorOutcome::Undetermined`] inside `Ok`, because it establishes nothing
/// — the node may be a deliverable or may be one of the ordinary notes that
/// are nearly the whole tree, and only the caller knows whether its own work
/// can proceed without that answer.
///
/// Collapsing the second into [`MirrorOutcome::Ordinary`] would read as "no
/// chain here, carry on" on every store fault, which is precisely how the
/// fail-closed guarantee for deliverables would stop applying without anything
/// appearing to change.
///
/// # The scan, named rather than hidden
///
/// This lists the company's artifacts and looks for one whose *latest* version
/// carries `node_id`. That is a linear scan per save. It is bounded by what
/// artifacts are — a task's drafts and posts, not a repository — and buying an
/// index before there is a workload to size it against would be guessing. The
/// latest version rather than any version is the point: an operator's deletion
/// of a node sticks, so an old version's id names a node that is gone, and
/// matching on it would mirror today's edit into yesterday's history.
pub async fn mirror_node_edit(
    artifacts: &dyn ArtifactStore,
    company: &CompanyId,
    node_id: &str,
    body: &str,
    author: ArtifactAuthor,
    author_id: &str,
    note: Option<String>,
) -> Result<MirrorOutcome> {
    let mut record = match published_record_for_node(artifacts, company, node_id).await {
        Ok(Some(record)) => record,
        Ok(None) => return Ok(MirrorOutcome::Ordinary),
        Err(err) => return Ok(MirrorOutcome::Undetermined(err)),
    };
    let version = record.push_version(body, author, author_id, now_millis(), note);
    // The appended version lives in the same node as the one before it. Without
    // this the *next* edit's reverse lookup — which reads the latest version —
    // would find nothing and silently stop mirroring.
    record.stamp_workspace_node(node_id);
    // Fail-closed, and the one place in this function that is: the lookup
    // succeeded, so this node *is* a deliverable, and a caller that wrote it
    // anyway would leave the history claiming the agent's draft shipped
    // unchanged.
    artifacts.upsert(company, &record).await?;
    Ok(MirrorOutcome::Recorded(MirroredEdit {
        artifact_id: record.id,
        version,
    }))
}

/// What [`mirror_node_edit`] was able to do — and, when it could not act,
/// whether the caller may carry on without it.
///
/// [`Ordinary`](MirrorOutcome::Ordinary) and
/// [`Undetermined`](MirrorOutcome::Undetermined) both mean "nothing was
/// recorded", and that is the whole reason they are separate variants rather
/// than one absent value: the first is a complete answer from a healthy store
/// and the second is no answer at all.
#[derive(Debug)]
pub enum MirrorOutcome {
    /// `node_id` is a published deliverable, and this edit is now a version on
    /// its chain.
    Recorded(MirroredEdit),
    /// The store answered, and `node_id` names no artifact — an ordinary note.
    /// There is no chain here for a node write to get ahead of.
    Ordinary,
    /// The store could not be read, so whether `node_id` is published is
    /// **unknown** rather than "no". Carries the fault so a caller that
    /// tolerates it can still say why in a log.
    Undetermined(OpenCompanyError),
}

/// What [`mirror_node_edit`] appended, for a caller that wants to log or return
/// it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MirroredEdit {
    /// The artifact the edit was recorded on.
    pub artifact_id: String,
    /// The version number the edit became.
    pub version: u32,
}

/// The artifact whose current body lives in `node_id`, if any.
///
/// Shared by [`mirror_node_edit`] and the console's artifact-append route,
/// which needs the same "is this a published deliverable?" answer from the
/// other direction.
pub async fn published_record_for_node(
    artifacts: &dyn ArtifactStore,
    company: &CompanyId,
    node_id: &str,
) -> Result<Option<ArtifactRecord>> {
    Ok(artifacts
        .list(company, None)
        .await?
        .into_iter()
        .find(|record| record.workspace_node_id() == Some(node_id)))
}

/// This agent's authorship stamp. A published deliverable is the agent's work,
/// so every node created or written along its path is attributed to it.
fn origin(agent_id: &str) -> WorkspaceOrigin {
    WorkspaceOrigin::Agent {
        id: agent_id.to_string(),
    }
}

/// Split a normalized publish path into its segments, rejecting anything that
/// cannot name a chain of workspace nodes.
///
/// The publish tool normalizes before it gets here, so this is a guard against
/// a hand-built `PendingPublish` rather than the ordinary path — but a `..`
/// reaching [`WorkspaceStore::create`] as a node *name* would render a
/// traversal-shaped path in the console, and the sqlite and mongodb backends do
/// not reject one.
fn split_source(source: &str) -> Result<Vec<String>> {
    let segments: Vec<&str> = source
        .split('/')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .collect();
    if segments.is_empty() {
        return Err(OpenCompanyError::InvalidRequest(format!(
            "`{source}` names no workspace path segments, so it cannot be published into the tree"
        )));
    }
    for segment in &segments {
        if *segment == "." || *segment == ".." || segment.contains('\\') || segment.contains('\0') {
            return Err(OpenCompanyError::InvalidRequest(format!(
                "`{source}` contains a segment that cannot name a workspace node"
            )));
        }
    }
    // Every segment becomes a node name, so it is minted under the workspace's
    // one naming rule: lowercase and dashed. The sandbox is the agent's own
    // scratch and names files however it likes; the tree is what the operator
    // reads, and `specs/Launch Plan.md` arriving there as `specs/launch-plan.md`
    // is what keeps one document to one spelling.
    //
    // The artifact record's `source` is deliberately *not* rewritten to match:
    // it names the file in the sandbox the agent actually published, and it is
    // the key a republish extends the same record by. Normalizing it would make
    // the record claim a path the agent cannot read back.
    Ok(segments.into_iter().map(kebab_name).collect())
}

/// Adopt-or-create the folder `name` under `parent`, keeping `nodes` current.
///
/// # The snapshot answers, the store decides (issue #759)
///
/// The `nodes` snapshot is a fast path and nothing more: a hit means the folder
/// was already there when the tree was read, and a folder does not stop
/// existing. A *miss* is only a statement about that instant, and the create
/// used to act on it later — so two publishes needing `Agents/<agent>/<task>/`
/// both saw it free and both created, leaving two folders under one name.
///
/// That state does not decay. The `many` arm below answers a duplicated name
/// with `Conflict`, so a race lasting microseconds refuses every later publish
/// beneath that path, for every agent, permanently. The write therefore always
/// goes through [`WorkspaceStore::adopt_or_create_folder`], which decides the
/// contention where it can actually be decided — under the store's own lock,
/// transaction or unique index.
///
/// The node pushed back into the snapshot is the one the **store** returned, so
/// a publisher that adopted somebody else's folder walks on with the winner's
/// id rather than one it invented.
async fn resolve_folder(
    workspace: &dyn WorkspaceStore,
    company: &CompanyId,
    nodes: &mut Vec<WorkspaceNode>,
    minted: &mut Vec<String>,
    parent: &str,
    name: &str,
    agent_id: &str,
) -> Result<String> {
    let matches: Vec<&WorkspaceNode> = children_named(nodes, parent, name);
    match matches.as_slice() {
        [one] if one.kind == NodeKind::Folder => Ok(one.id.clone()),
        [_] => Err(OpenCompanyError::Conflict(format!(
            "`{name}` already exists as a note, not a folder, so a deliverable cannot be published \
             beneath it"
        ))),
        [] => {
            // Only a genuine mint is a rollback candidate (issue #1801): a
            // publisher that adopted somebody else's folder must not have it
            // swept if this publish then fails.
            let claim = workspace
                .adopt_or_create_folder(company, Some(parent), name, origin(agent_id))
                .await?;
            let created = claim.was_created();
            let node = claim.into_node();
            let id = node.id.clone();
            if created {
                minted.push(id.clone());
            }
            nodes.push(node);
            Ok(id)
        }
        many => Err(OpenCompanyError::Conflict(format!(
            "{count} nodes under this folder are named `{name}`, so the path is ambiguous",
            count = many.len()
        ))),
    }
}

/// The name a task's deliverable folder is minted under: the card's title,
/// then its id (issue #1687).
///
/// # Why the title, and why the id is still in it
///
/// The folder used to be named by the card ULID alone. That is a perfectly
/// good *key* and a useless *label*: an operator opening `artifacts/<agent>/`
/// saw a column of `01hq8zm4x…` and could not tell what any of them held
/// without opening each one. The card's title is the one string that already
/// says what the work was.
///
/// The id stays because it is the only thing in the name that is unique and
/// immutable. Dropping it would mean two cards a teammate titled "Weekly
/// update" share one folder and overwrite each other's deliverables, and it
/// would leave an operator holding a card id with nothing in the tree to match
/// it against. Title-then-id also puts the readable half first, which is what
/// survives the explorer's `truncate`.
///
/// # The title half is budgeted, the id half is not
///
/// [`kebab_name`] bounds a whole name at [`MAX_NAME_BYTES`]; here two names are
/// being joined, so the title is trimmed to whatever the id leaves and any
/// separator the cut exposed is trimmed with it. The id is never truncated — a
/// partial ULID is not the id, and matching one is the whole point of
/// [`task_folder_task_id`]. A card whose title normalizes to nothing (an emoji,
/// punctuation) is named by the id alone rather than by `untitled`, which is
/// what [`kebab_name`] would otherwise hand back for every one of them at once.
///
/// # The two halves are joined by a dot, not by a dash
///
/// [`TASK_ID_BOUNDARY`] is the only separator that makes the id half
/// *findable*, which is the whole job of [`resolve_task_folder`]. A dash cannot:
/// a seed card's id is `[a-z0-9-]` (`task_file::normalize_task_id`), so cards
/// `login` and `fix-login` are both legal, and `password-reset-fix-login` ends
/// with `-login` as surely as `password-reset-login` does. Matching on that
/// suffix files one card's deliverables in the other's folder, and once both
/// have published it makes the shorter id ambiguous forever.
///
/// A dot has no such twin. Neither id grammar can produce one — a board card's
/// id is a ULID, a seed card's is `[a-z0-9-]` — so the name's *last* dot is
/// always the join, and the text after it is the whole id and nothing else. It
/// is also still one lawful workspace name: [`kebab_name`] keeps a dot that
/// something precedes, and collapses a dash run, so `--` would fail
/// [`is_kebab_name`](super::workspace_names::is_kebab_name) where this passes.
fn task_folder_name(task_id: &str, task_title: Option<&str>) -> String {
    let id = kebab_name(task_id);
    // An id carrying the boundary itself would put the join in the wrong place,
    // so such a card is named by its id alone — which is what every folder was
    // called before this, and what the `name == id` arm of the lookup still
    // matches. Guarding an input neither id grammar can produce keeps this a
    // total function rather than one with an unstated precondition.
    let Some(title) = task_title.filter(|_| !id.contains(TASK_ID_BOUNDARY)) else {
        return id;
    };
    // `kebab_name_or` falls back **only** when the title normalized to nothing,
    // which is the distinction `kebab_name` flattens: it answers `untitled` both
    // for a card actually titled "Untitled" — which has a perfectly good name —
    // and for one titled "🎉", which has none. Falling back to the id keeps the
    // second off `untitled.<id>` without taking the first's title away.
    let mut slug = kebab_name_or(title, task_id);
    if slug == id {
        return id;
    }
    // `+ 1` for the boundary joining the two halves. Both halves are ASCII by
    // construction (`kebab_name` emits only `[a-z0-9.-]`), so a byte cut is
    // always a character cut.
    let room = MAX_NAME_BYTES.saturating_sub(id.len() + 1);
    if slug.len() > room {
        slug.truncate(room);
    }
    while slug.ends_with('-') || slug.ends_with('.') {
        slug.pop();
    }
    if slug.is_empty() {
        return id;
    }
    format!("{slug}{TASK_ID_BOUNDARY}{id}")
}

/// The character [`task_folder_name`] joins the readable half to the id half
/// with, and therefore the boundary [`task_folder_task_id`] reads back.
const TASK_ID_BOUNDARY: char = '.';

/// The task id `name` was composed around, when it was composed by
/// [`task_folder_name`] at all.
///
/// The **last** boundary rather than the first, because the readable half can
/// hold dots of its own (`v1.2 plan` normalizes to `v1.2-plan`) while the id
/// half can hold none. So the tail is the whole id, exactly, and a lookup on it
/// is an equality test rather than the unbounded suffix match a dash join would
/// force — see [`task_folder_name`] for the card pair that breaks.
fn task_folder_task_id(name: &str) -> Option<&str> {
    name.rsplit_once(TASK_ID_BOUNDARY).map(|(_, id)| id)
}

/// Adopt-or-create the folder holding `task_id`'s deliverables, **matched by
/// id rather than by name** (issue #1687).
///
/// # Why this is not [`resolve_folder`] with a different name
///
/// [`resolve_folder`] matches a name exactly, and a task folder's name is no
/// longer a function of the task alone: it carries the card's title, and a
/// title is editable. An exact-name lookup would therefore stop finding the
/// folder the moment somebody renamed the card, and the next publish would
/// mint a rival beside it — one task, two folders, deliverables split across
/// both. Matching on the id suffix makes the lookup depend only on the half
/// that cannot change.
///
/// The same match is what **adopts** a folder minted before this change, whose
/// name is the bare id: a company that has published already keeps its existing
/// folders and its console deep links, and only a task publishing for the first
/// time gets a titled name. Nothing is renamed, for the reason
/// [`workspace_names`](super::workspace_names) gives at length — an operator
/// must not find their tree rearranged by an upgrade they did not ask for, and
/// a rename breaks every reference anyone kept to the old name.
///
/// # A note wearing the id is refused, even when a folder also matches
///
/// A deliverable cannot be published beneath a note, so a match of the wrong
/// kind is a [`Conflict`](OpenCompanyError::Conflict) rather than a guess —
/// the same fail-closed rule [`resolve_folder`] applies to a name. It is
/// checked **before** any folder is chosen, not only when no folder matched:
/// no backend enforces unique sibling names, so a legacy or imported tree can
/// carry a note and a folder under one name, and publishing into the folder
/// would leave the deliverable at a path `PathIndex` reads as ambiguous — a
/// note the agent that just wrote it could not then open.
///
/// # Two folders for one task: the oldest wins, deterministically
///
/// [`resolve_folder`]'s create is atomic, but it is keyed by the folder's
/// **name**, and two *first* publishes of one task can now compute two
/// different names — one caller holding the card's title and one holding
/// `None` ([`PublishTarget::task_title`]), or a retitle landing between them.
/// Both then see no match, both create, and the tree carries two folders for
/// one task.
///
/// Answering that with `Conflict` would be the worst of the options available:
/// the state does not decay, so a race lasting microseconds would refuse every
/// later publish for that task permanently — the exact failure
/// [`resolve_folder`] documents at length and the store's atomic
/// adopt-or-create exists to prevent. And there is no identity here to guess
/// at, which is what makes this unlike [`resolve_folder`]'s duplicate *name*:
/// both folders were matched on this task's own immutable id, so both are
/// provably its own. The lowest node id therefore wins — node ids are ULIDs, so
/// that is the older of the two, and it is the same answer on every later
/// publish, in every process, on every backend. The deliverables that landed in
/// the loser stay where they are and stay readable; nothing is moved or
/// renamed.
///
/// Closing the window instead of converging after it would need an
/// adopt-or-create keyed by something other than the name — a new
/// [`WorkspaceStore`] method across all three backends, which is a change to
/// the port rather than to this naming rule.
// The `minted` accumulator (issue #1801) pushes this over clippy's threshold;
// the args are the cohesive publish context plus the two walk accumulators, so
// bundling them would trade one honest signature for a struct that exists only
// to satisfy the lint — the same call the repo's other sites make.
#[allow(clippy::too_many_arguments)]
async fn resolve_task_folder(
    workspace: &dyn WorkspaceStore,
    company: &CompanyId,
    nodes: &mut Vec<WorkspaceNode>,
    minted: &mut Vec<String>,
    parent: &str,
    task_id: &str,
    task_title: Option<&str>,
    agent_id: &str,
) -> Result<String> {
    let id = kebab_name(task_id);
    let mut folders: Vec<&str> = Vec::new();
    let mut other_kind = false;
    for node in nodes.iter().filter(|node| {
        node.parent_id.as_deref() == Some(parent)
            && (node.name == id || task_folder_task_id(&node.name) == Some(id.as_str()))
    }) {
        match node.kind {
            NodeKind::Folder => folders.push(&node.id),
            _ => other_kind = true,
        }
    }
    if other_kind {
        return Err(OpenCompanyError::Conflict(format!(
            "`{id}` already exists as a note, not a folder, so a deliverable cannot be \
             published beneath it"
        )));
    }
    if let Some(oldest) = folders.iter().min() {
        return Ok((*oldest).to_string());
    }
    let name = task_folder_name(task_id, task_title);
    resolve_folder(workspace, company, nodes, minted, parent, &name, agent_id).await
}

/// The existing file `name` under `parent`, or `None` when the name is free.
fn resolve_file(nodes: &[WorkspaceNode], parent: &str, name: &str) -> Result<Option<String>> {
    let matches = children_named(nodes, parent, name);
    match matches.as_slice() {
        [one] if one.kind == NodeKind::File => Ok(Some(one.id.clone())),
        [_] => Err(OpenCompanyError::Conflict(format!(
            "`{name}` already exists as a folder, not a note, so a deliverable cannot be published \
             over it"
        ))),
        [] => Ok(None),
        many => Err(OpenCompanyError::Conflict(format!(
            "{count} nodes under this folder are named `{name}`, so the path is ambiguous",
            count = many.len()
        ))),
    }
}

/// Every node directly under `parent` carrying `name`.
fn children_named<'a>(
    nodes: &'a [WorkspaceNode],
    parent: &str,
    name: &str,
) -> Vec<&'a WorkspaceNode> {
    nodes
        .iter()
        .filter(|node| node.parent_id.as_deref() == Some(parent) && node.name == name)
        .collect()
}

#[cfg(test)]
mod test {
    use std::sync::Arc;

    use super::*;
    use crate::company::workspace_scaffold::ARTIFACTS_ROOT;
    use crate::ports::artifacts::ArtifactKind;
    use crate::store::FsOps;

    /// One `FsOps` backing both ports, so a test exercises the real stores
    /// rather than a stub that cannot tell a create from an overwrite.
    fn stores() -> (tempfile::TempDir, Arc<FsOps>, CompanyId) {
        let dir = tempfile::tempdir().expect("tempdir");
        let ops = Arc::new(FsOps::new(dir.path()));
        (dir, ops, CompanyId::new("mirror-co"))
    }

    /// A store that delegates everything to a real backend but **refuses every
    /// create**, which is the shape a quota refusal takes here.
    ///
    /// A wrapper rather than a configured quota because the assertion is about
    /// what `materialize` does when the create fails, not about which limit
    /// produced the failure — and a real limit would tie the test to whichever
    /// cap happens to be tunable.
    struct RefusingCreate(Arc<FsOps>);

    #[async_trait::async_trait]
    impl WorkspaceStore for RefusingCreate {
        async fn tree(&self, company: &CompanyId) -> Result<Vec<WorkspaceNode>> {
            WorkspaceStore::tree(&*self.0, company).await
        }
        async fn read(
            &self,
            company: &CompanyId,
            id: &str,
        ) -> Result<Option<(WorkspaceNode, String)>> {
            WorkspaceStore::read(&*self.0, company, id).await
        }
        async fn read_capped(
            &self,
            company: &CompanyId,
            id: &str,
            max_bytes: u64,
        ) -> Result<Option<(WorkspaceNode, String, u64)>> {
            WorkspaceStore::read_capped(&*self.0, company, id, max_bytes).await
        }
        async fn write(
            &self,
            company: &CompanyId,
            id: &str,
            content: &str,
            author: WorkspaceOrigin,
        ) -> Result<WorkspaceNode> {
            WorkspaceStore::write(&*self.0, company, id, content, author).await
        }
        async fn create(
            &self,
            company: &CompanyId,
            node: &WorkspaceNode,
            content: Option<&str>,
        ) -> Result<()> {
            // Folders still have to be creatable: the scaffold mints the agent
            // and task folders on the way in, and refusing those would fail the
            // publish before it ever reaches the replacement.
            if node.kind == NodeKind::Folder {
                return WorkspaceStore::create(&*self.0, company, node, content).await;
            }
            Err(OpenCompanyError::InvalidRequest("over quota".to_string()))
        }
        /// Folders are claimed for real, for the same reason `create` lets them
        /// through: the scaffold walks `agents/<id>/<task>/` on the way in, and
        /// refusing that would fail the publish before it ever reaches the file
        /// this double exists to refuse.
        async fn adopt_or_create_folder(
            &self,
            company: &CompanyId,
            parent: Option<&str>,
            name: &str,
            origin: WorkspaceOrigin,
        ) -> Result<crate::ports::workspace::FolderClaim> {
            WorkspaceStore::adopt_or_create_folder(&*self.0, company, parent, name, origin).await
        }
        async fn create_binary(
            &self,
            _company: &CompanyId,
            _node: &WorkspaceNode,
            _bytes: &[u8],
        ) -> Result<WorkspaceNode> {
            Err(OpenCompanyError::InvalidRequest("over quota".to_string()))
        }
        async fn write_binary(
            &self,
            company: &CompanyId,
            id: &str,
            bytes: &[u8],
            mime: Option<&str>,
            author: WorkspaceOrigin,
        ) -> Result<WorkspaceNode> {
            WorkspaceStore::write_binary(&*self.0, company, id, bytes, mime, author).await
        }
        async fn read_bytes(
            &self,
            company: &CompanyId,
            id: &str,
        ) -> Result<Option<(WorkspaceNode, crate::ports::workspace::BlobStream)>> {
            WorkspaceStore::read_bytes(&*self.0, company, id).await
        }
        async fn rename_move(
            &self,
            company: &CompanyId,
            id: &str,
            name: Option<&str>,
            parent: Option<Option<&str>>,
        ) -> Result<WorkspaceNode> {
            WorkspaceStore::rename_move(&*self.0, company, id, name, parent).await
        }
        async fn swap_files(
            &self,
            company: &CompanyId,
            expected_id: Option<&str>,
            replacement_id: &str,
            name: &str,
        ) -> Result<Option<WorkspaceNode>> {
            WorkspaceStore::swap_files(&*self.0, company, expected_id, replacement_id, name).await
        }
        async fn delete(&self, company: &CompanyId, id: &str) -> Result<bool> {
            WorkspaceStore::delete(&*self.0, company, id).await
        }
        async fn is_empty(&self, company: &CompanyId) -> Result<bool> {
            WorkspaceStore::is_empty(&*self.0, company).await
        }
    }

    /// [`RefusingCreate`] with a twist: on the note create, a rival publisher
    /// **adopts the just-minted parent folder** before the create is refused
    /// (issue #1839) — the exact mid-write race the adoption lease exists for.
    ///
    /// This is what `RefusingCreate` alone cannot model: there, the folders this
    /// publish minted are swept because nobody else laid a claim on them. Here a
    /// second writer has, so `materialize`'s rollback must find the lease and
    /// leave the folder standing rather than delete the one the rival is about to
    /// write into.
    struct AdoptParentThenRefuse(Arc<FsOps>);

    #[async_trait::async_trait]
    impl WorkspaceStore for AdoptParentThenRefuse {
        async fn tree(&self, company: &CompanyId) -> Result<Vec<WorkspaceNode>> {
            WorkspaceStore::tree(&*self.0, company).await
        }
        async fn read(
            &self,
            company: &CompanyId,
            id: &str,
        ) -> Result<Option<(WorkspaceNode, String)>> {
            WorkspaceStore::read(&*self.0, company, id).await
        }
        async fn read_capped(
            &self,
            company: &CompanyId,
            id: &str,
            max_bytes: u64,
        ) -> Result<Option<(WorkspaceNode, String, u64)>> {
            WorkspaceStore::read_capped(&*self.0, company, id, max_bytes).await
        }
        async fn write(
            &self,
            company: &CompanyId,
            id: &str,
            content: &str,
            author: WorkspaceOrigin,
        ) -> Result<WorkspaceNode> {
            WorkspaceStore::write(&*self.0, company, id, content, author).await
        }
        async fn create(
            &self,
            company: &CompanyId,
            node: &WorkspaceNode,
            content: Option<&str>,
        ) -> Result<()> {
            if node.kind == NodeKind::Folder {
                return WorkspaceStore::create(&*self.0, company, node, content).await;
            }
            // The note create is about to be refused — but first a rival adopts
            // the folder this publish just minted for it, taking the lease. The
            // rival is a real `adopt_or_create_folder` against the inner store,
            // so the flag lands exactly as it would in production.
            if let Some(parent_id) = node.parent_id.as_deref() {
                let nodes = WorkspaceStore::tree(&*self.0, company).await?;
                if let Some(parent) = nodes.iter().find(|n| n.id == parent_id) {
                    WorkspaceStore::adopt_or_create_folder(
                        &*self.0,
                        company,
                        parent.parent_id.as_deref(),
                        &parent.name,
                        WorkspaceOrigin::Operator,
                    )
                    .await?;
                }
            }
            Err(OpenCompanyError::InvalidRequest("over quota".to_string()))
        }
        async fn adopt_or_create_folder(
            &self,
            company: &CompanyId,
            parent: Option<&str>,
            name: &str,
            origin: WorkspaceOrigin,
        ) -> Result<crate::ports::workspace::FolderClaim> {
            WorkspaceStore::adopt_or_create_folder(&*self.0, company, parent, name, origin).await
        }
        async fn create_binary(
            &self,
            _company: &CompanyId,
            _node: &WorkspaceNode,
            _bytes: &[u8],
        ) -> Result<WorkspaceNode> {
            Err(OpenCompanyError::InvalidRequest("over quota".to_string()))
        }
        async fn write_binary(
            &self,
            company: &CompanyId,
            id: &str,
            bytes: &[u8],
            mime: Option<&str>,
            author: WorkspaceOrigin,
        ) -> Result<WorkspaceNode> {
            WorkspaceStore::write_binary(&*self.0, company, id, bytes, mime, author).await
        }
        async fn read_bytes(
            &self,
            company: &CompanyId,
            id: &str,
        ) -> Result<Option<(WorkspaceNode, crate::ports::workspace::BlobStream)>> {
            WorkspaceStore::read_bytes(&*self.0, company, id).await
        }
        async fn rename_move(
            &self,
            company: &CompanyId,
            id: &str,
            name: Option<&str>,
            parent: Option<Option<&str>>,
        ) -> Result<WorkspaceNode> {
            WorkspaceStore::rename_move(&*self.0, company, id, name, parent).await
        }
        async fn swap_files(
            &self,
            company: &CompanyId,
            expected_id: Option<&str>,
            replacement_id: &str,
            name: &str,
        ) -> Result<Option<WorkspaceNode>> {
            WorkspaceStore::swap_files(&*self.0, company, expected_id, replacement_id, name).await
        }
        async fn delete(&self, company: &CompanyId, id: &str) -> Result<bool> {
            WorkspaceStore::delete(&*self.0, company, id).await
        }
        async fn delete_if_empty(&self, company: &CompanyId, id: &str) -> Result<bool> {
            // Forward to the inner backend's override, per the port contract, so
            // the rollback exercises the real fs guard rather than the decorator
            // default.
            WorkspaceStore::delete_if_empty(&*self.0, company, id).await
        }
        async fn is_empty(&self, company: &CompanyId) -> Result<bool> {
            WorkspaceStore::is_empty(&*self.0, company).await
        }
    }

    /// Every node id in the workspace, sorted — the set, which is what `tree`
    /// actually promises.
    async fn sorted_ids(ws: &dyn WorkspaceStore, company: &CompanyId) -> Vec<String> {
        let mut ids: Vec<String> = ws
            .tree(company)
            .await
            .unwrap()
            .into_iter()
            .map(|n| n.id)
            .collect();
        ids.sort();
        ids
    }

    /// A node's rendered path, so an assertion reads as a path rather than a
    /// ULID.
    async fn path_of(ws: &dyn WorkspaceStore, company: &CompanyId, id: &str) -> String {
        let nodes = ws.tree(company).await.unwrap();
        let mut parts = Vec::new();
        let mut cursor = Some(id.to_string());
        while let Some(current) = cursor {
            let Some(node) = nodes.iter().find(|n| n.id == current) else {
                break;
            };
            parts.push(node.name.clone());
            cursor = node.parent_id.clone();
        }
        parts.reverse();
        parts.join("/")
    }

    fn target<'a>(source: &'a str, body: &'a str) -> PublishTarget<'a> {
        PublishTarget {
            agent_id: "cmo",
            task_id: "t-1",
            task_title: None,
            source,
            payload: MirrorPayload::Text(body),
            existing_node_id: None,
        }
    }

    /// The headline: a published deliverable lands in the shared tree, under
    /// `artifacts/<agent-id>/`, attributed to the agent that published it.
    ///
    /// The member folder is asserted rather than assumed because it does not
    /// exist beforehand — member folders are minted on first use (#570), so
    /// this proves `materialize` calls the minter instead of expecting a
    /// folder somebody else laid down. The root it hangs off is the
    /// deliverables root, never the publishing agent's scratch home: filing a
    /// deliverable beside its author's working notes is what made "what has
    /// this company produced?" unanswerable by navigation.
    #[tokio::test]
    async fn a_publish_lands_under_the_agents_own_artifacts_folder_it_mints() {
        let (_dir, ops, co) = stores();
        let ws: &dyn WorkspaceStore = ops.as_ref();

        let id = materialize(ws, &co, target("launch.md", "# Launch"))
            .await
            .expect("materialize")
            .node_id;

        assert_eq!(
            path_of(ws, &co, &id).await,
            format!("{ARTIFACTS_ROOT}/cmo/t-1/launch.md")
        );
        let (node, body) = ws.read(&co, &id).await.unwrap().expect("the node exists");
        assert_eq!(body, "# Launch");
        assert_eq!(node.kind, NodeKind::File);
        assert_eq!(
            node.created_by,
            WorkspaceOrigin::Agent {
                id: "cmo".to_string()
            },
            "a published deliverable is the agent's work, and the tree must say so"
        );
    }

    /// A sandbox path with a space and a capital in it becomes a tree path
    /// under the workspace naming rule.
    ///
    /// The sandbox is the agent's own scratch and it names files however it
    /// likes; the tree is what the operator reads, and one document there has
    /// one spelling. Every interior segment goes through the rule too, not just
    /// the file, or a deliverable would land in `specs/` beside `Specs/`.
    #[tokio::test]
    async fn a_published_path_is_normalized_into_the_tree() {
        let (_dir, ops, co) = stores();
        let ws: &dyn WorkspaceStore = ops.as_ref();

        let id = materialize(ws, &co, target("Specs/Launch Plan.md", "# Launch"))
            .await
            .expect("materialize")
            .node_id;

        assert_eq!(
            path_of(ws, &co, &id).await,
            format!("{ARTIFACTS_ROOT}/cmo/t-1/specs/launch-plan.md")
        );
    }

    /// Two spellings of one sandbox path are one node in the tree, and the
    /// second publish revises the first rather than opening a rival beside it.
    ///
    /// Without this the normalization would be worse than no rule at all: a
    /// path that resolved differently per publish is exactly the ambiguity the
    /// mirror refuses everywhere else.
    #[tokio::test]
    async fn two_spellings_of_one_path_revise_one_node() {
        let (_dir, ops, co) = stores();
        let ws: &dyn WorkspaceStore = ops.as_ref();

        let first = materialize(ws, &co, target("Launch Plan.md", "v1"))
            .await
            .expect("first")
            .node_id;
        let second = materialize(ws, &co, target("launch-plan.md", "v2"))
            .await
            .expect("second")
            .node_id;

        assert_eq!(first, second, "one deliverable, one node");
        let (_, body) = ws.read(&co, &second).await.unwrap().expect("the node");
        assert_eq!(body, "v2");
    }

    /// A **binary** publish lands real bytes in the tree (issue #553).
    ///
    /// This is the payoff of the whole issue: before it, a generated image
    /// became a reference record naming a sandbox path, and wiping the sandbox
    /// left the digest pointing at nothing. Now the same publish produces a
    /// node the operator can open, on every backend.
    #[tokio::test]
    async fn a_binary_publish_lands_real_bytes_under_the_agents_folder() {
        let (_dir, ops, co) = stores();
        let ws: &dyn WorkspaceStore = ops.as_ref();
        let png: Vec<u8> = vec![0x89, b'P', b'N', b'G', 0xff, 0xfe, 0x00];

        let id = materialize(
            ws,
            &co,
            PublishTarget {
                payload: MirrorPayload::Bytes {
                    bytes: &png,
                    mime: "image/png",
                },
                ..target("shots/hero.png", "")
            },
        )
        .await
        .expect("materialize")
        .node_id;

        assert_eq!(
            path_of(ws, &co, &id).await,
            format!("{ARTIFACTS_ROOT}/cmo/t-1/shots/hero.png")
        );
        let (node, stream) = ws
            .read_bytes(&co, &id)
            .await
            .unwrap()
            .expect("the payload is retrievable");
        assert_eq!(node.mime.as_deref(), Some("image/png"));
        assert_eq!(node.size, Some(png.len() as u64));
        assert_eq!(
            node.created_by,
            WorkspaceOrigin::Agent {
                id: "cmo".to_string()
            }
        );
        let mut got = Vec::new();
        {
            use futures::StreamExt;
            let mut stream = stream;
            while let Some(chunk) = stream.next().await {
                got.extend_from_slice(&chunk.unwrap());
            }
        }
        assert_eq!(got, png, "the published bytes are the stored bytes");
    }

    /// Re-publishing the same path as a different shape replaces the node
    /// rather than failing.
    ///
    /// Neither write path converts a note into a payload or back — the store
    /// refuses both — so a markdown draft later re-exported as a PDF would
    /// otherwise error on every publish. The path is the deliverable's
    /// identity, so the node is replaced and the history stays on the artifact
    /// chain.
    #[tokio::test]
    async fn republishing_a_note_as_a_payload_replaces_the_node() {
        let (_dir, ops, co) = stores();
        let ws: &dyn WorkspaceStore = ops.as_ref();

        let first = materialize(ws, &co, target("report.md", "# Draft"))
            .await
            .unwrap()
            .node_id;
        let second = materialize(
            ws,
            &co,
            PublishTarget {
                payload: MirrorPayload::Bytes {
                    bytes: &[0x25, 0x50, 0x44, 0x46],
                    mime: "application/pdf",
                },
                existing_node_id: Some(&first),
                ..target("report.md", "")
            },
        )
        .await
        .expect("a shape change must not fail the publish")
        .node_id;

        let (node, _) = ws
            .read_bytes(&co, &second)
            .await
            .unwrap()
            .expect("the new node holds bytes");
        assert_eq!(node.mime.as_deref(), Some("application/pdf"));
        assert_eq!(
            path_of(ws, &co, &second).await,
            format!("{ARTIFACTS_ROOT}/cmo/t-1/report.md"),
            "the deliverable keeps its path"
        );
        assert!(
            ws.read(&co, &first).await.unwrap().is_none(),
            "the superseded node is gone, not left beside its replacement"
        );
    }

    /// The reverse shape change is equally important: a generated payload can
    /// later be republished as editable prose without asking either write API
    /// to violate its type guard.
    #[tokio::test]
    async fn republishing_a_payload_as_a_note_replaces_the_node() {
        let (_dir, ops, co) = stores();
        let ws: &dyn WorkspaceStore = ops.as_ref();

        let first = materialize(
            ws,
            &co,
            PublishTarget {
                payload: MirrorPayload::Bytes {
                    bytes: &[0x25, 0x50, 0x44, 0x46],
                    mime: "application/pdf",
                },
                ..target("report.md", "")
            },
        )
        .await
        .unwrap()
        .node_id;
        let second = materialize(
            ws,
            &co,
            PublishTarget {
                existing_node_id: Some(&first),
                ..target("report.md", "# Editable")
            },
        )
        .await
        .expect("a binary-to-text shape change must succeed")
        .node_id;

        let (node, body) = ws.read(&co, &second).await.unwrap().unwrap();
        assert_eq!(body, "# Editable");
        assert!(!node.is_binary());
        assert_eq!(
            path_of(ws, &co, &second).await,
            format!("{ARTIFACTS_ROOT}/cmo/t-1/report.md")
        );
        assert!(ws.read_bytes(&co, &first).await.unwrap().is_none());

        // …and the staging name does not survive. The text side stages through
        // plain `create` rather than `create_binary`, so it is a different pair
        // of store calls from the text→bytes case above: a swap that promoted
        // the node but left a sibling behind would satisfy every assertion
        // before this one.
        let nodes = ws.tree(&co).await.unwrap();
        let parent = node
            .parent_id
            .clone()
            .expect("the replacement has a parent");
        let siblings: Vec<&WorkspaceNode> = nodes
            .iter()
            .filter(|n| n.parent_id.as_deref() == Some(parent.as_str()))
            .collect();
        assert_eq!(
            siblings.iter().filter(|n| n.name == "report.md").count(),
            1,
            "exactly one node carries the deliverable's name: {siblings:?}"
        );
        assert!(
            !siblings.iter().any(|n| n.name.contains(".publishing-")),
            "no staging name may survive a successful publish: {siblings:?}"
        );
    }

    /// A real store whose swap boundary pauses until two publishers have both
    /// staged their payloads. This makes the race deterministic without
    /// replacing the compare-and-swap implementation under test.
    struct PausedSwap(Arc<FsOps>, Arc<tokio::sync::Barrier>);

    #[async_trait::async_trait]
    impl WorkspaceStore for PausedSwap {
        async fn tree(&self, company: &CompanyId) -> Result<Vec<WorkspaceNode>> {
            WorkspaceStore::tree(&*self.0, company).await
        }
        async fn read(
            &self,
            company: &CompanyId,
            id: &str,
        ) -> Result<Option<(WorkspaceNode, String)>> {
            WorkspaceStore::read(&*self.0, company, id).await
        }
        async fn read_capped(
            &self,
            company: &CompanyId,
            id: &str,
            max_bytes: u64,
        ) -> Result<Option<(WorkspaceNode, String, u64)>> {
            WorkspaceStore::read_capped(&*self.0, company, id, max_bytes).await
        }
        async fn write(
            &self,
            company: &CompanyId,
            id: &str,
            content: &str,
            author: WorkspaceOrigin,
        ) -> Result<WorkspaceNode> {
            WorkspaceStore::write(&*self.0, company, id, content, author).await
        }
        async fn create(
            &self,
            company: &CompanyId,
            node: &WorkspaceNode,
            content: Option<&str>,
        ) -> Result<()> {
            WorkspaceStore::create(&*self.0, company, node, content).await
        }
        async fn create_binary(
            &self,
            company: &CompanyId,
            node: &WorkspaceNode,
            bytes: &[u8],
        ) -> Result<WorkspaceNode> {
            WorkspaceStore::create_binary(&*self.0, company, node, bytes).await
        }
        async fn write_binary(
            &self,
            company: &CompanyId,
            id: &str,
            bytes: &[u8],
            mime: Option<&str>,
            author: WorkspaceOrigin,
        ) -> Result<WorkspaceNode> {
            WorkspaceStore::write_binary(&*self.0, company, id, bytes, mime, author).await
        }
        async fn read_bytes(
            &self,
            company: &CompanyId,
            id: &str,
        ) -> Result<Option<(WorkspaceNode, crate::ports::workspace::BlobStream)>> {
            WorkspaceStore::read_bytes(&*self.0, company, id).await
        }
        async fn rename_move(
            &self,
            company: &CompanyId,
            id: &str,
            name: Option<&str>,
            parent: Option<Option<&str>>,
        ) -> Result<WorkspaceNode> {
            WorkspaceStore::rename_move(&*self.0, company, id, name, parent).await
        }
        async fn adopt_or_create_folder(
            &self,
            company: &CompanyId,
            parent: Option<&str>,
            name: &str,
            origin: WorkspaceOrigin,
        ) -> Result<crate::ports::workspace::FolderClaim> {
            WorkspaceStore::adopt_or_create_folder(&*self.0, company, parent, name, origin).await
        }
        async fn swap_files(
            &self,
            company: &CompanyId,
            expected_id: Option<&str>,
            replacement_id: &str,
            name: &str,
        ) -> Result<Option<WorkspaceNode>> {
            self.1.wait().await;
            WorkspaceStore::swap_files(&*self.0, company, expected_id, replacement_id, name).await
        }
        async fn delete(&self, company: &CompanyId, id: &str) -> Result<bool> {
            WorkspaceStore::delete(&*self.0, company, id).await
        }
        async fn is_empty(&self, company: &CompanyId) -> Result<bool> {
            WorkspaceStore::is_empty(&*self.0, company).await
        }
    }

    /// Two publishers can prepare the same shape-changing path concurrently.
    /// Both must reach the real store's swap boundary before either proceeds;
    /// exactly one wins, and the loser cannot create a duplicate final path or
    /// leak its staging node.
    #[tokio::test]
    async fn concurrent_shape_changes_have_one_winner_and_no_duplicate_path() {
        let (_dir, ops, co) = stores();
        let ws: &dyn WorkspaceStore = ops.as_ref();

        let first = materialize(ws, &co, target("report.md", "# Draft"))
            .await
            .unwrap()
            .node_id;

        let racing = PausedSwap(ops.clone(), Arc::new(tokio::sync::Barrier::new(2)));
        let left = materialize(
            &racing,
            &co,
            PublishTarget {
                payload: MirrorPayload::Bytes {
                    bytes: &[0x25, 0x50, 0x44, 0x46],
                    mime: "application/pdf",
                },
                existing_node_id: Some(&first),
                ..target("report.md", "")
            },
        );
        let right = materialize(
            &racing,
            &co,
            PublishTarget {
                payload: MirrorPayload::Bytes {
                    bytes: &[0x89, b'P', b'N', b'G'],
                    mime: "image/png",
                },
                existing_node_id: Some(&first),
                ..target("report.md", "")
            },
        );
        let (left, right) = tokio::join!(left, right);
        let outcomes = [left, right];
        assert_eq!(
            outcomes.iter().filter(|result| result.is_ok()).count(),
            1,
            "exactly one compare-and-swap must win: {outcomes:?}"
        );
        let loser = outcomes
            .iter()
            .find_map(|result| result.as_ref().err())
            .expect("one publisher loses");
        assert!(
            loser.to_string().contains("another publish"),
            "the refusal must say what happened: {loser}"
        );

        let nodes = ws.tree(&co).await.unwrap();
        let named: Vec<&WorkspaceNode> = nodes.iter().filter(|n| n.name == "report.md").collect();
        assert_eq!(
            named.len(),
            1,
            "the final path must continuously have one winner: {named:?}"
        );
        assert!(
            !nodes.iter().any(|n| n.name.contains(".publishing-")),
            "the losing compare-and-swap must consume its staged node: {nodes:?}"
        );
    }

    /// Issue #697, the sibling race: two **first** publishes of a path that
    /// does not exist yet.
    ///
    /// Both resolve the path to `None` — correctly, at the instant they look —
    /// and before the fix both then created, leaving two nodes under one name.
    /// That state does not decay: `resolve_file` answers a duplicated name with
    /// `Conflict`, so a race lasting microseconds refuses every later publish to
    /// that deliverable, for every agent, permanently.
    ///
    /// Reuses `PausedSwap` unchanged, which is the point of routing creates
    /// through the same primitive: both publishers are held at the store's
    /// compare-and-swap boundary and released together, so the interleaving is
    /// forced rather than hoped for. A test that merely ran two publishes
    /// concurrently would pass on a machine that happened to serialize them.
    #[tokio::test]
    async fn two_first_publishes_of_one_path_have_one_winner_and_no_duplicate() {
        let (_dir, ops, co) = stores();
        let ws: &dyn WorkspaceStore = ops.as_ref();

        // A different deliverable is published first, purely to mint the agent
        // and task folders the racers will share. Without it each publisher
        // walks `ensure_artifact_folder` / `resolve_folder` itself and mints its
        // OWN parent, so the two `report.md` nodes land under different folders
        // and never contend for one path — the test would pass while asserting
        // nothing about the race it names. (That folder walk is racy in its own
        // right; it is a separate defect from this one and is not what this
        // test pins.)
        materialize(ws, &co, target("seed.md", "# Seed"))
            .await
            .expect("seeding the shared folders");

        // The path under test must still not exist: this is the create arm.
        let before = ws.tree(&co).await.unwrap();
        assert!(
            !before.iter().any(|n| n.name == "report.md"),
            "the race is about a path that does not exist yet: {before:?}"
        );

        let racing = PausedSwap(ops.clone(), Arc::new(tokio::sync::Barrier::new(2)));
        let left = materialize(&racing, &co, target("report.md", "# From the left"));
        let right = materialize(&racing, &co, target("report.md", "# From the right"));
        let (left, right) = tokio::join!(left, right);
        let outcomes = [left, right];

        assert_eq!(
            outcomes.iter().filter(|result| result.is_ok()).count(),
            1,
            "exactly one first publish may create the path: {outcomes:?}"
        );
        let loser = outcomes
            .iter()
            .find_map(|result| result.as_ref().err())
            .expect("one publisher loses");
        assert!(
            loser.to_string().contains("another publish"),
            "the refusal must say what happened: {loser}"
        );

        let nodes = ws.tree(&co).await.unwrap();
        let named: Vec<&WorkspaceNode> = nodes.iter().filter(|n| n.name == "report.md").collect();
        assert_eq!(
            named.len(),
            1,
            "one name, one node — a duplicate here is permanent: {named:?}"
        );
        assert!(
            !nodes.iter().any(|n| n.name.contains(".publishing-")),
            "the loser must consume its staged node: {nodes:?}"
        );

        // The path stays publishable. This is the assertion that speaks to why
        // the issue ranks the defect as it does: a duplicate would make every
        // future publish refuse, so proving the winner can still be revised is
        // proving the damage did not happen.
        materialize(ws, &co, target("report.md", "# A later revision"))
            .await
            .expect("the surviving path must still accept a publish");
        let after = ws.tree(&co).await.unwrap();
        assert_eq!(
            after.iter().filter(|n| n.name == "report.md").count(),
            1,
            "and revising it must not fork the path either: {after:?}"
        );
    }

    /// A real store that holds the first `arrivals` `tree()` reads at a
    /// two-party barrier, so two publishers provably act on the *same* snapshot.
    ///
    /// # Why the tree read and not the folder write
    ///
    /// This is the race's own precondition: both publishers read "that folder is
    /// not there", and before issue #759 both then created. Pausing where the
    /// snapshot is taken forces exactly that interleaving without replacing any
    /// of the code under test — the folder claim, whichever backend decides it,
    /// runs for real afterwards.
    ///
    /// It is also the only pause point that exists **on both sides of the fix**,
    /// which is what lets these two tests be run against the base commit to
    /// watch them fail. A barrier inside the new primitive could only ever
    /// observe the fixed code.
    ///
    /// `arrivals` is a budget rather than a switch: after that many `tree()`
    /// calls the barrier is bypassed, so a publisher that fails early (which is
    /// exactly what the *unfixed* code does) cannot strand its partner waiting
    /// for a rendezvous that will never come.
    struct PausedTreeRead {
        inner: Arc<FsOps>,
        barrier: Arc<tokio::sync::Barrier>,
        arrivals: std::sync::atomic::AtomicUsize,
    }

    impl PausedTreeRead {
        fn new(inner: Arc<FsOps>, arrivals: usize) -> Self {
            Self {
                inner,
                barrier: Arc::new(tokio::sync::Barrier::new(2)),
                arrivals: std::sync::atomic::AtomicUsize::new(arrivals),
            }
        }
    }

    #[async_trait::async_trait]
    impl WorkspaceStore for PausedTreeRead {
        async fn tree(&self, company: &CompanyId) -> Result<Vec<WorkspaceNode>> {
            let budget = self
                .arrivals
                .fetch_update(
                    std::sync::atomic::Ordering::SeqCst,
                    std::sync::atomic::Ordering::SeqCst,
                    |left| left.checked_sub(1),
                )
                .is_ok();
            if budget {
                self.barrier.wait().await;
            }
            WorkspaceStore::tree(&*self.inner, company).await
        }
        async fn read(
            &self,
            company: &CompanyId,
            id: &str,
        ) -> Result<Option<(WorkspaceNode, String)>> {
            WorkspaceStore::read(&*self.inner, company, id).await
        }
        async fn read_capped(
            &self,
            company: &CompanyId,
            id: &str,
            max_bytes: u64,
        ) -> Result<Option<(WorkspaceNode, String, u64)>> {
            WorkspaceStore::read_capped(&*self.inner, company, id, max_bytes).await
        }
        async fn write(
            &self,
            company: &CompanyId,
            id: &str,
            content: &str,
            author: WorkspaceOrigin,
        ) -> Result<WorkspaceNode> {
            WorkspaceStore::write(&*self.inner, company, id, content, author).await
        }
        async fn create(
            &self,
            company: &CompanyId,
            node: &WorkspaceNode,
            content: Option<&str>,
        ) -> Result<()> {
            WorkspaceStore::create(&*self.inner, company, node, content).await
        }
        async fn adopt_or_create_folder(
            &self,
            company: &CompanyId,
            parent: Option<&str>,
            name: &str,
            origin: WorkspaceOrigin,
        ) -> Result<crate::ports::workspace::FolderClaim> {
            WorkspaceStore::adopt_or_create_folder(&*self.inner, company, parent, name, origin)
                .await
        }
        async fn create_binary(
            &self,
            company: &CompanyId,
            node: &WorkspaceNode,
            bytes: &[u8],
        ) -> Result<WorkspaceNode> {
            WorkspaceStore::create_binary(&*self.inner, company, node, bytes).await
        }
        async fn write_binary(
            &self,
            company: &CompanyId,
            id: &str,
            bytes: &[u8],
            mime: Option<&str>,
            author: WorkspaceOrigin,
        ) -> Result<WorkspaceNode> {
            WorkspaceStore::write_binary(&*self.inner, company, id, bytes, mime, author).await
        }
        async fn read_bytes(
            &self,
            company: &CompanyId,
            id: &str,
        ) -> Result<Option<(WorkspaceNode, crate::ports::workspace::BlobStream)>> {
            WorkspaceStore::read_bytes(&*self.inner, company, id).await
        }
        async fn rename_move(
            &self,
            company: &CompanyId,
            id: &str,
            name: Option<&str>,
            parent: Option<Option<&str>>,
        ) -> Result<WorkspaceNode> {
            WorkspaceStore::rename_move(&*self.inner, company, id, name, parent).await
        }
        async fn swap_files(
            &self,
            company: &CompanyId,
            expected_id: Option<&str>,
            replacement_id: &str,
            name: &str,
        ) -> Result<Option<WorkspaceNode>> {
            WorkspaceStore::swap_files(&*self.inner, company, expected_id, replacement_id, name)
                .await
        }
        async fn delete(&self, company: &CompanyId, id: &str) -> Result<bool> {
            WorkspaceStore::delete(&*self.inner, company, id).await
        }
        async fn is_empty(&self, company: &CompanyId) -> Result<bool> {
            WorkspaceStore::is_empty(&*self.inner, company).await
        }
    }

    /// Every node under `parent` carrying `name`, by id.
    fn named_children(nodes: &[WorkspaceNode], parent: &str, name: &str) -> Vec<String> {
        nodes
            .iter()
            .filter(|n| n.parent_id.as_deref() == Some(parent) && n.name == name)
            .map(|n| n.id.clone())
            .collect()
    }

    /// Issue #759, the folder half of the publish walk: two publishers needing
    /// the same **task folder** that does not exist yet.
    ///
    /// The filenames differ on purpose. Issue #697 already made two publishers
    /// contending for one *file* path resolve to a single winner, so a test
    /// using one filename would be satisfied by that fix alone and would say
    /// nothing about the folder above it. Different names means both files must
    /// land — and they can only both land if the folder they land in is one
    /// folder.
    ///
    /// The last assertion is the one that speaks to severity. A duplicated
    /// folder is not a transient: `resolve_folder`'s ambiguity arm answers
    /// `Conflict` for every later publish beneath that path, for every agent,
    /// permanently. Proving a third publish still works is proving the momentary
    /// race did not become a standing outage.
    #[tokio::test]
    async fn two_publishes_needing_one_task_folder_share_it_rather_than_duplicating_it() {
        let (_dir, ops, co) = stores();
        let ws: &dyn WorkspaceStore = ops.as_ref();

        // Seed only the agent folder, by publishing something for a different
        // task. The task folder under test must still be absent — that is the
        // create arm this test exists for.
        materialize(
            ws,
            &co,
            PublishTarget {
                task_id: "t-seed",
                task_title: None,
                ..target("seed.md", "# Seed")
            },
        )
        .await
        .expect("seeding `agents/cmo/`");
        let agent_folder = ws
            .tree(&co)
            .await
            .unwrap()
            .into_iter()
            .find(|n| n.name == "cmo")
            .expect("the agent folder exists")
            .id;
        assert!(
            named_children(&ws.tree(&co).await.unwrap(), &agent_folder, "t-9").is_empty(),
            "the race is about a task folder that does not exist yet"
        );

        // Four arrivals: each publisher reads the tree twice on this path — once
        // in the member-folder minter, once in `materialize` itself — and the
        // second rendezvous is the one that puts both of them on a snapshot with
        // no `t-9` in it.
        let racing = PausedTreeRead::new(ops.clone(), 4);
        let left = materialize(
            &racing,
            &co,
            PublishTarget {
                task_id: "t-9",
                task_title: None,
                ..target("left.md", "# From the left")
            },
        );
        let right = materialize(
            &racing,
            &co,
            PublishTarget {
                task_id: "t-9",
                task_title: None,
                ..target("right.md", "# From the right")
            },
        );
        let (left, right) = tokio::join!(left, right);
        let left = left.expect("the left publish must succeed");
        let right = right.expect("the right publish must succeed");

        let nodes = ws.tree(&co).await.unwrap();
        let task_folders = named_children(&nodes, &agent_folder, "t-9");
        assert_eq!(
            task_folders.len(),
            1,
            "one task folder, or every later publish beneath it is refused forever: {nodes:?}"
        );
        let task_folder = &task_folders[0];

        for (id, name) in [(&left.node_id, "left.md"), (&right.node_id, "right.md")] {
            let node = nodes
                .iter()
                .find(|n| &n.id == id)
                .unwrap_or_else(|| panic!("the published node for {name} is in the tree"));
            assert_eq!(node.name, name);
            assert_eq!(
                node.parent_id.as_deref(),
                Some(task_folder.as_str()),
                "both deliverables must land in the one task folder"
            );
        }

        // …and the path stays publishable, which a duplicate would have ended.
        materialize(
            ws,
            &co,
            PublishTarget {
                task_id: "t-9",
                task_title: None,
                ..target("later.md", "# A later publish")
            },
        )
        .await
        .expect("the shared task folder must still accept a publish");
        let after = ws.tree(&co).await.unwrap();
        assert_eq!(
            named_children(&after, &agent_folder, "t-9").len(),
            1,
            "and it must still be one folder: {after:?}"
        );
    }

    /// The same race one level up, on the folders the *scaffold* mints:
    /// `agents/` and `agents/<agent-id>/`.
    ///
    /// Nothing is seeded, so both publishers read an empty tree and both need
    /// the root and the agent's own folder. Different task ids keep the task
    /// folders apart, so the only thing they can contend for is the pair
    /// `ensure_member_folder` claims — which is what pins that conversion
    /// independently of `resolve_folder`'s.
    ///
    /// Two arrivals rather than four: the rendezvous that matters is the member
    /// minter's tree read, and budgeting only that one leaves a publisher that
    /// fails there (the unfixed behaviour) unable to strand its partner.
    #[tokio::test]
    async fn two_publishers_minting_one_agent_folder_share_it_rather_than_duplicating_it() {
        let (_dir, ops, co) = stores();
        let ws: &dyn WorkspaceStore = ops.as_ref();
        assert!(ws.is_empty(&co).await.unwrap(), "nothing is seeded");

        let racing = PausedTreeRead::new(ops.clone(), 2);
        let left = materialize(
            &racing,
            &co,
            PublishTarget {
                task_id: "t-left",
                task_title: None,
                ..target("left.md", "# From the left")
            },
        );
        let right = materialize(
            &racing,
            &co,
            PublishTarget {
                task_id: "t-right",
                task_title: None,
                ..target("right.md", "# From the right")
            },
        );
        let (left, right) = tokio::join!(left, right);
        left.expect("the left publish must succeed");
        right.expect("the right publish must succeed");

        let nodes = ws.tree(&co).await.unwrap();
        let roots: Vec<&WorkspaceNode> = nodes
            .iter()
            .filter(|n| n.parent_id.is_none() && n.name == ARTIFACTS_ROOT)
            .collect();
        assert_eq!(
            roots.len(),
            1,
            "one `{ARTIFACTS_ROOT}` root — two would make every agent folder ambiguous: {nodes:?}"
        );
        assert_eq!(
            named_children(&nodes, &roots[0].id, "cmo").len(),
            1,
            "one folder for the agent, or the agent can never publish again: {nodes:?}"
        );

        // The whole subtree stays usable afterwards.
        materialize(
            ws,
            &co,
            PublishTarget {
                task_id: "t-later",
                task_title: None,
                ..target("later.md", "# A later publish")
            },
        )
        .await
        .expect("the agent's folder must still accept a publish");
    }

    /// Issue #662. A shape-changing publish whose replacement **fails** must
    /// leave the previous deliverable exactly where it was.
    ///
    /// This is the defect: the old code deleted first, so a refused create —
    /// over `max_blob_mb`, over `tree_quota_gb`, any store error — destroyed a
    /// deliverable that was fine, and left the artifact record pointing at a
    /// node id that no longer resolved. Quota refusal is a *designed* outcome of
    /// this path, so the window was never theoretical.
    #[tokio::test]
    async fn a_failed_replacement_leaves_the_previous_deliverable_intact() {
        let (_dir, ops, co) = stores();
        let ws: &dyn WorkspaceStore = ops.as_ref();

        let first = materialize(ws, &co, target("report.md", "# Draft"))
            .await
            .unwrap()
            .node_id;
        let before = path_of(ws, &co, &first).await;

        // The same publish as the test above, but the store refuses to create
        // the binary node — the shape every quota refusal takes here.
        let refusing = RefusingCreate(ops.clone());
        let err = materialize(
            &refusing,
            &co,
            PublishTarget {
                payload: MirrorPayload::Bytes {
                    bytes: &[0x25, 0x50, 0x44, 0x46],
                    mime: "application/pdf",
                },
                existing_node_id: Some(&first),
                ..target("report.md", "")
            },
        )
        .await
        .expect_err("the refused create must fail the publish");
        assert!(
            err.to_string().contains("over quota"),
            "the store's own refusal must reach the caller: {err}"
        );

        let (node, body) = ws
            .read(&co, &first)
            .await
            .unwrap()
            .expect("the previous deliverable must still exist");
        assert_eq!(body, "# Draft", "its content is untouched");
        assert_eq!(node.name, "report.md");
        assert_eq!(
            path_of(ws, &co, &first).await,
            before,
            "and it still sits at the deliverable's path"
        );
    }

    /// The other half: a refused replacement must not leave the staged node
    /// behind either. The workspace has to look exactly as it did before the
    /// publish was attempted — one node at the path, and nothing beside it.
    #[tokio::test]
    async fn a_failed_replacement_leaves_no_staged_node_behind() {
        let (_dir, ops, co) = stores();
        let ws: &dyn WorkspaceStore = ops.as_ref();

        let first = materialize(ws, &co, target("report.md", "# Draft"))
            .await
            .unwrap()
            .node_id;
        // Sorted: `tree` promises the set, not an order, so comparing raw
        // sequences would fail on a reshuffle that changed nothing.
        let before = sorted_ids(ws, &co).await;

        let refusing = RefusingCreate(ops.clone());
        let _ = materialize(
            &refusing,
            &co,
            PublishTarget {
                payload: MirrorPayload::Bytes {
                    bytes: &[0x25, 0x50, 0x44, 0x46],
                    mime: "application/pdf",
                },
                existing_node_id: Some(&first),
                ..target("report.md", "")
            },
        )
        .await;

        let after = sorted_ids(ws, &co).await;
        assert_eq!(
            before, after,
            "a refused publish must change nothing at all"
        );
    }

    /// The success path still ends with exactly ONE node at the path — the
    /// staging name is an implementation detail that must never survive.
    ///
    /// Without this, the staged replacement could silently ship under
    /// `report.md.publishing-<id>` and every assertion about the failure paths
    /// would still pass.
    #[tokio::test]
    async fn a_successful_replacement_leaves_one_node_at_the_path() {
        let (_dir, ops, co) = stores();
        let ws: &dyn WorkspaceStore = ops.as_ref();

        let first = materialize(ws, &co, target("report.md", "# Draft"))
            .await
            .unwrap()
            .node_id;
        let second = materialize(
            ws,
            &co,
            PublishTarget {
                payload: MirrorPayload::Bytes {
                    bytes: &[0x25, 0x50, 0x44, 0x46],
                    mime: "application/pdf",
                },
                existing_node_id: Some(&first),
                ..target("report.md", "")
            },
        )
        .await
        .expect("the replacement lands")
        .node_id;

        let nodes = ws.tree(&co).await.unwrap();
        let parent = nodes
            .iter()
            .find(|n| n.id == second)
            .and_then(|n| n.parent_id.clone())
            .expect("the replacement has a parent");
        let named: Vec<&WorkspaceNode> = nodes
            .iter()
            .filter(|n| n.parent_id.as_deref() == Some(parent.as_str()))
            .collect();
        assert_eq!(
            named.iter().filter(|n| n.name == "report.md").count(),
            1,
            "exactly one node carries the deliverable's name: {named:?}"
        );
        assert!(
            !named.iter().any(|n| n.name.contains(".publishing-")),
            "no staging name may survive a successful publish: {named:?}"
        );
    }

    /// Interior segments become folders. Flattening to the basename would make
    /// two genuinely different deliverables of one task collide on one node —
    /// so the same basename in two directories must be two nodes.
    #[tokio::test]
    async fn the_same_basename_in_two_directories_is_two_nodes() {
        let (_dir, ops, co) = stores();
        let ws: &dyn WorkspaceStore = ops.as_ref();

        let spec = materialize(ws, &co, target("specs/a.md", "spec body"))
            .await
            .unwrap()
            .node_id;
        let doc = materialize(ws, &co, target("docs/a.md", "doc body"))
            .await
            .unwrap()
            .node_id;

        assert_ne!(spec, doc, "one node for two paths would lose a deliverable");
        assert_eq!(
            path_of(ws, &co, &spec).await,
            format!("{ARTIFACTS_ROOT}/cmo/t-1/specs/a.md")
        );
        assert_eq!(
            path_of(ws, &co, &doc).await,
            format!("{ARTIFACTS_ROOT}/cmo/t-1/docs/a.md")
        );
        assert_eq!(ws.read(&co, &spec).await.unwrap().unwrap().1, "spec body");
        assert_eq!(ws.read(&co, &doc).await.unwrap().unwrap().1, "doc body");
    }

    /// Re-publishing with the node from last time revises **that** node, so the
    /// operator's open tab, deep link and backlinks all keep working. Nothing
    /// new is created.
    #[tokio::test]
    async fn a_republish_revises_the_same_node() {
        let (_dir, ops, co) = stores();
        let ws: &dyn WorkspaceStore = ops.as_ref();

        let first = materialize(ws, &co, target("launch.md", "draft one"))
            .await
            .unwrap()
            .node_id;
        let before = ws.tree(&co).await.unwrap().len();

        let again = materialize(
            ws,
            &co,
            PublishTarget {
                existing_node_id: Some(&first),
                ..target("launch.md", "draft two")
            },
        )
        .await
        .unwrap()
        .node_id;

        assert_eq!(again, first, "a re-publish must not open a rival node");
        assert_eq!(ws.tree(&co).await.unwrap().len(), before, "nothing created");
        assert_eq!(ws.read(&co, &first).await.unwrap().unwrap().1, "draft two");
    }

    /// The operator's deletions stick. A re-publish whose remembered node is
    /// gone mints a fresh one rather than resurrecting the old id — and the
    /// path is the same, so the deliverable reappears where it belongs.
    #[tokio::test]
    async fn a_republish_after_the_operator_deleted_the_node_mints_a_fresh_one() {
        let (_dir, ops, co) = stores();
        let ws: &dyn WorkspaceStore = ops.as_ref();

        let first = materialize(ws, &co, target("launch.md", "draft one"))
            .await
            .unwrap()
            .node_id;
        assert!(ws.delete(&co, &first).await.unwrap());

        let again = materialize(
            ws,
            &co,
            PublishTarget {
                existing_node_id: Some(&first),
                ..target("launch.md", "draft two")
            },
        )
        .await
        .unwrap()
        .node_id;

        assert_ne!(again, first, "a deleted node must not be resurrected by id");
        assert_eq!(
            path_of(ws, &co, &again).await,
            format!("{ARTIFACTS_ROOT}/cmo/t-1/launch.md"),
            "the replacement belongs at the same path"
        );
        assert_eq!(ws.read(&co, &again).await.unwrap().unwrap().1, "draft two");
    }

    /// Losing the id but not the node — a pre-#552 record re-published — must
    /// adopt what is already at the path rather than mint a duplicate beside
    /// it. Two nodes on one path is precisely the ambiguity the tool layer's
    /// resolver then refuses for every agent.
    #[tokio::test]
    async fn a_publish_over_an_existing_path_adopts_it_rather_than_duplicating() {
        let (_dir, ops, co) = stores();
        let ws: &dyn WorkspaceStore = ops.as_ref();

        let first = materialize(ws, &co, target("launch.md", "draft one"))
            .await
            .unwrap()
            .node_id;
        // `existing_node_id: None` is exactly what a pre-#552 artifact carries.
        let again = materialize(ws, &co, target("launch.md", "draft two"))
            .await
            .unwrap()
            .node_id;

        assert_eq!(again, first, "the node already at the path must be adopted");
        assert_eq!(ws.read(&co, &first).await.unwrap().unwrap().1, "draft two");
    }

    /// A folder sitting where the note should go is refused, not overwritten:
    /// the deliverable would otherwise vanish into a name that resolves to
    /// something else entirely.
    #[tokio::test]
    async fn a_folder_in_the_notes_place_is_refused() {
        let (_dir, ops, co) = stores();
        let ws: &dyn WorkspaceStore = ops.as_ref();

        // Publish once to lay down `agents/cmo/t-1/`, then put a folder where
        // the next publish's note wants to be.
        let sibling = materialize(ws, &co, target("other.md", "x"))
            .await
            .unwrap()
            .node_id;
        let parent = ws
            .read(&co, &sibling)
            .await
            .unwrap()
            .unwrap()
            .0
            .parent_id
            .unwrap();
        ws.adopt_or_create_folder(&co, Some(&parent), "launch.md", WorkspaceOrigin::Operator)
            .await
            .expect("claim the folder standing in the note's place");

        let refused = materialize(ws, &co, target("launch.md", "body"))
            .await
            .expect_err("a folder in the note's place must be refused");
        assert!(
            refused.to_string().contains("already exists as a folder"),
            "unexpected refusal: {refused}"
        );
    }

    /// A traversal segment reaching `create` as a node *name* would render a
    /// path the console cannot navigate, and the sqlite/mongodb backends do not
    /// reject one — so the guard lives here.
    #[tokio::test]
    async fn a_traversal_segment_is_refused() {
        let (_dir, ops, co) = stores();
        let ws: &dyn WorkspaceStore = ops.as_ref();

        for bad in ["../escape.md", "specs/../../escape.md", "   "] {
            assert!(
                materialize(ws, &co, target(bad, "body")).await.is_err(),
                "`{bad}` must not name a workspace path"
            );
        }
    }

    // -- mirror_node_edit ---------------------------------------------------

    /// An edit to a published node is recorded on the artifact chain, as a new
    /// version by the editing author — which is what keeps `human_edit_diff`
    /// answerable after the operator revises a deliverable in the console.
    #[tokio::test]
    async fn an_edit_to_a_published_node_appends_an_operator_version() {
        let (_dir, ops, co) = stores();
        let artifacts: &dyn ArtifactStore = ops.as_ref();

        let mut record = ArtifactRecord::new(
            "a-1",
            "t-1",
            "Launch",
            ArtifactKind::Markdown,
            "agent draft",
            "cmo",
            1,
        );
        record.stamp_workspace_node("node-1");
        artifacts.upsert(&co, &record).await.unwrap();

        let MirrorOutcome::Recorded(mirrored) = mirror_node_edit(
            artifacts,
            &co,
            "node-1",
            "operator draft",
            ArtifactAuthor::Operator,
            "operator",
            Some("operator edit before approval".to_string()),
        )
        .await
        .unwrap() else {
            panic!("a published node's edit is recorded");
        };

        assert_eq!(mirrored.artifact_id, "a-1");
        assert_eq!(mirrored.version, 2);

        let stored = artifacts.get(&co, "a-1").await.unwrap().unwrap();
        assert_eq!(stored.latest().unwrap().body, "operator draft");
        assert_eq!(stored.latest().unwrap().author, ArtifactAuthor::Operator);
        assert_eq!(
            stored.workspace_node_id(),
            Some("node-1"),
            "the appended version must carry the node too, or the NEXT edit's \
             reverse lookup finds nothing and mirroring silently stops"
        );
        assert!(
            stored.human_edit_diff().is_some(),
            "the whole reason the chain must see console edits"
        );
    }

    /// Most of the tree is ordinary notes. Editing one touches no artifact and
    /// is not an error — the common answer, and deliberately silent.
    #[tokio::test]
    async fn an_edit_to_an_unpublished_node_records_nothing() {
        let (_dir, ops, co) = stores();
        let artifacts: &dyn ArtifactStore = ops.as_ref();

        let mut published = ArtifactRecord::new(
            "a-1",
            "t-1",
            "Launch",
            ArtifactKind::Markdown,
            "body",
            "cmo",
            1,
        );
        published.stamp_workspace_node("node-1");
        artifacts.upsert(&co, &published).await.unwrap();

        let mirrored = mirror_node_edit(
            artifacts,
            &co,
            "some-other-note",
            "new body",
            ArtifactAuthor::Operator,
            "operator",
            None,
        )
        .await
        .unwrap();

        assert!(matches!(mirrored, MirrorOutcome::Ordinary), "{mirrored:?}");
        let stored = artifacts.get(&co, "a-1").await.unwrap().unwrap();
        assert_eq!(
            stored.versions.len(),
            1,
            "an unrelated note must not append"
        );
    }

    /// The lookup matches the **latest** version's node, not any version's. An
    /// artifact whose node the operator deleted and which was re-published into
    /// a new one must mirror into the new node — matching on the stale id would
    /// write today's edit into yesterday's history.
    #[tokio::test]
    async fn the_lookup_matches_the_current_node_not_a_retired_one() {
        let (_dir, ops, co) = stores();
        let artifacts: &dyn ArtifactStore = ops.as_ref();

        let mut record = ArtifactRecord::new(
            "a-1",
            "t-1",
            "Launch",
            ArtifactKind::Markdown,
            "v1",
            "cmo",
            1,
        );
        record.stamp_workspace_node("node-old");
        record.push_version("v2", ArtifactAuthor::Agent, "cmo", 2, None);
        record.stamp_workspace_node("node-new");
        artifacts.upsert(&co, &record).await.unwrap();

        assert!(
            matches!(
                mirror_node_edit(
                    artifacts,
                    &co,
                    "node-old",
                    "edit",
                    ArtifactAuthor::Operator,
                    "operator",
                    None,
                )
                .await
                .unwrap(),
                MirrorOutcome::Ordinary
            ),
            "the retired node no longer addresses this artifact"
        );
        assert!(matches!(
            mirror_node_edit(
                artifacts,
                &co,
                "node-new",
                "edit",
                ArtifactAuthor::Operator,
                "operator",
                None,
            )
            .await
            .unwrap(),
            MirrorOutcome::Recorded(_)
        ));
    }

    // -- the two store faults, told apart --------------------------------

    /// An artifact store with one chosen fault, so a test can ask for exactly
    /// the failure it means: unreadable (`list`) or unwritable (`upsert`).
    struct FaultyArtifacts {
        listed: Vec<ArtifactRecord>,
        list_fails: bool,
        upsert_fails: bool,
    }

    #[async_trait::async_trait]
    impl ArtifactStore for FaultyArtifacts {
        async fn list(&self, _: &CompanyId, _: Option<&str>) -> Result<Vec<ArtifactRecord>> {
            if self.list_fails {
                return Err(OpenCompanyError::Store("the artifact store is down".into()));
            }
            Ok(self.listed.clone())
        }
        async fn get(&self, _: &CompanyId, _: &str) -> Result<Option<ArtifactRecord>> {
            Ok(None)
        }
        async fn upsert(&self, _: &CompanyId, _: &ArtifactRecord) -> Result<()> {
            if self.upsert_fails {
                return Err(OpenCompanyError::Store("the disk is full".into()));
            }
            Ok(())
        }
        async fn delete(&self, _: &CompanyId, _: &str) -> Result<bool> {
            Ok(false)
        }
    }

    fn published_as(node_id: &str) -> ArtifactRecord {
        let mut record = ArtifactRecord::new(
            "a-1",
            "t-1",
            "Launch",
            ArtifactKind::Markdown,
            "agent draft",
            "cmo",
            1,
        );
        record.stamp_workspace_node(node_id);
        record
    }

    /// A store that cannot be listed establishes **nothing**, and must not be
    /// reported as the ordinary-note answer.
    ///
    /// This is the variant that carries the whole guarantee: `Ordinary` is what
    /// callers are entitled to write a node behind. If a read fault collapsed
    /// into it, every published deliverable would silently lose its fail-closed
    /// protection the moment the store got sick — the one moment it matters.
    #[tokio::test]
    async fn an_unreadable_store_is_undetermined_not_ordinary() {
        let co = CompanyId::new("acme");
        let artifacts = FaultyArtifacts {
            listed: Vec::new(),
            list_fails: true,
            upsert_fails: false,
        };

        let outcome = mirror_node_edit(
            &artifacts,
            &co,
            "node-1",
            "edit",
            ArtifactAuthor::Operator,
            "operator",
            None,
        )
        .await
        .expect("an unreadable store is the caller's decision, not an error");

        assert!(
            matches!(outcome, MirrorOutcome::Undetermined(_)),
            "a read fault must stay distinguishable from `Ordinary`: {outcome:?}"
        );
    }

    /// Once the store has answered and named this node a deliverable, a version
    /// that cannot be appended is an error — the caller must not go on to write
    /// the node, because that is the silent, permanent direction.
    #[tokio::test]
    async fn a_refused_append_on_a_published_node_still_fails_closed() {
        let co = CompanyId::new("acme");
        let artifacts = FaultyArtifacts {
            listed: vec![published_as("node-1")],
            list_fails: false,
            upsert_fails: true,
        };

        assert!(
            mirror_node_edit(
                &artifacts,
                &co,
                "node-1",
                "edit",
                ArtifactAuthor::Operator,
                "operator",
                None,
            )
            .await
            .is_err(),
            "a known deliverable whose version cannot be recorded must refuse the save"
        );
    }

    /// Issue #1687: the task folder is named for the *work*, and still carries
    /// the id.
    ///
    /// The whole complaint is legibility. `artifacts/cmo/01hq8zm4x…/` is a
    /// perfectly good key and tells an operator scanning the tree nothing
    /// whatsoever — every sibling looks identical, and finding the most recent
    /// one means opening each. The title goes first because the explorer pane
    /// truncates from the right; the id stays because it is the only unique,
    /// immutable half and it is what an operator holding a card id matches
    /// against.
    #[tokio::test]
    async fn a_task_folder_is_named_for_the_card_and_keeps_its_id() {
        let (_dir, ops, co) = stores();
        let ws: &dyn WorkspaceStore = ops.as_ref();

        let id = materialize(
            ws,
            &co,
            PublishTarget {
                task_title: Some("Q3 Launch Brief"),
                ..target("launch.md", "# Launch")
            },
        )
        .await
        .expect("materialize")
        .node_id;

        assert_eq!(
            path_of(ws, &co, &id).await,
            format!("{ARTIFACTS_ROOT}/cmo/q3-launch-brief.t-1/launch.md"),
            "the folder must read as the work and still end in the card id"
        );
    }

    /// Renaming a card must not split its deliverables across two folders.
    ///
    /// The folder name is now a function of an **editable** string, so an
    /// exact-name lookup would miss the folder the moment somebody retitled
    /// the card and the next publish would mint a rival beside it. The lookup
    /// therefore matches the id suffix, which is the half that cannot change —
    /// and the existing folder keeps the name it was minted under, because
    /// nothing in this runtime renames a node an operator may have linked to.
    #[tokio::test]
    async fn a_publish_after_the_card_was_retitled_stays_in_the_same_folder() {
        let (_dir, ops, co) = stores();
        let ws: &dyn WorkspaceStore = ops.as_ref();

        let first = materialize(
            ws,
            &co,
            PublishTarget {
                task_title: Some("Q3 Launch Brief"),
                ..target("launch.md", "# Launch")
            },
        )
        .await
        .expect("first publish")
        .node_id;
        let second = materialize(
            ws,
            &co,
            PublishTarget {
                task_title: Some("Q3 Launch Brief (revised scope)"),
                ..target("timeline.md", "# Timeline")
            },
        )
        .await
        .expect("second publish")
        .node_id;

        assert_eq!(
            path_of(ws, &co, &first).await,
            format!("{ARTIFACTS_ROOT}/cmo/q3-launch-brief.t-1/launch.md")
        );
        assert_eq!(
            path_of(ws, &co, &second).await,
            format!("{ARTIFACTS_ROOT}/cmo/q3-launch-brief.t-1/timeline.md"),
            "a retitled card must publish into the folder it already has, not a second one"
        );
    }

    /// A company that published before this change keeps the folders it has.
    ///
    /// Its task folders are named by the bare id, and the id-suffix lookup
    /// matches those too — so the next publish *adopts* the existing folder
    /// rather than opening a titled twin beside it and splitting one task's
    /// deliverables in half. Nothing is renamed: an operator must not find
    /// their tree rearranged, and a rename breaks every link kept to the old
    /// name.
    #[tokio::test]
    async fn a_folder_minted_before_titles_is_adopted_rather_than_twinned() {
        let (_dir, ops, co) = stores();
        let ws: &dyn WorkspaceStore = ops.as_ref();

        let legacy = materialize(ws, &co, target("launch.md", "# Launch"))
            .await
            .expect("legacy publish")
            .node_id;
        assert_eq!(
            path_of(ws, &co, &legacy).await,
            format!("{ARTIFACTS_ROOT}/cmo/t-1/launch.md"),
            "a caller with no title still names the folder by the id alone"
        );

        let titled = materialize(
            ws,
            &co,
            PublishTarget {
                task_title: Some("Q3 Launch Brief"),
                ..target("timeline.md", "# Timeline")
            },
        )
        .await
        .expect("titled publish")
        .node_id;

        assert_eq!(
            path_of(ws, &co, &titled).await,
            format!("{ARTIFACTS_ROOT}/cmo/t-1/timeline.md"),
            "the pre-existing id-named folder must be adopted, not joined by a titled twin"
        );
    }

    /// The name is composed under the same rule every other minted name obeys,
    /// and the id half survives every input the title half can throw at it.
    #[test]
    fn a_task_folder_name_is_readable_first_and_addressable_last() {
        use super::super::workspace_names::is_kebab_name;

        let ulid = "01hq8zm4xk3n7y2p9v1w5c8t4b";

        // No title at all — a caller with no board record — is exactly what
        // every folder was called before this.
        assert_eq!(task_folder_name(ulid, None), ulid);

        // The ordinary case: readable half first, id last, one workspace name.
        let named = task_folder_name(ulid, Some("Q3 Launch Brief"));
        assert_eq!(named, format!("q3-launch-brief.{ulid}"));
        assert!(is_kebab_name(&named), "{named}");

        // A title that normalizes to nothing must not collapse every such card
        // onto `untitled.<id>`; the id alone is both shorter and truer.
        assert_eq!(task_folder_name(ulid, Some("🎉 ✨")), ulid);
        assert_eq!(task_folder_name(ulid, Some("   ")), ulid);

        // But a card an operator actually titled "Untitled" has a real name,
        // and it must keep it: `kebab_name` answers `untitled` for that title
        // and for `🎉` alike, and only the second of those has nothing to say.
        assert_eq!(
            task_folder_name(ulid, Some("Untitled")),
            format!("untitled.{ulid}"),
            "a real title must not be mistaken for the fallback it collides with"
        );

        // The id half is read back whole, whatever the title half held — the
        // one property the lookup depends on.
        assert_eq!(
            task_folder_task_id(&task_folder_name("fix-login", Some("v1.2 Plan"))),
            Some("fix-login")
        );

        // A long title is trimmed to whatever the id leaves — never the id, a
        // partial ULID being no id at all — and leaves no dangling separator.
        let long = task_folder_name(ulid, Some(&"Very Long Card Title ".repeat(20)));
        assert!(long.len() <= MAX_NAME_BYTES, "{} bytes", long.len());
        assert!(long.ends_with(ulid), "{long}");
        assert!(is_kebab_name(&long), "{long}");
    }

    /// One card's id being the tail of another's must not file one card's
    /// deliverables in the other's folder.
    ///
    /// A seed card's id is `[a-z0-9-]` (`task_file::normalize_task_id`), so
    /// `login` and `fix-login` are both legal ids on one board, and a dash
    /// join would leave `password-reset-fix-login` ending in `-login` exactly
    /// as `login`'s own folder does. The shorter card would then adopt the
    /// longer card's folder — silently, and overwriting its deliverables
    /// wherever the two publish the same source path — and once both had
    /// published, every later lookup for `login` would match two folders. The
    /// dot boundary makes the id half an equality test rather than a suffix
    /// search, so neither can reach the other.
    #[tokio::test]
    async fn a_card_whose_id_ends_in_another_cards_id_keeps_its_own_folder() {
        let (_dir, ops, co) = stores();
        let ws: &dyn WorkspaceStore = ops.as_ref();

        let longer = materialize(
            ws,
            &co,
            PublishTarget {
                task_id: "fix-login",
                task_title: Some("Password reset"),
                ..target("notes.md", "# Notes")
            },
        )
        .await
        .expect("publish for `fix-login`")
        .node_id;
        assert_eq!(
            path_of(ws, &co, &longer).await,
            format!("{ARTIFACTS_ROOT}/cmo/password-reset.fix-login/notes.md")
        );

        let shorter = materialize(
            ws,
            &co,
            PublishTarget {
                task_id: "login",
                task_title: Some("Login page"),
                ..target("spec.md", "# Spec")
            },
        )
        .await
        .expect("publish for `login`")
        .node_id;
        assert_eq!(
            path_of(ws, &co, &shorter).await,
            format!("{ARTIFACTS_ROOT}/cmo/login-page.login/spec.md"),
            "`login` must mint its own folder, not adopt the one `fix-login` already has"
        );
    }

    /// A note wearing the task's id is refused even when a folder matches too.
    ///
    /// No backend enforces unique sibling names, so a legacy or imported tree
    /// can carry a note and a folder under one name. Publishing into the folder
    /// would land the deliverable at a path the agents' `PathIndex` reads as
    /// ambiguous — a note the agent that just wrote it could not open again —
    /// so the wrong kind is checked before any folder is chosen, not only when
    /// no folder matched.
    #[tokio::test]
    async fn a_note_wearing_the_task_id_is_refused_even_beside_a_matching_folder() {
        let (_dir, ops, co) = stores();
        let ws: &dyn WorkspaceStore = ops.as_ref();

        let first = materialize(
            ws,
            &co,
            PublishTarget {
                task_title: Some("Launch brief"),
                ..target("launch.md", "# Launch")
            },
        )
        .await
        .expect("first publish")
        .node_id;
        let tree = ws.tree(&co).await.expect("tree");
        let published = tree.iter().find(|n| n.id == first).expect("published node");
        let task_folder = tree
            .iter()
            .find(|n| Some(&n.id) == published.parent_id.as_ref())
            .expect("task folder");
        let agent_folder = task_folder.parent_id.clone().expect("agent folder");

        // The shape an import can leave behind: a note carrying the bare task
        // id, beside the folder that actually holds the deliverables.
        let note = WorkspaceNode {
            id: "imported-note".to_string(),
            name: "t-1".to_string(),
            kind: NodeKind::File,
            parent_id: Some(agent_folder),
            updated_at_millis: 1,
            created_by: origin("cmo"),
            updated_by: origin("cmo"),
            mime: None,
            size: None,
            sha256: None,
            adopted: false,
        };
        ws.create(&co, &note, Some("imported"))
            .await
            .expect("imported note");

        let refused = materialize(
            ws,
            &co,
            PublishTarget {
                task_title: Some("Launch brief"),
                ..target("timeline.md", "# Timeline")
            },
        )
        .await;
        assert!(
            matches!(refused, Err(OpenCompanyError::Conflict(_))),
            "a matching note must fail the publish closed rather than be stepped over: {refused:?}"
        );
    }

    /// Two folders for one task converge on one of them, rather than refusing
    /// every publish that follows.
    ///
    /// The create is atomic under the store's lock but keyed by *name*, and two
    /// first publishes of one card can compute two names — one caller holding
    /// the title and one holding `None`. Answering the result with `Conflict`
    /// would make a race lasting microseconds refuse that task's publishes
    /// permanently, which is the failure `resolve_folder` exists to prevent.
    /// Both folders were matched on the card's own immutable id, so there is no
    /// identity to guess at: the older wins, on every later publish and in
    /// every process.
    #[tokio::test]
    async fn two_folders_for_one_task_resolve_to_the_oldest_rather_than_refusing() {
        let (_dir, ops, co) = stores();
        let ws: &dyn WorkspaceStore = ops.as_ref();

        let first = materialize(ws, &co, target("launch.md", "# Launch"))
            .await
            .expect("untitled first publish")
            .node_id;
        let tree = ws.tree(&co).await.expect("tree");
        let published = tree.iter().find(|n| n.id == first).expect("published node");
        let task_folder = tree
            .iter()
            .find(|n| Some(&n.id) == published.parent_id.as_ref())
            .expect("task folder");
        assert_eq!(task_folder.name, "t-1");
        let agent_folder = task_folder.parent_id.clone().expect("agent folder");

        // What a concurrent first publish holding the card's title would have
        // left behind: the same task, under a second name.
        let twin = ws
            .adopt_or_create_folder(&co, Some(&agent_folder), "launch-brief.t-1", origin("cmo"))
            .await
            .expect("twin folder")
            .into_node();
        assert!(
            twin.id > task_folder.id,
            "the twin must be the younger of the two for this to prove anything"
        );

        let second = materialize(
            ws,
            &co,
            PublishTarget {
                task_title: Some("Launch brief"),
                ..target("timeline.md", "# Timeline")
            },
        )
        .await
        .expect("a task with two folders must still be publishable")
        .node_id;
        assert_eq!(
            path_of(ws, &co, &second).await,
            format!("{ARTIFACTS_ROOT}/cmo/t-1/timeline.md"),
            "the older of the two folders must win, and win the same way every time"
        );
    }

    // -- no empty / duplicate folders (#1801) -------------------------------

    /// Publishing one deliverable twice lands exactly one `artifacts/<agent>/`
    /// and one task folder beneath it, and leaves no empty folder anywhere —
    /// the compensating rollback added for the failure path must not fire on a
    /// publish that succeeds.
    #[tokio::test]
    async fn republishing_one_deliverable_leaves_one_folder_chain_and_no_empties() {
        let (_dir, ops, co) = stores();
        let ws: &dyn WorkspaceStore = ops.as_ref();

        let first = materialize(ws, &co, target("launch.md", "v1"))
            .await
            .unwrap()
            .node_id;
        let second = materialize(ws, &co, target("launch.md", "v2"))
            .await
            .unwrap()
            .node_id;
        assert_eq!(first, second, "one deliverable, one node");

        let nodes = ws.tree(&co).await.unwrap();
        assert_eq!(
            nodes.iter().filter(|n| n.name == "cmo").count(),
            1,
            "exactly one agent folder: {nodes:?}"
        );
        assert_eq!(
            nodes
                .iter()
                .filter(|n| n.name == "t-1" || task_folder_task_id(&n.name) == Some("t-1"))
                .count(),
            1,
            "exactly one task folder: {nodes:?}"
        );
        for folder in nodes.iter().filter(|n| n.kind == NodeKind::Folder) {
            assert!(
                nodes
                    .iter()
                    .any(|n| n.parent_id.as_deref() == Some(folder.id.as_str())),
                "`{}` is an empty folder a successful publish must not leave: {nodes:?}",
                folder.name
            );
        }

        let (_, body) = ws.read(&co, &second).await.unwrap().unwrap();
        assert_eq!(body, "v2");
    }

    /// A first publish whose note create is refused **after** its folders were
    /// minted must leave no empty `artifacts/<agent>/…` skeleton behind — the
    /// residual, non-race seam this issue is about. `RefusingCreate` mints
    /// folders but refuses the note, the exact shape a quota refusal takes on a
    /// first publish.
    #[tokio::test]
    async fn a_refused_first_publish_leaves_no_empty_folders() {
        let (_dir, ops, co) = stores();
        let ws: &dyn WorkspaceStore = ops.as_ref();
        let refusing = RefusingCreate(ops.clone());

        let err = materialize(&refusing, &co, target("launch.md", "# Launch"))
            .await
            .expect_err("the refused note create must fail the publish");
        assert!(
            err.to_string().contains("over quota"),
            "the store's refusal must reach the caller: {err}"
        );

        let nodes = ws.tree(&co).await.unwrap();
        assert!(
            !nodes.iter().any(|n| n.name == "cmo"),
            "the empty agent folder minted for the refused publish must be swept: {nodes:?}"
        );
        assert!(
            !nodes
                .iter()
                .any(|n| n.name == "t-1" || task_folder_task_id(&n.name) == Some("t-1")),
            "and so must the empty task folder: {nodes:?}"
        );
        assert!(
            nodes
                .iter()
                .all(|n| n.kind != NodeKind::Folder || n.name == ARTIFACTS_ROOT),
            "no folder beneath the root should survive a fully-refused publish: {nodes:?}"
        );
    }

    /// Issue #1839, the other side of the same refused publish: when a rival
    /// **adopts** the folder this publish minted in the write window, the
    /// rollback must leave it standing rather than sweep the folder the rival is
    /// about to write into. `AdoptParentThenRefuse` takes the lease on the task
    /// folder the instant before the note create is refused — the exact race —
    /// so the agent and task folders survive despite the failed publish.
    #[tokio::test]
    async fn a_folder_a_rival_adopted_survives_the_refused_publish() {
        let (_dir, ops, co) = stores();
        let ws: &dyn WorkspaceStore = ops.as_ref();
        let racing = AdoptParentThenRefuse(ops.clone());

        let err = materialize(&racing, &co, target("launch.md", "# Launch"))
            .await
            .expect_err("the refused note create must still fail the publish");
        assert!(
            err.to_string().contains("over quota"),
            "the store's refusal must reach the caller: {err}"
        );

        let nodes = ws.tree(&co).await.unwrap();
        assert!(
            nodes.iter().any(|n| n.name == "cmo"),
            "the agent folder a rival adopted must survive the minter's rollback: {nodes:?}"
        );
        assert!(
            nodes
                .iter()
                .any(|n| n.name == "t-1" || task_folder_task_id(&n.name) == Some("t-1")),
            "and the adopted task folder it parents must survive too: {nodes:?}"
        );
    }

    /// Two workflow nodes whose raw ids differ only by underscore vs dash
    /// still capture to distinct folders.
    ///
    /// `write_up` and `write-up` both kebab-normalize to `write-up`. Workflow
    /// validation only requires the RAW ids to be unique, so both are legal
    /// node ids in one graph. Without a raw-id-derived suffix on the path
    /// segment, the second node's capture would resolve to the same
    /// destination as the first and silently overwrite its output.
    #[tokio::test]
    async fn run_nodes_with_colliding_kebab_ids_capture_to_distinct_folders() {
        let (_dir, ops, co) = stores();
        let ws: &dyn WorkspaceStore = ops.as_ref();

        let first = materialize_run(
            ws,
            &co,
            RunTarget {
                agent_id: "cmo",
                run_id: "run-1",
                node_id: "write_up",
                source: "notes.md",
                payload: MirrorPayload::Text("first node's body"),
            },
        )
        .await
        .expect("first node capture");

        let second = materialize_run(
            ws,
            &co,
            RunTarget {
                agent_id: "cmo",
                run_id: "run-1",
                node_id: "write-up",
                source: "notes.md",
                payload: MirrorPayload::Text("second node's body"),
            },
        )
        .await
        .expect("second node capture");

        assert_ne!(
            first.node_id, second.node_id,
            "colliding kebab node ids must not resolve to the same workspace node"
        );

        let (_, first_body) = ws
            .read(&co, &first.node_id)
            .await
            .expect("read")
            .expect("first node still present");
        assert_eq!(
            first_body, "first node's body",
            "the second node's capture must not overwrite the first node's output"
        );
    }
}
