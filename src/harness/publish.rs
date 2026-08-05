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
use sha2::{Digest, Sha256};

use openhuman_core::openhuman as oh;

use oh::tools::traits::{PermissionLevel, Tool, ToolResult};

use crate::ports::artifacts::ArtifactKind;

/// Tool name: promote a workspace file to a versioned deliverable.
pub const PUBLISH_ARTIFACT_TOOL: &str = "publish_artifact";

/// The largest body stored inline, in bytes.
///
/// Text at or under this is stored whole. Anything over it — or anything that
/// is not UTF-8 — is stored as a structured **reference** instead, never as
/// silently-truncated content presented as complete. A half-spec that looks
/// finished is worse than an honest pointer to a file that is too big to
/// inline.
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
/// `checkpoints`. The agent's `workspace_dir` is *also* where OpenHuman writes
/// its session transcripts (`sessions/<date>/*.md`, `session_raw/*.jsonl`), its
/// own artifact store and its subagent checkpoints. Those are written on **every
/// single run**, by the harness rather than by the agent, so without this the
/// scan would report unpublished changes after every dispatch and the nudge
/// would fire every time — asking an agent whether its own transcript is a
/// deliverable. That is not a tuning detail; it is the difference between a
/// feature and a permanent false positive.
const SCAN_SKIP_DIRS: [&str; 6] = [
    "node_modules",
    "target",
    "sessions",
    "session_raw",
    "artifacts",
    "checkpoints",
];

/// File names the scan ignores wherever they appear.
///
/// `audit.log` is the per-workspace shell audit trail the `shell` toolbelt
/// writes (`AuditConfig::log_path`), so any agent granted shell rewrites it on
/// every run.
const SCAN_SKIP_FILES: [&str; 1] = ["audit.log"];

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
    /// The normalized workspace-relative path — the artifact's identity
    /// alongside its task id.
    pub source: String,
    /// The operator-facing title. Defaults to the file's name.
    pub title: String,
    /// What it holds, from the `kind` argument or inferred from the extension.
    pub kind: ArtifactKind,
    /// Why this revision exists, if the agent said.
    pub note: Option<String>,
    /// The stored body: the file's text, or a reference block for an over-cap
    /// or non-UTF-8 file.
    pub body: String,
}

/// A shared, in-memory queue of staged publishes — the exact
/// [`McpFailureQueue`](crate::harness::mcp_probe::McpFailureQueue) pattern.
///
/// Cheap to [`Clone`] (a shared handle); the tool built into the agent and the
/// brain that drains it see the same queue because
/// [`HarnessDeps`](crate::harness::HarnessDeps) clones share this handle.
#[derive(Clone, Default)]
pub struct PendingPublishQueue {
    inner: Arc<Mutex<Vec<PendingPublish>>>,
}

impl PendingPublishQueue {
    /// Stages a publish.
    pub fn push(&self, publish: PendingPublish) {
        self.inner.lock().expect("publish queue").push(publish);
    }

    /// Empties the queue. Called before each turn so nothing a prior turn — an
    /// operator chat turn earlier in the same cycle, or an abandoned redirect
    /// re-run — staged can be attributed to this card.
    pub fn clear(&self) {
        self.inner.lock().expect("publish queue").clear();
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
            Self::Empty => "`path` is required: give the workspace-relative path of the file you \
                            want to publish, e.g. \"specs/launch.md\"."
                .to_string(),
            Self::Outside => format!(
                "`{path}` is outside your workspace. Publish only files you wrote inside it, using \
                 a relative path like \"specs/launch.md\" — absolute paths and `..` are refused."
            ),
            Self::Missing => format!(
                "There is no file at `{path}` in your workspace. Check the path with `list_files` \
                 or `glob`, and write the file before publishing it."
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
#[derive(Debug, PartialEq, Eq)]
pub struct CapturedBody {
    /// The stored body.
    pub body: String,
    /// The kind the capture forces, when it forces one. A reference body is
    /// never `Text`/`Markdown` — presenting a pointer under a kind the console
    /// renders as prose would be a lie about what the operator is reading.
    pub forced_kind: Option<ArtifactKind>,
    /// Whether the body is a reference rather than the content.
    pub is_reference: bool,
}

/// Reads `file` into a stored body, deciding inline-versus-reference **now**.
///
/// Over-cap or non-UTF-8 produces a structured reference block naming the
/// workspace-relative path, the exact byte size and the sha256 of the bytes on
/// disk at this moment. That digest is what makes the reference verifiable
/// later: the record survives a wiped sandbox even though the payload does not,
/// and a reader can tell whether the file they are holding is the one that was
/// published.
pub fn capture_body(
    file: &Path,
    source: &str,
    inferred: ArtifactKind,
) -> std::io::Result<CapturedBody> {
    let bytes = std::fs::read(file)?;
    let size = bytes.len();
    if size <= MAX_ARTIFACT_BODY_BYTES
        && let Ok(text) = String::from_utf8(bytes.clone())
    {
        return Ok(CapturedBody {
            body: text,
            forced_kind: None,
            is_reference: false,
        });
    }
    let mut hasher = Sha256::new();
    hasher.update(&bytes);
    let digest = hasher.finalize();
    let sha = digest.iter().fold(String::new(), |mut acc, b| {
        use std::fmt::Write as _;
        let _ = write!(acc, "{b:02x}");
        acc
    });
    let why = if size > MAX_ARTIFACT_BODY_BYTES {
        format!("{size} bytes exceeds the {MAX_ARTIFACT_BODY_BYTES}-byte inline cap")
    } else {
        "the file is not UTF-8 text".to_string()
    };
    // A reference to an image stays an image; anything else is a file. Either
    // way it is NOT prose, so the console will not try to render or diff it as
    // though it were.
    let forced = if matches!(inferred, ArtifactKind::Image) {
        ArtifactKind::Image
    } else {
        ArtifactKind::File
    };
    Ok(CapturedBody {
        body: format!(
            "Reference — the content is not stored inline because {why}.\n\
             \n\
             path: {source}\n\
             bytes: {size}\n\
             sha256: {sha}\n\
             \n\
             The file lives in the agent's workspace. Wiping the sandbox leaves this record \
             intact and the payload unreachable."
        ),
        forced_kind: Some(forced),
        is_reference: true,
    })
}

// ---------------------------------------------------------------------------
// The tool
// ---------------------------------------------------------------------------

/// Promotes one workspace file to a versioned deliverable, by staging it on the
/// shared queue the brain drains.
pub struct PublishArtifactTool {
    workspace: PathBuf,
    queue: PendingPublishQueue,
}

impl PublishArtifactTool {
    /// Binds the tool to one agent's workspace and the shared queue.
    pub fn new(workspace: impl Into<PathBuf>, queue: PendingPublishQueue) -> Self {
        Self {
            workspace: workspace.into(),
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
        "Publish a file you wrote in your workspace as a deliverable for this task. USE FOR the \
         finished output somebody asked for — a spec, a draft, a report, an invoice, an exported \
         dataset. The operator sees published files on the task's Artifacts tab, and republishing \
         the same path on a later run adds a version rather than a duplicate. NOT for scratch \
         files, notes to yourself, logs, or build output, and NOT a way to send a message — your \
         reply already reaches the operator. Publishing nothing is a perfectly good outcome for a \
         task that produced no file."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Workspace-relative path of the file to publish, e.g. \"specs/launch.md\". Must be a file you wrote inside your own workspace."
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

        let captured = match capture_body(&file, &source, inferred) {
            Ok(captured) => captured,
            Err(err) => {
                return Ok(ToolResult::error(format!(
                    "Could not read `{source}`: {err}"
                )));
            }
        };
        let kind = captured.forced_kind.unwrap_or(inferred);

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
            source: source.clone(),
            title: title.clone(),
            kind,
            note,
            body: captured.body,
        });

        // The message describes what was **captured**, in the past tense,
        // because that is the only thing still true after a later shell step
        // rewrites the file.
        let how = if captured.is_reference {
            "recorded as a reference (path, size and sha256) because it is too large or not text"
        } else {
            "captured in full"
        };
        Ok(ToolResult::success(format!(
            "Published `{source}` as \"{title}\" ({kind}) — {how}. It appears on this task's \
             Artifacts tab when the run finishes.",
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
    pub fn changed_since(&self, workspace: &Path) -> Vec<String> {
        let now = Self::take(workspace);
        now.entries
            .into_iter()
            .filter(|(path, stat)| self.entries.get(path) != Some(stat))
            .map(|(path, _)| path)
            .collect()
    }
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
pub fn nudge_instruction(brief: &str, reply: &str, changed_files: &[String]) -> String {
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
         You changed these files in your workspace and published none of them:\n\
         {files}\n\
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
pub fn publish_brief() -> String {
    format!(
        "\n\n## Deliverables\n\
         If this task asks you to produce something — a document, a report, a draft, an export — \
         write it to a file in your workspace and then publish it with `{PUBLISH_ARTIFACT_TOOL}`. \
         That is what puts it on the task's Artifacts tab where the operator can read, edit and \
         version it; a file you merely wrote is invisible to them, and pasting the whole document \
         into your reply is not the same thing. Republish the same path on a later run to add a \
         version rather than a duplicate.\n\
         Publish only the finished thing somebody asked for. Scratch files, notes to yourself, \
         logs and build output are not deliverables, and plenty of tasks — a question answered, a \
         check run, a decision made — produce no file at all. Having nothing to publish is a \
         normal outcome, not a gap to fill."
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
