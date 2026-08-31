//! The mention directory: `GET {scope}/chat/mentionables`.
//!
//! Everything a composer needs to offer an `@` picker — the teammates, the
//! people, the desks, and the broadcast token — in one read, resolved the same
//! way the host resolves a mention it receives.
//!
//! # Why this is not `GET {scope}/users`
//!
//! The user directory is **admin-gated**
//! ([`require_admin`](crate::server::users::admin::require_admin)), and
//! correctly so: it carries login identities, roles, statuses, and invite
//! state, which is administration, not collaboration. But every member has to
//! be able to mention every other member, so mentioning a colleague cannot
//! require being an admin.
//!
//! The answer is a second, much narrower read rather than a relaxation of the
//! first. This route hands out **an id, a label, and the person's chosen face**,
//! and nothing else — no email, no role, no status, no last-seen. The avatar is
//! already a collaboration-facing identity asset: it is shown beside that
//! person's messages to the same members. That is the same discipline
//! [`author_labels`](crate::server::chat_history) already enforces on every
//! message a member reads, so this widens nothing sensitive: a person who has
//! ever posted is already named to their colleagues by exactly this label and
//! face.
//!
//! # Signed-in humans only
//!
//! A machine credential names no person and has no composer, so both the
//! privacy argument and the use case are absent. Same `401` as
//! [`read_state`](super::read_state), and for the same reason.

use crate::server::error::Rejection;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use serde::Serialize;
use serde_json::json;

use crate::AppState;
use crate::runtime::mentions::{EVERYONE_ALIASES, user_label, user_slugs};
use crate::server::error::ApiError;
use crate::server::ops::scope::{ScopedCompany, scoped};

pub fn router() -> Router<AppState> {
    scoped("/chat/mentionables", get(list_mentionables))
}

/// One teammate the composer can offer.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct MentionableAgentDto {
    /// The roster id — the authored, typable handle, and what a resolved
    /// mention is stored under.
    id: String,
    /// What to show in the picker. The display name for an operator-added
    /// teammate, the id for a manifest one, which is already human-authored.
    name: String,
    /// The teammate's job title, so two similarly-named teammates are
    /// distinguishable in the list.
    role: String,
}

/// One person the composer can offer.
///
/// **Id, label, and chosen face only.** See the module note: this is
/// deliberately not the admin user record, and must not grow toward it.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct MentionablePersonDto {
    /// The user id a resolved mention is stored under.
    id: String,
    /// How this person is named to their colleagues — the same label their
    /// messages are attributed with.
    label: String,
    /// The person's collaboration-facing avatar reference, when they chose
    /// one. This carries no login or contact identity and is already shown in
    /// chat alongside the person's authored messages.
    #[serde(skip_serializing_if = "Option::is_none")]
    avatar: Option<String>,
    /// A short typable alias, disambiguated across the company.
    ///
    /// **Not a handle and not stored.** Recomputed on every read, so a rename
    /// can never strand it; it exists so somebody typing fast has something
    /// shorter than a two-word display name to hit.
    slug: String,
}

/// One desk the composer can offer.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct MentionableDeskDto {
    /// The desk id a resolved mention is stored under.
    id: String,
    /// The desk's display name.
    name: String,
    /// The teammates a mention of this desk expands to, so the composer can
    /// warn about the blast radius before the message is sent rather than
    /// after.
    member_ids: Vec<String>,
}

/// The broadcast token, described rather than assumed.
///
/// Sent as data so the composer does not hard-code the spellings — the host
/// decides what `@everyone` is called, and a console that disagreed would offer
/// a row that resolves to nothing.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct MentionableEveryoneDto {
    /// The canonical spelling to insert.
    label: String,
    /// Every spelling the host accepts, so the picker can match on any of them.
    aliases: Vec<String>,
}

/// `GET {scope}/chat/mentionables` — everything an `@` can name.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct MentionablesDto {
    agents: Vec<MentionableAgentDto>,
    people: Vec<MentionablePersonDto>,
    desks: Vec<MentionableDeskDto>,
    everyone: MentionableEveryoneDto,
}

async fn list_mentionables(company: ScopedCompany) -> Result<Json<MentionablesDto>, Rejection> {
    if company.actor.is_none() {
        return Err(unauthorized().into());
    }

    let record = company
        .runtime
        .store()
        .load(company.id())
        .await
        .map_err(|e| ApiError(e).into_response())?
        .ok_or_else(|| {
            (
                StatusCode::NOT_FOUND,
                Json(json!({ "error": "no such company", "code": "not_found" })),
            )
                .into_response()
        })?;

    // `effective_agents()` rather than the raw manifest: an operator-renamed
    // manifest teammate's stored name lives in an override, and reading the
    // manifest directly would ignore it and advertise the authored id forever.
    let mut agents: Vec<MentionableAgentDto> = record
        .effective_agents()
        .into_iter()
        .map(|a| MentionableAgentDto {
            id: a.id.clone(),
            // A manifest teammate's id is human-authored (`engineer`, `ceo`),
            // so it is already the best label there is for one absent an
            // override.
            name: a.name.clone().unwrap_or(a.id),
            role: a.role,
        })
        .collect();
    agents.extend(
        record
            .overlay_agents
            .iter()
            .filter(|a| !record.is_retired(&a.id))
            .map(|a| MentionableAgentDto {
                id: a.id.clone(),
                name: a.name.clone(),
                role: a.role.clone(),
            }),
    );

    // The caller's own rows are dropped, for both kinds. A self-mention can
    // never survive sending — `normalize` refuses it — so offering a row that
    // names the caller would look pickable and then silently un-chip on
    // reload. An operator token's id matches no user and no agent, so the
    // filter is a no-op for it.
    let self_id = company.actor.as_ref().map(|a| a.id.clone());
    agents.retain(|a| self_id.as_ref().is_none_or(|s| a.id != *s));

    let mut desks: Vec<MentionableDeskDto> = record
        .manifest
        .group_chats
        .iter()
        .map(|c| MentionableDeskDto {
            id: c.id.clone(),
            name: c.name.clone(),
            member_ids: record.effective_desk_members(&c.id),
        })
        .collect();
    desks.extend(record.overlay_desks.iter().map(|d| MentionableDeskDto {
        id: d.id.clone(),
        name: d.name.clone(),
        member_ids: record.effective_desk_members(&d.id),
    }));

    // Sorted by id before the slugs are minted, so the `-2`/`-3` suffix a
    // colliding label gets is stable between two reads. An unsorted list would
    // let two people swap suffixes because a store returned them in a different
    // order, which would move a picker entry under somebody mid-type.
    let mut users = company
        .runtime
        .users()
        .list_users(company.id())
        .await
        .map_err(|e| ApiError(e).into_response())?;
    // `Suspended` is retained only for attribution and is refused on every
    // request (see `UserStatus::Suspended`) — advertising a suspended user
    // here would offer a mention target that can never sign back in to see
    // it, and the same list feeds mention resolution's "live" check below, so
    // an unfiltered list would also let a removed collaborator keep accepting
    // non-quiet direct mentions and `@everyone`.
    users.retain(|u| u.status == crate::ports::users::UserStatus::Active);
    users.sort_by(|a, b| a.id.cmp(&b.id));
    let slugs = user_slugs(&users);
    let people = users
        .iter()
        .zip(slugs)
        .filter(|(u, _)| self_id.as_ref().is_none_or(|s| u.id != *s))
        .map(|(u, slug)| MentionablePersonDto {
            id: u.id.clone(),
            label: user_label(u),
            avatar: u.avatar.clone(),
            slug,
        })
        .collect();

    Ok(Json(MentionablesDto {
        agents,
        people,
        desks,
        everyone: MentionableEveryoneDto {
            label: EVERYONE_ALIASES[0].to_string(),
            aliases: EVERYONE_ALIASES.iter().map(|a| a.to_string()).collect(),
        },
    }))
}

/// The `401` for a caller with no person behind it.
fn unauthorized() -> Response {
    (
        StatusCode::UNAUTHORIZED,
        Json(json!({
            "error": "the mention directory is for signed-in people, and this credential names none",
            "code": "unauthorized",
        })),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use axum::body::{Body, to_bytes};
    use axum::http::{Request, StatusCode};
    use serde_json::Value;
    use tower::ServiceExt;

    use crate::company::CompanyManifest;
    use crate::ports::CompanyStore;
    use crate::ports::types::{CompanyId, CompanyRecord};
    use crate::runtime::RuntimeBuilder;
    use crate::server::router;
    use crate::store::FsCompanyStore;
    use crate::{AppConfig, AppState};

    const MANIFEST: &str = "[company]\nname = \"Acme\"\n\
         [[agent]]\nid = \"ceo\"\nrole = \"Chief Executive\"\n\
         [[agent]]\nid = \"engineer\"\nrole = \"Backend Engineer\"\n\
         [[group_chat]]\nid = \"engineering\"\nname = \"Engineering\"\n\
         members = [\"engineer\", \"ceo\"]\n";

    fn home() -> tempfile::TempDir {
        tempfile::Builder::new()
            .prefix("oc-mentionables-")
            .tempdir()
            .expect("tempdir")
    }

    async fn state(home: &std::path::Path) -> AppState {
        let manifest: CompanyManifest = toml::from_str(MANIFEST).unwrap();
        let store = FsCompanyStore::new(home.to_path_buf());
        let id = CompanyId::new("acme");
        store
            .save(&CompanyRecord {
                overlay_retired_agents: Vec::new(),
                overlay_agent_edits: Vec::new(),
                id: id.clone(),
                manifest: manifest.clone(),
                ledger: Vec::new(),
                lifecycle: "running".to_string(),
                overlay_agents: Vec::new(),
                overlay_desk_members: Vec::new(),
                overlay_desk_order: Vec::new(),
                overlay_desks: Vec::new(),
                overlay_workflows: Vec::new(),
                overlay_budgets: Vec::new(),
                overlay_policy: None,
                overlay_tool_grants: None,
                overlay_desk_tools: Default::default(),
                disabled_workflows: Vec::new(),
                template_provenance: None,
                setup: None,
                name_confirmed: false,
                activation_completed_at: None,
                created_at_millis: None,
            })
            .await
            .unwrap();
        let runtime = RuntimeBuilder::new(home.to_path_buf(), manifest)
            .with_id(id.clone())
            .build()
            .await
            .unwrap();
        let state = AppState::new(AppConfig::default());
        state.registry().insert(id, std::sync::Arc::new(runtime));
        crate::server::test_support::seed_fixed_admin(&state, "acme").await;
        state
    }

    async fn call(state: &AppState, signed_in: bool) -> (StatusCode, Value) {
        let cookie = if signed_in {
            Some(crate::server::test_support::fixed_cookie("acme"))
        } else {
            None
        };
        call_with_cookie(state, cookie).await
    }

    async fn call_with_cookie(state: &AppState, cookie: Option<String>) -> (StatusCode, Value) {
        let mut request = Request::builder()
            .method("GET")
            .uri("/api/v1/company/chat/mentionables");
        if let Some(cookie) = cookie {
            request = request.header("cookie", cookie);
        }
        let response = router(state.clone())
            .oneshot(request.body(Body::empty()).unwrap())
            .await
            .unwrap();
        let status = response.status();
        let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        )
    }

    #[tokio::test]
    async fn a_signed_in_member_gets_every_kind_of_target() {
        let home = home();
        let state = state(home.path()).await;
        let (status, body) = call(&state, true).await;
        assert_eq!(status, StatusCode::OK);

        let agents: Vec<&str> = body["agents"]
            .as_array()
            .expect("agents")
            .iter()
            .map(|a| a["id"].as_str().expect("id"))
            .collect();
        // Containment, not equality: the global baseline roster
        // (`docs/spec/runtime/globals.md`) is appended to every company, and
        // those teammates are real teammates — so they are mentionable too.
        // Pinning the exact list here would fail every time the baseline grows.
        assert!(agents.contains(&"ceo"), "{agents:?}");
        assert!(agents.contains(&"engineer"), "{agents:?}");
        assert_eq!(
            body["agents"][0]["role"], "Chief Executive",
            "a row carries the job title, so two similar names stay apart"
        );

        let desks = body["desks"].as_array().expect("desks");
        assert_eq!(desks[0]["id"], "engineering");
        assert_eq!(desks[0]["name"], "Engineering");
        // The blast radius, so a composer can warn before the send rather than
        // after it.
        assert_eq!(
            desks[0]["memberIds"].as_array().expect("memberIds").len(),
            2
        );

        // The seeded admin is a person — but the caller's own row is dropped,
        // so with nobody else in the directory there is no person to offer.
        // See `a_person_never_sees_their_own_row` for the two-person case.
        let people = body["people"].as_array().expect("people");
        assert!(
            people.is_empty(),
            "the caller must not be offered as a mention target: {people:?}"
        );

        assert_eq!(body["everyone"]["label"], "everyone");
        let aliases = body["everyone"]["aliases"].as_array().expect("aliases");
        assert!(aliases.iter().any(|a| a == "channel"));
        assert!(aliases.iter().any(|a| a == "here"));
    }

    /// The whole reason this route exists rather than a relaxation of
    /// `GET {scope}/users`: a person is offered by **label and id only**.
    #[tokio::test]
    async fn a_person_row_carries_no_email_role_or_status() {
        let home = home();
        let state = state(home.path()).await;
        // The caller's own row is dropped, so a second person is needed for the
        // directory to have anyone to inspect.
        crate::server::test_support::seed_fixed_member(&state, "acme").await;
        let (_, body) = call(&state, true).await;

        let person = &body["people"][0];
        let keys: Vec<&String> = person.as_object().expect("object").keys().collect();
        assert_eq!(
            keys,
            vec!["id", "label", "slug"],
            "a mentionable person must carry nothing else: {person}"
        );
        assert!(
            !person.to_string().contains("example.test"),
            "the login identity must never reach a member: {person}"
        );
    }

    /// A self-mention can never survive sending (`normalize` refuses it), so a
    /// row that names the caller is not offered — offering it would look
    /// pickable and then silently un-chip on reload.
    #[tokio::test]
    async fn a_person_never_sees_their_own_row() {
        let home = home();
        let state = state(home.path()).await;
        crate::server::test_support::seed_fixed_member(&state, "acme").await;
        let (status, body) = call_with_cookie(
            &state,
            Some(crate::server::test_support::fixed_cookie("acme")),
        )
        .await;
        assert_eq!(status, StatusCode::OK);

        let labels: Vec<&str> = body["people"]
            .as_array()
            .expect("people")
            .iter()
            .map(|p| p["label"].as_str().expect("label"))
            .collect();
        assert_eq!(
            labels,
            vec!["Harness Member"],
            "the caller's own row is dropped and the other person's is offered: {labels:?}"
        );
    }

    /// A machine credential has no composer and no person to be. Same `401`
    /// the read-state routes answer, for the same reason.
    #[tokio::test]
    async fn a_caller_with_no_person_behind_it_is_refused() {
        let home = home();
        let state = state(home.path()).await;
        let (status, body) = call(&state, false).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert_eq!(body["code"], "unauthorized");
    }

    /// A suspended user is retained only for attribution and is refused on
    /// every request (`UserStatus::Suspended`) — advertising one here would
    /// offer a mention target that can never sign back in, and the same list
    /// backs live mention resolution, so a stale collaborator must not keep
    /// accepting non-quiet direct mentions or `@everyone` either.
    #[tokio::test]
    async fn a_suspended_user_is_not_offered_as_a_mention_target() {
        use crate::ports::CompanyId;
        use crate::ports::users::{UserRecord, UserRole, UserStatus};

        let home = home();
        let state = state(home.path()).await;
        crate::server::test_support::seed_fixed_member(&state, "acme").await;
        let id = CompanyId::new("acme");
        let runtime = state.registry().get(&id).expect("registered");
        runtime
            .users()
            .upsert_user(
                &id,
                &UserRecord {
                    id: "gone".to_string(),
                    email: "gone@acme.test".to_string(),
                    display_name: Some("Gone Guy".to_string()),
                    avatar: None,
                    role: UserRole::Member,
                    status: UserStatus::Suspended,
                    password_hash: None,
                    must_change_password: false,
                    created_at_millis: 0,
                    last_seen_at_millis: None,
                    updated_at_millis: 0,
                },
            )
            .await
            .expect("upsert suspended user");

        let (status, body) = call(&state, true).await;
        assert_eq!(status, StatusCode::OK);
        let people = body["people"].as_array().expect("people");
        assert_eq!(
            people.len(),
            1,
            "only the seeded active admin is offered: {people:?}"
        );
        assert!(
            people.iter().all(|p| p["id"] != "gone"),
            "a suspended user must not be a mention target: {people:?}"
        );
    }

    /// A manifest teammate's operator-set display name is a stored override,
    /// applied through `effective_agents()` — the picker must show it, not the
    /// authored roster id, and future edits must not be silently ignored.
    #[tokio::test]
    async fn a_renamed_manifest_agent_is_offered_by_its_effective_name() {
        use crate::ports::types::AgentOverride;

        let home = home();
        let state = state(home.path()).await;
        let id = CompanyId::new("acme");
        let store = FsCompanyStore::new(home.path().to_path_buf());
        let mut record = store.load(&id).await.unwrap().expect("record");
        record.overlay_agent_edits.push(AgentOverride {
            agent_id: "ceo".to_string(),
            name: Some("Ada".to_string()),
            role: None,
            description: None,
            tools: None,
            instructions: None,
            avatar: None,
            ..Default::default()
        });
        store.save(&record).await.expect("save");

        let (status, body) = call(&state, true).await;
        assert_eq!(status, StatusCode::OK);
        let agents = body["agents"].as_array().expect("agents");
        let ceo = agents
            .iter()
            .find(|a| a["id"] == "ceo")
            .expect("ceo present");
        assert_eq!(ceo["name"], "Ada", "{agents:?}");
    }
}
