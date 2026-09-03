//! The rules only one code path enforces.

use std::collections::BTreeMap;

use serde_json::json;

use super::*;
use crate::company::runtime::CompanyRuntime;
use crate::ledger::LedgerAuthor;
use crate::ports::types::CompanyId;

/// The context under test, plus the runtime and home it borrows from.
async fn ledgers() -> (Ledgers, CompanyRuntime, tempfile::TempDir) {
    let (runtime, home) = runtime().await;
    let ctx = Ledgers::from(&runtime);
    (ctx, runtime, home)
}

async fn runtime() -> (CompanyRuntime, tempfile::TempDir) {
    let home = tempfile::tempdir().expect("tempdir");
    let manifest: crate::company::CompanyManifest = toml::from_str(
        r#"
        [company]
        name = "Acme"

        [[agent]]
        id = "ceo"
        role = "Chief"

        [policy]
        mode = "supervised"
        "#,
    )
    .expect("manifest");
    let runtime = crate::runtime::RuntimeBuilder::new(home.path().to_path_buf(), manifest)
        .with_id(CompanyId::new("acme"))
        .build()
        .await
        .expect("runtime");
    (runtime, home)
}

fn hazards() -> serde_json::Value {
    json!({
        "slug": "hazards",
        "title": "Hazards",
        "purpose": "What could go wrong.",
        "derived": "derived/hazards.md",
        "fields": [
            { "name": "id", "role": "id" },
            { "name": "risk", "role": "title" },
            { "name": "status", "role": "status" },
            { "name": "reason", "role": "prose" }
        ],
        "statuses": [
            { "name": "open" },
            { "name": "closed", "closed": true, "needs_reason": true }
        ],
        "sections": [
            { "heading": "Live", "statuses": ["open"], "order": "recent" },
            { "heading": "Closed", "statuses": ["closed"] }
        ],
        "checks": ["known-status", "closed-needs-reason"]
    })
}

fn fields(pairs: &[(&str, &str)]) -> BTreeMap<String, Option<String>> {
    pairs
        .iter()
        .map(|(k, v)| ((*k).to_string(), Some((*v).to_string())))
        .collect()
}

fn agent() -> LedgerAuthor {
    LedgerAuthor::agent("ceo")
}

fn person() -> LedgerAuthor {
    LedgerAuthor::human("u-1", "Dana")
}

/// A company starts with the three built-ins and nothing else.
#[tokio::test]
async fn a_fresh_company_has_the_built_ins_and_the_baseline() {
    let (ctx, _runtime, _home) = ledgers().await;
    let registry = registry(&ctx).await.expect("registry");
    // The built-ins first, in registry order, then whatever the global
    // baseline seeds (`crate::globals::ledgers`) — a company that starts with
    // nothing to record its risks, promises or learnings on gets one only if
    // some turn thinks to invent it.
    let slugs = registry.slugs();
    assert_eq!(&slugs[..3], ["tasks", "goals", "decisions"]);
    for global in crate::globals::ledgers() {
        assert!(slugs.contains(&global.slug), "`{}` is missing", global.slug);
    }
    assert!(registry.faults().is_empty());
}

/// An agent may declare an axis nobody anticipated — the whole point.
#[tokio::test]
async fn an_agent_may_declare_a_ledger_and_record_into_it() {
    let (ctx, _runtime, _home) = ledgers().await;
    let spec = define(&ctx, &hazards()).await.expect("declared");
    assert_eq!(spec.slug, "hazards");

    let entry = record(
        &ctx,
        &spec,
        &agent(),
        "vendor-slip",
        fields(&[("risk", "the vendor misses the date"), ("status", "open")]),
    )
    .await
    .expect("recorded");
    assert_eq!(entry.get("risk"), "the vendor misses the date");
    assert_eq!(entry.opened_by.kind, crate::ledger::AuthorKind::Agent);

    let registry = registry(&ctx).await.expect("registry");
    assert!(registry.find("hazards").is_some());
}

/// Recording twice against one id is an amendment, not a second row.
#[tokio::test]
async fn recording_again_amends_the_same_row() {
    let (ctx, _runtime, _home) = ledgers().await;
    let spec = define(&ctx, &hazards()).await.expect("declared");
    record(&ctx, &spec, &agent(), "r1", fields(&[("risk", "first")]))
        .await
        .expect("recorded");
    let amended = record(&ctx, &spec, &agent(), "r1", fields(&[("risk", "second")]))
        .await
        .expect("amended");
    assert_eq!(amended.events, 2);
    let read = read(&ctx, &spec, &Query::default()).await.expect("read");
    assert_eq!(read.entries.len(), 1);
    assert_eq!(read.entries[0].get("risk"), "second");
}

/// Refused at the **write**, not reported at the read: by the time somebody
/// reads it, the person who knew why has moved on.
#[tokio::test]
async fn closing_without_a_reason_is_refused() {
    let (ctx, _runtime, _home) = ledgers().await;
    let spec = define(&ctx, &hazards()).await.expect("declared");
    record(&ctx, &spec, &agent(), "r1", fields(&[("risk", "a")]))
        .await
        .expect("recorded");
    let error = record(&ctx, &spec, &agent(), "r1", fields(&[("status", "closed")]))
        .await
        .expect_err("no reason");
    assert!(format!("{error}").contains("reason"), "{error}");

    close(
        &ctx,
        &spec,
        &agent(),
        "r1",
        "closed",
        "the vendor delivered",
    )
    .await
    .expect("closed with a reason");
}

/// A row that already explained itself must not be refused for saying it twice.
#[tokio::test]
async fn a_reason_already_on_the_row_satisfies_the_close() {
    let (ctx, _runtime, _home) = ledgers().await;
    let spec = define(&ctx, &hazards()).await.expect("declared");
    record(
        &ctx,
        &spec,
        &agent(),
        "r1",
        fields(&[("risk", "a"), ("reason", "the vendor delivered")]),
    )
    .await
    .expect("recorded");
    record(&ctx, &spec, &agent(), "r1", fields(&[("status", "closed")]))
        .await
        .expect("the reason is already there");
}

#[tokio::test]
async fn an_undeclared_status_is_refused_and_names_the_real_ones() {
    let (ctx, _runtime, _home) = ledgers().await;
    let spec = define(&ctx, &hazards()).await.expect("declared");
    let error = record(
        &ctx,
        &spec,
        &agent(),
        "r1",
        fields(&[("status", "resolved")]),
    )
    .await
    .expect_err("unknown status");
    let message = format!("{error}");
    assert!(message.contains("resolved"), "{message}");
    assert!(message.contains("closed"), "{message}");
}

/// `close` refuses a status that closes nothing — the mistake a caller reaching
/// for "close" actually makes.
#[tokio::test]
async fn close_refuses_a_status_that_does_not_close() {
    let (ctx, _runtime, _home) = ledgers().await;
    let spec = define(&ctx, &hazards()).await.expect("declared");
    let error = close(&ctx, &spec, &agent(), "r1", "open", "done")
        .await
        .expect_err("open does not close");
    assert!(format!("{error}").contains("closed"), "{error}");
}

/// The rule, in the one place it lives. An agent's whole relationship with a
/// ledger is additive; deleting is not, and it is a person's call.
#[tokio::test]
async fn only_a_person_may_delete_a_row() {
    let (ctx, _runtime, _home) = ledgers().await;
    let spec = define(&ctx, &hazards()).await.expect("declared");
    record(&ctx, &spec, &agent(), "r1", fields(&[("risk", "a")]))
        .await
        .expect("recorded");

    let error = delete_entry(&ctx, &spec, &agent(), "r1")
        .await
        .expect_err("an agent may not delete");
    let message = format!("{error}");
    assert!(message.contains("only a person"), "{message}");
    assert!(message.contains("Close the row instead"), "{message}");
    // Refused, not silently ignored.
    assert!(
        read(&ctx, &spec, &Query::default())
            .await
            .expect("read")
            .entries
            .iter()
            .any(|entry| entry.id == "r1")
    );

    assert!(
        delete_entry(&ctx, &spec, &person(), "r1")
            .await
            .expect("a person may")
    );
    assert!(
        read(&ctx, &spec, &Query::default())
            .await
            .expect("read")
            .entries
            .is_empty()
    );
}

/// The runtime is not exempt either: a sweep that could delete rows is the same
/// loss with nobody to ask about it.
#[tokio::test]
async fn the_runtime_itself_may_not_delete_a_row() {
    let (ctx, _runtime, _home) = ledgers().await;
    let spec = define(&ctx, &hazards()).await.expect("declared");
    let error = delete_entry(&ctx, &spec, &LedgerAuthor::system("sweep"), "r1")
        .await
        .expect_err("system is not a person");
    assert!(format!("{error}").contains("only a person"), "{error}");
}

#[tokio::test]
async fn only_a_person_may_retire_a_ledger_and_the_rows_survive_it() {
    let (ctx, runtime, _home) = ledgers().await;
    let spec = define(&ctx, &hazards()).await.expect("declared");
    record(&ctx, &spec, &agent(), "r1", fields(&[("risk", "a")]))
        .await
        .expect("recorded");

    assert!(retire(&ctx, &agent(), "hazards", false).await.is_err());
    retire(&ctx, &person(), "hazards", false)
        .await
        .expect("a person may");
    assert!(
        registry(&ctx)
            .await
            .expect("registry")
            .find("hazards")
            .is_none()
    );

    // Retiring a ledger nobody reads is worth doing; deleting what it recorded
    // is a separate, explicit act.
    let events = runtime
        .ledgers()
        .events(runtime.id(), "hazards")
        .await
        .expect("events");
    assert_eq!(events.len(), 1, "the log survives the retirement");
}

#[tokio::test]
async fn a_built_in_cannot_be_retired() {
    let (ctx, _runtime, _home) = ledgers().await;
    let error = retire(&ctx, &person(), "goals", false)
        .await
        .expect_err("built in");
    assert!(
        format!("{error}").contains("ships with the runtime"),
        "{error}"
    );
}

/// The board keeps its own store, its own routes and its own dispatch edge, so
/// `record_entry` must refuse it — and say what does write it.
#[tokio::test]
async fn the_board_is_readable_through_the_ledger_surface_and_not_writable_by_it() {
    let (ctx, _runtime, _home) = ledgers().await;
    let registry = registry(&ctx).await.expect("registry");
    let tasks = registry.find("tasks").expect("built in");

    let error = record(&ctx, tasks, &agent(), "t1", fields(&[("title", "x")]))
        .await
        .expect_err("native");
    assert!(format!("{error}").contains("spawn_task"), "{error}");

    // Reading it works the same as reading any other ledger.
    let read = read(&ctx, tasks, &Query::default()).await.expect("read");
    assert!(read.entries.is_empty(), "a fresh company has no cards");

    // And so does deleting: a card is deleted through the board.
    let error = delete_entry(&ctx, tasks, &person(), "t1")
        .await
        .expect_err("native");
    assert!(format!("{error}").contains("elsewhere"), "{error}");
}

/// Every write re-renders, so `derived/` is never a stale copy of something.
#[tokio::test]
async fn a_write_publishes_the_derived_file() {
    let (ctx, runtime, _home) = ledgers().await;
    let spec = define(&ctx, &hazards()).await.expect("declared");
    record(
        &ctx,
        &spec,
        &agent(),
        "vendor-slip",
        fields(&[("risk", "the vendor misses the date"), ("status", "open")]),
    )
    .await
    .expect("recorded");

    let tree = runtime.workspace().tree(runtime.id()).await.expect("tree");
    let folder = tree
        .iter()
        .find(|node| node.name == "derived")
        .expect("the derived folder exists");
    let file = tree
        .iter()
        .find(|node| {
            node.parent_id.as_deref() == Some(folder.id.as_str()) && node.name == "hazards.md"
        })
        .expect("the ledger's file exists");
    let (_, body) = runtime
        .workspace()
        .read(runtime.id(), &file.id)
        .await
        .expect("read")
        .expect("present");
    assert!(body.contains("vendor-slip"), "{body}");
    assert!(body.contains("Do not edit this file"), "{body}");
}

/// A ledger is visible in `derived/` from the moment it exists, not from its
/// first row — a folder that gains a file only on first write reads as though
/// the ledger was never created.
#[tokio::test]
async fn declaring_a_ledger_publishes_its_empty_file() {
    let (ctx, runtime, _home) = ledgers().await;
    define(&ctx, &hazards()).await.expect("declared");
    let tree = runtime.workspace().tree(runtime.id()).await.expect("tree");
    assert!(tree.iter().any(|node| node.name == "hazards.md"));
}

/// A read that returned twenty rows must be distinguishable from one that
/// returned all of them.
#[tokio::test]
async fn a_read_is_bounded_and_says_how_many_matched() {
    let (ctx, _runtime, _home) = ledgers().await;
    let spec = define(&ctx, &hazards()).await.expect("declared");
    for n in 0..40 {
        record(
            &ctx,
            &spec,
            &agent(),
            &format!("r{n}"),
            fields(&[("risk", "a"), ("status", "open")]),
        )
        .await
        .expect("recorded");
    }
    let read = read(&ctx, &spec, &Query::default()).await.expect("read");
    assert_eq!(
        read.entries.len(),
        crate::ledger::budget::DEFAULT_READ_LIMIT
    );
    assert_eq!(read.matched, 40);

    let huge = read2(&ctx, &spec, 10_000).await;
    assert_eq!(
        huge.entries.len(),
        crate::ledger::budget::MAX_READ_LIMIT.min(40)
    );
}

async fn read2(ctx: &Ledgers, spec: &crate::ledger::LedgerSpec, limit: usize) -> Read {
    read(
        ctx,
        spec,
        &Query {
            limit: Some(limit),
            ..Query::default()
        },
    )
    .await
    .expect("read")
}

#[tokio::test]
async fn a_read_narrows_by_status_entry_and_text() {
    let (ctx, _runtime, _home) = ledgers().await;
    let spec = define(&ctx, &hazards()).await.expect("declared");
    record(
        &ctx,
        &spec,
        &agent(),
        "vendor",
        fields(&[("risk", "supplier misses the date"), ("status", "open")]),
    )
    .await
    .expect("recorded");
    record(
        &ctx,
        &spec,
        &agent(),
        "hiring",
        fields(&[("risk", "the role stays open")]),
    )
    .await
    .expect("recorded");
    close(&ctx, &spec, &agent(), "hiring", "closed", "role filled")
        .await
        .expect("recorded");

    let open = read(
        &ctx,
        &spec,
        &Query {
            status: Some("open".into()),
            ..Query::default()
        },
    )
    .await
    .expect("read");
    assert_eq!(open.entries.len(), 1);
    assert_eq!(open.entries[0].id, "vendor");

    let one = read(
        &ctx,
        &spec,
        &Query {
            entry: Some("hiring".into()),
            ..Query::default()
        },
    )
    .await
    .expect("read");
    assert_eq!(one.entries.len(), 1);

    let found = read(
        &ctx,
        &spec,
        &Query {
            text: Some("SUPPLIER".into()),
            ..Query::default()
        },
    )
    .await
    .expect("read");
    assert_eq!(found.entries.len(), 1);
}

/// A ledger whose required fields the read path polices, so a write that skips
/// one is exactly the write that folds into an unreadable row.
fn decisions() -> serde_json::Value {
    json!({
        "slug": "choices",
        "title": "Choices",
        "purpose": "What the work chose.",
        "derived": "derived/choices.md",
        "fields": [
            { "name": "id", "role": "id" },
            { "name": "decision", "role": "title", "required": true },
            { "name": "status", "role": "status", "required": true },
            { "name": "constraint", "role": "prose", "required": true },
            { "name": "reason", "role": "prose" }
        ],
        "statuses": [
            { "name": "proposed" },
            { "name": "settled" },
            { "name": "superseded", "closed": true, "needs_reason": true }
        ],
        "checks": ["required-field", "known-status", "closed-needs-reason"]
    })
}

/// The same shape with no `checks` at all — what `define_ledger` produces when
/// a caller omits them, where nothing reported the corruption either.
fn decisions_unchecked() -> serde_json::Value {
    let mut spec = decisions();
    spec["slug"] = json!("unchecked");
    spec["derived"] = json!("derived/unchecked.md");
    spec.as_object_mut().expect("object").remove("checks");
    spec
}

#[tokio::test]
async fn a_write_missing_a_required_field_is_refused_not_folded_unreadable() {
    let (ctx, _runtime, _home) = ledgers().await;
    let spec = define(&ctx, &decisions()).await.expect("declared");

    let error = record(
        &ctx,
        &spec,
        &agent(),
        "palette",
        fields(&[
            ("constraint", "the accessibility bar"),
            ("status", "proposed"),
        ]),
    )
    .await
    .expect_err("a row the ledger cannot read back must be refused");

    let message = error.to_string();
    assert!(message.contains("`decision`"), "names the field: {message}");
    assert!(message.contains("choices"), "names the ledger: {message}");

    // Refused, not merely reported: nothing landed to be read back.
    let after = read(&ctx, &spec, &Query::default()).await.expect("read");
    assert!(after.entries.is_empty(), "{:?}", after.entries);
    assert!(after.faults.is_empty(), "{:?}", after.faults);
}

/// The write refuses exactly what the read would have called unreadable. Two
/// implementations of "required" would drift; this pins them to each other.
#[tokio::test]
async fn the_write_refuses_what_the_read_would_report_unreadable() {
    let (ctx, _runtime, _home) = ledgers().await;
    let spec = define(&ctx, &decisions()).await.expect("declared");
    let missing = fields(&[("constraint", "the accessibility bar")]);

    let refused = record(&ctx, &spec, &agent(), "palette", missing.clone())
        .await
        .expect_err("refused");

    let folded = crate::ledger::engine::fold(
        &spec,
        &[crate::ledger::LedgerEvent {
            ledger: spec.slug.clone(),
            id: "palette".into(),
            author: agent(),
            at_millis: 0,
            fields: missing,
        }],
    );
    for name in ["decision", "status"] {
        assert!(
            folded.faults.iter().any(|fault| fault.contains(name)),
            "read reports `{name}` unreadable: {:?}",
            folded.faults
        );
        assert!(
            refused.to_string().contains(name),
            "write refuses for `{name}`: {refused}"
        );
    }
}

/// A required field already on the row does not have to be resent: the check
/// judges the merged row, so amending one field is a complete write.
#[tokio::test]
async fn an_amendment_need_not_resend_fields_the_row_already_holds() {
    let (ctx, _runtime, _home) = ledgers().await;
    let spec = define(&ctx, &decisions()).await.expect("declared");
    record(
        &ctx,
        &spec,
        &agent(),
        "palette",
        fields(&[
            ("decision", "a four-tone ramp"),
            ("status", "proposed"),
            ("constraint", "the accessibility bar"),
        ]),
    )
    .await
    .expect("recorded");

    record(
        &ctx,
        &spec,
        &agent(),
        "palette",
        fields(&[("status", "settled")]),
    )
    .await
    .expect("an amendment carrying only what changed is complete");

    let after = read(&ctx, &spec, &Query::default()).await.expect("read");
    assert!(after.faults.is_empty(), "{:?}", after.faults);
    assert_eq!(after.entries[0].get("decision"), "a four-tone ramp");
}

/// Clearing a required field is the same corruption arriving by another route.
#[tokio::test]
async fn clearing_a_required_field_is_refused() {
    let (ctx, _runtime, _home) = ledgers().await;
    let spec = define(&ctx, &decisions()).await.expect("declared");
    record(
        &ctx,
        &spec,
        &agent(),
        "palette",
        fields(&[
            ("decision", "a four-tone ramp"),
            ("status", "proposed"),
            ("constraint", "the accessibility bar"),
        ]),
    )
    .await
    .expect("recorded");

    let mut clearing = BTreeMap::new();
    clearing.insert("decision".to_string(), None);
    let error = record(&ctx, &spec, &agent(), "palette", clearing)
        .await
        .expect_err("clearing a required field must be refused");
    assert!(error.to_string().contains("`decision`"), "{error}");
}

/// A ledger declared without `checks` still refuses the write. `required` is
/// the ledger's schema; `checks` only chooses what a read reports — and a
/// declaration that omitted them accepted the corruption *and* stayed silent
/// about it, which is the worse of the two failures.
#[tokio::test]
async fn a_ledger_declaring_no_checks_still_refuses_a_missing_required_field() {
    let (ctx, _runtime, _home) = ledgers().await;
    let spec = define(&ctx, &decisions_unchecked())
        .await
        .expect("declared");
    assert!(spec.checks.is_empty(), "fixture declares no checks");

    let error = record(
        &ctx,
        &spec,
        &agent(),
        "palette",
        fields(&[("constraint", "the accessibility bar")]),
    )
    .await
    .expect_err("refused even with nothing set to report it");
    assert!(error.to_string().contains("`decision`"), "{error}");
}

/// Closing an id that does not exist opened a fresh row instead: closed, empty,
/// and named by the typo that produced it.
#[tokio::test]
async fn closing_an_unknown_id_is_refused_rather_than_opening_a_closed_row() {
    let (ctx, _runtime, _home) = ledgers().await;
    let spec = define(&ctx, &hazards()).await.expect("declared");

    let error = close(&ctx, &spec, &agent(), "typo", "closed", "role filled")
        .await
        .expect_err("closing a row that does not exist must be refused");
    let message = error.to_string();
    assert!(message.contains("typo"), "names the id: {message}");
    assert!(message.contains("hazards"), "names the ledger: {message}");

    let after = read(&ctx, &spec, &Query::default()).await.expect("read");
    assert!(after.entries.is_empty(), "no row was opened: {:?}", after);
}

/// `tasks` is the one built-in the runtime renders itself, and the only ledger
/// that can be native at all — `define` refuses the source for anything a
/// company declares — so this is the whole class, not one example of it.
async fn tasks_spec(ctx: &Ledgers) -> crate::ledger::LedgerSpec {
    registry(ctx)
        .await
        .expect("registry")
        .require("tasks")
        .expect("tasks is a built-in")
        .clone()
}

/// Closing an id on a ledger this tool does not write must say so, rather than
/// report on a row. "There is no such row" sends the caller looking for a row;
/// the ledger's own `written_by` sends them to the tool that owns the write.
#[tokio::test]
async fn closing_a_native_ledger_names_the_owning_tool_not_a_missing_row() {
    let (ctx, _runtime, _home) = ledgers().await;
    let spec = tasks_spec(&ctx).await;

    let error = close(&ctx, &spec, &agent(), "never-existed", "done", "finished")
        .await
        .expect_err("a native ledger is not written here");

    let message = error.to_string();
    assert!(
        !message.contains("there is no"),
        "must not report on a row the caller cannot write anyway: {message}"
    );
    assert!(
        message.contains("record_entry"),
        "names the tool that does not own this write: {message}"
    );
    assert!(
        message.contains(spec.written_by.split_whitespace().next().unwrap_or("task")),
        "keeps the ledger's own written_by guidance: {message}"
    );
}

/// The same precedence one level down: a caller who may not write the ledger
/// hears that, not that the status they chose does not close a row on it.
#[tokio::test]
async fn a_native_ledger_outranks_the_closing_status_check() {
    let (ctx, _runtime, _home) = ledgers().await;
    let spec = tasks_spec(&ctx).await;

    let error = close(&ctx, &spec, &agent(), "any", "not-a-closing-status", "why")
        .await
        .expect_err("a native ledger is not written here");
    assert!(
        error.to_string().contains("record_entry"),
        "the write guard outranks the status vocabulary: {error}"
    );
}

/// A value the caller got wrong outranks one they left out: told only that a
/// field is missing, a caller resends with the same rejected status and learns
/// the second half on a further round trip.
#[tokio::test]
async fn a_status_the_ledger_rejects_is_named_before_the_rows_gaps() {
    let (ctx, _runtime, _home) = ledgers().await;
    let spec = define(&ctx, &decisions()).await.expect("declared");

    let error = record(
        &ctx,
        &spec,
        &agent(),
        "palette",
        fields(&[("status", "banana")]),
    )
    .await
    .expect_err("refused");

    let message = error.to_string();
    assert!(
        message.contains("banana"),
        "names the bad status: {message}"
    );
    assert!(
        !message.contains("leaves"),
        "the missing-field report must not shadow it: {message}"
    );
}

#[tokio::test]
async fn an_unknown_sort_is_refused_rather_than_defaulted() {
    let (ctx, _runtime, _home) = ledgers().await;
    let spec = define(&ctx, &hazards()).await.expect("declared");
    let error = read(
        &ctx,
        &spec,
        &Query {
            sort: Some("newest".into()),
            ..Query::default()
        },
    )
    .await
    .expect_err("unknown sort");
    assert!(format!("{error}").contains("recorded"), "{error}");
}

/// Holding `record_entry` is not permission to write everything: the set of
/// ledgers is not fixed when tools are wired.
#[tokio::test]
async fn a_writers_list_is_enforced_at_the_write() {
    let (ctx, _runtime, _home) = ledgers().await;
    let mut document = hazards();
    document["writers"] = json!(["cfo"]);
    let spec = define(&ctx, &document).await.expect("declared");

    let error = record(&ctx, &spec, &agent(), "r1", fields(&[("risk", "a")]))
        .await
        .expect_err("ceo is not a writer");
    assert!(format!("{error}").contains("cfo"), "{error}");

    record(
        &ctx,
        &spec,
        &LedgerAuthor::agent("cfo"),
        "r1",
        fields(&[("risk", "a")]),
    )
    .await
    .expect("cfo may");
}

#[tokio::test]
async fn a_declaration_that_collides_is_refused() {
    let (ctx, _runtime, _home) = ledgers().await;
    define(&ctx, &hazards()).await.expect("declared");
    let error = define(&ctx, &hazards())
        .await
        .expect_err("already a ledger");
    assert!(format!("{error}").contains("hazards"), "{error}");

    let mut shadow = hazards();
    shadow["slug"] = json!("goals");
    shadow["derived"] = json!("derived/other.md");
    let error = define(&ctx, &shadow).await.expect_err("built in");
    assert!(format!("{error}").contains("built-in"), "{error}");
}

/// Over-long text is truncated rather than rejected: losing the tail of a long
/// note is a smaller failure than losing the whole write.
#[tokio::test]
async fn an_over_long_value_is_truncated_rather_than_refused() {
    let (ctx, _runtime, _home) = ledgers().await;
    let spec = define(&ctx, &hazards()).await.expect("declared");
    let entry = record(
        &ctx,
        &spec,
        &agent(),
        "r1",
        fields(&[("risk", &"x".repeat(20_000))]),
    )
    .await
    .expect("recorded");
    assert_eq!(
        entry.get("risk").chars().count(),
        crate::ledger::MAX_FIELD_CHARS
    );
}

/// A blank value clears the field rather than storing a present-but-empty one,
/// which would render an empty bullet under every row that ever set it.
#[tokio::test]
async fn a_blank_value_clears_the_field() {
    let (ctx, _runtime, _home) = ledgers().await;
    let spec = define(&ctx, &hazards()).await.expect("declared");
    record(&ctx, &spec, &agent(), "r1", fields(&[("risk", "a")]))
        .await
        .expect("recorded");
    let cleared = record(&ctx, &spec, &agent(), "r1", fields(&[("risk", "   ")]))
        .await
        .expect("recorded");
    assert_eq!(cleared.get("risk"), "");
}

/// The briefing is what a turn carries: every ledger named, every open row
/// identified, and the call that fetches the rest on each one.
#[tokio::test]
async fn the_briefing_names_every_ledger_and_how_to_read_more() {
    let (ctx, _runtime, _home) = ledgers().await;
    let spec = define(&ctx, &hazards()).await.expect("declared");
    record(
        &ctx,
        &spec,
        &agent(),
        "vendor-slip",
        fields(&[("risk", "a"), ("status", "open")]),
    )
    .await
    .expect("recorded");

    let registry = registry(&ctx).await.expect("registry");
    let briefing = briefing(&ctx, &registry).await.expect("briefing");
    for slug in ["tasks", "goals", "decisions", "hazards"] {
        assert!(briefing.contains(slug), "`{slug}` is missing: {briefing}");
    }
    assert!(briefing.contains("vendor-slip"), "{briefing}");
    assert!(briefing.contains("read_ledger"), "{briefing}");
}

#[tokio::test]
async fn republish_writes_every_ledgers_file() {
    let (ctx, runtime, _home) = ledgers().await;
    define(&ctx, &hazards()).await.expect("declared");
    let written = republish_all(&ctx).await.expect("republished");
    // Three built-ins, the baseline's own, and the one just declared.
    assert_eq!(written, 4 + crate::globals::ledgers().len());
    let tree = runtime.workspace().tree(runtime.id()).await.expect("tree");
    for name in ["tasks.md", "goals.md", "decisions.md", "hazards.md"] {
        assert!(
            tree.iter().any(|node| node.name == name),
            "`{name}` was not written"
        );
    }
}
