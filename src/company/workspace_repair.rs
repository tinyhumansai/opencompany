//! Issue #759: fold the duplicate sibling folders a publish race already left
//! behind, and name what is left over.
//!
//! Two concurrent publishes of one deliverable both read "this folder does not
//! exist yet" and both create it. The window is microseconds; the damage does
//! not decay. From then on
//! [`resolve_folder`](crate::company::artifact_mirror)'s `many` arm answers
//! every publish beneath that path with `Conflict — the path is ambiguous`, for
//! **every** agent, until somebody edits the tree by hand. Stopping new races is
//! one half of #759 and lands separately; this is the other half — what happens
//! to a tree a race has *already* broken.
//!
//! # Conservative by construction
//!
//! The failure mode of a repair pass is losing the work it was asked to rescue,
//! so the design gives itself as little room as possible:
//!
//! * **Nothing is renamed.** A merge that disambiguated by renaming would leave
//!   the operator hunting for `report (2).md`, and would break every
//!   `[[wikilink]]` pointing at the old name.
//! * **Nothing is overwritten.** Two files at one path is the one shape this
//!   pass refuses to resolve — see the residuals below.
//! * **Nothing non-empty is deleted.** A duplicate folder goes only once its
//!   children are provably somewhere else, and emptiness is re-checked against
//!   a *fresh* read of the tree rather than the snapshot the plan came from
//!   (the issue #671 / #700 discipline).
//! * **Node ids survive.** Children are relocated with
//!   [`rename_move`](WorkspaceStore::rename_move), which keeps a file's id — so
//!   the artifact chain that records a published node id still resolves after
//!   the repair.
//!
//! The result is that a publish landing in the middle of a merge degrades the
//! run to "not merged this time", never to lost work. Running it again picks up
//! where it left off.
//!
//! # The irreducible remainder
//!
//! Folders merge because a folder is a container: two containers at one path
//! hold a union, and the union is unambiguous. **Files do not.** Two files at
//! one path are two different documents, and any rule for picking one — newest,
//! largest, longest — silently discards somebody's work. So a collision that
//! involves a file at either end is not merged: the node stays exactly where it
//! is and is reported as a [`Residual`]. Detect-and-report is the honest
//! boundary, and the console renders that list so the operator knows what is
//! still theirs to settle.
//!
//! # Operator-triggered, never automatic
//!
//! Nothing here runs at boot. Issues #570, #645 and #700 all made the same call
//! — a tenant must not find its tree rearranged by an upgrade it did not ask
//! for — and this pass, which *moves* nodes rather than removing provably empty
//! ones, has even less business running unasked. The console's action is the
//! opt-in, and `dry_run` is what lets the confirm dialog name every folder and
//! every relocated child first.
//!
//! # Idempotence, and where it is only convergence
//!
//! [`duplicate_folder_plan`] is a pure function of one tree snapshot, so a tree
//! with no duplicate folders yields an empty plan and a second run right after
//! a first changes nothing. Two pathological shapes converge over runs rather
//! than settling in one: a folder that is a duplicate *and* holds duplicates of
//! its own is folded one layer per run, and a name duplicated three or more
//! times inside a folder that is itself a duplicate resolves in the order the
//! passes happen to reach it. Both are loss-free at every step; they just are
//! not finished in one click.

use std::collections::{HashMap, HashSet, VecDeque};

use serde::Serialize;

use crate::Result;
use crate::ports::types::CompanyId;
use crate::ports::workspace::{NodeKind, WorkspaceNode, WorkspaceStore};

#[cfg(test)]
pub(crate) mod loose_store;

/// One node the repair relocated into the surviving folder, or would.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct MovedChild {
    /// The node id, unchanged by the move — an artifact recorded against it
    /// still resolves.
    pub id: String,
    /// The node's name, which the move does not touch either.
    pub name: String,
}

/// One duplicate folder folded into its twin.
///
/// Named rather than counted, for the reason issue #700's preview is: "merged 3
/// folders" is a claim an operator cannot check, and this is a dialog they are
/// about to agree to.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MergedFolder {
    /// The folder that gives way.
    pub id: String,
    /// The duplicated name, shared with the folder it folds into.
    pub name: String,
    /// The surviving twin its children move to.
    pub into_id: String,
    /// The children that moved (or, on a preview, would).
    pub moved: Vec<MovedChild>,
    /// Whether the emptied folder itself went. `false` means something is still
    /// inside it — a residual, or a nested duplicate this run did not finish —
    /// so it was left standing.
    pub removed: bool,
}

/// Why the repair left a node exactly where it found it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum ResidualCause {
    /// A **file** shares the duplicated name with its siblings, so the whole
    /// group was left alone. Merging the folders in such a group would not even
    /// disambiguate the path — the file would still be sitting on it.
    FileSharesTheName,
    /// The child's name is already taken inside the surviving folder, and one
    /// of the two nodes is a file. Two files at one path are two documents; the
    /// repair does not choose between them.
    FileInTheWay,
    /// The move was refused because the tree changed underneath the repair.
    /// Nothing was lost and nothing is inconsistent — running it again picks
    /// this up.
    TreeMovedOn,
    /// The node's `parent_id` resolves to no node in the tree — a true orphan
    /// (issue #1839). A concurrent child create can commit *after* a
    /// [`delete_if_empty`](WorkspaceStore::delete_if_empty) read-deletes its
    /// parent on a backend with no per-company lock, leaving a child whose
    /// rendered path is unaddressable and which no sweep otherwise reaches.
    ///
    /// The repair is the guaranteed net: an orphan **folder** that is provably
    /// empty is removed (nothing non-empty is ever deleted — the module's
    /// standing invariant), while an orphan **file**, or a non-empty orphan
    /// folder, is surfaced here so the operator can re-home it rather than being
    /// silently swept. A residual with this cause that survives a real run is
    /// therefore always something a human must settle.
    DanglingParent,
}

/// One node the repair deliberately did not touch, and why.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Residual {
    /// The node id.
    pub id: String,
    /// The node's name.
    pub name: String,
    /// The folder it is still sitting in, or `None` at the workspace root.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_id: Option<String>,
    /// Why it stayed.
    pub cause: ResidualCause,
}

impl Residual {
    fn new(node: &WorkspaceNode, cause: ResidualCause) -> Self {
        Self {
            id: node.id.clone(),
            name: node.name.clone(),
            parent_id: node.parent_id.clone(),
            cause,
        }
    }
}

/// What the repair did, or would do.
///
/// `residuals` is present either way and is the point of the whole answer: an
/// operator who is told "3 folders merged" and nothing else has no idea whether
/// the tree is now correct. The list says what is left for a human.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct RepairPlan {
    /// The folders folded away, in the order the repair works through them.
    pub folders: Vec<MergedFolder>,
    /// What the repair refused to decide.
    pub residuals: Vec<Residual>,
}

impl RepairPlan {
    /// Whether there is nothing to do and nothing to report.
    pub fn is_empty(&self) -> bool {
        self.folders.is_empty() && self.residuals.is_empty()
    }
}

/// Which duplicate folders would merge into which, decided from one tree
/// snapshot.
///
/// Pure: it reads a `Vec<WorkspaceNode>` and touches no store. That is what
/// lets the shapes this exists for be pinned on hand-built input — the `fs`
/// backend rejects a duplicate sibling name at creation (`reject_path_collision`,
/// issue #665), so the very state being repaired is unreachable through it. The
/// backends hosted tenants actually run — sqlite and mongodb — do not reject it,
/// which is why the state exists in the first place.
///
/// The rules, in the order they apply:
///
/// 1. **A duplicate set is two or more sibling _folders_ sharing a name.**
///    Sibling by `parent_id`, never by rendered path: two nodes may share a
///    path, they never share an id.
/// 2. **A group holding a _file_ is left entirely alone** and every member is
///    reported ([`ResidualCause::FileSharesTheName`]).
/// 3. **The oldest folder wins**, by `updated_at_millis`, with the node id
///    breaking a tie so the answer is stable rather than dependent on the order
///    `tree()` happened to return.
/// 4. **Losers' children move into the winner.** A child whose name is free in
///    the winner moves; a folder-folder collision becomes another merge in the
///    same run (this is the fixpoint); a collision involving a file is a
///    residual and the child does not move.
/// 5. **A loser is folded away only once nothing is left in it** — and the real
///    run re-checks that against a fresh read before deleting anything.
pub(crate) fn duplicate_folder_plan(nodes: &[WorkspaceNode]) -> RepairPlan {
    let by_id: HashMap<&str, &WorkspaceNode> =
        nodes.iter().map(|node| (node.id.as_str(), node)).collect();

    // The simulated tree the plan is built against: every relocation is applied
    // here first, so a later step sees the tree as it will be rather than as it
    // was. Both directions are kept because the cycle guard walks upwards and
    // the merge walks down.
    let mut children: HashMap<Option<&str>, Vec<&str>> = HashMap::new();
    let mut parent_of: HashMap<&str, Option<&str>> = HashMap::new();
    for node in nodes {
        children
            .entry(node.parent_id.as_deref())
            .or_default()
            .push(node.id.as_str());
        parent_of.insert(node.id.as_str(), node.parent_id.as_deref());
    }

    let mut residuals: Vec<Residual> = Vec::new();
    let mut queue: VecDeque<(&str, &str)> = VecDeque::new();

    // Seeded in a sorted walk rather than in `HashMap` order: the plan is shown
    // to an operator and re-derived on the confirm, so two reads of one tree
    // must not disagree about which twin survives or in what order.
    let mut parents: Vec<Option<&str>> = children.keys().copied().collect();
    parents.sort_unstable();
    for parent in parents {
        let siblings = children.get(&parent).cloned().unwrap_or_default();
        let mut groups: HashMap<&str, Vec<&str>> = HashMap::new();
        for id in siblings {
            groups.entry(by_id[id].name.as_str()).or_default().push(id);
        }
        let mut names: Vec<&str> = groups.keys().copied().collect();
        names.sort_unstable();
        for name in names {
            let group = &groups[name];
            if group.len() < 2 {
                continue;
            }
            if group.iter().any(|id| by_id[id].kind == NodeKind::File) {
                residuals.extend(
                    group
                        .iter()
                        .map(|id| Residual::new(by_id[id], ResidualCause::FileSharesTheName)),
                );
                continue;
            }
            let winner = oldest(&by_id, group);
            for loser in group.iter().copied().filter(|id| *id != winner) {
                queue.push_back((winner, loser));
            }
        }
    }

    // Each loser is folded exactly once. A tree can offer the same folder as a
    // loser twice (a name duplicated three ways inside a folder that is itself a
    // duplicate); taking the first and leaving the rest to the next run is the
    // loss-free way to answer that, since every fold only ever moves children
    // into a folder that survives.
    let mut visited: HashSet<&str> = HashSet::new();
    let mut folds: Vec<(&str, &str, Vec<&str>)> = Vec::new();

    while let Some((winner, loser)) = queue.pop_front() {
        if winner == loser || !visited.insert(loser) {
            continue;
        }
        // A corrupt tree can make the winner a descendant of the loser, and
        // moving a node into its own subtree is a cycle the store rejects.
        // Refusing to plan it keeps the preview honest rather than showing the
        // operator a move that will fail.
        if is_descendant(&parent_of, winner, loser) {
            continue;
        }

        let kids = children.get(&Some(loser)).cloned().unwrap_or_default();
        let mut moved: Vec<&str> = Vec::new();
        for kid in kids {
            let name = by_id[kid].name.as_str();
            let rivals: Vec<&str> = children
                .get(&Some(winner))
                .map(|ids| {
                    ids.iter()
                        .copied()
                        .filter(|id| by_id[id].name == name)
                        .collect()
                })
                .unwrap_or_default();

            if rivals.is_empty() {
                if let Some(siblings) = children.get_mut(&Some(loser)) {
                    siblings.retain(|id| *id != kid);
                }
                children.entry(Some(winner)).or_default().push(kid);
                parent_of.insert(kid, Some(winner));
                moved.push(kid);
            } else if by_id[kid].kind == NodeKind::Folder
                && rivals.iter().all(|id| by_id[id].kind == NodeKind::Folder)
            {
                // Folder onto folder: the union is unambiguous, so this becomes
                // another merge. Queued rather than recursed — a corrupt tree
                // must not be able to blow the stack — and aimed at the rival
                // that would survive its own group, so the children land in the
                // folder that is going to be there afterwards.
                queue.push_back((oldest(&by_id, &rivals), kid));
            } else {
                residuals.push(Residual::new(by_id[kid], ResidualCause::FileInTheWay));
            }
        }
        folds.push((loser, winner, moved));
    }

    // What is still inside each loser once every relocation is accounted for:
    // its residuals, and the nested losers that have not been folded away yet.
    // A loser is removable when all of those are themselves removable, which is
    // a fixpoint rather than a single pass because emptiness cascades upwards.
    let remaining: HashMap<&str, Vec<&str>> = folds
        .iter()
        .map(|(loser, _, _)| {
            (
                *loser,
                children.get(&Some(*loser)).cloned().unwrap_or_default(),
            )
        })
        .collect();
    let mut removable: HashSet<&str> = HashSet::new();
    loop {
        let mut changed = false;
        for (loser, kids) in &remaining {
            if !removable.contains(loser) && kids.iter().all(|kid| removable.contains(kid)) {
                removable.insert(loser);
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }

    // Orphans (issue #1839): a node whose `parent_id` names nothing in the tree.
    // Independent of the duplicate-folder fold above — appended after it so a
    // node already reported as a residual is not named twice — this is what
    // turns a Race-2 orphan from an invisible, unaddressable node into something
    // the operator is told about and the apply half can act on. Sorted so the
    // preview and the confirm agree on the order.
    let already_residual: HashSet<&str> = residuals.iter().map(|r| r.id.as_str()).collect();
    let mut orphans: Vec<&WorkspaceNode> = nodes
        .iter()
        .filter(|node| {
            node.parent_id
                .as_deref()
                .is_some_and(|parent| !by_id.contains_key(parent))
                && !already_residual.contains(node.id.as_str())
        })
        .collect();
    orphans.sort_unstable_by(|a, b| a.id.cmp(&b.id));
    for node in orphans {
        residuals.push(Residual::new(node, ResidualCause::DanglingParent));
    }

    RepairPlan {
        folders: folds
            .into_iter()
            .map(|(loser, winner, moved)| MergedFolder {
                id: loser.to_string(),
                name: by_id[loser].name.clone(),
                into_id: winner.to_string(),
                moved: moved
                    .into_iter()
                    .map(|id| MovedChild {
                        id: id.to_string(),
                        name: by_id[id].name.clone(),
                    })
                    .collect(),
                removed: removable.contains(loser),
            })
            .collect(),
        residuals,
    }
}

/// The folder a duplicate set keeps: the oldest by `updated_at_millis`, with the
/// node id breaking a tie.
///
/// Oldest rather than newest because the older twin is the one other things have
/// had time to point at — a `[[wikilink]]`, an operator's open tab, an artifact
/// version recorded against a node inside it.
fn oldest<'a>(by_id: &HashMap<&str, &WorkspaceNode>, group: &[&'a str]) -> &'a str {
    group
        .iter()
        .copied()
        .min_by_key(|id| (by_id[id].updated_at_millis, by_id[id].id.as_str()))
        .expect("a duplicate group is never empty")
}

/// Whether `node` sits anywhere beneath `ancestor` in the simulated tree.
///
/// Walks with a step bound rather than trusting the chain to terminate: a tree
/// holding a parent cycle is exactly the kind of damage this module is called in
/// to look at, and a repair pass that hangs on one is worse than the duplicates.
fn is_descendant(parent_of: &HashMap<&str, Option<&str>>, node: &str, ancestor: &str) -> bool {
    let mut at = node;
    for _ in 0..parent_of.len() {
        match parent_of.get(at).copied().flatten() {
            Some(parent) if parent == ancestor => return true,
            Some(parent) => at = parent,
            None => return false,
        }
    }
    false
}

/// Merge every duplicate sibling folder in `company`, naming what moved and what
/// was left behind.
///
/// `dry_run` answers the plan without touching anything, so the console can show
/// the operator every folder that gives way, every child that relocates and
/// every node the repair refuses to decide, before they agree to it.
///
/// # Two phases, and a fresh read between them
///
/// A real run relocates children first, then re-reads the tree and deletes only
/// the losers that are **structurally empty in that second read** — counted over
/// every node the store returned, per the issue #671 measure that a child with
/// no renderable path is still a child, and the port's `delete` is recursive.
/// The plan's own emptiness claim is deliberately not trusted: issue #552's
/// publish path can mint a deliverable into any folder at any moment, and a
/// claim computed before a preview-then-confirm round trip describes a tree that
/// has moved on.
///
/// The count is decremented as this run's own deletions land, which is what lets
/// a nested duplicate and the folder holding it both go in one pass — the losers
/// are visited deepest-first for exactly that reason. Nothing else adjusts it,
/// so a child that arrived concurrently keeps its folder standing.
///
/// # Errors
///
/// A relocation that the store refuses is **not** an error: the child stays
/// where it was, which is the state the repair started from, and it is reported
/// as a [`ResidualCause::TreeMovedOn`] residual so the receipt says so rather
/// than reading as "nothing needed doing". Only a failure to read the tree or to
/// delete an emptied folder propagates.
pub async fn merge_duplicate_folders(
    store: &dyn WorkspaceStore,
    company: &CompanyId,
    dry_run: bool,
) -> Result<RepairPlan> {
    let nodes = store.tree(company).await?;
    let plan = duplicate_folder_plan(&nodes);
    if dry_run {
        return Ok(plan);
    }

    let RepairPlan {
        folders,
        mut residuals,
    } = plan;

    // -- phase one: relocate ------------------------------------------------
    let mut done: Vec<MergedFolder> = Vec::with_capacity(folders.len());
    for folder in folders {
        let MergedFolder {
            id,
            name,
            into_id,
            moved: planned,
            removed: _,
        } = folder;
        let mut moved = Vec::new();
        for child in planned {
            match store
                .rename_move(company, &child.id, None, Some(Some(&into_id)))
                .await
            {
                Ok(_) => moved.push(child),
                Err(err) => {
                    tracing::info!(
                        company = %company,
                        node = %child.id,
                        into = %into_id,
                        error = %err,
                        "[workspace] `{}` could not be moved out of the duplicate `{name}`; \
                         leaving it where it is",
                        child.name
                    );
                    residuals.push(Residual {
                        id: child.id,
                        name: child.name,
                        parent_id: Some(id.clone()),
                        cause: ResidualCause::TreeMovedOn,
                    });
                }
            }
        }
        done.push(MergedFolder {
            id,
            name,
            into_id,
            moved,
            removed: false,
        });
    }

    // -- phase two: remove what is now provably empty -----------------------
    let fresh = store.tree(company).await?;
    let mut child_count: HashMap<&str, usize> = HashMap::new();
    for node in &fresh {
        if let Some(parent) = node.parent_id.as_deref() {
            *child_count.entry(parent).or_default() += 1;
        }
    }
    let parent_of: HashMap<&str, Option<&str>> = fresh
        .iter()
        .map(|node| (node.id.as_str(), node.parent_id.as_deref()))
        .collect();

    // Reverse order is deepest-first: a nested duplicate is always queued after
    // the fold that exposed it, so it is always later in this list than the
    // folder holding it.
    for folder in done.iter_mut().rev() {
        let Some(parent) = parent_of.get(folder.id.as_str()).copied() else {
            tracing::info!(
                company = %company,
                node = %folder.id,
                "[workspace] the duplicate `{}` was already gone",
                folder.name
            );
            continue;
        };
        let left = child_count
            .get(folder.id.as_str())
            .copied()
            .unwrap_or_default();
        if left != 0 {
            tracing::info!(
                company = %company,
                node = %folder.id,
                remaining = left,
                "[workspace] the duplicate `{}` still holds {left} node(s); leaving it in place",
                folder.name
            );
            continue;
        }
        if store.delete(company, &folder.id).await? {
            folder.removed = true;
            if let Some(parent) = parent
                && let Some(count) = child_count.get_mut(parent)
            {
                *count = count.saturating_sub(1);
            }
        }
    }

    // -- phase three: reap provably-empty orphan folders (issue #1839) -------
    //
    // A Race-2 orphan is a node whose parent was read-deleted out from under a
    // concurrent child insert on a lockless backend. The plan named every orphan
    // as a `DanglingParent` residual; here the guaranteed net acts on them. An
    // orphan *folder* that is still childless goes — through `delete_if_empty`,
    // which re-checks emptiness against the store's current state, so a child
    // that arrived in the meantime keeps its folder exactly as the duplicate
    // path's second read does. An orphan *file*, or a folder something now sits
    // under, is left standing and stays a residual for the operator to re-home:
    // nothing non-empty is ever deleted, and no file is destroyed on a path the
    // repair could not choose. Removed folders drop out of the residual list;
    // what remains is what a human still has to settle.
    let orphan_ids: Vec<String> = residuals
        .iter()
        .filter(|r| r.cause == ResidualCause::DanglingParent)
        .map(|r| r.id.clone())
        .collect();
    if !orphan_ids.is_empty() {
        let current = store.tree(company).await?;
        let kind_of: HashMap<&str, NodeKind> = current
            .iter()
            .map(|node| (node.id.as_str(), node.kind))
            .collect();
        let mut removed: HashSet<String> = HashSet::new();
        for id in &orphan_ids {
            if kind_of.get(id.as_str()) == Some(&NodeKind::Folder)
                && store.delete_if_empty(company, id).await?
            {
                removed.insert(id.clone());
            }
        }
        residuals.retain(|r| r.cause != ResidualCause::DanglingParent || !removed.contains(&r.id));
    }

    tracing::info!(
        company = %company,
        merged = done.iter().filter(|folder| folder.removed).count(),
        moved = done.iter().map(|folder| folder.moved.len()).sum::<usize>(),
        residuals = residuals.len(),
        "[workspace] merged duplicate folders"
    );

    Ok(RepairPlan {
        folders: done,
        residuals,
    })
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::loose_store::LooseWorkspace;
    use super::*;
    use crate::ports::workspace::WorkspaceOrigin;

    fn node(id: &str, name: &str, parent: Option<&str>, updated: u64) -> WorkspaceNode {
        WorkspaceNode {
            id: id.to_string(),
            name: name.to_string(),
            kind: NodeKind::Folder,
            parent_id: parent.map(str::to_string),
            updated_at_millis: updated,
            created_by: WorkspaceOrigin::Seed,
            updated_by: WorkspaceOrigin::Seed,
            mime: None,
            size: None,
            sha256: None,
            adopted: false,
        }
    }

    fn folder(id: &str, name: &str, parent: Option<&str>) -> WorkspaceNode {
        node(id, name, parent, 1)
    }

    fn file(id: &str, name: &str, parent: Option<&str>) -> WorkspaceNode {
        WorkspaceNode {
            kind: NodeKind::File,
            ..folder(id, name, parent)
        }
    }

    fn moved_ids(folder: &MergedFolder) -> Vec<&str> {
        folder.moved.iter().map(|m| m.id.as_str()).collect()
    }

    /// The tree a `LooseWorkspace` holds, as `id → parent`, so an assertion says
    /// where everything ended up rather than counting.
    async fn placement(ws: &Arc<dyn WorkspaceStore>, company: &CompanyId) -> Vec<(String, String)> {
        let mut out: Vec<(String, String)> = ws
            .tree(company)
            .await
            .unwrap()
            .into_iter()
            .map(|node| (node.id, node.parent_id.unwrap_or_else(|| "-".to_string())))
            .collect();
        out.sort();
        out
    }

    // -- the plan, on shapes the `fs` backend refuses to create ---------------

    /// The whole point, in one tree: the race left two `reports/` folders, each
    /// holding a different deliverable. The older one keeps its id, the newer
    /// one's file moves across, and the emptied folder goes.
    #[test]
    fn a_duplicate_pair_folds_into_the_older_folder() {
        let nodes = vec![
            folder("root", "Agents", None),
            node("keep", "reports", Some("root"), 100),
            node("dupe", "reports", Some("root"), 200),
            file("a", "q1.md", Some("keep")),
            file("b", "q2.md", Some("dupe")),
        ];

        let plan = duplicate_folder_plan(&nodes);

        assert_eq!(plan.folders.len(), 1, "{plan:?}");
        let fold = &plan.folders[0];
        assert_eq!((fold.id.as_str(), fold.into_id.as_str()), ("dupe", "keep"));
        assert_eq!(moved_ids(fold), vec!["b"], "the newer twin's file moves");
        assert!(fold.removed, "the emptied duplicate goes");
        assert!(plan.residuals.is_empty(), "{:?}", plan.residuals);
    }

    /// The tiebreak, stated on its own: two folders written in the same
    /// millisecond — which sqlite's millisecond clock makes ordinary for a race
    /// — must still produce one stable answer, or a preview and the confirm that
    /// follows it could disagree about which folder survives.
    #[test]
    fn a_tie_on_the_timestamp_is_broken_by_the_node_id() {
        let nodes = vec![
            node("zzz", "reports", None, 50),
            node("aaa", "reports", None, 50),
        ];

        let plan = duplicate_folder_plan(&nodes);

        assert_eq!(plan.folders.len(), 1);
        assert_eq!(plan.folders[0].into_id, "aaa");
        assert_eq!(plan.folders[0].id, "zzz");
    }

    /// The fixpoint: the duplicate holds a folder whose name is taken inside the
    /// survivor, so that pair merges too — in the same run, and deep enough that
    /// the emptiness has to cascade two levels for both folders to go.
    #[test]
    fn nested_folder_collisions_iterate_to_a_fixpoint() {
        let nodes = vec![
            node("keep", "reports", None, 100),
            node("dupe", "reports", None, 200),
            node("keep-q1", "q1", Some("keep"), 100),
            node("dupe-q1", "q1", Some("dupe"), 200),
            file("stray", "notes.md", Some("dupe-q1")),
        ];

        let plan = duplicate_folder_plan(&nodes);

        let folds: Vec<(&str, &str, bool)> = plan
            .folders
            .iter()
            .map(|f| (f.id.as_str(), f.into_id.as_str(), f.removed))
            .collect();
        assert_eq!(
            folds,
            vec![("dupe", "keep", true), ("dupe-q1", "keep-q1", true)],
            "both layers fold, and both empty out: {plan:?}"
        );
        assert_eq!(
            moved_ids(&plan.folders[1]),
            vec!["stray"],
            "the file two levels down moves into the surviving `q1`"
        );
        assert!(plan.residuals.is_empty(), "{:?}", plan.residuals);
    }

    /// The irreducible remainder. Both duplicates hold a `summary.md`; those are
    /// two documents, not one, so neither moves and neither is overwritten — the
    /// file is named as a residual and the duplicate folder stays standing
    /// around it.
    #[test]
    fn a_file_collision_is_reported_and_moves_nothing() {
        let nodes = vec![
            node("keep", "reports", None, 100),
            node("dupe", "reports", None, 200),
            file("mine", "summary.md", Some("keep")),
            file("theirs", "summary.md", Some("dupe")),
            file("safe", "appendix.md", Some("dupe")),
        ];

        let plan = duplicate_folder_plan(&nodes);

        let fold = &plan.folders[0];
        assert_eq!(
            moved_ids(fold),
            vec!["safe"],
            "the child whose name is free still moves"
        );
        assert!(
            !fold.removed,
            "a duplicate still holding a document must not be deleted"
        );
        assert_eq!(
            plan.residuals,
            vec![Residual {
                id: "theirs".to_string(),
                name: "summary.md".to_string(),
                parent_id: Some("dupe".to_string()),
                cause: ResidualCause::FileInTheWay,
            }],
            "the operator is told exactly which document is still theirs to settle"
        );
    }

    /// A file and a folder sharing a name is a broken path too, and one this
    /// pass cannot fix: merging the folders would leave the file sitting on the
    /// path regardless. So the group is left entirely alone and reported.
    #[test]
    fn a_file_sharing_the_name_leaves_the_whole_group_untouched() {
        let nodes = vec![
            node("keep", "reports", None, 100),
            node("dupe", "reports", None, 200),
            file("note", "reports", None),
            file("inside", "q1.md", Some("dupe")),
        ];

        let plan = duplicate_folder_plan(&nodes);

        assert!(
            plan.folders.is_empty(),
            "nothing may move while a file shares the name: {plan:?}"
        );
        let reported: Vec<(&str, ResidualCause)> = plan
            .residuals
            .iter()
            .map(|r| (r.id.as_str(), r.cause))
            .collect();
        assert_eq!(
            reported,
            vec![
                ("keep", ResidualCause::FileSharesTheName),
                ("dupe", ResidualCause::FileSharesTheName),
                ("note", ResidualCause::FileSharesTheName),
            ]
        );
    }

    /// A healthy tree is not a duplicate set: same name under different parents,
    /// and a folder beside a file it does not share a name with, are both
    /// ordinary.
    #[test]
    fn a_tree_without_duplicates_yields_an_empty_plan() {
        let nodes = vec![
            folder("agents", "Agents", None),
            folder("desks", "Desks", None),
            folder("ceo", "reports", Some("agents")),
            folder("cmo", "reports", Some("desks")),
            file("note", "reports.md", Some("agents")),
        ];

        assert_eq!(duplicate_folder_plan(&nodes), RepairPlan::default());
    }

    /// Issue #1839: a node whose `parent_id` resolves to nothing is a true
    /// orphan — the Race-2 shape a lockless backend leaves when a child insert
    /// commits after its parent was read-deleted. The plan names every orphan,
    /// both kinds, so the preview can show them and the apply half can act.
    #[test]
    fn a_dangling_parent_is_reported_as_a_residual() {
        let nodes = vec![
            file("orphan-file", "lost.md", Some("ghost")),
            folder("orphan-dir", "lost", Some("ghost")),
        ];

        let plan = duplicate_folder_plan(&nodes);

        assert!(
            plan.folders.is_empty(),
            "there is nothing to merge: {plan:?}"
        );
        let reported: Vec<(&str, ResidualCause)> = plan
            .residuals
            .iter()
            .map(|r| (r.id.as_str(), r.cause))
            .collect();
        assert_eq!(
            reported,
            vec![
                ("orphan-dir", ResidualCause::DanglingParent),
                ("orphan-file", ResidualCause::DanglingParent),
            ],
            "both orphans are named, sorted by id"
        );
    }

    /// A node whose parent *does* exist is not an orphan, so a healthy tree — and
    /// the duplicate shapes above — never gain a spurious dangling residual.
    #[test]
    fn a_present_parent_is_never_dangling() {
        let nodes = vec![
            folder("root", "agents", None),
            folder("child", "cmo", Some("root")),
        ];

        assert_eq!(duplicate_folder_plan(&nodes), RepairPlan::default());
    }

    /// A parent cycle is the kind of damage this module gets called in to look
    /// at, so the ancestor walk must terminate rather than hang the request.
    #[test]
    fn a_parent_cycle_does_not_hang_the_plan() {
        let nodes = vec![
            node("a", "reports", Some("b"), 100),
            node("b", "reports", Some("a"), 200),
        ];

        let plan = duplicate_folder_plan(&nodes);

        assert!(
            plan.folders.is_empty(),
            "neither twin can be moved into the other: {plan:?}"
        );
    }

    // -- the real run, over a store that permits the broken state ------------

    fn store() -> Arc<dyn WorkspaceStore> {
        Arc::new(LooseWorkspace::default())
    }

    async fn seed(ws: &Arc<dyn WorkspaceStore>, company: &CompanyId, nodes: &[WorkspaceNode]) {
        for node in nodes {
            ws.create(company, node, Some("")).await.unwrap();
        }
    }

    /// The apply path end to end: the child relocates, keeps its id, and the
    /// emptied duplicate is deleted.
    #[tokio::test]
    async fn it_merges_over_a_store_and_keeps_every_node_id() {
        let ws = store();
        let company = CompanyId::new("acme");
        seed(
            &ws,
            &company,
            &[
                node("keep", "reports", None, 100),
                node("dupe", "reports", None, 200),
                file("a", "q1.md", Some("keep")),
                file("b", "q2.md", Some("dupe")),
            ],
        )
        .await;

        let done = merge_duplicate_folders(ws.as_ref(), &company, false)
            .await
            .unwrap();

        assert!(done.folders[0].removed);
        assert_eq!(
            placement(&ws, &company).await,
            vec![
                ("a".to_string(), "keep".to_string()),
                ("b".to_string(), "keep".to_string()),
                ("keep".to_string(), "-".to_string()),
            ],
            "both files sit under the survivor, with the ids they were published as"
        );
    }

    /// A dry run is the confirm dialog's evidence: it names the fold and leaves
    /// the tree exactly as it found it.
    #[tokio::test]
    async fn a_dry_run_names_the_fold_and_changes_nothing() {
        let ws = store();
        let company = CompanyId::new("acme");
        seed(
            &ws,
            &company,
            &[
                node("keep", "reports", None, 100),
                node("dupe", "reports", None, 200),
                file("b", "q2.md", Some("dupe")),
            ],
        )
        .await;
        let before = placement(&ws, &company).await;

        let preview = merge_duplicate_folders(ws.as_ref(), &company, true)
            .await
            .unwrap();

        assert_eq!(moved_ids(&preview.folders[0]), vec!["b"]);
        assert!(preview.folders[0].removed, "it would go");
        assert_eq!(placement(&ws, &company).await, before);
    }

    /// Acceptance criterion: a second run changes nothing. The pass is a
    /// function of the current tree and holds no state, so an operator's double
    /// click is harmless.
    #[tokio::test]
    async fn running_it_twice_changes_nothing_the_second_time() {
        let ws = store();
        let company = CompanyId::new("acme");
        seed(
            &ws,
            &company,
            &[
                node("keep", "reports", None, 100),
                node("dupe", "reports", None, 200),
                file("b", "q2.md", Some("dupe")),
            ],
        )
        .await;

        merge_duplicate_folders(ws.as_ref(), &company, false)
            .await
            .unwrap();
        let after_first = placement(&ws, &company).await;
        let second = merge_duplicate_folders(ws.as_ref(), &company, false)
            .await
            .unwrap();

        assert_eq!(
            second,
            RepairPlan::default(),
            "the second run found nothing"
        );
        assert_eq!(
            placement(&ws, &company).await,
            after_first,
            "and changed nothing either"
        );
    }

    /// The residual, over a real store: the rival document is still there, under
    /// the id it always had, and the duplicate folder holding it is still
    /// standing.
    #[tokio::test]
    async fn a_file_collision_survives_the_real_run_untouched() {
        let ws = store();
        let company = CompanyId::new("acme");
        seed(
            &ws,
            &company,
            &[
                node("keep", "reports", None, 100),
                node("dupe", "reports", None, 200),
                file("mine", "summary.md", Some("keep")),
                file("theirs", "summary.md", Some("dupe")),
            ],
        )
        .await;

        let done = merge_duplicate_folders(ws.as_ref(), &company, false)
            .await
            .unwrap();

        assert!(!done.folders[0].removed);
        assert_eq!(
            done.residuals
                .iter()
                .map(|r| (r.id.as_str(), r.cause))
                .collect::<Vec<_>>(),
            vec![("theirs", ResidualCause::FileInTheWay)]
        );
        assert_eq!(
            placement(&ws, &company).await,
            vec![
                ("dupe".to_string(), "-".to_string()),
                ("keep".to_string(), "-".to_string()),
                ("mine".to_string(), "keep".to_string()),
                ("theirs".to_string(), "dupe".to_string()),
            ],
            "both documents are exactly where they were"
        );
    }

    /// Issue #671's discipline, at this module's boundary: the emptiness that
    /// authorises a delete is re-read, not remembered. A child that lands in the
    /// duplicate between the plan and the delete keeps its folder standing —
    /// and the port's `delete` is recursive, so this is the difference between
    /// leaving a folder behind and taking a deliverable with it.
    #[tokio::test]
    async fn a_child_that_arrives_mid_merge_keeps_its_folder() {
        let loose = Arc::new(LooseWorkspace::default());
        let ws: Arc<dyn WorkspaceStore> = loose.clone();
        let company = CompanyId::new("acme");
        seed(
            &ws,
            &company,
            &[
                node("keep", "reports", None, 100),
                node("dupe", "reports", None, 200),
                file("b", "q2.md", Some("dupe")),
            ],
        )
        .await;

        // A publish lands inside the duplicate the instant its last child has
        // been relocated — the exact window the second read exists to see.
        loose.on_next_move(|nodes| nodes.push(file("late", "late.md", Some("dupe"))));

        let done = merge_duplicate_folders(ws.as_ref(), &company, false)
            .await
            .unwrap();

        assert!(
            !done.folders[0].removed,
            "the duplicate gained a deliverable and must survive: {done:?}"
        );
        assert!(
            ws.read(&company, "late").await.unwrap().is_some(),
            "and the deliverable itself must still be there"
        );
    }

    /// Issue #1839, the guaranteed net: a real run reaps a provably-empty orphan
    /// **folder** and surfaces an orphan **file** rather than destroying it.
    /// Both are injected past the store's parent check, because a lawful create
    /// refuses a missing parent — the orphan only arises from the Race-2 the
    /// reaper exists to clean up.
    #[tokio::test]
    async fn the_orphan_reaper_reaps_an_empty_folder_and_surfaces_a_file() {
        let loose = Arc::new(LooseWorkspace::default());
        let ws: Arc<dyn WorkspaceStore> = loose.clone();
        let company = CompanyId::new("acme");
        loose.inject(
            &company,
            vec![
                file("orphan-file", "lost.md", Some("ghost")),
                folder("orphan-dir", "lost", Some("ghost")),
            ],
        );

        let done = merge_duplicate_folders(ws.as_ref(), &company, false)
            .await
            .unwrap();

        // The empty orphan folder is gone from the residual list; the file stays.
        let surfaced: Vec<(&str, ResidualCause)> = done
            .residuals
            .iter()
            .map(|r| (r.id.as_str(), r.cause))
            .collect();
        assert_eq!(
            surfaced,
            vec![("orphan-file", ResidualCause::DanglingParent)],
            "the orphan file is surfaced; the empty orphan folder is not: {done:?}"
        );

        let remaining: Vec<String> = ws
            .tree(&company)
            .await
            .unwrap()
            .into_iter()
            .map(|node| node.id)
            .collect();
        assert!(
            remaining.contains(&"orphan-file".to_string()),
            "the orphan file must be surfaced, never destroyed: {remaining:?}"
        );
        assert!(
            !remaining.contains(&"orphan-dir".to_string()),
            "the empty orphan folder must be reaped: {remaining:?}"
        );
    }

    /// The reaper honours the module's standing invariant: a **non-empty** orphan
    /// folder is not deleted. It stays surfaced, and its child — whose own parent
    /// now resolves — is left where it is.
    #[tokio::test]
    async fn a_non_empty_orphan_folder_is_surfaced_not_reaped() {
        let loose = Arc::new(LooseWorkspace::default());
        let ws: Arc<dyn WorkspaceStore> = loose.clone();
        let company = CompanyId::new("acme");
        loose.inject(
            &company,
            vec![
                folder("orphan-dir", "lost", Some("ghost")),
                file("inside", "kept.md", Some("orphan-dir")),
            ],
        );

        let done = merge_duplicate_folders(ws.as_ref(), &company, false)
            .await
            .unwrap();

        // Only the top orphan is dangling — `inside`'s parent resolves — and it
        // holds a document, so `delete_if_empty` refuses it and it stays a
        // residual.
        assert_eq!(
            done.residuals
                .iter()
                .map(|r| (r.id.as_str(), r.cause))
                .collect::<Vec<_>>(),
            vec![("orphan-dir", ResidualCause::DanglingParent)],
        );
        assert!(
            ws.read(&company, "inside").await.unwrap().is_some(),
            "the child of a non-empty orphan folder is untouched"
        );
    }
}
