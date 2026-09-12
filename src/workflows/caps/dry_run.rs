//! Stub capabilities for a **dry run** (issue #542).
//!
//! A dry run walks the *real* graph — real compile, real branch selection, real
//! item flow — but over these stubs in place of the effectful capabilities, so
//! it proves a workflow's routing and output shape without any real effect:
//! zero agent inference, zero tool/http execution.
//!
//! # Fail-closed by construction, not by remembering
//!
//! Every effectful slot is stubbed, so there is no path by which a *future*
//! node kind could reach a real effect in a dry run: the engine only ever calls
//! what is on the bundle, and on a dry bundle every effectful entry is one of
//! these. A stub that forgot to exist would be a compile error at
//! [`build_capabilities`](super::build_capabilities), not a silent live effect.
//!
//! # What is NOT stubbed, and why
//!
//! The read-only, effect-free capabilities stay real:
//!
//! * the **resolver** ([`StoreWorkflowResolver`](super::resolver)) — resolving a
//!   `sub_workflow` child is a read, and the child runs under this same dry
//!   bundle, so a dry run propagates into sub-workflows rather than stopping at
//!   the boundary;
//! * **state** is the inert [`NoopState`](super::state::NoopState) — never the
//!   durable [`CompanyStateStore`](super::state::CompanyStateStore), so a dry
//!   run cannot persist run state either;
//! * `llm` / `code` / `memory` are unchanged (already unwired stubs / `None`).
//!
//! # The grant check is kept, deliberately
//!
//! [`DryRunTools`] still runs the fail-closed `[tools].allow` grant check before
//! returning its canned echo. The check is pure — it reads the company's grants
//! and touches nothing outside the process — so keeping it means a `tool_call`
//! the company does not grant is refused in a dry run *exactly* as it is live.
//! A test run is meant to prove routing, and "this node would have been denied"
//! is part of the routing.

use async_trait::async_trait;
use serde_json::{Value, json};
use tinyflows::caps::{AgentRunner, HttpClient, ToolInvoker};
use tinyflows::error::{EngineError, Result as TfResult};

use super::tools::{WorkflowToolWiring, refusal_for};

/// A marker key set on every dry-stub output, so a downstream node (or a test)
/// can tell a stubbed item from a real one.
pub(super) const DRY_RUN_MARKER: &str = "dry_run";

/// The [`AgentRunner`] a dry run wires in place of
/// [`HarnessAgentRunner`](super::HarnessAgentRunner): it returns a structured
/// echo of what the node *would* have asked, with **zero** pool routing and zero
/// inference.
///
/// The reply mirrors the real runner's `{ text, agent_ref }` envelope shape so a
/// downstream `=item.text` binding still resolves — a dry run must exercise the
/// same routing the real one would — plus the [`DRY_RUN_MARKER`] so nothing
/// mistakes the fixture for a real turn.
pub(super) struct DryRunAgent;

#[async_trait]
impl AgentRunner for DryRunAgent {
    async fn run_agent(
        &self,
        agent_ref: &str,
        request: Value,
        _conn: Option<&str>,
    ) -> TfResult<Value> {
        // Same extraction the real runner uses, so the fixture echoes exactly
        // the instruction the live turn would have received.
        let instruction = super::message_from_request(&request);
        tracing::debug!(
            agent = agent_ref,
            "workflow dry run: stubbing agent node (no inference)"
        );
        Ok(json!({
            "text": format!("[dry run] agent `{agent_ref}` would run: {instruction}"),
            "agent_ref": agent_ref,
            "instruction": instruction,
            DRY_RUN_MARKER: true,
        }))
    }
}

/// The [`ToolInvoker`] a dry run wires in place of
/// [`WorkflowToolInvoker`](super::tools::WorkflowToolInvoker): it keeps the
/// exact same fail-closed grant gate — so an ungranted `tool_call` refuses
/// identically in a dry run — but returns a canned echo instead of executing the
/// tool, so nothing touches the workspace, the network, or a priced backend.
pub(super) struct DryRunTools {
    /// The company's `[tools].allow` grant globs — the same gate the live
    /// invoker applies, reused verbatim.
    grants: Vec<String>,
    wiring: WorkflowToolWiring,
}

impl DryRunTools {
    /// Builds a dry invoker gated by the company's `[tools].allow`.
    pub(super) fn new(grants: Vec<String>, wiring: WorkflowToolWiring) -> Self {
        Self { grants, wiring }
    }
}

#[async_trait]
impl ToolInvoker for DryRunTools {
    async fn invoke(&self, slug: &str, args: Value, _conn: Option<&str>) -> TfResult<Value> {
        // Issue #846: the replay arm, mirrored from the live invoker.
        //
        // A dry run cannot reach here through the host's own path — dry runs are
        // never continuations, park no gate and stub every effect — so this is
        // not load-bearing today. It is here because the alternative is worse
        // than redundant: without it, a graph carrying a replay slug would fall
        // through to `namespace_of` and fail the node with "not a wired workflow
        // tool", which is a dry run reporting a routing failure that the real run
        // does not have. The two invokers agreeing about every slug is the
        // property a test run's answer is worth anything for.
        if let Some(result) = super::super::replay::replayed_result(slug, &args) {
            return Ok(result);
        }
        // FAIL-CLOSED grant check FIRST, identical to the live invoker
        // (`WorkflowToolInvoker::invoke`): a dry run must refuse an ungranted
        // tool exactly as a real one does, because that refusal is part of the
        // routing a test run exists to prove. The check is pure — no effect.
        if let Some(message) = refusal_for(slug, &self.grants, &self.wiring) {
            return Err(EngineError::Capability(message));
        }
        tracing::debug!(slug, "workflow dry run: stubbing tool_call (not executed)");
        Ok(json!({
            "text": format!("[dry run] tool_call `{slug}` was not executed"),
            "slug": slug,
            "args": args,
            DRY_RUN_MARKER: true,
        }))
    }
}

/// The [`HttpClient`] a dry run wires in place of
/// [`GuardedHttpClient`](super::http::GuardedHttpClient): it echoes the request
/// descriptor back without issuing anything, so no outbound request (guarded or
/// not) leaves the process.
pub(super) struct DryRunHttp {
    /// The company's `[tools].web_allowed_domains`, so the target check a dry
    /// run *can* make is made against the same list the live client uses.
    allowed_domains: Vec<String>,
}

impl DryRunHttp {
    pub(super) fn new(allowed_domains: Vec<String>) -> Self {
        Self { allowed_domains }
    }
}

#[async_trait]
impl HttpClient for DryRunHttp {
    /// Refuses a target the real run would refuse; otherwise reports the request
    /// as **not checked**, never as a success.
    ///
    /// Issue #1048: this used to return the same cheerful stub for every URL, so
    /// a node aimed at a host the capability layer blocks reported `ok` on Test
    /// run and failed immediately on the real one. Test run is the single
    /// control an operator has before arming a graph on a schedule, so a green
    /// there followed by a refusal on the real run is worse than no dry run at
    /// all — it converts "I checked it" into a false belief.
    ///
    /// The refusal message and `EngineError::Capability` shape match
    /// [`GuardedHttpClient`](super::http::GuardedHttpClient)'s, so a node fails
    /// under its own `on_error`/retry policy exactly as it would live.
    ///
    /// **Still no request is issued, in either branch.** The verdict below is a
    /// pure function of the URL and the company's config — see
    /// [`preflight_refusal`](super::http::preflight_refusal) for what it decides
    /// and, more importantly, what it deliberately leaves to the real run.
    async fn request(&self, request: Value, _conn: Option<&str>) -> TfResult<Value> {
        if let Some(reason) = super::http::preflight_refusal(&request, &self.allowed_domains) {
            tracing::debug!(%reason, "workflow dry run: refusing http_request target");
            return Err(EngineError::Capability(format!("http_request: {reason}")));
        }
        tracing::debug!("workflow dry run: stubbing http_request (not sent)");
        Ok(json!({
            "status": Value::Null,
            // Deliberately not phrased as a result. A dry run cannot know
            // whether the host is up, the credential is current or the response
            // parses, and saying so is more honest than a green that implies it
            // does — the opposite error to the one #1048 fixed, and the one that
            // erodes trust fastest because an operator cannot tell it is wrong.
            "body": "[dry run] http_request was not sent — target allowed, delivery not checked",
            "checked": "target only: a real run may still fail to reach this host",
            "request": request,
            DRY_RUN_MARKER: true,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn dry_agent_echoes_without_inference() {
        let out = DryRunAgent
            .run_agent("ceo", json!({ "prompt": "ship it" }), None)
            .await
            .expect("dry agent never fails");
        assert_eq!(out[DRY_RUN_MARKER], json!(true));
        assert_eq!(out["agent_ref"], "ceo");
        // The `text` envelope is preserved so a downstream `=item.text` resolves.
        assert!(out["text"].as_str().unwrap().contains("ship it"), "{out}");
    }

    #[tokio::test]
    async fn dry_tools_keep_the_grant_gate_but_do_not_execute() {
        // No `code` grant → a `code`-namespace slug is refused, exactly as live.
        let ungranted = DryRunTools::new(
            vec!["web.*".to_string()],
            WorkflowToolWiring {
                wired_namespaces: ["web"].into_iter().collect(),
                ..WorkflowToolWiring::default()
            },
        );
        let denied = ungranted.invoke("csv_export", json!({}), None).await;
        assert!(
            matches!(denied, Err(EngineError::Capability(ref m)) if m.contains("not granted")),
            "{denied:?}"
        );
        // An unknown slug is refused as unwired, exactly as live.
        let unwired = ungranted.invoke("email.send", json!({}), None).await;
        assert!(
            matches!(unwired, Err(EngineError::Capability(ref m)) if m.contains("not a wired")),
            "{unwired:?}"
        );
        // A granted slug returns the canned echo — no execution.
        let granted = DryRunTools::new(
            vec!["code.*".to_string()],
            WorkflowToolWiring {
                wired_namespaces: ["code"].into_iter().collect(),
                ..WorkflowToolWiring::default()
            },
        );
        let echoed = granted
            .invoke("csv_export", json!({ "filename": "x.csv" }), None)
            .await
            .expect("granted dry tool echoes");
        assert_eq!(echoed[DRY_RUN_MARKER], json!(true));
        assert_eq!(echoed["slug"], "csv_export");
    }

    #[tokio::test]
    async fn dry_tools_refuse_granted_search_without_a_backend() {
        let dry = DryRunTools::new(
            vec!["search".to_string()],
            WorkflowToolWiring {
                missing: [(
                    "search",
                    super::super::tools::MissingReason::SearchBackendNotConfigured,
                )]
                .into_iter()
                .collect(),
                ..WorkflowToolWiring::default()
            },
        );
        let refused = dry.invoke("web_search", json!({}), None).await;
        assert!(
            matches!(refused, Err(EngineError::Capability(ref message))
                if message.contains("no managed search backend")
                    && message.contains("Settings → Search")
                    && message.contains("ask the platform operator")),
            "{refused:?}"
        );
    }

    /// An allowed target is still never sent — and the stub says so rather than
    /// implying the request succeeded (issue #1048).
    ///
    /// This case used to be written with `http://127.0.0.1:9/`, a URL the real
    /// guard refuses, so the old assertion pinned the bug in place: it proved a
    /// dry run reports success for a target no real run can reach.
    #[tokio::test]
    async fn dry_http_reports_an_allowed_target_as_unchecked_rather_than_ok() {
        let out = DryRunHttp::new(Vec::new())
            .request(json!({ "url": "https://example.com/hook" }), None)
            .await
            .expect("an allowed target is not refused");
        assert_eq!(out[DRY_RUN_MARKER], json!(true));
        assert_eq!(out["status"], Value::Null);
        let body = out["body"].as_str().unwrap_or_default();
        assert!(
            body.contains("not checked"),
            "a dry run must not imply the request would succeed: {body}"
        );
    }

    /// A target the real run refuses is refused here too, in the same shape.
    #[tokio::test]
    async fn dry_http_refuses_a_blocked_target() {
        let refused = DryRunHttp::new(Vec::new())
            .request(json!({ "url": "http://127.0.0.1:9/" }), None)
            .await;
        assert!(
            matches!(refused, Err(EngineError::Capability(ref m))
                if m.contains("http_request") && m.contains("127.0.0.1")),
            "{refused:?}"
        );

        // The company's own allowlist, when it is unambiguous.
        let off_list = DryRunHttp::new(vec!["example.com".to_string()])
            .request(json!({ "url": "https://elsewhere.test/x" }), None)
            .await;
        assert!(
            matches!(off_list, Err(EngineError::Capability(ref m))
                if m.contains("allowed domains")),
            "{off_list:?}"
        );

        // On the list — and a subdomain of it — passes.
        for url in ["https://example.com/x", "https://api.example.com/x"] {
            assert!(
                DryRunHttp::new(vec!["example.com".to_string()])
                    .request(json!({ "url": url }), None)
                    .await
                    .is_ok(),
                "{url} is on the allowlist and must not be refused"
            );
        }
    }

    /// A malformed allowlist is not the only case [`preflight_refusal`] decides
    /// differently from the real guard. When **every** entry filters out during
    /// normalization, the real guard (`normalize_allowed_domains`, upstream)
    /// treats the list as misconfigured and fails closed — refusing every URL,
    /// on-list host or not. `preflight_refusal`'s own doc comment takes the
    /// opposite, deliberate stance for this case: "an allowlist this cannot
    /// read cleanly is left unchecked" rather than reproducing that subtlety.
    ///
    /// The result is a real, if narrow, dry-run/real-run parity gap: a
    /// workflow whose `web_allowed_domains` is entirely unparseable (every
    /// entry blank, or scheme-only) previews clean and then refuses every
    /// live request once armed. This pins the gap as it stands today rather
    /// than asserting it away, so a future change to either side has to
    /// touch this test on purpose.
    #[tokio::test]
    async fn dry_http_passes_an_entirely_malformed_allowlist_the_real_guard_fails_closed_on() {
        use crate::workflows::caps::http::GuardedHttpClient;
        use openhuman_core::openhuman::security::SecurityPolicy;
        use std::sync::Arc;

        // Whitespace-only: `normalize`/`normalize_domain` both trim it to empty
        // and drop it, so this is the "every entry malformed" case on both
        // sides, not a mixed list (which is a different, already-documented
        // divergence).
        let malformed = vec!["   ".to_string()];
        let request = json!({ "method": "GET", "url": "https://example.com/hook" });

        let real = GuardedHttpClient::new(Arc::new(SecurityPolicy::default()), malformed.clone());
        let live = real.request(request.clone(), None).await;
        assert!(
            matches!(&live, Err(EngineError::Capability(m)) if m.contains("allowed websites")),
            "a wholly malformed allowlist must fail the real guard closed, or this \
             test no longer pins the divergence it names: {live:?}"
        );

        let dry = DryRunHttp::new(malformed).request(request, None).await;
        assert!(
            dry.is_ok(),
            "the dry run reported the malformed-allowlist request as refused — \
             either preflight_refusal grew a fail-closed check for this case \
             (update this test to match) or something else changed: {dry:?}"
        );
    }

    /// **Behavioural parity with the real client.**
    ///
    /// The authoritative rules live upstream in a private `url_guard` module, so
    /// the pre-flight in `super::http` is a copy — and a copy drifts. This does
    /// not compare it against upstream's *source*; it drives the same URLs
    /// through the real [`GuardedHttpClient`](super::super::http::GuardedHttpClient)
    /// and asserts the two agree, so it keeps holding when upstream changes its
    /// internals and fails loudly the day upstream loosens a rule.
    ///
    /// This half covers the *too permissive* direction only — the real guard
    /// rejects every URL below **before it dials anything**, so the comparison
    /// needs no network and performs no effect. The other direction, a dry run
    /// stricter than the guard, is covered by
    /// [`dry_run_does_not_refuse_what_the_real_client_allows`]; asserting only
    /// this one is what let #1075's trailing-dot break sit on `main` unnoticed.
    #[tokio::test]
    async fn dry_run_refusal_matches_the_real_client() {
        use crate::workflows::caps::http::GuardedHttpClient;
        use openhuman_core::openhuman::security::SecurityPolicy;
        use std::sync::Arc;

        let allowed = vec!["example.com".to_string()];
        let cases = [
            "http://127.0.0.1:9/",
            "http://localhost:8080/x",
            "http://10.0.0.5/admin",
            "http://169.254.169.254/latest/meta-data/",
            "http://[::1]:9/",
            "https://elsewhere.test/x",
        ];

        let real = GuardedHttpClient::new(Arc::new(SecurityPolicy::default()), allowed.clone());
        for url in cases {
            let request = json!({ "method": "GET", "url": url });
            let dry = DryRunHttp::new(allowed.clone())
                .request(request.clone(), None)
                .await;
            let live = real.request(request, None).await;
            assert!(
                live.is_err(),
                "{url} must be refused by the real client for this comparison to mean anything"
            );
            assert!(
                dry.is_err(),
                "the real client refuses {url} but the dry run reported success —                  that is the false green issue #1048 is about"
            );
        }

        // Open-allowlist mode. Each of these is refused by the *shape* of the
        // URL, before any allowlist is consulted, so an empty list is no excuse
        // for the dry run to stay quiet — and each one did slip through it until
        // #1075, because the copy read past userinfo, unwrapped IPv6 brackets and
        // left a trailing dot on the host.
        let open_cases = [
            "https://user@example.com/x",
            "http://[2606:4700::1111]/x",
            "http://127.0.0.1./",
            "ftp://example.com/x",
        ];
        let open = GuardedHttpClient::new(Arc::new(SecurityPolicy::default()), Vec::new());
        for url in open_cases {
            let request = json!({ "method": "GET", "url": url });
            let dry = DryRunHttp::new(Vec::new())
                .request(request.clone(), None)
                .await;
            let live = open.request(request, None).await;
            assert!(
                live.is_err(),
                "{url} must be refused by the real client for this comparison to mean anything"
            );
            assert!(
                dry.is_err(),
                "the real client refuses {url} but the dry run reported success: {dry:?}"
            );
        }
    }

    /// **The other direction: the real guard allows ⇒ the dry run must not refuse.**
    ///
    /// [`dry_run_refusal_matches_the_real_client`] only ever asserts that both
    /// sides refuse, so it is structurally blind to this copy becoming *stricter*
    /// than the guard — and that is the failure that costs something. Issue
    /// #1075: `https://example.com./x` against `["example.com"]` was allowed by
    /// the real guard (which trims the trailing dot off the host) and refused
    /// here (which trimmed it off allowlist *entries* only), so Test run blocked
    /// a graph that runs, and an operator could not tell that from a real
    /// refusal without arming it.
    ///
    /// The real client cannot be driven all the way to "allowed" without issuing
    /// the request, which this suite may not do. So the host is an RFC 2606
    /// `.invalid` name: it clears every guard rule and the run then stops at DNS
    /// resolution, which cannot succeed — no connection is ever made. The
    /// assertion is "the guard did not refuse it", which is the claim under test.
    #[tokio::test]
    async fn dry_run_does_not_refuse_what_the_real_client_allows() {
        use crate::workflows::caps::http::GuardedHttpClient;
        use openhuman_core::openhuman::security::SecurityPolicy;
        use std::sync::Arc;

        let allowed = vec!["parity.invalid".to_string()];
        let cases = [
            // The regression: a legal fully-qualified host.
            "https://parity.invalid./x",
            "https://parity.invalid/x",
            "https://sub.parity.invalid/x",
        ];

        let real = GuardedHttpClient::new(Arc::new(SecurityPolicy::default()), allowed.clone());
        for url in cases {
            let request = json!({ "method": "GET", "url": url });
            let live = real.request(request.clone(), None).await;
            let guard_refused = matches!(
                &live,
                Err(EngineError::Capability(message))
                    if message.contains("allowed websites")
                        || message.contains("Blocked local/private host")
                        || message.contains("URL userinfo")
                        || message.contains("IPv6 hosts are not supported")
            );
            assert!(
                !guard_refused,
                "{url} must pass the real guard for this comparison to mean anything: {live:?}"
            );

            let dry = DryRunHttp::new(allowed.clone())
                .request(request, None)
                .await;
            assert!(
                dry.is_ok(),
                "the real guard allows {url} but the dry run refused it — a dry run \
                 stricter than the guard blocks a graph that would run: {dry:?}"
            );
        }
    }
}
