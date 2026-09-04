//! The [`WorkspaceStore`] port: the company's durable file tree.
//!
//! The workspace is an Obsidian-style tree of folders and Markdown notes the
//! operator organizes, edits, and links with `[[wiki links]]`. Node ids are
//! stable ULIDs, **not** paths, so a rename or move never breaks a reference.
//! The tree is seeded once from `companies/<name>/workspace/**` (WS1 walker)
//! and thereafter written by both the operator and the company's agents.
//!
//! # Authorship (issue #326)
//!
//! Every node records two origins: [`WorkspaceNode::created_by`], fixed at
//! creation, and [`WorkspaceNode::updated_by`], restamped by each content
//! write. Both are a [`WorkspaceOrigin`]. Without them a note an agent wrote is
//! indistinguishable from one the operator typed, which is untenable now that
//! agents can create notes as well as overwrite them (issue #551).
//!
//! The split is deliberate: `rename_move` does **not** touch `updated_by`, so
//! an operator tidying an agent's note into a different folder cannot make the
//! body look operator-authored.

use async_trait::async_trait;
use bytes::Bytes;
use futures::stream::BoxStream;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::Result;
use crate::ports::types::CompanyId;

/// A blob's bytes, delivered in chunks.
///
/// Reads stream and writes buffer, and the asymmetry is deliberate. A quota
/// check has to see the whole size *before* anything is stored — that is what
/// makes a refused write leave nothing behind — so [`create_binary`] takes a
/// slice. A read has no such constraint and a 200 MiB video has no business
/// being resident, so it hands back a stream the HTTP layer can forward
/// straight into a response body.
///
/// Not every backend can honour the streaming half equally: `FsOps` streams
/// from disk and GridFS streams natively, but the sqlite backend holds its blob
/// in one row and yields it as a single chunk. That chunk is bounded by the
/// per-file cap rather than by anything sqlite does, which is stated here rather
/// than discovered in production.
///
/// [`create_binary`]: WorkspaceStore::create_binary
pub type BlobStream = BoxStream<'static, Result<Bytes>>;

/// The message every backend gives when prose is written over a binary node.
///
/// Shared rather than written out three times so the refusal cannot drift
/// between backends — the conformance suite asserts on it, which is only worth
/// anything if all three produce the same sentence.
pub fn binary_write_refusal(name: &str, mime: &str) -> String {
    format!(
        "`{name}` holds {mime} data, not text. Writing text over it would leave its recorded \
         size and checksum describing bytes that are no longer there; replace the payload \
         instead, or delete the node and create a new one."
    )
}

/// The size and content digest of a blob, computed from the bytes themselves.
///
/// Callers never supply either value. A `sha256` a caller could pass would be a
/// claim about bytes the store never checked, and the digest exists precisely so
/// a reader can tell whether the payload they hold is the one that was written —
/// a claim is worth nothing there. Every backend calls this on the bytes it is
/// about to persist, so all three answer identically for identical input, which
/// the shared conformance suite pins.
pub fn blob_metadata(bytes: &[u8]) -> (u64, String) {
    let digest = Sha256::digest(bytes);
    let sha = digest.iter().fold(String::new(), |mut acc, b| {
        use std::fmt::Write as _;
        let _ = write!(acc, "{b:02x}");
        acc
    });
    (bytes.len() as u64, sha)
}

/// Whether a workspace node is a folder or a file.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum NodeKind {
    /// A directory that may contain other nodes.
    Folder,
    /// A file with Markdown content.
    File,
}

/// Who authored a workspace node.
///
/// Internally tagged, so one serde shape serves all three surfaces this value
/// crosses: the opaque `node_json` blob every backend persists, the REST wire
/// body the console reads, and the TypeScript type mirroring it. An agent
/// origin is `{"kind":"agent","id":"ceo"}`; the other two are just
/// `{"kind":"seed"}` / `{"kind":"operator"}`.
///
/// # Why this is not [`ActorKind`](crate::ports::types::ActorKind)
///
/// `ActorKind` is the crate's established "who did this" enum and is
/// deliberately fieldless and `Copy`, with the id carried alongside it in
/// [`Actor::id`](crate::ports::types::Actor). This type diverges from that
/// convention on purpose, for two reasons:
///
/// * **`Seed` is not an actor.** A node walked out of
///   `companies/<name>/workspace/**` at first boot was authored by nobody — not
///   the operator, not an agent. Folding it into `ActorKind::System` would make
///   "the runtime wrote this" and "this shipped with the company" the same
///   badge in the console, erasing the distinction issue #326 exists to draw.
/// * **The flat alternative is worse across seven surfaces.** Four independent
///   fields (`created_by_kind` + `created_by_id` + `updated_by_kind` +
///   `updated_by_id`) would make the store JSON, the REST body, the GraphQL
///   type, the TypeScript type, the console, the seeder and the agent tools
///   each re-derive the invariant "an id is present iff the kind is agent". One
///   enum states it once and serde enforces it at every boundary.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum WorkspaceOrigin {
    /// Walked out of the company bundle at first boot by the WS1 seeder.
    Seed,
    /// A human operator, through the console or the REST routes.
    ///
    /// The default, and therefore what every node written before authorship
    /// existed deserializes to. That is a one-time, direction-honest
    /// misattribution: a legacy seeded note reads as operator-authored, which
    /// is the conservative answer (it never credits an agent for something it
    /// did not write).
    #[default]
    Operator,
    /// An agent inside this company, named by its roster id.
    Agent {
        /// The agent's roster id, e.g. `ceo`.
        id: String,
    },
}

/// What [`WorkspaceStore::adopt_or_create_folder`] did to satisfy a caller's
/// claim on `(parent, name)` (issue #759).
///
/// The caller almost always wants the node and not the verb — a publish walking
/// `agents/<agent>/<task>/` does the same thing either way. The distinction is
/// carried anyway because exactly one consumer must be able to tell: the
/// workspace announcer emits a node-created frame, and a frame for a folder that
/// was already standing would tell an open console that something appeared when
/// nothing did. Returning a bare id would make that undecidable at the only
/// layer that has to decide it.
#[derive(Clone, Debug, PartialEq)]
pub enum FolderClaim {
    /// The name was free and this call minted the folder.
    Created(WorkspaceNode),
    /// A folder was already there and is handed back untouched — authorship
    /// stamp, timestamp and all. See [`WorkspaceStore::adopt_or_create_folder`]
    /// for why adoption rather than refusal is the right answer for a folder.
    Adopted(WorkspaceNode),
}

impl FolderClaim {
    /// The folder that now answers to `(parent, name)`, however it got there.
    pub fn node(&self) -> &WorkspaceNode {
        match self {
            Self::Created(node) | Self::Adopted(node) => node,
        }
    }

    /// The folder's id — the thing nearly every caller actually wants.
    pub fn id(&self) -> &str {
        &self.node().id
    }

    /// Consumes the claim for the node inside it.
    pub fn into_node(self) -> WorkspaceNode {
        match self {
            Self::Created(node) | Self::Adopted(node) => node,
        }
    }

    /// Whether this call is the one that minted the folder.
    pub fn was_created(&self) -> bool {
        matches!(self, Self::Created(_))
    }
}

/// The folder node a store is about to insert for a claim.
///
/// Shared so all three backends mint the same shape — a fresh ULID, both origin
/// fields stamped with the caller's `origin`, and no blob metadata — rather than
/// three chances to stamp `created_by` differently.
pub fn new_folder(name: &str, parent_id: Option<&str>, origin: WorkspaceOrigin) -> WorkspaceNode {
    WorkspaceNode {
        id: crate::ports::generate_id(),
        name: name.to_string(),
        kind: NodeKind::Folder,
        parent_id: parent_id.map(str::to_string),
        updated_at_millis: crate::ports::now_millis(),
        created_by: origin.clone(),
        updated_by: origin,
        mime: None,
        size: None,
        sha256: None,
        adopted: false,
    }
}

/// The folder already answering to `(parent, name)`, or `None` when the name is
/// free — refusing anything a claim must not resolve.
///
/// The read half of [`WorkspaceStore::adopt_or_create_folder`], shared so the
/// fail-closed rule has one implementation and all three backends refuse with
/// the same sentence. The conformance suite asserts on those sentences, which is
/// only worth anything if they cannot drift.
///
/// A *file* holding the name, or several nodes holding it, is a
/// [`Conflict`](crate::error::OpenCompanyError::Conflict): the first because a
/// folder and a note at one path is the ambiguity the tool layer already
/// refuses, and the second because a tree that lost this race *before* the guard
/// existed must stay refused rather than have a third node added to it.
pub fn existing_folder_claim<'a>(
    nodes: impl Iterator<Item = &'a WorkspaceNode>,
    parent: Option<&str>,
    name: &str,
) -> Result<Option<WorkspaceNode>> {
    use crate::error::OpenCompanyError;
    let matches: Vec<&WorkspaceNode> = nodes
        .filter(|node| node.parent_id.as_deref() == parent && node.name == name)
        .collect();
    match matches.as_slice() {
        [one] if one.kind == NodeKind::Folder => Ok(Some((*one).clone())),
        [_] => Err(OpenCompanyError::Conflict(folder_claim_file_refusal(name))),
        [] => Ok(None),
        many => Err(OpenCompanyError::Conflict(folder_claim_ambiguous_refusal(
            name,
            many.len(),
        ))),
    }
}

/// The refusal every backend gives when a *file* already carries the name a
/// folder claim asked for.
pub fn folder_claim_file_refusal(name: &str) -> String {
    format!(
        "`{name}` already exists as a note, not a folder, so a folder cannot be claimed at that \
         path"
    )
}

/// The refusal every backend gives when the name a folder claim asked for is
/// already carried by more than one node — a tree that lost this race before the
/// guard existed.
pub fn folder_claim_ambiguous_refusal(name: &str, count: usize) -> String {
    format!("{count} nodes under this folder are named `{name}`, so the path is ambiguous")
}

/// Whether a **file** already occupies `(parent, name)` (issue #894).
///
/// The sibling-uniqueness predicate every backend's [`WorkspaceStore::create`]
/// applies, shared so the three cannot drift on what "taken" means.
///
/// # Files only, and that is the rule rather than an omission
///
/// A folder is not consulted and does not block, so a folder and a file may
/// still share a name in one parent exactly as they always could. Folder-vs-
/// folder claims are [`adopt_or_create_folder`]'s job (issue #759), which
/// adopts rather than refuses. Widening this to every kind would be a **new
/// tree rule** rather than the race fix issue #894 asks for, and it would break
/// MongoDB, whose guard is a partial index over file documents alone.
///
/// [`adopt_or_create_folder`]: WorkspaceStore::adopt_or_create_folder
pub fn file_name_taken<'a>(
    nodes: impl Iterator<Item = &'a WorkspaceNode>,
    parent: Option<&str>,
    name: &str,
) -> bool {
    nodes.into_iter().any(|node| {
        node.kind == NodeKind::File && node.parent_id.as_deref() == parent && node.name == name
    })
}

/// The refusal every backend gives when a file already carries the name a
/// [`create`](WorkspaceStore::create) asked for.
pub fn duplicate_file_refusal(name: &str) -> String {
    format!("workspace already contains a note named `{name}` in that folder")
}

/// One node in the workspace tree. `id` is a stable ULID; `parent_id` is `None`
/// at the workspace root.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceNode {
    /// Stable ULID id.
    pub id: String,
    /// Display name (including any extension).
    pub name: String,
    /// Whether this node is a folder or a file.
    pub kind: NodeKind,
    /// The parent folder's id, or `None` at the root.
    #[serde(default)]
    pub parent_id: Option<String>,
    /// Epoch-millis timestamp of the last update.
    pub updated_at_millis: u64,
    /// Who created this node. Never restamped — the creator of a note is a
    /// fact about its history, not about its current body.
    #[serde(default)]
    pub created_by: WorkspaceOrigin,
    /// Who last wrote this node's **content**.
    ///
    /// Restamped by [`WorkspaceStore::write`] and by nothing else — in
    /// particular not by [`WorkspaceStore::rename_move`], so an operator
    /// reorganising the tree cannot mask agent authorship of the body that is
    /// actually stored.
    #[serde(default)]
    pub updated_by: WorkspaceOrigin,
    /// The media type of a **binary** node's payload, e.g. `image/png`.
    ///
    /// `Some` is the one flag that says "this node holds bytes, not prose", and
    /// every surface keys off it: the console renders instead of editing, the
    /// agent tools answer with metadata instead of a body, and the text `write`
    /// path refuses. See the invariant on [`WorkspaceStore::create_binary`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mime: Option<String>,
    /// The payload's exact length in bytes. Store-computed; see
    /// [`blob_metadata`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size: Option<u64>,
    /// The lowercase hex SHA-256 of the payload. Store-computed; see
    /// [`blob_metadata`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sha256: Option<String>,
    /// Whether a second caller has *adopted* this folder through
    /// [`WorkspaceStore::adopt_or_create_folder`] and now has a legitimate
    /// reason to write beneath it (issue #1839).
    ///
    /// # Why a folder needs a lease flag
    ///
    /// The empty-folder rollback issue #1801 added
    /// ([`rollback_empty_minted_folders`](crate::company::workspace_scaffold::rollback_empty_minted_folders))
    /// undoes a *minted* folder whose write then failed. But a folder one caller
    /// minted, a second caller can adopt — `adopt_or_create_folder` hands the
    /// loser [`FolderClaim::Adopted`] and, by design, does **not** add it to that
    /// caller's own `minted` set. Nothing then recorded that the folder has a
    /// second writer, so the minter's later rollback could sweep the folder the
    /// adopter was about to write into. This flag is that record: an adoption
    /// stamps it `true`, and [`delete_if_empty`](WorkspaceStore::delete_if_empty)
    /// refuses a folder carrying it. See the guard on `delete_if_empty`.
    ///
    /// # Sticky, and conservative on the migration
    ///
    /// `#[serde(default)]` **is** the migration: every node written before this
    /// field existed loads as `false` on all three backends, no rewrite — and
    /// `false` is the conservative reading, because it leaves a pre-#1839 empty
    /// folder exactly as rollback-eligible as it is today. The flag only ever
    /// *adds* a folder to the set of things a delete refuses; it never shrinks
    /// it. It is never cleared: an adopted-then-emptied folder waits for
    /// Tidy/Repair, whose job empty-folder cleanup already is.
    #[serde(default)]
    pub adopted: bool,
}

impl WorkspaceNode {
    /// Whether this node holds bytes rather than prose.
    ///
    /// `mime` is the single discriminator — `size` and `sha256` are derived, and
    /// a node carrying one of them without a mime would be a store bug rather
    /// than a shape any caller should have to handle.
    pub fn is_binary(&self) -> bool {
        self.mime.is_some()
    }
}

/// Answers [`WorkspaceStore::read_capped`] by reading the whole body and
/// measuring it.
///
/// For a store that already holds every body in memory — the test doubles — where
/// the read *is* the measurement and there is nothing to transfer. **Never for a
/// decorator over a real store**: forwarding is what keeps the ceiling, and this
/// would quietly spend the allocation the caller asked the store to avoid.
pub async fn read_capped_by_reading(
    store: &(impl WorkspaceStore + ?Sized),
    company: &CompanyId,
    id: &str,
    max_bytes: u64,
) -> Result<Option<(WorkspaceNode, String, u64)>> {
    Ok(store.read(company, id).await?.map(|(node, body)| {
        let len = body.len() as u64;
        if len > max_bytes {
            (node, String::new(), len)
        } else {
            (node, body, len)
        }
    }))
}

/// Wraps already-buffered bytes as a one-chunk [`BlobStream`].
///
/// For the backends that hold a payload in memory by the time they can answer
/// (sqlite's row `BLOB`). A backend that can genuinely stream should not use
/// this.
pub fn one_chunk(bytes: Vec<u8>) -> BlobStream {
    Box::pin(futures::stream::once(async move { Ok(Bytes::from(bytes)) }))
}

/// Validates a node about to be created as binary and stamps its derived
/// metadata.
///
/// Every backend calls this rather than trusting the node it was handed, so
/// "`size` and `sha256` are computed, never accepted" is one implementation
/// instead of three chances to get it wrong.
pub fn stamped_binary(node: &WorkspaceNode, bytes: &[u8]) -> Result<WorkspaceNode> {
    use crate::error::OpenCompanyError;
    if node.kind != NodeKind::File {
        return Err(OpenCompanyError::InvalidRequest(
            "a binary node must be a file, not a folder".to_string(),
        ));
    }
    let Some(mime) = node.mime.clone() else {
        return Err(OpenCompanyError::InvalidRequest(
            "a binary node must carry a mime type".to_string(),
        ));
    };
    let (size, sha256) = blob_metadata(bytes);
    Ok(WorkspaceNode {
        mime: Some(mime),
        size: Some(size),
        sha256: Some(sha256),
        adopted: false,
        ..node.clone()
    })
}

/// Re-stamps an existing binary node for a payload replacement.
///
/// Refuses a folder and refuses a *prose* node: neither kind can be converted
/// into the other by a write, which is the same rule
/// [`WorkspaceStore::write`] enforces from the other side.
pub fn rebind_binary(
    node: &mut WorkspaceNode,
    bytes: &[u8],
    mime: Option<&str>,
    author: WorkspaceOrigin,
) -> Result<()> {
    use crate::error::OpenCompanyError;
    if node.kind != NodeKind::File {
        return Err(OpenCompanyError::InvalidRequest(
            "cannot write bytes to a folder".to_string(),
        ));
    }
    if !node.is_binary() {
        return Err(OpenCompanyError::InvalidRequest(format!(
            "`{}` holds text, not binary data; write it as text instead",
            node.name
        )));
    }
    let (size, sha256) = blob_metadata(bytes);
    if let Some(mime) = mime {
        node.mime = Some(mime.to_string());
    }
    node.size = Some(size);
    node.sha256 = Some(sha256);
    node.updated_at_millis = crate::ports::now_millis();
    node.updated_by = author;
    Ok(())
}

/// Durable per-company workspace tree. Company A's files MUST be invisible to
/// company B.
///
/// # Bytes are a second, additive path (issue #553)
///
/// The text half of this port — [`read`](Self::read), [`write`](Self::write),
/// [`create`](Self::create) — takes and returns `String`, and stays that way.
/// Seventeen call sites reference this port; widening them to bytes would move
/// the backlink scan, the seeder, the GraphQL projection, the agent tools and
/// the REST layer for no gain, because none of them can do anything with a PNG.
/// So a binary node is reached through [`create_binary`](Self::create_binary),
/// [`write_binary`](Self::write_binary) and [`read_bytes`](Self::read_bytes),
/// and every text caller compiles and behaves exactly as it did.
///
/// **The invariant**: a binary node is a [`NodeKind::File`] whose
/// [`mime`](WorkspaceNode::mime), [`size`](WorkspaceNode::size) and
/// [`sha256`](WorkspaceNode::sha256) are all `Some`. All three are set together
/// by the store, or none is.
///
/// **A text [`read`](Self::read) of a binary node yields an empty body**, the
/// same answer a folder gives. That is not a placeholder — it is what keeps
/// `file_with_backlinks`, `rename_move`'s content refetch and the GraphQL
/// resolver compiling and correct without knowing binaries exist: none of them
/// can render bytes, and an empty body is the honest thing to hand a
/// prose-shaped caller. Surfaces that *should* care check
/// [`WorkspaceNode::is_binary`] and say something better.
#[async_trait]
pub trait WorkspaceStore: Send + Sync {
    /// May a payload of `len` bytes be accepted for storage under `name`,
    /// whatever it will end up being stored *as* (issue #665)?
    ///
    /// # Why this exists next to the write methods that already check
    ///
    /// The quota decorator meters binary payloads and deliberately leaves prose
    /// notes uncounted — see
    /// [`QuotaEnforcedWorkspace`](crate::runtime::QuotaEnforcedWorkspace), whose
    /// premise is that "a note is bounded by what a model will emit into a tool
    /// call". That premise holds for every writer it covers and fails for
    /// exactly one caller: the multipart upload route, where arbitrary
    /// operator-supplied bytes enter the tree and are classified as prose purely
    /// because they happen to decode as UTF-8.
    ///
    /// So this is the route **asking the store to judge**, rather than the route
    /// applying a limit of its own. That distinction is what keeps policy where
    /// the quota module insists it belongs: a caller cannot know a company's
    /// configured cap — routers are built once, before any company exists — and
    /// a limit re-derived at the call site would be the global default, silently
    /// wrong for any company that raised or lowered its own.
    ///
    /// Defaults to admitting everything, so a backend that stores no quota — and
    /// every test double — is unaffected.
    async fn admit_upload(&self, _company: &CompanyId, _name: &str, _len: u64) -> Result<()> {
        Ok(())
    }
    /// Returns every node in the tree (order unspecified; callers build the
    /// tree from `parent_id`).
    async fn tree(&self, company: &CompanyId) -> Result<Vec<WorkspaceNode>>;
    /// Reads one node and, for files, its content. Folders — and binary nodes —
    /// yield an empty body; see the trait docs.
    async fn read(&self, company: &CompanyId, id: &str) -> Result<Option<(WorkspaceNode, String)>>;
    /// Reads one node's body only when it weighs no more than `max_bytes`,
    /// alongside the byte length it actually has.
    ///
    /// The body is empty when it is longer than `max_bytes`; the length tells
    /// that apart from a note that is genuinely empty. Folders and binary nodes
    /// answer an empty body and a length of `0`, exactly as [`read`](Self::read)
    /// does.
    ///
    /// # Why this is not `read` plus a length check
    ///
    /// [`read`](Self::read) has no ceiling. A caller that will discard anything
    /// over a cap — a chat attachment's extraction, whose cap is a fraction of
    /// what a note may weigh — would have to materialise the whole body to find
    /// out it must throw it away, which is the allocation the cap exists to
    /// prevent. The binary half of this port has never had that problem:
    /// [`size`](WorkspaceNode::size) rides the node, so a caller decides on
    /// metadata and [`read_bytes`](Self::read_bytes) is never reached for a
    /// payload over the cap. A prose node carries no `size`, so the store is the
    /// only place that can answer the same question that cheaply — and it is the
    /// only place that can answer both parts of it at once, so a body cannot
    /// grow past the cap between the measurement and the read.
    ///
    /// A decorator MUST forward this rather than let it fall back to `read`;
    /// the whole point is what is *not* transferred.
    async fn read_capped(
        &self,
        company: &CompanyId,
        id: &str,
        max_bytes: u64,
    ) -> Result<Option<(WorkspaceNode, String, u64)>>;
    /// Overwrites a file's content, returning the updated node. A folder id —
    /// or a **binary** node's id — is an
    /// [`OpenCompanyError::InvalidRequest`](crate::error::OpenCompanyError).
    ///
    /// Refusing a binary node here is what stops the corruption this path would
    /// otherwise cause silently: the node's `mime`, `size` and `sha256` describe
    /// a payload, and storing prose behind them would leave every reader — the
    /// console's download, the digest check, the agent's metadata read — being
    /// told about bytes that are no longer there. [`write_binary`] is the way to
    /// replace a payload.
    ///
    /// `author` is stamped onto [`WorkspaceNode::updated_by`]; it is the
    /// caller's identity, never anything derived from `content`.
    ///
    /// [`write_binary`]: Self::write_binary
    async fn write(
        &self,
        company: &CompanyId,
        id: &str,
        content: &str,
        author: WorkspaceOrigin,
    ) -> Result<WorkspaceNode>;
    /// Creates a node (folder or file). The node's `id` must be fresh; the
    /// `parent_id`, when set, must name an existing folder. `content` seeds a
    /// file body.
    ///
    /// # A file's name must be free among its siblings (issue #894)
    ///
    /// Creating a **file** whose `(parent_id, name)` a file already occupies is
    /// an [`OpenCompanyError::Conflict`](crate::error::OpenCompanyError). The
    /// predicate is [`file_name_taken`] and the refusal is
    /// [`duplicate_file_refusal`], shared so the backends cannot disagree about
    /// either.
    ///
    /// **This is a store guarantee, not a caller convention, and the difference
    /// is the whole of issue #894.** `workspace_create` checks the path index
    /// and then calls this — check-then-act, which two concurrent callers both
    /// pass. Only the store can decide the loser: fs under the index lock,
    /// SQLite inside an `IMMEDIATE` transaction, MongoDB against a partial
    /// unique index. A caller-side check is a narrowing of the window, never a
    /// closing of it, so nothing above this line may be trusted to enforce it.
    ///
    /// Folders are deliberately exempt — see [`file_name_taken`]. A tree that
    /// *already* holds duplicates keeps them: this refuses new collisions and
    /// repairs no history, so a store carrying pre-existing duplicates opens
    /// and serves exactly as before (`read` still answers that path
    /// `Ambiguous`, `list` still shows both).
    ///
    /// No `author` argument: the node arrives fully formed, so the caller sets
    /// [`WorkspaceNode::created_by`] and [`WorkspaceNode::updated_by`] on it
    /// directly.
    async fn create(
        &self,
        company: &CompanyId,
        node: &WorkspaceNode,
        content: Option<&str>,
    ) -> Result<()>;
    /// Claims the folder `name` under `parent`, minting it or adopting the one
    /// already there — atomically, against every other caller of this method
    /// (issue #759).
    ///
    /// **The contract**: when this returns `Ok`, exactly one folder answers to
    /// `(parent, name)` among the nodes this primitive governs, and the returned
    /// node is it. `parent` of `None` is the workspace root, which is why it is
    /// an `Option` rather than a `&str` — the `agents/` and `desks/` roots are
    /// claimed by the same call as everything beneath them.
    ///
    /// Adoption **preserves the original authorship stamp**: `origin` is used
    /// only when this call is the one that creates the folder. A second
    /// publisher does not get to rewrite whose folder it is.
    ///
    /// Fail-closed on anything that is not a single folder: a *file* holding the
    /// name is a [`Conflict`](crate::error::OpenCompanyError::Conflict), and so
    /// is a name already carried by several nodes — a tree that lost this race
    /// before the guard existed stays refused rather than gaining a third node.
    /// Both refusals come from [`existing_folder_claim`], so they read the same
    /// on every backend.
    ///
    /// # Why adoption, and not [`swap_files`](Self::swap_files)'s compare-and-swap
    ///
    /// `swap_files` stages a payload and makes the loser **fail**, which is
    /// right for a file: its bytes are a content claim with one legitimate
    /// winner. A folder is a payload-free **container** claim — two publishers
    /// that both want `agents/cmo/task-42/` want the same thing, so the loser
    /// must adopt and carry on rather than report a publish failure the operator
    /// can do nothing about. Forcing folders through that CAS would need a
    /// re-read-and-adopt retry loop wrapped around it, and `swap_files` rejects
    /// non-file replacements outright for fs-rename and GridFS reasons that do
    /// not apply here.
    ///
    /// It also dissolves loser cleanup: a loser never owns a folder, so nothing
    /// was ever written beneath one that gets discarded.
    ///
    /// # No default implementation, deliberately
    ///
    /// A read-then-create default would compile everywhere and silently
    /// reintroduce the exact race this exists to close on any backend that
    /// forgot to override it. Required, so the compiler names every implementor.
    async fn adopt_or_create_folder(
        &self,
        company: &CompanyId,
        parent: Option<&str>,
        name: &str,
        origin: WorkspaceOrigin,
    ) -> Result<FolderClaim>;
    /// Creates a **binary** file node holding `bytes`.
    ///
    /// The binary twin of [`create`](Self::create), with the same freshness and
    /// parent rules. `node.kind` must be [`NodeKind::File`] and `node.mime` must
    /// be set — the caller inferred the media type and only the caller can.
    ///
    /// `node.size` and `node.sha256` are **ignored and recomputed** from `bytes`
    /// via [`blob_metadata`]. A digest a caller supplied would be an unverified
    /// claim about the payload, which is the one thing a digest must not be.
    ///
    /// `bytes` is a slice rather than a stream on purpose: the quota decorator
    /// has to know the full size before anything is written, so that a refused
    /// write leaves no partial blob and no node behind. See [`BlobStream`].
    ///
    /// # Returns the **stamped** node, and that is the point (issue #668)
    ///
    /// The node handed back is the one [`stamped_binary`] produced, carrying the
    /// `size` and `sha256` the store computed from `bytes` — not the node the
    /// caller passed in. [`write_binary`](Self::write_binary) already returned
    /// its updated node; this makes the pair symmetric.
    ///
    /// It exists so a caller that needs the digest — the publish drain, which
    /// records it on the artifact version — can only ever obtain it **from the
    /// store**. Hashing the same bytes caller-side would give one algorithm
    /// called twice, and two calls can disagree if the bytes differ between
    /// them; a returned node makes the digest's provenance structural, so a
    /// future backend cannot quietly substitute its own.
    async fn create_binary(
        &self,
        company: &CompanyId,
        node: &WorkspaceNode,
        bytes: &[u8],
    ) -> Result<WorkspaceNode>;
    /// Replaces a binary node's payload, returning the updated node.
    ///
    /// The binary twin of [`write`](Self::write), and the reason re-publishing a
    /// generated image revises the note the operator has been reading rather
    /// than opening a rival beside it. A missing id, a folder, or a node that
    /// holds *text* is an
    /// [`InvalidRequest`](crate::error::OpenCompanyError::InvalidRequest) — the
    /// mirror of `write`'s refusal, so neither kind of node can be turned into
    /// the other by a write.
    ///
    /// `mime` of `None` keeps the node's current media type; `size` and
    /// `sha256` are recomputed, and `author` is stamped onto `updated_by`
    /// exactly as `write` does.
    async fn write_binary(
        &self,
        company: &CompanyId,
        id: &str,
        bytes: &[u8],
        mime: Option<&str>,
        author: WorkspaceOrigin,
    ) -> Result<WorkspaceNode>;
    /// Reads a binary node's metadata and streams its payload.
    ///
    /// `None` when the id names nothing **or** names a node that is not binary —
    /// a folder or a prose note. Callers use this to serve a download, so
    /// "there is no payload here" and "there is no node here" lead to the same
    /// 404 and are deliberately not distinguished.
    async fn read_bytes(
        &self,
        company: &CompanyId,
        id: &str,
    ) -> Result<Option<(WorkspaceNode, BlobStream)>>;
    /// Renames and/or reparents a node, returning the updated node. Moving a
    /// folder under its own descendant (a cycle) is rejected.
    ///
    /// Leaves both origin fields alone — see [`WorkspaceNode::updated_by`].
    ///
    /// `parent` distinguishes three intents: `None` leaves the parent
    /// unchanged, `Some(None)` moves the node to the workspace root, and
    /// `Some(Some(id))` reparents it under folder `id`.
    async fn rename_move(
        &self,
        company: &CompanyId,
        id: &str,
        name: Option<&str>,
        parent: Option<Option<&str>>,
    ) -> Result<WorkspaceNode>;
    /// Atomically installs a staged file at `name`, conditional on what is
    /// there now. A compare-and-swap on the *occupant of the path*.
    ///
    /// `replacement_id` is renamed to `name` as part of the operation, under
    /// its own existing parent. `expected_id` says what the caller believes
    /// occupies that name, and is the whole guard:
    ///
    /// * **`Some(id)`** — expect `id` to be the file at `name`. It is removed
    ///   and the staged file takes its place. This is a republish (issue #662).
    /// * **`None`** — assert the name is **unoccupied**, and install only while
    ///   that holds. This is a first publish (issue #697).
    ///
    /// `None` does **not** mean "any occupant will do", and it is not a way to
    /// skip the check: passing `None` for a path that *is* occupied must fail
    /// the compare-and-swap rather than overwrite the node that is there. The
    /// two spellings are one type apart and mean opposite things, so a caller
    /// that means "replace whatever is at this path" must read the current id
    /// and pass `Some(it)`. The conformance suite pins the occupied-`None` case
    /// directly, because nothing else stops that mistake from compiling.
    ///
    /// Returns the promoted node on success. When the compare-and-swap loses —
    /// the expected node has already gone, or the name the caller expected to
    /// be free has been taken — returns `None` **and consumes the staged
    /// replacement**, including any binary payload. That cleanup is part of the
    /// port contract: a caller losing the race must not leak its private
    /// staging node, which would charge the tenant's quota for bytes nothing
    /// can reach.
    ///
    /// Backends must make the logical tree transition without an observable
    /// delete-then-rename gap, and must decide concurrent callers so that
    /// exactly one wins. This is deliberately a store primitive rather than two
    /// existing calls: a process-local lock cannot protect MongoDB when two
    /// server instances publish concurrently.
    async fn swap_files(
        &self,
        company: &CompanyId,
        expected_id: Option<&str>,
        replacement_id: &str,
        name: &str,
    ) -> Result<Option<WorkspaceNode>>;
    /// Deletes a node; folders are removed recursively. Returns whether a node
    /// was removed.
    ///
    /// A binary node's payload goes with it — a backend that dropped the node
    /// and kept the blob would leak the tenant's quota to nothing reachable.
    /// The conformance suite checks this by asking [`read_bytes`](Self::read_bytes)
    /// afterwards rather than by trusting the delete's return value.
    async fn delete(&self, company: &CompanyId, id: &str) -> Result<bool>;
    /// Deletes `id` only if it is currently childless, checked and removed as
    /// close to one operation as the backend can manage — never a caller's
    /// earlier [`tree`](Self::tree) snapshot handed back to [`delete`](Self::delete).
    ///
    /// Exists for callers like [`rollback_empty_minted_folders`
    /// (`workspace_scaffold`)](crate::company::workspace_scaffold::rollback_empty_minted_folders)
    /// that decide a folder is safe to remove from a tree read taken earlier
    /// in the same request. [`delete`](Self::delete) recurses unconditionally,
    /// so re-using it against that stale read races a concurrent adopter: a
    /// child can land in the window between the read and the call, and
    /// `delete` sweeps it away with the folder it was supposed to save (found
    /// in review on issue #1801's PR).
    ///
    /// Returns `Ok(false)`, and deletes nothing, when `id` does not exist or
    /// currently has a child — a caller must not read a `false` as "gone".
    ///
    /// The default re-derives this from [`tree`](Self::tree) and
    /// [`delete`](Self::delete), which only narrows the window a caller
    /// already has rather than closing it — adequate for a decorator that has
    /// no tighter primitive of its own, but a decorator MUST still forward to
    /// its inner store's override rather than rely on this default, or an
    /// inner backend's real fix never gets called. [`FsOps`](crate::store::FsOps)
    /// overrides this under the same per-company index lock every other
    /// writer takes, closing the window entirely; MongoDB has no equivalent
    /// lock, so its override only re-checks immediately before deleting.
    async fn delete_if_empty(&self, company: &CompanyId, id: &str) -> Result<bool> {
        let nodes = self.tree(company).await?;
        let Some(node) = nodes.iter().find(|node| node.id == id) else {
            return Ok(false);
        };
        // A folder a second caller has adopted has a legitimate writer, even
        // while it is still childless (issue #1839). The adopter's write has
        // not landed yet, so an emptiness check alone would let the minter's
        // rollback sweep the folder out from under it. The lease flag is the
        // record that says "someone else is about to write here"; refuse.
        if node.adopted {
            return Ok(false);
        }
        if nodes
            .iter()
            .any(|node| node.parent_id.as_deref() == Some(id))
        {
            return Ok(false);
        }
        self.delete(company, id).await
    }
    /// Whether the workspace has no nodes — the gate the seeder checks so a
    /// seeded-then-emptied workspace is never re-seeded.
    async fn is_empty(&self, company: &CompanyId) -> Result<bool>;
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every backend persists a node as opaque JSON, so a node written before
    /// authorship existed has neither field. It must still load — and must load
    /// as `Operator`, the conservative answer that never credits an agent for
    /// something it did not write.
    ///
    /// This is the whole of the migration story: no `ALTER TABLE`, no
    /// `add_column_if_missing`, no backfill.
    #[test]
    fn a_legacy_node_without_origins_loads_as_operator() {
        let legacy = r#"{
            "id": "n-1",
            "name": "voice.md",
            "kind": "file",
            "parentId": null,
            "updatedAtMillis": 1700000000000
        }"#;
        let node: WorkspaceNode = serde_json::from_str(legacy).expect("legacy node must load");
        assert_eq!(node.created_by, WorkspaceOrigin::Operator);
        assert_eq!(node.updated_by, WorkspaceOrigin::Operator);
        // Issue #1839: the adoption lease is `#[serde(default)]`, so a node
        // written before it existed loads as `false` — the conservative reading
        // that leaves a pre-#1839 empty folder exactly as rollback-eligible as it
        // is today. That default IS the whole migration: no rewrite, no backfill.
        assert!(
            !node.adopted,
            "a legacy node without the field must load unadopted"
        );
    }

    /// The internally-tagged wire shape, pinned.
    ///
    /// The same bytes are read by three independent consumers — the stores'
    /// `node_json`, the REST body the console parses, and the GraphQL
    /// projection — so a stray `rename_all` or a switch to an adjacently-tagged
    /// representation would break the console at runtime with nothing in Rust
    /// CI noticing. This test is what turns that into a compile-suite failure.
    #[test]
    fn the_agent_origin_wire_shape_is_tagged_kind_plus_id() {
        let agent = WorkspaceOrigin::Agent {
            id: "ceo".to_string(),
        };
        assert_eq!(
            serde_json::to_value(&agent).unwrap(),
            serde_json::json!({ "kind": "agent", "id": "ceo" })
        );
        assert_eq!(
            serde_json::to_value(WorkspaceOrigin::Seed).unwrap(),
            serde_json::json!({ "kind": "seed" })
        );
        assert_eq!(
            serde_json::to_value(WorkspaceOrigin::Operator).unwrap(),
            serde_json::json!({ "kind": "operator" })
        );

        // …and back, so the shape is a round trip rather than a one-way render.
        let parsed: WorkspaceOrigin =
            serde_json::from_value(serde_json::json!({ "kind": "agent", "id": "ceo" })).unwrap();
        assert_eq!(parsed, agent);
    }

    /// A node carrying both origins round-trips through the exact `node_json`
    /// path the backends use.
    #[test]
    fn a_node_round_trips_both_origins() {
        let node = WorkspaceNode {
            id: "n-1".to_string(),
            name: "brief.md".to_string(),
            kind: NodeKind::File,
            parent_id: Some("f-1".to_string()),
            updated_at_millis: 42,
            created_by: WorkspaceOrigin::Agent {
                id: "cmo".to_string(),
            },
            updated_by: WorkspaceOrigin::Operator,
            mime: None,
            size: None,
            sha256: None,
            adopted: false,
        };
        let json = serde_json::to_string(&node).unwrap();
        assert_eq!(serde_json::from_str::<WorkspaceNode>(&json).unwrap(), node);
        // camelCase on the node, matching every other field on it.
        assert!(json.contains("\"createdBy\""), "{json}");
        assert!(json.contains("\"updatedBy\""), "{json}");
    }

    /// A prose note carries no blob metadata **on the wire at all** — the three
    /// fields are `skip_serializing_if`, so the tree read the console makes on
    /// every mount does not grow three nulls per node, and `mime` being present
    /// is a reliable "this is binary" test rather than a present-but-null
    /// ambiguity.
    #[test]
    fn a_text_node_serializes_without_the_blob_fields() {
        let node = WorkspaceNode {
            id: "n-1".to_string(),
            name: "voice.md".to_string(),
            kind: NodeKind::File,
            parent_id: None,
            updated_at_millis: 1,
            created_by: WorkspaceOrigin::Operator,
            updated_by: WorkspaceOrigin::Operator,
            mime: None,
            size: None,
            sha256: None,
            adopted: false,
        };
        let json = serde_json::to_string(&node).unwrap();
        assert!(!json.contains("mime"), "{json}");
        assert!(!json.contains("size"), "{json}");
        assert!(!json.contains("sha256"), "{json}");
        assert!(!node.is_binary());
    }

    /// A binary node round-trips all three fields through the exact `node_json`
    /// path every backend persists, in camelCase like the rest of the node.
    #[test]
    fn a_binary_node_round_trips_its_blob_metadata() {
        let node = WorkspaceNode {
            id: "n-2".to_string(),
            name: "chart.png".to_string(),
            kind: NodeKind::File,
            parent_id: None,
            updated_at_millis: 1,
            created_by: WorkspaceOrigin::Operator,
            updated_by: WorkspaceOrigin::Operator,
            mime: Some("image/png".to_string()),
            size: Some(1234),
            sha256: Some("abc123".to_string()),
            adopted: false,
        };
        let json = serde_json::to_string(&node).unwrap();
        assert_eq!(serde_json::from_str::<WorkspaceNode>(&json).unwrap(), node);
        assert!(json.contains("\"sha256\""), "{json}");
        assert!(node.is_binary());
    }

    /// The digest is over the bytes, not over any text rendering of them — so a
    /// payload that is not UTF-8 at all still has one, which is the whole point.
    #[test]
    fn blob_metadata_is_the_sha256_of_the_raw_bytes() {
        // The empty-input SHA-256, a value with a published constant to check
        // against, so this pins the encoding (lowercase hex) and not just
        // self-consistency.
        assert_eq!(
            blob_metadata(b""),
            (
                0,
                "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855".to_string()
            )
        );
        // Invalid UTF-8: a lone continuation byte. Digesting must not care.
        let (size, sha) = blob_metadata(&[0xff, 0xfe, 0x00]);
        assert_eq!(size, 3);
        assert_eq!(sha.len(), 64);
        assert!(
            sha.chars()
                .all(|c| c.is_ascii_hexdigit() && !c.is_uppercase())
        );
    }
}
