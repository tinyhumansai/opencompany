//! Issue #846: a continuation must not make an outward call its own lineage
//! already made.
//!
//! # What this is a second half of
//!
//! A paused workflow run is **settled**, not suspended — the engine finishes and
//! reports the gates it stopped at — so approving is a re-run from the trigger
//! with the gate listed in `approvals`. That primitive is deliberate, it is
//! restart-durable for free, and #438 accepted it: the requirement it wrote down
//! was not "stop re-executing" but **"an approval must never cause a message to
//! be sent to a person twice."**
//!
//! #496 met that requirement for the half the host performs with its own hands:
//! an `output` node's report, routed by [`deliver_outputs`](super::delivery)
//! after the engine settles. A ledger rides the approval card, and a reached
//! output node listed in it is skipped rather than dispatched.
//!
//! It does not reach the other half. A `send`, `publish` or `repo_publish`
//! wired as a **`tool_call` node** is performed by the engine, mid-run, through
//! a capability — so there is no post-hoc dispatch for the host to decline. Put
//! such a node upstream of a gate and today the first run sends, the operator
//! approves, and the continuation sends again. Same exposure, same lineage, the
//! other mechanism.
//!
//! # The seam, and why it is this one
//!
//! [`ToolInvoker::invoke`](tinyflows::caps::ToolInvoker) receives
//! `(slug, args, conn)` — no node id, no run state — which
//! [`gate`](super::gate) already records as the reason the *approval* gate could
//! not live there. The same fact rules out a node-keyed skip inside the invoker,
//! and node identity is not negotiable here: keying on `(tool, args)` instead
//! would miss every send whose body an upstream agent node re-generates, which
//! is most of them.
//!
//! What the host does own is the **translation** — it builds the graph per run,
//! which is where [`apply_policy_gates`](super::gate::apply_policy_gates)
//! already writes per-node decisions. So a node the lineage has already called
//! is rewritten *before the run starts* to invoke a host-private sentinel slug
//! carrying its recorded result, and the invoker answers that slug from its
//! arguments without touching the toolbelt. The engine is unchanged, the graph
//! shape is unchanged, and the node still produces output — the same output,
//! because the recorded value is the verbatim capability return and
//! `envelope::wrap` is a pure function of it.
//!
//! # Which nodes, and the line drawn
//!
//! Two kinds, on two different rules, because the two have different amounts of
//! declaration behind them.
//!
//! **`http_request` with a mutating method** — anything but `GET`/`HEAD`. This
//! is the live one. An `http_request` node reaches an arbitrary address today,
//! on every company, and a `POST` upstream of a gate is a second POST on every
//! approval. Idempotency by HTTP method is the standard the method names carry,
//! and it is the only thing the host can read about a call whose destination is
//! authored per node.
//!
//! **`tool_call` whose [`consequence_of`](crate::policy::consequence_of) group
//! is an outward one** — anything but
//! [`EffectGroup::Other`](crate::ports::types::EffectGroup::Other), less
//! [`Reach::Money`](crate::policy::Reach::Money) (below). That is a reader of the
//! one declaration both policy tiers read, not a second table to keep in step
//! with it.
//!
//! ## What that second rule does and does not reach today, stated plainly
//!
//! **Nothing, today.** A workflow `tool_call` can only invoke the four families
//! [`WORKFLOW_TOOL_NAMESPACES`](super::caps) wires — shell, code, web, search —
//! and every slug in them is declared `EffectGroup::Other` except `web_search`,
//! which this rule excludes. So there is no wired slug this arm currently
//! guards, and #846's own example — "put a `send`, `publish` or `repo_publish`
//! node after two gated steps" — describes agent-turn tool families the workflow
//! invoker does not wire at all.
//!
//! That is worth stating rather than quietly implying otherwise, and it is a
//! reason to write the rule now rather than later: it is the guard that has to
//! exist *before* a send-capable namespace is wired into workflows, or wiring
//! one silently re-opens #438 on the day it lands. It costs one arm of one
//! `match`, and it is exercised by this module's tests against the declared
//! table rather than against a wired tool — which is the most that can honestly
//! be claimed for a rule whose subject does not exist yet.
//!
//! ## Two gaps this does NOT close
//!
//! * **`shell`, `curl` and `git_operations`.** All three are `EffectGroup::Other`
//!   and all three can reach a counterparty — `curl` to an address, `shell` to
//!   anything at all, `git_operations` to a remote. The host cannot tell a
//!   `git log` from a `git push`, and replaying every shell node's recorded
//!   stdout on every continuation would change the behaviour of the most common
//!   effectful node there is on the strength of a guess. Left as it is,
//!   deliberately, and filed as issue #850 rather than folded in.
//! * **`web_fetch` and `web_search`.** Read-only, so the three re-executed
//!   fetches in #846's own reproduction still re-execute — which is the issue's
//!   own reading of them: idempotent reads whose cost is latency and tokens.
//!   `web_search` is excluded by the [`Reach::Money`] carve-out rather than by
//!   its group, because it is declared `EffectGroup::Spend` for billing and is
//!   a read: it reaches nobody and changes nothing, and #438 priced repeated
//!   spend as cost rather than as the harm being guarded here. Replaying it
//!   would also hand a later run a stale answer it never asked for, which is the
//!   argument against replaying a fetch, and the two should not differ.
//!
//! # Two limits, stated rather than hidden
//!
//! * **A truncated result is never replayed.** The recorded value rides the
//!   durable approval card, so it is bounded like everything else that does
//!   ([`bound_node_output`](crate::ports::bound_node_output)). If bounding would
//!   clip it, the node is not recorded and the continuation calls again — a
//!   duplicate send is bad, and feeding downstream a silently-clipped receipt as
//!   if it were real is worse. The operator is told, via a run notice.
//! * **A per-item fan-out is not replayed.** A `split_out` → `tool_call` node
//!   invokes once per item, and the invoker sees no item index, so one recorded
//!   result cannot answer N invocations without inventing which. Recording only
//!   the single-invocation shape keeps the guard exact where it applies instead
//!   of approximate everywhere; the fan-out case behaves as it does today and
//!   says so. #496 took the same side of the same trade for a partial delivery
//!   fan-out.
//!
//! Both limits are *visible*: [`replay_performed`] returns what it could not
//! guard so the runner can surface it as a notice (issue #638's mechanism), so
//! "this continuation called out again" is something an operator reads rather
//! than something they reconstruct.
//!
//! The complete answer to all of this is checkpointed resume, and it is still
//! not available: tinyflows' `run_resumable` installs a no-op observer and a
//! process-local in-memory checkpointer and takes no cancellation token, so it
//! composes with neither the per-node progress trail (#371) nor the stop signal
//! (#398) this runner is built on, and an approval that arrives after a restart
//! would find no checkpoint. That is engine work in a vendored crate, exactly as
//! #438 recorded. This guard is forward-compatible with it: it is keyed on the
//! node and it withdraws to nothing when the ledger is empty.

use serde_json::{Value, json};
use tinyflows::model::{NodeKind, WorkflowGraph};

use crate::company::{WorkflowFile, WorkflowNodeKind};
use crate::ports::bound_node_output;

use crate::runtime::workflow_resume::{PerformedCall, performed_in_input};

use crate::workflows::caps::resolver::{
    ChildGateRecord, ChildGateRegistry, GATE_NAMESPACE, child_id_of,
};

/// The host-private slug a replayed node invokes instead of its real tool.
///
/// Namespaced with a `__opencompany` prefix that no toolbelt tool carries and no
/// authoring surface accepts. Even so the invoker's arm is written to be safe
/// against an author who types it anyway: it reaches no capability, executes
/// nothing and returns only what its own arguments carry, so the worst an
/// authored occurrence can do is produce an inert node — strictly less than any
/// grant would already allow.
pub(crate) const REPLAY_SLUG: &str = "__opencompany.already_performed";

/// The argument key carrying the recorded result, **JSON-encoded as a string**.
///
/// Encoded rather than embedded, and this is load-bearing. Node config is walked
/// by [`tinyflows::expr::resolve`] before the node runs, and every leaf string
/// beginning with `=` is evaluated as an expression against the run scope. A
/// recorded result is arbitrary provider data that may contain such a string, so
/// embedding it verbatim would hand a counterparty's response to the expression
/// engine. `serde_json::to_string` of any value yields a document starting with
/// `{`, `[`, `"`, a digit, `t`, `f` or `n` — never `=` — so a single encoded
/// string is inert by construction rather than by escaping rules.
pub(crate) const REPLAY_RESULT_KEY: &str = "result_json";

/// The recorded result behind a [`REPLAY_SLUG`] invocation, or `None` for every
/// other slug (issue #846).
///
/// The whole of what a tool invoker has to know about this module: one
/// comparison and one decode, so both invokers can answer the sentinel
/// identically without either growing a copy of the rules.
///
/// A sentinel invocation whose argument is missing or is not the JSON the host
/// wrote yields [`Value::Null`] rather than `None`. Falling through to the real
/// toolbelt would be the one outcome this module exists to prevent — the node
/// was rewritten precisely *because* calling out again is the bug — and no live
/// tool answers to this slug anyway, so the fall-through would fail the node
/// rather than send. A null return is a node that produced nothing, which is
/// visible in the run's own output.
pub(crate) fn replayed_result(slug: &str, args: &Value) -> Option<Value> {
    if slug != REPLAY_SLUG {
        return None;
    }
    let decoded = args
        .get(REPLAY_RESULT_KEY)
        .and_then(Value::as_str)
        .and_then(|encoded| serde_json::from_str(encoded).ok())
        .unwrap_or(Value::Null);
    tracing::info!(
        recovered = !decoded.is_null(),
        "workflow: replaying a call this run's lineage already made; not calling out again \
         (issue #846)"
    );
    Some(decoded)
}

/// A node the continuation could not replay, and why — surfaced to the operator
/// as a run notice rather than left in the host's logs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct UnreplayableCall {
    /// The node that will call out a second time.
    pub node_id: String,
    /// The tool it will call.
    pub slug: String,
    /// Operator-facing prose for why the guard did not apply.
    pub why: &'static str,
}

impl UnreplayableCall {
    /// The sentence an operator reads, on the run that parked — before they
    /// approve, which is the only moment they can still act on it.
    pub fn notice(&self) -> String {
        format!(
            "“{}” ({}) reached outside the company on this run, and approving the gate below it \
             will call it again: {}.",
            self.node_id, self.slug, self.why
        )
    }
}

/// Every `tool_call` node in `graph` whose call left the building on this run,
/// paired with the verbatim result it returned (issue #846).
///
/// Read off the settled run's own `output["nodes"]`, which already holds every
/// completed node's items — so this costs no engine change and no second source
/// of truth. A node that did not complete has no entry and is not recorded; a
/// node the run never reached (everything past the gate) has none either, which
/// is what makes "recorded" mean "actually happened".
///
/// Returns `(performed, unreplayable)`: what a continuation may replay, and what
/// it may not, so the caller can carry the second half to the operator instead
/// of silently guarding less than it appears to.
pub(crate) fn outward_calls_performed(
    graph: &WorkflowGraph,
    output: &Value,
    authored: &WorkflowFile,
) -> (Vec<PerformedCall>, Vec<UnreplayableCall>) {
    let nodes = output.get("nodes");
    let mut performed = Vec::new();
    let mut unreplayable = Vec::new();
    let declared = declared_unrepeatable(authored);
    let declared_names = declared_call_names(authored);

    for node in &graph.nodes {
        let is_declared = declared.contains(node.id.as_str());
        // `replay_performed` overwrites a replayed node's own config with the
        // `REPLAY_SLUG` sentinel *before* this run's engine ever sees it, so by
        // the time the settled output reaches here the node's own `slug` no
        // longer names the call it made — it names the sentinel (issue #850 +
        // #846 interaction). Recording that sentinel verbatim would put
        // `__opencompany.already_performed` on the operator's approval card in
        // place of the real tool name. A declared node has to keep tracking
        // under its own name instead — dropping it (the naive fix) would stop
        // guarding it after this hop, so a third run downstream of a second
        // gate would find an empty ledger entry and call it for real, which is
        // exactly what the declaration exists to prevent. An undeclared node's
        // replay is dropped here exactly as it already, if accidentally, was:
        // it falls to `outward_call_of`'s classifier below, which cannot
        // classify the sentinel either — this just makes that explicit instead
        // of leaning on the policy table never having an opinion about it.
        let is_replay_sentinel = matches!(node.kind, NodeKind::ToolCall)
            && node.config.get("slug").and_then(Value::as_str) == Some(REPLAY_SLUG);
        let Some(slug) = (if is_replay_sentinel {
            is_declared
                .then(|| declared_names.get(node.id.as_str()).cloned())
                .flatten()
        } else {
            outward_call_of(node, is_declared)
        }) else {
            continue;
        };
        // Not reached, or reached and produced nothing: there is nothing to
        // guard and nothing to replay.
        let Some(items) = nodes
            .and_then(|map| map.get(&node.id))
            .and_then(|node_output| node_output.get("items"))
            .and_then(Value::as_array)
            .filter(|items| !items.is_empty())
        else {
            continue;
        };

        if items.len() > 1 {
            unreplayable.push(UnreplayableCall {
                node_id: node.id.clone(),
                slug,
                why: "it runs once per input item, and a single recorded result cannot stand in \
                      for several",
            });
            continue;
        }
        // The engine wraps a capability return as `{ json, text, raw }`, so
        // `raw` is the verbatim value — replaying it reconstructs this exact
        // envelope rather than approximating it. An item without `raw` is not a
        // capability envelope and is not something this can faithfully replay.
        let Some(raw) = items[0].get("raw") else {
            unreplayable.push(UnreplayableCall {
                node_id: node.id.clone(),
                slug,
                why: "its output is not in the engine's capability envelope, so there is no \
                      verbatim result to replay",
            });
            continue;
        };

        let (bounded, truncated) = bound_node_output(raw);
        if truncated {
            unreplayable.push(UnreplayableCall {
                node_id: node.id.clone(),
                slug,
                why: "its result is too large to carry on the approval card, and a clipped \
                      result must not be replayed as if it were whole",
            });
            continue;
        }
        performed.push(PerformedCall {
            node: node.id.clone(),
            tool: slug,
            result: bounded,
        });
    }

    (performed, unreplayable)
}

/// The ungated outward calls a paused child will repeat when its gate is
/// approved (issue #617).
///
/// A `sub_workflow` child that stops at an approval gate pauses the *parent*:
/// tinyflows reports the child's gates namespaced (`<node>::<gate>`, nested
/// one level per child — `sub::nested::work` for a gate two levels down), and
/// the continuation re-runs the parent, which re-runs every child from the
/// top. Any outward call the child made before the gate therefore fires again —
/// but unlike a top-level call, its result does not travel up when the child
/// pauses (tinyflows drops the child's partial output on
/// `ChildOutcome::Paused`), so there is nothing to replay. Report it the same
/// way [`outward_calls_performed`] reports a top-level call it cannot replay,
/// so the operator is warned that approving restarts the child from the top.
///
/// Each namespaced pending id is resolved through `registry` to the child graph
/// the resolver actually gated — descending through nested namespaces, and
/// resolving an expression-bound `workflow_id` against `trigger_input` where
/// the engine's `once` scope allows — then every call **upstream-reachable**
/// from the paused gate is examined. "Upstream-reachable" is read off the
/// child's edges by walking backward from the gate; a call on an un-taken
/// branch of a fan-out is over-reported, the same conservative direction the
/// top-level [`outward_calls_performed`] takes when it reads "completed" from
/// the run state rather than per-branch.
///
/// A `requires_approval` node this run's list has already approved is *not*
/// treated as still-blocked: the engine executes an approved gate (it skips the
/// interrupt only when the id is listed), so the call fires on this
/// continuation and will fire again on the next — exactly what the operator
/// must be warned about.
pub(crate) fn child_calls_to_repeat(
    parent: &WorkflowGraph,
    pending: &[String],
    registry: &ChildGateRegistry,
    trigger_input: &Value,
) -> Vec<UnreplayableCall> {
    let approved = approved_ids(trigger_input);
    let mut out = Vec::new();
    for node_id in pending {
        let mut segments: Vec<&str> = node_id.split(GATE_NAMESPACE).collect();
        let Some(gate) = segments.pop() else {
            continue;
        };
        let mut graph = parent.clone();
        let mut record: Option<ChildGateRecord> = None;
        let mut prefix = String::new();

        // A nested child is restarted from its own root, so calls in every
        // ancestor child before the next `sub_workflow` node are repeated too.
        // Walk each intermediate graph before descending to the record below it.
        for segment in segments {
            let Some(child_id) = child_id_of(&graph, segment, Some(trigger_input)) else {
                record = None;
                break;
            };
            if let Some(ancestor) = record.as_ref() {
                for call in
                    child_calls_preceding(&ancestor.graph, segment, &approved, &prefix, &prefix)
                {
                    if !out.contains(&call) {
                        out.push(call);
                    }
                }
            }
            let Some(next) = registry.get(&child_id) else {
                record = None;
                break;
            };
            prefix.push_str(segment);
            prefix.push_str(GATE_NAMESPACE);
            graph = next.graph.clone();
            record = Some(next);
        }

        let Some(record) = record else {
            continue;
        };
        // Keep the deepest-child node ids in their established local form;
        // ancestor calls carry the namespace so equal local ids remain distinct.
        for call in child_calls_preceding(&record.graph, gate, &approved, &prefix, "") {
            if !out.contains(&call) {
                out.push(call);
            }
        }
    }
    out
}

/// The ids this lineage has already approved, read off the continuation input
/// the same way the engine's `approvals_for_child` reads them
/// (`run.trigger.approvals`).
fn approved_ids(trigger_input: &Value) -> std::collections::HashSet<String> {
    trigger_input
        .get("approvals")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(str::to_string)
        .collect()
}

/// The outward calls in `child` that run before `gate` and are not themselves
/// gated.
///
/// Every node from which `gate` is reachable by walking `edges` backward must
/// have run (or be on the branch that ran) before the pause; everything past
/// the gate is unreachable and excluded. A `requires_approval` node the gate
/// pass marked is excluded too — the child restarts and *pauses at it again*
/// rather than executing it — **unless its namespaced id (`namespace_prefix` +
/// the node's own id) is already in `approved`**: an approved gate does
/// execute, on this continuation, and will execute again on the next, so it is
/// exactly the call the operator must be warned about (issue #617).
///
/// Classified with the same [`outward_call_of`] the top-level guard uses, so
/// the two reports agree about what an "outward call" is. The child's authored
/// `repeatable` declarations are not consulted (the resolver keeps the
/// translated graph, not the authored file); an undeclared classification is
/// the conservative direction for a warning.
fn child_calls_preceding(
    child: &WorkflowGraph,
    gate: &str,
    approved: &std::collections::HashSet<String>,
    namespace_prefix: &str,
    output_prefix: &str,
) -> Vec<UnreplayableCall> {
    let mut reached = std::collections::HashSet::new();
    let mut queue = std::collections::VecDeque::from([gate.to_string()]);
    while let Some(id) = queue.pop_front() {
        if !reached.insert(id.clone()) {
            continue;
        }
        for edge in &child.edges {
            if edge.to_node == id {
                queue.push_back(edge.from_node.clone());
            }
        }
    }
    let mut out = Vec::new();
    for node in &child.nodes {
        if !reached.contains(&node.id) {
            continue;
        }
        let is_gate = node
            .config
            .get("requires_approval")
            .and_then(Value::as_bool)
            == Some(true);
        if is_gate && !approved.contains(&format!("{namespace_prefix}{}", node.id)) {
            // The child restarts and pauses at this node again; it does not
            // execute it.
            continue;
        }
        let Some(slug) = outward_call_of(node, false) else {
            continue;
        };
        out.push(UnreplayableCall {
            node_id: format!("{output_prefix}{}", node.id),
            slug,
            why: "it runs inside a child workflow whose ungated calls are not carried up when \
                  the child pauses, so approving restarts the child and calls it again",
        });
    }
    out
}

/// Rewrites every node this lineage has already called so the continuation
/// replays its recorded result instead of calling again (issue #846).
///
/// Mutates `graph` in place, beside [`apply_policy_gates`](super::gate::apply_policy_gates)
/// and before compilation, which is the one moment the host holds both the node
/// ids and the trigger input. A first run — anything with no ledger on its input
/// — leaves the graph byte-identical, so every existing run and every existing
/// test is untouched.
///
/// Returns the node ids it rewrote, for the log line and for the tests that pin
/// this: an empty return is the honest answer for a graph whose ledger names
/// nodes it does not contain (a graph edited between the pause and the
/// approval), where re-calling is the only thing left to do.
pub(crate) fn replay_performed(graph: &mut WorkflowGraph, trigger_input: &Value) -> Vec<String> {
    let ledger = performed_in_input(trigger_input);
    if ledger.is_empty() {
        return Vec::new();
    }

    let mut replayed = Vec::new();
    for node in &mut graph.nodes {
        if !matches!(node.kind, NodeKind::ToolCall | NodeKind::HttpRequest) {
            continue;
        }
        let Some(call) = ledger.iter().find(|call| call.node == node.id) else {
            continue;
        };
        let Value::Object(config) = &mut node.config else {
            continue;
        };
        let Ok(encoded) = serde_json::to_string(&call.result) else {
            // Unserializable is unreachable for a value that arrived by
            // deserialization, and re-calling is the safe direction if it ever
            // is not: a node that runs twice is the bug being fixed, a node that
            // silently returns nothing is a new one.
            continue;
        };

        // An `http_request` node becomes a `tool_call` invoking the sentinel.
        //
        // The kind change is what lets one seam serve both, and it is safe
        // because the two node kinds already agree about the only thing that
        // leaves the node: every capability node wraps its result in the same
        // `{ json, text, raw }` envelope, so a downstream `=item.json.<field>`
        // binding reads the same value either way. The alternative was a second
        // replay mechanism inside `GuardedHttpClient` — which, like the invoker,
        // sees no node id and would have needed its own identity scheme.
        node.kind = NodeKind::ToolCall;
        // The request descriptor, removed rather than left to rot. Leaving it
        // would have the engine resolve a URL and headers for a call that is
        // not made, and would leave a reader of the compiled graph unable to
        // tell a replayed node from a live one.
        for key in ["url", "method", "headers", "body"] {
            config.remove(key);
        }

        config.insert("slug".to_string(), json!(REPLAY_SLUG));
        config.insert("args".to_string(), json!({ REPLAY_RESULT_KEY: encoded }));
        // Nothing to connect to, and leaving a stale ref would have the engine
        // resolve a connection for a call that is not made.
        config.remove("connection_ref");
        // One invocation, one recorded result. A node left in `per_item` mode
        // would replay the same result once per input item; `outward_calls_performed`
        // refuses to record a fan-out for exactly that reason, and pinning the
        // mode here means a graph edited between the pause and the approval
        // cannot re-open the hole.
        config.insert("execution".to_string(), json!("once"));
        replayed.push(node.id.clone());
    }

    replayed
}

/// The node ids whose author declared `repeatable = false` (issue #850).
///
/// Read off the **authored** file rather than the compiled graph, the same way
/// [`deliver_outputs`](super::delivery::deliver_outputs) reads `destination`:
/// this is host-side policy the engine never sees, so putting it in engine
/// config would be an inert key riding into the graph.
///
/// Restricted to the two kinds that make a call, mirroring the validation that
/// rejects the field anywhere else — so a graph loaded from an older or looser
/// source cannot widen the guarded set through a kind the rewrite would not
/// touch anyway.
fn declared_unrepeatable(authored: &WorkflowFile) -> std::collections::HashSet<&str> {
    authored
        .nodes
        .iter()
        .filter(|node| {
            node.repeatable == Some(false)
                && matches!(
                    node.kind,
                    WorkflowNodeKind::ToolCall | WorkflowNodeKind::HttpRequest
                )
        })
        .map(|node| node.id.as_str())
        .collect()
}

/// The outward-call identity a declared-unrepeatable node's own authored
/// config names, keyed by node id.
///
/// Consulted only when [`outward_calls_performed`] finds a node whose compiled
/// config has already been overwritten by [`replay_performed`] with the
/// [`REPLAY_SLUG`] sentinel: at that point the graph itself has nothing left
/// to classify, so the name has to be read back off `authored` — the same
/// source [`declared_unrepeatable`] reads, for the same reason.
fn declared_call_names(authored: &WorkflowFile) -> std::collections::HashMap<&str, String> {
    authored
        .nodes
        .iter()
        .filter(|node| node.repeatable == Some(false))
        .filter_map(|node| {
            let name = match node.kind {
                WorkflowNodeKind::ToolCall => node
                    .config
                    .as_ref()
                    .and_then(|c| c.get("slug"))
                    .and_then(Value::as_str)?
                    .to_string(),
                WorkflowNodeKind::HttpRequest => {
                    let method = node
                        .config
                        .as_ref()
                        .and_then(|c| c.get("method"))
                        .and_then(Value::as_str)
                        .unwrap_or("GET")
                        .to_uppercase();
                    format!("http_request {method}")
                }
                _ => return None,
            };
            Some((node.id.as_str(), name))
        })
        .collect()
}

/// The name of the outward call a node makes — or `None` when the node makes no
/// call, or makes one that reaches nobody outside the company.
///
/// The name is for the operator and the log line; the *identity* a ledger entry
/// is matched on is always the node id.
///
/// An `agent` node is deliberately absent, and it is a different question rather
/// than an oversight: its own tool calls park through #395's drain and are
/// decided one at a time, and its re-execution is the token cost #438 already
/// priced and declined to fix here.
fn outward_call_of(node: &tinyflows::model::Node, declared_unrepeatable: bool) -> Option<String> {
    match node.kind {
        NodeKind::ToolCall => {
            let slug = node.config.get("slug").and_then(Value::as_str)?;
            let args = node
                .config
                .get("args")
                .cloned()
                .unwrap_or_else(|| json!({}));
            // The author said this call must not be made twice (issue #850).
            // Read BEFORE the classifier, because the whole point is the calls
            // the classifier cannot see: `shell` runs an arbitrary command and
            // the host does not parse it, so no amount of inspection here will
            // ever reach the right answer. Only the guarding direction is taken
            // from the author — `repeatable = true` falls through to the
            // classifier below rather than overriding it, so a declaration can
            // never switch off a guard #846 already applies.
            if declared_unrepeatable {
                return Some(slug.to_string());
            }
            let consequence = crate::policy::consequence_of(slug, &args);
            // The residual bucket — "no particular consequence to name on the
            // card" — is everything that stays inside the company plus the three
            // this module's docs name as an open gap.
            if consequence.group.is_unclassified() {
                return None;
            }
            // Billed, but it reaches nobody and changes nothing. See the module
            // docs: repeated spend is cost, not the harm being guarded.
            if consequence.reach.costs_money() {
                return None;
            }
            Some(slug.to_string())
        }
        // Reaches an arbitrary address on every company today, so this is the
        // arm that guards a *live* duplicate. Read by method, which is the only
        // thing the host knows about a destination the author supplies per node
        // — and which is exactly what the method names are for.
        NodeKind::HttpRequest => {
            let method = node
                .config
                .get("method")
                .and_then(Value::as_str)
                .unwrap_or("GET")
                .to_uppercase();
            // A `GET` the author knows has a side effect — the method promises
            // to have done nothing, and an endpoint is free to break that
            // promise. Same one-way rule as the `tool_call` arm: the
            // declaration adds this node to the guarded set and can never take
            // a non-safe method out of it.
            if SAFE_METHODS.contains(&method.as_str()) && !declared_unrepeatable {
                return None;
            }
            Some(format!("http_request {method}"))
        }
        _ => None,
    }
}

/// HTTP methods a continuation may repeat: the **safe** ones, not the
/// idempotent ones.
///
/// `PUT` and `DELETE` are idempotent in the RFC's sense — the server state after
/// N identical requests equals the state after one — and that is deliberately
/// not the property being asked for. A duplicate `DELETE` still fires whatever
/// the endpoint does on receipt, and "the row is still gone" is no comfort if
/// the second request also sent someone a notification. Only `GET` and `HEAD`
/// promise to have done nothing.
const SAFE_METHODS: [&str; 2] = ["GET", "HEAD"];

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ports::run_output::RUN_OUTPUT_MAX_BYTES;
    use crate::runtime::workflow_resume::CONTINUATION_PERFORMED_KEY;
    use crate::workflows::caps::resolver::ChildGateRecord;
    use tinyflows::model::Node;

    use crate::company::{WorkflowEdgeDef, WorkflowNodeDef};

    /// The authored file behind a test graph: one `WorkflowNodeDef` per
    /// `(id, kind, repeatable)`, and nothing else set.
    ///
    /// Declared kinds matter — [`declared_unrepeatable`] filters on them, so a
    /// test that passed the wrong kind would assert nothing.
    fn authored(nodes: &[(&str, WorkflowNodeKind, Option<bool>)]) -> WorkflowFile {
        WorkflowFile {
            id: "wf".into(),
            name: "wf".into(),
            description: None,
            owner_desk: None,
            nodes: nodes
                .iter()
                .map(|(id, kind, repeatable)| WorkflowNodeDef {
                    id: (*id).to_string(),
                    kind: *kind,
                    name: String::new(),
                    summary: None,
                    agent: None,
                    schedule: None,
                    config: None,
                    on_error: None,
                    retry: None,
                    requires_approval: None,
                    repeatable: *repeatable,
                    destination: None,
                    postcondition: None,
                })
                .collect(),
            edges: Vec::<WorkflowEdgeDef>::new(),
            global: false,
        }
    }

    /// An authored file that declares nothing — the pre-#850 world, and the
    /// shape every test written before this issue implicitly assumed.
    fn undeclared() -> WorkflowFile {
        authored(&[])
    }

    fn node(id: &str, kind: NodeKind, config: Value) -> Node {
        Node {
            id: id.to_string(),
            kind,
            type_version: 1,
            name: String::new(),
            config,
            ports: Vec::new(),
            position: None,
        }
    }

    fn graph(nodes: Vec<Node>) -> WorkflowGraph {
        WorkflowGraph {
            id: Some("wf".to_string()),
            nodes,
            ..WorkflowGraph::default()
        }
    }

    /// A settled run's output: one completed node, one capability envelope.
    fn settled(node_id: &str, raw: Value) -> Value {
        json!({ "nodes": { node_id: { "items": [{ "json": raw, "text": null, "raw": raw }] } } })
    }

    /// A `POST` that already fired is recorded, so a continuation can replay it.
    ///
    /// The live half of issue #846: an `http_request` node reaches an arbitrary
    /// address on every company today, so this is the duplicate that is
    /// reachable now rather than the one that becomes reachable when a
    /// send-capable namespace is wired.
    #[test]
    fn a_post_that_already_fired_is_recorded() {
        let g = graph(vec![node(
            "notify",
            NodeKind::HttpRequest,
            json!({ "method": "POST", "url": "https://api.test/hooks" }),
        )]);
        let (performed, unreplayable) = outward_calls_performed(
            &g,
            &settled("notify", json!({ "status": 201 })),
            &undeclared(),
        );

        assert_eq!(performed.len(), 1, "{performed:?}");
        assert_eq!(performed[0].node, "notify");
        assert_eq!(performed[0].tool, "http_request POST");
        assert_eq!(performed[0].result, json!({ "status": 201 }));
        assert!(unreplayable.is_empty(), "{unreplayable:?}");
    }

    /// A `GET` is not recorded, and neither is a read-only tool.
    ///
    /// The negative control for the classification, and the issue's own reading
    /// of its reproduction: the three re-executed `web_fetch` nodes still
    /// re-execute, because repeating a read costs latency rather than reaching
    /// anybody. `web_search` is the `Reach::Money` carve-out — declared
    /// `EffectGroup::Spend` for billing, but still a read.
    #[test]
    fn reads_are_not_recorded_whatever_they_cost() {
        let g = graph(vec![
            node(
                "fetch",
                NodeKind::HttpRequest,
                json!({ "method": "GET", "url": "https://api.test/x" }),
            ),
            node(
                "implicit_get",
                NodeKind::HttpRequest,
                json!({ "url": "https://api.test/x" }),
            ),
            node(
                "page",
                NodeKind::ToolCall,
                json!({ "slug": "web_fetch", "args": { "url": "https://www.bbc.com/sport" } }),
            ),
            node(
                "search",
                NodeKind::ToolCall,
                json!({ "slug": "web_search", "args": { "query": "scores" } }),
            ),
        ]);
        let mut output = json!({ "nodes": {} });
        for id in ["fetch", "implicit_get", "page", "search"] {
            output["nodes"][id] = settled(id, json!({ "ok": true }))["nodes"][id].clone();
        }

        let (performed, unreplayable) = outward_calls_performed(&g, &output, &undeclared());
        assert!(performed.is_empty(), "{performed:?}");
        assert!(unreplayable.is_empty(), "{unreplayable:?}");
    }

    /// `shell` is NOT recorded on its own (issue #850).
    ///
    /// The load-bearing negative. `shell` runs an arbitrary command the host
    /// does not parse, so it is `EffectGroup::Other` and falls in the residual
    /// bucket — and it must stay there. Replaying it by default would stop
    /// every `shell` node that builds, lints or reads from re-running, to guard
    /// the rare one that reached a counterparty. #846 declined that trade on
    /// purpose; this test is what stops a later change making it silently.
    #[test]
    fn shell_is_not_recorded_without_a_declaration() {
        let g = graph(vec![node(
            "build",
            NodeKind::ToolCall,
            json!({ "slug": "shell", "args": { "command": "curl -X POST https://api.test/x" } }),
        )]);
        let (performed, unreplayable) = outward_calls_performed(
            &g,
            &settled("build", json!({ "stdout": "" })),
            &undeclared(),
        );
        assert!(performed.is_empty(), "{performed:?}");
        assert!(unreplayable.is_empty(), "{unreplayable:?}");
    }

    /// `repeatable = false` puts a `shell` node in the guarded set (issue #850).
    ///
    /// The whole feature: the author states what the host cannot infer, and the
    /// same recording path every classified call already travels picks it up.
    #[test]
    fn a_declared_shell_node_is_recorded() {
        let g = graph(vec![node(
            "publish",
            NodeKind::ToolCall,
            json!({ "slug": "shell", "args": { "command": "./bin/announce" } }),
        )]);
        let (performed, unreplayable) = outward_calls_performed(
            &g,
            &settled("publish", json!({ "stdout": "sent" })),
            &authored(&[("publish", WorkflowNodeKind::ToolCall, Some(false))]),
        );
        assert!(unreplayable.is_empty(), "{unreplayable:?}");
        assert_eq!(performed.len(), 1, "{performed:?}");
        assert_eq!(performed[0].node, "publish");
        assert_eq!(performed[0].tool, "shell");
    }

    /// A replayed declared node still records its own tool name, not the
    /// replay sentinel (issue #850 + #846 interaction).
    ///
    /// `replay_performed` overwrites a declared node's own config with the
    /// `REPLAY_SLUG` sentinel before this run's engine ever executes it, so by
    /// the time `outward_calls_performed` looks at the graph, the node's
    /// `slug` reads `__opencompany.already_performed`, not `shell`. Recording
    /// that sentinel verbatim would put it on the operator's approval card in
    /// place of the tool name — and dropping the node instead (the naive fix)
    /// would stop tracking it after this hop: a third run downstream of a
    /// second gate would find an empty ledger entry for it and call `shell`
    /// for real, which is exactly the violation issue #850 exists to prevent.
    /// This pins both: the name stays correct, and the node stays guarded.
    #[test]
    fn a_replayed_declared_node_keeps_its_own_name() {
        let authored = {
            let mut file = authored(&[("publish", WorkflowNodeKind::ToolCall, Some(false))]);
            file.nodes[0].config = Some(json!({
                "slug": "shell",
                "args": { "command": "./bin/announce" }
            }));
            file
        };
        // What `replay_performed` leaves behind on the second hop: the node's
        // own config is gone, replaced by the sentinel.
        let g = graph(vec![node(
            "publish",
            NodeKind::ToolCall,
            json!({ "slug": REPLAY_SLUG, "args": { "__replayed_result": "\"sent\"" } }),
        )]);
        let (performed, unreplayable) = outward_calls_performed(
            &g,
            &settled("publish", json!({ "stdout": "sent" })),
            &authored,
        );
        assert!(unreplayable.is_empty(), "{unreplayable:?}");
        assert_eq!(performed.len(), 1, "{performed:?}");
        assert_eq!(performed[0].node, "publish");
        assert_eq!(
            performed[0].tool, "shell",
            "must record the authored tool name, not the replay sentinel — recording the \
             sentinel is a display bug on the operator's card, and dropping the node instead \
             would silently let it run for real on the hop after next"
        );
    }

    /// The same recovery, for an `http_request` node (issue #850 + #846
    /// interaction).
    ///
    /// `declared_call_names` has a separate branch for `HttpRequest` that
    /// builds the name from the authored method rather than a `slug` — this
    /// pins that it is actually reached through the sentinel-recovery path,
    /// not just present in the source. `replay_performed` converts a replayed
    /// `HttpRequest` node into `NodeKind::ToolCall` with `REPLAY_SLUG` — the
    /// same shape a replayed `ToolCall` node ends up in — so this is the
    /// fixture that exercises the `HttpRequest` arm of `declared_call_names`
    /// rather than its `ToolCall` arm.
    #[test]
    fn a_replayed_declared_http_node_keeps_its_own_name() {
        let authored = {
            let mut file = authored(&[("notify", WorkflowNodeKind::HttpRequest, Some(false))]);
            file.nodes[0].config = Some(json!({
                "method": "POST",
                "url": "https://api.test/hooks"
            }));
            file
        };
        // What `replay_performed` leaves behind on the second hop: kind
        // rewritten to `ToolCall`, config replaced by the sentinel — an
        // `HttpRequest` node is indistinguishable in shape from a replayed
        // `ToolCall` node by the time this function sees it.
        let g = graph(vec![node(
            "notify",
            NodeKind::ToolCall,
            json!({ "slug": REPLAY_SLUG, "args": { "__replayed_result": "{\"status\":201}" } }),
        )]);
        let (performed, unreplayable) =
            outward_calls_performed(&g, &settled("notify", json!({ "status": 201 })), &authored);
        assert!(unreplayable.is_empty(), "{unreplayable:?}");
        assert_eq!(performed.len(), 1, "{performed:?}");
        assert_eq!(performed[0].node, "notify");
        assert_eq!(
            performed[0].tool, "http_request POST",
            "must recover the authored method-based name through the HttpRequest arm of \
             declared_call_names, not the replay sentinel"
        );
    }

    /// A declaration only ever ADDS a guard.
    ///
    /// `repeatable = true` on a node the host already classifies as outward is
    /// not a way to switch #846 off. The author is not more authoritative than
    /// the consequence table about a call the table can see; the field exists
    /// for the calls it cannot.
    #[test]
    fn repeatable_true_cannot_remove_a_guard() {
        let g = graph(vec![node(
            "notify",
            NodeKind::HttpRequest,
            json!({ "method": "POST", "url": "https://api.test/hooks" }),
        )]);
        let (performed, _) = outward_calls_performed(
            &g,
            &settled("notify", json!({ "status": 201 })),
            &authored(&[("notify", WorkflowNodeKind::HttpRequest, Some(true))]),
        );
        assert_eq!(
            performed.len(),
            1,
            "a declared-repeatable POST is still guarded"
        );
    }

    /// A declaration on a `GET` guards it, because an endpoint is free to break
    /// the promise the method makes.
    #[test]
    fn a_declared_get_is_recorded() {
        let g = graph(vec![node(
            "trip",
            NodeKind::HttpRequest,
            json!({ "method": "GET", "url": "https://api.test/fire" }),
        )]);
        let (performed, _) = outward_calls_performed(
            &g,
            &settled("trip", json!({ "status": 200 })),
            &authored(&[("trip", WorkflowNodeKind::HttpRequest, Some(false))]),
        );
        assert_eq!(performed.len(), 1, "{performed:?}");
    }

    /// A declaration names a node id, and a stale one guards nothing.
    ///
    /// A graph edited between the pause and the approval can leave a
    /// declaration naming a node that is gone. Silently guarding the wrong node
    /// would be worse than guarding none, so the lookup is by id and misses.
    #[test]
    fn a_declaration_for_a_node_not_in_the_graph_guards_nothing() {
        let g = graph(vec![node(
            "build",
            NodeKind::ToolCall,
            json!({ "slug": "shell", "args": { "command": "make" } }),
        )]);
        let (performed, unreplayable) = outward_calls_performed(
            &g,
            &settled("build", json!({ "stdout": "" })),
            &authored(&[("gone", WorkflowNodeKind::ToolCall, Some(false))]),
        );
        assert!(performed.is_empty(), "{performed:?}");
        assert!(unreplayable.is_empty(), "{unreplayable:?}");
    }

    /// A declared node still obeys every limit `outward_calls_performed`
    /// already enforces — here, the fan-out refusal.
    ///
    /// The declaration decides *whether the node is guarded*, never *how*. A
    /// declared fan-out is still refused and surfaced, because one recorded
    /// result cannot answer N invocations whoever asked for the guard.
    #[test]
    fn a_declared_fan_out_is_still_refused() {
        let g = graph(vec![node(
            "notify_each",
            NodeKind::ToolCall,
            json!({ "slug": "shell", "args": { "command": "./bin/announce" } }),
        )]);
        let output = json!({
            "nodes": { "notify_each": { "items": [
                { "raw": { "status": 201 } },
                { "raw": { "status": 201 } }
            ] } }
        });
        let (performed, unreplayable) = outward_calls_performed(
            &g,
            &output,
            &authored(&[("notify_each", WorkflowNodeKind::ToolCall, Some(false))]),
        );
        assert!(performed.is_empty(), "{performed:?}");
        assert_eq!(unreplayable.len(), 1, "{unreplayable:?}");
        assert_eq!(unreplayable[0].node_id, "notify_each");
    }

    /// A declaration on a kind that makes no call is ignored here as well as
    /// rejected at validation.
    ///
    /// Validation is the place an author hears about it; this is the belt to
    /// that braces, so a graph loaded from an older or looser source cannot
    /// widen the guarded set through a kind the rewrite would not touch.
    #[test]
    fn a_declaration_on_a_non_calling_kind_is_ignored() {
        let file = authored(&[("report", WorkflowNodeKind::Output, Some(false))]);
        assert!(declared_unrepeatable(&file).is_empty());
    }

    /// A node the run never reached is not recorded — which is what makes
    /// "recorded" mean "actually happened" rather than "is in the graph".
    #[test]
    fn a_node_the_run_never_reached_is_not_recorded() {
        let g = graph(vec![node(
            "notify",
            NodeKind::HttpRequest,
            json!({ "method": "POST", "url": "https://api.test/hooks" }),
        )]);
        let (performed, unreplayable) = outward_calls_performed(
            &g,
            &json!({ "nodes": { "notify": { "items": [] } } }),
            &undeclared(),
        );
        assert!(performed.is_empty(), "{performed:?}");
        assert!(unreplayable.is_empty(), "{unreplayable:?}");
    }

    /// A per-item fan-out is NOT recorded, and says so.
    ///
    /// The invoker sees no item index, so one recorded result cannot answer N
    /// invocations without inventing which. Guarding it approximately would be
    /// worse than not guarding it, so this refuses — and surfaces a notice, so
    /// the operator learns before they approve rather than afterwards.
    #[test]
    fn a_fan_out_is_refused_and_surfaced() {
        let g = graph(vec![node(
            "notify_each",
            NodeKind::HttpRequest,
            json!({ "method": "POST", "url": "https://api.test/hooks" }),
        )]);
        let output = json!({
            "nodes": { "notify_each": { "items": [
                { "raw": { "status": 201 } },
                { "raw": { "status": 201 } },
            ] } }
        });

        let (performed, unreplayable) = outward_calls_performed(&g, &output, &undeclared());
        assert!(performed.is_empty(), "{performed:?}");
        assert_eq!(unreplayable.len(), 1, "{unreplayable:?}");
        assert_eq!(unreplayable[0].node_id, "notify_each");
        assert!(
            unreplayable[0].notice().contains("will call it again"),
            "the notice must say what approving does: {}",
            unreplayable[0].notice()
        );
    }

    /// A result too large for the card is NOT recorded.
    ///
    /// A duplicate send is bad; feeding a downstream node a silently-clipped
    /// receipt as though it were whole is worse, so the guard withdraws rather
    /// than degrading — and says so.
    #[test]
    fn an_oversized_result_is_refused_rather_than_clipped() {
        let g = graph(vec![node(
            "notify",
            NodeKind::HttpRequest,
            json!({ "method": "POST", "url": "https://api.test/hooks" }),
        )]);
        let huge = json!({ "body": "x".repeat(RUN_OUTPUT_MAX_BYTES + 1) });
        let (performed, unreplayable) =
            outward_calls_performed(&g, &settled("notify", huge), &undeclared());

        assert!(performed.is_empty(), "{performed:?}");
        assert_eq!(unreplayable.len(), 1, "{unreplayable:?}");
        assert!(
            unreplayable[0].why.contains("too large"),
            "{}",
            unreplayable[0].why
        );
    }

    /// The rewrite: a recorded node invokes the sentinel instead of its tool.
    #[test]
    fn a_recorded_node_is_rewritten_to_replay() {
        let mut g = graph(vec![
            node(
                "notify",
                NodeKind::HttpRequest,
                json!({ "method": "POST", "url": "https://api.test/hooks", "body": "hi" }),
            ),
            node(
                "page",
                NodeKind::ToolCall,
                json!({ "slug": "web_fetch", "args": { "url": "https://www.bbc.com" } }),
            ),
        ]);
        let input = json!({
            CONTINUATION_PERFORMED_KEY: [
                { "node": "notify", "tool": "http_request POST", "result": { "status": 201 } }
            ]
        });

        assert_eq!(replay_performed(&mut g, &input), vec!["notify".to_string()]);

        let notify = &g.nodes[0];
        assert_eq!(notify.kind, NodeKind::ToolCall, "the seam is the invoker's");
        assert_eq!(notify.config["slug"], json!(REPLAY_SLUG));
        assert_eq!(notify.config["execution"], json!("once"));
        // The request descriptor is gone, so nothing resolves a URL for a call
        // that is not made.
        for key in ["url", "method", "body"] {
            assert!(
                notify.config.get(key).is_none(),
                "{key} survived the rewrite"
            );
        }
        // An unrecorded node is untouched — this promotes, it never rewrites
        // what it was not told about.
        assert_eq!(g.nodes[1].config["slug"], json!("web_fetch"));
    }

    /// A first run — no ledger — leaves the graph byte-identical.
    ///
    /// The claim every existing run and every existing test depends on.
    #[test]
    fn a_first_run_rewrites_nothing() {
        let original = graph(vec![node(
            "notify",
            NodeKind::HttpRequest,
            json!({ "method": "POST", "url": "https://api.test/hooks" }),
        )]);
        let mut g = original.clone();

        assert!(replay_performed(&mut g, &json!({ "topic": "q3" })).is_empty());
        assert_eq!(
            serde_json::to_value(&g).unwrap(),
            serde_json::to_value(&original).unwrap(),
        );
    }

    /// The invoker answers the sentinel with the verbatim recorded value.
    #[test]
    fn the_sentinel_returns_the_recorded_result() {
        let encoded = serde_json::to_string(&json!({ "status": 201, "id": "abc" })).unwrap();
        let replayed = replayed_result(REPLAY_SLUG, &json!({ REPLAY_RESULT_KEY: encoded }));
        assert_eq!(replayed, Some(json!({ "status": 201, "id": "abc" })));

        // Every other slug is none of this module's business.
        assert_eq!(replayed_result("web_fetch", &json!({ "url": "x" })), None);
        // A sentinel with nothing to replay yields null rather than falling
        // through to the real toolbelt — falling through is the one outcome
        // this exists to prevent.
        assert_eq!(replayed_result(REPLAY_SLUG, &json!({})), Some(Value::Null));
    }

    /// A recorded result containing an `=`-prefixed string is replayed
    /// **verbatim**, not evaluated as an engine expression.
    ///
    /// This is why the result is JSON-encoded into a single string rather than
    /// embedded in the node config: `tinyflows::expr::resolve` walks a node's
    /// config before it runs and evaluates every leaf beginning with `=`, and a
    /// recorded result is arbitrary data from a counterparty. The encoding makes
    /// that inert by construction — `serde_json::to_string` never yields a
    /// document starting with `=`.
    #[test]
    fn a_recorded_expression_string_is_not_evaluated() {
        let hostile = json!({ "note": "=run.trigger.secret" });
        let mut g = graph(vec![node(
            "notify",
            NodeKind::HttpRequest,
            json!({ "method": "POST", "url": "https://api.test/hooks" }),
        )]);
        let (performed, _) =
            outward_calls_performed(&g, &settled("notify", hostile.clone()), &undeclared());
        let input = json!({ CONTINUATION_PERFORMED_KEY: performed });

        replay_performed(&mut g, &input);
        let args = &g.nodes[0].config["args"];

        // Nothing in the rewritten config is an `=`-expression: the whole
        // recorded value is one JSON string.
        let encoded = args[REPLAY_RESULT_KEY]
            .as_str()
            .expect("encoded as a string");
        assert!(!encoded.starts_with('='), "{encoded}");
        assert_eq!(replayed_result(REPLAY_SLUG, args), Some(hostile));
    }

    // ---- Issue #617: child-gate repeat warnings ---------------------------

    /// A parent graph running one child from a node named `sub`.
    fn child_parent_graph(child_id: &str) -> WorkflowGraph {
        graph(vec![node(
            "sub",
            NodeKind::SubWorkflow,
            json!({ "workflow_id": child_id }),
        )])
    }

    /// A child graph with an ungated POST then two sequential gated POSTs —
    /// the shape the repeat warning exists for.
    fn two_gate_child_graph() -> WorkflowGraph {
        let mut g = graph(vec![
            node(
                "notify",
                NodeKind::HttpRequest,
                json!({ "method": "POST", "url": "https://api.test/notify" }),
            ),
            node(
                "work",
                NodeKind::HttpRequest,
                json!({
                    "method": "POST",
                    "url": "https://api.test/work",
                    "requires_approval": true,
                }),
            ),
            node(
                "work2",
                NodeKind::HttpRequest,
                json!({
                    "method": "POST",
                    "url": "https://api.test/work2",
                    "requires_approval": true,
                }),
            ),
            node("done", NodeKind::Transform, json!({})),
        ]);
        g.edges = vec![
            tinyflows::model::Edge {
                from_node: "notify".into(),
                from_port: "main".into(),
                to_node: "work".into(),
                to_port: "main".into(),
            },
            tinyflows::model::Edge {
                from_node: "work".into(),
                from_port: "main".into(),
                to_node: "work2".into(),
                to_port: "main".into(),
            },
            tinyflows::model::Edge {
                from_node: "work2".into(),
                from_port: "main".into(),
                to_node: "done".into(),
                to_port: "main".into(),
            },
        ];
        g
    }

    /// A registry holding `child`'s gated record.
    fn child_registry(graph: WorkflowGraph) -> ChildGateRegistry {
        let registry = ChildGateRegistry::default();
        registry.record(
            "child",
            ChildGateRecord {
                graph,
                gated: Vec::new(),
            },
        );
        registry
    }

    /// Issue #617. Two sequential gated nodes in one child: approving the first
    /// makes it execute on the continuation (the engine skips the interrupt
    /// only when the id is listed), the child then pauses at the second, and
    /// approving that re-runs the child with BOTH approvals — so the first
    /// gate's call fires on every hop. A `requires_approval` exclusion that
    /// treats every such node as still-blocked would omit exactly the call the
    /// operator must be warned about.
    #[test]
    fn an_approved_child_gate_that_will_fire_again_is_reported() {
        let parent = child_parent_graph("child");
        let pending = vec!["sub::work2".to_string()];
        let input = json!({ "approvals": ["sub::work"] });

        let warned = child_calls_to_repeat(
            &parent,
            &pending,
            &child_registry(two_gate_child_graph()),
            &input,
        );

        let ids: Vec<&str> = warned.iter().map(|w| w.node_id.as_str()).collect();
        assert_eq!(ids, vec!["notify", "work"], "{warned:?}");
        assert!(
            !warned.iter().any(|w| w.node_id == "work2"),
            "the gate this run paused on is not yet approved, so it is excluded: {warned:?}"
        );
    }

    /// The negative control: a not-yet-approved gate stays excluded. On the run
    /// that parks it the child restarts and pauses at it again — it does not
    /// execute — so reporting it would be a warning for a call that will not
    /// happen.
    #[test]
    fn a_gate_this_run_has_not_cleared_stays_excluded() {
        let parent = child_parent_graph("child");
        let pending = vec!["sub::work".to_string()];

        let warned = child_calls_to_repeat(
            &parent,
            &pending,
            &child_registry(two_gate_child_graph()),
            &json!({}),
        );

        let ids: Vec<&str> = warned.iter().map(|w| w.node_id.as_str()).collect();
        assert_eq!(ids, vec!["notify"], "{warned:?}");
    }

    /// Issue #617, nested ancestor calls are included too. A call in the
    /// intermediate child has already happened before that child enters its
    /// nested workflow, so approving the grandchild gate restarts and repeats
    /// the intermediate call as well.
    #[test]
    fn a_nested_gate_warns_for_calls_in_ancestor_children() {
        let mut ancestor = graph(vec![
            node(
                "notify_parent_child",
                NodeKind::HttpRequest,
                json!({ "method": "POST", "url": "https://api.test/ancestor" }),
            ),
            node(
                "nested",
                NodeKind::SubWorkflow,
                json!({ "workflow_id": "b" }),
            ),
        ]);
        ancestor.edges = vec![tinyflows::model::Edge {
            from_node: "notify_parent_child".into(),
            from_port: "main".into(),
            to_node: "nested".into(),
            to_port: "main".into(),
        }];

        let registry = ChildGateRegistry::default();
        registry.record(
            "a",
            ChildGateRecord {
                graph: ancestor,
                gated: Vec::new(),
            },
        );
        registry.record(
            "b",
            ChildGateRecord {
                graph: two_gate_child_graph(),
                gated: Vec::new(),
            },
        );

        let warned = child_calls_to_repeat(
            &child_parent_graph("a"),
            &["sub::nested::work2".to_string()],
            &registry,
            &json!({ "approvals": ["sub::nested::work"] }),
        );

        let ids: Vec<&str> = warned.iter().map(|w| w.node_id.as_str()).collect();
        assert_eq!(
            ids,
            vec!["sub::notify_parent_child", "notify", "work"],
            "{warned:?}"
        );
    }

    /// `sub::nested::work`; the child whose graph holds `work` is the
    /// grandchild, reachable only by descending the registry through the
    /// intermediate `sub_workflow` node's `workflow_id`. The approved first
    /// gate then reads `sub::nested::work` off the same namespace.
    #[test]
    fn a_two_level_child_namespace_warns_for_the_grandchilds_upstream() {
        let registry = ChildGateRegistry::default();
        registry.record(
            "a",
            ChildGateRecord {
                graph: graph(vec![node(
                    "nested",
                    NodeKind::SubWorkflow,
                    json!({ "workflow_id": "b" }),
                )]),
                gated: Vec::new(),
            },
        );
        registry.record(
            "b",
            ChildGateRecord {
                graph: two_gate_child_graph(),
                gated: Vec::new(),
            },
        );
        let parent = child_parent_graph("a");
        let pending = vec!["sub::nested::work2".to_string()];
        let input = json!({ "approvals": ["sub::nested::work"] });

        let warned = child_calls_to_repeat(&parent, &pending, &registry, &input);

        let ids: Vec<&str> = warned.iter().map(|w| w.node_id.as_str()).collect();
        assert_eq!(ids, vec!["notify", "work"], "{warned:?}");
    }

    /// Issue #617, the dynamic half. A `workflow_id = "=item.target"` child is
    /// keyed in the registry by the RESOLVED id, so the repeat walk must
    /// resolve the same expression against the trigger input — the same way
    /// [`child_gate_call`](crate::workflows::caps::resolver::child_gate_call)
    /// does for the card — or the warning is silently dropped for a dynamic
    /// child.
    #[test]
    fn an_expr_bound_child_gate_warns_for_the_resolved_children_calls() {
        let parent = graph(vec![node(
            "sub",
            NodeKind::SubWorkflow,
            json!({ "workflow_id": "=item.target" }),
        )]);
        let pending = vec!["sub::work".to_string()];
        let input = json!({ "target": "child" });

        let warned = child_calls_to_repeat(
            &parent,
            &pending,
            &child_registry(two_gate_child_graph()),
            &input,
        );

        let ids: Vec<&str> = warned.iter().map(|w| w.node_id.as_str()).collect();
        assert_eq!(ids, vec!["notify"], "{warned:?}");
    }

    /// A per-item expression-bound child cannot be reconstructed — the paused
    /// id carries no item index to say which element's scope resolved the id —
    /// so the walk falls back rather than describing the wrong child.
    #[test]
    fn a_per_item_expr_bound_child_id_falls_back_to_nothing() {
        let parent = graph(vec![node(
            "sub",
            NodeKind::SubWorkflow,
            json!({ "workflow_id": "=item.target", "execution": "per_item" }),
        )]);
        let pending = vec!["sub::work".to_string()];

        let warned = child_calls_to_repeat(
            &parent,
            &pending,
            &child_registry(two_gate_child_graph()),
            &json!({ "target": "child" }),
        );

        assert!(
            warned.is_empty(),
            "falls back rather than guessing: {warned:?}"
        );
    }
}
