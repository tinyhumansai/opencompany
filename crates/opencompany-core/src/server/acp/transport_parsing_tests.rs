use super::*;
use serde_json::json;

use crate::ports::users::UserRole;
use crate::server::graphql::auth::UserPrincipal;
use crate::server::platform_auth::PlatformClaims;

#[test]
fn target_requires_an_explicit_company() {
    assert!(target(&json!({ "_meta": { "opencompany": {} } })).is_err());
}

#[test]
fn target_leaves_an_omitted_chat_unset() {
    let (_, chat, _) =
        target(&json!({ "_meta": { "opencompany": { "company": "acme" } } })).unwrap();
    assert_eq!(chat, None);
    let (_, chat, _) =
        target(&json!({ "_meta": { "opencompany": { "company": "acme", "chat": "" } } })).unwrap();
    assert_eq!(chat, None);
}

#[test]
fn target_reads_an_agent_pin() {
    let (_, _, agent) =
        target(&json!({ "_meta": { "opencompany": { "company": "acme", "agentId": "ceo" } } }))
            .unwrap();
    assert_eq!(agent.as_deref(), Some("ceo"));
}

#[test]
fn initialize_result_is_acp_shaped() {
    let result = initialize_result();
    // Numeric ACP version, not MCP's date-valued protocolVersion.
    assert_eq!(result["protocolVersion"], json!(1));
    assert!(result.get("capabilities").is_none(), "no MCP capabilities");
    assert!(result.get("serverInfo").is_none(), "no MCP serverInfo");
    // The two ACP-required result fields.
    assert!(result.get("agentCapabilities").is_some());
    assert!(result.get("agentInfo").is_some());
    assert!(result["agentInfo"]["name"].is_string());
    assert!(result["agentInfo"]["version"].is_string());
}

#[test]
fn prompt_blocks_concatenate_text() {
    let params = json!({
        "prompt": [
            { "type": "text", "text": "hello " },
            { "type": "text", "text": "world" },
        ]
    });
    assert_eq!(prompt_text(&params).unwrap(), "hello world");
}

#[test]
fn prompt_must_be_an_array_of_blocks() {
    assert!(prompt_text(&json!({ "prompt": "hello" })).is_err());
    assert!(prompt_text(&json!({ "prompt": { "text": "hello" } })).is_err());
}

#[test]
fn unsupported_prompt_blocks_are_named() {
    let err =
        prompt_text(&json!({ "prompt": [ { "type": "image", "data": "..." } ] })).unwrap_err();
    assert!(
        err.contains("image"),
        "rejected by type, not generically: {err}"
    );
}

#[test]
fn an_empty_prompt_is_refused() {
    assert!(prompt_text(&json!({ "prompt": [] })).is_err());
}

#[test]
fn owner_keys_a_user_by_company_as_well_as_id() {
    // `user_id` is only guaranteed unique within a company, so two
    // companies minting the same id must not collide into one owner.
    let same_id_in_acme = GqlAuth::User(UserPrincipal {
        company: CompanyId::new("acme"),
        user_id: "u1".to_string(),
        email: "a@example.test".to_string(),
        role: UserRole::Admin,
        must_change_password: false,
        session_token_hash: "hash".to_string(),
        credential: crate::ports::SessionKind::Browser,
    });
    let same_id_in_globex = GqlAuth::User(UserPrincipal {
        company: CompanyId::new("globex"),
        user_id: "u1".to_string(),
        email: "b@example.test".to_string(),
        role: UserRole::Admin,
        must_change_password: false,
        session_token_hash: "hash".to_string(),
        credential: crate::ports::SessionKind::Browser,
    });
    assert_ne!(owner(&same_id_in_acme), owner(&same_id_in_globex));
}

#[test]
fn owner_canonicalizes_the_platform_tenant() {
    // `authorize_address` compares tenants in canonical_tenant form, so a
    // raw-string owner key would treat `tenant:acme` and `acme` as two
    // different owners even though they name the same tenant.
    let prefixed = GqlAuth::Platform(PlatformClaims {
        tenant: "tenant:acme".to_string(),
        scopes: Default::default(),
        companies: None,
    });
    let bare = GqlAuth::Platform(PlatformClaims {
        tenant: "acme".to_string(),
        scopes: Default::default(),
        companies: None,
    });
    assert_eq!(owner(&prefixed), owner(&bare));
}
