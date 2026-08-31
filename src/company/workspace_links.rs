//! One workspace file plus the `[[wikilink]]` backlinks pointing at it.
//!
//! The company workspace is read through two surfaces — the GraphQL
//! `Company.workspaceFile(id)` resolver and the REST `GET …/workspace/file/{id}`
//! route the operator console calls. Both need the same answer: the node, its
//! body, and every *other* file whose content links to it. That scan lives here,
//! once, so the two surfaces cannot drift apart (a backlink the console shows
//! but GraphQL doesn't, or vice versa, would be a silent correctness bug in
//! whichever one the reader wasn't looking at).
//!
//! Always compiled and feature-free: `ops::workspace` is in the default build,
//! so gating this would only re-create the divergence it exists to prevent.

use crate::Result;
use crate::company::extract_wikilinks;
use crate::ports::types::CompanyId;
use crate::ports::workspace::{NodeKind, WorkspaceNode, WorkspaceStore};

/// The link target a file's name presents: its base name minus a `.md` suffix.
///
/// Deliberately narrower than the console's `titleOf`, which also strips
/// `.markdown` / `.txt`. A `[[link]]` resolves against note *names*, and the
/// seeder only ever creates `.md` notes, so `.md` is the only extension a link
/// can be written without. See the "Known limits" note in the workspace module
/// docs.
pub(crate) fn link_target(name: &str) -> &str {
    name.strip_suffix(".md").unwrap_or(name)
}

/// The form two link targets are compared in: the workspace naming rule,
/// applied to both sides.
///
/// One function so a link and the note it points at are always reduced the same
/// way — the failure this replaces was exactly two sides reducing differently.
fn link_key(target: &str) -> String {
    crate::company::workspace_names::kebab_name(target)
}

/// Reads one workspace node with its content and inbound `[[wikilink]]`
/// backlinks, or `None` when the id names nothing in this company's tree.
///
/// Kind-agnostic on purpose: the store answers a folder id with an empty body,
/// and each caller decides what that means (GraphQL returns it; the REST route
/// 404s, because the console only ever opens files). Backlinks only ever name
/// *file* nodes, and a node never links to itself.
pub(crate) async fn file_with_backlinks(
    store: &dyn WorkspaceStore,
    company: &CompanyId,
    id: &str,
) -> Result<Option<(WorkspaceNode, String, Vec<WorkspaceNode>)>> {
    let Some((node, content)) = store.read(company, id).await? else {
        return Ok(None);
    };
    // Normalized to match the console, which resolves a wiki link through
    // `fileByTitle` against the same rule (`frontend/src/lib/workspace.ts`).
    // Comparing literally here would let `[[Voice]]` render as a resolved link
    // to `voice.md` while `voice.md` reported no backlink — the two halves of
    // one feature disagreeing about the same link.
    //
    // The rule is the workspace naming rule itself
    // ([`crate::company::workspace_names`]), not merely a lowercasing: a note is
    // stored as `close-checklist.md` while people, and the seeded prose,
    // reasonably write `[[Close checklist]]`. Link text is how a document reads;
    // the file name is what the tree is kept in, and those two need not be the
    // same string for the link to resolve.
    let target = link_key(link_target(&node.name));

    // Backlinks: scan every other file node's content for a `[[target]]` link.
    let mut backlinks = Vec::new();
    for other in store.tree(company).await? {
        if other.id == node.id || !matches!(other.kind, NodeKind::File) {
            continue;
        }
        if let Some((other_node, other_content)) = store.read(company, &other.id).await?
            && extract_wikilinks(&other_content)
                .iter()
                .any(|link| link_key(link) == target)
        {
            backlinks.push(other_node);
        }
    }

    Ok(Some((node, content, backlinks)))
}

#[cfg(test)]
mod test {
    use std::sync::Arc;

    use super::*;
    use crate::ports::workspace::WorkspaceOrigin;
    use crate::store::FsOps;

    async fn note(
        ws: &Arc<dyn WorkspaceStore>,
        company: &CompanyId,
        name: &str,
        body: &str,
    ) -> String {
        let id = crate::ports::generate_id();
        ws.create(
            company,
            &crate::ports::workspace::WorkspaceNode {
                id: id.clone(),
                name: name.to_string(),
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
            Some(body),
        )
        .await
        .expect("create");
        id
    }

    /// Link text is how a document reads; the file name is what the tree is
    /// kept in. `[[Close checklist]]` therefore has to find `close-checklist.md`
    /// — otherwise the lowercase-dashed naming rule would have silently
    /// unresolved every wiki link in every seeded company on the day it landed.
    #[tokio::test]
    async fn a_link_written_as_prose_backlinks_a_dashed_note() {
        let dir = tempfile::tempdir().expect("tempdir");
        let ws: Arc<dyn WorkspaceStore> = Arc::new(FsOps::new(dir.path()));
        let company = CompanyId::new("acme");

        let target = note(&ws, &company, "close-checklist.md", "# Close").await;
        note(
            &ws,
            &company,
            "readme.md",
            "Follow the [[Close checklist]] every month.",
        )
        .await;

        let (_, _, backlinks) = file_with_backlinks(ws.as_ref(), &company, &target)
            .await
            .expect("reads")
            .expect("the note exists");

        assert_eq!(
            backlinks
                .iter()
                .map(|n| n.name.as_str())
                .collect::<Vec<_>>(),
            vec!["readme.md"],
        );
    }

    /// The converse, for a company that predates the rule: a note still named
    /// `Close checklist.md` is found by a link written in the new spelling.
    #[tokio::test]
    async fn a_dashed_link_backlinks_a_legacy_note() {
        let dir = tempfile::tempdir().expect("tempdir");
        let ws: Arc<dyn WorkspaceStore> = Arc::new(FsOps::new(dir.path()));
        let company = CompanyId::new("acme");

        let target = note(&ws, &company, "Close checklist.md", "# Close").await;
        note(&ws, &company, "readme.md", "See [[close-checklist]].").await;

        let (_, _, backlinks) = file_with_backlinks(ws.as_ref(), &company, &target)
            .await
            .expect("reads")
            .expect("the note exists");

        assert_eq!(backlinks.len(), 1, "{backlinks:?}");
    }

    /// Two notes that differ by more than separators stay unlinked — the rule
    /// normalizes spelling, it does not make matching fuzzy.
    #[tokio::test]
    async fn an_unrelated_link_is_not_a_backlink() {
        let dir = tempfile::tempdir().expect("tempdir");
        let ws: Arc<dyn WorkspaceStore> = Arc::new(FsOps::new(dir.path()));
        let company = CompanyId::new("acme");

        let target = note(&ws, &company, "close-checklist.md", "# Close").await;
        note(&ws, &company, "readme.md", "See [[open-checklist]].").await;

        let (_, _, backlinks) = file_with_backlinks(ws.as_ref(), &company, &target)
            .await
            .expect("reads")
            .expect("the note exists");

        assert!(backlinks.is_empty(), "{backlinks:?}");
    }
}
