use super::*;

fn tenant_claims(tenant: &str, scopes: &[&str]) -> PlatformClaims {
    PlatformClaims {
        tenant: tenant.to_string(),
        scopes: scopes.iter().map(|s| s.to_string()).collect(),
        companies: None,
    }
}

#[test]
fn b64url_round_trips() {
    for sample in [&b""[..], b"a", b"ab", b"abc", b"abcd", b"hello world!"] {
        let encoded = b64url_encode(sample);
        assert_eq!(b64url_decode(&encoded).unwrap(), sample);
    }
}

#[test]
fn platform_secret_grants_platform_scope() {
    let verifier = StaticPlatformVerifier::new("top-secret");
    let claims = verifier.verify("top-secret").unwrap();
    assert!(claims.has_platform_scope());
    assert_eq!(claims.tenant, "tenant:platform");
}

/// The offline codec the gate suites mint tenant principals with. It lives
/// on the test-only type; the assertion below is about the codec, not about
/// anything a shipped build accepts.
#[test]
fn unsigned_codec_round_trips_for_the_gate_suites() {
    let verifier = UnsignedTenantVerifier::new("top-secret");
    let token = UnsignedTenantVerifier::tenant_token(&tenant_claims("tenant:acme", &["operator"]));
    let claims = verifier.verify(&token).unwrap();
    assert_eq!(claims.tenant, "tenant:acme");
    assert!(!claims.has_platform_scope());

    // It still delegates the shared-secret arm, so a suite can hold both.
    assert!(verifier.verify("top-secret").unwrap().has_platform_scope());
}

/// The shipped default build must not turn a caller-supplied payload into
/// platform claims. The bearer is assembled literally, from nothing but the
/// wire shape, so this stays a black-box statement about what an
/// unauthenticated request can obtain — no helper, no shared constant.
#[test]
fn hand_constructed_tenant_bearer_is_refused() {
    let verifier = StaticPlatformVerifier::new("top-secret");
    let forged = format!(
        "oc_tenant.{}",
        b64url_encode(br#"{"tenant":"tenant:victim","scopes":["platform","operator"]}"#)
    );
    assert!(
        verifier.verify(&forged).is_err(),
        "an unsigned, caller-supplied payload must not resolve to platform claims"
    );
}

#[cfg(feature = "platform-jwt")]
fn sign(secret: &str, claims: &serde_json::Value) -> String {
    use jsonwebtoken::{Algorithm, EncodingKey, Header, encode};
    encode(
        &Header::new(Algorithm::HS256),
        claims,
        &EncodingKey::from_secret(secret.as_bytes()),
    )
    .expect("sign")
}

#[cfg(feature = "platform-jwt")]
#[test]
fn jwt_verifier_refuses_an_expired_token() {
    let secret = "signing-secret";
    // 2001-09-09, comfortably in the past whenever this runs.
    let token = sign(
        secret,
        &json!({"tenant": "tenant:acme", "scopes": ["operator"], "exp": 1_000_000_000u64}),
    );
    assert!(JwtPlatformVerifier::new(secret).verify(&token).is_err());
}

/// Pins down a documented, deliberately-deferred limit (this module's own
/// doc comment: "A signed token carrying no `exp` never expires... \
/// Changing that is a separate policy call") rather than changing it. A
/// token with no `exp` claim at all is accepted, and stays accepted
/// however long from now this runs — there is no lever in this verifier
/// that would ever refuse it on staleness alone. If a future change adds
/// a mandatory-expiry policy, this test is the one that should start
/// failing and prompt updating it, rather than the behavior silently
/// drifting either direction unnoticed.
#[cfg(feature = "platform-jwt")]
#[test]
fn jwt_verifier_accepts_a_token_with_no_exp_claim_at_all() {
    let secret = "signing-secret";
    let token = sign(
        secret,
        &json!({"tenant": "tenant:acme", "scopes": ["operator"]}),
    );
    let claims = JwtPlatformVerifier::new(secret)
        .verify(&token)
        .expect("a token with no exp claim is currently accepted unconditionally");
    assert_eq!(claims.tenant, "tenant:acme");
}

#[cfg(feature = "platform-jwt")]
#[test]
fn jwt_verifier_refuses_a_tampered_payload() {
    let secret = "signing-secret";
    let token = sign(
        secret,
        &json!({"tenant": "tenant:acme", "scopes": ["operator"]}),
    );

    // Rewrite the claims to grant the platform scope, keeping the original
    // signature: it no longer covers the body it is attached to.
    let parts: Vec<&str> = token.split('.').collect();
    let mut claims: serde_json::Value =
        serde_json::from_slice(&b64url_decode(parts[1]).expect("payload")).expect("claims");
    claims["scopes"] = json!(["platform", "operator"]);
    let tampered = format!(
        "{}.{}.{}",
        parts[0],
        b64url_encode(&serde_json::to_vec(&claims).expect("re-encode")),
        parts[2]
    );

    assert!(JwtPlatformVerifier::new(secret).verify(&tampered).is_err());
}

/// `alg: "none"` is the oldest JWT forgery there is: drop the signature,
/// keep whatever payload you like, and hope the verifier honors the header's
/// choice of algorithm. `Validation::new(Algorithm::HS256)` pins the
/// algorithm so it does not, and clearing `required_spec_claims` does not
/// loosen that — but only a test says so out loud. The same claims, signed,
/// are accepted, which leaves the header as the only thing the rejection can
/// be about.
#[cfg(feature = "platform-jwt")]
#[test]
fn jwt_verifier_refuses_an_unsigned_token() {
    let secret = "signing-secret";
    let claims = json!({"tenant": "tenant:victim", "scopes": ["platform", "operator"]});

    // Assembled literally, from nothing but the wire shape: header, payload,
    // and the empty signature an `alg: none` token carries.
    let unsigned = format!(
        "{}.{}.",
        b64url_encode(br#"{"alg":"none","typ":"JWT"}"#),
        b64url_encode(&serde_json::to_vec(&claims).expect("claims"))
    );

    assert!(
        JwtPlatformVerifier::new(secret).verify(&unsigned).is_err(),
        "an unsigned `alg: none` token must not resolve to platform claims"
    );

    let signed = sign(secret, &claims);
    assert!(JwtPlatformVerifier::new(secret).verify(&signed).is_ok());
}

#[test]
fn unrecognized_token_is_rejected() {
    let verifier = StaticPlatformVerifier::new("top-secret");
    assert!(verifier.verify("nope").is_err());
    assert!(verifier.verify("oc_tenant.@@@not-base64@@@").is_err());
}

#[test]
fn no_credential_configures_no_platform_auth() {
    assert!(configure(None, None).unwrap().is_none());
    // A blank injected variable is unset, not an empty accepted bearer.
    assert!(
        configure(Some("  ".to_string()), Some(String::new()))
            .unwrap()
            .is_none()
    );
}

#[test]
fn shared_secret_alone_configures_shared_secret_mode() {
    let (config, mode) = configure(Some("plat-secret".to_string()), None)
        .unwrap()
        .unwrap();
    assert_eq!(mode, PlatformAuthMode::SharedSecret);
    assert!(
        config
            .verifier
            .verify("plat-secret")
            .unwrap()
            .has_platform_scope()
    );
    assert!(config.verifier.verify("wrong").is_err());
}

#[cfg(feature = "platform-jwt")]
#[test]
fn signing_secret_alone_configures_jwt_mode() {
    let (config, mode) = configure(None, Some("signing-secret".to_string()))
        .unwrap()
        .unwrap();
    assert_eq!(mode, PlatformAuthMode::Jwt);

    let token = sign(
        "signing-secret",
        &json!({"tenant": "tenant:acme", "scopes": ["operator"]}),
    );
    assert_eq!(
        config.verifier.verify(&token).unwrap().tenant,
        "tenant:acme"
    );
    // The shared-secret arm is not wired, so nothing else gets in.
    assert!(config.verifier.verify("signing-secret").is_err());
}

#[cfg(feature = "platform-jwt")]
#[test]
fn both_secrets_accept_either_credential() {
    let (config, mode) = configure(
        Some("plat-secret".to_string()),
        Some("signing-secret".to_string()),
    )
    .unwrap()
    .unwrap();
    assert_eq!(mode, PlatformAuthMode::Both);

    assert!(
        config
            .verifier
            .verify("plat-secret")
            .unwrap()
            .has_platform_scope()
    );
    let token = sign(
        "signing-secret",
        &json!({"tenant": "tenant:acme", "scopes": ["operator"]}),
    );
    assert_eq!(
        config.verifier.verify(&token).unwrap().tenant,
        "tenant:acme"
    );

    // And a bearer neither arm authenticates is still refused.
    assert!(config.verifier.verify("oc_tenant.e30").is_err());
    assert!(config.verifier.verify("nope").is_err());
}

/// A build that cannot verify signatures must abort boot when a signing
/// secret is set, not fall back to the shared secret. `configure_with` takes
/// the availability flag so this stays observable on a build that *has* the
/// feature; a featureless build reaches the same error through
/// `jwt_verifier`.
#[test]
fn signing_secret_without_the_feature_refuses_to_boot() {
    let err = configure_with(
        Some("plat-secret".to_string()),
        Some("signing-secret".to_string()),
        false,
    )
    .expect_err("a signing secret this build cannot use must abort boot");

    let message = err.to_string();
    assert!(
        message.contains(PLATFORM_JWT_SECRET_ENV) && message.contains("platform-jwt"),
        "the refusal must name the variable and the missing feature: {message}"
    );
    assert!(
        !message.contains("signing-secret"),
        "the refusal must never carry secret material: {message}"
    );
}

#[test]
fn auth_mode_names_are_stable() {
    assert_eq!(PlatformAuthMode::SharedSecret.to_string(), "shared-secret");
    assert_eq!(PlatformAuthMode::Jwt.to_string(), "jwt");
    assert_eq!(PlatformAuthMode::Both.to_string(), "both");
}

#[test]
fn may_address_honors_allow_list() {
    let mut claims = tenant_claims("tenant:acme", &["operator"]);
    assert!(claims.may_address(&CompanyId::new("anything")));
    claims.companies = Some(HashSet::from(["acme".to_string()]));
    assert!(claims.may_address(&CompanyId::new("acme")));
    assert!(!claims.may_address(&CompanyId::new("globex")));
}

/// [`CompanyAuth`] only authenticates: it resolves *a* principal, not
/// whether that principal may reach the addressed company.
/// [`authorize_address`] is the separate call every real handler makes
/// right after — this is the one place that pairing is proven directly
/// against the function itself, rather than only through whichever route
/// handler happens to call it. A tenant token that owns a *different*
/// company must be refused `403`, not let through because it merely
/// verified.
#[test]
fn authorize_address_denies_a_platform_token_for_a_company_it_does_not_own() {
    use crate::app::AppConfig;

    let state = crate::AppState::new(AppConfig::default());
    state.set_owner(CompanyId::new("globex"), "tenant:globex-corp");

    let auth = GqlAuth::Platform(tenant_claims("tenant:acme-corp", &["operator"]));
    let resp = authorize_address(&state, &auth, &CompanyId::new("globex"))
        .expect("a tenant that does not own the addressed company must be refused");
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
}

/// The positive control for the test above: the same shape, but the
/// tenant actually owns the company, so `authorize_address` must let it
/// through (`None`). Without this, the denial test could pass for the
/// wrong reason (e.g. every call refused).
#[test]
fn authorize_address_allows_a_platform_token_for_a_company_it_owns() {
    use crate::app::AppConfig;

    let state = crate::AppState::new(AppConfig::default());
    state.set_owner(CompanyId::new("acme"), "tenant:acme-corp");

    let auth = GqlAuth::Platform(tenant_claims("tenant:acme-corp", &["operator"]));
    assert!(authorize_address(&state, &auth, &CompanyId::new("acme")).is_none());
}

/// The `platform` scope is not tenant-owned at all — it is the hosting
/// layer's own credential and may address any company, including one no
/// tenant owns yet (e.g. mid-provisioning). Distinct from the allow-list
/// check: platform scope bypasses ownership entirely.
#[test]
fn authorize_address_platform_scope_bypasses_ownership() {
    use crate::app::AppConfig;

    let state = crate::AppState::new(AppConfig::default());
    // Deliberately unowned.
    let auth = GqlAuth::Platform(tenant_claims("tenant:platform", &[SCOPE_PLATFORM]));
    assert!(authorize_address(&state, &auth, &CompanyId::new("unowned")).is_none());
}

/// Ownership alone is not enough: a tenant token whose own claims carry an
/// allow-list that excludes the company must still be refused, even
/// though the ownership map says the tenant owns it. Two independent
/// checks — [`AppState::owner_of`] and [`PlatformClaims::may_address`] —
/// both have to say yes.
#[test]
fn authorize_address_honors_the_claims_allow_list_even_when_the_tenant_owns_the_company() {
    use crate::app::AppConfig;

    let state = crate::AppState::new(AppConfig::default());
    state.set_owner(CompanyId::new("acme"), "tenant:acme-corp");

    let mut claims = tenant_claims("tenant:acme-corp", &["operator"]);
    claims.companies = Some(HashSet::from(["some-other-company".to_string()]));
    let auth = GqlAuth::Platform(claims);

    let resp = authorize_address(&state, &auth, &CompanyId::new("acme"))
        .expect("an allow-list that excludes the company must refuse even the owning tenant");
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
}

/// A user session is scoped to exactly one company by construction
/// ([`UserPrincipal::company`]); `authorize_address` refuses any other.
/// There is no ownership map involved on this arm at all — a session
/// minted for one company must never authorize a request against
/// another, however the ids happen to be spelled.
#[test]
fn authorize_address_denies_a_user_session_addressing_a_different_company() {
    use crate::app::AppConfig;
    use crate::ports::SessionKind;
    use crate::ports::users::UserRole;
    use crate::server::graphql::auth::UserPrincipal;

    let state = crate::AppState::new(AppConfig::default());
    let auth = GqlAuth::User(UserPrincipal {
        company: CompanyId::new("acme"),
        user_id: "u1".to_string(),
        email: "a@example.test".to_string(),
        role: UserRole::Admin,
        must_change_password: false,
        session_token_hash: "hash".to_string(),
        credential: SessionKind::Browser,
    });

    let resp = authorize_address(&state, &auth, &CompanyId::new("globex"))
        .expect("a session minted for one company must not authorize another");
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    assert!(authorize_address(&state, &auth, &CompanyId::new("acme")).is_none());
}

#[cfg(feature = "platform-jwt")]
#[test]
fn jwt_verifier_round_trips_signed_claims() {
    use jsonwebtoken::{Algorithm, EncodingKey, Header, encode};

    let secret = "signing-secret";
    let claims = tenant_claims("tenant:acme", &["platform", "operator"]);
    let token = encode(
        &Header::new(Algorithm::HS256),
        &claims,
        &EncodingKey::from_secret(secret.as_bytes()),
    )
    .unwrap();

    let verifier = JwtPlatformVerifier::new(secret);
    let verified = verifier.verify(&token).unwrap();
    assert_eq!(verified.tenant, "tenant:acme");
    assert!(verified.has_platform_scope());

    // A token signed with the wrong secret is rejected.
    let wrong = JwtPlatformVerifier::new("other-secret");
    assert!(wrong.verify(&token).is_err());
}
