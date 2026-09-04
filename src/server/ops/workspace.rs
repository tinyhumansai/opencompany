//! Workspace reads + writes: list the tree, read one file with its backlinks,
//! create a node, overwrite a file, rename/move a node, and delete (folders
//! recursive) — under both scope forms.
//!
//! Bodies mirror the console's `FsNode` (`frontend/src/api/workspace.ts`).
//! Writes land in the [`WorkspaceStore`](crate::ports::WorkspaceStore); node
//! ids are stable ULIDs so a rename/move never breaks a reference.
//!
//! ```text
//! GET    …/workspace                  the whole tree (metadata; no bodies)
//! GET    …/workspace/file/{nodeId}    one file: content + inbound backlinks
//! GET    …/workspace/blob/{nodeId}    one binary node's payload, streamed
//! GET    …/workspace/search?q=…       which notes mention a phrase
//! POST   …/workspace                  create a folder/file (JSON body)
//! POST   …/workspace/upload           upload a file of any kind (multipart)
//! POST   …/workspace/sweep-empty-agent-folders?dry_run=  tidy `agents/` strays
//! POST   …/workspace/merge-duplicate-folders?dry_run=    repair a raced tree
//! PUT    …/workspace/file/{nodeId}    overwrite file content
//! PATCH  …/workspace/{nodeId}         rename / move
//! DELETE …/workspace/{nodeId}         delete a node (folders recursive)
//! ```
//!
//! ## Why the two `GET`s are REST and not GraphQL
//!
//! Every other console read goes through GraphQL, and `Company.workspaceTree` /
//! `workspaceFile` have existed since the read plane landed — but the operator
//! console ships **no GraphQL client**. Reaching them from the Workspace tab
//! would mean a second wire protocol, a second auth path, a second error
//! envelope and ISO-8601 string timestamps in a view whose siblings all use
//! epoch millis. These twins keep the console on one client; the backlink scan
//! itself is shared with the resolver
//! ([`file_with_backlinks`](crate::company::workspace_links::file_with_backlinks))
//! so the two surfaces cannot answer differently.
//!
//! ## Known limits (issue #177 — documented, not worked around)
//!
//! * **No live push.** A write that lands while the tab is open is only visible
//!   on a refetch (refresh button / window focus). Tracked by issue #327.
//! * **No CAS on the console write path.** Agent writes require an
//!   `expected_updated_at` compare-and-swap token; the console `PUT` does not,
//!   so a concurrent agent write can be overwritten by the operator's save. That
//!   is the store's stated design — the operator is the dominant editor, and the
//!   agent's *next* CAS write fails and re-reads, so the agent side self-heals.
//!
//! ## Text and bytes are different resources (issue #553)
//!
//! A node holds prose or it holds bytes, never both, and the two are read
//! through different routes: `…/file/{id}` answers with text and backlinks,
//! `…/blob/{id}` streams a payload. Asking the wrong one is an error that names
//! the right one rather than an empty body — the port's honest answer to a
//! prose-shaped read of a payload is `""`, which as an HTTP response would
//! render as a blank editor over a file that is not blank.

use std::num::NonZeroUsize;

use axum::body::Body;
use axum::extract::multipart::MultipartError;
use axum::extract::{DefaultBodyLimit, Multipart, Path, Query};
use axum::http::{StatusCode, header};
use axum::response::Response;
use axum::routing::{get, patch, post, put};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};

use crate::AppState;
use crate::company::artifact_mirror::{MirrorOutcome, mirror_node_edit};
use crate::company::workspace_links::file_with_backlinks;
use crate::company::workspace_names::{MAX_NAME_BYTES, kebab_name_or};
use crate::company::workspace_repair::{
    MergedFolder, RepairPlan, Residual, merge_duplicate_folders as merge_workspace_duplicates,
};
use crate::company::workspace_search::{
    DEFAULT_SEARCH_LIMIT, MAX_SEARCH_RESULTS, search_workspace,
};
use crate::company::workspace_sweep::{
    SweptFolder, sweep_empty_agent_folders as sweep_workspace_agent_folders,
};
use crate::error::OpenCompanyError;
use crate::ports::artifacts::ArtifactAuthor;
use crate::ports::generate_id;
use crate::ports::workspace::{NodeKind, WorkspaceNode, WorkspaceOrigin};
use crate::runtime::UPLOAD_BODY_LIMIT_BYTES;
use crate::runtime::workspace_quota::human;
use crate::server::error::ApiError;
use crate::server::ops::artifacts::OPERATOR_EDIT_NOTE;
use crate::server::ops::{ScopedCompany, scoped};

/// Builds the workspace route fragment.
pub fn router() -> Router<AppState> {
    scoped("/workspace", post(create_node).get(list_tree))
        .merge(scoped(
            "/workspace/file/{node_id}",
            put(write_file).get(read_file),
        ))
        // A static sibling of `/workspace/{node_id}` below, which is exactly
        // what `/workspace/upload` already is — axum's router prefers a literal
        // segment over a parameter, so `search` is never captured as a node id.
        .merge(scoped("/workspace/search", get(search)))
        // Another static sibling of `/workspace/{node_id}`, for the same reason
        // `search` is one (issue #700).
        .merge(scoped(
            "/workspace/sweep-empty-agent-folders",
            post(sweep_empty_agent_folders),
        ))
        // And another (issue #759) — the repair for a tree a publish race has
        // already left ambiguous.
        .merge(scoped(
            "/workspace/merge-duplicate-folders",
            post(merge_duplicate_folders),
        ))
        .merge(scoped("/workspace/blob/{node_id}", get(read_blob)))
        .merge(
            scoped("/workspace/upload", post(upload))
                // On this route only. The default limit is a couple of
                // megabytes, which every upload this route exists for would
                // exceed; raising it globally would lift it for every JSON
                // handler in the process, which is the opposite of what an
                // upload endpoint should cost its neighbours.
                //
                // Two limits, in this order (issue #647):
                //
                // 1. The **store's** per-file cap decides policy. It sees the
                //    whole payload, so it refuses by name and size —
                //    "`hero.mov` is 91.4 MiB, over the 64.0 MiB limit for a
                //    single file" — and it honours the company's configured
                //    `[workspace] max_blob_mb`, which this layer cannot: routers
                //    are built once, before any company exists.
                // 2. This layer is the **backstop**, four times the default cap.
                //    It exists so an unbounded body cannot be buffered, not to
                //    express policy, and the headroom is what lets the store
                //    speak first for every realistic over-cap upload.
                //
                // Both answer 413. Setting this *at* the cap — what it used to
                // be — made (1) unreachable: the body limit fired mid-parse, and
                // a body that stops mid-part is indistinguishable from a
                // malformed one, so the honest 413 came out as `400 malformed
                // multipart`. `upload` below still has to classify, because this
                // layer's own failure arrives through the same parse error.
                //
                // The trade: this is also how much one in-flight upload may hold
                // in memory, because the write path buffers by design.
                .layer(DefaultBodyLimit::max(
                    crate::runtime::UPLOAD_BODY_LIMIT_BYTES as usize,
                )),
        )
        .merge(scoped(
            "/workspace/{node_id}",
            patch(rename_move).delete(delete_node),
        ))
        // Chat attachments (issue #1682) enter on their own route but land in
        // the same blob store as a workspace upload, so the handler lives here
        // beside `upload` to share `resolve_mime`, the filename sanitizer,
        // `admit_upload` and `create_binary` verbatim. It carries the identical
        // body-limit layer, for the identical reason — an unbounded multipart
        // body must not be buffered, and the store speaks first on policy.
        .merge(
            scoped("/chat/upload", post(chat_upload)).layer(DefaultBodyLimit::max(
                crate::runtime::UPLOAD_BODY_LIMIT_BYTES as usize,
            )),
        )
}

/// A workspace node as the console renders it.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct FsNode {
    id: String,
    name: String,
    kind: NodeKind,
    #[serde(skip_serializing_if = "Option::is_none")]
    parent_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    content: Option<String>,
    updated_at: u64,
    /// Who created the node, and who last wrote its body (issue #326).
    ///
    /// Always serialized, unlike `parentId` / `content` above: the console
    /// renders an authorship badge off these, and an absent field there would
    /// be indistinguishable from "unknown" when the honest answer is
    /// `operator`. The port already defaults a legacy node to `operator`, so
    /// this is never null.
    created_by: WorkspaceOrigin,
    updated_by: WorkspaceOrigin,
    /// Set only on a **binary** node (issue #553), and omitted entirely
    /// otherwise — so `mime` being present is the console's test for "render or
    /// download this instead of editing it", with no present-but-null case to
    /// disambiguate. The tree read happens on every mount, so three nulls per
    /// prose note is a cost worth not paying.
    #[serde(skip_serializing_if = "Option::is_none")]
    mime: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    size: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    sha256: Option<String>,
}

impl FsNode {
    fn from_node(node: WorkspaceNode, content: Option<String>) -> Self {
        Self {
            id: node.id,
            name: node.name,
            kind: node.kind,
            parent_id: node.parent_id,
            content,
            updated_at: node.updated_at_millis,
            created_by: node.created_by,
            updated_by: node.updated_by,
            mime: node.mime,
            size: node.size,
            sha256: node.sha256,
        }
    }
}

/// One workspace file with its body and the notes that link to it.
///
/// The REST twin of the GraphQL `WorkspaceFile`, differing only in timestamp
/// shape (epoch millis, like every other console read) — the backlinks come
/// from the same shared scan.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct WorkspaceFileBody {
    id: String,
    name: String,
    content: String,
    updated_at: u64,
    /// Who created this note, and who last wrote the body above (issue #326).
    created_by: WorkspaceOrigin,
    updated_by: WorkspaceOrigin,
    /// Other files whose content links to this one via `[[name]]`.
    backlinks: Vec<FsNode>,
}

/// One search hit as the console renders it (issue #607).
///
/// Carries the whole node — so the console can badge origin and mark a binary
/// exactly as it does in the tree — plus the two things only a search knows:
/// where the node sits (`path`, which the tree view derives from `parentId` but
/// a flat hit list cannot) and why it matched.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct SearchHitBody {
    #[serde(flatten)]
    node: FsNode,
    /// The node's logical path, e.g. `standards/Engineering.md`.
    path: String,
    /// `name` or `content`.
    matched: &'static str,
    /// Text around the first body match. Absent for a name match, a folder, and
    /// a binary node — a payload is never excerpted.
    #[serde(skip_serializing_if = "Option::is_none")]
    excerpt: Option<String>,
}

/// The search response: the page, and how many matched in total.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct SearchResults {
    hits: Vec<SearchHitBody>,
    /// Matches before the limit was applied — so the console can say "20 of 137"
    /// rather than implying it is showing everything.
    total: usize,
}

/// The search query string.
#[derive(Debug, Deserialize)]
struct SearchQuery {
    /// The text to look for. Required; an empty one is a 400.
    #[serde(default)]
    q: Option<String>,
    /// Optional subtree scope, by logical path.
    #[serde(default)]
    prefix: Option<String>,
    /// Optional page size. Absent means the default; `0` is a 400 rather than a
    /// silent "everything".
    #[serde(default)]
    limit: Option<usize>,
}

/// Whether a maintenance pass is a preview or the real thing (issues #700,
/// #759).
///
/// Shared by both passes rather than duplicated: they make the same promise to
/// the console — preview, name everything, then confirm — and a second copy of
/// this would be a second chance for one of them to default the other way.
#[derive(Debug, Deserialize)]
struct PreviewQuery {
    /// `true` names what *would* happen and changes nothing. Absent means a real
    /// run: this is a `POST`, and a caller that asked for one without saying
    /// "preview" asked for the change.
    #[serde(default)]
    dry_run: bool,
}

/// What the sweep did, or would do.
///
/// Exactly one of the two lists is present, and which one says what actually
/// happened — a preview answers `wouldRemove`, a real run answers `removed`.
/// That is deliberate rather than tidy: a console reading the field it asked for
/// cannot mistake a preview for a deletion (or the reverse) if the host and the
/// request ever disagree about `dry_run`.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct SweepResult {
    /// The candidates, on a dry run.
    #[serde(skip_serializing_if = "Option::is_none")]
    would_remove: Option<Vec<SweptFolder>>,
    /// What was actually deleted, on a real run.
    #[serde(skip_serializing_if = "Option::is_none")]
    removed: Option<Vec<SweptFolder>>,
}

/// What the duplicate-folder repair did, or would do (issue #759).
///
/// The same "exactly one list, and which one says what happened" discipline the
/// sweep above uses: a preview answers `wouldMerge`, a real run answers
/// `merged`, so a console reading the field it asked for cannot mistake one for
/// the other.
///
/// `residuals` is present either way, and always — including as an empty list.
/// It is the half of the answer that says whether the tree is *actually* fixed:
/// a repair that merged three folders and quietly left two rival documents on
/// one path has not finished, and the operator is the only one who can.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct RepairResult {
    /// The folds, on a dry run.
    #[serde(skip_serializing_if = "Option::is_none")]
    would_merge: Option<Vec<MergedFolder>>,
    /// What was actually folded away, on a real run.
    #[serde(skip_serializing_if = "Option::is_none")]
    merged: Option<Vec<MergedFolder>>,
    /// What the repair refused to decide, and why.
    residuals: Vec<Residual>,
}

impl RepairResult {
    fn new(plan: RepairPlan, dry_run: bool) -> Self {
        let RepairPlan { folders, residuals } = plan;
        let (would_merge, merged) = if dry_run {
            (Some(folders), None)
        } else {
            (None, Some(folders))
        };
        Self {
            would_merge,
            merged,
            residuals,
        }
    }
}

/// The create-node body.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CreateNode {
    name: String,
    kind: NodeKind,
    #[serde(default)]
    parent_id: Option<String>,
    #[serde(default)]
    content: Option<String>,
}

/// The overwrite-file body.
#[derive(Debug, Deserialize)]
struct WriteFile {
    content: String,
}

/// The rename/move body.
///
/// `parent_id` uses a double option so an omitted `parentId` (leave the parent
/// unchanged) is distinguished from an explicit `"parentId": null` (move the
/// node back to the workspace root).
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RenameMove {
    #[serde(default)]
    name: Option<String>,
    #[serde(default, deserialize_with = "double_option")]
    parent_id: Option<Option<String>>,
}

/// Deserializes into `Some(inner)` when the field is present (so an explicit
/// `null` becomes `Some(None)`); the `#[serde(default)]` leaves an omitted field
/// as `None`.
fn double_option<'de, D, T>(deserializer: D) -> Result<Option<Option<T>>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::deserialize(deserializer).map(Some)
}

/// The overwrite-file response.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct WriteAck {
    updated_at: u64,
}

/// The sub-resource path (`node_id`).
#[derive(Debug, Deserialize)]
struct NodePath {
    node_id: String,
}

/// `GET …/workspace` — every node in the tree, metadata only.
///
/// Bodies are deliberately omitted: a tree read is the console's *navigation*
/// call and happens on every mount, focus and refresh, so shipping every note's
/// content would make it grow without bound with the workspace. The console
/// fetches a body when a note is opened ([`read_file`]) — the same
/// index-then-fetch split the agent-facing `workspace_list` / `workspace_read`
/// tools make.
async fn list_tree(company: ScopedCompany) -> Result<Json<Vec<FsNode>>, ApiError> {
    let nodes = company.runtime.workspace().tree(company.id()).await?;
    Ok(Json(
        nodes
            .into_iter()
            .map(|node| FsNode::from_node(node, None))
            .collect(),
    ))
}

/// `GET …/workspace/file/{node_id}` — one file's content plus the notes that
/// link to it.
///
/// A folder id 404s rather than answering with an empty body: the console only
/// ever opens files, so a folder id here is a caller bug, and reporting it as an
/// empty note would hide it.
async fn read_file(
    company: ScopedCompany,
    Path(NodePath { node_id }): Path<NodePath>,
) -> Result<Json<WorkspaceFileBody>, ApiError> {
    let found =
        file_with_backlinks(company.runtime.workspace().as_ref(), company.id(), &node_id).await?;
    let Some((node, content, backlinks)) = found.filter(|(node, _, _)| node.kind == NodeKind::File)
    else {
        return Err(ApiError(OpenCompanyError::CompanyNotFound(format!(
            "workspace file {node_id}"
        ))));
    };
    // A binary node would otherwise answer here with an empty body — the port's
    // honest answer to a prose-shaped read, and a confusing one to receive: the
    // console would render a blank editor over a file that is not blank. Naming
    // the route that does serve it turns a silent wrong answer into a directed
    // one (issue #553).
    if let Some(mime) = &node.mime {
        return Err(ApiError(OpenCompanyError::InvalidRequest(format!(
            "`{}` holds {mime} data, not text; fetch it from \
             `…/workspace/blob/{node_id}` instead",
            node.name
        ))));
    }
    Ok(Json(WorkspaceFileBody {
        id: node.id,
        name: node.name,
        content,
        updated_at: node.updated_at_millis,
        created_by: node.created_by,
        updated_by: node.updated_by,
        backlinks: backlinks
            .into_iter()
            .map(|node| FsNode::from_node(node, None))
            .collect(),
    }))
}

/// `GET …/workspace/search?q=…` — which notes mention a phrase (issue #607).
///
/// A thin call into
/// [`search_workspace`](crate::company::workspace_search::search_workspace),
/// the same helper behind the GraphQL `workspaceSearch` resolver and the agent
/// `workspace_search` tool. That is the point rather than a convenience: three
/// surfaces answering "which notes mention X" with three scans would drift, and
/// the drift would be invisible from whichever one the reader was not looking
/// at — the same argument that put the backlink scan in
/// [`workspace_links`](crate::company::workspace_links).
///
/// `q` is required and an empty one is a 400: an empty query is not "everything"
/// (that is the tree read on this same prefix), and answering it as such would
/// turn a cleared search box into a full-tree fetch on every keystroke.
async fn search(
    company: ScopedCompany,
    Query(query): Query<SearchQuery>,
) -> Result<Json<SearchResults>, ApiError> {
    let q = query.q.unwrap_or_default();
    // Stated, never silently unlimited. `limit=0` is a caller meaning something
    // specific, and both available guesses — "the default" and "no limit" — are
    // wrong.
    let limit = match query.limit {
        None => NonZeroUsize::new(DEFAULT_SEARCH_LIMIT).expect("the default limit is non-zero"),
        Some(n) => NonZeroUsize::new(n).ok_or_else(|| {
            ApiError(OpenCompanyError::InvalidRequest(format!(
                "`limit` is 0, which would return no matches; omit it for the default of \
                 {DEFAULT_SEARCH_LIMIT}, or pass a value between 1 and {MAX_SEARCH_RESULTS}"
            )))
        })?,
    };

    let outcome = search_workspace(
        company.runtime.workspace().as_ref(),
        company.id(),
        &q,
        query.prefix.as_deref(),
        limit,
    )
    .await?;

    Ok(Json(SearchResults {
        total: outcome.total,
        hits: outcome
            .hits
            .into_iter()
            .map(|hit| SearchHitBody {
                node: FsNode::from_node(hit.node, None),
                path: hit.path,
                matched: hit.matched.as_str(),
                excerpt: hit.excerpt,
            })
            .collect(),
    }))
}

/// `GET …/workspace/blob/{node_id}` — download a file's payload.
///
/// The counterpart of [`read_file`], and the only way a file leaves the tree as
/// a download. A binary node's body is streamed rather than buffered, so serving
/// a 200 MiB video costs the process a chunk at a time; a prose note is served
/// from its body, under the same neutralised headers.
///
/// A folder and an id that names nothing 404 identically: telling them apart
/// would leak which node ids exist to a caller that cannot read them anyway.
///
/// `ETag` is the payload's sha256 — the digest the store computed from the
/// bytes it holds, so a conditional request is answered by the thing itself
/// rather than by a timestamp that a rename would move. A prose note carries no
/// digest, so it is served without one.
///
/// # A stored `mime` is a caller's claim, so it does not decide the disposition
///
/// `node.mime` comes from the upload's declared `Content-Type` (or from
/// `mime_guess` on a published deliverable's filename), which means it is
/// influenced by whoever produced the file rather than derived from the bytes.
/// Serving that value back with `Content-Disposition: inline` handed an
/// uploader the ability to run script on the console's own origin, with the
/// operator's `SameSite=Lax` session cookie attached — a top-level navigation
/// sends it (issue #667).
///
/// So the mime no longer selects the disposition; [`serving_for`] does, from a
/// closed list. Nothing about *storage* changed, which is the point: a payload
/// already sitting in the tree under a mime some caller chose is neutralised the
/// next time it is served, rather than only on uploads that happen after this.
async fn read_blob(
    company: ScopedCompany,
    Path(NodePath { node_id }): Path<NodePath>,
) -> Result<Response, ApiError> {
    let missing = || {
        ApiError(OpenCompanyError::CompanyNotFound(format!(
            "workspace blob {node_id}"
        )))
    };
    let (node, stream, size) = match company
        .runtime
        .workspace()
        .read_bytes(company.id(), &node_id)
        .await?
    {
        Some((node, stream)) => {
            let size = node.size;
            (node, stream, size)
        }
        None => {
            let Some((node, content)) = company
                .runtime
                .workspace()
                .read(company.id(), &node_id)
                .await?
            else {
                return Err(missing());
            };
            if node.kind != NodeKind::File || node.is_binary() {
                return Err(missing());
            }
            let bytes = content.into_bytes();
            let size = Some(bytes.len() as u64);
            (node, crate::ports::workspace::one_chunk(bytes), size)
        }
    };
    let serving = serving_for(node.mime.as_deref());
    let mut response = Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, serving.content_type)
        // Without this, a browser is free to disregard the type above and sniff
        // the bytes — which would make forcing `application/octet-stream` a
        // suggestion rather than a decision.
        .header(header::X_CONTENT_TYPE_OPTIONS, "nosniff")
        // Quoted per RFC 9110, and escaped defensively: a node name never
        // reaches this header, but a digest that somehow did would break the
        // response rather than the parse.
        .header(
            header::CONTENT_DISPOSITION,
            format!(
                "{}; filename=\"{}\"",
                serving.disposition,
                node.name.replace('"', "").replace(['\r', '\n'], "")
            ),
        );
    if let Some(sha) = node.sha256 {
        response = response.header(header::ETAG, format!("\"{sha}\""));
    }
    if let Some(size) = size {
        response = response.header(header::CONTENT_LENGTH, size);
    }
    response.body(Body::from_stream(stream)).map_err(|e| {
        ApiError(OpenCompanyError::Store(format!(
            "blob response failed: {e}"
        )))
    })
}

/// The media types a browser may render as a **document** on this origin.
///
/// Every entry is a format a browser decodes into pixels or into a viewer of its
/// own, not one it parses into a DOM with a script context. `image/*` as a
/// prefix rule would be the obvious way to write this and would be wrong:
/// `image/svg+xml` is an XML *document* format whose `<script>` executes, so it
/// is deliberately absent here and handled below instead.
///
/// `application/pdf` is included because a browser renders a PDF in a viewer
/// that has no reach into the embedding origin's DOM or cookies, and because
/// "open the URL, see the file" is the whole reason the route serves anything
/// inline. Dropping it would be the more conservative choice — it would cost
/// only direct navigation, since the console has never previewed a PDF — and is
/// a one-line change if the tradeoff is ever judged differently.
const INLINE_RENDERABLE: &[&str] = &[
    "image/apng",
    "image/avif",
    "image/bmp",
    "image/gif",
    "image/jpeg",
    "image/png",
    "image/tiff",
    "image/vnd.microsoft.icon",
    "image/webp",
    "image/x-icon",
    "application/pdf",
];

/// Types the console may preview but no browser may open as a document.
///
/// SVG is both at once. In an `<img>` element it renders in the SVG spec's
/// *secure static mode* — no script, no external references — which is why the
/// console's image preview of one is safe, and why the type has to survive this
/// far to reach it: an `<img>` will not decode SVG bytes without
/// `image/svg+xml`. At the top of a tab the same bytes are a full document on
/// this origin. Keeping the type and forcing `attachment` separates the two,
/// where a single allow-list would have had to sacrifice one for the other.
const PREVIEW_ONLY: &[&str] = &["image/svg+xml"];

/// How [`read_blob`] will serve one stored payload.
struct BlobServing {
    content_type: String,
    disposition: &'static str,
}

/// Decides a payload's response type and disposition from its stored mime.
///
/// Three outcomes, and the default is the safe one: a type nobody vouched for
/// is served as opaque bytes the browser is told to download. That is what makes
/// this a closed list rather than a blocklist — a media type invented after this
/// was written lands in the last arm, not in the first.
fn serving_for(stored: Option<&str>) -> BlobServing {
    // The essence only: `image/png; charset=binary` is `image/png`, and a
    // comparison against the raw value would miss it and downgrade a legitimate
    // image to a download.
    let essence = stored
        .map(|m| m.split(';').next().unwrap_or(m).trim().to_ascii_lowercase())
        .unwrap_or_default();

    if INLINE_RENDERABLE.contains(&essence.as_str()) {
        BlobServing {
            content_type: essence,
            disposition: "inline",
        }
    } else if PREVIEW_ONLY.contains(&essence.as_str()) {
        BlobServing {
            content_type: essence,
            disposition: "attachment",
        }
    } else {
        BlobServing {
            content_type: "application/octet-stream".to_string(),
            disposition: "attachment",
        }
    }
}

/// Names a multipart failure for what it actually was (issue #647).
///
/// One error type carries two unrelated causes. A body that ran past the
/// route's `DefaultBodyLimit` stops mid-part, and to the multipart reader that
/// looks exactly like a body that was malformed to begin with — so reporting
/// every failure here as `InvalidRequest` told an operator whose only mistake
/// was picking a large file that their request was broken. It is not: it is
/// oversized, which is a different sentence and a different fix.
///
/// axum keeps the two apart. `MultipartError::status()` answers 413 for the
/// size-limit variants — multer's own `FieldSizeExceeded` / `StreamSizeExceeded`
/// and the `StreamReadFailed` whose source is `http_body_util`'s
/// `LengthLimitError`, which is the `DefaultBodyLimit` case — and 400 for the
/// genuinely malformed ones. Reading that rather than matching on the message
/// keeps this from breaking when axum rewords an error.
///
/// The 413 is raised as [`OpenCompanyError::WorkspaceQuota`] on purpose: that
/// is already the store's over-cap refusal, so the two causes share a status
/// (413) and a stable code (`workspace_quota_exceeded`) rather than inventing a
/// second vocabulary for "too big". A caller keying on the code cannot tell —
/// and should not have to — which of the two limits noticed.
///
/// Only the limit is named, never a size. The body was cut off, so the true
/// total is not knowable here; guessing at it would be worse than omitting it.
fn multipart_error(error: MultipartError, context: &str) -> ApiError {
    if error.status() == StatusCode::PAYLOAD_TOO_LARGE {
        return ApiError(OpenCompanyError::WorkspaceQuota(format!(
            "this upload is larger than the {} this endpoint will read in one request, \
             so it was cut off before its size could be measured. Nothing was stored.",
            human(UPLOAD_BODY_LIMIT_BYTES),
        )));
    }
    ApiError(OpenCompanyError::InvalidRequest(format!(
        "{context}: {error}"
    )))
}

/// `POST …/workspace/upload` — multipart upload of a file of any kind.
///
/// The existing create route takes a JSON body, which cannot carry bytes; this
/// is the path an image, a PDF or a zip arrives on.
///
/// # Text still goes down the text path
///
/// A file that is *typed* as text and *is* valid UTF-8 is stored as a prose
/// note, not as a payload. That is not a size optimisation: a note is
/// diffable, backlinkable, searchable and editable in the console, and a
/// Markdown file uploaded as an opaque blob would silently lose all four.
/// Anything else — including a `.txt` that turns out to be binary — becomes a
/// binary node, because the decision is made on the bytes and not only on what
/// the caller claimed.
async fn upload(
    company: ScopedCompany,
    mut multipart: Multipart,
) -> Result<Json<FsNode>, ApiError> {
    let mut file: Option<(String, Option<String>, Vec<u8>)> = None;
    let mut parent_id: Option<String> = None;

    // Every one of these three is a place the body limit can be noticed — this
    // one while draining a part the route ignores, the two below while reading
    // one it wants — so all three classify rather than the one that happened to
    // be reported first.
    while let Some(field) = multipart
        .next_field()
        .await
        .map_err(|e| multipart_error(e, "malformed multipart upload"))?
    {
        match field.name() {
            Some("parentId") | Some("parent_id") => {
                let value = field
                    .text()
                    .await
                    .map_err(|e| multipart_error(e, "unreadable parentId"))?;
                // An empty value is the console saying "the workspace root",
                // which is what an absent field means too.
                if !value.trim().is_empty() {
                    parent_id = Some(value);
                }
            }
            Some("file") => {
                let name = field
                    .file_name()
                    .map(str::to_string)
                    .filter(|n| !n.trim().is_empty())
                    .ok_or_else(|| {
                        ApiError(OpenCompanyError::InvalidRequest(
                            "the uploaded file has no filename".to_string(),
                        ))
                    })?;
                let declared = field.content_type().map(str::to_string);
                let bytes = field
                    .bytes()
                    .await
                    .map_err(|e| multipart_error(e, "unreadable file part"))?;
                file = Some((name, declared, bytes.to_vec()));
            }
            // Ignored rather than rejected: a browser's `FormData` may carry
            // fields this route has no use for, and refusing the upload over
            // one would be a puzzle to debug from the console side.
            _ => {}
        }
    }

    let Some((name, declared, bytes)) = file else {
        return Err(ApiError(OpenCompanyError::InvalidRequest(
            "the upload carried no `file` part".to_string(),
        )));
    };
    // The last path segment only: a browser may send a full path as the
    // filename, and the store's own name check would reject it — better to
    // accept the upload under the obvious name than to fail on a detail the
    // operator did not choose.
    let name = name
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(&name)
        .trim()
        .to_string();
    // Named under the workspace rule before anything else looks at it, so the
    // mime, the size check and the stored node all speak about one name — and
    // an uploaded `Q3 Report.pdf` sits in the tree as `q3-report.pdf`, beside
    // everything else. The response carries the stored node, so the console
    // shows what it actually got rather than what was sent.
    let name = kebab_name_or(&name, &name);
    let mime = resolve_mime(&name, declared.as_deref());

    // Issue #665: a file uploaded as a file is bounded as a file, whatever its
    // encoding turns out to be. Asked **here, before the text/binary branch**
    // below, because that branch decides how the payload is *stored* and must
    // not also decide whether a limit applies.
    //
    // The store is asked rather than a limit applied here: a route cannot know a
    // company's configured cap — routers are built once, before any company
    // exists — so a check written inline would silently enforce the global
    // default against a company that raised or lowered its own.
    //
    // This is not the per-writer check `workspace_quota` argues against. That
    // argument is about writers *forgetting*; the decorator's narrowing rests on
    // "a note is bounded by what a model will emit into a tool call", which is
    // true of every writer it covers and false here — this route is where
    // arbitrary operator-supplied bytes enter the tree.
    company
        .runtime
        .workspace()
        .admit_upload(company.id(), &name, bytes.len() as u64)
        .await?;

    let mut node = WorkspaceNode {
        id: generate_id(),
        name,
        kind: NodeKind::File,
        parent_id,
        updated_at_millis: crate::ports::now_millis(),
        created_by: WorkspaceOrigin::Operator,
        updated_by: WorkspaceOrigin::Operator,
        mime: None,
        size: None,
        sha256: None,
        adopted: false,
    };

    match text_body(&mime, &bytes) {
        Some(text) => {
            company
                .runtime
                .workspace()
                .create(company.id(), &node, Some(&text))
                .await?;
            Ok(Json(FsNode::from_node(node, Some(text))))
        }
        None => {
            node.mime = Some(mime);
            company
                .runtime
                .workspace()
                .create_binary(company.id(), &node, &bytes)
                .await?;
            // Re-read so the response carries the size and digest the STORE
            // computed, rather than the `None`s this handler sent in. The
            // console shows both, and showing a value the store did not
            // produce is how a digest stops meaning anything.
            let stored = company
                .runtime
                .workspace()
                .tree(company.id())
                .await?
                .into_iter()
                .find(|n| n.id == node.id)
                .unwrap_or(node);
            Ok(Json(FsNode::from_node(stored, None)))
        }
    }
}

/// A stored chat attachment, as the composer needs it back (issue #1682).
///
/// The compact counterpart of [`FsNode`]: a chat attachment is not a tree node
/// the console edits, so the send path needs only the id to reference it and
/// the name / mime / size to draw a pending chip. Every field is the **store's**
/// — the id it generated, the name it stored under, the mime it resolved, the
/// length it measured — never the client's claim, which is the same discipline
/// the send route re-applies when it re-resolves the id.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct AttachmentRef {
    node_id: String,
    name: String,
    mime: String,
    size: u64,
}

/// `POST …/chat/upload` — multipart upload of one file to attach to a chat
/// message (issue #1682).
///
/// The byte-transfer half of a two-step send: the client uploads here, gets a
/// stable [`AttachmentRef`] back, and then posts the ordinary JSON `/chat`
/// message carrying that node id. Decoupling the two keeps the synchronous,
/// turn-running `/chat` POST off the bytes.
///
/// # Binary-only, unlike [`upload`]
///
/// The workspace `upload` stores a UTF-8 text file as an editable prose note,
/// because a Markdown file in the tree earns a diffable, backlinkable editor. A
/// chat attachment earns none of that — it is a file hung on a message, not a
/// document someone maintains — so this always stores bytes, whatever the
/// encoding. The download path is then 100% shared: the file is served by the
/// existing hardened `GET …/workspace/blob/{node_id}` (issue #667), so no new
/// serve is added and the `nosniff` + closed inline allow-list already cover it.
///
/// Everything that can refuse still refuses first, and in the same words:
/// `admit_upload` gates size and quota, the body-limit layer backstops an
/// unbounded body, and the last-segment filename sanitizer keeps a browser's
/// full path from reaching the store.
async fn chat_upload(
    company: ScopedCompany,
    mut multipart: Multipart,
) -> Result<Json<AttachmentRef>, ApiError> {
    let mut file: Option<(String, Option<String>, Vec<u8>)> = None;

    while let Some(field) = multipart
        .next_field()
        .await
        .map_err(|e| multipart_error(e, "malformed multipart upload"))?
    {
        // Only the `file` part is read; a browser's `FormData` may carry other
        // fields this route has no use for, and ignoring them keeps an upload
        // from failing over a stray part.
        if field.name() == Some("file") {
            let name = field
                .file_name()
                .map(str::to_string)
                .filter(|n| !n.trim().is_empty())
                .ok_or_else(|| {
                    ApiError(OpenCompanyError::InvalidRequest(
                        "the uploaded file has no filename".to_string(),
                    ))
                })?;
            let declared = field.content_type().map(str::to_string);
            let bytes = field
                .bytes()
                .await
                .map_err(|e| multipart_error(e, "unreadable file part"))?;
            file = Some((name, declared, bytes.to_vec()));
        }
    }

    let Some((name, declared, bytes)) = file else {
        return Err(ApiError(OpenCompanyError::InvalidRequest(
            "the upload carried no `file` part".to_string(),
        )));
    };
    // The last path segment only, then named under the workspace rule — the
    // same sanitizer `upload` applies, so a browser's full path never reaches
    // the store and a client string never reaches a filesystem path.
    let name = name
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(&name)
        .trim()
        .to_string();
    let name = kebab_name_or(&name, &name);
    let mime = resolve_mime(&name, declared.as_deref());

    // Size + quota, decided by the store so a company's configured cap is
    // honoured rather than the global default — identical to `upload`.
    company
        .runtime
        .workspace()
        .admit_upload(company.id(), &name, bytes.len() as u64)
        .await?;

    let id = generate_id();
    let mut node = WorkspaceNode {
        id: id.clone(),
        name: name.clone(),
        kind: NodeKind::File,
        parent_id: None,
        updated_at_millis: crate::ports::now_millis(),
        created_by: WorkspaceOrigin::Operator,
        updated_by: WorkspaceOrigin::Operator,
        // A chat attachment is bytes, never a note — so it is a binary node
        // whatever its encoding, and the text branch `upload` owns is skipped.
        mime: Some(mime.clone()),
        size: None,
        sha256: None,
        adopted: false,
    };
    // Chat uploads all land at the workspace root (`parent_id: None`), so a
    // second attachment reusing an earlier one's exact filename — the same
    // `image.png` picked in two different messages — collides on the sibling
    // uniqueness `create_binary` enforces. That used to surface as a 409 on
    // the second attach, silently losing it, since deleting the first (the
    // only way to free the name) would break its download too. On exactly
    // that conflict, retry once under a disambiguated name derived from this
    // upload's own freshly-minted id — guaranteed free — rather than fail the
    // attach. The stored name is what `resolve_attachments` later reads back
    // as the display name, so a repeat filename is honestly shown suffixed
    // rather than silently dropped.
    //
    // Matched in full — codex review finding — rather than `if let
    // Err(Conflict(_)) = ...`, which let every OTHER failure (a filesystem,
    // SQLite, or MongoDB write error) fall through as if the first attempt
    // had succeeded: the composer would show a node that was never stored,
    // and the later `/chat` POST would 400 resolving a reference that names
    // nothing.
    match company
        .runtime
        .workspace()
        .create_binary(company.id(), &node, &bytes)
        .await
    {
        Ok(_) => {}
        Err(OpenCompanyError::Conflict(_)) => {
            // On MongoDB the failed attempt is not a dry run: `create_binary`
            // uploads the blob before the node-document insert, so a
            // name-collision conflict has already written bytes under this id
            // (blob-first ordering, issue #894). The store's conflict path
            // reclaims that payload before returning, so only the name remains
            // contested. Retrying under the *same* id would upload a second
            // blob that matches a live node, which the orphan sweep can never
            // reclaim — so mint a fresh id (and a disambiguated name derived
            // from it) for the retry.
            node.id = generate_id();
            node.name = disambiguate_name(&name, &node.id);
            company
                .runtime
                .workspace()
                .create_binary(company.id(), &node, &bytes)
                .await?;
        }
        Err(other) => return Err(ApiError(other)),
    }

    Ok(Json(AttachmentRef {
        node_id: node.id,
        name: node.name,
        mime,
        // The stored length is exactly what was written — `create_binary`
        // measures the same bytes — so it needs no re-read to report.
        size: bytes.len() as u64,
    }))
}

/// Inserts a short disambiguator before `name`'s extension (or at its end,
/// with none), from the tail of `id` — already a fresh, collision-free ULID,
/// so no extra uniqueness check is needed against it.
///
/// `image.png` + id `...01j8` → `image-01j8.png`.
///
/// The result stays within the workspace-name budget ([`MAX_NAME_BYTES`]).
/// `name` arrives already canonical, so it may already fill the whole budget;
/// appending the tag would then exceed the cap, and the next normalization
/// would truncate the tag back off — mapping the disambiguated name onto the
/// very collision it exists to escape. The stem is trimmed to make room for
/// the tag and the extension; an extension that would alone eat the budget is
/// dropped (the same rule the sanitizer applies).
fn disambiguate_name(name: &str, id: &str) -> String {
    let tag = &id[id.len().saturating_sub(6)..];
    let (stem, extension) = match name.rsplit_once('.') {
        Some((stem, ext)) if !stem.is_empty() => (stem, Some(ext)),
        _ => (name, None),
    };
    let with_extension = extension.is_some_and(|ext| 8 + ext.len() <= MAX_NAME_BYTES); // `-<tag>.<ext>`
    let suffix = if with_extension {
        format!("-{tag}.{}", extension.unwrap())
    } else {
        format!("-{tag}")
    };
    let mut trimmed = &stem[..stem.len().min(MAX_NAME_BYTES.saturating_sub(suffix.len()))];
    // A cut that lands mid-run leaves a separator abutting the tag; trim it so
    // the result is still canonical kebab-case and `is_kebab_name` fixes it.
    while trimmed.ends_with('-') || trimmed.ends_with('.') {
        trimmed = &trimmed[..trimmed.len() - 1];
    }
    format!("{trimmed}{suffix}")
}

/// The media type to store a upload under.
///
/// The browser's declared type wins when it says anything specific; otherwise
/// the extension decides. `application/octet-stream` is what a browser sends
/// when it has no idea, so it is treated as no answer rather than as an answer.
fn resolve_mime(name: &str, declared: Option<&str>) -> String {
    let declared = declared
        .map(|d| d.split(';').next().unwrap_or(d).trim().to_lowercase())
        .filter(|d| !d.is_empty() && d != "application/octet-stream");
    declared.unwrap_or_else(|| {
        mime_guess::from_path(name)
            .first_raw()
            .unwrap_or("application/octet-stream")
            .to_string()
    })
}

/// The upload's text body, when it should be stored as a prose note.
///
/// Both halves must hold: the type says text **and** the bytes decode as UTF-8.
/// Trusting the type alone would store a mislabelled binary as a note and
/// mangle it; trusting the bytes alone would turn a small UTF-8-clean PDF-ish
/// blob into a "note" nobody can read.
fn text_body(mime: &str, bytes: &[u8]) -> Option<String> {
    let texty = mime.starts_with("text/")
        || matches!(
            mime,
            "application/json" | "application/xml" | "application/x-yaml" | "application/yaml"
        );
    if !texty {
        return None;
    }
    String::from_utf8(bytes.to_vec()).ok()
}

async fn create_node(
    company: ScopedCompany,
    Json(body): Json<CreateNode>,
) -> Result<Json<FsNode>, ApiError> {
    let node = WorkspaceNode {
        id: generate_id(),
        // One naming rule for the tree, whoever is writing
        // ([`crate::company::workspace_names`]). The console is the operator
        // and the operator is not confined here — this is not a restriction on
        // what they may create, only on how it is spelled, and the response
        // returns the node so the console renders the stored name.
        name: kebab_name_or(&body.name, &body.name),
        kind: body.kind,
        parent_id: body.parent_id,
        updated_at_millis: crate::ports::now_millis(),
        // These routes are the console's, and the console is the operator.
        created_by: WorkspaceOrigin::Operator,
        updated_by: WorkspaceOrigin::Operator,
        mime: None,
        size: None,
        sha256: None,
        adopted: false,
    };
    company
        .runtime
        .workspace()
        .create(company.id(), &node, body.content.as_deref())
        .await?;
    let content = match node.kind {
        NodeKind::File => Some(body.content.unwrap_or_default()),
        NodeKind::Folder => None,
    };
    Ok(Json(FsNode::from_node(node, content)))
}

/// `PUT …/workspace/file/{node_id}` — overwrite a note's body.
///
/// # A published deliverable is edited on both surfaces (issue #552)
///
/// Since #552 a note in this tree may be the projection of a task artifact, and
/// an operator's save of one is *the human edit* — the single datum the artifact
/// port exists to capture. Overwriting only the node would leave the version
/// history saying the agent's draft was shipped unchanged, and
/// `human_edit_diff` answering `None` for an artifact a human rewrote.
///
/// So the chain is written **first**, then the node. The two failure modes are
/// not symmetric: a version recorded whose node write then fails leaves a stale
/// node, which is visible and heals on the next write; a node written whose
/// version was never recorded is silent, permanent, and corrupts the diff. Of
/// the two, only the first is survivable, so it is the one this ordering
/// chooses. See [`artifact_mirror`](crate::company::artifact_mirror).
///
/// An ordinary note — which is nearly all of them — matches no artifact, so the
/// lookup answers `Ordinary` and this behaves exactly as it did before. The
/// lookup is a scan of the company's artifacts per save; it is bounded by what
/// artifacts are (a task's drafts, not a repository) and is named as the place
/// to add an index if it ever hurts.
///
/// # When the artifact store cannot answer at all
///
/// The lookup is the only reason this route reads the artifact store, and it
/// runs for every note. Propagating its failure would mean an artifact-store
/// fault rejects the save of a plain note — losing an operator's edit to a
/// store that note does not depend on, to protect a chain it does not have.
/// So a lookup that *fails* is warned about and the node write proceeds.
///
/// This is a deliberate narrowing of the ordering guarantee, not an exception
/// to it. Fail-closed still holds wherever it can be applied: once the store
/// answers and names this node a deliverable, a version that cannot be appended
/// still refuses the save. What is given up is only the case where the store
/// cannot be read at all — there, a published deliverable edited during the
/// outage lands node-ahead-of-chain, the silent direction. The window is an
/// unreachable artifact store, the alternative is refusing every note in the
/// company for the same duration, and the divergence heals on the next
/// successful save of that note.
async fn write_file(
    company: ScopedCompany,
    Path(NodePath { node_id }): Path<NodePath>,
    Json(body): Json<WriteFile>,
) -> Result<Json<WriteAck>, ApiError> {
    // No kind check first: a folder id can never match an artifact (nothing
    // stamps one), so the lookup answers `None` for it and the `write` below
    // still rejects it — an extra read per save to pre-empt an error case would
    // cost every ordinary save to save nothing.
    if let MirrorOutcome::Undetermined(err) = mirror_node_edit(
        company.runtime.artifacts().as_ref(),
        company.id(),
        &node_id,
        &body.content,
        ArtifactAuthor::Operator,
        "operator",
        Some(OPERATOR_EDIT_NOTE.to_string()),
    )
    .await?
    {
        tracing::warn!(
            company = %company.id(),
            node = %node_id,
            error = %err,
            "[workspace] could not read the artifact store, so whether this note is a \
             published deliverable is unknown; saving it anyway. If it was published, its \
             chain is one version behind until the next successful save"
        );
    }

    let node = company
        .runtime
        .workspace()
        .write(
            company.id(),
            &node_id,
            &body.content,
            WorkspaceOrigin::Operator,
        )
        .await?;
    Ok(Json(WriteAck {
        updated_at: node.updated_at_millis,
    }))
}

async fn rename_move(
    company: ScopedCompany,
    Path(NodePath { node_id }): Path<NodePath>,
    Json(body): Json<RenameMove>,
) -> Result<Json<FsNode>, ApiError> {
    // As in `create_node`: a rename lands under the naming rule, so renaming
    // cannot walk a node back out of the convention the tree is kept in.
    let renamed = body.name.as_ref().map(|name| kebab_name_or(name, name));
    let node = company
        .runtime
        .workspace()
        .rename_move(
            company.id(),
            &node_id,
            renamed.as_deref(),
            body.parent_id.as_ref().map(Option::as_deref),
        )
        .await?;
    let content = match node.kind {
        NodeKind::File => company
            .runtime
            .workspace()
            .read(company.id(), &node_id)
            .await?
            .map(|(_, body)| body),
        NodeKind::Folder => None,
    };
    Ok(Json(FsNode::from_node(node, content)))
}

async fn delete_node(
    company: ScopedCompany,
    Path(NodePath { node_id }): Path<NodePath>,
) -> Result<StatusCode, ApiError> {
    if company
        .runtime
        .workspace()
        .delete(company.id(), &node_id)
        .await?
    {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(ApiError(OpenCompanyError::CompanyNotFound(format!(
            "workspace node {node_id}"
        ))))
    }
}

/// `POST …/workspace/sweep-empty-agent-folders` — remove the empty
/// `agents/<id>/` folders a pre-#570 company still carries (issue #700).
///
/// Operator-triggered rather than automatic, and this route is the surface that
/// makes that possible. The affected population is hosted tenants whose operator
/// has a console and no shell, so a subcommand would be unreachable for exactly
/// the people who need it; a boot sweep would change a tenant's tree on an
/// upgrade nobody asked for, which is the outcome #570 and #645 both declined.
/// The click is the opt-in.
///
/// `?dry_run=true` names the candidates and touches nothing, so the console can
/// list every folder on a confirm dialog before the operator agrees. The real
/// call answers with what actually went, and logs it — "removed 17 folders" is
/// not something an operator who disagrees can check.
///
/// The deletes run through `runtime.workspace()`, the same announcer-wrapped
/// handle [`delete_node`] uses, so each removal emits its own
/// `WorkspaceChanged{change:"removed"}` (issue #327) and any console watching
/// the feed sees the tree change rather than discovering it on the next refetch.
///
/// Same authorization as the per-node delete: addressing the company is the
/// guard, because this removes only nodes that provably hold nothing.
async fn sweep_empty_agent_folders(
    company: ScopedCompany,
    Query(query): Query<PreviewQuery>,
) -> Result<Json<SweepResult>, ApiError> {
    let folders = sweep_workspace_agent_folders(
        company.runtime.workspace().as_ref(),
        company.id(),
        query.dry_run,
    )
    .await?;

    Ok(Json(if query.dry_run {
        SweepResult {
            would_remove: Some(folders),
            removed: None,
        }
    } else {
        SweepResult {
            would_remove: None,
            removed: Some(folders),
        }
    }))
}

/// `POST …/workspace/merge-duplicate-folders` — fold the duplicate sibling
/// folders a publish race already left behind (issue #759).
///
/// The recovery half of #759. Stopping new races leaves every tree an old race
/// already broke exactly as broken as it was: two sibling folders share a name,
/// and from then on every publish beneath that path is refused as ambiguous, for
/// every agent, until somebody edits the tree by hand. On a hosted tenant
/// "somebody" has a console and no shell, which is why this is a route.
///
/// Operator-triggered, never automatic — the #570 / #645 / #700 doctrine, and
/// with more reason here: this pass *moves* nodes rather than removing provably
/// empty ones. `?dry_run=true` answers the same plan without touching anything,
/// so the console can name every folder that gives way and every child that
/// relocates before the operator agrees to it.
///
/// The answer always carries `residuals` — what the repair refused to decide,
/// which is a file collision, because two files at one path are two documents
/// and picking one silently discards somebody's work. Merging what can be merged
/// and *saying* what cannot is the honest boundary; a route that answered only
/// with successes would report a half-fixed tree as fixed.
///
/// Every move and delete runs through `runtime.workspace()`, the announcer-
/// wrapped handle the per-node routes use, so an open console sees the tree
/// change rather than discovering it on the next refetch (issue #327).
///
/// Same authorization as the per-node rename/delete it is built out of:
/// addressing the company is the guard.
async fn merge_duplicate_folders(
    company: ScopedCompany,
    Query(query): Query<PreviewQuery>,
) -> Result<Json<RepairResult>, ApiError> {
    let plan = merge_workspace_duplicates(
        company.runtime.workspace().as_ref(),
        company.id(),
        query.dry_run,
    )
    .await?;

    Ok(Json(RepairResult::new(plan, query.dry_run)))
}

#[cfg(test)]
mod serving_test {
    use super::serving_for;

    /// The classification table, stated once so the arms cannot drift apart.
    #[test]
    fn a_stored_mime_maps_to_exactly_one_serving() {
        let cases: &[(Option<&str>, &str, &str)] = &[
            // Renderable: the type survives and the browser shows it.
            (Some("image/png"), "image/png", "inline"),
            (Some("image/jpeg"), "image/jpeg", "inline"),
            (Some("image/gif"), "image/gif", "inline"),
            (Some("image/webp"), "image/webp", "inline"),
            (Some("application/pdf"), "application/pdf", "inline"),
            // Previewable but never a document.
            (Some("image/svg+xml"), "image/svg+xml", "attachment"),
            // Everything else is opaque bytes, whatever the caller called it.
            (Some("text/html"), "application/octet-stream", "attachment"),
            (
                Some("application/xhtml+xml"),
                "application/octet-stream",
                "attachment",
            ),
            (Some("text/plain"), "application/octet-stream", "attachment"),
            (
                Some("application/zip"),
                "application/octet-stream",
                "attachment",
            ),
            (None, "application/octet-stream", "attachment"),
        ];
        for (stored, content_type, disposition) in cases {
            let serving = serving_for(*stored);
            assert_eq!(
                (serving.content_type.as_str(), serving.disposition),
                (*content_type, *disposition),
                "stored mime {stored:?}"
            );
        }
    }

    /// A mime reaches this function from more than one writer, and only the
    /// upload route normalises before storing — `capture_body` stores whatever
    /// `mime_guess` produced, and a payload written straight through the port
    /// stores whatever its caller passed. So the essence is matched here too,
    /// or a parameterised or upper-cased `image/png` would be downgraded to a
    /// download and the console's preview would break for it.
    #[test]
    fn the_essence_is_matched_not_the_raw_header_value() {
        for stored in [
            "image/png; charset=binary",
            "  image/png  ",
            "IMAGE/PNG",
            "Image/Png ;q=1",
        ] {
            let serving = serving_for(Some(stored));
            assert_eq!(serving.content_type, "image/png", "stored mime {stored:?}");
            assert_eq!(serving.disposition, "inline", "stored mime {stored:?}");
        }
    }

    /// The list is closed, not a blocklist: a type nobody has considered is
    /// downloaded rather than rendered.
    #[test]
    fn an_unknown_type_falls_to_the_safe_arm() {
        let serving = serving_for(Some("application/x-invented-2031"));
        assert_eq!(serving.content_type, "application/octet-stream");
        assert_eq!(serving.disposition, "attachment");
    }
}

#[cfg(test)]
mod disambiguate_name_test {
    use super::disambiguate_name;
    use crate::company::workspace_names::{MAX_NAME_BYTES, is_kebab_name, kebab_name_or};

    /// The tag always lands between stem and extension, whatever the name
    /// carried.
    #[test]
    fn inserts_the_tag_before_the_extension() {
        assert_eq!(disambiguate_name("image.png", "01j8ab"), "image-01j8ab.png");
        assert_eq!(disambiguate_name("notes", "01j8ab"), "notes-01j8ab");
        assert_eq!(
            disambiguate_name("page.compiled.mjs", "01j8ab"),
            "page.compiled-01j8ab.mjs"
        );
    }

    /// The regression the budget guard exists for: a name that already fills
    /// the whole workspace-name budget must stay within it once the tag lands,
    /// or the next normalization truncates the tag back off and the name maps
    /// onto the very collision the retry exists to escape.
    #[test]
    fn a_full_budget_name_stays_within_the_budget_and_keeps_the_tag() {
        let stem = "a".repeat(92); // `a…a.png` fills the 96-byte budget exactly.
        let name = format!("{stem}.png");
        assert_eq!(name.len(), MAX_NAME_BYTES);
        assert_eq!(kebab_name_or(&name, &name), name);

        let disambiguated = disambiguate_name(&name, "ab12cd");
        assert!(
            disambiguated.len() <= MAX_NAME_BYTES,
            "disambiguated name is {} bytes, over the {MAX_NAME_BYTES} budget",
            disambiguated.len()
        );
        // The whole tag survives — truncation must never cut it off.
        assert!(disambiguated.contains("-ab12cd"), "{disambiguated}");
        // And the result is canonical, so the repair pass leaves it alone.
        assert!(is_kebab_name(&disambiguated), "{disambiguated}");

        // Re-running the sanitizer is a fixed point: the tag is not a casualty
        // of the budget, which is what would re-collide with the original name.
        assert_eq!(kebab_name_or(&disambiguated, &disambiguated), disambiguated);
    }

    /// A near-full name with a short stem survives whole; the tag still fits.
    #[test]
    fn a_short_stem_is_never_truncated() {
        assert_eq!(disambiguate_name("image.png", "01j8ab"), "image-01j8ab.png");
    }
}
