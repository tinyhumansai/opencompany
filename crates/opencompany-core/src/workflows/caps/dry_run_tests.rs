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
            if m.contains("allowed websites") && m.contains("elsewhere.test")),
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

/// A wholly malformed allowlist fails closed in the dry run exactly as it does
/// in a real run. This used to be a documented divergence — the dry run's own
/// copy of the guard skipped a list it could not read rather than reproduce the
/// fail-closed sentinel — and it went away when the dry run started calling the
/// real guard's rules instead of copying them.
#[tokio::test]
async fn dry_http_fails_closed_on_an_entirely_malformed_allowlist_like_the_real_guard() {
    use crate::workflows::caps::http::GuardedHttpClient;
    use openhuman_core::security::SecurityPolicy;
    use std::sync::Arc;

    let malformed = vec!["   ".to_string()];
    let request = json!({ "method": "GET", "url": "https://example.com/hook" });

    let real = GuardedHttpClient::new(Arc::new(SecurityPolicy::default()), malformed.clone());
    let live = real.request(request.clone(), None).await;
    assert!(
        matches!(&live, Err(EngineError::Capability(m)) if m.contains("allowed websites")),
        "a wholly malformed allowlist must fail the real guard closed: {live:?}"
    );

    let dry = DryRunHttp::new(malformed).request(request, None).await;
    assert!(
        matches!(&dry, Err(EngineError::Capability(m)) if m.contains("allowed websites")),
        "the dry run must refuse what the real guard refuses: {dry:?}"
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
    use openhuman_core::security::SecurityPolicy;
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
    use openhuman_core::security::SecurityPolicy;
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
