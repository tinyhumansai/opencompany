//! The agent's ledger surface: five tools, however many ledgers exist.
//!
//! # Why five tools and not five per ledger
//!
//! Because the tool schema is built once, when the agent is constructed, and a
//! ledger a company declares mid-run must be reachable without a rebuild. That
//! forces the design: `ledger` is a plain **string** checked against the
//! registry at call time rather than an enum baked into a schema, and an
//! unknown slug comes back with the real ones — which is the discovery path a
//! model actually follows, in one turn, without having thought to list them
//! first.
//!
//! It also means the count does not grow when a company adds an axis. A surface
//! whose tool list changes shape per tenant is one no prompt can describe.
//!
//! # What is deliberately not here
//!
//! **No delete tool.** Everything an agent does to a ledger is additive and
//! therefore recoverable by reading its log; deletion is not. Being finished
//! with a row is [`CLOSE_ENTRY_TOOL`], which keeps the reason — and the reason
//! is the whole value of a closed row to whoever reads it next. See
//! [`AuthorKind::may_delete`](crate::ledger::AuthorKind::may_delete).
//!
//! **No retire tool**, for the same reason: retiring a ledger is a person's
//! call in the console.
//!
//! **[`DEFINE_LEDGER_TOOL`] *is* here**, though, and that asymmetry is the
//! point. A company discovers which axes it needs while it is running, so a
//! declaration that required an operator would be discovered and then not made.
//! What an agent cannot do is undo one.

use std::collections::BTreeMap;
use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{Value, json};

use oh::tools::traits::{PermissionLevel, Tool, ToolResult};
use openhuman_core as oh;

use crate::company::ledgers::{self, Ledgers, Query};
use crate::company::{LedgerAccess, LedgerGrant};
use crate::ledger::{LedgerAuthor, LedgerSource, LedgerSpec, ORDERS};

/// Names every ledger this company has.
pub const LIST_LEDGERS_TOOL: &str = "list_ledgers";
/// Reads rows off one ledger.
pub const READ_LEDGER_TOOL: &str = "read_ledger";
/// Opens or amends a row.
pub const RECORD_ENTRY_TOOL: &str = "record_entry";
/// Closes a row, with the reason.
pub const CLOSE_ENTRY_TOOL: &str = "close_entry";
/// Declares a new ledger.
pub const DEFINE_LEDGER_TOOL: &str = "define_ledger";

/// This agent's access to slug `slug`, from its manifest-declared grants.
///
/// `None` grants (an omitted `[[agent]].ledgers` key) means every slug answers
/// `Some(Record)` — unrestricted, the tool surface every agent had before this
/// field existed. `Some(list)` answers only for the slugs it names, so an
/// agent that declares a `ledgers` list at all is confined to exactly what it
/// lists — the same opt-in-confinement shape as `Agent::write_scope`. Mirrors
/// [`crate::company::Agent::ledger_access`], duplicated here rather than
/// called through an `&Agent` because these tools are built once and held
/// `'static`, past the manifest's own lifetime.
fn ledger_access(grants: &Option<Vec<LedgerGrant>>, slug: &str) -> Option<LedgerAccess> {
    match grants {
        None => Some(LedgerAccess::Record),
        Some(list) => list
            .iter()
            .find(|grant| grant.name.eq_ignore_ascii_case(slug.trim()))
            .map(|grant| grant.access),
    }
}

/// Builds the five tools for one agent.
///
/// `ledger_grants` and `can_declare_ledgers` are the manifest's
/// `[[agent]].ledgers` and `.can_declare_ledgers` — see
/// [`crate::company::Agent::ledger_access`] for what an omitted `ledgers` key
/// means.
pub fn ledger_tools(
    ctx: Ledgers,
    agent_id: String,
    ledger_grants: Option<Vec<LedgerGrant>>,
    can_declare_ledgers: bool,
) -> Vec<Box<dyn Tool>> {
    let author = LedgerAuthor::agent(agent_id);
    vec![
        Box::new(ListLedgers {
            ctx: ctx.clone(),
            ledger_grants: ledger_grants.clone(),
        }),
        Box::new(ReadLedger {
            ctx: ctx.clone(),
            ledger_grants: ledger_grants.clone(),
        }),
        Box::new(RecordEntry {
            ctx: ctx.clone(),
            author: author.clone(),
            ledger_grants: ledger_grants.clone(),
        }),
        Box::new(CloseEntry {
            ctx: ctx.clone(),
            author: author.clone(),
            ledger_grants,
        }),
        Box::new(DefineLedger {
            ctx,
            can_declare_ledgers,
        }),
    ]
}

/// The prompt section describing the surface.
///
/// Sync over an already-resolved registry, because the prompt is assembled
/// synchronously — see [`HarnessDeps::ledger_registry`](crate::harness::HarnessDeps::ledger_registry).
///
/// A **catalogue**, not a sentence saying the tools exist. The reader brief
/// riemann started with said "`list_ledgers` names every one" and stopped,
/// which puts the answer behind a call a model has to think to make — and a
/// tool granted, unmentioned and never called is the observed failure mode, not
/// a hypothetical one. So every ledger is named here with its purpose, built
/// from the registry at prompt-assembly time, and a ledger declared afterwards
/// is named in the next prompt built.
pub fn ledger_brief(registry: &crate::ledger::Registry) -> String {
    let mut brief = String::from(
        "\n\n## The company's ledgers\n\nA ledger is the company's durable record of one kind of \
         thing — rows with an id, a status and a reason each closed one closed. Read one with \
         `read_ledger` BEFORE opening work or re-answering a question: re-proposing something \
         already ruled out is the cheapest mistake available, and the reason on the closed row is \
         what prevents it. Record what you decide or discover with `record_entry`, which merges \
         into an existing row when you reuse its id — so amending, re-prioritising and closing are \
         all the same call. Finish a row with `close_entry` and say why; a row that does not say \
         why it closed is worth nothing to whoever reads it next. You cannot delete anything: \
         everything you record is additive and stays readable, and removing a row is a person's \
         call.\n\nThis company keeps:\n\n",
    );
    for spec in registry.specs() {
        let purpose = crate::ledger::budget::truncate(&spec.purpose, 300);
        brief.push_str(&format!("- `{}` — {purpose}", spec.slug));
        if spec.source == LedgerSource::Native {
            brief.push_str(&format!(" _(read-only here: {})_", spec.written_by));
        } else if !spec.writable_by("") {
            brief.push_str(" _(writable by a named few; try it and the refusal says who)_");
        }
        brief.push('\n');
    }
    brief.push_str(
        "\nIf none of these fits something the company will need to look up again — a hiring \
         pipeline, a customer promise, an experiment — declare one with `define_ledger` rather \
         than putting it in a note, where nothing can find it later.\n",
    );
    brief
}

/// Resolves a slug the agent may use, or an error naming the ones it may not.
///
/// Access is checked on the **raw** slug before the registry is opened, so an
/// agent without a grant never learns that a ledger exists: the refusal is the
/// same whether the ledger is real or not. The registry's "here are all the
/// slugs" error is only reachable by an agent whose grant already names the
/// ledger.
async fn spec_for(
    ctx: &Ledgers,
    arguments: &Value,
    grants: &Option<Vec<LedgerGrant>>,
    need: LedgerAccess,
) -> Result<LedgerSpec, String> {
    let slug = text(arguments, "ledger");
    if slug.is_empty() {
        return Err("Name the `ledger` to use. `list_ledgers` names them all.".to_string());
    }
    require_access(grants, &slug, need)?;
    let registry = ledgers::registry(ctx)
        .await
        .map_err(|error| format!("Could not read this company's ledgers: {error}."))?;
    registry
        .require(&slug)
        .cloned()
        .map_err(|error| format!("{error}"))
}

/// Refuses `slug` when `grants` does not give at least `need`.
///
/// `Record` implies `Read` — recording requires reading nothing extra. Named
/// so every ledger tool refuses in the same words, whether the reason is "not
/// in your declared list" or "declared read-only for you".
fn require_access(
    grants: &Option<Vec<LedgerGrant>>,
    slug: &str,
    need: LedgerAccess,
) -> Result<(), String> {
    match ledger_access(grants, slug) {
        Some(LedgerAccess::Record) => Ok(()),
        Some(LedgerAccess::Read) if need == LedgerAccess::Read => Ok(()),
        Some(LedgerAccess::Read) => Err(format!(
            "Refused: your manifest grants only read access to `{slug}`. Recording or closing a \
             row needs `record` access — ask the operator to change your `ledgers` grant."
        )),
        None => Err(format!(
            "Refused: `{slug}` is not in your declared `ledgers` grant. `list_ledgers` names what \
             you can reach."
        )),
    }
}

fn text(arguments: &Value, key: &str) -> String {
    arguments
        .get(key)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .trim()
        .to_string()
}

fn optional(arguments: &Value, key: &str) -> Option<String> {
    let value = text(arguments, key);
    if value.is_empty() { None } else { Some(value) }
}

/// Reads the `fields` object into the merge map.
///
/// A JSON `null` clears a field, which is the one thing a merge cannot
/// otherwise express. A non-string value is rendered rather than refused: a
/// model that answers a `count` field with `12` means twelve, and refusing it
/// over a quoting detail spends a turn to gain nothing.
fn merge_fields(arguments: &Value) -> BTreeMap<String, Option<String>> {
    arguments
        .get("fields")
        .and_then(Value::as_object)
        .map(|object| {
            object
                .iter()
                .map(|(name, value)| {
                    let value = match value {
                        Value::Null => None,
                        Value::String(text) => Some(text.clone()),
                        other => Some(other.to_string()),
                    };
                    (name.clone(), value)
                })
                .collect()
        })
        .unwrap_or_default()
}

/// The `ledger` argument's schema, shared so every tool describes it the same
/// way.
fn ledger_argument() -> Value {
    json!({
        "type": "string",
        "description": "The ledger's slug, as `list_ledgers` reports it — for example `tasks`, `goals` or `decisions`."
    })
}

// ---------------------------------------------------------------------------
// list_ledgers
// ---------------------------------------------------------------------------

struct ListLedgers {
    ctx: Ledgers,
    ledger_grants: Option<Vec<LedgerGrant>>,
}

#[async_trait]
impl Tool for ListLedgers {
    fn name(&self) -> &str {
        LIST_LEDGERS_TOOL
    }

    fn description(&self) -> &str {
        "Name every ledger this company keeps, with what each one holds, its statuses and how many \
         rows are open. USE FOR finding where something belongs before recording it, and for \
         checking whether an axis already exists before declaring a new one. Read a ledger's rows \
         with `read_ledger`."
    }

    fn parameters_schema(&self) -> Value {
        json!({ "type": "object", "properties": {}, "additionalProperties": false })
    }

    fn permission_level(&self) -> PermissionLevel {
        PermissionLevel::ReadOnly
    }

    async fn execute(&self, _arguments: Value) -> anyhow::Result<ToolResult> {
        let registry = match ledgers::registry(&self.ctx).await {
            Ok(registry) => registry,
            Err(error) => {
                return Ok(ToolResult::error(format!(
                    "Could not read this company's ledgers: {error}."
                )));
            }
        };
        let mut out = String::new();
        for spec in registry
            .specs()
            .iter()
            .filter(|spec| ledger_access(&self.ledger_grants, &spec.slug).is_some())
        {
            let entries = ledgers::entries(&self.ctx, spec).await.unwrap_or_default();
            out.push_str(&format!(
                "- `{}` — {}\n  statuses: {}\n  {} open, {} closed\n",
                spec.slug,
                crate::ledger::budget::truncate(&spec.purpose, 300),
                spec.statuses
                    .iter()
                    .map(|status| status.name.as_str())
                    .collect::<Vec<_>>()
                    .join(", "),
                entries.open_count(spec),
                entries.closed_count(spec),
            ));
            if spec.source == LedgerSource::Native {
                out.push_str(&format!("  read-only here: {}\n", spec.written_by));
            }
        }
        // Surfaced rather than swallowed: a company whose ledger silently
        // stopped appearing has no way to find out why.
        for fault in registry.faults() {
            out.push_str(&format!("- (not loaded) {fault}\n"));
        }
        Ok(ToolResult::success(out))
    }
}

// ---------------------------------------------------------------------------
// read_ledger
// ---------------------------------------------------------------------------

struct ReadLedger {
    ctx: Ledgers,
    ledger_grants: Option<Vec<LedgerGrant>>,
}

#[async_trait]
impl Tool for ReadLedger {
    fn name(&self) -> &str {
        READ_LEDGER_TOOL
    }

    fn description(&self) -> &str {
        "Read rows off one of this company's ledgers. USE FOR checking what has already been \
         decided, goaled, ruled out or finished before you propose, re-answer or open work — \
         including the task board, which is readable here as `tasks`. Narrow with `status`, one \
         `entry` id, or a `query` matched against every field. Returns whole rows, bounded, and \
         says how many matched."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "ledger": ledger_argument(),
                "status": {
                    "type": "string",
                    "description": "Only rows in this status. Optional."
                },
                "entry": {
                    "type": "string",
                    "description": "One row, by its id. Optional."
                },
                "query": {
                    "type": "string",
                    "description": "Only rows whose id or any field contains this text. Optional."
                },
                "sort": {
                    "type": "string",
                    "enum": ORDERS,
                    "description": "`recent` for most-recently-recorded-or-updated first, `recorded` for the order rows were first opened. Optional; each ledger has its own default."
                },
                "limit": {
                    "type": "integer",
                    "description": "Rows to return. Optional; bounded whatever you ask for."
                }
            },
            "required": ["ledger"],
            "additionalProperties": false
        })
    }

    fn permission_level(&self) -> PermissionLevel {
        PermissionLevel::ReadOnly
    }

    async fn execute(&self, arguments: Value) -> anyhow::Result<ToolResult> {
        let spec = match spec_for(
            &self.ctx,
            &arguments,
            &self.ledger_grants,
            LedgerAccess::Read,
        )
        .await
        {
            Ok(spec) => spec,
            Err(message) => return Ok(ToolResult::error(message)),
        };
        let query = Query {
            entry: optional(&arguments, "entry"),
            status: optional(&arguments, "status"),
            text: optional(&arguments, "query"),
            sort: optional(&arguments, "sort"),
            limit: arguments
                .get("limit")
                .and_then(Value::as_u64)
                .and_then(|limit| usize::try_from(limit).ok()),
        };
        let read = match ledgers::read(&self.ctx, &spec, &query).await {
            Ok(read) => read,
            Err(error) => return Ok(ToolResult::error(format!("{error}"))),
        };
        if read.entries.is_empty() {
            return Ok(ToolResult::success(format!(
                "`{}` has no rows matching that. It holds {} in total.",
                spec.slug,
                ledgers::entries(&self.ctx, &spec)
                    .await
                    .map(|entries| entries.entries.len())
                    .unwrap_or_default()
            )));
        }
        let mut out = String::new();
        for entry in &read.entries {
            out.push_str(&format!("### `{}`\n", entry.id));
            for (name, value) in &entry.fields {
                out.push_str(&format!("- {name}: {value}\n"));
            }
            out.push_str(&format!(
                "- last recorded by: {}\n\n",
                entry.updated_by.byline()
            ));
        }
        // A short list that reads as complete is worse than a long one: the
        // reader concludes there is nothing more and re-proposes what was cut.
        if read.matched > read.entries.len() {
            out.push_str(&format!(
                "_{} of {} shown. Ask for more with `limit`, or narrow with `status` or \
                 `query` — the rest are not gone._\n",
                read.entries.len(),
                read.matched
            ));
        }
        for fault in &read.faults {
            out.push_str(&format!("_Fault: {fault}_\n"));
        }
        Ok(ToolResult::success(out))
    }
}

// ---------------------------------------------------------------------------
// record_entry
// ---------------------------------------------------------------------------

struct RecordEntry {
    ctx: Ledgers,
    author: LedgerAuthor,
    ledger_grants: Option<Vec<LedgerGrant>>,
}

#[async_trait]
impl Tool for RecordEntry {
    fn name(&self) -> &str {
        RECORD_ENTRY_TOOL
    }

    fn description(&self) -> &str {
        "Record a row on one of this company's ledgers — a goal, a decision, a risk, whatever that \
         ledger holds. USE FOR putting something durable where the company will find it again, \
         instead of leaving it in a reply or a note. Reusing an existing `id` MERGES into that row \
         rather than opening a second one, so amending a row and moving it back to the top of the \
         list are both this call. Fill the fields `list_ledgers` names for that ledger: a new row \
         must carry every one it marks required, and is REFUSED without them; an amendment need \
         only carry what changes, because the row keeps the rest."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "ledger": ledger_argument(),
                "id": {
                    "type": "string",
                    "description": "The row's id. Reuse an existing one to amend that row; a new one opens a new row. Short and readable — `vendor-slip`, not a number."
                },
                "fields": {
                    "type": "object",
                    "description": "The fields to set, as a flat object. Only what you are changing — everything else on the row is left alone. A JSON null clears a field.",
                    "additionalProperties": true
                }
            },
            "required": ["ledger", "id", "fields"],
            "additionalProperties": false
        })
    }

    fn permission_level(&self) -> PermissionLevel {
        PermissionLevel::Write
    }

    async fn execute(&self, arguments: Value) -> anyhow::Result<ToolResult> {
        let spec = match spec_for(
            &self.ctx,
            &arguments,
            &self.ledger_grants,
            LedgerAccess::Record,
        )
        .await
        {
            Ok(spec) => spec,
            Err(message) => return Ok(ToolResult::error(message)),
        };
        let id = text(&arguments, "id");
        match ledgers::record(
            &self.ctx,
            &spec,
            &self.author,
            &id,
            merge_fields(&arguments),
        )
        .await
        {
            Ok(entry) => Ok(ToolResult::success(format!(
                "Recorded `{}` on `{}`. It now reads:\n{}",
                entry.id,
                spec.slug,
                entry
                    .fields
                    .iter()
                    .map(|(name, value)| format!("- {name}: {value}"))
                    .collect::<Vec<_>>()
                    .join("\n")
            ))),
            Err(error) => Ok(ToolResult::error(format!("{error}"))),
        }
    }
}

// ---------------------------------------------------------------------------
// close_entry
// ---------------------------------------------------------------------------

struct CloseEntry {
    ctx: Ledgers,
    author: LedgerAuthor,
    ledger_grants: Option<Vec<LedgerGrant>>,
}

#[async_trait]
impl Tool for CloseEntry {
    fn name(&self) -> &str {
        CLOSE_ENTRY_TOOL
    }

    fn description(&self) -> &str {
        "Close a row on a ledger — done, dropped, met, missed — and say why. USE FOR finishing or \
         ruling out something the company recorded. The row is KEPT, not deleted: a known dead end \
         is a result, and the reason beside it is what stops the next person paying for it again. \
         A reason naming what actually settled it — 'the vendor delivered on the 4th' — saves the \
         next reader; 'done' saves nothing and costs the same to write. You cannot delete a row; \
         that is a person's call."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "ledger": ledger_argument(),
                "id": { "type": "string", "description": "The row to close." },
                "status": {
                    "type": "string",
                    "description": "The closing status this ledger declares — `list_ledgers` names them, and the refusal names them too if you guess."
                },
                "reason": {
                    "type": "string",
                    "description": "What came of it, in a sentence or two. Required by most ledgers, and the whole value of a closed row."
                }
            },
            "required": ["ledger", "id", "status", "reason"],
            "additionalProperties": false
        })
    }

    fn permission_level(&self) -> PermissionLevel {
        PermissionLevel::Write
    }

    async fn execute(&self, arguments: Value) -> anyhow::Result<ToolResult> {
        let spec = match spec_for(
            &self.ctx,
            &arguments,
            &self.ledger_grants,
            LedgerAccess::Record,
        )
        .await
        {
            Ok(spec) => spec,
            Err(message) => return Ok(ToolResult::error(message)),
        };
        match ledgers::close(
            &self.ctx,
            &spec,
            &self.author,
            &text(&arguments, "id"),
            &text(&arguments, "status"),
            &text(&arguments, "reason"),
        )
        .await
        {
            Ok(entry) => Ok(ToolResult::success(format!(
                "Closed `{}` on `{}` as `{}`. It stays on the ledger with the reason.",
                entry.id,
                spec.slug,
                entry.status(&spec)
            ))),
            Err(error) => Ok(ToolResult::error(format!("{error}"))),
        }
    }
}

// ---------------------------------------------------------------------------
// define_ledger
// ---------------------------------------------------------------------------

struct DefineLedger {
    ctx: Ledgers,
    can_declare_ledgers: bool,
}

#[async_trait]
impl Tool for DefineLedger {
    fn name(&self) -> &str {
        DEFINE_LEDGER_TOOL
    }

    fn description(&self) -> &str {
        "Declare a new ledger — an axis this company will need to look up again and none of the \
         existing ones holds. USE FOR a recurring kind of record: a hiring pipeline, customer \
         promises, experiments run, invoices chased. Check `list_ledgers` first; a near-duplicate \
         axis splits the record in two and neither half is trusted. It renders into `derived/` \
         immediately and everyone can read and write it. You cannot retire one — that is a \
         person's call — so declare an axis the company will keep, not a container for one task."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "slug": {
                    "type": "string",
                    "description": "Lowercase letters, digits and hyphens — how every tool will name it, e.g. `customer-promises`."
                },
                "title": { "type": "string", "description": "The rendered file's heading." },
                "purpose": {
                    "type": "string",
                    "description": "What this axis holds that no existing ledger does, and when somebody should read it. Everyone sees this line before they see any row."
                },
                "fields": {
                    "type": "array",
                    "description": "The fields a row carries. Exactly one must have role `id`. Give one `title` (the one-line summary a list shows) and one `status`.",
                    "items": {
                        "type": "object",
                        "properties": {
                            "name": { "type": "string" },
                            "role": {
                                "type": "string",
                                "enum": ["id", "title", "status", "prose", "refs", "owner", "date", "number"]
                            },
                            "description": { "type": "string" },
                            "required": { "type": "boolean" }
                        },
                        "required": ["name"]
                    }
                },
                "statuses": {
                    "type": "array",
                    "description": "The statuses a row may be in. Mark the ones that END a row `closed`, and set `needs_reason` on those — a row that closes without saying why is worth nothing later.",
                    "items": {
                        "type": "object",
                        "properties": {
                            "name": { "type": "string" },
                            "closed": { "type": "boolean" },
                            "needs_reason": { "type": "boolean" }
                        },
                        "required": ["name"]
                    }
                },
                "sections": {
                    "type": "array",
                    "description": "How the rendered file is laid out. Each section shows the statuses it names. Optional — one section holding everything is the default.",
                    "items": {
                        "type": "object",
                        "properties": {
                            "heading": { "type": "string" },
                            "blurb": { "type": "string" },
                            "statuses": { "type": "array", "items": { "type": "string" } },
                            "order": { "type": "string", "enum": ORDERS }
                        },
                        "required": ["heading"]
                    }
                },
                "checks": {
                    "type": "array",
                    "description": "Faults to report on rows: `required-field`, `known-status`, `closed-needs-reason`.",
                    "items": { "type": "string" }
                }
            },
            "required": ["slug", "title", "purpose", "fields", "statuses"],
            "additionalProperties": false
        })
    }

    fn permission_level(&self) -> PermissionLevel {
        PermissionLevel::Write
    }

    async fn execute(&self, arguments: Value) -> anyhow::Result<ToolResult> {
        if !self.can_declare_ledgers {
            return Ok(ToolResult::error(
                "Refused: your manifest sets `can_declare_ledgers = false`. Ask the operator to \
                 declare this axis, or to grant you `define_ledger`."
                    .to_string(),
            ));
        }
        match ledgers::define(&self.ctx, &arguments).await {
            Ok(spec) => Ok(ToolResult::success(format!(
                "Declared `{}`. It renders into `{}` and takes `record_entry` with \
                 `ledger: \"{}\"`.",
                spec.slug, spec.derived, spec.slug
            ))),
            Err(error) => Ok(ToolResult::error(format!("{error}"))),
        }
    }
}

/// Every tool name this module registers.
///
/// Named once so the capability filter and the grant checks cannot drift from
/// what is actually built.
pub const LEDGER_TOOL_NAMES: [&str; 5] = [
    LIST_LEDGERS_TOOL,
    READ_LEDGER_TOOL,
    RECORD_ENTRY_TOOL,
    CLOSE_ENTRY_TOOL,
    DEFINE_LEDGER_TOOL,
];

/// Silences the unused-import warning on [`Arc`] in builds that do not
/// construct a store here.
const _: Option<Arc<()>> = None;

#[cfg(test)]
#[path = "ledger_tools_test.rs"]
mod test;
