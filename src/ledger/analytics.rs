//! What a ledger write is allowed to say to analytics: **the shape and the
//! count, never a cell**.
//!
//! A ledger holds the company's own business data — customer names, amounts,
//! addresses, the reason a deal was lost. It is the single richest source of
//! user content in this product, which makes it the one place a telemetry leak
//! would hurt most. So the write path reports two things and nothing else:
//! which *kind* of ledger was written, and how many records the write carried.
//!
//! # Why the slug is folded rather than passed through
//!
//! A ledger's slug is **author-defined at runtime**
//! ([`crate::company::ledgers::define`]): an agent or an operator invents one
//! while the company is running, and nothing in this repository reviews it.
//! `acme-holdings-merger` is a perfectly legal slug, and it names the customer.
//! Passing it through would be exactly the leak `crate::analytics::types`
//! exists to prevent — arriving, as such leaks always do, by way of a value
//! nobody anticipated.
//!
//! [`shape_slug`] therefore folds onto [`SHAPES`], a list compiled into this
//! binary, and everything else becomes
//! [`OTHER`](crate::analytics::types::OTHER). That loses segmentation on the
//! ledgers a company invented, which is the correct direction for a default:
//! *less information*, never *more*.
//!
//! # Why these six
//!
//! They are the ledgers **every company has**, whichever vertical it started
//! from, and their names are ours rather than a customer's:
//!
//! * `tasks`, `goals`, `decisions` — the built-in registry
//!   ([`crate::ledger::registry::builtin_documents`]).
//! * `risks`, `commitments`, `learnings` — the global baseline authored in
//!   `globals/ledgers/` and embedded at build time
//!   (`docs/spec/runtime/globals.md`).
//!
//! A company bundle's own ledgers (`companies/*/ledgers/*.toml` — `candidates`,
//! `deals`, `matters`, and ninety more) are deliberately **not** here. They are
//! per-vertical data files rather than a vocabulary this module owns, the set
//! turns over whenever a template is added, and a company can declare a ledger
//! at runtime under any of those names anyway — so listing them would buy a
//! little segmentation in exchange for a vocabulary that nobody could review as
//! a closed set. They fold to `other` alongside the runtime-declared ones.

use crate::analytics::types::OTHER;
use crate::analytics::{Event, Tracker};

/// The ledger shapes an analytics payload may name.
///
/// Enumerated by hand, on purpose, for the same reason
/// `crate::analytics::test::vocabulary` is: this is the list a reviewer reads
/// to answer "what can a ledger write send?", and a list derived from the code
/// would answer that question with itself. [`the_fold_agrees_with_the_list`]
/// keeps it and [`shape_slug`] from drifting apart, and
/// [`every_built_in_ledger_is_named`] fails if a built-in is added without
/// being recorded here.
pub const SHAPES: &[&str] = &[
    // The built-in registry.
    "tasks",
    "goals",
    "decisions",
    // The global baseline every company gets.
    "risks",
    "commitments",
    "learnings",
];

/// Folds a ledger slug onto [`SHAPES`], or [`OTHER`].
///
/// Lower-cased first: `normalize_slug` already lower-cases what it stores, but
/// a classifier that only recognises the spelling it expects would send a
/// capitalised built-in to `other` and quietly under-count it.
pub fn shape_slug(slug: &str) -> &'static str {
    let lower = slug.trim().to_ascii_lowercase();
    match lower.as_str() {
        "tasks" => "tasks",
        "goals" => "goals",
        "decisions" => "decisions",
        "risks" => "risks",
        "commitments" => "commitments",
        "learnings" => "learnings",
        _ => OTHER,
    }
}

/// Reports one ledger append: the folded shape and the count.
///
/// The **only** constructor of [`Event::LedgerAppended`] in the crate, so no
/// call site can choose to pass a raw slug — or, worse, a field value — through
/// on its own. Same reasoning as [`Event::metered`]: a constructor that folds
/// is a guarantee, a convention that call sites should fold is a hope.
///
/// Synchronous and infallible, like every [`Tracker::track`]: a ledger write
/// must never be delayed by, or fail because of, telemetry.
pub fn track_append(tracker: &dyn Tracker, slug: &str, records: u64) {
    tracker.track(Event::LedgerAppended {
        shape: shape_slug(slug),
        records,
    });
}

#[cfg(test)]
mod test {
    use super::*;

    use crate::analytics::types::OpaqueId;
    use crate::analytics::{Envelope, RecordingTracker, payload};
    use crate::app::deployment::Deployment;
    use crate::ports::brain::{Cognition, UsageMetering};

    fn envelope() -> Envelope {
        Envelope::new(
            OpaqueId::instance("0123456789abcdef0123456789abcdef"),
            Deployment::HostedTenant,
            Cognition {
                path: "harness",
                provider: "openrouter",
                model: None,
                metering: UsageMetering::PerTurn,
            },
        )
    }

    /// The event fires with the shape and the count the caller named.
    #[test]
    fn an_append_reports_its_shape_and_its_count() {
        let tracker = RecordingTracker::new();
        track_append(&tracker, "decisions", 1);
        assert_eq!(
            tracker.events(),
            vec![Event::LedgerAppended {
                shape: "decisions",
                records: 1,
            }]
        );

        let rendered = payload(&envelope(), &tracker.events()[0]);
        assert_eq!(rendered["event"], "ledger_appended");
        assert_eq!(rendered["properties"]["shape"], "decisions");
        assert_eq!(rendered["properties"]["records"], 1);
    }

    /// Every built-in and global-baseline slug survives the fold as itself.
    #[test]
    fn the_fold_agrees_with_the_list() {
        for shape in SHAPES {
            assert_eq!(
                shape_slug(shape),
                *shape,
                "`{shape}` is in SHAPES but `shape_slug` does not name it — the two lists have \
                 drifted, and the reviewable one is now wrong"
            );
        }
    }

    /// A ledger every company has, added without being recorded in [`SHAPES`],
    /// would be folded to `other` — silently merging the product's own axes
    /// with every ledger a company invented, which is the one way this
    /// vocabulary can rot without anybody noticing. This is what says so out
    /// loud, from both sources rather than from a list retyped here.
    #[test]
    fn every_ledger_every_company_has_is_named() {
        let (builtins, _faults) = crate::ledger::registry::builtins();
        let baseline = crate::globals::ledgers().iter().cloned();
        for spec in builtins.into_iter().chain(baseline) {
            assert_ne!(
                shape_slug(&spec.slug),
                OTHER,
                "`{}` is a ledger every company has, and it folds to `other`; add it to `SHAPES`",
                spec.slug
            );
        }
    }

    /// A slug nobody compiled in — the runtime-declared case — reaches `other`.
    #[test]
    fn a_declared_slug_reaches_the_catch_all() {
        for slug in [
            "acmecorp-holdings",
            "project-titan",
            "candidates",
            "",
            "   ",
        ] {
            assert_eq!(shape_slug(slug), OTHER, "{slug:?}");
        }
    }

    /// Case and surrounding whitespace do not turn a built-in into `other`.
    #[test]
    fn a_built_in_is_recognised_however_it_is_spelled() {
        assert_eq!(shape_slug("  Decisions "), "decisions");
        assert_eq!(shape_slug("GOALS"), "goals");
    }

    /// **The write path actually reports itself**, and reports the shape and
    /// the count rather than the row.
    ///
    /// Through `company::ledgers::record` — the crate's only ledger append —
    /// rather than by calling [`track_append`] directly, because the thing
    /// worth proving is that the emit is wired to the append. A unit test of
    /// the helper alone would pass with the call site deleted.
    #[tokio::test]
    async fn a_recorded_entry_reports_its_shape_and_carries_no_cell() {
        use crate::company::ledgers::{self, Ledgers};
        use crate::ports::types::CompanyId;
        use crate::store::FsOps;

        // Every kind of content a ledger holds, all of it hostile: the slug an
        // author invented, and the cells, which are the company's own business
        // data outright.
        const HOSTILE: &[&str] = &[
            "acmecorp-holdings-merger",
            "founder@acme.example",
            "1250000 payable on close",
            "sk-not-a-real-key",
        ];

        let home = tempfile::Builder::new()
            .prefix("oc-ledger-analytics-")
            .tempdir()
            .expect("tempdir");
        let recorder = std::sync::Arc::new(RecordingTracker::new());
        let ctx = Ledgers::new(
            CompanyId::new("acme"),
            std::sync::Arc::new(FsOps::new(home.path().to_path_buf())),
        )
        .with_analytics(recorder.clone());

        let spec = ledgers::define(
            &ctx,
            &serde_json::json!({
                "slug": "acmecorp-holdings-merger",
                "title": "Merger",
                "purpose": "What we agreed.",
                "fields": [
                    { "name": "id", "role": "id" },
                    { "name": "headline", "role": "title" },
                    { "name": "status", "role": "status" },
                    { "name": "counterparty", "role": "prose" },
                    { "name": "amount", "role": "prose" }
                ],
                "statuses": [{ "name": "open" }],
            }),
        )
        .await
        .expect("a declared ledger");

        let mut fields = std::collections::BTreeMap::new();
        fields.insert("status".to_string(), Some("open".to_string()));
        fields.insert(
            "headline".to_string(),
            Some("1250000 payable on close".to_string()),
        );
        fields.insert(
            "counterparty".to_string(),
            Some("founder@acme.example".to_string()),
        );
        fields.insert("amount".to_string(), Some("sk-not-a-real-key".to_string()));
        ledgers::record(&ctx, &spec, &ledgers::agent_author("ceo"), "row-1", fields)
            .await
            .expect("the record lands");

        // The append reported itself — once. A count here and the value at the
        // end: the needle search below is the guard a leak has to get past, so
        // the stricter equality must not run first and mask it.
        assert_eq!(
            recorder.events().len(),
            1,
            "the append did not report itself"
        );

        let envelope = envelope();
        let rendered = payload(&envelope, &recorder.events()[0])
            .to_string()
            .to_ascii_lowercase();
        for hostile in HOSTILE {
            let needle = hostile.to_ascii_lowercase();
            assert!(
                !rendered.contains(&needle),
                "a ledger_appended payload leaked {needle:?}: {rendered}"
            );
            // The self-check, against a payload that really does carry it: the
            // same search finds the needle in an unredacted rendering, so the
            // assertion above refuses something findable rather than passing
            // because it could never have matched.
            let mut unredacted = payload(&envelope, &recorder.events()[0]);
            unredacted["properties"]["shape"] = serde_json::Value::from(*hostile);
            assert!(
                unredacted
                    .to_string()
                    .to_ascii_lowercase()
                    .contains(&needle),
                "the needle must be findable in an unredacted rendering, or the guard above is \
                 vacuous"
            );
        }

        // And, last, the stricter statement: the shape folded away and a count
        // in place of the row.
        assert_eq!(
            recorder.events(),
            vec![Event::LedgerAppended {
                shape: OTHER,
                records: 1,
            }]
        );
    }

    /// **The guarantee**: neither a ledger slug an author invented nor any cell
    /// value in the row can reach the payload.
    ///
    /// Built the way `crate::analytics::test` builds its guard — a distinctive
    /// needle, searched for case-insensitively, **with a self-check** proving
    /// the same search finds the needle in an unredacted rendering. Without
    /// that second half the assertion could pass because the needle was
    /// unfindable rather than because it was absent, which is the way a
    /// redaction test rots.
    #[test]
    fn no_ledger_slug_or_cell_value_reaches_the_payload() {
        // Standing in for the two kinds of ledger content: the slug an author
        // chose (which is frequently the customer's own brand) and a cell,
        // which is the company's business data outright.
        const HOSTILE: &[&str] = &[
            "acmecorp-holdings-merger",
            "founder@acme.example",
            "$1,250,000 payable on close",
            "sk-not-a-real-key",
        ];

        let envelope = envelope();
        for hostile in HOSTILE {
            // The slug is the only caller-supplied string `track_append`
            // accepts at all — a cell value cannot be offered to it, which is
            // the structural half of the guarantee. So each needle is pushed
            // through the one door that exists.
            let tracker = RecordingTracker::new();
            track_append(&tracker, hostile, 3);
            let event = tracker.events()[0];

            let rendered = payload(&envelope, &event).to_string().to_ascii_lowercase();
            let needle = hostile.to_ascii_lowercase();
            assert!(
                !rendered.contains(&needle),
                "a ledger_appended payload leaked {needle:?}: {rendered}"
            );

            // The self-check, against a payload that really does carry the
            // slug: the same case-insensitive search **finds** it there, so the
            // assertion above is refusing something findable rather than
            // passing because the needle could never have matched. Without
            // this half, a rendering that silently stopped emitting `shape` at
            // all would read as clean.
            let mut unredacted = payload(&envelope, &event);
            unredacted["properties"]["shape"] = serde_json::Value::from(*hostile);
            assert!(
                unredacted
                    .to_string()
                    .to_ascii_lowercase()
                    .contains(&needle),
                "the needle must be findable in an unredacted rendering, or the guard above is \
                 vacuous"
            );

            // And, last, the stricter statement of the same thing: what
            // survived is a word this module compiled in. Ordered after the
            // needle search on purpose — it is the tighter assertion, so on its
            // own it would mask the guard a leak actually has to get past.
            assert_eq!(
                event,
                Event::LedgerAppended {
                    shape: OTHER,
                    records: 3,
                }
            );
        }
    }
}
