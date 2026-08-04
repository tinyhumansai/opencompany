//! Brain-agnostic delegation-tool primitives (issue #176, slice 2).
//!
//! The delegation *runtime* — draining a queue and running desk-lead turns —
//! lives in [`delegation`](crate::runtime::delegation) and is harness-only
//! (it needs in-process cognition). But the delegation *tools* themselves —
//! their names, their argument schemas, and the desk-lead resolver — are
//! brain-agnostic: the hosted Medulla path advertises the same tools to the
//! remote cognition service and services them device-side.
//!
//! This module holds those brain-agnostic pieces so BOTH brains share one
//! definition:
//!
//! - the canonical tool-name constants ([`SPAWN_TASK_TOOL`],
//!   [`DELEGATE_TO_DESK_TOOL`]) — the harness `orchestrator` module re-exports
//!   them so the two paths cannot drift;
//! - [`delegation_manifest_entries`], the [`ToolManifestEntry`] catalog the
//!   hosted brain registers with Medulla;
//! - [`desk_lead`], the desk-lead resolver (moved here from the harness-only
//!   [`delegation`](crate::runtime::delegation) module so the hosted path can
//!   resolve a desk's lead without the `openhuman` feature);
//! - the argument parsers ([`SpawnTaskArgs`], [`DelegateArgs`]) the host uses
//!   to service a `spawn_task` / `delegate_to_desk` tool-call frame.
//!
//! Compiled in every build (no feature gate): the hosted brain is in the
//! default build, and the harness path re-exports from here.

use serde_json::{Value, json};

use crate::brain::medulla::wire::ToolManifestEntry;
use crate::ports::types::CompanyRecord;

/// The `spawn_task` tool name — open a tracked task card on the board.
pub const SPAWN_TASK_TOOL: &str = "spawn_task";
/// The `delegate_to_desk` tool name — hand work to a desk's lead member.
pub const DELEGATE_TO_DESK_TOOL: &str = "delegate_to_desk";

/// The `spawn_task` argument schema, shared by the harness tool's
/// `parameters_schema` and the hosted manifest entry so the two never drift.
pub fn spawn_task_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "title": { "type": "string", "description": "The task title." },
            "note": { "type": "string", "description": "An optional longer brief." },
            "assignee": { "type": "string", "description": "An optional desk/teammate id to own it." }
        },
        "required": ["title"],
        "additionalProperties": false
    })
}

/// The `delegate_to_desk` argument schema, shared by the harness tool and the
/// hosted manifest entry.
pub fn delegate_to_desk_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "desk": { "type": "string", "description": "The desk id or name to delegate to." },
            "instruction": { "type": "string", "description": "The instruction for the desk's lead member." }
        },
        "required": ["desk", "instruction"],
        "additionalProperties": false
    })
}

/// The delegation tools advertised to Medulla on the hosted path: `spawn_task`
/// and `delegate_to_desk`, with the same names + schemas the harness exposes.
///
/// Registered on top of the manifest's own `tools.allow` catalog so a hosted
/// company's orchestrator can delegate exactly as the harness one does. The
/// device services the resulting tool-call frames in
/// [`CycleHostImpl`](crate::runtime::cycle) without any local cognition — a
/// `spawn_task` opens a board card, a `delegate_to_desk` writes a durable
/// hand-off card assigned to the desk. (The *synchronous* desk-lead cognition
/// relay the harness performs in-process needs Medulla multi-agent support and
/// is tracked separately in #176 — the hosted hand-off is durable and
/// asynchronous.)
pub fn delegation_manifest_entries() -> Vec<ToolManifestEntry> {
    vec![
        ToolManifestEntry {
            name: SPAWN_TASK_TOOL.to_string(),
            description: Some(
                "Open a tracked task card on the company's board for work that should be \
followed up. Provide a `title`, an optional `note` brief, and an optional `assignee` (a desk \
or teammate id)."
                    .to_string(),
            ),
            input_schema: Some(spawn_task_schema()),
        },
        ToolManifestEntry {
            name: DELEGATE_TO_DESK_TOOL.to_string(),
            description: Some(
                "Hand work to a desk's lead member. Provide the `desk` (its id or name) and the \
`instruction` to carry out. The hand-off is tracked as a task card assigned to that desk, so \
the desk knows it was handed the work when asked directly."
                    .to_string(),
            ),
            input_schema: Some(delegate_to_desk_schema()),
        },
    ]
}

/// The lead member of a desk: the first effective member (manifest ∪ overlay)
/// that is a real roster teammate. `None` when no desk matches or none of its
/// members are on the roster.
///
/// Resolves the desk key (id or case-insensitive name) against both manifest
/// and operator-created overlay desks, then reads the same **effective**
/// membership the REST `list_desks` handler uses
/// ([`CompanyRecord::effective_desk_members`]), so routing and the console
/// cannot drift. An overlay-added lead is reachable on a desk the manifest left
/// empty.
///
/// Lifted here from the harness-only delegation runtime so the hosted path can
/// resolve a desk lead without the `openhuman` feature.
pub fn desk_lead(record: &CompanyRecord, desk: &str) -> Option<String> {
    let desk_id = record.resolve_desk_id(desk)?;
    record
        .effective_desk_members(&desk_id)
        .into_iter()
        .find(|m| record.is_roster_agent(m))
}

/// How many desk ids a rejection message names before eliding the rest, so the
/// message stays short enough to be useful on a company with many desks.
const LISTED_DESKS: usize = 12;

/// Every desk id the company actually has: the manifest `[[group_chat]]` desks
/// in declaration order, then any operator-created overlay desks, deduplicated.
///
/// The **id** is what [`delegate_to_desk`](DELEGATE_TO_DESK_TOOL) takes, so this
/// is the set a delegation target is grounded against. Reads the same two
/// sources [`CompanyRecord::resolve_desk_id`] searches, so "what ids exist" and
/// "does this id resolve" cannot disagree.
pub fn desk_ids(record: &CompanyRecord) -> Vec<String> {
    let mut ids: Vec<String> = Vec::new();
    for chat in &record.manifest.group_chats {
        if !ids.contains(&chat.id) {
            ids.push(chat.id.clone());
        }
    }
    for desk in &record.overlay_desks {
        if !ids.contains(&desk.id) {
            ids.push(desk.id.clone());
        }
    }
    ids
}

/// Why a `delegate_to_desk` target cannot be delivered, phrased for the model
/// that called the tool — or `None` when `key` names a desk that can actually
/// take the work (issue #272).
///
/// Two refusals, deliberately distinct so the caller can act on them:
///
/// * **Unknown desk** — `key` resolves to no desk at all. This is the invented
///   target: the observed failure was a hand-off to `writer`, which is a
///   *teammate*, not a desk. The message names the company's real desk ids so
///   the model can retry in the same turn, and when the key does name a
///   teammate it says so and points at the desk that teammate leads.
/// * **Leadless desk** — the desk is real but no member of it is on the roster,
///   so no turn can ever run for it. Delegating there is always a no-op.
///
/// Returning the reason as a string (rather than rejecting inside the tool)
/// keeps this brain-agnostic: the harness tool turns it into a
/// `ToolResult::error` and the hosted device-side handler turns it into a failed
/// tool frame, from one definition.
pub fn reject_desk_target(record: &CompanyRecord, key: &str) -> Option<String> {
    let Some(desk_id) = record.resolve_desk_id(key) else {
        return Some(unknown_desk_message(record, key));
    };
    if desk_lead(record, &desk_id).is_some() {
        return None;
    }
    let with_leads = desk_list(
        desk_ids(record)
            .into_iter()
            .filter(|id| desk_lead(record, id).is_some())
            .collect(),
    );
    Some(match with_leads {
        Some(list) => format!(
            "The \"{desk_id}\" desk has no member on the roster, so nothing can be handed to it. \
Desks that can take work: {list}."
        ),
        None => format!(
            "The \"{desk_id}\" desk has no member on the roster, so nothing can be handed to it, \
and no other desk has a lead either. Answer directly instead of delegating."
        ),
    })
}

/// The refusal for a delegation target that matches no desk (issue #272).
///
/// Public because the hosted path refuses **only** this case: there, a hand-off
/// is a durable board card assigned to the desk, so a real desk with no lead yet
/// is still visible work rather than a silent drop. The harness path — where a
/// hand-off is a live turn that a leadless desk can never run — goes through
/// [`reject_desk_target`], which covers both.
pub fn unknown_desk_message(record: &CompanyRecord, key: &str) -> String {
    let Some(list) = desk_list(desk_ids(record)) else {
        return format!(
            "There is no \"{key}\" desk — this company has no desks at all, so `delegate_to_desk` \
cannot be used. Answer directly instead."
        );
    };
    // A teammate's id is the most common invented target (the orchestrator
    // reaches for the person it has in mind rather than the desk they sit on),
    // so name the mistake instead of only listing the alternatives.
    let Some(agent) = record.resolve_roster_agent_id(key) else {
        return format!(
            "There is no \"{key}\" desk. Valid desk ids: {list}. Call `delegate_to_desk` again \
with one of those ids."
        );
    };
    match desk_of_member(record, &agent) {
        Some(desk) => format!(
            "There is no \"{key}\" desk — \"{key}\" is a teammate, not a desk. The desk they are \
on is \"{desk}\". Valid desk ids: {list}."
        ),
        None => format!(
            "There is no \"{key}\" desk — \"{key}\" is a teammate, not a desk, and is on no desk. \
Valid desk ids: {list}."
        ),
    }
}

/// The first desk `member` is on, so an invented teammate-as-desk target can be
/// redirected at the desk that teammate actually sits on.
fn desk_of_member(record: &CompanyRecord, member: &str) -> Option<String> {
    desk_ids(record).into_iter().find(|id| {
        record
            .effective_desk_members(id)
            .iter()
            .any(|m| m == member)
    })
}

/// Renders a desk-id list for a message, capped at [`LISTED_DESKS`] with the
/// remainder counted. `None` when there are no ids to list.
fn desk_list(ids: Vec<String>) -> Option<String> {
    if ids.is_empty() {
        return None;
    }
    let shown = ids.len().min(LISTED_DESKS);
    let mut list = ids[..shown].join(", ");
    if ids.len() > shown {
        list.push_str(&format!(" (+{} more)", ids.len() - shown));
    }
    Some(list)
}

/// Parsed `spawn_task` arguments: a required title, an optional brief note, and
/// an optional assignee. Blank strings are treated as absent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpawnTaskArgs {
    /// The task title (required, non-blank).
    pub title: String,
    /// An optional longer brief.
    pub note: Option<String>,
    /// An optional desk/teammate id to own the card.
    pub assignee: Option<String>,
}

impl SpawnTaskArgs {
    /// Parses `spawn_task` args, returning `None` when `title` is missing or
    /// blank (the one hard requirement).
    pub fn parse(args: &Value) -> Option<Self> {
        let title = trimmed_str(args, "title")?;
        Some(Self {
            title,
            note: trimmed_str(args, "note"),
            assignee: trimmed_str(args, "assignee"),
        })
    }
}

/// Parsed `delegate_to_desk` arguments: the target desk and the instruction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DelegateArgs {
    /// The desk id or name to hand the work to.
    pub desk: String,
    /// The instruction the desk should carry out.
    pub instruction: String,
}

impl DelegateArgs {
    /// Parses `delegate_to_desk` args, returning `None` when either `desk` or
    /// `instruction` is missing or blank.
    pub fn parse(args: &Value) -> Option<Self> {
        Some(Self {
            desk: trimmed_str(args, "desk")?,
            instruction: trimmed_str(args, "instruction")?,
        })
    }
}

/// Reads `key` as a string, trims it, and returns it only when non-empty.
fn trimmed_str(args: &Value, key: &str) -> Option<String> {
    args.get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn manifest_entries_cover_both_delegation_tools() {
        let entries = delegation_manifest_entries();
        let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
        assert!(names.contains(&SPAWN_TASK_TOOL), "got {names:?}");
        assert!(names.contains(&DELEGATE_TO_DESK_TOOL), "got {names:?}");
        // Every entry carries a schema so Medulla can shape the call.
        assert!(
            entries
                .iter()
                .all(|e| e.input_schema.is_some() && e.description.is_some())
        );
    }

    #[test]
    fn spawn_task_args_require_a_nonblank_title() {
        assert_eq!(SpawnTaskArgs::parse(&json!({})), None);
        assert_eq!(SpawnTaskArgs::parse(&json!({ "title": "  " })), None);
        let parsed = SpawnTaskArgs::parse(&json!({
            "title": "  Ship it ",
            "note": " brief ",
            "assignee": " eng "
        }))
        .expect("valid");
        assert_eq!(parsed.title, "Ship it");
        assert_eq!(parsed.note.as_deref(), Some("brief"));
        assert_eq!(parsed.assignee.as_deref(), Some("eng"));
        // Blank optionals collapse to None.
        let bare = SpawnTaskArgs::parse(&json!({ "title": "x", "note": "", "assignee": "" }))
            .expect("valid");
        assert_eq!(bare.note, None);
        assert_eq!(bare.assignee, None);
    }

    /// The company shape issue #272 was observed on: real desks, plus a
    /// teammate (`writer`) the orchestrator mistook for one.
    fn record() -> CompanyRecord {
        let manifest = toml::from_str(
            r#"
[company]
name = "Acme"

[[agent]]
id = "ceo"
role = "Chief Executive"

[[agent]]
id = "writer"
role = "Writer"

[[group_chat]]
id = "engineering"
name = "Engineering desk"
members = ["ceo"]

[[group_chat]]
id = "content"
name = "Content desk"
members = ["writer"]

[[group_chat]]
id = "legal"
name = "Legal desk"
members = ["counsel"]
"#,
        )
        .expect("valid manifest");
        CompanyRecord {
            id: crate::ports::types::CompanyId::new("acme"),
            manifest,
            ledger: Vec::new(),
            lifecycle: "running".to_string(),
            overlay_agents: Vec::new(),
            overlay_desk_members: Vec::new(),
            overlay_desk_order: Vec::new(),
            overlay_desks: Vec::new(),
            overlay_workflows: Vec::new(),
            template_provenance: None,
        }
    }

    #[test]
    fn desk_ids_lists_manifest_desks_in_declaration_order() {
        assert_eq!(desk_ids(&record()), ["engineering", "content", "legal"]);
    }

    #[test]
    fn a_real_desk_with_a_lead_is_not_rejected() {
        assert_eq!(reject_desk_target(&record(), "content"), None);
        // The id-or-name key `resolve_desk_id` already accepts still works.
        assert_eq!(reject_desk_target(&record(), "Content desk"), None);
    }

    /// The observed bug: `writer` is a teammate, not a desk. The refusal must
    /// name the real desk ids so the model can retry in the same turn, and say
    /// which desk that teammate is actually on.
    #[test]
    fn an_invented_desk_is_rejected_with_the_valid_set() {
        let message = reject_desk_target(&record(), "writer").expect("rejected");
        assert!(message.contains("engineering"), "{message}");
        assert!(message.contains("content"), "{message}");
        assert!(message.contains("legal"), "{message}");
        assert!(
            message.contains("teammate"),
            "a teammate-as-desk target must be named as such: {message}"
        );
        // A key that is neither a desk nor a teammate still gets the valid set.
        let message = reject_desk_target(&record(), "growth").expect("rejected");
        assert!(message.contains("engineering"), "{message}");
        assert!(!message.contains("teammate"), "{message}");
    }

    /// A desk that exists but has nobody on the roster can never run a turn, so
    /// it is refused too — with the desks that CAN take work.
    #[test]
    fn a_desk_with_no_roster_lead_is_rejected() {
        let message = reject_desk_target(&record(), "legal").expect("rejected");
        assert!(message.contains("no member on the roster"), "{message}");
        assert!(
            message.contains("Desks that can take work: engineering, content."),
            "only desks with a lead may be offered as alternatives: {message}"
        );
    }

    /// A company with no desks at all cannot delegate; say so rather than
    /// offering an empty list.
    #[test]
    fn a_company_with_no_desks_says_so() {
        let mut record = record();
        record.manifest.group_chats.clear();
        let message = reject_desk_target(&record, "content").expect("rejected");
        assert!(message.contains("no desks at all"), "{message}");
    }

    #[test]
    fn the_desk_list_is_capped_and_counts_the_remainder() {
        let ids: Vec<String> = (0..LISTED_DESKS + 3).map(|i| format!("d{i}")).collect();
        let list = desk_list(ids).expect("non-empty");
        assert!(list.ends_with("(+3 more)"), "{list}");
        assert_eq!(desk_list(Vec::new()), None);
    }

    #[test]
    fn delegate_args_require_desk_and_instruction() {
        assert_eq!(DelegateArgs::parse(&json!({ "desk": "eng" })), None);
        assert_eq!(
            DelegateArgs::parse(&json!({ "instruction": "do it" })),
            None
        );
        let parsed = DelegateArgs::parse(&json!({
            "desk": " engineering ",
            "instruction": " build the thing "
        }))
        .expect("valid");
        assert_eq!(parsed.desk, "engineering");
        assert_eq!(parsed.instruction, "build the thing");
    }
}
