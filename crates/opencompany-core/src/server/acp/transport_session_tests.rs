use super::*;
use serde_json::json;

use super::test_support::*;

/// A colon is legal in both halves of the owner key, so the join has to be
/// injective on its own rather than by assuming the components are clean.
#[test]
fn two_different_principals_never_share_an_owner_key() {
    let left = owner(&admin_auth(&CompanyId::new("a:b"), "c".to_string(), "h"));
    let right = owner(&admin_auth(&CompanyId::new("a"), "b:c".to_string(), "h"));
    assert_ne!(
        left, right,
        "a company and a user id that split differently must not collide"
    );

    // The same principal still resolves to one stable key, or an operator
    // would lose their own connection between calls.
    let again = owner(&admin_auth(&CompanyId::new("a:b"), "c".to_string(), "h"));
    assert_eq!(left, again);
}

#[tokio::test]
async fn a_caller_cannot_open_a_session_on_a_connection_it_does_not_own() {
    let home = tempfile::Builder::new()
        .prefix("oc-acp-conn-owner-")
        .tempdir()
        .expect("tempdir");
    let state = acp_state(home.path()).await;
    let company = CompanyId::new("acme");
    let alice = seed_user(&state, &company, "u-alice", "Alice").await;
    let bob = seed_user(&state, &company, "u-bob", "Bob").await;
    let auth_alice = admin_auth(&company, alice, "hash-alice");
    let auth_bob = admin_auth(&company, bob, "hash-bob");
    let params = json!({
        "_meta": {
            "opencompany": { "company": "acme" },
            "opencompany/connectionId": "conn-shared",
        }
    });

    let first = open_session(&state, &auth_alice, &params).await;
    assert!(first.is_ok(), "alice opens the connection first: {first:?}");

    let second = open_session(&state, &auth_bob, &params).await;
    assert!(
        second.is_err(),
        "bob must not be able to open a session on alice's connection"
    );

    // And bob gets no view into what alice has, by any surface.
    let listed = list_sessions(&state, &auth_bob, &params);
    assert!(listed.is_err(), "bob cannot list alice's connection either");
}

#[tokio::test]
async fn open_session_refuses_once_the_per_connection_cap_is_hit() {
    let home = tempfile::Builder::new()
        .prefix("oc-acp-conn-cap-")
        .tempdir()
        .expect("tempdir");
    let state = acp_state(home.path()).await;
    let company = CompanyId::new("acme");
    let admin = seed_user(&state, &company, "u-admin", "Admin Person").await;
    let auth = admin_auth(&company, admin, "hash");
    let params = json!({
        "_meta": {
            "opencompany": { "company": "acme" },
            "opencompany/connectionId": "conn-cap",
        }
    });

    for _ in 0..crate::server::acp::session::MAX_SESSIONS_PER_CONNECTION {
        let result = open_session(&state, &auth, &params).await;
        assert!(result.is_ok(), "{result:?}");
    }
    let refusal = open_session(&state, &auth, &params).await;
    assert!(refusal.is_err(), "the cap must refuse the next open");
}

#[tokio::test]
async fn disconnect_closes_every_session_on_the_connection_at_once() {
    let home = tempfile::Builder::new()
        .prefix("oc-acp-disconnect-")
        .tempdir()
        .expect("tempdir");
    let state = acp_state(home.path()).await;
    let company = CompanyId::new("acme");
    let admin = seed_user(&state, &company, "u-admin", "Admin Person").await;
    let auth = admin_auth(&company, admin, "hash");
    let params = json!({
        "_meta": {
            "opencompany": { "company": "acme" },
            "opencompany/connectionId": "conn-close",
        }
    });

    open_session(&state, &auth, &params)
        .await
        .expect("first session");
    open_session(&state, &auth, &params)
        .await
        .expect("second session");

    let closed = disconnect(&state, &auth, &params);
    assert!(closed.is_ok());

    // The whole connection is gone, not merely emptied of sessions:
    // `list_sessions` on an unknown connection is the same refusal as one
    // this caller never owned.
    assert!(
        list_sessions(&state, &auth, &params).is_err(),
        "the connection must not survive its own disconnect"
    );
}

#[tokio::test]
async fn a_session_opened_without_a_chat_is_bound_to_the_default_agents_dm() {
    let home = tempfile::Builder::new()
        .prefix("oc-acp-default-chat-")
        .tempdir()
        .expect("tempdir");
    let state = acp_state(home.path()).await;
    let company = CompanyId::new("acme");
    let admin = seed_user(&state, &company, "u-admin", "Admin Person").await;
    let auth = admin_auth(&company, admin, "hash");
    let params = json!({
        "_meta": {
            "opencompany": { "company": "acme" },
            "opencompany/connectionId": "conn-default-chat",
        }
    });

    let opened = open_session(&state, &auth, &params)
        .await
        .expect("the session opens");
    let id = opened["sessionId"].as_str().expect("a session id");
    let session = state
        .acp_sessions()
        .get(
            "conn-default-chat",
            &owner(&auth),
            id,
            crate::ports::now_millis(),
        )
        .expect("the session is registered");
    assert_eq!(session.chat, "dm:product_manager");
}
