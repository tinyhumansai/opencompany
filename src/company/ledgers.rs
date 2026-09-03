//! The one place a ledger is read, written, declared or retired.
//!
//! Every surface — the REST routes, the agent tools, the console — comes
//! through here rather than reaching the [`LedgerStore`] directly, and that is
//! load-bearing rather than tidy. Four rules only hold if exactly one code path
//! enforces them:
//!
//! * **Only a person deletes.** [`delete_entry`] and [`retire`] check
//!   [`AuthorKind::may_delete`] on the caller, so an agent cannot reach a
//!   deletion by finding a route that forgot to.
//! * **A writer must be allowed to write.** [`LedgerSpec::writers`] is checked
//!   at call time, because the set of ledgers is not fixed when tools are
//!   wired — a company may declare one afterwards, so holding `record_entry` is
//!   not permission to write everything.
//! * **A close says why.** A closing status that declares `needs_reason` and is
//!   written with no reason is refused at the write rather than reported at the
//!   read, because a row that closed silently is worth nothing to whoever picks
//!   it up next and by then the person who knew has moved on.
//! * **The derived file follows the write.** Every mutation re-renders and
//!   republishes, so `derived/` is never a stale copy of something.

use std::collections::BTreeMap;
use std::sync::Arc;

use crate::Result;
use crate::company::runtime::CompanyRuntime;
use crate::error::OpenCompanyError;
use crate::ledger::{
    AuthorKind, Entries, LedgerAuthor, LedgerEvent, LedgerSource, LedgerSpec, MAX_FIELD_CHARS,
    Order, REASON_FIELD, Registry, budget, derived, engine, native, parse_order,
};
use crate::ports::now_millis;

/// Everything a ledger operation needs, without a whole [`CompanyRuntime`].
///
/// The routes have a runtime and the agent tools do not — they are built per
/// turn from the stores directly — so the service takes the three handles it
/// actually uses rather than the runtime. That is what lets the tools and the
/// routes share **one** implementation of the rules in this module's docs,
/// instead of the tools growing a second, slightly different copy.
#[derive(Clone)]
pub struct Ledgers {
    company: crate::ports::types::CompanyId,
    ledgers: Arc<dyn crate::ports::ledgers::LedgerStore>,
    /// Read only to project the native task board. A context without one reads
    /// `tasks` as empty, which is right for a caller that has no board rather
    /// than a reason to fail.
    tasks: Option<Arc<dyn crate::ports::tasks::TaskStore>>,
    /// Written only to publish a derived file. A context without one records
    /// perfectly well and simply publishes nothing.
    workspace: Option<Arc<dyn crate::ports::workspace::WorkspaceStore>>,
}

impl Ledgers {
    /// A context over the ledger store alone.
    pub fn new(
        company: crate::ports::types::CompanyId,
        ledgers: Arc<dyn crate::ports::ledgers::LedgerStore>,
    ) -> Self {
        Self {
            company,
            ledgers,
            tasks: None,
            workspace: None,
        }
    }

    /// Wires the board, so the `tasks` ledger reads.
    #[must_use]
    pub fn with_tasks(mut self, tasks: Arc<dyn crate::ports::tasks::TaskStore>) -> Self {
        self.tasks = Some(tasks);
        self
    }

    /// [`with_tasks`](Self::with_tasks), for a caller holding an `Option`.
    #[must_use]
    pub fn with_tasks_opt(
        mut self,
        tasks: Option<Arc<dyn crate::ports::tasks::TaskStore>>,
    ) -> Self {
        self.tasks = tasks;
        self
    }

    /// [`with_workspace`](Self::with_workspace), for a caller holding an
    /// `Option`.
    #[must_use]
    pub fn with_workspace_opt(
        mut self,
        workspace: Option<Arc<dyn crate::ports::workspace::WorkspaceStore>>,
    ) -> Self {
        self.workspace = workspace;
        self
    }

    /// Wires the workspace, so writes publish their `derived/` file.
    #[must_use]
    pub fn with_workspace(
        mut self,
        workspace: Arc<dyn crate::ports::workspace::WorkspaceStore>,
    ) -> Self {
        self.workspace = Some(workspace);
        self
    }

    /// The company these ledgers belong to.
    pub fn company(&self) -> &crate::ports::types::CompanyId {
        &self.company
    }
}

impl From<&CompanyRuntime> for Ledgers {
    fn from(runtime: &CompanyRuntime) -> Self {
        Ledgers::new(runtime.id().clone(), runtime.ledgers().clone())
            .with_tasks(runtime.tasks().clone())
            .with_workspace(runtime.workspace().clone())
    }
}

/// Builds the company's registry: the built-ins plus whatever it declared.
///
/// # Errors
///
/// Returns whatever the store returns. A declaration that will not load is a
/// [`Registry::faults`] entry rather than an error — a company that wrote one
/// bad ledger must still reach its board.
pub async fn registry(ctx: &Ledgers) -> Result<Registry> {
    let declared = ctx.ledgers.list_specs(&ctx.company).await?;
    Ok(Registry::build(declared))
}

/// Every entry on one ledger, folded and checked.
///
/// A [`LedgerSource::Native`] ledger is projected from the store that actually
/// owns it, so a caller reads the board and a declared ledger through the same
/// shape and never has to know which it got.
///
/// # Errors
///
/// Returns whatever the store returns.
pub async fn entries(ctx: &Ledgers, spec: &LedgerSpec) -> Result<Entries> {
    match spec.source {
        LedgerSource::Native => match &ctx.tasks {
            Some(tasks) => Ok(native::entries_from_tasks(&tasks.list(&ctx.company).await?)),
            None => Ok(Entries::default()),
        },
        LedgerSource::Events => {
            let events = ctx.ledgers.events(&ctx.company, &spec.slug).await?;
            Ok(engine::fold(spec, &events))
        }
    }
}

/// One ledger's rendered Markdown, as the `derived/` file holds it.
///
/// # Errors
///
/// Returns whatever the store returns.
pub async fn render(ctx: &Ledgers, spec: &LedgerSpec) -> Result<String> {
    Ok(engine::render(spec, &entries(ctx, spec).await?))
}

/// Every ledger's index, for a turn that needs to know what the company records.
///
/// Bounded by construction: an index carries identities and drops reasoning,
/// and each one ends with the call that fetches the rest. See
/// [`engine::index`] for why that closing line is not optional.
///
/// # Errors
///
/// Returns whatever the store returns.
pub async fn briefing(ctx: &Ledgers, registry: &Registry) -> Result<String> {
    let mut out = String::from(
        "## The company's ledgers\n\nEach of these is a durable record with its own rows. Read one \
         with `read_ledger`; record into one with `record_entry`.\n\n",
    );
    for spec in registry.specs() {
        let entries = entries(ctx, spec).await?;
        out.push_str(&engine::index(spec, &entries));
        out.push('\n');
    }
    Ok(out)
}

/// What one read returned.
#[derive(Clone, Debug)]
pub struct Read {
    /// The rows, already ordered and bounded.
    pub entries: Vec<engine::Entry>,
    /// How many matched before the bound was applied.
    pub matched: usize,
    /// What the declared checks found on the whole ledger.
    pub faults: Vec<String>,
}

/// What a read is narrowed by.
#[derive(Clone, Debug, Default)]
pub struct Query {
    /// One entry, by id.
    pub entry: Option<String>,
    /// Only entries in this status.
    pub status: Option<String>,
    /// Only entries whose id or any field contains this.
    pub text: Option<String>,
    /// `recorded` or `recent`; the ledger's own default when absent.
    pub sort: Option<String>,
    /// Rows to return. Bounded whatever is asked for.
    pub limit: Option<usize>,
}

/// Reads one ledger, narrowed and bounded.
///
/// The bound is applied whatever the caller asks for, and [`Read::matched`]
/// reports how many there were — so a caller that got a short list can tell the
/// difference between *that is all of them* and *that is the first twenty*.
///
/// # Errors
///
/// Returns whatever the store returns, or
/// [`OpenCompanyError::InvalidRequest`] when `sort` is not an order name.
pub async fn read(ctx: &Ledgers, spec: &LedgerSpec, query: &Query) -> Result<Read> {
    let folded = entries(ctx, spec).await?;
    let order = match query.sort.as_deref() {
        Some(name) => parse_order(name)?,
        None => spec.default_order(),
    };
    let mut rows: Vec<engine::Entry> = engine::ordered(&folded.entries, order)
        .into_iter()
        .filter(|entry| match &query.entry {
            Some(id) => entry.id == id.trim(),
            None => true,
        })
        .filter(|entry| match &query.status {
            // Both sides canonical (issue #1512): a query for a retired status
            // finds the rows that were stored under it, because they now render
            // under the status that adopted it and a filter that could not
            // reach them would report a row that is plainly in the file as
            // absent.
            Some(status) => entry
                .status(spec)
                .eq_ignore_ascii_case(spec.canonical_status(status.trim())),
            None => true,
        })
        .filter(|entry| match &query.text {
            Some(text) => entry.matches(text),
            None => true,
        })
        .cloned()
        .collect();
    let matched = rows.len();
    let limit = query
        .limit
        .unwrap_or(budget::DEFAULT_READ_LIMIT)
        .clamp(1, budget::MAX_READ_LIMIT);
    rows.truncate(limit);
    Ok(Read {
        entries: rows,
        matched,
        faults: folded.faults,
    })
}

/// Records one event against a ledger, and republishes its derived file.
///
/// Fields are trimmed and capped on the way in ([`MAX_FIELD_CHARS`]);
/// over-long text is truncated rather than rejected, because losing the tail of
/// a long note is a smaller failure than losing the whole write.
///
/// # Errors
///
/// Returns [`OpenCompanyError::InvalidRequest`] when the ledger is native (its
/// rows are written by the surface that owns them), when the author may not
/// write it, when the event names no entry, when it sets a status the ledger
/// does not declare, or when it closes a row into a status that demands a
/// reason without giving one.
/// Whether this author may write this ledger at all, and under what id.
///
/// Every one of these outranks anything read off the ledger's rows: a caller
/// holding the wrong ledger needs to hear that, and a report about some row's
/// contents would send them looking for a row instead of for the tool they
/// should be using. Returns the trimmed id the write proceeds under.
fn guard_write<'a>(spec: &LedgerSpec, author: &LedgerAuthor, id: &'a str) -> Result<&'a str> {
    if spec.source == LedgerSource::Native {
        return Err(OpenCompanyError::InvalidRequest(format!(
            "`{}` is not written with `record_entry`. {}",
            spec.slug, spec.written_by
        )));
    }
    if !spec.writable_by(&author.id) {
        return Err(OpenCompanyError::InvalidRequest(format!(
            "`{}` is written by {} on this company, and `{}` is not one of them.",
            spec.slug,
            spec.writers.join(", "),
            author.id
        )));
    }
    let id = id.trim();
    if id.is_empty() {
        return Err(OpenCompanyError::InvalidRequest(
            "an entry needs an id. Reuse an existing one to amend that row; a new one opens a new \
             row."
                .to_string(),
        ));
    }
    Ok(id)
}

pub async fn record(
    ctx: &Ledgers,
    spec: &LedgerSpec,
    author: &LedgerAuthor,
    id: &str,
    fields: BTreeMap<String, Option<String>>,
) -> Result<engine::Entry> {
    let id = guard_write(spec, author, id)?;

    let fields = normalize_fields(fields);
    // Every check below judges the row the write produces, not the event that
    // produces it: a ledger write is a merge, so an event supplying only the
    // fields that changed is complete whenever the row already holds the rest.
    let existing = entries(ctx, spec).await?;
    let prospective = existing.preview(id, &fields);

    if let Some(field) = spec.status_field()
        && let Some(Some(status)) = fields.get(&field.name)
    {
        // `declares_status`, not `knows_status`: a stored row heals onto a
        // surviving status when it is read, but a *write* of a retired word is
        // refused so the client learns the vocabulary once (issue #1512).
        if !spec.declares_status(status) {
            return Err(OpenCompanyError::InvalidRequest(format!(
                "`{status}` is not a status on `{}`; use one of: {}",
                spec.slug,
                spec.statuses
                    .iter()
                    .map(|declared| declared.name.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            )));
        }
        // The reason may arrive on this event or already be on the row, which
        // the merged row above already accounts for — otherwise closing a row
        // that already explained itself would be refused for saying it twice.
        if spec
            .status(status)
            .is_some_and(|declared| declared.needs_reason)
            && prospective.get(REASON_FIELD).trim().is_empty()
        {
            return Err(OpenCompanyError::InvalidRequest(format!(
                "closing `{id}` as `{status}` needs a `{REASON_FIELD}`. A row that does not say \
                 why it closed is worth nothing to whoever reads it next — and 'did not work' \
                 costs the same to write as the reason that would have saved them."
            )));
        }
    }

    // Last of the write checks: a value the caller got wrong is more useful to
    // report than one they left out, so a bad status is named before the row's
    // gaps are counted.
    let missing = engine::missing_required(&prospective, spec);
    if !missing.is_empty() {
        let declared: Vec<&str> = spec
            .fields
            .iter()
            .map(|field| field.name.as_str())
            .collect();
        return Err(OpenCompanyError::InvalidRequest(format!(
            "recording `{id}` on `{}` leaves {} unset, which this ledger requires. The row would \
             not be readable back — it would be reported under the ledger's unreadable rows \
             instead of appearing in its section. Send {} in this same call. `{}` declares: {}.",
            spec.slug,
            name_list(&missing),
            name_list(&missing),
            spec.slug,
            name_list(&declared),
        )));
    }

    let event = LedgerEvent {
        ledger: spec.slug.clone(),
        id: id.to_string(),
        author: author.clone(),
        at_millis: now_millis(),
        fields,
    };
    ctx.ledgers.append(&ctx.company, &event).await?;
    let folded = entries(ctx, spec).await?;
    publish(ctx, spec, &folded).await;
    folded.find(id).cloned().ok_or_else(|| {
        OpenCompanyError::NotFound(format!(
            "recorded `{id}` on `{}` but the fold did not return it",
            spec.slug
        ))
    })
}

/// Closes an entry: a status and a reason, which is [`record`] with two fields.
///
/// A separate entry point rather than a separate operation, because *the write
/// is still a merge* — see [`LedgerEvent`]. What it adds is the refusal to
/// close into a status that does not close anything, which is the mistake a
/// caller reaching for "close" actually makes.
///
/// # Errors
///
/// As [`record`], plus [`OpenCompanyError::InvalidRequest`] when `status` is
/// not one of the ledger's closing statuses.
pub async fn close(
    ctx: &Ledgers,
    spec: &LedgerSpec,
    author: &LedgerAuthor,
    id: &str,
    status: &str,
    reason: &str,
) -> Result<engine::Entry> {
    // First, ahead of everything read off the spec or the rows: on a ledger
    // this author may not write — a native one above all — every message below
    // answers a question the caller is not in a position to ask, and each one
    // buries the `written_by` line naming the tool that does own the write.
    let id = guard_write(spec, author, id)?;

    let closing = spec.closing_statuses();
    if !closing
        .iter()
        .any(|name| name.eq_ignore_ascii_case(status.trim()))
    {
        return Err(OpenCompanyError::InvalidRequest(if closing.is_empty() {
            format!(
                "`{}` declares no status that closes a row, so nothing on it can be closed. \
                 Record a status against it instead.",
                spec.slug
            )
        } else {
            format!(
                "`{status}` does not close a row on `{}`; use one of: {}",
                spec.slug,
                closing.join(", ")
            )
        }));
    }
    let Some(field) = spec.status_field() else {
        return Err(OpenCompanyError::InvalidRequest(format!(
            "`{}` declares no status field, so nothing on it has a status to close.",
            spec.slug
        )));
    };
    // Closing is an amendment to a row that exists: an id naming none would
    // otherwise open one, carrying nothing but the status that closed it.
    if entries(ctx, spec).await?.find(id).is_none() {
        return Err(OpenCompanyError::InvalidRequest(format!(
            "there is no `{id}` on `{}` to close. Check the id — closing a row that does not \
             exist would open one, closed and empty.",
            spec.slug
        )));
    }

    let mut fields = BTreeMap::new();
    fields.insert(field.name.clone(), Some(status.trim().to_string()));
    if !reason.trim().is_empty() {
        fields.insert(REASON_FIELD.to_string(), Some(reason.trim().to_string()));
    }
    record(ctx, spec, author, id, fields).await
}

/// Declares a new ledger.
///
/// Open to agents as well as people, deliberately: the whole point is that the
/// company discovers which axes it needs while it is running, and a declaration
/// that needed a release — or an operator — would be discovered and then not
/// made. What an agent cannot do is *undo* one; see [`retire`].
///
/// # Errors
///
/// Returns [`OpenCompanyError::InvalidRequest`] when the declaration is
/// malformed, collides with an existing ledger, or is past the cap.
pub async fn define(ctx: &Ledgers, document: &serde_json::Value) -> Result<LedgerSpec> {
    let spec = crate::ledger::parse(document, false)?;
    let registry = registry(ctx).await?;
    registry.admits(&spec)?;
    ctx.ledgers.put_spec(&ctx.company, &spec).await?;
    // Rendered immediately, empty, so the ledger is visible in `derived/` from
    // the moment it exists rather than from its first row. A folder that gains
    // a file only once somebody writes to it reads as though the ledger was
    // never created.
    let folded = entries(ctx, &spec).await?;
    publish(ctx, &spec, &folded).await;
    Ok(spec)
}

/// Retires a declared ledger.
///
/// **A person's call.** The rows are left where they are — a ledger nobody
/// reads is worth retiring and the record in it is not — unless `purge` is set,
/// which is the explicit *and delete what it holds* that only a person can ask
/// for.
///
/// # Errors
///
/// Returns [`OpenCompanyError::InvalidRequest`] when the author is not a person
/// or the slug names a built-in, and [`OpenCompanyError::NotFound`] when
/// nothing carries that slug.
pub async fn retire(ctx: &Ledgers, author: &LedgerAuthor, slug: &str, purge: bool) -> Result<()> {
    refuse_non_human(author, "retire a ledger")?;
    let registry = registry(ctx).await?;
    let spec = registry.require(slug)?;
    if spec.builtin {
        return Err(OpenCompanyError::InvalidRequest(format!(
            "`{}` ships with the runtime and cannot be retired. Every prompt and route naming it \
             is written against it existing.",
            spec.slug
        )));
    }
    ctx.ledgers.delete_spec(&ctx.company, &spec.slug).await?;
    if purge {
        ctx.ledgers.purge_ledger(&ctx.company, &spec.slug).await?;
    }
    Ok(())
}

/// Deletes one entry and everything recorded against it.
///
/// **A person's call**, and the only irreversible one on a ledger. An agent
/// that wants to be finished with a row closes it, which keeps the reason.
///
/// # Errors
///
/// Returns [`OpenCompanyError::InvalidRequest`] when the author is not a person
/// or the ledger is native (its rows are deleted through the surface that owns
/// them).
pub async fn delete_entry(
    ctx: &Ledgers,
    spec: &LedgerSpec,
    author: &LedgerAuthor,
    id: &str,
) -> Result<bool> {
    refuse_non_human(author, "delete a row")?;
    if spec.source == LedgerSource::Native {
        return Err(OpenCompanyError::InvalidRequest(format!(
            "`{}` keeps its rows elsewhere, so they are deleted there. {}",
            spec.slug, spec.written_by
        )));
    }
    let removed = ctx
        .ledgers
        .purge_entry(&ctx.company, &spec.slug, id.trim())
        .await?;
    if removed {
        let folded = entries(ctx, spec).await?;
        publish(ctx, spec, &folded).await;
    }
    Ok(removed)
}

/// Re-renders every ledger into `derived/`.
///
/// For the paths that change what a file *would* say without writing a row: a
/// declaration edited, a bound tightened, a company restored from a bundle. A
/// file rendered by a build that has since changed is a file whose reader is
/// being told last release's answer, and nothing on the write path would ever
/// notice.
///
/// # Errors
///
/// Returns whatever the store returns.
pub async fn republish_all(ctx: &Ledgers) -> Result<usize> {
    let registry = registry(ctx).await?;
    let mut written = 0;
    for spec in registry.specs() {
        let folded = entries(ctx, spec).await?;
        publish(ctx, spec, &folded).await;
        written += 1;
    }
    Ok(written)
}

/// Refuses anything but a person.
fn refuse_non_human(author: &LedgerAuthor, what: &str) -> Result<()> {
    if author.kind.may_delete() {
        return Ok(());
    }
    Err(OpenCompanyError::InvalidRequest(format!(
        "only a person may {what}. Everything a teammate does to a ledger is additive and \
         therefore recoverable by reading its log; a deletion is not, and a runtime where a turn \
         can erase the record of what it did is one whose record means nothing. Close the row \
         instead — it keeps the reason."
    )))
}

/// Trims and caps every value, dropping keys that are only whitespace.
/// Backtick-quoted names, comma separated, for an error a caller has to act on.
fn name_list(names: &[&str]) -> String {
    names
        .iter()
        .map(|name| format!("`{name}`"))
        .collect::<Vec<_>>()
        .join(", ")
}

fn normalize_fields(fields: BTreeMap<String, Option<String>>) -> BTreeMap<String, Option<String>> {
    fields
        .into_iter()
        .filter_map(|(name, value)| {
            let name = name.trim().to_string();
            if name.is_empty() {
                return None;
            }
            // An empty string clears, exactly as an explicit null does. A caller
            // sending `""` means "there is nothing here", and storing it as a
            // present-but-blank field would render an empty bullet under every
            // row that ever set it.
            let value = value.and_then(|text| {
                let text = text.trim();
                if text.is_empty() {
                    None
                } else {
                    Some(budget::truncate(text, MAX_FIELD_CHARS))
                }
            });
            Some((name, value))
        })
        .collect()
}

/// Writes the ledger's rendered file into `derived/`, logging a failure rather
/// than failing the write that produced it.
///
/// The store is the record and the file is a projection of it. A workspace that
/// refused the write — a quota, a backend hiccup — must not turn a row that
/// *was* recorded into a call that reports failure, because that is the one
/// outcome the caller cannot recover from: it will retry, and the retry will
/// record the row a second time.
async fn publish(ctx: &Ledgers, spec: &LedgerSpec, folded: &Entries) {
    let Some(workspace) = &ctx.workspace else {
        return;
    };
    let body = engine::render(spec, folded);
    if let Err(error) = derived::publish(workspace.as_ref(), &ctx.company, spec, &body).await {
        tracing::warn!(
            company = %ctx.company,
            ledger = %spec.slug,
            %error,
            "ledger recorded but its derived file could not be written"
        );
    }
}

/// The order names a caller may pass, for a schema or an error.
pub const SORTS: [&str; 2] = crate::ledger::ORDERS;

/// The order a ledger uses when a caller names none.
pub fn default_sort(spec: &LedgerSpec) -> Order {
    spec.default_order()
}

/// An author for a turn belonging to `agent`.
pub fn agent_author(agent: &str) -> LedgerAuthor {
    LedgerAuthor::agent(agent)
}

/// An author for a request made by a person.
pub fn human_author(user_id: &str, label: &str) -> LedgerAuthor {
    LedgerAuthor::human(user_id, label)
}

/// Whether `author` may delete, for a surface that wants to hide the control
/// rather than let it fail.
pub fn may_delete(author: &LedgerAuthor) -> bool {
    matches!(author.kind, AuthorKind::Human)
}

#[cfg(test)]
#[path = "ledgers_test.rs"]
mod test;
