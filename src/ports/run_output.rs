//! The [`WorkflowRunOutputStore`] port: one durable, console-facing snapshot of
//! a workflow run's per-node output (issue #596, Stage 1).
//!
//! # The gap this closes
//!
//! A finished workflow run persists only `{node_id, status, elapsed_ms}` per node
//! (the [`WorkflowRunNodeRow`](crate::ports::WorkflowRunNodeRow)s the journal
//! carries). The text each agent node produced, the object a `tool_call`
//! returned, the body an `http_request` fetched — all of it lived only in the
//! in-memory `WorkflowRun.output` and was dropped the moment the run settled. So
//! opening a *past* run from History showed a node's config and nothing it made:
//! the run was a black box that said "done" and could not say *what* it did.
//!
//! # Why a separate store from #418's [`RunOutputCache`]
//!
//! [`RunOutputCache`](crate::harness::orchestrator::RunOutputCache) (issues
//! #418/#594) is an **agent-facing, in-process, evictable** cache: the
//! orchestrator's `read_run_output` tool reads back the items a run summary
//! clipped, in the same process that produced the run. It is deliberately not
//! durable and it *refuses* a run whose node map is oversized, because caching
//! one giant run would blow the whole in-memory budget.
//!
//! This port is the other consumer of the **same capture value**
//! (`outcome.output["nodes"]`): a durable, cross-restart, **console-facing**
//! record so a person can open any past run and see what each node produced. The
//! two never share storage — the cache stays exactly as it was — but they read
//! one capture, so console, scheduled, and agent-tool runs all persist
//! uniformly.
//!
//! ## The deliberate divergence: clip, never refuse
//!
//! Where the cache *refuses* an oversized run (an agent can be told "too big, see
//! the console"), this durable record has nowhere else to point: the console
//! *is* the fallback. A 404 on a run that genuinely produced output would be the
//! very black-box failure this feature exists to end. So
//! [`bound_node_output`] **clips** an oversized value and flags
//! [`WorkflowRunOutputRecord::truncated`] — the record degrades to a clipped
//! value, it never disappears.
//!
//! # One write per run, self-bounding
//!
//! [`put_run_output`](WorkflowRunOutputStore::put_run_output) is called once, at
//! settle, and prunes each company to its newest
//! [`MAX_RUN_OUTPUTS_PER_COMPANY`] runs so the store cannot grow without bound —
//! the same "the cap is a property of the port, not of every writer" stance
//! [`WorkflowRevisionStore`](crate::ports::workflow_revisions::WorkflowRevisionStore)
//! takes.
//!
//! # The journal invariant is untouched
//!
//! The runtime journal deliberately carries **no** node output (it feeds the SSE
//! stream and the inference sidecar — see the no-output scrub in
//! [`crate::ports::types`]). This port is a **sibling** durable surface, written
//! beside the journal and never on it, so that invariant is preserved.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::Result;
use crate::ports::types::CompanyId;

/// How many run-output records a single company keeps before the oldest are
/// pruned on the next write. Bounds a hot, frequently-run tenant; the console
/// only ever reads a recent run, and older runs still show their node timeline
/// from the journal history even after their output snapshot ages out.
pub const MAX_RUN_OUTPUTS_PER_COMPANY: usize = 200;

/// Per-item string ceiling (in **characters**, not bytes) applied by
/// [`bound_node_output`]. A single node's text — an agent draft, an HTTP body —
/// past this is clipped on a char boundary with an ellipsis marker. Generous
/// enough that an ordinary draft survives whole, small enough that one runaway
/// node cannot dominate the record.
pub const NODE_OUTPUT_ITEM_CHAR_CAP: usize = 16 * 1024;

/// Total serialized-bytes ceiling for one run's bounded node map. Reached only
/// by a run with very many items (each already under the per-item cap); past it
/// [`bound_node_output`] clips further and flags `truncated`, rather than
/// refusing — see the module docs on why a durable console record must degrade
/// rather than 404.
pub const RUN_OUTPUT_MAX_BYTES: usize = 512 * 1024;

/// The floor the byte-cap clip stops re-truncating strings at, so the clip loop
/// always terminates even on a map of very many tiny strings.
const BYTE_CLIP_MIN_CHAR_CAP: usize = 256;

/// The ellipsis marker appended to a clipped string. Kept as a constant so the
/// bounding logic and any reader that wants to strip it agree on one token.
pub const CLIP_MARKER: &str = "…";

/// One durable snapshot of a workflow run's per-node output.
///
/// `nodes` is the engine's `run.output["nodes"]` map — `{ "<node id>": {
/// "items": [ … ] } }` — already passed through [`bound_node_output`], so a
/// reader gets a value that is safe to render whole. `truncated` says whether
/// any clipping happened, so the console can badge it honestly. `partial` says
/// the run **did not settle cleanly** (it failed or blocked), so the map holds
/// only what the observer captured from the nodes that finished before the
/// stop — not a complete outcome. Issue #1008: a failed/blocked run used to
/// persist nothing, so its inspector wrongly claimed the run predated capture.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkflowRunOutputRecord {
    /// The run this output belongs to — the store key within a company.
    pub run_id: String,
    /// The workflow that produced it, for display and cross-reference.
    pub workflow_id: String,
    /// Epoch-millis the snapshot was captured (run settle time).
    pub at_millis: u64,
    /// The bounded per-node output map (`{ "<node id>": { "items": [ … ] } }`).
    pub nodes: Value,
    /// Whether [`bound_node_output`] clipped any value to fit the caps.
    pub truncated: bool,
    /// Whether this is a **partial** capture from a run that failed or blocked
    /// rather than a complete settled outcome (issue #1008). `#[serde(default)]`
    /// so a record written before this field existed reads back `false` — a
    /// pre-#1008 snapshot is, by definition, from a run that settled cleanly.
    #[serde(default)]
    pub partial: bool,
}

impl WorkflowRunOutputRecord {
    /// Builds a record from a *raw* node map, bounding it in the process. The
    /// caller hands the engine's `outcome.output["nodes"]` straight in; this is
    /// the one place bounding is applied, so no writer can forget it.
    ///
    /// `partial` flags a capture from a run that failed or blocked rather than
    /// settling cleanly (issue #1008): a clean settle passes `false`, the two
    /// failure arms pass `true` alongside the node map the progress observer
    /// accumulated before the stop.
    pub fn from_raw_nodes(
        run_id: impl Into<String>,
        workflow_id: impl Into<String>,
        at_millis: u64,
        raw_nodes: &Value,
        partial: bool,
    ) -> Self {
        let (nodes, truncated) = bound_node_output(raw_nodes);
        Self {
            run_id: run_id.into(),
            workflow_id: workflow_id.into(),
            at_millis,
            nodes,
            truncated,
            partial,
        }
    }
}

/// The canonical newest-first ordering for a company's run-output records:
/// `at_millis` descending, ties broken by `run_id` descending. Shared by every
/// backend so the prune keeps the same "newest N" set the conformance suite
/// asserts, rather than each backend's incidental order.
pub fn sort_newest_first(records: &mut [WorkflowRunOutputRecord]) {
    records.sort_by(|a, b| {
        b.at_millis
            .cmp(&a.at_millis)
            .then_with(|| b.run_id.cmp(&a.run_id))
    });
}

/// Bounds a raw per-node output map for durable storage, returning the bounded
/// value and whether anything was clipped.
///
/// Two caps, applied in order:
///
/// 1. **Per-item, char-boundary.** Every string anywhere in the map is truncated
///    to at most [`NODE_OUTPUT_ITEM_CHAR_CAP`] **characters** (never bytes — so a
///    multi-byte codepoint is never split), with [`CLIP_MARKER`] appended when it
///    is shortened.
/// 2. **Per-run, serialized bytes.** If the once-truncated map still serializes
///    past [`RUN_OUTPUT_MAX_BYTES`], strings are re-truncated at a shrinking cap
///    until it fits or the cap reaches [`BYTE_CLIP_MIN_CHAR_CAP`]; if it *still*
///    does not fit (a map of very many tiny items), the value is replaced by a
///    small marker object. Either way it **clips and flags**, never refuses.
///
/// A non-string, non-container leaf (number, bool, null) is returned unchanged.
pub fn bound_node_output(nodes: &Value) -> (Value, bool) {
    let mut truncated = false;
    let mut bounded = truncate_strings(nodes, NODE_OUTPUT_ITEM_CHAR_CAP, &mut truncated);

    if serialized_len(&bounded) > RUN_OUTPUT_MAX_BYTES {
        // Shrink the per-string cap until the whole map fits, re-truncating the
        // ORIGINAL each pass so a clip never compounds an already-clipped string.
        let mut cap = NODE_OUTPUT_ITEM_CHAR_CAP;
        while serialized_len(&bounded) > RUN_OUTPUT_MAX_BYTES && cap > BYTE_CLIP_MIN_CHAR_CAP {
            cap /= 2;
            let mut pass_clipped = false;
            bounded = truncate_strings(nodes, cap, &mut pass_clipped);
            truncated = true;
        }
        // Ultimate guarantee: a map with thousands of sub-floor strings can still
        // exceed the byte cap. Replace it with a marker rather than store a value
        // over the ceiling. This is the last resort, not the common path.
        if serialized_len(&bounded) > RUN_OUTPUT_MAX_BYTES {
            bounded = serde_json::json!({
                "__truncated": true,
                "note": "this run's output exceeded the durable size cap and was clipped",
            });
            truncated = true;
        }
    }

    (bounded, truncated)
}

/// The serialized byte length of a value, or [`usize::MAX`] if it cannot be
/// serialized (so an unserializable value is always treated as over any cap).
fn serialized_len(value: &Value) -> usize {
    serde_json::to_vec(value)
        .map(|v| v.len())
        .unwrap_or(usize::MAX)
}

/// Recursively rebuilds `value`, truncating every string to at most `char_cap`
/// characters on a codepoint boundary and setting `clipped` when it shortens one.
fn truncate_strings(value: &Value, char_cap: usize, clipped: &mut bool) -> Value {
    match value {
        Value::String(s) => Value::String(clip_string(s, char_cap, clipped)),
        Value::Array(items) => Value::Array(
            items
                .iter()
                .map(|item| truncate_strings(item, char_cap, clipped))
                .collect(),
        ),
        Value::Object(map) => Value::Object(
            map.iter()
                .map(|(k, v)| (k.clone(), truncate_strings(v, char_cap, clipped)))
                .collect(),
        ),
        // Numbers, bools, null carry no length to clip.
        other => other.clone(),
    }
}

/// Truncates `s` to at most `char_cap` **characters** (codepoints), appending
/// [`CLIP_MARKER`] and setting `clipped` when it is shortened. Char-based by
/// construction, so it can never split a multi-byte codepoint — the byte-slice
/// panic class this whole feature has to avoid.
fn clip_string(s: &str, char_cap: usize, clipped: &mut bool) -> String {
    // Cheap fast-path: bytes ≥ chars, so if the byte length already fits the cap
    // the char length does too, and no allocation/count is needed.
    if s.len() <= char_cap {
        return s.to_string();
    }
    let mut out: String = s.chars().take(char_cap).collect();
    if out.chars().count() < s.chars().count() {
        *clipped = true;
        out.push_str(CLIP_MARKER);
    }
    out
}

/// Durable, per-company, console-facing per-node run output. Company A's run
/// output MUST be invisible to company B, exactly like every other per-company
/// port.
#[async_trait]
pub trait WorkflowRunOutputStore: Send + Sync {
    /// Records (or replaces) one run's output snapshot and prunes the company to
    /// its newest [`MAX_RUN_OUTPUTS_PER_COMPANY`] runs.
    ///
    /// The prune is part of the write on purpose — a company can never accumulate
    /// an unbounded pile of run snapshots, and doing it here keeps the cap a
    /// property of the port rather than of the one caller remembering to trim.
    /// Re-writing the same `run_id` is idempotent: last write wins, and the run
    /// still counts once toward the cap.
    async fn put_run_output(
        &self,
        company: &CompanyId,
        record: &WorkflowRunOutputRecord,
    ) -> Result<()>;

    /// Fetches one run's output snapshot by id, or `None`.
    ///
    /// `None` is the honest answer for every run that predates this feature, was
    /// a dry run (writes nothing durable), or was hard-aborted (no outcome to
    /// persist) — the read route turns it into a 404 and the console renders an
    /// explicit empty state.
    async fn get_run_output(
        &self,
        company: &CompanyId,
        run_id: &str,
    ) -> Result<Option<WorkflowRunOutputRecord>>;
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn a_record_round_trips_as_camel_case_json() {
        let record = WorkflowRunOutputRecord {
            run_id: "run-1".to_string(),
            workflow_id: "greet".to_string(),
            at_millis: 42,
            nodes: serde_json::json!({ "ceo": { "items": ["hi"] } }),
            truncated: false,
            partial: false,
        };
        let json = serde_json::to_string(&record).unwrap();
        assert!(json.contains("\"runId\""), "{json}");
        assert!(json.contains("\"workflowId\""), "{json}");
        assert!(json.contains("\"atMillis\""), "{json}");
        assert_eq!(record, serde_json::from_str(&json).unwrap());
    }

    #[test]
    fn partial_round_trips_and_defaults_false_for_a_pre_feature_payload() {
        // Issue #1008: a `partial` capture round-trips as camelCase.
        let record = WorkflowRunOutputRecord {
            run_id: "run-2".to_string(),
            workflow_id: "greet".to_string(),
            at_millis: 7,
            nodes: serde_json::json!({ "ceo": { "items": ["hi"] } }),
            truncated: false,
            partial: true,
        };
        let json = serde_json::to_string(&record).unwrap();
        assert!(json.contains("\"partial\":true"), "{json}");
        assert_eq!(record, serde_json::from_str(&json).unwrap());

        // A payload persisted before this field existed carries no `partial`
        // key; `#[serde(default)]` reads it back `false` — a pre-#1008 snapshot
        // is from a run that settled cleanly, so "not partial" is the honest
        // default.
        let pre_feature = serde_json::json!({
            "runId": "old-run",
            "workflowId": "greet",
            "atMillis": 1,
            "nodes": { "ceo": { "items": ["hi"] } },
            "truncated": false,
        });
        let decoded: WorkflowRunOutputRecord = serde_json::from_value(pre_feature).unwrap();
        assert!(
            !decoded.partial,
            "a pre-feature payload with no `partial` key must default to false"
        );
    }

    #[test]
    fn a_small_map_is_unchanged_and_not_truncated() {
        let nodes = serde_json::json!({ "ceo": { "items": ["hello world"] } });
        let (bounded, truncated) = bound_node_output(&nodes);
        assert!(!truncated, "a small map must not be flagged");
        assert_eq!(bounded, nodes, "a small map must survive byte-for-byte");
    }

    #[test]
    fn an_oversized_string_is_clipped_on_a_char_boundary_and_flagged() {
        // A string well past the per-item cap, ending in a multi-byte codepoint
        // to prove the clip never splits one (the byte-slice panic class).
        let long = "a".repeat(NODE_OUTPUT_ITEM_CHAR_CAP + 500) + "🚀🚀🚀";
        let nodes = serde_json::json!({ "writer": { "items": [long] } });

        let (bounded, truncated) = bound_node_output(&nodes);
        assert!(truncated, "an oversized item must be flagged truncated");

        let clipped = bounded["writer"]["items"][0].as_str().unwrap();
        // Clipped to the cap (+ the ellipsis marker), never the original length.
        assert!(
            clipped.chars().count() <= NODE_OUTPUT_ITEM_CHAR_CAP + CLIP_MARKER.chars().count(),
            "clipped length {} exceeds the cap",
            clipped.chars().count()
        );
        assert!(clipped.ends_with(CLIP_MARKER), "a clip must be marked");
        // Valid UTF-8 by construction — the assertion is that we got here without
        // a panic mid-codepoint, which char-based truncation guarantees.
    }

    #[test]
    fn a_multibyte_string_under_the_cap_is_untouched() {
        let text = "héllo 🚀 wörld";
        let nodes = serde_json::json!({ "n": { "items": [text] } });
        let (bounded, truncated) = bound_node_output(&nodes);
        assert!(!truncated);
        assert_eq!(bounded["n"]["items"][0], text);
    }

    #[test]
    fn sort_is_newest_first_with_id_tiebreak() {
        let rec = |run_id: &str, at: u64| WorkflowRunOutputRecord {
            run_id: run_id.to_string(),
            workflow_id: "wf".to_string(),
            at_millis: at,
            nodes: Value::Null,
            truncated: false,
            partial: false,
        };
        let mut recs = vec![rec("a", 10), rec("c", 20), rec("b", 20)];
        sort_newest_first(&mut recs);
        let ids: Vec<&str> = recs.iter().map(|r| r.run_id.as_str()).collect();
        assert_eq!(ids, ["c", "b", "a"], "newest first, id-desc tiebreak");
    }

    #[test]
    fn from_raw_nodes_bounds_and_flags() {
        let long = "z".repeat(NODE_OUTPUT_ITEM_CHAR_CAP + 10);
        let raw = serde_json::json!({ "n": { "items": [long] } });
        let record = WorkflowRunOutputRecord::from_raw_nodes("r", "wf", 5, &raw, false);
        assert!(record.truncated, "from_raw_nodes must bound its input");
        assert_eq!(record.run_id, "r");
        assert_eq!(record.workflow_id, "wf");
        assert_eq!(record.at_millis, 5);
        assert!(!record.partial, "a clean settle passes partial=false");

        // Issue #1008: the failure arms hand `partial=true`; it survives onto
        // the record unchanged.
        let flagged = WorkflowRunOutputRecord::from_raw_nodes("r2", "wf", 6, &raw, true);
        assert!(flagged.partial, "from_raw_nodes must carry partial through");
    }
}
