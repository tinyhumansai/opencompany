//! Folding a ledger's event log into entries, and rendering those entries.
//!
//! One engine for every ledger, built-in or declared. After the fold they are
//! the same thing: a set of entries with ids, statuses and prose, rendered into
//! bounded sections.
//!
//! # Why the log is the ledger
//!
//! The board this replaces stored *current state* — a JSON array rewritten
//! whole on every write. That shape loses two things quietly. It loses **why**:
//! a card moved from In Review to Done records the landing and not the verdict.
//! And it loses **what was there before**: a rewrite that drops a row is
//! byte-identical to a rewrite that never had it, so a bug that eats work looks
//! exactly like work nobody did.
//!
//! An append-only log loses neither, and costs one fold on read. The fold is
//! linear in events and every reader takes its own view of the result, so
//! nothing that reads entries has its meaning changed by a section reordering
//! underneath it.

use std::collections::BTreeMap;
use std::fmt::Write as _;

use serde::{Deserialize, Serialize};

use super::budget;
use super::spec::{Check, FieldRole, LedgerSpec, Order, REASON_FIELD, Section};
use super::types::{LedgerAuthor, LedgerEvent};

/// One entry, folded from every event that named it.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Entry {
    /// The entry's identity.
    pub id: String,
    /// Every field recorded, **including ones the spec does not declare**.
    ///
    /// Deliberately kept rather than dropped. A ledger may be amended after
    /// rows were written against an older shape, and a recorded fact that
    /// vanishes because a declaration did not anticipate it is worse than one
    /// that shows up in the wrong place — only the second is visible. The
    /// undeclared ones are reported by [`Entries::faults`], never discarded.
    pub fields: BTreeMap<String, String>,
    /// Who wrote the entry's **first** event.
    pub opened_by: LedgerAuthor,
    /// Who wrote its **last**.
    pub updated_by: LedgerAuthor,
    /// When it was opened, epoch millis.
    pub opened_at_millis: u64,
    /// When it was last touched, epoch millis.
    pub updated_at_millis: u64,
    /// How many events fold into it.
    pub events: u32,
    /// Where this entry's **last** event sits in the log.
    ///
    /// What [`Order::Recent`] sorts on. The log position is the ordering, not
    /// the timestamp: timestamps are written by whichever replica is running
    /// and a clock that steps backwards would shuffle the board, while the
    /// log's own append order is the only thing the fold can read that is
    /// guaranteed monotone.
    pub touched: usize,
}

impl Entry {
    /// A field's value, or the empty string.
    pub fn get(&self, name: &str) -> &str {
        self.fields.get(name).map_or("", String::as_str)
    }

    /// The entry's status under `spec`, lowercased, or the empty string.
    ///
    /// **Canonical**, so a row stored under a status the spec has since retired
    /// reports the one that adopted it (issue #1512). Every caller — the
    /// section membership test, the closed count, the rendered row — asks here,
    /// which is what keeps them from disagreeing about where a migrated row
    /// belongs. See [`StatusSpec::aliases`](crate::ledger::spec::StatusSpec::aliases).
    pub fn status(&self, spec: &LedgerSpec) -> String {
        spec.status_field()
            .map(|field| {
                let stored = self.get(&field.name).trim().to_ascii_lowercase();
                spec.canonical_status(&stored).to_string()
            })
            .unwrap_or_default()
    }

    /// The one-line summary a rendered row carries.
    pub fn title(&self, spec: &LedgerSpec) -> &str {
        spec.title_field().map_or("", |field| self.get(&field.name))
    }

    /// Whether any field contains `needle`, case-insensitively.
    pub fn matches(&self, needle: &str) -> bool {
        let needle = needle.trim().to_ascii_lowercase();
        if needle.is_empty() {
            return true;
        }
        self.id.to_ascii_lowercase().contains(&needle)
            || self
                .fields
                .values()
                .any(|value| value.to_ascii_lowercase().contains(&needle))
    }
}

/// Every entry a ledger holds, with the faults found reading them.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Entries {
    /// In first-seen order — the order ids first appeared in the log.
    pub entries: Vec<Entry>,
    /// What the declared checks found.
    ///
    /// Reported rather than acted on: an entry silently discarded leaves the
    /// ledger reading as though it recorded something.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub faults: Vec<String>,
}

impl Entries {
    /// The entry named `id`, if there is one.
    pub fn find(&self, id: &str) -> Option<&Entry> {
        let id = id.trim();
        self.entries.iter().find(|entry| entry.id == id)
    }

    /// The row `id` becomes if `fields` are recorded against it now.
    ///
    /// A write is a merge, so what a check must judge is the resulting row and
    /// not the event: a field already on the row is still there afterwards, and
    /// an event supplying only the rest is complete. Built with the same merge
    /// [`fold`] applies, so a write-time check sees the row a later read folds.
    pub fn preview(&self, id: &str, fields: &BTreeMap<String, Option<String>>) -> Entry {
        let id = id.trim();
        let mut entry = self.find(id).cloned().unwrap_or_else(|| Entry {
            id: id.to_string(),
            ..Entry::default()
        });
        merge_fields(&mut entry, fields);
        entry
    }

    /// How many entries are in a status the spec calls closed.
    pub fn closed_count(&self, spec: &LedgerSpec) -> usize {
        self.entries
            .iter()
            .filter(|entry| spec.is_closed(&entry.status(spec)))
            .count()
    }

    /// How many entries are still open.
    pub fn open_count(&self, spec: &LedgerSpec) -> usize {
        self.entries.len() - self.closed_count(spec)
    }
}

/// Folds an event log into entries and runs the declared checks.
///
/// Events are applied in the order given; the caller's store is what keeps that
/// order. A duplicate id is an amendment, not a second row — that is what makes
/// closing a row and re-prioritising it the same operation.
pub fn fold(spec: &LedgerSpec, events: &[LedgerEvent]) -> Entries {
    let mut out = Entries::default();
    // Insertion order is the order ids were first seen, which is the order a
    // reader expects a list to keep. A map keyed by id would sort
    // alphabetically, which is no order at all.
    let mut index: BTreeMap<String, usize> = BTreeMap::new();
    for (position, event) in events.iter().enumerate() {
        let id = event.id.trim();
        if id.is_empty() {
            out.faults.push(format!(
                "event {} names no entry and was skipped",
                position + 1
            ));
            continue;
        }
        let slot = *index.entry(id.to_string()).or_insert_with(|| {
            out.entries.push(Entry {
                id: id.to_string(),
                opened_by: event.author.clone(),
                opened_at_millis: event.at_millis,
                ..Entry::default()
            });
            out.entries.len() - 1
        });
        let entry = &mut out.entries[slot];
        entry.updated_by = event.author.clone();
        entry.updated_at_millis = event.at_millis;
        entry.events = entry.events.saturating_add(1);
        // Every event, not only the first: the last one to name an id is what
        // `Order::Recent` reads, which is what lets a writer raise an existing
        // row by recording against it.
        entry.touched = position;
        merge_fields(entry, &event.fields);
    }
    check(&mut out, spec);
    out
}

/// Applies one event's fields to a row: a value sets, a null clears.
fn merge_fields(entry: &mut Entry, fields: &BTreeMap<String, Option<String>>) {
    for (name, value) in fields {
        match value {
            Some(text) => {
                entry.fields.insert(name.clone(), text.clone());
            }
            None => {
                entry.fields.remove(name);
            }
        }
    }
}

/// The required fields a row leaves unfilled, in declaration order.
///
/// Shared by the read-time fault list and the write-time refusal: a ledger that
/// would report a row unreadable is the same ledger that must not accept the
/// write, and two implementations of "required" would eventually disagree about
/// which rows those are.
pub fn missing_required<'a>(entry: &Entry, spec: &'a LedgerSpec) -> Vec<&'a str> {
    spec.fields
        .iter()
        .filter(|field| field.required)
        .filter(|field| {
            // The id lives *beside* the fields, not inside them: every event
            // names it at the top level. Looking for it in `fields` fails for
            // every well-formed entry, so a spec declaring its id required
            // would call each of its own rows unreadable.
            let present = if field.role == FieldRole::Id {
                !entry.id.trim().is_empty()
            } else {
                !entry.get(&field.name).trim().is_empty()
            };
            !present
        })
        .map(|field| field.name.as_str())
        .collect()
}

/// Runs the declared checks, appending what they find to the fault list.
fn check(entries: &mut Entries, spec: &LedgerSpec) {
    let mut faults = Vec::new();
    for entry in &entries.entries {
        for declared in &spec.checks {
            match declared {
                Check::RequiredField => {
                    for name in missing_required(entry, spec) {
                        faults.push(format!(
                            "`{}` has no `{}`, which this ledger requires",
                            entry.id, name
                        ));
                    }
                }
                Check::KnownStatus => {
                    let status = entry.status(spec);
                    if !status.is_empty() && !spec.knows_status(&status) {
                        faults.push(format!(
                            "`{}` is in status `{status}`, which this ledger does not declare",
                            entry.id
                        ));
                    }
                }
                Check::ClosedNeedsReason => {
                    let status = entry.status(spec);
                    if spec
                        .status(&status)
                        .is_some_and(|declared| declared.needs_reason)
                        && entry.get(REASON_FIELD).trim().is_empty()
                    {
                        faults.push(format!(
                            "`{}` was closed as `{status}` with no `{REASON_FIELD}` — a row that \
                             does not say why is worth nothing to whoever reads it next",
                            entry.id
                        ));
                    }
                }
            }
        }
    }
    entries.faults.extend(faults);
}

/// Puts entries in the order a section — or a read — asked for.
///
/// Ordering happens here rather than in [`fold`], so the fold keeps saying one
/// thing (first-seen order) and every reader picks its own view of it.
pub fn ordered(entries: &[Entry], order: Order) -> Vec<&Entry> {
    let mut out: Vec<&Entry> = entries.iter().collect();
    if order == Order::Recent {
        out.sort_by_key(|entry| std::cmp::Reverse(entry.touched));
    }
    out
}

/// Renders the derived Markdown file every reader opens.
///
/// Written into the workspace's `derived/` folder on every write to the ledger.
/// The header says so, in the file itself, because the guard that refuses a
/// hand edit is only half the answer — the other half is that somebody reading
/// the file knows before they try.
pub fn render(spec: &LedgerSpec, entries: &Entries) -> String {
    let mut out = format!("# {}\n\n", spec.title);
    if !spec.purpose.is_empty() {
        let _ = writeln!(out, "{}\n", spec.purpose);
    }
    let _ = writeln!(
        out,
        "_Derived from the `{}` ledger and rewritten on every write. **Do not edit this file** — \
         the next write re-derives it and your edit is gone. Write it with {}._\n",
        spec.slug, spec.written_by
    );

    if entries.entries.is_empty() {
        let _ = writeln!(out, "_Nothing recorded yet._");
    }

    if spec.sections.is_empty() {
        append_section(
            &mut out,
            spec,
            entries,
            &Section {
                heading: "Entries".to_string(),
                blurb: String::new(),
                statuses: Vec::new(),
                cap: budget::MAX_LISTED,
                order: Order::Recorded,
            },
        );
    }
    for section in &spec.sections {
        append_section(&mut out, spec, entries, section);
    }
    append_faults(&mut out, spec, entries);
    out
}

fn append_section(out: &mut String, spec: &LedgerSpec, entries: &Entries, section: &Section) {
    let sorted = ordered(&entries.entries, section.order);
    let matching = sorted.into_iter().filter(|entry| {
        section.statuses.is_empty()
            || section
                .statuses
                .iter()
                .any(|wanted| wanted.eq_ignore_ascii_case(&entry.status(spec)))
    });
    let (rows, dropped) = budget::listed(matching, section.cap, |rows, entry| {
        let title = entry.title(spec);
        if title.trim().is_empty() {
            let _ = writeln!(rows, "- `{}`", entry.id);
        } else {
            let _ = writeln!(
                rows,
                "- `{}` — {}",
                entry.id,
                budget::truncate(title, budget::REASON_CHARS)
            );
        }
        // Every declared field the row filled in, one indented line each. A row
        // that carried only its title would make a second read mandatory to
        // learn anything, and the point of the derived file is that a reader
        // opens it and is done.
        for field in &spec.fields {
            if matches!(field.role, FieldRole::Id | FieldRole::Title) {
                continue;
            }
            let value = entry.get(&field.name);
            if value.trim().is_empty() {
                continue;
            }
            let _ = writeln!(
                rows,
                "  - {}: {}",
                field.name,
                budget::truncate(value, budget::REASON_CHARS)
            );
        }
    });
    if rows.is_empty() {
        return;
    }
    let _ = write!(out, "\n## {}\n\n", section.heading);
    if !section.blurb.is_empty() {
        let _ = write!(out, "{}\n\n", section.blurb);
    }
    out.push_str(&rows);
    out.push_str(&budget::elided(dropped, &spec.slug));
}

fn append_faults(out: &mut String, spec: &LedgerSpec, entries: &Entries) {
    if entries.faults.is_empty() {
        return;
    }
    out.push_str(
        "\n## Rows that could not be read\n\nReported rather than dropped: an entry silently \
         discarded leaves the ledger reading as though it recorded something.\n\n",
    );
    let (rows, dropped) = budget::listed(&entries.faults, budget::MAX_LISTED, |rows, fault| {
        let _ = writeln!(rows, "- {}", budget::truncate(fault, budget::REASON_CHARS));
    });
    out.push_str(&rows);
    out.push_str(&budget::elided(dropped, &spec.slug));
}

/// Closed rows per status an index carries.
///
/// Enough that *what did we just finish* is answerable without a second call —
/// the question somebody picking up work actually asks — and few enough that a
/// long-lived company's archive stops being re-sent on every turn that holds
/// the ledger. The rest is one read away, and the index says so by status.
const CLOSED_IN_INDEX: usize = 5;

/// Renders the short form a turn carries in place of [`render`]'s output.
///
/// # Why not just a sentence saying the ledger exists
///
/// Because the obligation a ledger discharges is **specific**. It is not *be
/// aware there are decisions*, it is *do not re-litigate this one*. It is not
/// *risks exist*, it is *do not re-raise this*. A description discharges
/// neither, because neither is about the ledger — both are about the rows.
///
/// So an index keeps every row's **identity** and drops the **reasoning**: one
/// line per row with its id, its status and a headline, ending with the call
/// that fetches the rest. Every open row is carried; each closed status keeps
/// its most recent few and the header says how many it left and where they are.
///
/// **The saving is only real if the pull happens.** A reader that never fetches
/// the rest is cheaper *and dumber* — strictly worse than being handed the
/// whole file, because it reads a shortened list, concludes there is nothing
/// more, and re-proposes what was cut. That is why the closing line is not
/// optional and why it names the exact call.
pub fn index(spec: &LedgerSpec, entries: &Entries) -> String {
    let (kept, elided) = indexed_entries(spec, entries);
    let mut out = format!("### {} (`{}`)\n\n", spec.title, spec.slug);
    if !spec.purpose.is_empty() {
        let _ = writeln!(out, "{}\n", spec.purpose);
    }
    if kept.is_empty() {
        let _ = writeln!(out, "_Nothing recorded yet._");
        return out;
    }
    for entry in kept {
        let status = entry.status(spec);
        let headline = budget::truncate(entry.title(spec), INDEX_HEADLINE);
        match (status.is_empty(), headline.is_empty()) {
            (true, true) => {
                let _ = writeln!(out, "- `{}`", entry.id);
            }
            (true, false) => {
                let _ = writeln!(out, "- `{}` — {headline}", entry.id);
            }
            (false, true) => {
                let _ = writeln!(out, "- `{}` [{status}]", entry.id);
            }
            (false, false) => {
                let _ = writeln!(out, "- `{}` [{status}] — {headline}", entry.id);
            }
        }
    }
    if !elided.is_empty() {
        let _ = writeln!(
            out,
            "\n_The closed rows above are the {CLOSED_IN_INDEX} most recent of each kind. Not \
             shown: {}._",
            elided.join(", ")
        );
    }
    let _ = writeln!(
        out,
        "\n_This is a shortened copy. Read a row in full with `read_ledger {{ ledger: \"{}\", \
         entry: \"<id>\" }}`._",
        spec.slug
    );
    out
}

/// Characters of headline an index line carries.
///
/// Narrower than a rendered section's prose bound, because an index line exists
/// to say *which row this is*, not what it holds.
const INDEX_HEADLINE: usize = 140;

/// The entries an index carries, and a phrase per status it cut rows from.
fn indexed_entries<'a>(spec: &LedgerSpec, entries: &'a Entries) -> (Vec<&'a Entry>, Vec<String>) {
    let mut open: Vec<&Entry> = Vec::new();
    // Most recent first within a status, whatever order the ledger renders its
    // own sections in: "what closed latest" is the question being answered.
    let mut archive: BTreeMap<String, Vec<&Entry>> = BTreeMap::new();
    for entry in ordered(&entries.entries, spec.default_order()) {
        let status = entry.status(spec);
        if spec.is_closed(&status) {
            archive.entry(status).or_default().push(entry);
        } else {
            open.push(entry);
        }
    }
    let mut elided = Vec::new();
    for (status, mut rows) in archive {
        rows.sort_by_key(|entry| std::cmp::Reverse(entry.touched));
        if rows.len() > CLOSED_IN_INDEX {
            elided.push(format!("{} more `{status}`", rows.len() - CLOSED_IN_INDEX));
            rows.truncate(CLOSED_IN_INDEX);
        }
        open.extend(rows);
    }
    (open, elided)
}

#[cfg(test)]
#[path = "engine_test.rs"]
mod test;
