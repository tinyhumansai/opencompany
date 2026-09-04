//! Issue #553: the workspace refuses writes it cannot afford.
//!
//! A shared tree that agents can write video into needs a cap before it needs
//! anything else. The media tools spend real money and emit real megabytes, the
//! `shell` grant can write anything a script produces, and until now nothing
//! between an agent and the tenant's volume said no.
//!
//! ## Why a decorator, and not a check at the writers
//!
//! [`QuotaEnforcedWorkspace`] wraps the company's [`WorkspaceStore`], wrapped in
//! once by [`RuntimeBuilder`](crate::runtime::RuntimeBuilder) — the same shape
//! and the same argument as [`WorkspaceAnnouncer`](super::WorkspaceAnnouncer).
//! Every writer already goes through this port, so the limit is enforced **once,
//! where bytes are actually stored**, rather than once per call site that
//! happens to store some. A check at each writer is the same shape as the bug:
//! correct only for the paths somebody remembered, and absent for the next one
//! added. Here a new writer cannot forget, because it does not get a say.
//!
//! ## Refuse before, never clean up after
//!
//! The whole reason the port buffers writes rather than streaming them is this
//! check: the full size is known before a single byte reaches the store, so a
//! refusal leaves **nothing** behind — no partial blob, no node, no orphan for a
//! sweep to find. Enforcing mid-stream would mean writing until the limit is hit
//! and then unwinding, which is the failure mode the quota exists to prevent.
//!
//! ## What is counted, and what is not
//!
//! Only binary payloads. Prose notes are uncounted, and that is a deliberate
//! narrowing rather than an oversight: the threat model is media — a company
//! filling a volume with generated video — and a note is bounded by what a
//! model will emit into a tool call. Counting text would mean reading every
//! note's body on every write to total it, since the port does not carry a text
//! node's length. If notes ever become the pressure, `size` on a text node is
//! the thing to add, and this is the one place that would have to change.
//!
//! The total is computed from the tree's own metadata on each write, not
//! cached. That costs one `tree` call per binary write — the same call the
//! backends already make to validate a parent — and it cannot drift from what
//! is actually stored, which a counter maintained alongside the store could.

use std::sync::Arc;

use async_trait::async_trait;

use crate::error::{OpenCompanyError, Result};
use crate::ports::types::CompanyId;
use crate::ports::workspace::{BlobStream, WorkspaceNode, WorkspaceOrigin, WorkspaceStore};

/// The default cap on a single binary write: 64 MiB.
///
/// # Why there is a number here at all
///
/// Until this issue, MongoDB's 16 MB BSON document limit was an *accidental*
/// brake: a large payload physically could not be stored, so nothing had to
/// decide whether it should be. GridFS exists precisely to remove that limit by
/// chunking it away. The moment it lands, the only thing standing between a
/// retry-looping agent and unbounded writes into a hosted tenant's database is
/// a deliberate refusal — and there was none, and no lever for an operator to
/// see or set one. This constant is that refusal. It is cheap now and a
/// migration later.
///
/// # Why 64 MiB
///
/// Sized against what a binary node is *for*: the output of
/// `media_generate_image`, a `csv_export`, a PDF or spreadsheet a `shell` step
/// produced, a downloaded attachment. A generated image is single-digit
/// megabytes even at high resolution; a generated document rarely reaches ten.
/// 64 MiB clears all of those by an order of magnitude, so no legitimate
/// deliverable of the kinds this feature exists to make durable is anywhere
/// near it.
///
/// It deliberately is **not** sized down to image-only. Issue #553's stated
/// purpose is to stop a paid generation from becoming a dangling digest, and
/// `media_generate_video` is one of the paid tools named in it; a cap that
/// refused every short generated clip would re-create the exact bug this change
/// removes, one tier down. A few seconds of generated video lands in the low
/// tens of megabytes and fits.
///
/// # What it costs to hit it
///
/// A caller producing something genuinely larger — a long or high-bitrate
/// video, a multi-gigabyte dataset, a full disk image — is refused with the
/// limit and the attempted size named, and nothing is stored. The file stays in
/// the agent's sandbox and is not durable, which is the pre-#553 situation for
/// that one file. That is a real loss and it is the intended trade: the
/// alternative is an unbounded write path into a shared tenant database, where
/// the cost is borne by everyone rather than by the one oversized artifact.
///
/// Operators who need more set `[workspace] max_blob_mb`; the constant is only
/// the default.
///
/// # This is the *policy* limit, and not the transport one
///
/// The upload route reads up to [`UPLOAD_BODY_LIMIT_BYTES`], which is
/// deliberately larger. Until issue #647 the two were the same number, and that
/// made this constant unreachable through that route: the body limit always
/// tripped first, a truncated body is a parse failure, and so an oversized
/// upload answered `400 malformed multipart` — a request the operator had made
/// perfectly correctly, described as broken. The refusal below, which names the
/// file, its size and the cap, was written and could never be seen.
///
/// The two limits answer different questions — "may this company keep these
/// bytes?" versus "will this process read this many bytes at all?" — so they
/// hold different numbers, and the smaller one is the one that speaks.
pub const DEFAULT_MAX_BLOB_BYTES: u64 = 64 * 1024 * 1024;

/// The most an upload request body may weigh before the route stops reading it:
/// four times [`DEFAULT_MAX_BLOB_BYTES`], so 256 MiB.
///
/// # Why it is not the same number as the cap
///
/// A `DefaultBodyLimit` set *at* the per-file cap cannot produce a good error,
/// because it fires while the multipart body is still being parsed: the reader
/// sees a stream that stops mid-part, which is indistinguishable from a
/// malformed one at that layer. Only a limit the store's cap sits comfortably
/// *inside* lets the whole body arrive, the real size be measured, and the
/// refusal name it. Headroom is what buys the honest message (issue #647).
///
/// # Why 4×
///
/// The multiplier is what keeps [`crate::app::WorkspaceConfig`]'s
/// `[workspace] max_blob_mb` knob real. Routers are built once, before any
/// company exists, so the route cannot read a company's configured cap — it can
/// only leave room above the default for one. Anything a company raises the cap
/// to within 4× of the default is enforced exactly, by the store, with the
/// store's message. Beyond that the route refuses first — still a 413, still
/// naming a limit, but this one rather than the company's.
///
/// 256 MiB is also the number this route has claimed to allow since #553; the
/// comment said so while the code did not. This makes the published contract
/// true rather than quietly lowering it.
///
/// # What it costs
///
/// The upload path buffers (see the module docs on why refusing beats cleaning
/// up), so this is also the ceiling on how much one in-flight upload can hold
/// in memory. 256 MiB per concurrent upload is the deliberate trade for an
/// error message that tells the truth; it is not a licence to store that much,
/// which is still [`DEFAULT_MAX_BLOB_BYTES`]' decision.
pub const UPLOAD_BODY_LIMIT_BYTES: u64 = 4 * DEFAULT_MAX_BLOB_BYTES;

/// The limits a company's workspace is held to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WorkspaceQuota {
    /// The largest single payload that may be stored.
    pub max_blob_bytes: u64,
    /// The total binary payload one company may hold. `None` is unlimited.
    ///
    /// Unlimited is the default because a self-hosted operator's disk is their
    /// own business; the hosted platform sets it per tenant.
    pub tree_quota_bytes: Option<u64>,
}

impl Default for WorkspaceQuota {
    fn default() -> Self {
        Self {
            max_blob_bytes: DEFAULT_MAX_BLOB_BYTES,
            tree_quota_bytes: None,
        }
    }
}

impl WorkspaceQuota {
    /// Whether this quota would let anything through unchecked.
    ///
    /// Never true in practice — the per-file cap always applies — and present so
    /// the builder can skip the wrapper when a caller has explicitly disabled
    /// both limits.
    pub fn is_unlimited(&self) -> bool {
        self.max_blob_bytes == u64::MAX && self.tree_quota_bytes.is_none()
    }
}

/// Renders a byte count the way an operator reads one.
///
/// Crate-visible so the upload route's own refusal (issue #647) spells a limit
/// the same way this store's does. Two renderers would eventually disagree
/// about the same number, and the operator would have no way to tell that the
/// two messages were describing one thing.
pub(crate) fn human(bytes: u64) -> String {
    const MIB: f64 = 1024.0 * 1024.0;
    const GIB: f64 = MIB * 1024.0;
    let b = bytes as f64;
    if b >= GIB {
        format!("{:.1} GiB", b / GIB)
    } else if b >= MIB {
        format!("{:.1} MiB", b / MIB)
    } else {
        format!("{bytes} bytes")
    }
}

/// A [`WorkspaceStore`] that refuses a binary write exceeding its limits.
///
/// Reads and every text operation pass straight through. See the module docs
/// for why this lives at the store rather than at the callers.
pub struct QuotaEnforcedWorkspace {
    inner: Arc<dyn WorkspaceStore>,
    quota: WorkspaceQuota,
}

impl QuotaEnforcedWorkspace {
    /// Wraps `inner`, holding it to `quota`.
    pub fn new(inner: Arc<dyn WorkspaceStore>, quota: WorkspaceQuota) -> Self {
        Self { inner, quota }
    }

    /// The bytes this company's binary nodes already occupy.
    ///
    /// `excluding` is the node a replacement is about to overwrite: its current
    /// payload is being freed by the same operation, so counting it would make a
    /// company at its limit unable to replace a file with a smaller one.
    async fn used_bytes(&self, company: &CompanyId, excluding: Option<&str>) -> Result<u64> {
        Ok(self
            .inner
            .tree(company)
            .await?
            .into_iter()
            .filter(|node| Some(node.id.as_str()) != excluding)
            .filter_map(|node| node.size)
            .sum())
    }

    /// The per-file half of the quota, on its own.
    ///
    /// Written once and called twice — by [`admit`](Self::admit) and by
    /// [`admit_upload`](WorkspaceStore::admit_upload) — for the reason
    /// [`human`] is shared: an operator who hits the same cap through the
    /// upload route and through a binary write is hitting one limit, and two
    /// copies of the sentence are two chances for the two paths to start
    /// describing it differently.
    fn admit_single(&self, name: &str, len: u64) -> Result<()> {
        if len > self.quota.max_blob_bytes {
            return Err(OpenCompanyError::WorkspaceQuota(format!(
                "`{name}` is {}, over the {} limit for a single file. Nothing was stored.",
                human(len),
                human(self.quota.max_blob_bytes),
            )));
        }
        Ok(())
    }

    /// Refuses `len` bytes when either limit would be broken.
    ///
    /// Runs before any call into the inner store, which is what makes a refusal
    /// leave the tree untouched.
    async fn admit(
        &self,
        company: &CompanyId,
        name: &str,
        len: u64,
        replacing: Option<&str>,
    ) -> Result<()> {
        self.admit_single(name, len)?;
        let Some(limit) = self.quota.tree_quota_bytes else {
            return Ok(());
        };
        let used = self.used_bytes(company, replacing).await?;
        // Saturating: a limit lowered below what is already stored must refuse
        // cleanly rather than overflow into accepting the write.
        if used.saturating_add(len) > limit {
            return Err(OpenCompanyError::WorkspaceQuota(format!(
                "storing `{name}` ({}) would put this company's workspace over its {} limit — \
                 {} of it is already in use. Nothing was stored; delete something first.",
                human(len),
                human(limit),
                human(used),
            )));
        }
        Ok(())
    }
}

#[async_trait]
impl WorkspaceStore for QuotaEnforcedWorkspace {
    /// The upload route's question, answered with this company's own limits
    /// (issue #665).
    ///
    /// Only [`admit`](Self::admit)'s per-file half applies: `tree_quota_bytes`
    /// totals `size`, which only a binary node carries, so a payload about to be
    /// stored as prose contributes nothing to the tree total and must not be
    /// measured against it. That is the documented narrowing above, unchanged —
    /// this bounds a single upload, it does not start counting notes.
    async fn admit_upload(&self, _company: &CompanyId, name: &str, len: u64) -> Result<()> {
        self.admit_single(name, len)
    }

    async fn tree(&self, company: &CompanyId) -> Result<Vec<WorkspaceNode>> {
        self.inner.tree(company).await
    }

    async fn read(&self, company: &CompanyId, id: &str) -> Result<Option<(WorkspaceNode, String)>> {
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

    async fn is_empty(&self, company: &CompanyId) -> Result<bool> {
        self.inner.is_empty(company).await
    }

    /// Unmetered — see the module docs on what is counted. A text write also
    /// cannot land on a binary node at all, so it can never grow a payload.
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

    /// Unmetered, like every other folder write: quota counts binary payloads,
    /// and a folder carries none. Delegated rather than defaulted because the
    /// port has no default — a wrapper that silently read-then-created would
    /// reintroduce the race (issue #759) from inside the decorator stack.
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
        self.admit(company, &node.name, bytes.len() as u64, None)
            .await?;
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
        // Named by id here rather than by the node's name: fetching the node to
        // get its name would cost a read on every replacement to improve one
        // error message. The id is what the caller passed and can act on.
        self.admit(company, id, bytes.len() as u64, Some(id))
            .await?;
        self.inner
            .write_binary(company, id, bytes, mime, author)
            .await
    }

    async fn read_bytes(
        &self,
        company: &CompanyId,
        id: &str,
    ) -> Result<Option<(WorkspaceNode, BlobStream)>> {
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

    /// The staged create has already paid the only quota charge this shape
    /// change can add. Workspace quota counts binary payloads only, and a
    /// shape-changing swap has exactly one binary side, so forwarding the swap
    /// cannot transiently double-count two blobs.
    ///
    /// A first publish (issue #697, `expected_id` of `None`) is charged the
    /// same way and needs no separate accounting: its staged create paid, and
    /// there is no superseded side at all. A publisher that *loses* that race
    /// has its stage consumed by the store, so the charge is released rather
    /// than stranded on bytes nothing can reach.
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

    /// Forwards to `self.inner.delete_if_empty` rather than the default trait
    /// method — quota has nothing to say about a delete, but the default
    /// would otherwise resolve `tree()`/`delete()` back through this
    /// decorator as two separate calls and lose whatever tighter guarantee
    /// the wrapped store provides. See the port doc.
    async fn delete_if_empty(&self, company: &CompanyId, id: &str) -> Result<bool> {
        self.inner.delete_if_empty(company, id).await
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::ports::workspace::NodeKind;
    use crate::store::FsOps;

    fn wired(quota: WorkspaceQuota) -> (tempfile::TempDir, QuotaEnforcedWorkspace, CompanyId) {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = QuotaEnforcedWorkspace::new(Arc::new(FsOps::new(dir.path())), quota);
        (dir, store, CompanyId::new("quota-co"))
    }

    fn png(id: &str, name: &str) -> WorkspaceNode {
        WorkspaceNode {
            id: id.to_string(),
            name: name.to_string(),
            kind: NodeKind::File,
            parent_id: None,
            updated_at_millis: 1,
            created_by: WorkspaceOrigin::Operator,
            updated_by: WorkspaceOrigin::Operator,
            mime: Some("image/png".to_string()),
            size: None,
            sha256: None,
            adopted: false,
        }
    }

    /// The per-file cap refuses, names both numbers, and says nothing was
    /// stored — the sentence an operator has to act on.
    #[tokio::test]
    async fn a_file_over_the_per_file_cap_is_refused_by_name_and_size() {
        let (_dir, store, co) = wired(WorkspaceQuota {
            max_blob_bytes: 10,
            tree_quota_bytes: None,
        });

        let err = store
            .create_binary(&co, &png("big", "hero.png"), &[0u8; 64])
            .await
            .expect_err("over the per-file cap");
        let msg = err.to_string();
        assert!(msg.contains("hero.png"), "{msg}");
        assert!(msg.contains("64 bytes"), "{msg}");
        assert!(msg.contains("10 bytes"), "{msg}");
        assert!(msg.contains("Nothing was stored"), "{msg}");
    }

    /// A refused write leaves **nothing** behind. This is the property the
    /// buffered-write design exists to make true, so it is asserted against a
    /// real store rather than argued for in a comment.
    #[tokio::test]
    async fn a_refused_write_leaves_the_tree_exactly_as_it_was() {
        let (_dir, store, co) = wired(WorkspaceQuota {
            max_blob_bytes: 10,
            tree_quota_bytes: None,
        });
        store
            .create_binary(&co, &png("ok", "small.png"), b"12345")
            .await
            .unwrap();
        let before = store.tree(&co).await.unwrap();

        assert!(
            store
                .create_binary(&co, &png("big", "hero.png"), &[0u8; 64])
                .await
                .is_err()
        );

        let after = store.tree(&co).await.unwrap();
        assert_eq!(after.len(), before.len(), "no node was created");
        assert!(after.iter().all(|n| n.id != "big"));
        assert!(
            store.read_bytes(&co, "big").await.unwrap().is_none(),
            "and no payload was left behind for a sweep to find"
        );
    }

    /// The tree quota totals what is stored and refuses the write that would
    /// cross it, naming the usage so the operator knows what to delete.
    #[tokio::test]
    async fn the_tree_quota_counts_what_is_stored_and_names_the_usage() {
        let (_dir, store, co) = wired(WorkspaceQuota {
            max_blob_bytes: DEFAULT_MAX_BLOB_BYTES,
            tree_quota_bytes: Some(100),
        });
        store
            .create_binary(&co, &png("a", "a.png"), &[0u8; 60])
            .await
            .unwrap();
        // 60 + 30 fits.
        store
            .create_binary(&co, &png("b", "b.png"), &[0u8; 30])
            .await
            .unwrap();
        // 90 + 20 does not.
        let err = store
            .create_binary(&co, &png("c", "c.png"), &[0u8; 20])
            .await
            .expect_err("over the tree quota");
        let msg = err.to_string();
        assert!(msg.contains("c.png"), "{msg}");
        assert!(msg.contains("90 bytes"), "already-used total: {msg}");
        assert!(msg.contains("100 bytes"), "the limit: {msg}");

        // Freeing space makes the same write succeed — the total is read from
        // the tree, not from a counter that could drift.
        assert!(store.delete(&co, "b").await.unwrap());
        store
            .create_binary(&co, &png("c", "c.png"), &[0u8; 20])
            .await
            .expect("60 + 20 fits under 100");
    }

    /// Replacing a payload does not double-count the one it replaces: a company
    /// at its limit must still be able to swap a file for a smaller one.
    #[tokio::test]
    async fn replacing_a_payload_frees_the_bytes_it_overwrites() {
        let (_dir, store, co) = wired(WorkspaceQuota {
            max_blob_bytes: DEFAULT_MAX_BLOB_BYTES,
            tree_quota_bytes: Some(100),
        });
        store
            .create_binary(&co, &png("a", "a.png"), &[0u8; 95])
            .await
            .unwrap();

        // Naively 95 + 90 = 185 > 100; correctly the old 95 is being freed.
        store
            .write_binary(&co, "a", &[0u8; 90], None, WorkspaceOrigin::Operator)
            .await
            .expect("a replacement is measured against the tree without the node it replaces");

        // …but a replacement that is genuinely too big is still refused.
        assert!(
            store
                .write_binary(&co, "a", &[0u8; 120], None, WorkspaceOrigin::Operator)
                .await
                .is_err()
        );
    }

    /// Prose is uncounted, by design — see the module docs. Pinned so the
    /// narrowing is a decision on record rather than something a later reader
    /// has to infer from the absence of a check.
    #[tokio::test]
    async fn text_notes_are_not_metered() {
        let (_dir, store, co) = wired(WorkspaceQuota {
            max_blob_bytes: 10,
            tree_quota_bytes: Some(10),
        });
        let note = WorkspaceNode {
            mime: None,
            ..png("note", "brief.md")
        };
        store
            .create(&co, &note, Some(&"x".repeat(5_000)))
            .await
            .expect("a note is not measured against the blob quota");
        store
            .write(&co, "note", &"y".repeat(9_000), WorkspaceOrigin::Operator)
            .await
            .expect("nor is an overwrite");
    }

    /// The upload-only admission seam uses the company's configured per-file
    /// cap without applying the binary-tree total to prose that remains
    /// deliberately uncounted.
    #[tokio::test]
    async fn upload_admission_uses_the_custom_cap_but_not_the_tree_total() {
        let (_dir, store, co) = wired(WorkspaceQuota {
            max_blob_bytes: 64,
            tree_quota_bytes: Some(1),
        });

        store
            .admit_upload(&co, "at.csv", 64)
            .await
            .expect("exactly at the configured per-file cap is admitted");
        let err = store
            .admit_upload(&co, "over.csv", 65)
            .await
            .expect_err("one byte over the configured cap is refused");
        let message = err.to_string();
        assert!(message.contains("over.csv"), "{message}");
        assert!(message.contains("65 bytes"), "{message}");
        assert!(message.contains("64 bytes"), "{message}");
    }

    /// The digest is the store's, not the caller's — the decorator forwards
    /// bytes and never a claim about them, so a lying caller changes nothing.
    #[tokio::test]
    async fn the_stored_digest_is_computed_from_the_bytes_not_taken_from_the_caller() {
        let (_dir, store, co) = wired(WorkspaceQuota::default());
        let lying = WorkspaceNode {
            size: Some(1),
            sha256: Some("deadbeef".to_string()),
            adopted: false,
            ..png("img", "hero.png")
        };
        store
            .create_binary(&co, &lying, b"real-bytes")
            .await
            .unwrap();

        let (node, _) = store.read_bytes(&co, "img").await.unwrap().unwrap();
        let (size, sha) = crate::ports::workspace::blob_metadata(b"real-bytes");
        assert_eq!(node.size, Some(size));
        assert_eq!(node.sha256.as_deref(), Some(sha.as_str()));
    }

    /// The default admits an ordinary upload and still caps a runaway one.
    #[tokio::test]
    async fn the_default_quota_caps_per_file_and_leaves_the_tree_unlimited() {
        let quota = WorkspaceQuota::default();
        assert_eq!(quota.max_blob_bytes, DEFAULT_MAX_BLOB_BYTES);
        assert_eq!(quota.tree_quota_bytes, None);
        assert!(!quota.is_unlimited());
    }

    /// The boundary, from both sides.
    ///
    /// An off-by-one here is not cosmetic: one direction refuses a deliverable
    /// that is exactly at the documented limit, and the other is the first byte
    /// of an unbounded write path. Asserted on the decorator rather than by
    /// storing 64 MiB three times, because the comparison *is* the logic —
    /// everything below it is the backends' round-trip, which
    /// `assert_workspace_binary_store` already pins with a 17 MiB payload.
    #[tokio::test]
    async fn a_write_at_the_cap_is_admitted_and_one_byte_over_is_refused() {
        let (_dir, store, co) = wired(WorkspaceQuota {
            max_blob_bytes: 64,
            tree_quota_bytes: None,
        });

        store
            .create_binary(&co, &png("at", "at.png"), &[0u8; 64])
            .await
            .expect("exactly at the cap must be stored");

        let err = store
            .create_binary(&co, &png("over", "over.png"), &[0u8; 65])
            .await
            .expect_err("one byte over must be refused");
        // Actionable: what was attempted and what is allowed. A truncation
        // would be worse than either — a truncated binary is a corrupt binary,
        // and it would carry a digest the store computed over the wrong bytes.
        let msg = err.to_string();
        assert!(msg.contains("65 bytes"), "the attempted size: {msg}");
        assert!(msg.contains("64 bytes"), "the limit: {msg}");
        assert!(msg.contains("Nothing was stored"), "{msg}");

        // The refusal stored nothing — not a truncated 64-byte node.
        assert!(store.read_bytes(&co, "over").await.unwrap().is_none());
        // …and the replacement path enforces the same boundary.
        assert!(
            store
                .write_binary(&co, "at", &[0u8; 65], None, WorkspaceOrigin::Operator)
                .await
                .is_err(),
            "the cap is per write, so a replacement is capped too"
        );
    }
}
