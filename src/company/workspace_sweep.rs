//! Issue #700: remove the empty `agents/<id>/` folders a pre-#570 company is
//! still carrying, and nothing else.
//!
//! Every company provisioned before issue #570 minted one folder per roster
//! member at boot, whether or not that member ever produced anything. #570
//! stopped doing it and deliberately chose no backfill — the right call at the
//! time, and the reason there is no migration to lean on here. What changed
//! since is what those folders now *mean*:
//!
//! * #570 made a member folder mean "this teammate produced something", so on a
//!   pre-#570 tenant the two populations are indistinguishable by eye.
//! * #552 publishes deliverables into `<agent>/<task-id>/` under a member
//!   folder — `artifacts/` today, `agents/` when this was written — so an empty
//!   folder here reads as "this agent has produced nothing" — a claim, and on
//!   these tenants an unfounded one.
//! * #607 made the tree searchable with a bounded result set, so a name-matching
//!   empty folder competes for slots against notes that actually hold something.
//!
//! # The whole risk is the emptiness predicate
//!
//! The failure mode is deleting somebody's work, and the port's
//! [`delete`](WorkspaceStore::delete) is **recursive** — so a folder handed to it
//! takes its subtree with it whether or not the caller knew the subtree was
//! there. Issue #671 found exactly this bug one layer over, in the agent delete
//! tool: the tool layer's `PathIndex` omits every node it cannot address by path
//! (a name carrying a separator, a dangling or cyclic ancestor chain), so a
//! folder holding only such children looks empty by every path-shaped measure
//! while `delete` would still take them.
//!
//! [`empty_agent_folder_candidates`] therefore counts **structurally**, over
//! every node `tree()` returned, before and without any path rendering — the
//! same `parent id → child count` map #671 replaced the prefix scan with. It is
//! also exact per node id rather than per rendered path: two folders may share a
//! path, they never share an id.
//!
//! The shape is not hypothetical on the tenants this targets. The `fs` backend
//! rejects separator-carrying names at creation (`reject_unsafe_name`); the
//! sqlite and mongodb backends do not, and hosted tenants run those.
//!
//! # Why there is no authorship filter
//!
//! Stamping would be the obvious narrower predicate — sweep only what the #551
//! boot burst created — and it cannot work here. A node written before issue
//! #326 has no origin field at all and deserializes to
//! [`Operator`](crate::ports::workspace::WorkspaceOrigin::Operator), which is
//! precisely the population this exists for. Structural emptiness is the only
//! predicate that is safe on the affected tenants, and naming every folder
//! before it goes (see `dry_run`) is what keeps that honest.
//!
//! # Operator-triggered, not automatic
//!
//! Nothing here runs at boot. A tenant finding its tree changed by an upgrade it
//! did not ask for is the outcome #570 and #645 both avoided by leaving existing
//! nodes alone; the console's action is the opt-in that replaces it. The
//! functions below are a stateless function of the current tree, so running the
//! sweep twice removes nothing the second time — a property the tests assert
//! rather than assume.

use std::collections::{HashMap, HashSet};

use serde::Serialize;

use crate::Result;
use crate::company::workspace_scaffold::{AGENTS_ROOT, Found, find};
use crate::error::OpenCompanyError;
use crate::ports::types::CompanyId;
use crate::ports::workspace::{NodeKind, WorkspaceNode, WorkspaceStore};

/// One folder the sweep removed, or would remove.
///
/// The name is the point. "Removed 17 empty folders" is a number an operator
/// cannot check; an operator who disagrees with the sweep needs to know *what*
/// went, which is why this carries the name on the preview as well as on the
/// result. The id comes along because a console that wants to drop the row it is
/// showing needs to match it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct SweptFolder {
    /// The node id.
    pub id: String,
    /// The folder's name, as the tree holds it.
    pub name: String,
}

impl From<&WorkspaceNode> for SweptFolder {
    fn from(node: &WorkspaceNode) -> Self {
        Self {
            id: node.id.clone(),
            name: node.name.clone(),
        }
    }
}

/// Which folders directly under `agents/` hold nothing at all.
///
/// Pure: it decides from one tree snapshot and touches no store, which is what
/// lets the shapes that matter — an unaddressable child, a dangling chain — be
/// pinned on hand-built input that no backend would let a test create.
///
/// Three conditions, all necessary:
///
/// 1. **A direct child of the `Agents` root, by node id.** Never by rendered
///    path: the root is resolved with the scaffold's own
///    [`find`](crate::company::workspace_scaffold::find), so the sweep and the
///    scaffold cannot disagree about which node that root is.
/// 2. **A folder.** A *file* sitting directly under `agents/` is somebody's
///    note, not a stray container.
/// 3. **No children, counted structurally.** See the module docs: the count runs
///    over every node the store returned, so a child with no renderable path
///    still protects its parent.
///
/// # Errors
///
/// An ambiguous root — several nodes named `Agents`, or a *file* carrying the
/// name — is refused rather than guessed at. "Under `agents/`" has no single
/// answer there, and picking one would delete beneath a root the scaffold itself
/// refuses to touch. No root at all is not an error: there is simply nothing to
/// sweep.
pub(crate) fn empty_agent_folder_candidates(
    nodes: &[WorkspaceNode],
) -> Result<Vec<&WorkspaceNode>> {
    let root_id = match find(nodes, None, AGENTS_ROOT) {
        Found::Folder(id) => id,
        Found::Free => return Ok(Vec::new()),
        Found::Collision(why) => {
            return Err(OpenCompanyError::Conflict(format!(
                "{why}, so which folders are `{AGENTS_ROOT}/` members is undecidable; nothing \
                 was removed"
            )));
        }
    };

    // Counted before anything is filtered, and over parent ids rather than
    // rendered paths — the issue #671 measure. A child whose name carries a
    // separator has no path, is absent from every path-shaped index, and would
    // still be taken by the port's recursive delete; here it is a child like
    // any other.
    let mut child_count: HashMap<&str, usize> = HashMap::new();
    for node in nodes {
        if let Some(parent) = node.parent_id.as_deref() {
            *child_count.entry(parent).or_insert(0) += 1;
        }
    }

    Ok(nodes
        .iter()
        .filter(|node| {
            node.parent_id.as_deref() == Some(root_id.as_str())
                && node.kind == NodeKind::Folder
                && child_count
                    .get(node.id.as_str())
                    .copied()
                    .unwrap_or_default()
                    == 0
        })
        .collect())
}

/// Remove every provably empty `agents/<id>/` folder in `company`, naming what
/// went.
///
/// `dry_run` returns the candidates without touching anything, so the console
/// can name every folder on a confirm dialog before the operator agrees to lose
/// them. A real run answers with what it actually removed, which is not
/// necessarily the same list — see below.
///
/// # Two snapshots, on purpose
///
/// A real run re-reads the tree and intersects the two candidate sets by id
/// before deleting anything. Issue #552's publish path can mint a deliverable
/// into a folder at any moment, and a candidate list computed once is a claim
/// about a tree that has since moved on. Two reads do not make this atomic —
/// the port offers no transaction across `tree` and `delete`, and pretending
/// otherwise would be worse than saying so — but they narrow the window to the
/// gap between the second read and each delete, instead of leaving it open for
/// the whole preview-then-confirm round trip.
///
/// A folder that raced away entirely (`delete` answering `false`) is logged and
/// left out of the result. It is not an error: the outcome the caller asked for
/// — that folder gone — is the outcome they got.
pub async fn sweep_empty_agent_folders(
    store: &dyn WorkspaceStore,
    company: &CompanyId,
    dry_run: bool,
) -> Result<Vec<SweptFolder>> {
    let nodes = store.tree(company).await?;
    let candidates: Vec<SweptFolder> = empty_agent_folder_candidates(&nodes)?
        .into_iter()
        .map(SweptFolder::from)
        .collect();

    if dry_run {
        return Ok(candidates);
    }

    let fresh = store.tree(company).await?;
    let still_empty: HashSet<&str> = empty_agent_folder_candidates(&fresh)?
        .into_iter()
        .map(|node| node.id.as_str())
        .collect();

    let mut removed = Vec::new();
    for folder in candidates {
        if !still_empty.contains(folder.id.as_str()) {
            tracing::info!(
                company = %company,
                node = %folder.id,
                name = %folder.name,
                "[workspace] `{AGENTS_ROOT}/{}` gained a child between the preview and the \
                 sweep; leaving it alone",
                folder.name
            );
            continue;
        }
        if store.delete(company, &folder.id).await? {
            removed.push(folder);
        } else {
            tracing::info!(
                company = %company,
                node = %folder.id,
                name = %folder.name,
                "[workspace] `{AGENTS_ROOT}/{}` was already gone",
                folder.name
            );
        }
    }

    tracing::info!(
        company = %company,
        count = removed.len(),
        folders = %removed
            .iter()
            .map(|folder| folder.name.as_str())
            .collect::<Vec<_>>()
            .join(", "),
        "[workspace] swept empty `{AGENTS_ROOT}/` member folders"
    );

    Ok(removed)
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::company::workspace_scaffold::{
        DESKS_ROOT, ensure_agent_folder, ensure_workspace_scaffold,
    };
    use crate::ports::workspace::WorkspaceOrigin;
    use crate::store::FsOps;

    /// Claim one folder and hand back its id.
    ///
    /// The store primitive (issue #759), spelled for a test that only wants a
    /// folder to exist. It replaced the scaffold's old `create_folder` helper,
    /// which was a plain read-then-create and is gone.
    async fn claim_folder(
        ws: &dyn WorkspaceStore,
        company: &CompanyId,
        name: &str,
        parent: Option<&str>,
        origin: WorkspaceOrigin,
    ) -> String {
        ws.adopt_or_create_folder(company, parent, name, origin)
            .await
            .expect("claim a folder")
            .into_node()
            .id
    }

    async fn store() -> (tempfile::TempDir, Arc<dyn WorkspaceStore>) {
        let dir = tempfile::tempdir().expect("tempdir");
        let ops: Arc<dyn WorkspaceStore> = Arc::new(FsOps::new(dir.path()));
        (dir, ops)
    }

    /// The sorted names in the tree, so an assertion says what survived rather
    /// than counting.
    async fn names(ws: &Arc<dyn WorkspaceStore>, company: &CompanyId) -> Vec<String> {
        let mut out: Vec<String> = ws
            .tree(company)
            .await
            .unwrap()
            .into_iter()
            .map(|node| node.name)
            .collect();
        out.sort();
        out
    }

    fn swept_names(folders: &[SweptFolder]) -> Vec<&str> {
        let mut out: Vec<&str> = folders.iter().map(|f| f.name.as_str()).collect();
        out.sort();
        out
    }

    async fn root_id(ws: &Arc<dyn WorkspaceStore>, company: &CompanyId) -> String {
        ws.tree(company)
            .await
            .unwrap()
            .into_iter()
            .find(|node| node.name == AGENTS_ROOT)
            .expect("the scaffold laid down the root")
            .id
    }

    fn folder(id: &str, name: &str, parent: Option<&str>) -> WorkspaceNode {
        WorkspaceNode {
            id: id.to_string(),
            name: name.to_string(),
            kind: NodeKind::Folder,
            parent_id: parent.map(str::to_string),
            updated_at_millis: 1,
            created_by: WorkspaceOrigin::Seed,
            updated_by: WorkspaceOrigin::Seed,
            mime: None,
            size: None,
            sha256: None,
            adopted: false,
        }
    }

    fn file(id: &str, name: &str, parent: Option<&str>) -> WorkspaceNode {
        WorkspaceNode {
            kind: NodeKind::File,
            ..folder(id, name, parent)
        }
    }

    // -- over a real store --------------------------------------------------

    /// The whole point, in one tree: the folder that was minted for a teammate
    /// who never produced anything goes, and the one holding a deliverable
    /// stays.
    #[tokio::test]
    async fn it_removes_empty_member_folders_and_keeps_occupied_ones() {
        let (_dir, ws) = store().await;
        let company = CompanyId::new("acme");
        ensure_workspace_scaffold(ws.as_ref(), &company)
            .await
            .unwrap();
        for id in ["ceo", "cmo", "cto"] {
            ensure_agent_folder(ws.as_ref(), &company, id)
                .await
                .unwrap();
        }
        // The CMO actually published something.
        let cmo = ws
            .tree(&company)
            .await
            .unwrap()
            .into_iter()
            .find(|node| node.name == "cmo")
            .unwrap()
            .id;
        claim_folder(
            ws.as_ref(),
            &company,
            "task-1",
            Some(&cmo),
            WorkspaceOrigin::Seed,
        )
        .await;

        let removed = sweep_empty_agent_folders(ws.as_ref(), &company, false)
            .await
            .unwrap();

        assert_eq!(swept_names(&removed), vec!["ceo", "cto"]);
        assert_eq!(
            names(&ws, &company).await,
            vec![
                "agents",
                "artifacts",
                "cmo",
                "readme.md",
                "readme.md",
                "secrets",
                "task-1"
            ],
            "a folder holding anything at all must survive"
        );
    }

    /// A *file* directly under `agents/` is somebody's note, not a stray
    /// container — and a file has no children by construction, so a predicate
    /// that forgot to check the kind would take it.
    #[tokio::test]
    async fn a_file_directly_under_the_root_is_never_a_candidate() {
        let (_dir, ws) = store().await;
        let company = CompanyId::new("acme");
        ensure_workspace_scaffold(ws.as_ref(), &company)
            .await
            .unwrap();
        let root = root_id(&ws, &company).await;
        ws.create(
            &company,
            &file("readme", "readme.md", Some(&root)),
            Some("# who is who"),
        )
        .await
        .unwrap();
        ensure_agent_folder(ws.as_ref(), &company, "ceo")
            .await
            .unwrap();

        let removed = sweep_empty_agent_folders(ws.as_ref(), &company, false)
            .await
            .unwrap();

        assert_eq!(swept_names(&removed), vec!["ceo"]);
        assert_eq!(
            names(&ws, &company).await,
            vec![
                "agents",
                "artifacts",
                "readme.md",
                "readme.md",
                "readme.md",
                "secrets"
            ]
        );
    }

    /// Acceptance criterion: running it twice is a no-op the second time. The
    /// sweep is a function of the current tree and holds no state, so this is
    /// the property that makes an operator's double click harmless.
    #[tokio::test]
    async fn running_it_twice_removes_nothing_the_second_time() {
        let (_dir, ws) = store().await;
        let company = CompanyId::new("acme");
        ensure_workspace_scaffold(ws.as_ref(), &company)
            .await
            .unwrap();
        ensure_agent_folder(ws.as_ref(), &company, "ceo")
            .await
            .unwrap();

        let first = sweep_empty_agent_folders(ws.as_ref(), &company, false)
            .await
            .unwrap();
        let after_first = names(&ws, &company).await;
        let second = sweep_empty_agent_folders(ws.as_ref(), &company, false)
            .await
            .unwrap();

        assert_eq!(swept_names(&first), vec!["ceo"]);
        assert!(second.is_empty(), "the second run removed {second:?}");
        assert_eq!(
            names(&ws, &company).await,
            after_first,
            "the second run must not change the tree either"
        );
    }

    /// A dry run is the confirm dialog's evidence: it names every folder and
    /// leaves the tree exactly as it found it.
    #[tokio::test]
    async fn a_dry_run_names_the_candidates_and_removes_nothing() {
        let (_dir, ws) = store().await;
        let company = CompanyId::new("acme");
        ensure_workspace_scaffold(ws.as_ref(), &company)
            .await
            .unwrap();
        for id in ["ceo", "cmo"] {
            ensure_agent_folder(ws.as_ref(), &company, id)
                .await
                .unwrap();
        }
        let before = names(&ws, &company).await;

        let preview = sweep_empty_agent_folders(ws.as_ref(), &company, true)
            .await
            .unwrap();

        assert_eq!(swept_names(&preview), vec!["ceo", "cmo"]);
        assert_eq!(names(&ws, &company).await, before);
        assert!(
            preview.iter().all(|folder| !folder.id.is_empty()),
            "the console needs an id to drop the row it is showing"
        );
    }

    /// Acceptance criterion: nothing outside `agents/` is touched — not an empty
    /// folder at the workspace root, not one under `desks/` (issue #645 left
    /// those alone on the same reasoning), and not a nested empty folder that
    /// happens to sit *below* a member folder.
    #[tokio::test]
    async fn nothing_outside_the_agents_root_is_touched() {
        let (_dir, ws) = store().await;
        let company = CompanyId::new("acme");
        ensure_workspace_scaffold(ws.as_ref(), &company)
            .await
            .unwrap();
        claim_folder(
            ws.as_ref(),
            &company,
            "archive",
            None,
            WorkspaceOrigin::Operator,
        )
        .await;
        let desks = claim_folder(
            ws.as_ref(),
            &company,
            DESKS_ROOT,
            None,
            WorkspaceOrigin::Seed,
        )
        .await;
        claim_folder(
            ws.as_ref(),
            &company,
            "creative-studio",
            Some(&desks),
            WorkspaceOrigin::Seed,
        )
        .await;
        let ceo = ensure_agent_folder(ws.as_ref(), &company, "ceo")
            .await
            .unwrap();
        claim_folder(
            ws.as_ref(),
            &company,
            "drafts",
            Some(&ceo),
            WorkspaceOrigin::Seed,
        )
        .await;

        let removed = sweep_empty_agent_folders(ws.as_ref(), &company, false)
            .await
            .unwrap();

        assert!(removed.is_empty(), "the sweep removed {removed:?}");
        assert_eq!(
            names(&ws, &company).await,
            vec![
                "agents",
                "archive",
                "artifacts",
                "ceo",
                "creative-studio",
                "desks",
                "drafts",
                "readme.md",
                "readme.md",
                "secrets"
            ],
        );
    }

    /// No `agents/` root at all — a company that never booted the scaffold — is
    /// nothing to sweep rather than an error. The console offers the action
    /// unconditionally, so this is a real request shape, not a defensive branch.
    #[tokio::test]
    async fn a_company_with_no_agents_root_is_a_no_op() {
        let (_dir, ws) = store().await;
        let company = CompanyId::new("fresh");
        claim_folder(
            ws.as_ref(),
            &company,
            "Notes",
            None,
            WorkspaceOrigin::Operator,
        )
        .await;

        let removed = sweep_empty_agent_folders(ws.as_ref(), &company, false)
            .await
            .unwrap();

        assert!(removed.is_empty());
        assert_eq!(names(&ws, &company).await, vec!["Notes"]);
    }

    /// Fail closed, exactly as the scaffold does: two roots named `Agents` make
    /// "under `agents/`" undecidable, so the sweep refuses instead of picking
    /// one and deleting beneath it.
    ///
    /// Hand-built rather than driven through `FsOps` on purpose: the backend now
    /// rejects a second `Agents` root at creation (`reject_path_collision`, see
    /// issue #665), so the ambiguous shape is no longer reachable through it —
    /// the same reason the #671 shape below is fed straight to the predicate.
    #[test]
    fn an_ambiguous_root_is_refused_and_removes_nothing() {
        let nodes = vec![
            folder("dup-a", AGENTS_ROOT, None),
            folder("dup-b", AGENTS_ROOT, None),
            // A real member beneath the (ambiguous) root: there is content a
            // naive pick would have deleted, and the refusal exists to protect
            // exactly that.
            file("stray", "stray.md", Some("dup-a")),
        ];

        let err = empty_agent_folder_candidates(&nodes)
            .expect_err("an ambiguous root must not be swept beneath");
        assert!(err.to_string().contains(AGENTS_ROOT), "{err}");
    }

    /// A *file* named `Agents` is the other unresolvable shape, and the refusal
    /// has to reach the dry run too — a preview that answered "nothing to do"
    /// would tell the operator the tree is fine when it is not.
    #[tokio::test]
    async fn a_root_file_named_agents_is_refused_on_the_dry_run_too() {
        let (_dir, ws) = store().await;
        let company = CompanyId::new("odd");
        ws.create(
            &company,
            &file("note", AGENTS_ROOT, None),
            Some("# not a folder"),
        )
        .await
        .unwrap();

        sweep_empty_agent_folders(ws.as_ref(), &company, true)
            .await
            .expect_err("a file carrying the root's name must be refused, not swept past");
    }

    /// The sweep is company-scoped: another tenant's identical tree is not
    /// touched.
    #[tokio::test]
    async fn sweeping_one_company_leaves_another_alone() {
        let (_dir, ws) = store().await;
        let acme = CompanyId::new("acme");
        let other = CompanyId::new("other");
        for company in [&acme, &other] {
            ensure_workspace_scaffold(ws.as_ref(), company)
                .await
                .unwrap();
            ensure_agent_folder(ws.as_ref(), company, "ceo")
                .await
                .unwrap();
        }

        sweep_empty_agent_folders(ws.as_ref(), &acme, false)
            .await
            .unwrap();

        assert_eq!(
            names(&ws, &acme).await,
            vec!["agents", "artifacts", "readme.md", "readme.md", "secrets"]
        );
        assert_eq!(
            names(&ws, &other).await,
            vec![
                "agents",
                "artifacts",
                "ceo",
                "readme.md",
                "readme.md",
                "secrets"
            ]
        );
    }

    // -- the predicate, on shapes no backend here would create ---------------

    /// The issue #671 regression, stated as a dual assertion so it cannot be
    /// satisfied by accident: a folder whose only child carries a path separator
    /// in its name reads as **empty** by every path-shaped measure, while the
    /// port's recursive `delete` would still take the child. The structural
    /// count is what closes the gap, and this pins both halves.
    ///
    /// Hand-built rather than driven through a store on purpose: the `fs`
    /// backend rejects such a name at creation (`reject_unsafe_name`), so the
    /// shape is unreachable through `FsOps` — and sqlite and mongodb, the
    /// backends hosted tenants actually run, accept it. See the sqlite and
    /// mongodb test modules for the same shape through a real backend.
    #[test]
    fn a_folder_holding_only_an_unaddressable_child_is_not_a_candidate() {
        let nodes = vec![
            folder("root", AGENTS_ROOT, None),
            folder("ghost", "ceo", Some("root")),
            // No renderable path: the name carries a separator, so every
            // path-shaped index drops it.
            file("hidden", "quarterly/report.md", Some("ghost")),
        ];

        // What a path-shaped emptiness check would have measured: the rendered
        // paths beneath `agents/ceo`. It sees nothing — that is the bug.
        let by_id: HashMap<&str, &WorkspaceNode> =
            nodes.iter().map(|n| (n.id.as_str(), n)).collect();
        let beneath = nodes
            .iter()
            .filter(|node| {
                crate::company::workspace_paths::render_path(node, &by_id)
                    .is_some_and(|path| path.starts_with("agents/ceo/"))
            })
            .count();
        assert_eq!(
            beneath, 0,
            "precondition: the path-shaped measure cannot see this child — that is the bug"
        );

        let candidates = empty_agent_folder_candidates(&nodes).unwrap();
        assert!(
            candidates.is_empty(),
            "the structural measure must see a child the path rules exclude, but the sweep \
             offered {:?}",
            swept_names(
                &candidates
                    .iter()
                    .copied()
                    .map(SweptFolder::from)
                    .collect::<Vec<_>>()
            )
        );
    }

    /// The dangling-chain half of the #671 shapes, and the thing it is easy to
    /// get backwards.
    ///
    /// A node whose parent id names nothing has no renderable path at all, so a
    /// naive renderer falls back to its bare name and files it at the workspace
    /// root. That must not make it a member folder (it is not a child of the
    /// root **by id**), and it must not protect an unrelated empty folder — its
    /// child count belongs to the parent it names, which is not in the tree.
    ///
    /// Note what this shows about the exposure here: a child of a *candidate*
    /// always has an intact chain through that candidate to the root, so a
    /// dangling ancestor cannot hide one. The separator-named case above is the
    /// whole of the #671 gap for this predicate — which is why the count is
    /// still structural, and why this test states the boundary rather than
    /// leaving it to be rediscovered.
    #[test]
    fn a_dangling_node_is_neither_a_candidate_nor_a_protection() {
        let nodes = vec![
            folder("root", AGENTS_ROOT, None),
            folder("ghost", "ceo", Some("root")),
            // Parent names a node that is not in the tree.
            file("orphan", "stray.md", Some("vanished")),
        ];

        let candidates = empty_agent_folder_candidates(&nodes).unwrap();

        assert_eq!(
            candidates.iter().map(|n| n.id.as_str()).collect::<Vec<_>>(),
            vec!["ghost"],
            "a stray elsewhere in the tree neither joins the sweep nor calls it off"
        );
    }

    /// A folder holding only *folders* is still occupied — the count is over
    /// children, not over files.
    #[test]
    fn a_folder_holding_only_a_subfolder_is_not_a_candidate() {
        let nodes = vec![
            folder("root", AGENTS_ROOT, None),
            folder("ghost", "ceo", Some("root")),
            folder("sub", "drafts", Some("ghost")),
            folder("empty", "cto", Some("root")),
        ];

        let candidates = empty_agent_folder_candidates(&nodes).unwrap();

        assert_eq!(
            candidates.iter().map(|n| n.id.as_str()).collect::<Vec<_>>(),
            vec!["empty"]
        );
    }

    /// Only *direct* children of the root are members. A folder two levels down
    /// is somebody's own structure, and emptying it is not this sweep's business
    /// even when it is empty.
    #[test]
    fn a_grandchild_of_the_root_is_never_a_candidate() {
        let nodes = vec![
            folder("root", AGENTS_ROOT, None),
            folder("ceo", "ceo", Some("root")),
            folder("deep", "archive", Some("ceo")),
        ];

        let candidates = empty_agent_folder_candidates(&nodes).unwrap();

        assert!(
            candidates.is_empty(),
            "`ceo` holds `archive`, and `archive` is not a member folder"
        );
    }

    /// The root is resolved by id, so a folder named `Agents` *nested* somewhere
    /// else does not lend its children to the sweep.
    #[test]
    fn a_nested_folder_named_agents_is_not_the_root() {
        let nodes = vec![
            folder("root", AGENTS_ROOT, None),
            folder("archive", "archive", None),
            folder("decoy", AGENTS_ROOT, Some("archive")),
            folder("under-decoy", "ceo", Some("decoy")),
        ];

        let candidates = empty_agent_folder_candidates(&nodes).unwrap();

        assert!(
            candidates.is_empty(),
            "only children of the real root are members, but the sweep offered {:?}",
            candidates.iter().map(|n| n.id.as_str()).collect::<Vec<_>>()
        );
    }
}
