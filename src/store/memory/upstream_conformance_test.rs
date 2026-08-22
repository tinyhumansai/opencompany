//! The upstream driver-conformance suite, run against the providers **this
//! host constructs**.
//!
//! The vendored tinymemory workspace already proves its adapters uphold the
//! contract (`adapters/remote/src/conformance_test.rs`,
//! `core/src/store/factories_provider_test.rs`) — but no OpenCompany lane runs
//! that workspace, and none of it goes through `open_driver`. Namespace-driver
//! defects #1201 and #1238 shipped through exactly that gap: the driver's own
//! suite was green upstream while the driver this host binds misbehaved.
//!
//! So these run `tinymemory_conformance::assert_provider` — the same eleven
//! behavioural assertions — against providers built the way production builds
//! them: a [`MemoryDriverConfig`] through [`open_driver`], nothing constructed
//! by hand. The embedded test binds the real `namespace` store on a tempdir;
//! the hosted tests bind each HTTP adapter against an in-process double
//! speaking that vendor's own wire shapes, ported from the vendored
//! `adapters/remote/src/conformance_test.rs` (keep them in step with it when
//! the adapters' dialects change).
//!
//! Each hosted engine also gets a **facade round-trip**: traces, facts and
//! context driven through [`BoundMemory`]'s ports over real HTTP. The facades
//! encode records into a JSON envelope inside `content` and re-derive
//! everything on decode — the surface #1201 corrupted — and nothing else
//! exercises that path against the hosted dialects.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use axum::extract::{Path, Query, State};
use axum::routing::{delete, get, post, put};
use axum::{Json, Router};
use serde_json::{Value, json};
use tinymemory::registry::DriverClass;
use tinymemory_api::provider::MemoryProvider;

use super::BoundMemory;
use super::driver::{MemoryDriverConfig, MemoryMode, open_driver};
use crate::ports::{CompanyId, CompressedTrace, ContextChunk, FactKind, FactRecord};

/// Opens a driver through the production path and fails the test on refusal.
fn open(config: &MemoryDriverConfig) -> (Arc<dyn MemoryProvider>, DriverClass) {
    open_driver(config)
        .expect("the driver must bind")
        .expect("this config names a driver, so `None` is a routing bug")
}

/// The suite skips every write-path assertion for a non-retaining driver, so a
/// provider (or a broken double behind it) that dropped writes would let
/// `assert_provider` pass having proved almost nothing. Guard first.
async fn assert_retains_then_conforms(provider: Arc<dyn MemoryProvider>) {
    assert!(
        tinymemory_conformance::retains_writes(provider.as_ref()).await,
        "driver `{}` must retain writes, or the conformance run is vacuous",
        provider.driver_id()
    );
    tinymemory_conformance::assert_provider(provider).await;
}

/// Traces, facts and context through the decorator's ports — the JSON-envelope
/// encode/decode path (#1201's shape) — against whatever engine is bound.
async fn facade_round_trip(provider: Arc<dyn MemoryProvider>, class: DriverClass) {
    let bound = BoundMemory::bind(provider, class).expect("bind");
    let company = CompanyId::new("acme");

    let memory = bound.memory();
    memory
        .save_trace(
            &company,
            CompressedTrace {
                cycle_id: "cycle-1".into(),
                // A card-shaped digit run in purchase-y prose, deliberately:
                // #1201 was the embedded scrubber redacting digits INTO BROKEN
                // JSON, so the whole record silently vanished. The contract
                // pinned here is survival, not verbatim digits — an engine
                // with a PII scrubber (the embedded driver, post-#1248) is
                // entitled to redact the card number out of the *content*; it
                // is never entitled to corrupt the envelope around it. #1248's
                // own pin covers the sharper half (Luhn-valid `at_millis`
                // stamps round-trip untouched); this one proves the decode
                // path over every engine this host binds. On the hosted
                // engines, which scrub nothing, the digits also come back —
                // but that is engine policy, so it is not asserted.
                // nosemgrep: coderabbit.pii.credit-card-number-dashed -- test
                // vector, not a credential; see the note above.
                summary: "ordered part 4111 1111 1111 1111 for the lathe".into(),
                at_millis: 1_700_000_000_000,
            },
        )
        .await
        .expect("save_trace");
    let traces = memory.recent_traces(&company, 10).await.expect("traces");
    assert_eq!(traces.len(), 1, "the trace must survive the envelope");
    assert_eq!(
        traces[0].cycle_id, "cycle-1",
        "the record's identity must survive"
    );
    assert!(
        traces[0].summary.contains("ordered part") && traces[0].summary.contains("for the lathe"),
        "the non-sensitive prose must survive whatever the engine's scrubbing policy is; got: {:?}",
        traces[0].summary
    );

    let facts = bound.facts();
    facts
        .upsert(
            &company,
            &FactRecord {
                id: "fact-1".into(),
                kind: FactKind::Fact,
                title: "supplier".into(),
                body: "lathe parts come from Initech".into(),
                source: "cto".into(),
                updated_at_millis: 1_700_000_000_001,
            },
        )
        .await
        .expect("fact upsert");
    let listed = facts.list(&company, None, None).await.expect("fact list");
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].body, "lathe parts come from Initech");

    // Characters a hosted engine sanitises out of `content` (tinymemory#80).
    // The envelope escapes them precisely so this assertion can hold, and the
    // assertion lives here — in the shared facade path — rather than beside
    // the escaping, because the escaping is only worth anything if the record
    // survives the whole way to an engine and back. Against live Supermemory
    // this failed before the escape landed: `U+FFFD` came back missing from
    // the middle of the body, so the record decoded to a string one character
    // shorter than the one that was stored, with nothing raising an error.
    //
    // `U+0000` rides along as the control. JSON already escapes it, so it
    // survived even before the fix — which is what makes it useful here: if a
    // later change to the envelope broke escaping wholesale, the two would
    // fail together, and if only `U+FFFD` fails then the strip is engine-side.
    for (label, character) in [("nul", '\u{0}'), ("replacement", '\u{FFFD}')] {
        let body = format!("before{character}after");
        let id = format!("fact-awkward-{label}");
        facts
            .upsert(
                &company,
                &FactRecord {
                    id: id.clone(),
                    kind: FactKind::Fact,
                    title: format!("awkward {label}"),
                    body: body.clone(),
                    source: "cto".into(),
                    updated_at_millis: 1_700_000_000_002,
                },
            )
            .await
            .unwrap_or_else(|error| panic!("{label} fact upsert: {error}"));
        let listed = facts.list(&company, None, None).await.expect("fact list");
        let stored = listed.iter().find(|fact| fact.id == id);
        assert_eq!(
            stored.map(|fact| fact.body.as_str()),
            Some(body.as_str()),
            "{label}: the record must read back as it was written, not with the \
             character removed from the middle of it"
        );
    }

    let context = bound.context();
    context
        .put(
            &company,
            ContextChunk {
                label: "notes/supplier".into(),
                body: "the quick brown fox".into(),
            },
        )
        .await
        .expect("context put");
    let hits = context
        .search(&company, "quick brown", 10)
        .await
        .expect("context search");
    assert_eq!(
        hits.len(),
        1,
        "context must be recallable through the engine"
    );

    // The port-level `(addr, label)` contract, over whatever engine is bound
    // (issue #1300). Every backend that implements `ContextStore` directly —
    // fs, sqlite, mongodb — proves these in its own test module; the engines
    // reached through `ProviderContextStore` did not, because nothing ran the
    // HOST's port suite against them. That gap matters precisely for the
    // hosted drivers: `delete_label` is a read-merge-write over `get`/`put`,
    // which on supermemory, mem0 and cognee is three HTTP calls against an
    // engine whose `get` and `list` shapes this host does not control. A
    // divergence there is a claim silently lost or a body reaped while
    // another label still points at it — and it would have been invisible
    // until an operator noticed a memory missing.
    //
    // Run against `bound.context()` rather than a fresh binding so the
    // envelope, namespace and decode path under test are the ones the rest of
    // this function already exercised.
    crate::store::conformance::assert_identical_body_two_labels(bound.context()).await;
    crate::store::conformance::assert_delete_label_scoped(bound.context()).await;
}

// ── The embedded `namespace` driver, on a real store ─────────────────────────

#[cfg(feature = "tinymemory-embedded")]
mod embedded {
    use super::*;

    fn namespace_config(dir: &std::path::Path) -> MemoryDriverConfig {
        MemoryDriverConfig {
            mode: MemoryMode::Embedded,
            driver_id: Some("namespace".into()),
            url: None,
            api_key: None,
            data_dir: Some(dir.to_path_buf()),
        }
    }

    /// #1201/#1238 regression net: the durable SQLite store this host binds
    /// under `OPENCOMPANY_MEMORY=embedded` + `OPENCOMPANY_MEMORY_DRIVER=
    /// namespace`, under the upstream suite, through `open_driver`.
    #[tokio::test]
    async fn the_namespace_driver_upholds_the_contract() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (provider, _) = open(&namespace_config(dir.path()));
        assert_retains_then_conforms(provider).await;
    }

    /// The #1201 regression net, armed. On the pre-#1248 driver the
    /// card-shaped digits in `facade_round_trip` tripped the scrubber into
    /// corrupting the stored envelope, and the record was silently gone.
    /// Post-#1248 the scrubber may still redact the digits — that is its job
    /// — but the record, its identity and its prose must survive, which is
    /// exactly what the round-trip asserts.
    #[tokio::test]
    async fn the_namespace_driver_round_trips_the_facades() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (provider, class) = open(&namespace_config(dir.path()));
        facade_round_trip(provider, class).await;
    }
}

// ── Vendor doubles ───────────────────────────────────────────────────────────
//
// Ported from the vendored `adapters/remote/src/conformance_test.rs` at
// tinymemory 38a34d2 — the shapes are each vendor's own, and the doubles
// retain what they are sent, which is what lets the suite's write-path
// assertions actually run. Auth headers are ignored: what the adapter *sends*
// is pinned upstream; these tests are about what this host binds.

/// A record as one of the vendor doubles holds it.
#[derive(Clone, Debug)]
struct Row {
    id: String,
    content: String,
    metadata: Value,
    /// The `containerTag` the adapter sent at create time (Supermemory only;
    /// Mem0 rows carry an empty one).
    tag: String,
}

/// The doubles' shared store: `id -> Row`, plus a counter for fresh ids.
#[derive(Default, Debug)]
struct Backend {
    rows: BTreeMap<String, Row>,
    next: usize,
}

impl Backend {
    fn fresh_id(&mut self) -> String {
        self.next += 1;
        format!("rec-{}", self.next)
    }
}

type Store = Arc<Mutex<Backend>>;

/// Serves `app` on an ephemeral port and returns its base URL.
async fn serve(app: Router) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let endpoint = format!("http://{}", listener.local_addr().expect("address"));
    tokio::spawn(async move {
        axum::serve(listener, app).await.expect("serve");
    });
    endpoint
}

/// The production remote config for one hosted engine, pointed at a double.
fn remote_config(driver: &str, endpoint: &str) -> MemoryDriverConfig {
    MemoryDriverConfig {
        mode: MemoryMode::Remote,
        driver_id: Some(driver.into()),
        url: Some(endpoint.into()),
        api_key: Some("test-key".into()),
        data_dir: None,
    }
}

/// Mem0's native shapes: the five routes `Mem0Dialect` issues, and the two
/// suites over the driver this host binds against them.
mod mem0 {
    use super::*;

    async fn mem0_list(State(store): State<Store>) -> Json<Value> {
        let store = store.lock().expect("store lock");
        let results: Vec<Value> = store
            .rows
            .values()
            .map(|r| {
                json!({
                    "id": r.id,
                    "memory": r.content,
                    "metadata": r.metadata,
                    "created_at": "1970-01-01T00:00:00Z",
                })
            })
            .collect();
        Json(json!({ "results": results }))
    }

    async fn mem0_create(State(store): State<Store>, Json(body): Json<Value>) -> Json<Value> {
        let mut store = store.lock().expect("store lock");
        let id = store.fresh_id();
        let content = body["messages"][0]["content"]
            .as_str()
            .unwrap_or_default()
            .to_owned();
        let metadata = body["metadata"].clone();
        store.rows.insert(
            id.clone(),
            Row {
                id: id.clone(),
                content,
                metadata,
                tag: String::new(),
            },
        );
        Json(json!({ "results": [{ "id": id }] }))
    }

    async fn mem0_update(
        State(store): State<Store>,
        Path(id): Path<String>,
        Json(body): Json<Value>,
    ) -> Json<Value> {
        let mut store = store.lock().expect("store lock");
        if let Some(row) = store.rows.get_mut(&id) {
            if let Some(text) = body["text"].as_str() {
                row.content = text.to_owned();
            }
            if !body["metadata"].is_null() {
                row.metadata = body["metadata"].clone();
            }
        }
        Json(json!({ "id": id }))
    }

    async fn mem0_delete(State(store): State<Store>, Path(id): Path<String>) -> Json<Value> {
        store.lock().expect("store lock").rows.remove(&id);
        Json(json!({ "deleted": true }))
    }

    async fn mem0_search(State(store): State<Store>, Json(body): Json<Value>) -> Json<Value> {
        // Substring matching is enough: the suite asserts that recall *narrows*,
        // not that the backend ranks well.
        let needle = body["query"].as_str().unwrap_or_default().to_lowercase();
        let limit = body["top_k"].as_u64().unwrap_or(100) as usize;
        let store = store.lock().expect("store lock");
        let results: Vec<Value> = store
            .rows
            .values()
            .filter(|r| r.content.to_lowercase().contains(&needle))
            .take(limit)
            .map(|r| {
                json!({
                    "id": r.id,
                    "memory": r.content,
                    "metadata": r.metadata,
                    "score": 0.9,
                })
            })
            .collect();
        Json(json!({ "results": results }))
    }

    async fn mem0_backend() -> String {
        let store: Store = Arc::new(Mutex::new(Backend::default()));
        let app = Router::new()
            .route("/memories", get(mem0_list).post(mem0_create))
            .route("/memories/{id}", put(mem0_update).delete(mem0_delete))
            .route("/search", post(mem0_search))
            .with_state(store);
        serve(app).await
    }

    #[tokio::test]
    async fn the_mem0_driver_this_host_binds_upholds_the_contract() {
        let endpoint = mem0_backend().await;
        let (provider, _) = open(&remote_config("mem0", &endpoint));
        assert_retains_then_conforms(provider).await;
    }

    #[tokio::test]
    async fn the_mem0_driver_round_trips_the_facades() {
        let endpoint = mem0_backend().await;
        let (provider, class) = open(&remote_config("mem0", &endpoint));
        facade_round_trip(provider, class).await;
    }
}

/// Supermemory's native shapes: container tags are its namespace equivalent,
/// so the double keeps a tag per row and the tag listing reflects real writes.
mod supermemory {
    use super::*;

    fn tag_of(row: &Row) -> String {
        row.tag.clone()
    }

    async fn sm_tags(State(store): State<Store>) -> Json<Value> {
        let store = store.lock().expect("store lock");
        let mut tags: Vec<String> = store
            .rows
            .values()
            .map(tag_of)
            .filter(|tag| !tag.is_empty())
            .collect();
        tags.sort();
        tags.dedup();
        Json(Value::Array(
            tags.into_iter()
                .map(|t| json!({ "containerTag": t }))
                .collect(),
        ))
    }

    async fn sm_list(State(store): State<Store>, Json(body): Json<Value>) -> Json<Value> {
        // The adapter pages until a short page comes back. One page, then empty.
        let page = body["page"].as_u64().unwrap_or(1);
        let wanted = body["containerTags"][0].as_str().unwrap_or_default();
        let store = store.lock().expect("store lock");
        let entries: Vec<Value> = if page > 1 {
            Vec::new()
        } else {
            store
                .rows
                .values()
                .filter(|r| tag_of(r) == wanted)
                .map(|r| {
                    json!({
                        "id": r.id,
                        "content": r.content,
                        "metadata": r.metadata,
                        "createdAt": "1970-01-01T00:00:00Z",
                        "isLatest": true,
                        "isForgotten": false,
                    })
                })
                .collect()
        };
        Json(json!({ "memoryEntries": entries }))
    }

    async fn sm_create(
        State(store): State<Store>,
        Json(body): Json<Value>,
    ) -> Result<Json<Value>, axum::http::StatusCode> {
        // The real v4 API requires `containerTag`; filing a malformed create under
        // "" would hide an adapter regression.
        let Some(tag) = body["containerTag"].as_str().filter(|tag| !tag.is_empty()) else {
            return Err(axum::http::StatusCode::BAD_REQUEST);
        };
        let mut store = store.lock().expect("store lock");
        let id = store.fresh_id();
        let first = &body["memories"][0];
        store.rows.insert(
            id.clone(),
            Row {
                id: id.clone(),
                content: first["content"].as_str().unwrap_or_default().to_owned(),
                metadata: first["metadata"].clone(),
                tag: tag.to_owned(),
            },
        );
        Ok(Json(json!({ "memories": [{ "id": id }] })))
    }

    async fn sm_update(State(store): State<Store>, Json(body): Json<Value>) -> Json<Value> {
        let mut store = store.lock().expect("store lock");
        let id = body["id"].as_str().unwrap_or_default().to_owned();
        if let Some(row) = store.rows.get_mut(&id) {
            if let Some(text) = body["newContent"].as_str() {
                row.content = text.to_owned();
            }
            if !body["metadata"].is_null() {
                row.metadata = body["metadata"].clone();
            }
        }
        Json(json!({ "id": id }))
    }

    async fn sm_delete(State(store): State<Store>, Json(body): Json<Value>) -> Json<Value> {
        let id = body["id"].as_str().unwrap_or_default();
        store.lock().expect("store lock").rows.remove(id);
        Json(json!({ "deleted": true }))
    }

    async fn sm_search(State(store): State<Store>, Json(body): Json<Value>) -> Json<Value> {
        let needle = body["q"]
            .as_str()
            .or_else(|| body["query"].as_str())
            .unwrap_or_default()
            .to_lowercase();
        let limit = body["limit"].as_u64().unwrap_or(100) as usize;
        let tag = body["containerTag"].as_str();
        let store = store.lock().expect("store lock");
        let results: Vec<Value> = store
            .rows
            .values()
            .filter(|r| tag.is_none_or(|t| tag_of(r) == t))
            .filter(|r| r.content.to_lowercase().contains(&needle))
            .take(limit)
            .map(|r| {
                json!({
                    "id": r.id,
                    "content": r.content,
                    "metadata": r.metadata,
                    "score": 0.9,
                })
            })
            .collect();
        Json(json!({ "results": results }))
    }

    async fn supermemory_backend() -> String {
        let store: Store = Arc::new(Mutex::new(Backend::default()));
        let app = Router::new()
            .route("/v3/container-tags/list", get(sm_tags))
            .route("/v4/memories/list", post(sm_list))
            .route(
                "/v4/memories",
                post(sm_create).patch(sm_update).delete(sm_delete),
            )
            .route("/v4/search", post(sm_search))
            .with_state(store);
        serve(app).await
    }

    #[tokio::test]
    async fn the_supermemory_driver_this_host_binds_upholds_the_contract() {
        let endpoint = supermemory_backend().await;
        let (provider, _) = open(&remote_config("supermemory", &endpoint));
        assert_retains_then_conforms(provider).await;
    }

    #[tokio::test]
    async fn the_supermemory_driver_round_trips_the_facades() {
        let endpoint = supermemory_backend().await;
        let (provider, class) = open(&remote_config("supermemory", &endpoint));
        facade_round_trip(provider, class).await;
    }
}

/// Cognee's native shapes — the odd one out: no per-record API. The adapter
/// uploads each record as a JSON *file* into a per-namespace dataset and reads
/// it back through `/raw`, so the double stores the uploaded bytes verbatim
/// and serves them unchanged.
mod cognee {
    use super::*;

    type Datasets = Arc<Mutex<BTreeMap<String, BTreeMap<String, String>>>>;

    async fn cg_datasets(State(sets): State<Datasets>) -> Json<Value> {
        let sets = sets.lock().expect("store lock");
        Json(Value::Array(
            sets.keys()
                .map(|name| json!({ "id": name, "name": name }))
                .collect(),
        ))
    }

    async fn cg_data(State(sets): State<Datasets>, Path(dataset): Path<String>) -> Json<Value> {
        let sets = sets.lock().expect("store lock");
        let ids: Vec<Value> = sets
            .get(&dataset)
            .map(|d| d.keys().map(|id| json!({ "id": id, "name": id })).collect())
            .unwrap_or_default();
        Json(Value::Array(ids))
    }

    async fn cg_raw(
        State(sets): State<Datasets>,
        Path((dataset, data_id)): Path<(String, String)>,
    ) -> String {
        sets.lock()
            .expect("store lock")
            .get(&dataset)
            .and_then(|d| d.get(&data_id))
            .cloned()
            .unwrap_or_default()
    }

    async fn cg_delete(
        State(sets): State<Datasets>,
        Path((dataset, data_id)): Path<(String, String)>,
    ) -> Json<Value> {
        if let Some(d) = sets.lock().expect("store lock").get_mut(&dataset) {
            d.remove(&data_id);
        }
        Json(json!({ "deleted": true }))
    }

    /// Pulls the uploaded envelope and the dataset name out of a multipart body.
    async fn multipart_parts(mut form: axum::extract::Multipart) -> (String, String, String) {
        let (mut body, mut dataset, mut filename) = (String::new(), String::new(), String::new());
        while let Ok(Some(field)) = form.next_field().await {
            match field.name().unwrap_or_default().to_owned().as_str() {
                "datasetName" => dataset = field.text().await.unwrap_or_default(),
                "data" | "file" | "files" => {
                    filename = field.file_name().unwrap_or_default().to_owned();
                    body = field.text().await.unwrap_or_default();
                }
                _ => {
                    let _ = field.bytes().await;
                }
            }
        }
        (body, dataset, filename)
    }

    async fn cg_remember(
        State(sets): State<Datasets>,
        form: axum::extract::Multipart,
    ) -> Json<Value> {
        let (body, dataset, filename) = multipart_parts(form).await;
        let mut sets = sets.lock().expect("store lock");
        sets.entry(dataset).or_default().insert(filename, body);
        Json(json!({ "status": "ok" }))
    }

    async fn cg_update(
        State(sets): State<Datasets>,
        Query(q): Query<BTreeMap<String, String>>,
        form: axum::extract::Multipart,
    ) -> Json<Value> {
        let (body, _, _) = multipart_parts(form).await;
        let dataset = q.get("dataset_id").cloned().unwrap_or_default();
        let data_id = q.get("data_id").cloned().unwrap_or_default();
        if let Some(d) = sets.lock().expect("store lock").get_mut(&dataset) {
            d.insert(data_id, body);
        }
        Json(json!({ "status": "ok" }))
    }

    async fn cg_recall(State(sets): State<Datasets>, Json(body): Json<Value>) -> Json<Value> {
        let needle = body["query"].as_str().unwrap_or_default().to_lowercase();
        let limit = body["top_k"].as_u64().unwrap_or(100) as usize;
        let wanted: Option<Vec<String>> = body["datasets"].as_array().map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(str::to_owned))
                .collect()
        });
        let sets = sets.lock().expect("store lock");
        let hits: Vec<Value> = sets
            .iter()
            .filter(|(name, _)| wanted.as_ref().is_none_or(|w| w.contains(name)))
            .flat_map(|(_, d)| d.values())
            .filter(|raw| raw.to_lowercase().contains(&needle))
            .take(limit)
            .map(|raw| json!({ "text": raw, "score": 0.9 }))
            .collect();
        // A TOP-LEVEL array, which is what the adapter's `search` iterates
        // (`response.as_array()`, cognee.rs). This deliberately DIVERGES from the
        // vendored double, which wraps hits in `{"results": …}` — a shape the
        // adapter cannot see, so the upstream suite's recall coverage for Cognee
        // is vacuously green. Caught here because the facade round-trip requires
        // an actual hit; reported upstream rather than mirrored.
        Json(Value::Array(hits))
    }

    async fn cognee_backend() -> String {
        let sets: Datasets = Arc::new(Mutex::new(BTreeMap::new()));
        let app = Router::new()
            // The real API serves the collection at the slashed form and 307s the
            // bare one; the adapter asks for `/api/v1/datasets/` directly.
            .route("/api/v1/datasets/", get(cg_datasets))
            .route("/api/v1/datasets/{dataset}/data", get(cg_data))
            .route("/api/v1/datasets/{dataset}/data/{data_id}/raw", get(cg_raw))
            .route(
                "/api/v1/datasets/{dataset}/data/{data_id}",
                delete(cg_delete),
            )
            .route("/api/v1/remember", post(cg_remember))
            .route("/api/v1/update", axum::routing::patch(cg_update))
            .route("/api/v1/recall", post(cg_recall))
            .with_state(sets);
        serve(app).await
    }

    #[tokio::test]
    async fn the_cognee_driver_this_host_binds_upholds_the_contract() {
        let endpoint = cognee_backend().await;
        let (provider, _) = open(&remote_config("cognee", &endpoint));
        assert_retains_then_conforms(provider).await;
    }

    #[tokio::test]
    async fn the_cognee_driver_round_trips_the_facades() {
        let endpoint = cognee_backend().await;
        let (provider, class) = open(&remote_config("cognee", &endpoint));
        facade_round_trip(provider, class).await;
    }
}

// ── The same suites, against the REAL hosted services ────────────────────────

/// Everything above runs against doubles: HTTP servers this repo writes to the
/// vendors' documented request and response shapes. That is the right default —
/// `cargo test` stays offline, and a shape change in a vendor's docs is caught
/// here rather than in a tenant.
///
/// It is also not the same claim as "this works against supermemory". A double
/// agrees with the documentation; a service agrees with itself. The gap between
/// those is exactly where an integration breaks — pagination that stops early,
/// a delete that is eventually consistent, a namespace filter the server
/// applies more loosely than the docs say — and none of it is visible from a
/// double, because the double was written from the same reading of the docs
/// that the adapter was.
///
/// So the identical suites run against a live endpoint when one is offered:
///
/// ```sh
/// OPENCOMPANY_TEST_SUPERMEMORY_URL=https://api.supermemory.ai \
/// OPENCOMPANY_TEST_SUPERMEMORY_KEY=sk-... \
///   cargo test --features tinymemory --lib live_hosted
/// ```
///
/// Same for `MEM0` and `COGNEE`. Without the pair, each test skips, so this
/// costs an offline run nothing.
///
/// **These tests write to the account they are given.** They store, recall and
/// forget under the `oc-live-test` company's namespace and clean up after
/// themselves, but a shared production account is the wrong thing to point
/// them at — use a scratch project or a free tier.
///
/// # What the first live run found, and why the split below exists
///
/// Run against the real Supermemory API on 2026-08-22, the **provider
/// contract** failed and the **host ports** passed. Both results are correct,
/// and the difference between them is the point.
///
/// Supermemory strips two characters from content, server-side: `U+0000` and
/// `U+FFFD`. That is not inferred from a read-back — `POST /v4/memories`
/// answers `201` and echoes the stored memory back already missing them, in
/// the creation response itself. Everything else offered to it survives,
/// including every other C0 control, so it is not a control-character policy.
/// The conformance suite hands raw content through, so it sees them vanish and
/// fails — correctly, because the engine really does alter what it is given.
/// Filed and fixed upstream as tinymemory#80; the provider-contract test here
/// stays red until the vendored pin picks that fix up.
///
/// Only one of the two was insulated by this host, and the difference is worth
/// keeping because the wrong half of it is the intuitive one. The facades
/// JSON-encode every record into `content` (see `encode`), and RFC 8259
/// requires escaping `U+0000`, so a NUL crosses the wire as the six ASCII
/// characters of its escape and arrives as text there is nothing to strip.
/// `U+FFFD` is not a control character, nothing required it to be escaped, and
/// it therefore crossed as itself and was eaten — every affected record read
/// back one character shorter than it was written, with no error anywhere.
/// `encode` now escapes it deliberately for exactly that reason.
///
/// So the reasoning "the envelope insulates us" was right in form and wrong in
/// fact, and it was wrong in the direction that loses data quietly. It held for
/// the character that had been measured and failed for the one that had only
/// been argued about. A live lane is what separates those two states; keep the
/// facade assertions running real characters through it rather than trusting
/// the shape of the argument.
///
/// The split still generalises: an engine's contract failures reach OpenCompany
/// only where the failing shape is one the facades actually produce. Keep the
/// two tests apart so the next divergence lands on whichever question it
/// belongs to — and answer "does this shape reach us?" by sending it.
#[cfg(test)]
mod live_hosted {
    use super::*;

    /// Whether a missing endpoint must FAIL rather than skip.
    ///
    /// Same shape and same reasoning as the MongoDB suite's
    /// `OPENCOMPANY_TEST_MONGODB_REQUIRED` (issue #555): the skip is right for
    /// a laptop with no vendor keys and wrong for a lane whose entire purpose
    /// is running these. There, an unset pair is a misconfigured job, and the
    /// skip would report it as a pass — the whole suite silently absent behind
    /// a green tick.
    ///
    /// `0` and the empty string read as unset, so it can be threaded through a
    /// workflow matrix that always defines it.
    fn required() -> bool {
        matches!(
            std::env::var("OPENCOMPANY_TEST_HOSTED_REQUIRED").as_deref(),
            Ok(value) if !value.is_empty() && value != "0"
        )
    }

    /// The `(url, key)` pair for one engine, or `None` to skip.
    fn credentials(engine: &str) -> Option<(String, String)> {
        let upper = engine.to_uppercase();
        let url = std::env::var(format!("OPENCOMPANY_TEST_{upper}_URL")).ok();
        let key = std::env::var(format!("OPENCOMPANY_TEST_{upper}_KEY")).ok();
        match (url, key) {
            (Some(url), Some(key)) if !url.is_empty() && !key.is_empty() => Some((url, key)),
            _ => {
                assert!(
                    !required(),
                    "OPENCOMPANY_TEST_HOSTED_REQUIRED is set but \
                     OPENCOMPANY_TEST_{upper}_URL / _KEY are not. This lane exists to run \
                     the port suite against the real {engine} service, so a skip here is a \
                     misconfigured job rather than a pass."
                );
                eprintln!("skipping {engine}: OPENCOMPANY_TEST_{upper}_URL / _KEY are not set");
                None
            }
        }
    }

    /// Binds the live engine named by the environment, or `None` to skip.
    fn live_driver(engine: &str) -> Option<(Arc<dyn MemoryProvider>, DriverClass)> {
        let (url, key) = credentials(engine)?;
        let config = MemoryDriverConfig {
            mode: MemoryMode::Remote,
            driver_id: Some(engine.into()),
            url: Some(url),
            api_key: Some(key),
            data_dir: None,
        };
        Some(
            open_driver(&config)
                .expect("the live driver binds")
                .expect("remote with a driver named yields a provider"),
        )
    }

    /// The **provider contract** against the live service: what the engine
    /// promises about raw content it is handed.
    ///
    /// Kept separate from the port suite below, because the two can disagree
    /// and the difference decides who has to act. A vendor that alters raw
    /// content fails here; whether that reaches this host depends on what this
    /// host actually sends, which is the next test's question.
    async fn live_provider_contract(engine: &str) {
        let Some((provider, _)) = live_driver(engine) else {
            return;
        };
        assert_retains_then_conforms(provider).await;
    }

    /// The **host's ports** against the live service: what OpenCompany itself
    /// depends on, through the facades it really uses.
    ///
    /// This is the claim that decides whether a tenant can run on this engine.
    /// It is deliberately not the same as the provider contract: the facades
    /// JSON-encode every record into `content`, so a byte that an engine
    /// mangles in raw text may never reach it as raw text at all.
    async fn live_host_ports(engine: &str) {
        let Some((provider, class)) = live_driver(engine) else {
            return;
        };
        facade_round_trip(provider, class).await;
    }

    #[tokio::test]
    async fn the_live_supermemory_service_upholds_the_provider_contract() {
        live_provider_contract("supermemory").await;
    }

    #[tokio::test]
    async fn the_live_supermemory_service_upholds_the_host_ports() {
        live_host_ports("supermemory").await;
    }

    #[tokio::test]
    async fn the_live_mem0_service_upholds_the_provider_contract() {
        live_provider_contract("mem0").await;
    }

    #[tokio::test]
    async fn the_live_mem0_service_upholds_the_host_ports() {
        live_host_ports("mem0").await;
    }

    #[tokio::test]
    async fn the_live_cognee_service_upholds_the_provider_contract() {
        live_provider_contract("cognee").await;
    }

    #[tokio::test]
    async fn the_live_cognee_service_upholds_the_host_ports() {
        live_host_ports("cognee").await;
    }
}
