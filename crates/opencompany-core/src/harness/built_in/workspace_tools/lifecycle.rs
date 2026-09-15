//! Lifecycle over the agent's **own** workspace folder (issue #671): deleting
//! and renaming what it put there.
//!
//! Since issue #551 an agent can add to the shared tree and overwrite anything
//! in it, but it could not remove or rename a single node — so the one thing it
//! could not do was tidy up after itself. Every superseded draft stayed
//! forever, under whatever name it was given on the first attempt, and issue
//! #607's search made that cost compound: a stale draft competes for a place in
//! a bounded result set with the note that replaced it.
//!
//! # Why this is coherence, not containment
//!
//! The confinement to `agents/<self>/` here is **not** a security boundary, and
//! must not be described as one. The same explicit `workspace` grant already
//! confers [`WORKSPACE_WRITE_TOOL`](super::WORKSPACE_WRITE_TOOL), which can
//! overwrite any note in the tree with an empty body — strictly broader
//! destructive reach than deleting one node inside your own folder. The scope
//! exists because tidying *your own* folder is upkeep the brief already asks
//! for, while reorganising somebody else's work is a judgement call the
//! operator has a console for. That is a division of labour, not a wall.
//!
//! One thing an overwrite does not do, and this does: a delete removes the
//! node's authorship record along with its body, so it is marginally worse
//! forensically than overwriting a note to empty. What narrows that is the
//! scope (your own folder), the per-call approval parking (both tools are
//! `Reach::Consequence` and never grantable standing) and the removal
//! announcement the store emits on the company feed (issue #327).
//!
//! # The shape of the two tools
//!
//! * [`WorkspaceDeleteTool`] carries a **required** `expected_updated_at`
//!   compare-and-swap token, exactly like `workspace_write`, and refuses a
//!   folder that still holds anything. One deliberate node per call, each
//!   individually announced and individually parked — the same
//!   one-node-per-call doctrine `workspace_create` follows, applied to the
//!   destructive direction where it matters more.
//! * [`WorkspaceRenameTool`] carries **no** token, and the asymmetry is worth
//!   stating rather than leaving to be noticed. A rename destroys nothing: the
//!   body, the id and both authorship stamps survive it (the port leaves the
//!   origins alone on purpose), links are by id rather than by path, and the
//!   operator can move it straight back. There is nothing for a stale token to
//!   protect.
//!
//! Both checks run against the **resolved** node's path, never the argument, so
//! passing a raw `id` that names something outside the agent's folder refuses
//! identically to passing its path.
//!
//! # What deliberately stays as it is
//!
//! * **A published note is deleted like any other**, and only the wording
//!   differs. Refusing the published case would invert the risk: a published
//!   deliverable's history lives on its artifact chain and a re-publish mints
//!   the node again, so *that* is the recoverable one. A note the agent merely
//!   created is the one whose deletion is final, and the success message says
//!   so.
//! * **The artifact version's `workspace_node_id` is left dangling**, exactly
//!   as the operator's own DELETE route leaves it today. `materialize`
//!   read-guards the id before reusing it, and older versions keeping the id of
//!   the node that actually held them is the stated "honest history" invariant.
//!   It is a checked tombstone, not a dangling reference.
//! * **Recursion is refused, not implemented.** The port deletes folders
//!   recursively, and exposing that would let one agent call remove a subtree
//!   with a single approval card naming a single path. Refusing a non-empty
//!   folder turns it into N deliberate calls, each announced and each parked.

use async_trait::async_trait;
use serde_json::{Value, json};

use openhuman_core::tools::traits::{PermissionLevel, Tool, ToolResult};

use crate::company::artifact_mirror::published_record_for_node;
use crate::company::workspace_names::kebab_name;
use crate::company::workspace_paths::{is_legal_segment, split_logical_path};
use crate::company::workspace_scaffold::AGENTS_ROOT;
use crate::ports::workspace::NodeKind;

use super::{
    CompanyWorkspace, Entry, PathIndex, WORKSPACE_CREATE_TOOL, WORKSPACE_LIST_TOOL,
    WORKSPACE_READ_TOOL, WORKSPACE_WRITE_TOOL, echo_path, store_reason,
};

/// Tool name: delete one node from the agent's own workspace folder.
pub const WORKSPACE_DELETE_TOOL: &str = "workspace_delete";
/// Tool name: rename and/or move one node inside the agent's own folder.
pub const WORKSPACE_RENAME_TOOL: &str = "workspace_rename";

// ---------------------------------------------------------------------------
// The shared scope gate
// ---------------------------------------------------------------------------

/// Refuse `path` unless it names something strictly inside this agent's own
/// home, returning the agent-facing message when it does not.
///
/// `gerund` is `"deleting"` / `"renaming"`, so one gate produces both tools'
/// refusals and they cannot drift apart.
///
/// The home folder gets its own message rather than the generic one because the
/// reason is different and actionable. `ensure_agent_folder` resolves the home
/// **by name**, so a home that is deleted or renamed is not merely gone — the
/// next publish mints a fresh, empty one beside the orphaned contents, and the
/// company's view of where this agent's work lives silently forks.
fn refuse_unless_inside_own_home(
    workspace: &CompanyWorkspace,
    path: &str,
    gerund: &str,
) -> Option<String> {
    let segments: Vec<&str> = path.split('/').collect();
    if workspace.is_strictly_inside_own_home(&segments) {
        return None;
    }
    if workspace.is_own_home(&segments) {
        return Some(format!(
            "Refused: `{shown}` is your own folder itself, and it is the handle the company uses \
             to find your work. {Gerund} it would strand everything filed under it and mint a \
             fresh, empty folder in its place the next time you publish. Act on what is inside it \
             instead.",
            shown = echo_path(path),
            Gerund = capitalize(gerund),
        ));
    }
    Some(format!(
        "Refused: `{shown}` is outside your own folder `{AGENTS_ROOT}/{agent}/`, and {gerund} is \
         confined to it. You can still revise a note anywhere in the tree with \
         `{WORKSPACE_WRITE_TOOL}`; removing or moving one elsewhere is the operator's to do in the \
         console.",
        shown = echo_path(path),
        agent = workspace.agent_id,
    ))
}

/// Upper-case the first character of an ASCII word, for a message that starts
/// with the shared `gerund`.
fn capitalize(word: &str) -> String {
    let mut chars = word.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => String::new(),
    }
}

/// Read `expected_updated_at` in either the integer or the stringified form.
///
/// Lifted from [`WorkspaceWriteTool`](super::WorkspaceWriteTool) rather than
/// re-derived: models stringify numbers constantly, and rejecting `"2000"`
/// produces an "is required" error for an argument the agent did in fact
/// supply — a misleading message that costs a whole turn to recover from.
fn revision_arg(args: &Value) -> Option<u64> {
    args.get("expected_updated_at").and_then(|value| {
        value
            .as_u64()
            .or_else(|| value.as_str().and_then(|s| s.trim().parse::<u64>().ok()))
    })
}

/// Resolve `path` / `id` against a freshly-read index, mapping every failure to
/// the message the agent should see.
///
/// Returns the index alongside the entry: both tools need to keep asking it
/// questions (what is under this folder, is the target path free) after the
/// node itself has resolved, and re-reading the tree for each would give two
/// answers from two snapshots.
async fn resolve_in_index(
    workspace: &CompanyWorkspace,
    path: Option<&str>,
    id: Option<&str>,
) -> Result<(PathIndex, Entry), ToolResult> {
    let index = match workspace.index().await {
        Ok(index) => index,
        Err(e) => {
            return Err(ToolResult::error(format!(
                "Could not read the company workspace: {reason}.",
                reason = store_reason(&e),
            )));
        }
    };
    let entry = match index.resolve(path, id) {
        Ok(entry) => entry.clone(),
        Err(e) => return Err(ToolResult::error(e.message())),
    };
    Ok((index, entry))
}

/// The `path` / `id` pair, trimmed and with empty strings treated as absent.
fn target_args(args: &Value) -> (Option<&str>, Option<&str>) {
    let path = args
        .get("path")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|p| !p.is_empty());
    let id = args
        .get("id")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|i| !i.is_empty());
    (path, id)
}

// ---------------------------------------------------------------------------
// workspace_delete
// ---------------------------------------------------------------------------

/// What the agent should be told about the permanence of a delete.
#[derive(Debug, PartialEq, Eq)]
enum History {
    /// The node backs a published deliverable, so the artifact chain outlives
    /// it and a re-publish recreates the path.
    Published,
    /// Nothing published points at this node. Removing it is final.
    Direct,
    /// No artifact store is wired, or it could not be read — so neither claim
    /// can honestly be made.
    Unknown,
}

impl History {
    /// The sentence appended to a successful delete.
    fn note(&self) -> &'static str {
        match self {
            Self::Published => {
                " This note was a published deliverable, so its version history stays in \
                 Artifacts and publishing it again recreates the path."
            }
            Self::Direct => {
                " Nothing published pointed at it, so this deletion is permanent — there is no \
                 version history to restore it from."
            }
            Self::Unknown => "",
        }
    }
}

/// Deletes one folder or note from the agent's own workspace folder. Wired only
/// under an explicit `workspace` grant, alongside the other mutating tools.
pub struct WorkspaceDeleteTool {
    workspace: CompanyWorkspace,
}

impl WorkspaceDeleteTool {
    pub(super) fn new(workspace: CompanyWorkspace) -> Self {
        Self { workspace }
    }

    /// Whether this node's removal is recoverable from the artifact chain.
    ///
    /// A failed lookup answers [`History::Unknown`] rather than guessing:
    /// telling an agent a deletion is permanent when it is not (or the reverse)
    /// is worse than telling it nothing, and neither answer changes what the
    /// call does.
    async fn history_of(&self, node_id: &str) -> History {
        let Some(artifacts) = self.workspace.artifacts.as_ref() else {
            return History::Unknown;
        };
        match published_record_for_node(artifacts.as_ref(), &self.workspace.company, node_id).await
        {
            Ok(Some(_)) => History::Published,
            Ok(None) => History::Direct,
            Err(e) => {
                tracing::warn!(
                    company = %self.workspace.company,
                    agent = %self.workspace.agent_id,
                    node = %node_id,
                    error = %e,
                    "[workspace] could not tell whether a node being deleted was published; the \
                     delete proceeds and the agent is told nothing either way"
                );
                History::Unknown
            }
        }
    }
}

#[async_trait]
impl Tool for WorkspaceDeleteTool {
    fn name(&self) -> &str {
        WORKSPACE_DELETE_TOOL
    }

    fn description(&self) -> &str {
        "Delete ONE folder or note from your own workspace folder `agents/<your agent id>/`. USE \
         FOR tidying up after yourself — removing a draft you have replaced, or a working note \
         that is no longer worth a teammate's time to find. You must pass `expected_updated_at` — \
         the `rev` from a `workspace_read` of the note, or the `rev=` on its `workspace_list` line \
         for a folder — and the delete is refused if it changed since. A folder that still holds \
         anything is refused: empty it one node at a time. NOT for anything outside your own \
         folder — shared guidance and a teammate's work stay the operator's to remove. NOT for \
         your own scratch files (use the `file_*` tools)."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "The node's path as shown by workspace_list, e.g. \"agents/ceo/Old draft.md\". Must be inside your own folder."
                },
                "id": {
                    "type": "string",
                    "description": "The node's id, as an alternative to `path`. It must still resolve to something inside your own folder."
                },
                "expected_updated_at": {
                    "type": "integer",
                    "description": "The `rev` value you last saw for this node, from workspace_read or its workspace_list line. The delete is refused if it changed since, so re-read rather than guessing."
                }
            },
            "required": ["expected_updated_at"],
            "additionalProperties": false
        })
    }

    /// Honest level for a tool that removes operator-visible content. As with
    /// the other mutating workspace tools this is not what gates the call — see
    /// the `workspace_delete` descriptor in
    /// [`policy::consequence`](crate::policy::consequence).
    fn permission_level(&self) -> PermissionLevel {
        PermissionLevel::Write
    }

    async fn execute(&self, args: Value) -> anyhow::Result<ToolResult> {
        let (path, id) = target_args(&args);

        let Some(expected) = revision_arg(&args) else {
            return Ok(ToolResult::error(format!(
                "Invalid arguments: `expected_updated_at` is required. Read the note with \
                 `{WORKSPACE_READ_TOOL}` first (or take a folder's `rev=` from its \
                 `{WORKSPACE_LIST_TOOL}` line) and pass that back, so something changed since you \
                 last looked at it is not deleted out from under whoever changed it."
            )));
        };

        let (index, entry) = match resolve_in_index(&self.workspace, path, id).await {
            Ok(pair) => pair,
            Err(refusal) => return Ok(refusal),
        };

        // Scope first, and against the resolved path rather than the argument —
        // a raw `id` naming a teammate's note must refuse exactly as its path
        // would.
        if let Some(refusal) =
            refuse_unless_inside_own_home(&self.workspace, &entry.path, "deleting")
        {
            return Ok(ToolResult::error(refusal));
        }

        // One node per call. The port would happily remove the whole subtree,
        // which is precisely why this layer must not ask it to: a recursive
        // delete is one approval card naming one path that takes an unbounded
        // amount of work with it.
        //
        // Counted over parent ids, not over rendered paths. A path-prefix count
        // reads `by_path`, which deliberately omits every node the path rules
        // exclude — a dangling or cyclic ancestor chain, or a name carrying a
        // separator, which the sqlite and mongodb backends both accept at
        // creation. A folder holding only such children therefore looks empty
        // by every path-shaped measure while the port's recursive delete would
        // still take them: unnamed on the card, unannounced, and precisely the
        // nodes this layer made unreachable so no tool could touch them.
        //
        // `child_count` is built before that filter, so it sees a child whether
        // or not the child has a renderable path. It is also exact per node id
        // rather than per path — two folders may share a path, but never an id,
        // so this is strictly sharper than the prefix count it replaces.
        if entry.node.kind == NodeKind::Folder {
            let held = index
                .child_count
                .get(&entry.node.id)
                .copied()
                .unwrap_or_default();
            if held > 0 {
                return Ok(ToolResult::error(format!(
                    "Refused: the folder `{shown}` still holds {held} node(s), and nothing was \
                     deleted. Delete each of them first — one call each, so every removal is \
                     visible on its own — and then delete the folder. List what is inside it with \
                     `{WORKSPACE_LIST_TOOL}` and prefix=\"{shown}\".",
                    shown = echo_path(&entry.path),
                )));
            }
        }

        // The same check-then-act revision guard `workspace_write` carries, and
        // with the same honesty about it: the tree snapshot above is one
        // authority on the current revision, so this catches the ordinary case
        // (the operator edited the note since the agent read it) and narrows
        // rather than closes the window.
        if entry.node.updated_at_millis != expected {
            return Ok(ToolResult::error(format!(
                "Refused: `{shown}` changed since you last looked at it — you passed \
                 expected_updated_at={expected}, but its current revision is {current}. Read it \
                 again with `{WORKSPACE_READ_TOOL}` and decide whether the change you have not \
                 seen is still worth deleting; do NOT retry with the same expected_updated_at.",
                shown = echo_path(&entry.path),
                current = entry.node.updated_at_millis,
            )));
        }

        // Asked before the delete, because afterwards the node id is the only
        // handle left and a lookup failure would be indistinguishable from
        // "never published".
        let history = match entry.node.kind {
            NodeKind::File => self.history_of(&entry.node.id).await,
            // A folder is never an artifact's node: publishing stamps the file
            // it materialized, so there is no chain to survive a folder.
            NodeKind::Folder => History::Unknown,
        };

        match self
            .workspace
            .store
            .delete(&self.workspace.company, &entry.node.id)
            .await
        {
            Ok(true) => {
                if let Some(outputs) = &self.workspace.outputs {
                    outputs.remove_workspace_node(&entry.node.id);
                }
                Ok(ToolResult::success(format!(
                    "Deleted the workspace {what} `{shown}` (id={id}).{history}",
                    what = match entry.node.kind {
                        NodeKind::Folder => "folder",
                        NodeKind::File => "note",
                    },
                    shown = echo_path(&entry.path),
                    id = entry.node.id,
                    history = history.note(),
                )))
            }
            Ok(false) => Ok(ToolResult::error(format!(
                "Refused: `{shown}` was already gone by the time the delete ran — somebody removed \
                 it while you were deciding. Nothing was changed.",
                shown = echo_path(&entry.path),
            ))),
            Err(e) => Ok(ToolResult::error(format!(
                "Could not delete `{shown}`: {reason}.",
                shown = echo_path(&entry.path),
                reason = store_reason(&e),
            ))),
        }
    }
}

// ---------------------------------------------------------------------------
// workspace_rename
// ---------------------------------------------------------------------------

/// Renames and/or moves one node inside the agent's own workspace folder. Wired
/// only under an explicit `workspace` grant, alongside the other mutating
/// tools.
pub struct WorkspaceRenameTool {
    workspace: CompanyWorkspace,
}

impl WorkspaceRenameTool {
    pub(super) fn new(workspace: CompanyWorkspace) -> Self {
        Self { workspace }
    }
}

#[async_trait]
impl Tool for WorkspaceRenameTool {
    fn name(&self) -> &str {
        WORKSPACE_RENAME_TOOL
    }

    fn description(&self) -> &str {
        "Rename and/or move ONE folder or note INSIDE your own workspace folder `agents/<your \
         agent id>/`. USE FOR tidying your own folder — giving a draft the title it earned, or \
         filing notes under a subfolder you made with `workspace_create`. Pass `new_name`, \
         `new_parent`, or both. The node AND the destination folder must both be inside your own \
         folder; the destination must already exist. The note keeps its body, its id and its \
         authorship — to change the body use `workspace_write`. NOT for moving work out of your \
         folder, and NOT for renaming anything elsewhere in the tree — those stay the operator's."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "The node's path as shown by workspace_list, e.g. \"agents/ceo/Draft.md\". Must be inside your own folder."
                },
                "id": {
                    "type": "string",
                    "description": "The node's id, as an alternative to `path`. It must still resolve to something inside your own folder."
                },
                "new_name": {
                    "type": "string",
                    "description": "The node's new name, one path segment only — no `/`. Include the file extension on a note. Normalized to the workspace convention (lowercase, dashed), so `Q3 Report.md` lands as `q3-report.md`. Omit to keep the current name."
                },
                "new_parent": {
                    "type": "string",
                    "description": "The path of an EXISTING folder to move the node into, e.g. \"agents/ceo/archive\". Must be your own folder or a folder inside it. Omit to leave the node where it is."
                }
            },
            "additionalProperties": false
        })
    }

    /// Honest level for a tool that reorganizes operator-visible content. As
    /// with the other mutating workspace tools this is not what gates the call —
    /// see the `workspace_rename` descriptor in
    /// [`policy::consequence`](crate::policy::consequence).
    fn permission_level(&self) -> PermissionLevel {
        PermissionLevel::Write
    }

    async fn execute(&self, args: Value) -> anyhow::Result<ToolResult> {
        let (path, id) = target_args(&args);

        let new_name = args
            .get("new_name")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|n| !n.is_empty());
        // Kept even when it trims to nothing, so "move me to the root" can be
        // told apart from "leave the parent alone" and answered on its merits.
        let new_parent_arg = args.get("new_parent").and_then(Value::as_str);

        if new_name.is_none() && new_parent_arg.is_none() {
            return Ok(ToolResult::error(format!(
                "Invalid arguments: pass `new_name` to rename, `new_parent` to move, or both. To \
                 change a note's body instead, use `{WORKSPACE_WRITE_TOOL}`."
            )));
        }

        if let Some(name) = new_name
            && !is_legal_segment(name)
        {
            return Ok(ToolResult::error(format!(
                "Invalid `new_name`: `{shown}` is not a single path segment. A name cannot be \
                 `.` or `..` and cannot contain `/` or `\\` — to move the node into a different \
                 folder, pass `new_parent` instead.",
                shown = echo_path(name),
            )));
        }

        // The host owns the name (the same rule `workspace_create` applies to
        // the name it mints): a rename lands lowercase and dashed, so renaming
        // cannot walk a node back out of the convention the tree is kept in.
        // The reply below echoes the path it actually landed at.
        let new_name = new_name.map(kebab_name);
        let new_name = new_name.as_deref();

        let (index, entry) = match resolve_in_index(&self.workspace, path, id).await {
            Ok(pair) => pair,
            Err(refusal) => return Ok(refusal),
        };

        if let Some(refusal) =
            refuse_unless_inside_own_home(&self.workspace, &entry.path, "renaming")
        {
            return Ok(ToolResult::error(refusal));
        }

        // The destination. `None` means "same parent"; anything else has to
        // resolve to a folder that is the agent's home or sits inside it.
        //
        // No mint-on-demand here, unlike `workspace_create`: a node that
        // resolved inside the home proves the home already exists, so there is
        // never a home to bring into existence at this point.
        let destination = match new_parent_arg {
            None => None,
            Some(raw) => {
                // The root is expressible (`""`, `"/"`) and is always outside
                // the agent's folder, so it gets the scope answer rather than a
                // parse error that hides the real reason.
                if raw.trim().trim_matches('/').trim().is_empty() {
                    return Ok(ToolResult::error(format!(
                        "Refused: the workspace root is outside your own folder \
                         `{AGENTS_ROOT}/{agent}/`, so nothing can be moved there. Pass \
                         `new_parent` as your own folder or a folder inside it, or ask the \
                         operator to move it in the console.",
                        agent = self.workspace.agent_id,
                    )));
                }
                let segments = match split_logical_path(raw) {
                    Ok(segments) => segments,
                    Err(why) => {
                        return Ok(ToolResult::error(format!("Invalid `new_parent`: {why}.")));
                    }
                };
                let parent_path = segments.join("/");
                let parent = match index.by_path.get(&parent_path).map(Vec::as_slice) {
                    Some([parent]) if parent.node.kind == NodeKind::Folder => parent,
                    Some([parent]) => {
                        return Ok(ToolResult::error(format!(
                            "Refused: `{shown}` is a note, not a folder, so nothing can be moved \
                             into it.",
                            shown = echo_path(&parent.path),
                        )));
                    }
                    Some(entries) => {
                        return Ok(ToolResult::error(format!(
                            "Refused: the destination `{shown}` is ambiguous — {n} nodes share it. \
                             Ask the operator to rename one of them in the console.",
                            shown = echo_path(&parent_path),
                            n = entries.len(),
                        )));
                    }
                    None => {
                        return Ok(ToolResult::error(format!(
                            "Refused: the folder `{shown}` does not exist, so `{node}` has nowhere \
                             to go. Create it first with `{WORKSPACE_CREATE_TOOL}` and \
                             kind=\"folder\", then retry this call.",
                            shown = echo_path(&parent_path),
                            node = echo_path(&entry.path),
                        )));
                    }
                };
                let parent_segments: Vec<&str> = parent.path.split('/').collect();
                if !(self.workspace.is_own_home(&parent_segments)
                    || self.workspace.is_strictly_inside_own_home(&parent_segments))
                {
                    return Ok(ToolResult::error(format!(
                        "Refused: `{shown}` is outside your own folder `{AGENTS_ROOT}/{agent}/`, \
                         so nothing can be moved into it. Anything you want the rest of the \
                         company to own is the operator's to place; what you keep, keep in your \
                         own folder.",
                        shown = echo_path(&parent.path),
                        agent = self.workspace.agent_id,
                    )));
                }
                Some(parent.clone())
            }
        };

        // Refuse a parent-cycle before the store is asked to create one: a
        // folder landing inside its own subtree (`a` → `a/b`) would leave the
        // tree unreadable for every agent from then on. Only a folder moving to
        // a new parent can cycle — a bare rename keeps the parent, which is
        // never a descendant of the node. Walking the destination's ancestry
        // via the index reaches the moved node (refuse) or the tree root first.
        if entry.node.kind == NodeKind::Folder
            && let Some(parent) = &destination
        {
            let mut cursor = parent.node.id.as_str();
            loop {
                if cursor == entry.node.id {
                    return Ok(ToolResult::error(format!(
                        "Refused: `{shown}` is `{node}` itself or a folder inside it, so moving \
                         the folder into it would make the tree unreadable. Pick a `new_parent` \
                         that is not the folder or one of its subfolders.",
                        shown = echo_path(&parent.path),
                        node = echo_path(&entry.path),
                    )));
                }
                match index
                    .by_id
                    .get(cursor)
                    .and_then(|e| e.node.parent_id.as_deref())
                {
                    Some(parent_id) => cursor = parent_id,
                    None => break,
                }
            }
        }

        // Where this lands, so the "already taken" check below asks about the
        // path that will actually exist rather than about one of its halves.
        // The node resolved strictly inside the home, so it always has a parent
        // segment to fall back on.
        let parent_path = match &destination {
            Some(parent) => parent.path.clone(),
            None => entry
                .path
                .rsplit_once('/')
                .map(|(parent, _)| parent.to_string())
                .expect("a node inside the home has a parent segment"),
        };
        let final_name = new_name.unwrap_or(entry.node.name.as_str());
        let target_path = format!("{parent_path}/{final_name}");

        if let Some(occupants) = index.lookup(&target_path) {
            if occupants.len() == 1 && occupants[0].node.id == entry.node.id {
                return Ok(ToolResult::error(format!(
                    "Refused: `{shown}` is already exactly where you asked to put it, so nothing \
                     was changed. Pass a `new_name` or a `new_parent` that differs from what it \
                     has now.",
                    shown = echo_path(&entry.path),
                )));
            }
            return Ok(ToolResult::error(format!(
                "Refused: `{shown}` already exists, and nothing was changed. A second node at one \
                 path makes it ambiguous for every agent from then on. Pick a name that is free, \
                 or revise the note that is already there with `{WORKSPACE_WRITE_TOOL}`.",
                shown = echo_path(&target_path),
            )));
        }

        match self
            .workspace
            .store
            .rename_move(
                &self.workspace.company,
                &entry.node.id,
                new_name,
                destination
                    .as_ref()
                    .map(|parent| Some(parent.node.id.as_str())),
            )
            .await
        {
            Ok(node) => Ok(ToolResult::success(format!(
                "Moved the workspace {what} `{from}` to `{to}` (id={id}). Its body and its \
                 authorship are unchanged, but the move restamped its revision to rev={rev} — use \
                 that as `expected_updated_at` if you edit or delete it this turn.",
                what = match entry.node.kind {
                    NodeKind::Folder => "folder",
                    NodeKind::File => "note",
                },
                from = echo_path(&entry.path),
                to = echo_path(&target_path),
                id = node.id,
                rev = node.updated_at_millis,
            ))),
            Err(e) => Ok(ToolResult::error(format!(
                "Could not move `{shown}`: {reason}.",
                shown = echo_path(&entry.path),
                reason = store_reason(&e),
            ))),
        }
    }
}

#[cfg(test)]
mod test;
