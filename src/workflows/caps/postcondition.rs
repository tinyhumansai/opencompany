//! The deterministic tier of issue #1866's sufficiency gate.
//!
//! The engine advances the moment a node returns `Ok`; nothing checks whether
//! the output is actually enough to hand downstream. The extreme case is
//! already fixed independently for the iteration-cap signal (#1865): a node
//! that stops at `max_tool_iterations` settles `Failed` rather than flowing a
//! truncated reply on as if it were a finished answer. This module is the
//! general form of that same idea, expressed as an author-declared,
//! **mechanical** check rather than a signal the engine happens to expose —
//! "the output has this shape, or it isn't good enough to advance."
//!
//! Deliberately narrow: three predicates, no LLM call, no network, no state.
//! The semantic judge tier the issue also describes (a tool-less model call
//! for nodes whose sufficiency cannot be expressed as a predicate) is Wave 3,
//! gated on #1861's blocker-park plumbing landing first — this module only
//! ever returns `Ok` or a plain-English gap sentence.
//!
//! # Fail-open on the unknown case, fail-closed on the broken one
//!
//! [`evaluate_postcondition`] is validated at author time
//! ([`crate::company::workflow_file::validate`] rejects an unknown `require`
//! before a graph is ever saved), so an unrecognized `require` reaching this
//! function at runtime can only mean a graph saved by an older or newer
//! version of the validator disagreeing with this binary. Observability must
//! never be able to fail the work it is observing (the same rule
//! [`super::HarnessAgentRunner::run_turn`] already applies to a failed
//! attempt-row mint) — so the unknown case warns and lets the node through
//! rather than halting a run over a predicate this binary cannot evaluate.
//!
//! That reasoning does NOT extend to `field_present` losing its own `field`.
//! `validate` requires every `field_present` postcondition to carry a
//! non-empty `field`, so this function is never handed one the validator
//! approved without it — a `field_present` spec reaching here with a
//! missing/non-string `field` means something rewrote a validated value
//! between save and this call (issue #1937/#1866: the whole `postcondition`
//! rides inside the engine-resolved node config, so an authored `field =
//! "=item.missing"` — a plausible mistake, since `=`-expressions are the
//! normal syntax everywhere else in config — gets evaluated by the SAME
//! generic config resolution as any other value, and a miss resolves to
//! `null` indistinguishably from "no field was ever authored"). Unlike an
//! unrecognized `require`, this is not "a predicate this binary cannot
//! evaluate" — `field_present` is fully understood here, it just has nothing
//! left to check. Passing it through anyway would silently switch the whole
//! gate off for exactly the graphs that most need it caught, so this one
//! case fails CLOSED instead: see the `field_present` arm below.

use serde_json::Value;

/// Evaluates a node's declared `postcondition` against its output envelope.
///
/// `spec` is the raw `postcondition` config node ({ "require": ..., "field":
/// ... (optional) }); `output` is the node's output value — for an agent node
/// today, the `{ "text", "agent_ref" }` envelope [`super::HarnessAgentRunner::run_turn`]
/// builds. `Ok(())` means the output clears the gate; `Err(gap)` carries a
/// plain-English sentence naming what is missing, suitable to surface as the
/// halting attempt's error message.
///
/// Three predicates:
/// - `non_empty` — the envelope's `text` is present and non-empty after
///   trimming whitespace. Catches the truncation class this issue opens
///   with: a capped or refused turn that still produced *some* prose.
/// - `field_present` — the dotted `field` path resolves to a present,
///   non-null value in `output`. If `field` itself did not resolve to a
///   usable name (a validated postcondition's own `field` going missing/
///   non-string between save and this call), this fails CLOSED rather than
///   passing the node through — see the module doc's second section.
/// - `non_empty_list` — the target (the whole `output`, or the dotted
///   `field` path within it when given) is a JSON array with at least one
///   element.
///
/// Any OTHER `require` value (one this function does not recognize at all)
/// fails OPEN: a `tracing::warn!` is emitted and the node is allowed to
/// proceed. See the module doc for why that case, specifically, is
/// different from `field_present` losing its `field`.
pub(crate) fn evaluate_postcondition(spec: &Value, output: &Value) -> Result<(), String> {
    let require = spec.get("require").and_then(Value::as_str).unwrap_or("");
    let field = spec
        .get("field")
        .and_then(Value::as_str)
        .filter(|f| !f.is_empty());

    match require {
        "non_empty" => {
            let text = output.get("text").and_then(Value::as_str).unwrap_or("");
            if text.trim().is_empty() {
                Err("the node's output was empty — nothing was produced to advance on.".to_string())
            } else {
                Ok(())
            }
        }
        "field_present" => {
            let Some(path) = field else {
                // Codex #3894038816 on #1937 — deliberately NOT the same
                // fail-open shape as the unknown-`require` case below. That
                // one is genuinely ambiguous (a future/older validator this
                // binary disagrees with — see the module doc). This one is
                // not: `workflow_file::validate` REQUIRES a `field` on every
                // `field_present` postcondition it ever saves, so a spec
                // reaching here with no usable `field` cannot be a graph the
                // validator approved as written — it means something
                // rewrote a validated `field` into null/non-string between
                // save and this call (an authored `=`-expression resolved
                // away by config resolution is the concrete case that
                // motivated this; see `workflows::caps::tests::
                // a_field_resolved_away_by_an_authored_expression_fails_closed_at_run_turn`).
                // `field_present`'s entire job is checking that one named
                // field exists — evaluating it with no field to check is
                // failing the very thing it exists to verify, not an
                // ambiguous no-op. Fail CLOSED: an authored safety gate must
                // never be silently switched off by a resolution quirk this
                // binary did not foresee.
                tracing::warn!(
                    require,
                    "workflow postcondition: `field_present` declared but its `field` did \
                     not resolve to a name to check — failing the node rather than silently \
                     passing an unverifiable gate"
                );
                return Err(
                    "the node's postcondition declares `field_present` but its `field` did \
                     not resolve to a name to check — refusing to advance rather than \
                     silently pass an unverifiable gate."
                        .to_string(),
                );
            };
            match resolve_path(output, path) {
                // Codex #3894162757 on #1937 — a bare scalar under the exact
                // `json` root is the one shape `field_present` must refuse
                // to certify even though it genuinely resolves. `path ==
                // "json"` means the author is checking the WHOLE parsed
                // reply, which is exactly what a downstream `=item.json`
                // binding reads too — but tinyflows' own envelope
                // construction (`finish_agent_run`/`envelope::structured_of`
                // in the vendored engine) normalizes anything that is not an
                // `Object`/`Array` to `Value::Null` on the way to that
                // binding. Certifying a scalar here would pass a gate whose
                // value the workflow can never actually read — the same
                // "gate certifies X, item never gets X" defect this whole
                // module exists to close, just reached through a shape that
                // resolves cleanly rather than one that resolves to null.
                // A dotted path UNDER `json` (`json.count`) is unaffected:
                // reaching a scalar there means the reply was already an
                // object, which merges into the emitted value intact (see
                // `HarnessAgentRunner::run_turn`'s `Value::Object` arm), so
                // `item.json.count` really does resolve downstream.
                Some(value) if path == "json" && is_bare_scalar(value) => Err(format!(
                    "the node's output under `json` is a bare scalar ({value}) — a \
                     downstream `=item.json` binding can never see it (tinyflows only \
                     carries an object or array through `json`; anything else \
                     normalizes to null), so this postcondition can never certify a \
                     value the workflow can actually read. Have the agent reply with \
                     an object instead naming it (e.g. `{{\"value\": ...}}`) and target \
                     the dotted path (`field = \"json.value\"`)."
                )),
                Some(value) if !value.is_null() => Ok(()),
                _ => Err(format!(
                    "the node's output is missing `{path}` — the expected field never landed."
                )),
            }
        }
        "non_empty_list" => {
            let target = match field {
                Some(path) => resolve_path(output, path),
                // No `field` given: for the standard `{ json, text, raw }`
                // envelope, "the output" means the structured payload under
                // `json`, not the envelope wrapper itself — the wrapper is
                // always an object (it also carries `text`/`agent_ref`), so
                // checking it directly could never see a `Value::Array` even
                // when the underlying result genuinely is a list. Falls back
                // to the raw value for any caller not using that envelope
                // shape (no `json` key at all), which is the pre-existing
                // behavior.
                None => output.get("json").or(Some(output)),
            };
            let described = field
                .map(|path| format!("`{path}`"))
                .unwrap_or_else(|| "the output".to_string());
            match target {
                Some(Value::Array(items)) if !items.is_empty() => Ok(()),
                Some(Value::Array(_)) => Err(format!(
                    "{described} is an empty list — nothing came back to advance on."
                )),
                Some(_) => Err(format!(
                    "{described} is not a list — the shape does not match."
                )),
                None => Err(format!(
                    "{described} is missing — nothing came back to advance on."
                )),
            }
        }
        other => {
            tracing::warn!(
                require = other,
                "workflow postcondition: unknown `require` — passing the node through unevaluated"
            );
            Ok(())
        }
    }
}

/// Resolves a dot-separated path (`"a.b.c"`) through nested JSON objects.
/// Does not index into arrays — every hop is an object-field lookup, which is
/// all the two field-aware predicates above need.
fn resolve_path<'a>(value: &'a Value, path: &str) -> Option<&'a Value> {
    path.split('.').try_fold(value, |acc, key| acc.get(key))
}

/// True for a JSON value with no structure of its own — a bool, number, or
/// string. Used by the `field_present`-on-bare-`json` check: these are the
/// shapes tinyflows' own envelope construction discards (normalizes to
/// `Value::Null`) rather than carries through to a downstream `=item.json`
/// binding. `Null` is deliberately excluded — it is handled by the ordinary
/// "missing" branch above this call, not this one.
fn is_bare_scalar(value: &Value) -> bool {
    matches!(value, Value::Bool(_) | Value::Number(_) | Value::String(_))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn spec(require: &str) -> Value {
        json!({ "require": require })
    }

    fn spec_with_field(require: &str, field: &str) -> Value {
        json!({ "require": require, "field": field })
    }

    #[test]
    fn non_empty_passes_on_real_text() {
        let output = json!({ "text": "the report is done", "agent_ref": "a" });
        assert_eq!(evaluate_postcondition(&spec("non_empty"), &output), Ok(()));
    }

    #[test]
    fn non_empty_fails_on_blank_text() {
        let output = json!({ "text": "   ", "agent_ref": "a" });
        assert!(evaluate_postcondition(&spec("non_empty"), &output).is_err());
    }

    #[test]
    fn non_empty_fails_on_missing_text() {
        let output = json!({ "agent_ref": "a" });
        assert!(evaluate_postcondition(&spec("non_empty"), &output).is_err());
    }

    #[test]
    fn field_present_passes_when_the_field_resolves() {
        let output = json!({ "items": [1, 2] });
        assert_eq!(
            evaluate_postcondition(&spec_with_field("field_present", "items"), &output),
            Ok(())
        );
    }

    #[test]
    fn field_present_fails_when_the_field_is_absent() {
        let output = json!({ "text": "prose only" });
        assert!(
            evaluate_postcondition(&spec_with_field("field_present", "items"), &output).is_err()
        );
    }

    #[test]
    fn field_present_fails_when_the_field_is_explicitly_null() {
        let output = json!({ "items": null });
        assert!(
            evaluate_postcondition(&spec_with_field("field_present", "items"), &output).is_err()
        );
    }

    #[test]
    fn field_present_resolves_a_dotted_path() {
        let output = json!({ "json": { "result": { "count": 3 } } });
        assert_eq!(
            evaluate_postcondition(
                &spec_with_field("field_present", "json.result.count"),
                &output
            ),
            Ok(())
        );
    }

    #[test]
    fn field_present_dotted_path_fails_partway_through() {
        let output = json!({ "json": { "result": {} } });
        assert!(
            evaluate_postcondition(
                &spec_with_field("field_present", "json.result.count"),
                &output
            )
            .is_err()
        );
    }

    /// Codex #3894162757 on #1937 — `field_present` on the bare `json` root
    /// resolves for ANY present, non-null value there, including a bare
    /// scalar the best-effort JSON parse produces just as readily as an
    /// object or array. Unlike an object/array, a scalar can never reach a
    /// downstream `=item.json` binding (tinyflows' own envelope construction
    /// normalizes anything but `Object`/`Array` to `Value::Null`), so
    /// certifying it here would pass a gate whose value the workflow can
    /// never actually read — refused instead. See
    /// `workflows::runner::tests::a_scalar_reply_cannot_satisfy_field_present_on_the_bare_json_root`
    /// for the full-engine proof of the delivery gap this closes.
    #[test]
    fn field_present_on_the_bare_json_root_rejects_a_scalar() {
        for scalar in [json!(42), json!(true), json!("ok")] {
            let output = json!({ "text": "irrelevant", "agent_ref": "a", "json": scalar });
            assert!(
                evaluate_postcondition(&spec_with_field("field_present", "json"), &output).is_err(),
                "a bare scalar under `json` must not satisfy field_present on the bare \
                 `json` root: {output}"
            );
        }
    }

    /// Companion GREEN: the rejection above is scoped to the exact `json`
    /// root, not to scalars in general. A dotted path UNDER `json`
    /// (`json.count`) reaching a scalar is fine — getting there at all means
    /// the reply was already an object, which merges into the emitted value
    /// intact (`HarnessAgentRunner::run_turn`'s `Value::Object` arm), so
    /// `item.json.count` really does resolve downstream.
    #[test]
    fn field_present_on_a_dotted_path_under_json_still_accepts_a_scalar() {
        let output = json!({ "json": { "count": 42 } });
        assert_eq!(
            evaluate_postcondition(&spec_with_field("field_present", "json.count"), &output),
            Ok(())
        );
    }

    /// Companion GREEN: the bare `text`/`agent_ref` roots are unaffected —
    /// they are always strings in the envelope tinyflows exposes directly as
    /// `item.text` (never nulled), so a scalar there is the ordinary,
    /// deliverable case, not the `json`-root delivery gap.
    #[test]
    fn field_present_on_bare_text_or_agent_ref_still_accepts_their_string_value() {
        let output = json!({ "text": "hello", "agent_ref": "researcher", "json": null });
        assert_eq!(
            evaluate_postcondition(&spec_with_field("field_present", "text"), &output),
            Ok(())
        );
        assert_eq!(
            evaluate_postcondition(&spec_with_field("field_present", "agent_ref"), &output),
            Ok(())
        );
    }

    /// Codex #3894038816 on #1937 — the silent-disable finding. `postcondition`
    /// rides inside the engine-resolved node config (see
    /// `workflows::caps::tests::a_field_resolved_away_by_an_authored_expression_fails_closed_at_run_turn`
    /// for the full authored-`"=item.missing"` → config-resolution trace), so
    /// a `field` that resolved to anything other than a present string reaches
    /// this function looking IDENTICAL to a `field_present` declared with no
    /// `field` at all — a shape `workflow_file::validate` refuses to ever save
    /// (`postcondition_field_present_without_a_field_is_rejected`). Before
    /// this fix, that shape was fail-OPEN here: a `tracing::warn!` and
    /// `Ok(())`, silently letting every reply through a gate the workflow file
    /// plainly declares. `field_present`'s entire job is checking one named
    /// field exists — evaluating it with no field to check is not an
    /// ambiguous "maybe intended" gap the way an unrecognized `require` is
    /// (the module doc's fail-open case), so this fails CLOSED instead.
    #[test]
    fn field_present_declared_with_a_field_that_resolved_away_fails_closed() {
        let spec = json!({ "require": "field_present", "field": null });
        let output = json!({ "text": "a reply that would satisfy nothing in particular" });
        assert!(
            evaluate_postcondition(&spec, &output).is_err(),
            "a `field_present` postcondition whose own `field` did not resolve to a \
             string must halt the node, not silently pass it — RED on the code as it \
             stood before this fix: this returned Ok(())"
        );
    }

    #[test]
    fn non_empty_list_passes_on_a_populated_array() {
        let output = json!(["a"]);
        assert_eq!(
            evaluate_postcondition(&spec("non_empty_list"), &output),
            Ok(())
        );
    }

    #[test]
    fn non_empty_list_fails_on_an_empty_array() {
        let output = json!([]);
        assert!(evaluate_postcondition(&spec("non_empty_list"), &output).is_err());
    }

    #[test]
    fn non_empty_list_fails_on_a_non_array() {
        let output = json!({ "text": "not a list" });
        assert!(evaluate_postcondition(&spec("non_empty_list"), &output).is_err());
    }

    /// Codex #3894277296 on #1937 — the specific unsatisfiable shape
    /// `company::workflow_file::validate` now refuses at author time
    /// (`postcondition_non_empty_list_on_text_or_agent_ref_is_rejected`):
    /// even if one reached this evaluator anyway, `text`/`agent_ref` are
    /// unconditionally strings in the envelope, so `non_empty_list` fails
    /// them the same honest way it fails any other non-array target — this
    /// pins that the evaluator-level behavior was ALREADY correct, and the
    /// gap was purely that authoring one was ever allowed to save.
    #[test]
    fn non_empty_list_on_text_or_agent_ref_fails_honestly() {
        let output = json!({ "text": "some prose", "agent_ref": "researcher", "json": null });
        for field in ["text", "agent_ref"] {
            assert!(
                evaluate_postcondition(&spec_with_field("non_empty_list", field), &output).is_err(),
                "field `{field}` is always a string, never an array: {output}"
            );
        }
    }

    /// Codex review on #1937 (issue #1866): the no-`field` form must look at
    /// the standard envelope's structured `json` payload, not the envelope
    /// object itself — an agent-node envelope always carries `text`/
    /// `agent_ref` alongside `json`, so checking the envelope directly could
    /// never see a `Value::Array` even when the agent's parsed reply
    /// genuinely is a non-empty list.
    #[test]
    fn non_empty_list_with_no_field_checks_the_envelopes_json_payload() {
        let output = json!({ "text": "[\"a\",\"b\"]", "agent_ref": "a", "json": ["a", "b"] });
        assert_eq!(
            evaluate_postcondition(&spec("non_empty_list"), &output),
            Ok(())
        );
    }

    /// Companion RED-shape: when the envelope's `json` payload didn't parse
    /// (a plain-prose reply), the no-`field` form still fails honestly —
    /// it must not silently pass just because a `json` key exists.
    #[test]
    fn non_empty_list_with_no_field_fails_when_the_envelopes_json_is_null() {
        let output = json!({ "text": "just prose, no list here", "agent_ref": "a", "json": null });
        assert!(evaluate_postcondition(&spec("non_empty_list"), &output).is_err());
    }

    #[test]
    fn non_empty_list_checks_the_named_field_when_given() {
        let output = json!({ "items": ["a", "b"] });
        assert_eq!(
            evaluate_postcondition(&spec_with_field("non_empty_list", "items"), &output),
            Ok(())
        );

        let empty = json!({ "items": [] });
        assert!(
            evaluate_postcondition(&spec_with_field("non_empty_list", "items"), &empty).is_err()
        );
    }

    #[test]
    fn unknown_require_fails_open() {
        let output = json!({});
        assert_eq!(
            evaluate_postcondition(&spec("some_future_predicate"), &output),
            Ok(())
        );
    }
}
