//! Classification, against the response shapes the providers actually send.
//!
//! Every body below is the real one, taken from the provider's own
//! documentation or from an unauthenticated probe with an obviously fake key.
//! The four cases that matter most are the ones the ordering exists for: Brave's
//! `422`, Brave's `403`, SearXNG's `403`, and `407 Proxy Authentication
//! Required`.

use super::*;

fn status(code: u16, body: &str) -> ProbeFailure {
    ProbeFailure::Status {
        status: code,
        body: body.to_string(),
    }
}

#[test]
fn brave_rejects_a_bad_key_with_422_not_401() {
    // Brave's API reference documents 200/404/422/429 and no 401 and no 403.
    // A classifier that maps auth to 401/403 leaves this key stored.
    let body = r#"{"error":{"code":"SUBSCRIPTION_TOKEN_INVALID","detail":"The provided subscription token is invalid.","meta":{"component":"authentication"},"status":422},"type":"ErrorResponse"}"#;
    assert_eq!(classify("brave", &status(422, body)), ProbeClass::Auth);
    assert!(destroys_credential(ProbeClass::Auth));
}

#[test]
fn a_brave_403_is_a_waf_and_must_not_delete_the_key() {
    // Brave never uses 403 for authentication, so a 403 from that host can only
    // be something in front of it.
    assert_eq!(
        classify("brave", &status(403, "Forbidden")),
        ProbeClass::Unknown
    );
    assert!(!destroys_credential(ProbeClass::Unknown));
}

#[test]
fn a_brave_422_that_is_not_the_token_code_is_our_bug_not_their_key() {
    let body =
        r#"{"error":{"code":"VALIDATION","detail":"Unable to validate request parameter(s)"}}"#;
    assert_eq!(classify("brave", &status(422, body)), ProbeClass::Unknown);
}

#[test]
fn exa_rejects_a_bad_key_with_401() {
    let body = r#"{"requestId":"abc","error":"Invalid API key","tag":"INVALID_API_KEY"}"#;
    assert_eq!(classify("exa", &status(401, body)), ProbeClass::Auth);
}

#[test]
fn an_exa_402_without_credential_wording_is_out_of_credit_not_a_bad_key() {
    // 402 means either "no credential at all" or "out of credit", so the body
    // decides. Out of credit keeps the key.
    assert_eq!(
        classify("exa", &status(402, r#"{"error":"insufficient credits"}"#)),
        ProbeClass::Quota
    );
    assert!(!destroys_credential(ProbeClass::Quota));
}

#[test]
fn querit_rejects_a_bad_key_with_401_and_a_string_typed_code() {
    let body =
        r#"{"error_code":"401","error_msg":"Invalid authorization header format.","search_id":7}"#;
    assert_eq!(classify("querit", &status(401, body)), ProbeClass::Auth);
}

#[test]
fn a_searxng_403_means_json_is_off_not_that_a_key_was_rejected() {
    // There is no key. Deleting one would be deleting nothing, and the operator
    // would lose the only message that tells them what to change.
    assert_eq!(
        classify("searxng", &status(403, "Forbidden")),
        ProbeClass::Format
    );
    assert!(!destroys_credential(ProbeClass::Format));
    assert!(describe(ProbeClass::Format, "SearXNG").contains("search.formats"));
}

#[test]
fn a_proxy_challenge_is_never_auth_whatever_provider_it_arrives_for() {
    // The word "authentication" inside `407 Proxy Authentication Required` is
    // the reason this branch runs first.
    for slug in ["brave", "exa", "querit", "searxng"] {
        assert_eq!(
            classify(slug, &status(407, "Proxy Authentication Required")),
            ProbeClass::Unknown,
            "{slug}"
        );
    }
}

#[test]
fn a_cloudflare_interstitial_is_not_auth() {
    assert_eq!(
        classify(
            "exa",
            &status(403, "<title>Just a moment...</title> Cloudflare")
        ),
        ProbeClass::Unknown
    );
}

#[test]
fn a_status_like_number_inside_an_id_does_not_match() {
    // Word boundaries: `1403` and `4071` are not statuses.
    let body = r#"{"requestId":"req-1403-4071","error":"something else"}"#;
    assert_eq!(classify("exa", &status(500, body)), ProbeClass::Unknown);
}

#[test]
fn rate_limiting_is_quota_everywhere_and_keeps_the_credential() {
    for slug in ["brave", "exa", "querit"] {
        assert_eq!(
            classify(slug, &status(429, "")),
            ProbeClass::Quota,
            "{slug}"
        );
    }
}

#[test]
fn a_404_is_the_endpoint_and_a_timeout_is_a_timeout() {
    assert_eq!(classify("querit", &status(404, "")), ProbeClass::Endpoint);
    assert_eq!(
        classify(
            "searxng",
            &ProbeFailure::Transport("error sending request: operation timed out".to_string())
        ),
        ProbeClass::Timeout
    );
    assert_eq!(
        classify(
            "searxng",
            &ProbeFailure::Transport("dns error: failed to lookup address".to_string())
        ),
        ProbeClass::Endpoint
    );
}

#[test]
fn only_auth_is_destructive() {
    for class in [
        ProbeClass::Format,
        ProbeClass::Quota,
        ProbeClass::Endpoint,
        ProbeClass::Timeout,
        ProbeClass::Unknown,
    ] {
        assert!(!destroys_credential(class), "{}", class.as_str());
    }
    assert!(destroys_credential(ProbeClass::Auth));
}

#[test]
fn no_sentence_carries_the_upstream_body() {
    // The body can echo request material, including fragments of a key, and
    // these sentences land in a banner somebody screenshots into a ticket.
    let leak = "sk-not-a-real-key";
    let body = format!(r#"{{"error":"bad token {leak}"}}"#);
    let class = classify("exa", &status(401, &body));
    assert!(!describe(class, "Exa").contains(leak));
}

#[test]
fn the_metadata_service_is_refused_and_a_private_instance_is_not() {
    // A self-hosted SearXNG at an RFC1918 address is the ordinary deployment,
    // so the guard here is narrower than the one that fetches pasted links.
    assert!(guard_instance_url("http://169.254.169.254/").is_err());
    assert!(guard_instance_url("http://[fe80::1]/").is_err());
    assert!(guard_instance_url("http://[::169.254.169.254]/").is_err());
    assert!(guard_instance_url("http://0.0.0.0/").is_err());

    // AWS's IPv6 metadata address is a unique-local address rather than a
    // link-local one, so nothing about its shape gives it away and the
    // link-local test walks straight past it. It is named explicitly.
    assert!(guard_instance_url("http://[fd00:ec2::254]/").is_err());
    assert!(guard_instance_url("http://[fd00:0ec2:0:0:0:0:0:254]/").is_err());

    assert!(guard_instance_url("http://10.0.0.5:8080").is_ok());
    // And an ordinary IPv6 private network is still ordinary — refusing every
    // ULA would be the same mistake as refusing RFC1918.
    assert!(guard_instance_url("http://[fd00:1234::5]:8080").is_ok());
    assert!(guard_instance_url("http://192.168.1.20").is_ok());
    assert!(guard_instance_url("http://127.0.0.1:8888").is_ok());
    assert!(guard_instance_url("https://search.acme.internal").is_ok());
}

#[test]
fn a_hostname_is_judged_by_what_it_resolves_to() {
    // `guard_instance_url` can only read a literal address, so a name walks
    // past it and the request goes wherever the name points. That is the whole
    // of the protection the guard exists to give, so the same rule is applied
    // to the resolved answers — and then pinned, so the name cannot mean
    // something else by the time the connection is made.
    let metadata: std::net::SocketAddr = "169.254.169.254:80".parse().unwrap();
    let ordinary: std::net::SocketAddr = "10.0.0.5:8080".parse().unwrap();

    assert!(pick_address(&[metadata]).is_err());
    // Any of them being the metadata service refuses all of them. A name that
    // points there is not a search instance whatever else it points at, and
    // choosing around it would make the outcome depend on DNS ordering.
    assert!(pick_address(&[ordinary, metadata]).is_err());
    assert!(pick_address(&[metadata, ordinary]).is_err());

    // A private address stays ordinary — a self-hosted SearXNG on the company
    // network is the normal deployment, not the attack.
    assert_eq!(pick_address(&[ordinary]).unwrap(), ordinary);
    assert!(
        pick_address(&[]).is_err(),
        "a name that resolves to nothing"
    );
}

#[test]
fn a_non_http_scheme_is_refused_before_anything_is_fetched() {
    assert!(guard_instance_url("file:///etc/passwd").is_err());
    assert!(guard_instance_url("ftp://example.test").is_err());
    assert!(guard_instance_url("not a url at all").is_err());
}

#[test]
fn the_log_line_withholds_the_body_it_classified_from() {
    // The body is allowed to reach `classify`. It is not allowed to reach the
    // response — the type already says so — and a log is a second durable copy
    // of the same material, so it does not reach that either.
    let leaked = "token sk-not-a-real-key was rejected";
    let detail = log_detail(&status(401, leaked));
    assert!(!detail.contains("sk-not-a-real-key"), "{detail}");
    assert!(!detail.contains(leaked), "{detail}");
    // What is left is what somebody reading the log acts on.
    assert!(detail.contains("401"), "{detail}");
    assert!(
        detail.contains(&leaked.len().to_string()),
        "the size is worth keeping even when the bytes are not: {detail}"
    );
}

#[test]
fn a_transport_failure_still_says_what_went_wrong() {
    // No credential can be in one: the key travels in a header, and a request
    // that failed at this layer never got an answer to echo it.
    let detail = log_detail(&ProbeFailure::Transport(
        "error sending request: operation timed out".to_string(),
    ));
    assert!(detail.contains("operation timed out"), "{detail}");
}

#[tokio::test]
async fn a_hostile_body_is_abandoned_rather_than_buffered() {
    // The cap has to be on the stream. `text()` then `.take(4096)` buffers the
    // whole body first, which caps what is kept and not what is accepted — and
    // for SearXNG the address is the operator's, so the answer is not this
    // host's to trust.
    //
    // Sixteen megabytes against a 4 KiB cap: four thousand times the limit, and
    // an ordinary rejection is under a kilobyte.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let Ok((mut stream, _)) = listener.accept().await else {
            return;
        };
        use tokio::io::AsyncWriteExt;
        let _ = stream
            .write_all(b"HTTP/1.1 500 Internal Server Error\r\nContent-Type: text/plain\r\nConnection: close\r\n\r\n")
            .await;
        // Written until the client hangs up, which is the point: the reader
        // must stop, not the writer.
        let chunk = vec![b'x'; 64 * 1024];
        for _ in 0..256 {
            if stream.write_all(&chunk).await.is_err() {
                return;
            }
        }
    });

    let info = crate::company::search::catalogue::entry("searxng").expect("searxng is catalogued");
    let endpoint = format!("http://{address}");
    let failure = probe(info, None, Some(&endpoint))
        .await
        .expect_err("a 500 is not a success");
    let ProbeFailure::Status { status, body } = failure else {
        panic!("expected a status failure, got {failure:?}");
    };
    assert_eq!(status, 500);
    assert!(
        body.len() <= BODY_CAP,
        "read {} bytes against a {BODY_CAP}-byte cap",
        body.len()
    );
    server.abort();
}

#[test]
fn a_longer_code_carrying_the_same_token_is_still_a_rejected_key() {
    // The match is a substring, and deliberately. It is anchored by the status
    // first — only a Brave 422 reaches it at all — and the token it looks for
    // is specific enough that a body containing it is a body about the
    // subscription token.
    //
    // A stricter equality check on a parsed `code` field would have to parse
    // four providers' envelopes, and a provider that changes its envelope would
    // then fail to parse and classify as not-auth: the credential stays stored
    // beside an amber advisory, which is the safe direction but loses the
    // signal. A longer code carrying the same token is a rejected key either
    // way, so it is pinned here rather than guarded against.
    let body = r#"{"error":{"code":"SUBSCRIPTION_TOKEN_INVALID_FORMAT","status":422}}"#;
    assert_eq!(classify("brave", &status(422, body)), ProbeClass::Auth);

    // And the anchor holds: the same token under a different status is not a
    // credential rejection, so nothing is rolled back.
    assert_ne!(classify("brave", &status(500, body)), ProbeClass::Auth);
    // As does the per-provider split — Brave's token code means nothing coming
    // from anybody else.
    assert_ne!(classify("searxng", &status(422, body)), ProbeClass::Auth);
}
