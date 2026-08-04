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
    // Lowercased to match the console, which resolves a wiki link through
    // `fileByTitle` with both sides lowercased (`frontend/src/lib/workspace.ts`).
    // Comparing case-sensitively here would let `[[Voice]]` render as a resolved
    // link to `voice.md` while `voice.md` reported no backlink — the two halves
    // of one feature disagreeing about the same link.
    let target = link_target(&node.name).to_ascii_lowercase();

    // Backlinks: scan every other file node's content for a `[[target]]` link.
    let mut backlinks = Vec::new();
    for other in store.tree(company).await? {
        if other.id == node.id || !matches!(other.kind, NodeKind::File) {
            continue;
        }
        if let Some((other_node, other_content)) = store.read(company, &other.id).await?
            && extract_wikilinks(&other_content)
                .iter()
                .any(|link| link.to_ascii_lowercase() == target)
        {
            backlinks.push(other_node);
        }
    }

    Ok(Some((node, content, backlinks)))
}
