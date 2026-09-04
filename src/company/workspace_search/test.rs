//! Unit tests for [`search_workspace`](super::search_workspace), run against a
//! live `FsOps` store wherever one can express the case.
//!
//! A hand-written double would prove the sort and the excerpt logic and nothing
//! else; the properties worth pinning here — that a binary node's text `read` is
//! empty, that two companies cannot see each other — are properties of a real
//! backend. The one exception is [`FixedTree`] at the bottom, which exists for
//! the nodes `FsOps` *refuses to create* and the other two backends accept.

use std::num::NonZeroUsize;
use std::sync::Arc;

use super::*;
use crate::ports::workspace::{NodeKind, WorkspaceOrigin};
use crate::store::FsOps;

fn limit(n: usize) -> NonZeroUsize {
    NonZeroUsize::new(n).expect("test limits are non-zero")
}

fn node(id: &str, name: &str, parent: Option<&str>, kind: NodeKind, rev: u64) -> WorkspaceNode {
    WorkspaceNode {
        id: id.to_string(),
        name: name.to_string(),
        kind,
        parent_id: parent.map(str::to_string),
        updated_at_millis: rev,
        created_by: WorkspaceOrigin::Operator,
        updated_by: WorkspaceOrigin::Operator,
        mime: None,
        size: None,
        sha256: None,
        adopted: false,
    }
}

fn folder(id: &str, name: &str, parent: Option<&str>) -> WorkspaceNode {
    node(id, name, parent, NodeKind::Folder, 1_000)
}

fn file(id: &str, name: &str, parent: Option<&str>, rev: u64) -> WorkspaceNode {
    node(id, name, parent, NodeKind::File, rev)
}

/// A live store plus the tempdir keeping it alive.
fn store() -> (tempfile::TempDir, Arc<dyn WorkspaceStore>) {
    let dir = tempfile::tempdir().expect("tempdir");
    let ops: Arc<dyn WorkspaceStore> = Arc::new(FsOps::new(dir.path()));
    (dir, ops)
}

/// `standards/` with two notes, plus a root README.
async fn seeded() -> (tempfile::TempDir, Arc<dyn WorkspaceStore>, CompanyId) {
    let (dir, ops) = store();
    let id = CompanyId::new("acme");
    ops.create(&id, &folder("f-std", "standards", None), None)
        .await
        .unwrap();
    ops.create(
        &id,
        &file("n-eng", "Engineering standards.md", Some("f-std"), 3_000),
        Some("# Engineering\nReview every PR before merging."),
    )
    .await
    .unwrap();
    ops.create(
        &id,
        &file("n-support", "support-playbook.md", Some("f-std"), 2_000),
        Some("Escalate a refund request to the CEO."),
    )
    .await
    .unwrap();
    ops.create(
        &id,
        &file("n-readme", "readme.md", None, 1_000),
        Some("# Acme\nNothing to see."),
    )
    .await
    .unwrap();
    (dir, ops, id)
}

async fn search(
    ops: &Arc<dyn WorkspaceStore>,
    id: &CompanyId,
    query: &str,
) -> super::SearchOutcome {
    search_workspace(ops.as_ref(), id, query, None, limit(20))
        .await
        .expect("search")
}

fn paths(outcome: &SearchOutcome) -> Vec<&str> {
    outcome.hits.iter().map(|h| h.path.as_str()).collect()
}

// -- matching ---------------------------------------------------------------

/// The base case, both halves: a name match and a content match for the same
/// query, each labelled with what it matched.
#[tokio::test]
async fn a_query_matches_both_names_and_bodies() {
    let (_dir, ops, id) = seeded().await;

    let by_name = search(&ops, &id, "playbook").await;
    assert_eq!(paths(&by_name), vec!["standards/support-playbook.md"]);
    assert_eq!(by_name.hits[0].matched, MatchKind::Name);
    assert_eq!(by_name.hits[0].excerpt, None, "a name match has no excerpt");

    let by_content = search(&ops, &id, "refund").await;
    assert_eq!(paths(&by_content), vec!["standards/support-playbook.md"]);
    assert_eq!(by_content.hits[0].matched, MatchKind::Content);
    assert!(
        by_content.hits[0]
            .excerpt
            .as_deref()
            .unwrap()
            .contains("refund"),
        "{:?}",
        by_content.hits[0].excerpt
    );

    // Nothing matches nothing — an empty result, not an error.
    let miss = search(&ops, &id, "quarterly dividend").await;
    assert!(miss.hits.is_empty());
    assert_eq!(miss.total, 0);
}

/// A node whose name matches is reported as a name hit even when its body also
/// contains the query — one node is one hit, and the stronger signal wins.
#[tokio::test]
async fn a_node_matching_both_ways_is_reported_once_as_a_name_match() {
    let (_dir, ops) = store();
    let id = CompanyId::new("acme");
    ops.create(
        &id,
        &file("n", "Refunds.md", None, 1_000),
        Some("Our refund policy."),
    )
    .await
    .unwrap();

    let outcome = search(&ops, &id, "refund").await;
    assert_eq!(outcome.total, 1);
    assert_eq!(outcome.hits[0].matched, MatchKind::Name);
}

/// Folders are searchable by name — a query naming a section should find the
/// section, not just the notes inside it.
#[tokio::test]
async fn a_folder_matches_by_name_and_never_carries_an_excerpt() {
    let (_dir, ops, id) = seeded().await;
    let outcome = search(&ops, &id, "standards").await;
    let folder = outcome
        .hits
        .iter()
        .find(|h| h.path == "standards")
        .expect("the folder itself must be a hit");
    assert_eq!(folder.node.kind, NodeKind::Folder);
    assert_eq!(folder.matched, MatchKind::Name);
    assert_eq!(folder.excerpt, None);
}

/// Case folding is Unicode's, not ASCII's — otherwise a query in any language
/// with cased non-ASCII letters silently matches only what happens to be typed
/// in the same case.
#[tokio::test]
async fn matching_folds_case_beyond_ascii() {
    let (_dir, ops) = store();
    let id = CompanyId::new("acme");
    ops.create(
        &id,
        &file("n-de", "STRASSE.md", None, 1_000),
        Some("Die STRAßE ist GRÜN."),
    )
    .await
    .unwrap();
    ops.create(
        &id,
        &file("n-tr", "notes.md", None, 1_000),
        Some("ÉCOLE POLYTECHNIQUE"),
    )
    .await
    .unwrap();

    assert_eq!(
        paths(&search(&ops, &id, "strasse").await),
        vec!["STRASSE.md"]
    );
    assert_eq!(paths(&search(&ops, &id, "grün").await), vec!["STRASSE.md"]);
    assert_eq!(paths(&search(&ops, &id, "GRÜN").await), vec!["STRASSE.md"]);
    assert_eq!(paths(&search(&ops, &id, "école").await), vec!["notes.md"]);
}

// -- excerpts ---------------------------------------------------------------

/// The regression this module's offset map exists for.
///
/// `İ` (U+0130) is two bytes and lowercases to three, so every byte offset found
/// in the lowercased copy of this body is *ahead* of the character it names in
/// the original. Slicing the original at that raw offset lands mid-codepoint and
/// panics. The excerpt must come back intact, and the test asserts on the
/// content rather than merely on "it did not panic" — a silently misaligned
/// window would pass the weaker check.
#[tokio::test]
async fn an_excerpt_survives_a_lowercasing_that_changes_byte_length() {
    let (_dir, ops) = store();
    let id = CompanyId::new("acme");
    // Ten expanding characters ahead of the match, so the naive offset is a full
    // ten bytes off — comfortably inside the following multibyte run.
    let body = format!("{}🦀🦀🦀 needle 🦀🦀🦀", "İ".repeat(10));
    ops.create(&id, &file("n", "note.md", None, 1_000), Some(&body))
        .await
        .unwrap();

    let outcome = search(&ops, &id, "needle").await;
    let excerpt = outcome.hits[0].excerpt.as_deref().unwrap();
    assert!(excerpt.contains("needle"), "{excerpt}");
    assert!(
        excerpt.contains('🦀'),
        "the window must not be shifted: {excerpt}"
    );
}

/// Both window edges land on char boundaries for every match position in a body
/// made entirely of multibyte characters — the property, swept, rather than one
/// lucky offset.
#[tokio::test]
async fn excerpt_windows_never_split_a_codepoint() {
    for lead in 0..40 {
        let body = format!("{}needle{}", "🦀".repeat(lead), "é".repeat(80));
        let excerpt = super::excerpt_around(&body, "needle").expect("match");
        assert!(excerpt.contains("needle"), "lead {lead}: {excerpt}");
        // Round-tripping through `String` proves nothing on its own — the real
        // proof is that building it did not panic, and that no replacement
        // character was produced.
        assert!(
            !excerpt.contains('\u{fffd}'),
            "lead {lead} produced a replacement character: {excerpt}"
        );
    }
}

/// A long body is elided on both sides and collapsed to one line, so a page of
/// hits reads as a list rather than as fragments of someone's formatting.
#[tokio::test]
async fn a_long_body_is_elided_on_both_sides_and_kept_to_one_line() {
    let body = format!("{}\n\n  needle  \n\n{}", "a ".repeat(400), "b ".repeat(400));
    let excerpt = super::excerpt_around(&body, "needle").expect("match");
    assert!(excerpt.starts_with('…'), "{excerpt}");
    assert!(excerpt.ends_with('…'), "{excerpt}");
    assert!(!excerpt.contains('\n'), "{excerpt}");
    assert!(excerpt.contains("needle"), "{excerpt}");
}

/// A short body is shown whole, with no ellipsis promising text that is not
/// there.
#[tokio::test]
async fn a_short_body_is_not_elided() {
    let excerpt = super::excerpt_around("a needle here", "needle").expect("match");
    assert_eq!(excerpt, "a needle here");
}

// -- binary nodes (issue #553) ----------------------------------------------

/// Constraint from #553, pinned: a binary node matches on its **name** and is
/// never content-scanned.
///
/// The port defines a text `read` of a binary node as an empty body, so a scan
/// built over `read` would find nothing in a payload without knowing payloads
/// exist — this states the rule rather than inheriting the silence. The second
/// half is the one with teeth: a query matching bytes *inside* the payload finds
/// nothing, which is what proves no byte scan happened.
#[tokio::test]
async fn a_binary_node_matches_by_name_and_is_never_content_scanned() {
    let (_dir, ops) = store();
    let id = CompanyId::new("acme");
    let mut chart = file("n-png", "refund chart.png", None, 5_000);
    chart.mime = Some("image/png".to_string());
    // Plain-ASCII payload bytes: if anything ever content-scanned a binary node,
    // this string is what it would find.
    ops.create_binary(&id, &chart, b"PNG-PAYLOAD-secretword")
        .await
        .unwrap();

    let by_name = search(&ops, &id, "refund").await;
    assert_eq!(paths(&by_name), vec!["refund chart.png"]);
    assert_eq!(by_name.hits[0].matched, MatchKind::Name);
    assert_eq!(
        by_name.hits[0].excerpt, None,
        "a payload must never be excerpted into a result"
    );
    // The hit still describes the payload, off the tree read alone.
    assert_eq!(by_name.hits[0].node.mime.as_deref(), Some("image/png"));
    assert!(by_name.hits[0].node.size.is_some());

    let by_payload = search(&ops, &id, "secretword").await;
    assert!(
        by_payload.hits.is_empty(),
        "payload bytes must not be searchable: {:?}",
        paths(&by_payload)
    );
}

// -- tenancy ----------------------------------------------------------------

/// The port's hard invariant — "Company A's files MUST be invisible to company
/// B" — asserted with both companies holding content that matches the same
/// query, so a leak would show up as an extra hit rather than as an empty result
/// that proves nothing.
#[tokio::test]
async fn a_search_never_crosses_the_company_boundary() {
    let (_dir, ops) = store();
    let a = CompanyId::new("acme");
    let b = CompanyId::new("beta");
    ops.create(
        &a,
        &file("n-a", "acme refunds.md", None, 1_000),
        Some("Acme refund policy."),
    )
    .await
    .unwrap();
    ops.create(
        &b,
        &file("n-b", "beta refunds.md", None, 1_000),
        Some("Beta refund policy."),
    )
    .await
    .unwrap();

    assert_eq!(
        paths(&search(&ops, &a, "refund").await),
        vec!["acme refunds.md"]
    );
    assert_eq!(
        paths(&search(&ops, &b, "refund").await),
        vec!["beta refunds.md"]
    );
}

// -- ordering, limits and totals --------------------------------------------

/// Name before content, freshest first inside each group, path as the tie-break.
/// Deterministic regardless of the order `tree()` happened to return.
#[tokio::test]
async fn hits_order_names_first_then_freshest_then_path() {
    let (_dir, ops) = store();
    let id = CompanyId::new("acme");
    // Two name matches with different revisions, two content matches likewise,
    // and two content matches sharing a revision to exercise the path tie-break.
    ops.create(&id, &file("n1", "alpha topic.md", None, 1_000), Some("x"))
        .await
        .unwrap();
    ops.create(&id, &file("n2", "beta topic.md", None, 9_000), Some("x"))
        .await
        .unwrap();
    ops.create(
        &id,
        &file("n3", "gamma.md", None, 5_000),
        Some("mentions topic"),
    )
    .await
    .unwrap();
    ops.create(
        &id,
        &file("n4", "delta.md", None, 7_000),
        Some("mentions topic"),
    )
    .await
    .unwrap();
    ops.create(
        &id,
        &file("n5", "zeta.md", None, 2_000),
        Some("mentions topic"),
    )
    .await
    .unwrap();
    ops.create(
        &id,
        &file("n6", "epsilon.md", None, 2_000),
        Some("mentions topic"),
    )
    .await
    .unwrap();

    let outcome = search(&ops, &id, "topic").await;
    assert_eq!(
        paths(&outcome),
        vec![
            // Name matches, freshest first.
            "beta topic.md",
            "alpha topic.md",
            // Then content matches, freshest first…
            "delta.md",
            "gamma.md",
            // …and equal revisions broken by path, not by store order.
            "epsilon.md",
            "zeta.md",
        ]
    );
}

/// `total` counts every match; `hits` carries only the page. Truncation has to
/// be visible or a caller cannot tell "all of them" from "the first few".
#[tokio::test]
async fn the_limit_truncates_the_page_and_total_stays_honest() {
    let (_dir, ops) = store();
    let id = CompanyId::new("acme");
    for i in 0..12 {
        ops.create(
            &id,
            &file(&format!("n{i}"), &format!("topic {i:02}.md"), None, 1_000),
            Some("body"),
        )
        .await
        .unwrap();
    }

    let outcome = search_workspace(ops.as_ref(), &id, "topic", None, limit(5))
        .await
        .unwrap();
    assert_eq!(outcome.hits.len(), 5);
    assert_eq!(outcome.total, 12);
    assert_eq!(outcome.omitted(), 7);
}

/// The hard ceiling: an over-large limit is clamped rather than honoured, so
/// "unlimited" is unreachable through the argument. `total` still tells the
/// truth about how much was left behind.
#[tokio::test]
async fn an_over_large_limit_is_clamped_to_the_maximum() {
    assert_eq!(clamp_limit(limit(1)), 1);
    assert_eq!(clamp_limit(limit(MAX_SEARCH_RESULTS)), MAX_SEARCH_RESULTS);
    assert_eq!(clamp_limit(limit(usize::MAX)), MAX_SEARCH_RESULTS);

    let (_dir, ops) = store();
    let id = CompanyId::new("acme");
    for i in 0..(MAX_SEARCH_RESULTS + 7) {
        ops.create(
            &id,
            &file(&format!("n{i}"), &format!("topic {i:03}.md"), None, 1_000),
            Some("body"),
        )
        .await
        .unwrap();
    }
    let outcome = search_workspace(ops.as_ref(), &id, "topic", None, limit(10_000))
        .await
        .unwrap();
    assert_eq!(outcome.hits.len(), MAX_SEARCH_RESULTS);
    assert_eq!(outcome.total, MAX_SEARCH_RESULTS + 7);
}

// -- scoping and rejection --------------------------------------------------

#[tokio::test]
async fn a_prefix_scopes_the_search_to_one_subtree() {
    let (_dir, ops, id) = seeded().await;
    // "standards" matches the folder, the note under it, and nothing at the root.
    let scoped = search_workspace(ops.as_ref(), &id, "e", Some("standards"), limit(20))
        .await
        .unwrap();
    assert!(
        scoped.hits.iter().all(|h| h.path.starts_with("standards")),
        "{:?}",
        paths(&scoped)
    );
    assert!(
        !paths(&scoped).contains(&"readme.md"),
        "the root note is outside the scope: {:?}",
        paths(&scoped)
    );

    // A prefix naming nothing yields nothing rather than falling back to the
    // whole tree.
    let empty = search_workspace(ops.as_ref(), &id, "e", Some("Nowhere"), limit(20))
        .await
        .unwrap();
    assert!(empty.hits.is_empty());

    // A prefix must not match a *sibling* by string prefix: `Stand` is not a
    // folder, and `Standards` must not be swept in by it.
    let sibling = search_workspace(ops.as_ref(), &id, "e", Some("Stand"), limit(20))
        .await
        .unwrap();
    assert!(sibling.hits.is_empty(), "{:?}", paths(&sibling));
}

#[tokio::test]
async fn a_traversal_shaped_prefix_is_refused() {
    let (_dir, ops, id) = seeded().await;
    for prefix in ["../etc", "standards/../..", "C:\\Windows", "."] {
        let err = search_workspace(ops.as_ref(), &id, "e", Some(prefix), limit(20))
            .await
            .expect_err("must refuse {prefix}");
        assert!(
            matches!(err, OpenCompanyError::InvalidRequest(_)),
            "{prefix}: {err:?}"
        );
    }
}

/// An empty query is refused rather than treated as "everything" — answering it
/// would turn a mistyped search into a full tree dump, which is the crawl this
/// module replaces.
#[tokio::test]
async fn an_empty_or_oversized_query_is_refused() {
    let (_dir, ops, id) = seeded().await;
    for query in ["", "   ", "\n\t "] {
        let err = search_workspace(ops.as_ref(), &id, query, None, limit(20))
            .await
            .expect_err("empty query must be refused");
        assert!(
            matches!(err, OpenCompanyError::InvalidRequest(_)),
            "{err:?}"
        );
    }

    let long = "a".repeat(MAX_QUERY_BYTES + 1);
    let err = search_workspace(ops.as_ref(), &id, &long, None, limit(20))
        .await
        .expect_err("an oversized query must be refused");
    assert!(
        matches!(err, OpenCompanyError::InvalidRequest(_)),
        "{err:?}"
    );

    // Exactly at the cap is accepted — the bound is inclusive, and a test that
    // only checks the refusal cannot tell an off-by-one from a working gate.
    search_workspace(
        ops.as_ref(),
        &id,
        &"a".repeat(MAX_QUERY_BYTES),
        None,
        limit(20),
    )
    .await
    .expect("a query at the cap is allowed");
}

/// A store that answers from a fixed node list, for the nodes no real backend
/// will create for us.
///
/// `FsOps` refuses a dangling parent (`parent folder does not exist`) and a
/// separator-bearing name, but the sqlite and mongodb backends validate neither
/// on create — so the only way to reach the unaddressable case at all is to hand
/// the helper the tree directly. The tools' own test double exists for the same
/// reason.
struct FixedTree(Vec<(WorkspaceNode, String)>);

#[async_trait::async_trait]
impl WorkspaceStore for FixedTree {
    async fn tree(&self, _company: &CompanyId) -> crate::Result<Vec<WorkspaceNode>> {
        Ok(self.0.iter().map(|(node, _)| node.clone()).collect())
    }
    async fn read(
        &self,
        _company: &CompanyId,
        id: &str,
    ) -> crate::Result<Option<(WorkspaceNode, String)>> {
        Ok(self
            .0
            .iter()
            .find(|(node, _)| node.id == id)
            .map(|(node, body)| (node.clone(), body.clone())))
    }

    async fn read_capped(
        &self,
        company: &CompanyId,
        id: &str,
        max_bytes: u64,
    ) -> crate::Result<Option<(WorkspaceNode, String, u64)>> {
        crate::ports::workspace::read_capped_by_reading(self, company, id, max_bytes).await
    }
    async fn write(
        &self,
        _company: &CompanyId,
        _id: &str,
        _content: &str,
        _author: WorkspaceOrigin,
    ) -> crate::Result<WorkspaceNode> {
        unreachable!("search never writes")
    }
    async fn create(
        &self,
        _company: &CompanyId,
        _node: &WorkspaceNode,
        _content: Option<&str>,
    ) -> crate::Result<()> {
        unreachable!("search never creates")
    }
    async fn adopt_or_create_folder(
        &self,
        _company: &CompanyId,
        _parent: Option<&str>,
        _name: &str,
        _origin: WorkspaceOrigin,
    ) -> crate::Result<crate::ports::workspace::FolderClaim> {
        unreachable!("search never claims a folder")
    }
    async fn create_binary(
        &self,
        _company: &CompanyId,
        _node: &WorkspaceNode,
        _bytes: &[u8],
    ) -> crate::Result<WorkspaceNode> {
        unreachable!("search never creates")
    }
    async fn write_binary(
        &self,
        _company: &CompanyId,
        _id: &str,
        _bytes: &[u8],
        _mime: Option<&str>,
        _author: WorkspaceOrigin,
    ) -> crate::Result<WorkspaceNode> {
        unreachable!("search never writes")
    }
    async fn read_bytes(
        &self,
        _company: &CompanyId,
        _id: &str,
    ) -> crate::Result<Option<(WorkspaceNode, crate::ports::workspace::BlobStream)>> {
        panic!("search must never read a payload — see the binary-node rule")
    }
    async fn rename_move(
        &self,
        _company: &CompanyId,
        _id: &str,
        _name: Option<&str>,
        _parent: Option<Option<&str>>,
    ) -> crate::Result<WorkspaceNode> {
        unreachable!("search never renames")
    }
    async fn swap_files(
        &self,
        _company: &CompanyId,
        _expected_id: Option<&str>,
        _replacement_id: &str,
        _name: &str,
    ) -> crate::Result<Option<WorkspaceNode>> {
        unreachable!("search never swaps files")
    }
    async fn delete(&self, _company: &CompanyId, _id: &str) -> crate::Result<bool> {
        unreachable!("search never deletes")
    }
    async fn is_empty(&self, _company: &CompanyId) -> crate::Result<bool> {
        Ok(self.0.is_empty())
    }
}

/// A node with no renderable path is absent from the agent tools' index by path
/// *and* by id, so `workspace_read` cannot open it. Search must not advertise
/// it — a result nothing can open is worse than no result.
///
/// Both shapes the tools exclude are covered: a dangling ancestor chain and a
/// name that is not a legal path segment.
#[tokio::test]
async fn an_unaddressable_node_is_never_a_hit() {
    let id = CompanyId::new("acme");
    let store: Arc<dyn WorkspaceStore> = Arc::new(FixedTree(vec![
        (
            file("n-orphan", "orphan topic.md", Some("missing"), 1_000),
            "mentions topic".to_string(),
        ),
        (
            file("n-slash", "a/b topic.md", None, 1_000),
            "mentions topic".to_string(),
        ),
        (
            file("n-ok", "visible topic.md", None, 1_000),
            "mentions topic".to_string(),
        ),
    ]));

    let outcome = search(&store, &id, "topic").await;
    assert_eq!(paths(&outcome), vec!["visible topic.md"]);
    assert_eq!(
        outcome.total, 1,
        "an unaddressable node must not be counted either"
    );
}
