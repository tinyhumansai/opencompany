//! `publish_artifact`: the **only** way a workspace file becomes a deliverable
//! (issue #244).
//!
//! # The rule
//!
//! An artifact exists **iff** the agent explicitly called [`PUBLISH_ARTIFACT_TOOL`]
//! on a file inside its own workspace, during a run that reached its success
//! terminal. A run that publishes nothing yields **no artifact** — an honest
//! first-class state whose addressable record is the run trace.
//!
//! Two things this replaces, and why each had to go:
//!
//! * **No auto-sweep.** The sandbox also hosts exec-grade shell and code tools,
//!   so it routinely contains repositories, caches and build output. Promoting
//!   whatever changed would flood the deliverable list with junk and poison the
//!   churn signal that is the whole point of the artifact port. An explicit call
//!   also carries intent — title, kind, note — which a sweep cannot invent.
//! * **No implicit reply capture.** A completed dispatch used to record its chat
//!   reply as an `ArtifactKind::Text` artifact, gated on run disposition and
//!   never on content. An agent answering *"I can't do this, I'm blocked on the
//!   API key"* still produced a versioned artifact indistinguishable from a real
//!   draft, so the Artifacts tab presented refusals as deliverables. Removing
//!   capture loses nothing: the reply already lands in the chat bubble, the
//!   timeline event, the terminal anchor, the card note and the run trace. It is
//!   five times recorded and zero times a deliverable.
//!
//! **No content classifier, anywhere.** Refusal-detection heuristics over prose
//! were considered and rejected: they are a guess about meaning, they fail
//! silently in both directions, and the honest signal — *did the agent publish
//! anything?* — is already available and exact.
//!
//! # Why the tool stages instead of writing
//!
//! Tools are built once per agent; the card varies per dispatch. So the tool
//! cannot hold a task id or an artifact store. It pushes a [`PendingPublish`]
//! onto the shared [`PendingPublishQueue`] — the [`McpFailureQueue`] pattern —
//! which the brain drains inside the completion path where it already holds the
//! card, the responder and the store. One write site, and "the queue is empty"
//! doubles as the detection signal the nudge reads.
//!
//! # Why staging must be *claimed* (issue #445)
//!
//! Staging has one failure mode, and it is the worst kind: a caller for whom
//! **nothing drains**. The tool returned success and named a destination
//! regardless, so a publish made during a chat turn was staged, never drained
//! (no task settles), then cleared by the next turn — a silent no-op reported
//! as a delivered deliverable. The agent then told the operator the file was
//! ready, because it had been told exactly that. A tool that cannot fail
//! launders the failure through the agent into a confident falsehood.
//!
//! So the queue carries a [`PublishDestination`] alongside the staged items,
//! and a drain site **claims** it — [`PendingPublishQueue::claim`] — for the
//! span in which it promises to drain. The claim is what the receipt is written
//! from, so the sentence the agent reads describes that caller's actual
//! destination rather than one case's sentence reused everywhere.
//!
//! The default is [`PublishDestination::Unclaimed`], and that direction is the
//! whole guarantee. A turn run from a path that has not claimed a destination —
//! including one written later, by someone who never read this module — gets an
//! honest in-turn refusal instead of a success receipt nothing will honour. The
//! invariant is enforced by construction rather than by remembering: *no claim,
//! no publish*. It generalizes [`build_agent`]'s existing fail-closed gate (an
//! agent with no artifact store is not offered the tool at all) from build time,
//! where it could only ask "could anything ever drain?", to call time, where it
//! can ask the question that actually matters — "will anything drain *this*?"
//!
//! [`build_agent`]: crate::harness::build::build_agent
//!
//! # What is validated, and when
//!
//! Everything, at `execute()` time, so the agent gets a truthful in-turn error
//! it can act on rather than a silent drop discovered by nobody:
//!
//! * the path resolves **inside the agent's workspace** — canonicalized, then
//!   prefix-checked against the canonicalized workspace, which is what makes it
//!   symlink-escape safe (a `..` check on the literal string is not);
//! * it exists and is a regular file (a directory is a different mistake and
//!   gets a different message);
//! * its size and UTF-8-ness are probed **at publish time**, and the stored body
//!   is computed then and there. A file that is later rewritten by a shell step
//!   cannot make the success message a liar, because the message describes what
//!   was captured, not what is on disk now.
//!
//! [`McpFailureQueue`]: crate::harness::mcp_probe::McpFailureQueue

use std::collections::BTreeMap;
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use serde_json::{Value, json};

use openhuman_core::openhuman as oh;

use oh::tools::traits::{PermissionLevel, Tool, ToolResult};

use crate::ports::artifacts::ArtifactKind;

/// Tool name: promote a workspace file to a versioned deliverable.
pub const PUBLISH_ARTIFACT_TOOL: &str = "publish_artifact";

/// The largest body stored inline on the artifact chain, in bytes.
///
/// Text at or under this is stored whole, as prose. Anything over it — or
/// anything that is not UTF-8 — is stored as **bytes** in a binary workspace
/// node instead (issue #553), never as silently-truncated content presented as
/// complete.
///
/// This decides *how* a deliverable is stored, not whether it survives. Before
/// #553 it decided the latter: over-cap meant a reference record pointing into
/// the agent's sandbox, which is exactly the payload that a wipe made
/// unreachable.
pub const MAX_ARTIFACT_BODY_BYTES: usize = 256 * 1024;

/// Directory names the workspace scan never descends into.
///
/// Two families, and the second one is not optional.
///
/// **Dependency and build trees** — `node_modules`, `target`. The sandbox hosts
/// shell and code tools, so a single `npm install` or `cargo build` puts tens of
/// thousands of entries under it.
///
/// **The runtime's own bookkeeping** — `sessions`, `session_raw`, `artifacts`,
/// `checkpoints`, `tinyagents_store`. The agent's `workspace_dir` is *also*
/// where OpenHuman writes its session transcripts (`sessions/<date>/*.md`,
/// `session_raw/*.jsonl`), its own artifact store and its subagent checkpoints,
/// and where TinyAgents writes its message journal
/// (`tinyagents_store/journal/session.*.messages.jsonl`). Those are written on
/// **every single run**, by the harness rather than by the agent, so without this the
/// scan would report unpublished changes after every dispatch and the nudge
/// would fire every time — asking an agent whether its own transcript is a
/// deliverable. That is not a tuning detail; it is the difference between a
/// feature and a permanent false positive.
const SCAN_SKIP_DIRS: [&str; 7] = [
    "node_modules",
    "target",
    "sessions",
    "session_raw",
    "artifacts",
    "checkpoints",
    "tinyagents_store",
];

/// File names the scan ignores wherever they appear.
///
/// `audit.log` **used to be** the per-workspace shell audit trail: the `shell`
/// toolbelt wrote `<workspace>/audit.log` (`AuditConfig::log_path`), so any agent
/// granted shell rewrote it on every run and the scan would have nudged about it
/// after every dispatch. Issue #775 moved that sink out of the workspace
/// entirely, to the host-owned `companies/<slug>/audit/<agent>/`, so the original
/// reason no longer applies to a workspace created after that change.
///
/// The entry stays anyway, for two reasons that outlive the move: a workspace
/// provisioned before it still holds the legacy file, and `audit.log` is a
/// plausible name for something else to write. Neither is ever a deliverable.
///
/// `STYLE.md`, `SOUL.md`, `IDENTITY.md` and `ROLE.md` are the vendored
/// OpenHuman prompt builder's own workspace seed files (openhuman#5701):
/// `sync_workspace_file` writes the compiled-in default for each of them into
/// the agent's workspace on **every prompt build** whose section runs, purely
/// so the file exists on disk to edit — the agent never asked for it and never
/// touched it. `STYLE.md` in particular is synced unconditionally by
/// `global_style_block` regardless of `omit_identity`, specifically so an
/// identity-omitted agent (the orchestrator among them) still gets style rules
/// — so a card dispatched to a brand-new workspace sees `STYLE.md` appear
/// between the pre-turn snapshot and the post-turn scan on its very first
/// turn, with nothing the agent did producing it. Before this entry, that read
/// as an unpublished deliverable and fired the "did you mean to publish this?"
/// nudge as a second, uncounted model call on a turn that delegated or
/// produced nothing — exactly the `audit.log` false positive above, for a file
/// the harness itself just started writing. Their `.builtin-hash` sidecars
/// need no entry of their own; `is_hidden` already skips any dot-prefixed name.
const SCAN_SKIP_FILES: [&str; 5] = ["audit.log", "STYLE.md", "SOUL.md", "IDENTITY.md", "ROLE.md"];

/// Whether a directory entry is hidden, and therefore skipped.
///
/// A blanket dot-prefix rule rather than an enumeration, because the runtime
/// keeps growing dot-paths under the workspace — `.git`, `.openhuman/`
/// (subagent checkpoints), `.runs/`, `.env`, `.memory-write.lock` — and an
/// enumeration would go stale silently, in the direction of false positives.
/// Nothing an agent is asked to deliver is a dotfile.
fn is_hidden(name: &str) -> bool {
    name.starts_with('.')
}

/// Hard ceiling on entries one workspace scan visits.
///
/// The scan is a *detection* aid feeding a warning, never a promotion — so
/// running out of budget degrades to "we may have missed something", which is
/// the correct failure direction for a heuristic. An unbounded walk of an
/// exec-grade sandbox is not.
const MAX_SCAN_ENTRIES: usize = 5_000;

/// How many changed file names the nudge and the fallback warning name.
///
/// Enough to be actionable, few enough that a build directory that slipped past
/// the skip list cannot turn one prompt into a directory listing.
const MAX_NAMED_FILES: usize = 20;

// ---------------------------------------------------------------------------
// The staged publish
// ---------------------------------------------------------------------------

/// One `publish_artifact` call, captured at the moment it was made.
///
/// `body` is computed **here**, not later: the whole point of validating at
/// execute time is that what the agent was told it published is what gets
/// stored.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PendingPublish {
    /// The agent that made the call (issue #463).
    ///
    /// The queue is shared by every turn a cycle runs, so "who published this"
    /// is not answerable from the drain site: an operator message can be
    /// answered by the orchestrator and handed to a desk, and the file comes
    /// from whichever of them reached for the tool. Recording it at the call —
    /// the tool is bound to exactly one agent's sandbox — is what stops the
    /// card and the artifact being filed under the turn's responder when
    /// somebody else did the work.
    ///
    /// Empty only for a value built by hand outside the tool; callers fall back
    /// to the responder in that case rather than writing a blank author.
    pub agent: String,
    /// The normalized workspace-relative path — the artifact's identity
    /// alongside its task id.
    pub source: String,
    /// The operator-facing title. Defaults to the file's name.
    pub title: String,
    /// What it holds, from the `kind` argument or inferred from the extension.
    pub kind: ArtifactKind,
    /// Why this revision exists, if the agent said.
    pub note: Option<String>,
    /// What was captured: the file's text, or its bytes (issue #553).
    pub payload: PublishPayload,
}

/// Where the publishes staged on a queue are going to be recorded — and
/// therefore what the tool is entitled to tell the agent (issue #445).
///
/// Read at `execute()` time, so the receipt describes the caller that made the
/// call. The variants are the *reachable destinations*, not the call sites: two
/// paths that file into the same place share one variant, because the agent's
/// receipt is about where its file lands, not about which function ran it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum PublishDestination {
    /// Nothing will drain. **The default, and deliberately so.**
    ///
    /// Any turn whose caller has not claimed a destination lands here and the
    /// tool refuses in-turn. That is the fail-safe direction: a new turn-running
    /// path added later inherits an honest refusal rather than the silent drop
    /// that made #445 a lie told through the agent.
    #[default]
    Unclaimed,
    /// A dispatched card's settle path drains this, filing each publish on that
    /// card. The pre-#445 behaviour, unchanged.
    Task,
    /// A conversation turn drains this, and publishing **mints the card** that
    /// carries the artifact — a chat deliverable is real work, so it gets the
    /// board record the console can already open.
    Conversation,
}

impl PublishDestination {
    /// The sentence appended to a success receipt, or `None` when no publish
    /// can succeed here at all.
    ///
    /// Returning `None` rather than a "sorry" string is what keeps the refusal
    /// path and the success path from ever being confused: there is no receipt
    /// to render, so the caller is forced to produce a tool **error** instead.
    fn receipt_tail(self) -> Option<&'static str> {
        match self {
            Self::Unclaimed => None,
            Self::Task => Some("It appears on this task's Artifacts tab when the run finishes."),
            Self::Conversation => Some(
                "Because this is a conversation and not a task, it is filed on a new board card \
                 for this conversation when your turn finishes — the operator opens it from that \
                 card's Artifacts tab.",
            ),
        }
    }
}

/// The agent-facing refusal when nothing is in a position to record a publish.
///
/// It has to do two jobs. It must not read as a transient glitch worth
/// retrying, and it must tell the agent what to say next — because the failure
/// this replaces was one the agent could not detect, and an agent that thinks it
/// published will report a delivery that did not happen.
fn cannot_publish_here(path: &str) -> String {
    format!(
        "`{path}` was NOT published: nothing here can record a deliverable, so publishing is \
         unavailable in this context. Do not retry — it will fail the same way, and do not tell \
         anyone the file was delivered. The file is still in your sandbox; say plainly that you \
         could not publish it."
    )
}

/// A shared, in-memory queue of staged publishes — the exact
/// [`McpFailureQueue`](crate::harness::mcp_probe::McpFailureQueue) pattern.
///
/// Cheap to [`Clone`] (a shared handle); the tool built into the agent and the
/// brain that drains it see the same queue because
/// [`HarnessDeps`](crate::harness::HarnessDeps) clones share this handle.
///
/// The destination (#445) rides the **same handle** rather than sitting beside
/// it in `HarnessDeps`, which is not a tidiness choice: `build_agent` hands the
/// tool this one clone and nothing else, so carrying the claim here makes it
/// impossible to wire a tool that cannot see where its publishes are going.
#[derive(Clone, Default)]
pub struct PendingPublishQueue {
    inner: Arc<Mutex<Vec<PendingPublish>>>,
    /// Publishes that were **refused** because nothing here could record one
    /// (issue #1192) — the source path, recorded at the moment the refusal is
    /// raised.
    ///
    /// A second bucket rather than a variant in `inner`, and the separation is
    /// the load-bearing part: a refused file is by definition **not** staged,
    /// so it must not be visible to [`sources`](Self::sources). See that
    /// method's note.
    refusals: Arc<Mutex<BTreeMap<PublishRefusalScope, Vec<String>>>>,
    destination: Arc<Mutex<PublishDestination>>,
}

impl PendingPublishQueue {
    /// Stages a publish.
    pub fn push(&self, publish: PendingPublish) {
        self.inner.lock().expect("publish queue").push(publish);
    }

    /// Records that a publish of `source` was **refused** because nothing in
    /// this context can record a deliverable (issue #1192).
    ///
    /// Written at the one site that raises the refusal, so what a caller later
    /// reports is a fact the tool produced rather than an inference drawn from
    /// its prose. Matching on
    /// [`cannot_publish_here`]'s wording would be the same drift trap a
    /// classifier keyed on a `Display` string always is: the sentence is
    /// agent-facing copy and will be reworded, and the day it is, the operator's
    /// notice silently stops appearing with every test still green.
    pub fn push_refusal(&self, source: String) {
        self.refusals
            .lock()
            .expect("publish refusals")
            .entry(Self::current_refusal_scope())
            .or_default()
            .push(source);
    }

    /// Takes every refusal recorded so far (FIFO), emptying the bucket.
    ///
    /// Drained per turn by whichever caller is in a position to tell somebody —
    /// today
    /// [`HarnessAgentRunner`](crate::workflows::caps::HarnessAgentRunner), which
    /// turns each one into a run notice because a workflow run has no
    /// conversation to say it in.
    pub fn drain_refusals(&self) -> Vec<String> {
        let mut guard = self.refusals.lock().expect("publish refusals");
        guard
            .remove(&Self::current_refusal_scope())
            .unwrap_or_default()
    }

    /// Claims one workflow run's refusal bucket.
    ///
    /// `PublishArtifactTool` is constructed once for a cached roster agent, so
    /// it cannot carry a run id. The task-local scope lets its live refusal
    /// write and this run's drain meet on the same shared queue handle.
    #[must_use = "the claim discards its run's refused publishes on drop"]
    pub fn claim_refusals_for_run(&self, run_id: impl Into<String>) -> PublishRefusalClaim {
        let scope = PublishRefusalScope::Run(run_id.into());
        self.clear_refusals_in(&scope);
        PublishRefusalClaim {
            queue: self.clone(),
            scope,
        }
    }

    /// Where staged publishes are currently headed.
    pub fn destination(&self) -> PublishDestination {
        *self.destination.lock().expect("publish destination")
    }

    /// Claims this queue for a drain site that promises to drain it, for as long
    /// as the returned [`PublishClaim`] lives.
    ///
    /// Clears on the way in for the reason the drain sites already cleared by
    /// hand — a prior turn's staged file must never be attributed to this
    /// caller — and, via [`PublishClaim`]'s `Drop`, on the way out too. The exit
    /// half is the one that is new and load-bearing: an early return, a `?`, or
    /// a panic mid-run used to leave items staged and the next caller to clear
    /// them, so correctness depended on every future path remembering. Now the
    /// claim's scope *is* the window in which publishing works.
    #[must_use = "the claim releases on drop; dropping it immediately un-claims the queue"]
    pub fn claim(&self, destination: PublishDestination) -> PublishClaim {
        self.clear();
        *self.destination.lock().expect("publish destination") = destination;
        PublishClaim {
            queue: self.clone(),
        }
    }

    /// Empties the queue. Called before each turn so nothing a prior turn — an
    /// operator chat turn earlier in the same cycle, or an abandoned redirect
    /// re-run — staged can be attributed to this card.
    ///
    /// Empties **both** buckets (issue #1192), for the reason the staged half is
    /// emptied: a redirect abandons the previous turn's work, and a refusal that
    /// turn provoked is part of the work being abandoned. Surfacing it on the
    /// re-run would report a refusal against a turn that never asked.
    pub fn clear(&self) {
        self.inner.lock().expect("publish queue").clear();
        self.clear_refusals_in(&Self::current_refusal_scope());
    }

    /// Drains every staged publish (FIFO), emptying the queue.
    pub fn drain(&self) -> Vec<PendingPublish> {
        let mut guard = self.inner.lock().expect("publish queue");
        std::mem::take(&mut *guard)
    }

    /// The paths staged so far, without draining.
    ///
    /// This is the nudge's whole gate: `changed − staged` is what the agent
    /// wrote and did not offer.
    ///
    /// **A refused publish is deliberately NOT in here** (issue #1192).
    /// Refusals live in their own bucket precisely so this list keeps meaning
    /// "offered and accepted". A file whose publish was refused is still
    /// unpublished — it is the file *most* at risk of being lost — so folding
    /// refusals in would make the #244 unpublished-work scan go quiet on exactly
    /// the case it exists for.
    pub fn sources(&self) -> Vec<String> {
        self.inner
            .lock()
            .expect("publish queue")
            .iter()
            .map(|p| p.source.clone())
            .collect()
    }

    /// How many publishes are staged.
    pub fn queued(&self) -> usize {
        self.inner.lock().expect("publish queue").len()
    }

    fn current_refusal_scope() -> PublishRefusalScope {
        CURRENT_REFUSAL_SCOPE
            .try_with(Clone::clone)
            .unwrap_or(PublishRefusalScope::Unscoped)
    }

    fn clear_refusals_in(&self, scope: &PublishRefusalScope) {
        self.refusals
            .lock()
            .expect("publish refusals")
            .remove(scope);
    }
}

/// Which turn owns a refused publish.
///
/// The queue handle is shared by cached roster tools, so workflow runs are
/// separated by an ambient key rather than by replacing that handle per run.
#[derive(Clone, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
enum PublishRefusalScope {
    /// Chat and task turns retain the historical company-wide bucket.
    #[default]
    Unscoped,
    /// One workflow run, keyed by its unique run id.
    Run(String),
}

tokio::task_local! {
    /// The workflow run whose refusal bucket the current task uses.
    static CURRENT_REFUSAL_SCOPE: PublishRefusalScope;
}

/// A workflow run's claim on its own refused-publish bucket.
pub struct PublishRefusalClaim {
    queue: PendingPublishQueue,
    scope: PublishRefusalScope,
}

impl PublishRefusalClaim {
    /// Runs `fut` in this claim's scope, routing refusal writes and drains to
    /// this run's bucket even though the tool itself belongs to a cached agent.
    pub async fn scoped<F, T>(&self, fut: F) -> T
    where
        F: std::future::Future<Output = T>,
    {
        CURRENT_REFUSAL_SCOPE.scope(self.scope.clone(), fut).await
    }
}

impl Drop for PublishRefusalClaim {
    fn drop(&mut self) {
        self.queue.clear_refusals_in(&self.scope);
    }
}

/// The live claim on a [`PendingPublishQueue`] — proof that some drain site is
/// listening (issue #445).
///
/// Held for the span in which a caller promises to drain; on `Drop` the queue
/// returns to [`PublishDestination::Unclaimed`] and is emptied, so publishing is
/// off again the moment that promise ends. Mirrors the RAII shape the in-flight
/// steer guard already uses in the brain, for the same reason: the cleanup has
/// to happen on **every** exit path, including the ones nobody wrote by hand.
///
/// Deliberately not [`Clone`] — two live claims would mean two owners of one
/// promise, and the second to drop would un-claim the queue underneath the
/// first.
pub struct PublishClaim {
    queue: PendingPublishQueue,
}

impl Drop for PublishClaim {
    fn drop(&mut self) {
        *self.queue.destination.lock().expect("publish destination") =
            PublishDestination::Unclaimed;
        self.queue.clear();
    }
}

// ---------------------------------------------------------------------------
// Path resolution
// ---------------------------------------------------------------------------

/// Why a path could not be published, phrased for the agent that sent it.
#[derive(Debug, PartialEq, Eq)]
pub enum PublishPathError {
    /// Blank, or nothing but separators.
    Empty,
    /// Absolute, or it climbed out of the workspace.
    Outside,
    /// Nothing is there.
    Missing,
    /// It is a directory, or a socket, or something else that is not a file.
    NotAFile,
}

impl PublishPathError {
    /// The agent-facing message. Names what to do next, because a tool error
    /// that only says "no" costs a whole turn to recover from.
    pub fn message(&self, path: &str) -> String {
        match self {
            Self::Empty => "`path` is required: give the path of the file you want to publish, \
                            relative to your sandbox, e.g. \"specs/launch.md\"."
                .to_string(),
            Self::Outside => format!(
                "`{path}` is outside your sandbox. Publish only files you wrote inside it, using \
                 a relative path like \"specs/launch.md\" — absolute paths and `..` are refused."
            ),
            Self::Missing => format!(
                "There is no file at `{path}` in your sandbox. Check the path with `list` or \
                 `glob`, and write the file before publishing it."
            ),
            Self::NotAFile => format!(
                "`{path}` is not a regular file. Publish one file at a time — a folder cannot be a \
                 deliverable."
            ),
        }
    }
}

/// Resolves an agent-supplied `path` to a real file inside `workspace`,
/// returning the canonical location and the normalized workspace-relative
/// `source`.
///
/// # Why canonicalize rather than string-check
///
/// A `..`-component check over the literal string is not containment: a symlink
/// inside the workspace pointing at `/etc` contains no `..` at all, and would
/// pass. Both sides are canonicalized — following every link — and then the
/// candidate is prefix-checked against the workspace. That mirrors the
/// `workspace_security` policy the file tools run under, which is the boundary
/// this tool must not be a hole in.
///
/// The `source` is rebuilt from the canonical pair rather than echoed from the
/// argument, so `./specs/../specs/launch.md` and `specs/launch.md` produce the
/// *same* identity and therefore extend the same artifact.
pub fn resolve_in_workspace(
    workspace: &Path,
    path: &str,
) -> Result<(PathBuf, String), PublishPathError> {
    let trimmed = path.trim();
    if trimmed.is_empty() {
        return Err(PublishPathError::Empty);
    }
    let candidate = Path::new(trimmed);
    if candidate.is_absolute() {
        return Err(PublishPathError::Outside);
    }
    // A prefix component (a Windows drive/UNC root) is as absolute as it gets.
    if candidate
        .components()
        .any(|c| matches!(c, Component::Prefix(_) | Component::RootDir))
    {
        return Err(PublishPathError::Outside);
    }

    // The workspace itself may not exist yet if the agent never wrote anything;
    // that is a missing file, not an escape.
    let root = workspace
        .canonicalize()
        .map_err(|_| PublishPathError::Missing)?;
    let resolved = root
        .join(candidate)
        .canonicalize()
        .map_err(|_| PublishPathError::Missing)?;
    // The containment check, after every symlink has been followed.
    let relative = resolved
        .strip_prefix(&root)
        .map_err(|_| PublishPathError::Outside)?;
    if !resolved.is_file() {
        return Err(PublishPathError::NotAFile);
    }
    let source = normalize_source(relative);
    if source.is_empty() {
        return Err(PublishPathError::Empty);
    }
    Ok((resolved, source))
}

/// Renders a workspace-relative path as the stable `source` string: forward
/// slashes on every platform, so an artifact's identity does not depend on the
/// host that produced it.
fn normalize_source(relative: &Path) -> String {
    relative
        .components()
        .filter_map(|c| match c {
            Component::Normal(part) => Some(part.to_string_lossy().into_owned()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("/")
}

// ---------------------------------------------------------------------------
// Kind + body
// ---------------------------------------------------------------------------

/// Infers what a file holds from its extension, for when the agent omits
/// `kind`.
///
/// Deliberately coarse. [`ArtifactKind`] drives the console's renderer choice,
/// and the only distinction that changes anything is "render this as markdown"
/// versus "this is a reference to something the browser cannot show inline".
pub fn kind_for_extension(path: &Path) -> ArtifactKind {
    let ext = path
        .extension()
        .map(|e| e.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default();
    match ext.as_str() {
        "md" | "markdown" | "mdx" => ArtifactKind::Markdown,
        "png" | "jpg" | "jpeg" | "gif" | "webp" | "svg" | "bmp" | "ico" | "avif" => {
            ArtifactKind::Image
        }
        "txt" | "text" | "log" | "csv" | "tsv" | "json" | "yaml" | "yml" | "toml" | "html"
        | "css" | "js" | "ts" | "rs" | "py" | "sh" | "sql" => ArtifactKind::Text,
        // Unknown, or no extension at all: a file, referenced not inlined.
        _ => ArtifactKind::File,
    }
}

/// Parses an explicit `kind` argument, accepting the wire words the enum
/// serializes to.
fn parse_kind(raw: &str) -> Option<ArtifactKind> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "text" => Some(ArtifactKind::Text),
        "markdown" | "md" => Some(ArtifactKind::Markdown),
        "image" => Some(ArtifactKind::Image),
        "file" => Some(ArtifactKind::File),
        _ => None,
    }
}

/// What was actually captured from a file: either its text, or a reference to
/// it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PublishPayload {
    /// UTF-8 prose, small enough to live inline on the artifact chain and to be
    /// stored as an editable, diffable, backlinkable note.
    Text(String),
    /// Opaque bytes, stored as a binary workspace node (issue #553).
    Bytes {
        /// The file's exact contents.
        bytes: Vec<u8>,
        /// The media type inferred from the file's extension.
        mime: String,
    },
}

/// What the workspace did with a published payload, as the artifact version has
/// to describe it (issues #663, #668).
///
/// Three states rather than a `bool` because "not stored yet" and "refused" are
/// different things to tell an operator, and collapsing them is how a record
/// came to assert storage that never happened.
#[derive(Debug, Clone, Copy)]
pub enum PayloadStorage<'a> {
    /// The store has not been asked yet — the body composed before the mirror.
    Pending,
    /// The store wrote it, and returned this digest (`None` for prose, or a
    /// backend that recorded none).
    Stored {
        /// The `sha256` **the store** computed. Never hashed on this side; see
        /// [`WorkspaceStore::create_binary`](crate::ports::workspace::WorkspaceStore::create_binary).
        sha256: Option<&'a str>,
    },
    /// The store refused it. The reason is logged, never written to the record.
    Refused,
}

impl PublishPayload {
    /// What the artifact chain records as this version's body.
    ///
    /// For prose, the prose. For bytes, a one-line description — because the
    /// version's real content is the workspace node the same publish creates,
    /// and the record points at it
    /// ([`stamp_workspace_node`](crate::ports::artifacts::ArtifactRecord::stamp_workspace_node)).
    /// That is issue #187's rule for a binary version: a reference to a node,
    /// never an inline body.
    ///
    /// **No digest here.** The store computes one from the same bytes when it
    /// writes the node, and a second hash on this path would be a second
    /// opportunity for the two to disagree about what was published.
    pub fn artifact_body(&self) -> String {
        self.artifact_body_for(PayloadStorage::Pending)
    }

    /// The version body for a payload whose storage outcome is `storage`
    /// (issues #663, #668).
    ///
    /// One function for all three wordings so they cannot drift into
    /// contradicting each other, which is the defect #663 is about: the body
    /// used to assert "stored as a file in the company workspace"
    /// unconditionally, and was composed *before* the store was asked. When the
    /// workspace refused the file, the record went on saying it was there.
    ///
    /// Prose ignores `storage` entirely: for text the version **is** the
    /// content, so it is complete whatever the tree does.
    pub fn artifact_body_for(&self, storage: PayloadStorage<'_>) -> String {
        match self {
            Self::Text(text) => text.clone(),
            Self::Bytes { bytes, mime } => {
                let head = format!("{mime}, {} bytes", bytes.len());
                match storage {
                    // Written before the store is asked, and true at that
                    // instant. It survives only if the process dies mid-drain;
                    // otherwise the outcome below replaces it.
                    PayloadStorage::Pending => format!(
                        "{head} — a binary payload being filed into the company workspace. This \
                         record is the version history, not the content."
                    ),
                    PayloadStorage::Stored { sha256: Some(sha) } => format!(
                        "{head}, sha256 {sha} — stored as a file in the company workspace. Open \
                         it there; this record is the version history, not the content."
                    ),
                    // A store that returned no digest: say so rather than
                    // implying the version is identified when it is not.
                    PayloadStorage::Stored { sha256: None } => format!(
                        "{head} — stored as a file in the company workspace, with no digest \
                         recorded. Open it there; this record is the version history, not the \
                         content."
                    ),
                    // Deliberately does NOT carry the store's error text. This
                    // string is permanent, and a backend error can name host
                    // paths; the operator needs to know the file is not there,
                    // and the diagnosis belongs in the log.
                    PayloadStorage::Refused => format!(
                        "{head} — NOT stored: the company workspace refused this file, so there \
                         is nothing to open there. This record is the version history, not the \
                         content."
                    ),
                }
            }
        }
    }

    /// The kind this payload forces, when it forces one.
    ///
    /// Bytes are never `Text`/`Markdown`: presenting them under a kind the
    /// console renders as prose would be a lie about what the operator is
    /// looking at. An image stays an image; anything else is a file.
    pub fn forced_kind(&self, inferred: ArtifactKind) -> ArtifactKind {
        match self {
            Self::Text(_) => inferred,
            Self::Bytes { .. } if matches!(inferred, ArtifactKind::Image) => ArtifactKind::Image,
            Self::Bytes { .. } => ArtifactKind::File,
        }
    }
}

/// Reads `file` into a payload, deciding text-versus-bytes **now**.
///
/// # The reference record is gone (issue #553)
///
/// This used to answer an over-cap or non-UTF-8 file with a structured
/// *reference* — path, size, sha256 — and the sentence "the file lives in the
/// agent's own sandbox. Wiping the sandbox leaves this record intact and the
/// payload unreachable." That was honest and useless: the most expensive thing
/// the product can produce, a paid image or video generation, became a dangling
/// digest pointing into a directory that gets wiped.
///
/// The workspace tree can hold bytes now, on every backend, so there is nothing
/// left for a fallback to fall back to and none is kept. Over-cap or non-UTF-8
/// simply means the payload is stored as bytes instead of as prose.
///
/// The cap still decides *how* a file is stored, not whether it survives: prose
/// under it stays a note — diffable, backlinkable, editable in the console —
/// and everything else becomes a binary node.
pub fn capture_body(
    file: &Path,
    source: &str,
    _inferred: ArtifactKind,
) -> std::io::Result<PublishPayload> {
    // Route by size before reading, so the read is bounded by the prose cap: a
    // file already past it goes straight to bytes, and a file within the cap is
    // read whole and probed in place.
    let over_cap = file
        .metadata()
        .map(|meta| meta.len() > MAX_ARTIFACT_BODY_BYTES as u64)
        .unwrap_or(false);
    let bytes = std::fs::read(file)?;
    if !over_cap && std::str::from_utf8(&bytes).is_ok() {
        // The borrowed probe validates in place; the move below reuses the same
        // buffer, so a file within the cap is never copied. The probe's `Err`
        // arm is unreachable here, hence the `expect` on a just-validated vec.
        return Ok(PublishPayload::Text(
            String::from_utf8(bytes).expect("probed utf-8 in place"),
        ));
    }
    Ok(PublishPayload::Bytes {
        mime: mime_guess::from_path(source)
            .first_raw()
            .unwrap_or("application/octet-stream")
            .to_string(),
        bytes,
    })
}

// ---------------------------------------------------------------------------
// The tool
// ---------------------------------------------------------------------------

/// Promotes one workspace file to a versioned deliverable, by staging it on the
/// shared queue the brain drains.
pub struct PublishArtifactTool {
    workspace: PathBuf,
    /// The agent this instance belongs to, stamped onto everything it stages
    /// (issue #463) — the tool is built per agent, so this is the one place the
    /// publisher's identity is known for certain.
    agent: String,
    queue: PendingPublishQueue,
}

impl PublishArtifactTool {
    /// Binds the tool to one agent, its workspace, and the shared queue.
    pub fn new(
        workspace: impl Into<PathBuf>,
        agent: impl Into<String>,
        queue: PendingPublishQueue,
    ) -> Self {
        Self {
            workspace: workspace.into(),
            agent: agent.into(),
            queue,
        }
    }
}

#[async_trait]
impl Tool for PublishArtifactTool {
    fn name(&self) -> &str {
        PUBLISH_ARTIFACT_TOOL
    }

    fn description(&self) -> &str {
        "Publish a file you wrote in your own sandbox as a deliverable. USE FOR the finished \
         output somebody asked for — a spec, a draft, a report, an invoice, an exported dataset. \
         Your sandbox is private to you and the operator cannot see into it, so publishing is the \
         only thing that hands a file over; republishing the same path later adds a version \
         rather than a duplicate. NOT for scratch files, notes to yourself, logs, or build \
         output, and NOT a way to send a message — your reply already reaches the operator. \
         Publishing nothing is a perfectly good outcome for work that produced no file."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Path of the file to publish, relative to your own sandbox, e.g. \"specs/launch.md\". Must be a file you wrote inside your sandbox — not a path in the company workspace, which is a different place you reach with the workspace tools."
                },
                "title": {
                    "type": "string",
                    "description": "Short operator-facing title. Defaults to the file name."
                },
                "kind": {
                    "type": "string",
                    "enum": ["text", "markdown", "image", "file"],
                    "description": "What the file holds. Inferred from the extension when omitted."
                },
                "note": {
                    "type": "string",
                    "description": "Optional one-line reason this version exists, e.g. \"rewrote the pricing section\"."
                }
            },
            "required": ["path"],
            "additionalProperties": false
        })
    }

    fn permission_level(&self) -> PermissionLevel {
        // Reads one file the agent already wrote and stages a record. It writes
        // nothing to the host and touches no other tenant.
        PermissionLevel::ReadOnly
    }

    async fn execute(&self, args: Value) -> anyhow::Result<ToolResult> {
        let raw_path = args.get("path").and_then(Value::as_str).unwrap_or_default();

        // Issue #445: can anything record a publish made from *this* turn?
        // Asked first, before the path is even resolved, because when the answer
        // is no it is the only fact that matters — validating a path we are not
        // going to publish would only produce a more specific way of being
        // unable to publish.
        let Some(receipt_tail) = self.queue.destination().receipt_tail() else {
            tracing::warn!(
                path = %raw_path.trim(),
                "[publish] `publish_artifact` was called from a turn with no claimed \
                 destination; refusing rather than staging into a queue nothing will drain"
            );
            // Issue #1192: record the refusal as a typed fact, here, where it is
            // raised. Before this the *only* record was the sentence below — the
            // model read it, wrote an apology about it, and that apology became
            // the node output while the run scored clean. A caller that can
            // reach an operator now has something structural to say instead.
            self.queue.push_refusal(raw_path.trim().to_string());
            return Ok(ToolResult::error(cannot_publish_here(raw_path.trim())));
        };

        let (file, source) = match resolve_in_workspace(&self.workspace, raw_path) {
            Ok(resolved) => resolved,
            Err(err) => return Ok(ToolResult::error(err.message(raw_path.trim()))),
        };

        let inferred = match args.get("kind").and_then(Value::as_str) {
            Some(raw) => match parse_kind(raw) {
                Some(kind) => kind,
                None => {
                    return Ok(ToolResult::error(format!(
                        "`{raw}` is not a kind. Use one of: text, markdown, image, file — or omit \
                         `kind` and it is inferred from the extension."
                    )));
                }
            },
            None => kind_for_extension(&file),
        };

        let payload = match capture_body(&file, &source, inferred) {
            Ok(payload) => payload,
            Err(err) => {
                return Ok(ToolResult::error(format!(
                    "Could not read `{source}`: {err}"
                )));
            }
        };
        let kind = payload.forced_kind(inferred);

        let title = args
            .get("title")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|t| !t.is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| {
                file.file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|| source.clone())
            });
        let note = args
            .get("note")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|n| !n.is_empty())
            .map(str::to_string);

        self.queue.push(PendingPublish {
            agent: self.agent.clone(),
            source: source.clone(),
            title: title.clone(),
            kind,
            note,
            payload: payload.clone(),
        });

        // The message describes what was **captured**, in the past tense,
        // because that is the only thing still true after a later shell step
        // rewrites the file.
        // Both arms are "captured in full" now: the reference record is gone
        // (issue #553), so nothing published is left pointing at the sandbox.
        let how = match &payload {
            PublishPayload::Text(_) => "captured in full".to_string(),
            PublishPayload::Bytes { bytes, .. } => {
                format!("captured in full as a {}-byte file", bytes.len())
            }
        };
        // The tail comes from the claim (#445), so the destination named is the
        // one this caller actually has. The single hard-coded task sentence is
        // what told a chat turn its file would appear on a run that was never
        // going to finish.
        Ok(ToolResult::success(format!(
            "Published `{source}` as \"{title}\" ({kind}) — {how}. {receipt_tail}",
            kind = kind.as_str()
        )))
    }
}

// ---------------------------------------------------------------------------
// The workspace scan
// ---------------------------------------------------------------------------

/// A bounded snapshot of a workspace's files by modification time.
///
/// Taken at dispatch start and diffed after the turn, this answers *"did the
/// agent write anything it did not publish?"* — the question the nudge and the
/// fallback warning are both built on.
///
/// It is a **detection aid**, never a promotion. mtime is a heuristic: a tool
/// side effect can move it, and a filesystem with coarse timestamps can hide a
/// same-second rewrite. Both failure modes cost at most a warning that is
/// slightly wrong, which is an acceptable price for never guessing at what an
/// operator's deliverables are.
#[derive(Clone, Debug, Default)]
pub struct WorkspaceSnapshot {
    /// Workspace-relative path → (mtime nanos, size). Two signals rather than
    /// one, so a rewrite that lands in the same timestamp tick is still caught
    /// whenever it changes the length.
    entries: BTreeMap<String, (u128, u64)>,
    /// Whether the walk hit [`MAX_SCAN_ENTRIES`] and stopped early.
    truncated: bool,
}

impl WorkspaceSnapshot {
    /// Walks `workspace`, skipping [`SCAN_SKIP_DIRS`] and stopping at
    /// [`MAX_SCAN_ENTRIES`].
    ///
    /// A workspace that does not exist yet snapshots as empty — an agent that
    /// never wrote anything has changed nothing, which is exactly right.
    pub fn take(workspace: &Path) -> Self {
        let mut snapshot = Self::default();
        let mut stack = vec![workspace.to_path_buf()];
        while let Some(dir) = stack.pop() {
            let Ok(reader) = std::fs::read_dir(&dir) else {
                continue;
            };
            for entry in reader.flatten() {
                if snapshot.entries.len() >= MAX_SCAN_ENTRIES {
                    snapshot.truncated = true;
                    return snapshot;
                }
                let path = entry.path();
                let name = entry.file_name().to_string_lossy().into_owned();
                // `file_type` does not follow symlinks, which is deliberate: a
                // link out of the workspace must not be walked, and a link is
                // not something the agent authored in any case.
                let Ok(file_type) = entry.file_type() else {
                    continue;
                };
                if file_type.is_dir() {
                    if !is_hidden(&name) && !SCAN_SKIP_DIRS.contains(&name.as_str()) {
                        stack.push(path);
                    }
                    continue;
                }
                if !file_type.is_file()
                    || is_hidden(&name)
                    || SCAN_SKIP_FILES.contains(&name.as_str())
                {
                    continue;
                }
                let Ok(meta) = entry.metadata() else {
                    continue;
                };
                let modified = meta
                    .modified()
                    .ok()
                    .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                    .map(|d| d.as_nanos())
                    .unwrap_or(0);
                let Ok(relative) = path.strip_prefix(workspace) else {
                    continue;
                };
                snapshot
                    .entries
                    .insert(normalize_source(relative), (modified, meta.len()));
            }
        }
        snapshot
    }

    /// Whether the walk stopped early. A truncated snapshot can only *miss*
    /// changes, never invent them.
    pub fn truncated(&self) -> bool {
        self.truncated
    }

    /// The number of files seen.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether the workspace was empty.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Files added or modified since this snapshot was taken, in path order.
    ///
    /// Deletions are deliberately not reported: a file the agent removed is not
    /// a deliverable it forgot to publish.
    ///
    /// # Why this returns whether it is complete (issue #420 item 3)
    ///
    /// Either snapshot may have stopped at [`MAX_SCAN_ENTRIES`], and a diff of
    /// two partial walks is itself partial. The flag was already set on both
    /// and [`truncated`](Self::truncated) had **no callers** — so the nudge took
    /// an arbitrary DFS prefix and presented it to the agent as the complete
    /// list of what it had changed, which is a quiet completeness claim the scan
    /// cannot support. Returning the fact alongside the files is what makes it
    /// impossible to keep ignoring: a caller has to destructure it.
    pub fn changed_since(&self, workspace: &Path) -> WorkspaceChanges {
        let now = Self::take(workspace);
        // Either side truncating makes the *diff* partial: a baseline that
        // stopped early can make an untouched file look new, and a current walk
        // that stops early simply misses changes.
        let partial = self.truncated || now.truncated;
        let files = now
            .entries
            .into_iter()
            .filter(|(path, stat)| self.entries.get(path) != Some(stat))
            .map(|(path, _)| path)
            .collect();
        WorkspaceChanges { files, partial }
    }
}

/// What changed in a sandbox since a snapshot — and whether that list is the
/// whole story (issue #420 item 3).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct WorkspaceChanges {
    /// Files added or modified, in path order.
    pub files: Vec<String>,
    /// The scan hit [`MAX_SCAN_ENTRIES`], so `files` is a subset of what
    /// actually changed and nothing can say how large a subset.
    ///
    /// Only ever *under*-reports: a partial scan can miss a deliverable, never
    /// invent one. That is the right direction for a heuristic, but it must be
    /// said out loud rather than left for the agent to assume completeness.
    pub partial: bool,
}

/// The changed files that were **not** staged for publication.
///
/// This is the nudge's gate and, later, the fallback warning's content. Empty
/// means either the agent wrote nothing or it offered everything it wrote —
/// both of which are complete, silent successes.
pub fn unpublished(changed: &[String], staged: &[String]) -> Vec<String> {
    changed
        .iter()
        .filter(|path| !staged.contains(path))
        .cloned()
        .collect()
}

/// Renders a bounded, comma-separated file list for a prompt or a log line.
pub fn name_files(files: &[String]) -> String {
    let named: Vec<&str> = files
        .iter()
        .take(MAX_NAMED_FILES)
        .map(String::as_str)
        .collect();
    let listed = named.join(", ");
    match files.len().saturating_sub(named.len()) {
        0 => listed,
        rest => format!("{listed}, and {rest} more"),
    }
}

// ---------------------------------------------------------------------------
// The nudge
// ---------------------------------------------------------------------------

/// The follow-up turn's instruction: *you wrote these and published none of
/// them — is any of them the deliverable?*
///
/// # Why it carries its own context
///
/// Conversation context is not retained across turns, so a bare "publish
/// something?" would reach an agent with no idea what task it is on. The
/// instruction therefore re-states the original brief, quotes the agent's own
/// completed reply, and names the files — everything needed to answer without a
/// shared history.
///
/// # Why it is deliberately non-coercive
///
/// A nudge that implies publishing is mandatory produces published build logs.
/// The prompt names the files, offers the decline in the same breath as the
/// publish, and says outright that scratch files and unfinished work are a fine
/// answer — because they are. A decline is a **clean outcome**: the reason is
/// kept on the card and nothing further happens.
///
/// Pure and stringly-typed on purpose, so its content is unit-testable without
/// a model, a workspace or a turn.
/// # Saying when the list is incomplete (issue #420 item 3)
///
/// `partial` marks a scan that hit [`MAX_SCAN_ENTRIES`]. The list is then a
/// subset, so the sentence introducing it must not read as an inventory — an
/// agent told "you changed these files" reasonably concludes those are the only
/// ones, and would decline on behalf of a deliverable the scan never reached.
pub fn nudge_instruction(
    brief: &str,
    reply: &str,
    changed_files: &[String],
    partial: bool,
) -> String {
    let completeness = if partial {
        "Your sandbox holds more files than this check can read, so this list is incomplete — \
         there may be other files you changed that are not named here."
    } else {
        "That is everything you changed."
    };
    format!(
        "You have just finished this task and your reply has already been sent. This is a \
         follow-up question about the files, and nothing you say here changes the answer you \
         gave.\n\
         \n\
         The task was:\n\
         {brief}\n\
         \n\
         Your reply was:\n\
         {reply}\n\
         \n\
         You changed these files in your sandbox and published none of them:\n\
         {files}\n\
         {completeness}\n\
         \n\
         If any of them is the deliverable this task was asking for, publish it now with \
         `{tool}`. If none of them is — scratch files, notes, intermediate output, work that is \
         not finished — just say briefly why not. Declining is a normal answer and nothing is \
         wrong if you give it; not every task produces a file worth keeping.",
        brief = brief.trim(),
        reply = reply.trim(),
        files = name_files(changed_files),
        tool = PUBLISH_ARTIFACT_TOOL,
    )
}

/// The persona paragraph telling an agent what a deliverable is and how to
/// hand one over.
///
/// Follows the `workspace_brief` precedent: **static**, never a snapshot. It
/// explains the contract rather than listing anything that could go stale.
///
/// The tone matters as much as the content. The nudge exists precisely because
/// agents forget to publish, and the temptation is to over-correct here with
/// "always publish your output" — which produces published build logs and
/// scratch notes, poisoning the churn signal the artifact port exists to
/// measure. So this says what a deliverable *is*, says plainly that many tasks
/// have none, and leaves the judgement where it belongs.
///
/// # Two different places called "workspace" (issue #445)
///
/// This paragraph and
/// [`workspace_brief`](crate::harness::workspace_tools::workspace_brief) can sit
/// in the same system prompt, and before #445 both called their own directory
/// "your workspace" — the agent's private sandbox here, the operator-owned
/// company note tree there. So "it's in the workspace" meant one place to the
/// agent and a different one to the operator reading the console, and an
/// operator sent to the obvious place correctly found nothing. That collision is
/// how a lost deliverable stayed lost even once someone went looking, so the
/// sandbox is named **sandbox** throughout this module and the distinction is
/// stated outright below rather than left to be inferred.
pub fn publish_brief() -> String {
    format!(
        "\n\n## Deliverables\n\
         The files you write live in your **sandbox** — your own private working directory. It is \
         not the company workspace (the shared note tree you read with the workspace tools), and \
         the operator cannot see into it. A file you merely wrote is therefore invisible to \
         everyone but you, however finished it is.\n\
         So if you are asked to produce something — a document, a report, a draft, an export — \
         write it to a file in your sandbox and then hand it over with \
         `{PUBLISH_ARTIFACT_TOOL}`. Publishing is what turns a file into a deliverable the \
         operator can open, read, edit and version; pasting the whole document into your reply is \
         not the same thing. Republish the same path later to add a version rather than a \
         duplicate. The tool tells you where the file landed — say that, and nothing more \
         confident than that. If it returns an error, the file was NOT delivered and you must not \
         report it as though it was.\n\
         Name the files you write in lowercase with dashes — `launch-plan.md`, not `Launch \
         Plan.md`. That is the convention for the whole workspace, and publishing normalizes the \
         name anyway, so a file named that way arrives under the name you gave it instead of one \
         you have to look up.\n\
         Publish only the finished thing somebody asked for. Scratch files, notes to yourself, \
         logs and build output are not deliverables, and plenty of work — a question answered, a \
         check run, a decision made — produces no file at all. Having nothing to publish is a \
         normal outcome, not a gap to fill."
    )
}

/// The title of the board card a conversation's publish mints (issue #445).
///
/// Named from what was actually published rather than from the chat text: the
/// card exists to carry these files, and a title lifted from conversation
/// ("could you write that up?") would describe the request instead of the
/// deliverable sitting on it. One file gives its own title; several give the
/// first plus a count, which stays a fixed-width string no matter how many were
/// published.
///
/// Empty input cannot occur — the card is only minted once there is something to
/// put on it — but it degrades to a neutral title rather than panicking, because
/// a card with a dull name is recoverable and a crashed cycle is not.
pub fn conversation_card_title(published: &[PendingPublish]) -> String {
    match published {
        [] => "Deliverable from a conversation".to_string(),
        [only] => only.title.clone(),
        [first, rest @ ..] => format!("{} (+{} more)", first.title, rest.len()),
    }
}

/// The note explaining why a card exists that nobody asked for (issue #445).
///
/// A card appearing on the board with no request behind it is otherwise a small
/// mystery, so it says outright where it came from: an agent published during a
/// conversation, and this card is the record that carries the result. Without
/// this the honest fix for a silent drop would introduce its own small
/// confusion.
pub fn conversation_card_note(agent: &str, published: &[PendingPublish]) -> String {
    let files: Vec<String> = published.iter().map(|p| p.source.clone()).collect();
    format!(
        "Opened to carry what {agent} published during a conversation: {files}. Chat turns run \
         without a card, so this card was created by the act of publishing — it records a \
         delivered file rather than a request somebody made.",
        files = name_files(&files),
    )
}

/// The note line recording a publish filed onto the card the message already
/// opened (issue #463).
///
/// Deliberately not [`conversation_card_note`]'s wording: that one explains why
/// a card exists that nobody asked for, and this card exists because somebody
/// asked for it. All this has to say is what landed on it and, through the
/// attribution the note carries, who put it there.
pub fn filed_on_card_note(published: &[PendingPublish]) -> String {
    let files: Vec<String> = published.iter().map(|p| p.source.clone()).collect();
    format!(
        "published {files} onto this card — open the Artifacts tab to read the delivered file.",
        files = name_files(&files),
    )
}

/// The line appended to an operator's reply when a publish was accepted in-turn
/// but could not be recorded (issue #445).
///
/// The agent has already said the file is delivered — it was told so — and the
/// receipt cannot be recalled. The remaining choice is whether the operator
/// hears about it, and a log line is not hearing about it. So the correction
/// goes where the false claim went: into the conversation, in the operator's own
/// words rather than the agent's, immediately after the reply that promised the
/// file.
pub fn recording_failed_notice(count: usize) -> String {
    let subject = if count == 1 {
        "1 file was".to_string()
    } else {
        format!("{count} files were")
    };
    format!(
        "\n\n---\n\n**Note from the system:** {subject} published during this turn but could NOT \
         be recorded, so there is no card or artifact to open — treat any claim above that the \
         work was delivered as incorrect. The file is still in the agent's sandbox. Ask for it \
         again, and if this repeats, the artifact store needs looking at."
    )
}

/// The note block recording what the agent said when it published nothing.
///
/// The "why not" is kept and addressable rather than dropped, which is what
/// makes a decline a *recorded* outcome instead of a silent one.
pub fn declined_note(unpublished_files: &[String], reply: &str) -> String {
    format!(
        "unpublished: {files} — agent: {reply}",
        files = name_files(unpublished_files),
        reply = reply.trim(),
    )
}

#[cfg(test)]
mod test;
