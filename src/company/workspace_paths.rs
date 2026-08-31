//! Logical paths over the workspace tree: rendering one, and validating one a
//! caller supplied.
//!
//! Node ids are ULIDs and a node's *path* is a derived value — the names on its
//! ancestor chain, joined with `/`. Two surfaces need that derivation and they
//! must agree exactly:
//!
//! * the agent tools (`harness::workspace_tools`), whose `PathIndex` is how
//!   `workspace_read` decides a node is addressable at all, and
//! * the workspace search helper ([`crate::company::workspace_search`]), which
//!   must never surface a hit for a node `workspace_read` would then refuse.
//!
//! Two copies of these rules would drift the moment one side gained a case, and
//! the failure would be silent in the direction that matters: search offering a
//! node nothing can open. So the rules live here, once, and both callers import
//! them.
//!
//! Always compiled and openhuman-free — search reaches the REST and GraphQL read
//! surfaces, which are in the default build, while the tools are behind
//! `openhuman`. The shared rule therefore cannot live under `src/harness/`.

use std::collections::HashMap;

use crate::ports::workspace::WorkspaceNode;

/// Depth guard when walking a node's ancestor chain to render its path.
///
/// The stores reject parent cycles on `rename_move`, but a hand-edited backing
/// row could still present one; this bounds the walk regardless.
pub(crate) const MAX_PATH_DEPTH: usize = 64;

/// Whether `name` is a legal single path segment.
///
/// Mirrors the `fs` backend's `reject_unsafe_name`, applied here so the sqlite
/// and mongodb backends — which do not validate names on create — cannot
/// present a node whose name would make a rendered path ambiguous or
/// traversal-shaped.
pub(crate) fn is_legal_segment(name: &str) -> bool {
    !name.is_empty()
        && name != "."
        && name != ".."
        && !name.contains('/')
        && !name.contains('\\')
        && !name.contains('\0')
}

/// Render a node's logical path by walking its ancestor chain to the root.
///
/// Returns `None` — leaving the node unaddressable by path — when the chain
/// dangles, exceeds [`MAX_PATH_DEPTH`], or any name on it is not a legal single
/// path segment.
pub(crate) fn render_path(
    node: &WorkspaceNode,
    by_id: &HashMap<&str, &WorkspaceNode>,
) -> Option<String> {
    let mut names = Vec::new();
    let mut cursor = Some(node);
    let mut depth = 0;
    while let Some(current) = cursor {
        if !is_legal_segment(&current.name) {
            return None;
        }
        names.push(current.name.as_str());
        depth += 1;
        if depth > MAX_PATH_DEPTH {
            return None;
        }
        cursor = match &current.parent_id {
            None => None,
            // A dangling parent means the chain never reaches the root, so the
            // node has no well-defined path.
            Some(parent) => Some(*by_id.get(parent.as_str())?),
        };
    }
    names.reverse();
    Some(names.join("/"))
}

/// Split a caller-supplied logical path into validated segments.
///
/// Takes the component-wise shape of tinycortex's `resolve_within_content_root`:
/// validate every component *before* it can be used, and reject rather than
/// normalise anything traversal-shaped. Leading/trailing and repeated `/` are
/// tolerated (a caller writing `/standards/` means `Standards`); `.` and `..`
/// segments are refused outright.
///
/// Note this is defence in depth, not the boundary itself: the result is only
/// ever matched against node names inside a company-scoped index, never joined
/// onto a host path.
pub(crate) fn split_logical_path(path: &str) -> Result<Vec<&str>, String> {
    let trimmed = path.trim();
    if trimmed.is_empty() {
        return Err("`path` is empty".to_string());
    }
    if trimmed.contains('\\') {
        return Err(format!(
            "`{trimmed}` contains a backslash; workspace paths separate segments with `/`"
        ));
    }
    let segments: Vec<&str> = trimmed.split('/').filter(|s| !s.is_empty()).collect();
    if segments.is_empty() {
        return Err(format!("`{path}` names no path segments"));
    }
    for segment in &segments {
        if *segment == "." || *segment == ".." {
            return Err(format!(
                "`{trimmed}` contains a `{segment}` segment; workspace paths are absolute within \
                 the company workspace and cannot traverse"
            ));
        }
    }
    Ok(segments)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ports::workspace::{NodeKind, WorkspaceOrigin};

    fn node(id: &str, name: &str, parent: Option<&str>) -> WorkspaceNode {
        WorkspaceNode {
            id: id.to_string(),
            name: name.to_string(),
            kind: NodeKind::File,
            parent_id: parent.map(str::to_string),
            updated_at_millis: 1,
            created_by: WorkspaceOrigin::Operator,
            updated_by: WorkspaceOrigin::Operator,
            mime: None,
            size: None,
            sha256: None,
            adopted: false,
        }
    }

    fn index(nodes: &[WorkspaceNode]) -> HashMap<&str, &WorkspaceNode> {
        nodes.iter().map(|n| (n.id.as_str(), n)).collect()
    }

    #[test]
    fn a_path_is_the_ancestor_chain_joined() {
        let nodes = vec![
            node("a", "standards", None),
            node("b", "engineering-standards.md", Some("a")),
        ];
        let by_id = index(&nodes);
        assert_eq!(
            render_path(&nodes[1], &by_id).as_deref(),
            Some("standards/engineering-standards.md")
        );
        assert_eq!(render_path(&nodes[0], &by_id).as_deref(), Some("standards"));
    }

    #[test]
    fn a_dangling_or_cyclic_chain_has_no_path() {
        let orphan = vec![node("b", "note.md", Some("missing"))];
        assert_eq!(render_path(&orphan[0], &index(&orphan)), None);

        let cycle = vec![node("a", "A", Some("b")), node("b", "B", Some("a"))];
        let by_id = index(&cycle);
        assert_eq!(render_path(&cycle[0], &by_id), None);
        assert_eq!(render_path(&cycle[1], &by_id), None);
    }

    #[test]
    fn an_illegal_segment_anywhere_on_the_chain_has_no_path() {
        for name in ["..", ".", "a/b", "a\\b", "", "nul\0"] {
            let nodes = vec![node("x", name, None)];
            assert_eq!(
                render_path(&nodes[0], &index(&nodes)),
                None,
                "name {name:?} must not render a path"
            );
        }
        // …including when the illegal name is an *ancestor*, not the node.
        let nodes = vec![node("a", "..", None), node("b", "note.md", Some("a"))];
        assert_eq!(render_path(&nodes[1], &index(&nodes)), None);
    }

    #[test]
    fn traversal_shaped_paths_are_rejected() {
        for path in [
            "../secrets.md",
            "standards/../../etc/passwd",
            "./Standards",
            "..",
            "standards/..",
            "C:\\Windows",
            "   ",
        ] {
            assert!(
                split_logical_path(path).is_err(),
                "path {path:?} must be rejected"
            );
        }
    }

    #[test]
    fn redundant_separators_are_tolerated_but_segments_are_not_invented() {
        assert_eq!(
            split_logical_path("/standards/").unwrap(),
            vec!["standards"]
        );
        assert_eq!(
            split_logical_path("standards//eng.md").unwrap(),
            vec!["standards", "eng.md"]
        );
        assert!(split_logical_path("/").unwrap_err().contains("segments"));
    }
}
