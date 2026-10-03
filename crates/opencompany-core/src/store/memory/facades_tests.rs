use std::sync::{Arc, Mutex};

use tinymemory::{Hit, ItemId, ItemKind, MemoryMeta, SourceKind, SourceRef};

use super::super::namespace::{Namespace, Scope};
use super::bound::{CHARACTERS_ENGINES_STRIP, encode};
use super::*;
use crate::ports::CompressedTrace;

/// Captures warnings emitted synchronously on this test's thread.
///
/// Every exercise of a warn callsite this helper later asserts on must
/// itself run through `warnings_from` — never bare. A `tracing::warn!`
/// fired on a thread with no subscriber registers its callsite against the
/// global NoSubscriber, whose `register_callsite` is `Interest::never()`,
/// and tracing caches that answer process-wide: the warn silently stops
/// firing everywhere, including here. The CI flake this helper was built
/// to make deterministic (`unreadable_content` racing
/// `decode_classifies`) was exactly that. See the load-bearing comments on
/// those two tests.
fn warnings_from(body: impl FnOnce()) -> String {
    use std::io::Write;

    #[derive(Clone)]
    struct Sink(Arc<Mutex<Vec<u8>>>);
    struct Writer(Arc<Mutex<Vec<u8>>>);

    impl Write for Writer {
        fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
            self.0.lock().expect("warning sink").extend_from_slice(data);
            Ok(data.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Sink {
        type Writer = Writer;

        fn make_writer(&'a self) -> Self::Writer {
            Writer(Arc::clone(&self.0))
        }
    }

    let sink = Arc::new(Mutex::new(Vec::new()));
    let subscriber = tracing_subscriber::fmt()
        .with_writer(Sink(Arc::clone(&sink)))
        .with_max_level(tracing::Level::WARN)
        .with_ansi(false)
        .finish();
    let dispatch = tracing::Dispatch::new(subscriber);
    tracing::dispatcher::with_default(&dispatch, || {
        // `tracing` caches each callsite's interest globally. A different
        // test can first register this warning callsite under the no-op
        // subscriber, caching `Interest::never` before this scoped
        // subscriber is installed. Rebuild while this thread's subscriber
        // is active so the warning assertion remains order-independent.
        tracing::callsite::rebuild_interest_cache();
        body();
    });
    let bytes = sink.lock().expect("warning sink").clone();
    String::from_utf8(bytes).expect("warnings are utf-8")
}

/// Derives a real namespace the same way production does. These tests never
/// build one from a raw string, because production code cannot either —
/// a helper that could would be testing a constructor that does not exist.
fn ns(company: &str, scope: Scope) -> Namespace {
    Namespace::company_root(&CompanyId::new(company)).child(&scope)
}

fn entry_in(namespace: &Namespace, content: &str) -> Hit {
    Hit {
        id: ItemId::new("id"),
        kind: ItemKind::Document,
        text: content.to_string(),
        meta: MemoryMeta {
            workspace: Some(namespace.as_str().to_string()),
            folder: Some(namespace.as_str().to_string()),
            source: SourceRef {
                kind: SourceKind::Agent,
                id: Some("key".into()),
            },
            ..MemoryMeta::default()
        },
        score: 0.0,
        confidence: None,
    }
}

fn a_fact() -> FactRecord {
    FactRecord {
        id: "f1".into(),
        kind: FactKind::Preference,
        title: "Ships on Fridays".into(),
        body: "The team releases at the end of the week.".into(),
        source: "cto".into(),
        updated_at_millis: 1_700_000_000_000,
    }
}

#[test]
fn envelope_round_trips_a_fact() {
    let facts = ns("acme", Scope::Facts);
    let fact = a_fact();
    let entry = entry_in(&facts, &encode(&fact).unwrap());
    assert_eq!(decode::<FactRecord>(&entry, &facts).unwrap(), fact);
}

#[test]
fn envelope_round_trips_a_trace() {
    let traces = ns("acme", Scope::Traces);
    let trace = CompressedTrace {
        cycle_id: "c1".into(),
        summary: "shipped the thing".into(),
        at_millis: 42,
    };
    let entry = entry_in(&traces, &encode(&trace).unwrap());
    assert_eq!(decode::<CompressedTrace>(&entry, &traces).unwrap(), trace);
}

#[test]
fn envelope_round_trips_a_chunk_including_its_stamp() {
    let context = ns("acme", Scope::Context);
    let chunk = StoredChunk {
        label: "notes/one".into(),
        body: "the quick brown fox".into(),
        stored_at_millis: 99,
        labels: vec!["notes/one".into()],
    };
    let entry = entry_in(&context, &encode(&chunk).unwrap());
    let decoded: StoredChunk = decode(&entry, &context).unwrap();
    assert_eq!(decoded.label, chunk.label);
    assert_eq!(decoded.body, chunk.body);
    assert_eq!(decoded.stored_at_millis, chunk.stored_at_millis);
    assert_eq!(decoded.labels, chunk.labels);
}

#[test]
fn the_encoding_leaves_no_character_a_hosted_engine_would_strip() {
    // The escape is only worth anything if it removes every literal from
    // the serialized text; an engine sanitises the bytes it receives, not
    // the record they represent.
    let chunk = StoredChunk {
        label: "notes/one".into(),
        body: "before\u{FFFD}after".into(),
        stored_at_millis: 1,
        labels: vec!["notes/one".into()],
    };
    let json = encode(&chunk).unwrap();
    for character in CHARACTERS_ENGINES_STRIP {
        assert!(
            !json.contains(character),
            "the encoded envelope still carries U+{:04X} as a literal: {json:?}",
            character as u32
        );
    }
    assert!(
        json.contains("\\ufffd"),
        "the character must be escaped rather than dropped: {json:?}"
    );
}

#[test]
fn escaping_preserves_the_record_including_around_a_backslash() {
    // The escape rewrites serialized JSON rather than the record, which is
    // safe only because a stripped character can appear solely inside a
    // string literal and `serde_json` has already escaped any backslash
    // beside it. A body that puts the two together is where a naive
    // substitution would corrupt the document, so pin it: this must decode
    // back to exactly what went in.
    let context = ns("acme", Scope::Context);
    let chunk = StoredChunk {
        label: "notes/one".into(),
        body: "a\\\u{FFFD}b\u{FFFD}\\u{FFFD}c\u{0}d".into(),
        stored_at_millis: 7,
        labels: vec!["notes/one".into()],
    };
    let entry = entry_in(&context, &encode(&chunk).unwrap());
    let decoded: StoredChunk = decode(&entry, &context).unwrap();
    assert_eq!(
        decoded.body, chunk.body,
        "every character must survive the escape and the decode"
    );
}

#[test]
fn another_companys_entry_is_dropped_not_decoded() {
    // The cross-tenant guard: an engine answering with somebody else's row
    // must not reach a caller holding this company's id.
    let mine = ns("acme", Scope::Facts);
    let theirs = ns("globex", Scope::Facts);
    let entry = entry_in(&theirs, &encode(&a_fact()).unwrap());
    assert!(decode::<FactRecord>(&entry, &mine).is_none());
}

#[test]
fn a_sibling_scope_of_the_same_company_is_dropped() {
    // Scope separation is not decoration: scratch must not decode as
    // context, or the firewall is a routing convention rather than a rule.
    let context = ns("acme", Scope::Context);
    let scratch = ns("acme", Scope::Scratch);
    let chunk = StoredChunk {
        label: "l".into(),
        body: "b".into(),
        stored_at_millis: 1,
        labels: vec!["l".into()],
    };
    let entry = entry_in(&scratch, &encode(&chunk).unwrap());
    assert!(decode::<StoredChunk>(&entry, &context).is_none());
}

#[test]
fn an_entry_with_no_namespace_is_dropped() {
    let facts = ns("acme", Scope::Facts);
    let mut entry = entry_in(&facts, &encode(&a_fact()).unwrap());
    entry.meta.workspace = None;
    assert!(decode::<FactRecord>(&entry, &facts).is_none());
}

#[test]
fn an_unknown_envelope_version_is_dropped() {
    let traces = ns("acme", Scope::Traces);
    let raw = serde_json::json!({ "v": 99, "record": { "cycle_id": "c1" } }).to_string();
    let entry = entry_in(&traces, &raw);
    assert!(decode::<CompressedTrace>(&entry, &traces).is_none());
}

#[test]
fn unreadable_content_is_dropped_rather_than_failing_the_read() {
    let facts = ns("acme", Scope::Facts);
    let entry = entry_in(&facts, "not json at all");
    // The `warnings_from` wrapper is LOAD-BEARING, not decoration: a corrupt
    // decode must always run under a subscriber, or tracing's global
    // callsite-interest cache can freeze this warn callsite to `never` for
    // the whole process. A bare test thread has no subscriber, so its
    // `get_default` is the global NoSubscriber whose `register_callsite`
    // answers `Interest::never()` — and once cached, every later capture of
    // this same callsite (in `warnings_from`) is silently dropped. The race
    // needs the thread-local sink to have already raised the global max
    // level to WARN, which `decode_classifies`'s own sink does concurrently,
    // so `unreadable_content` — run bare — was exactly the poisoner that
    // made the corruption warning intermittently vanish in CI.
    let warnings = warnings_from(|| {
        assert!(decode::<FactRecord>(&entry, &facts).is_none());
    });
    assert!(
        warnings.contains("memory item in our namespace failed to decode"),
        "unreadable content in our namespace must be reported: {warnings:?}"
    );
}

// The range-widening behavior `peek` relies on is pinned where the helper
// now lives: `crate::store::text` (shared with the fs/sqlite/mongo
// backends' peek and search-snippet slices).

#[test]
fn a_snippet_never_splits_a_character() {
    let body = "é".repeat(300);
    let cut = snippet(&body);
    assert!(body.starts_with(&cut));
    assert!(cut.len() >= 200);
}

/// The decode classification #1248 review asked to pin: an entry from an
/// envelope version this build does not know is a legitimate skip even
/// when its record shape does not fit today's `T` — it must not be read
/// as corruption — while a matching version with an unreadable record,
/// or no envelope at all, is the #1201 corruption path. All three return
/// `None` rather than failing the list.
#[test]
fn decode_classifies_unknown_versions_and_corruption_separately() {
    let namespace = Namespace::company_root(&CompanyId::new("acme"));
    let entry = |content: &str| entry_in(&namespace, content);

    // Round-trip control: a current-version envelope decodes.
    let good = encode(&42u32).unwrap();
    assert_eq!(decode::<u32>(&entry(&good), &namespace), Some(42));

    // Unknown version, record shape incompatible with `T`: skipped, and
    // reachable only through the version gate — the record is never
    // parsed as `T`, so this cannot trip the corruption path.
    let future = r#"{"v":9,"record":{"shape":"unknown"}}"#;
    assert_eq!(decode::<u32>(&entry(future), &namespace), None);

    // Matching version, unreadable record: the corruption path. The valid
    // envelope ensures this reaches record deserialization after the
    // version check instead of duplicating the malformed-JSON case below.
    #[derive(Deserialize)]
    struct Timestamp {
        #[allow(dead_code)]
        at_millis: u64,
    }
    let mangled = r#"{"v":1,"record":{"at_millis":"not-a-number"}}"#;
    let warnings = warnings_from(|| {
        assert!(decode::<Timestamp>(&entry(mangled), &namespace).is_none());
    });
    assert!(
        warnings.contains("memory item in our namespace failed to decode"),
        "matching-version record corruption must be reported: {warnings:?}"
    );

    // No envelope at all: also the corruption path.
    let warnings = warnings_from(|| {
        assert_eq!(decode::<u32>(&entry("not json"), &namespace), None);
    });
    assert!(
        warnings.contains("memory item in our namespace failed to decode"),
        "envelope-less content in our namespace must be reported: {warnings:?}"
    );
}
