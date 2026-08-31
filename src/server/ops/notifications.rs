//! The notification feed: `GET`/`PUT {scope}/notifications`.
//!
//! The first consumer was the mention badge; the runtime's failure/expiry
//! producers (`dispatch_failed`, `approval_expired`, `workflow_run_*`) and
//! the week-1 nudge banner (issue #1845) followed. `NotificationStore` was
//! wired into the runtime and unused since it was written (issue #749); this
//! is the surface that makes it reachable.
//!
//! No kind allowlist: `notifications()` has exactly one writer set (the
//! runtime's own notification producers), all of them user-facing, so
//! filtering by kind server-side would only recreate the "was this reachable
//! at all" bug the honest-verdicts work exists to close — a durable row
//! written but never returned to the one client that reads this store. Every
//! caller gets every unread row and filters to what it cares about
//! client-side (`pickActiveNudge`, the mention badge's own kind check, …) —
//! see `a_dispatch_failure_reaches_the_feed_alongside_a_mention` below.
//!
//! # Why this exists at all, when there is an SSE feed
//!
//! The live feed only reaches a browser that is **open**. A mention that lands
//! while somebody is asleep has to still be there when they come back, and that
//! is the entire job of this store — the module header on
//! [`crate::ports::notifications`] says so. A badge built from the live stream
//! alone would clear itself every time a tab was closed.
//!
//! # Delivery is polled, not pushed
//!
//! There is deliberately no `mention` frame on the company SSE feed. That
//! stream has **no per-viewer projection**, which is the documented reason
//! `ReactionToggled` is dropped from it entirely — a mention frame would have
//! to carry either everyone's user ids or nobody's. So the console refetches
//! this route on the poll it already runs, on each `agent_reply`, and on window
//! focus. Say that out loud rather than letting a reader assume it was an
//! oversight.
//!
//! # Signed-in humans only
//!
//! Same `401` as [`read_state`](super::read_state): a notification is addressed
//! to a person, and a machine credential names none.

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::AppState;
use crate::company::week1_nudge;
use crate::ports::notifications::NotificationView;
use crate::server::error::ApiError;
use crate::server::ops::scope::{ScopedCompany, scoped};

pub fn router() -> Router<AppState> {
    scoped("/notifications", get(list).put(mark_read))
}

/// One notification, as the person it is for reads it.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct NotificationDto {
    id: String,
    /// A free-form tag — `"mention"`, `"dispatch_failed"`,
    /// `"approval_expired"`, or `"workflow_run_*"`.
    kind: String,
    /// What it is about: `task` / `run` / `approval` / `workflow` / `message`.
    subject_kind: String,
    /// The subject's id in its own id space. For a `message` that is the chat
    /// message id, so the console can link straight at it.
    subject_id: String,
    /// The line a person reads.
    title: String,
    created_at: u64,
    /// When this person read it; absent while unread **for them**.
    #[serde(skip_serializing_if = "Option::is_none")]
    read_at: Option<u64>,
    /// The console channel this belongs to, so a badge can be placed without
    /// the transcript being loaded. Absent on rows that name no channel.
    #[serde(skip_serializing_if = "Option::is_none")]
    context: Option<String>,
}

impl From<NotificationView> for NotificationDto {
    fn from(view: NotificationView) -> Self {
        let NotificationView {
            notification,
            read_at,
        } = view;
        Self {
            id: notification.id,
            kind: notification.kind,
            subject_kind: notification.subject.kind.as_str().to_string(),
            subject_id: notification.subject.id,
            title: notification.title,
            created_at: notification.created_at,
            read_at,
            context: notification.context,
            // `audience` is deliberately NOT projected. It is the list of
            // everyone else who was mentioned, and handing each recipient the
            // user ids of all the others is a disclosure the badge has no use
            // for. Who else was named is already visible, as labels, on the
            // message itself.
        }
    }
}

/// `GET {scope}/notifications`.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct FeedDto {
    /// Newest first.
    notifications: Vec<NotificationDto>,
    /// How many are still unread for this person — what the badge renders.
    unread: usize,
}

/// `PUT {scope}/notifications` — mark read.
#[derive(Debug, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct MarkReadBody {
    /// The notifications to mark. **Absent or null marks everything** this
    /// person can see, which is what "clear the badge" means.
    ///
    /// An explicitly empty array marks nothing — a real distinction from
    /// absent, and the one a client sends when it has computed a set and found
    /// it empty.
    #[serde(default)]
    ids: Option<Vec<String>>,
}

/// `PUT {scope}/notifications` response.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct MarkReadDto {
    /// Still unread for this person after the mark — returned rather than
    /// assumed, because marking is a latch and two tabs race.
    unread: u64,
}

async fn list(company: ScopedCompany) -> Result<Json<FeedDto>, crate::server::Rejection> {
    let Some(user) = actor_id(&company) else {
        return Err(unauthorized().into());
    };
    let rows = company
        .runtime
        .notifications()
        .list(company.id(), &user)
        .await
        .map_err(|e| ApiError(e).into_response())?;
    // This endpoint is the durable-notification contract for every kind the
    // runtime appends here — `mention` and, as of the honest-verdicts work,
    // `dispatch_failed` / `approval_expired` / `workflow_run_*` / the week-1
    // nudge (issue #1845). There is no kind allowlist: `notifications()` has
    // exactly one writer set (the runtime's own notification producers), all
    // of them user-facing, so filtering by kind would only recreate the "was
    // this reachable at all" bug the honest-verdicts work exists to close —
    // a durable row written but never returned to the one client that reads
    // this store. Read rows are still excluded: the client only needs the
    // actionable, unread set.
    let rows: Vec<_> = rows
        .into_iter()
        .filter(|view| view.read_at.is_none())
        .collect();
    // PR #1878 review finding (comment 3879491539): a week-1 nudge row is
    // filed once by `LifecycleScheduler::tick` and, before this, cleared only
    // by the one client call site that calls `markNotificationsRead` after a
    // create — `WorkflowsView`'s own dialog. A workflow created through any
    // OTHER attributed path (accepting a Tasks proposal, a future second
    // create surface) left the row unread forever: nothing server-side ever
    // re-asked whether the fact the row is about had since become true.
    // Reconciling here, on every read, is what makes the banner self-heal
    // regardless of which surface satisfied it, rather than requiring every
    // future create path to remember to clear this one specific row. Scoped
    // internally to nudge-kind rows — this feed now carries every kind, not
    // only the nudge, since the kind-scoped `?kind=` query was retired above.
    let rows = reconcile_stale_nudges(&company, &user, rows).await;
    let unread = rows.len();
    Ok(Json(FeedDto {
        notifications: rows.into_iter().map(NotificationDto::from).collect(),
        unread,
    }))
}

/// Drops every unread week-1 nudge row out of `rows` — marking each read
/// server-side — once `user` has saved a workflow through any attributed
/// path, per `list`'s own call-site comment. Every non-nudge row passes
/// through untouched regardless of outcome; this only ever acts on the
/// nudge-kind subset of a feed that otherwise carries every kind.
///
/// Best-effort on both reads it makes (the user lookup and
/// `user_saved_workflow_in_week1`'s journal scan): either failing just
/// leaves the row(s) showing for one more poll rather than erroring the
/// whole feed over a reconciliation that can always run again next time —
/// the same non-fatal posture the client's own `refreshNudge` already takes
/// on its half of this (see its doc comment). A `mark_read` failure is
/// swallowed the same way: the row is still excluded from THIS response
/// (the caller has already learned the underlying fact), and a mark that
/// truly never lands just gets retried by the next poll's reconciliation.
async fn reconcile_stale_nudges(
    company: &ScopedCompany,
    user: &str,
    rows: Vec<NotificationView>,
) -> Vec<NotificationView> {
    let has_nudge_row = rows
        .iter()
        .any(|view| view.notification.kind == week1_nudge::NUDGE_KIND);
    if !has_nudge_row {
        return rows;
    }
    let Ok(Some(record)) = company.runtime.users().get_user(company.id(), user).await else {
        // No user record to read a signup instant from (a race with the
        // account being removed, or a store hiccup) — leave the row(s)
        // exactly as filed rather than guess.
        return rows;
    };
    let satisfied = week1_nudge::user_saved_workflow_in_week1(
        company.id(),
        company.runtime.events(),
        user,
        record.created_at_millis,
        crate::ports::now_millis(),
    )
    .await
    .unwrap_or(false);
    if !satisfied {
        return rows;
    }
    let stale_ids: Vec<String> = rows
        .iter()
        .filter(|view| view.notification.kind == week1_nudge::NUDGE_KIND)
        .map(|v| v.notification.id.clone())
        .collect();
    let _ = company
        .runtime
        .notifications()
        .mark_read(company.id(), user, Some(&stale_ids))
        .await;
    rows.into_iter()
        .filter(|view| view.notification.kind != week1_nudge::NUDGE_KIND)
        .collect()
}

async fn mark_read(
    company: ScopedCompany,
    body: axum::body::Bytes,
) -> Result<Json<MarkReadDto>, crate::server::Rejection> {
    let Some(user) = actor_id(&company) else {
        return Err(unauthorized().into());
    };
    // Read from raw bytes rather than through `Json`, so an **empty** body is
    // "mark everything" whatever the caller's `Content-Type` says.
    //
    // `Option<Json<_>>` looks like the idiomatic answer and is not: it yields
    // `None` only when the content type is absent, so the very common
    // `PUT` with `Content-Type: application/json` and no body is a `400` — a
    // client clearing a badge the obvious way gets an error for it. Clearing a
    // badge must not require knowing that.
    let ids = if body.is_empty() {
        None
    } else {
        match serde_json::from_slice::<MarkReadBody>(&body) {
            Ok(parsed) => parsed.ids,
            Err(err) => {
                return Err((
                    StatusCode::UNPROCESSABLE_ENTITY,
                    Json(json!({
                        "error": format!("body is not a mark-read request: {err}"),
                        "code": "invalid_request",
                    })),
                )
                    .into_response()
                    .into());
            }
        }
    };
    let unread = company
        .runtime
        .notifications()
        .mark_read(company.id(), &user, ids.as_deref())
        .await
        .map_err(|e| ApiError(e).into_response())?;
    Ok(Json(MarkReadDto { unread }))
}

/// The signed-in person behind the request, if there is one.
fn actor_id(company: &ScopedCompany) -> Option<String> {
    company.actor.as_ref().map(|a| a.id.clone())
}

/// The `401` for a caller with no person behind it.
fn unauthorized() -> Response {
    (
        StatusCode::UNAUTHORIZED,
        Json(json!({
            "error": "notifications are per person, and this credential names none",
            "code": "unauthorized",
        })),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use axum::body::{Body, to_bytes};
    use axum::http::{Request, StatusCode};
    use serde_json::{Value, json};
    use tower::ServiceExt;

    use crate::company::CompanyManifest;
    use crate::ports::CompanyStore;
    use crate::ports::notifications::{Notification, Subject, SubjectKind};
    use crate::ports::types::{CompanyId, CompanyRecord};
    use crate::ports::users::{UserRecord, UserRole, UserStatus};
    use crate::runtime::RuntimeBuilder;
    use crate::server::router;
    use crate::store::FsCompanyStore;
    use crate::{AppConfig, AppState};

    const MANIFEST: &str = "[company]\nname = \"Acme\"\n\
         [[agent]]\nid = \"ceo\"\nrole = \"Chief\"\n";

    fn home() -> tempfile::TempDir {
        tempfile::Builder::new()
            .prefix("oc-notifications-")
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

    /// The signed-in harness user's id, so a row can be addressed to them.
    async fn me(state: &AppState) -> String {
        state
            .registry()
            .get(&CompanyId::new("acme"))
            .expect("company")
            .users()
            .list_users(&CompanyId::new("acme"))
            .await
            .expect("users")
            .first()
            .expect("the seeded admin")
            .id
            .clone()
    }

    /// A colleague, so "somebody else's mention" is a real state to assert on.
    async fn seed_other(state: &AppState) -> String {
        let id = CompanyId::new("acme");
        let runtime = state.registry().get(&id).expect("company");
        let now = crate::ports::now_millis();
        let user = UserRecord {
            id: "u-other".to_string(),
            email: "other@example.test".to_string(),
            display_name: Some("Other Person".to_string()),
            avatar: None,
            role: UserRole::Member,
            status: UserStatus::Active,
            password_hash: None,
            must_change_password: false,
            created_at_millis: now,
            last_seen_at_millis: None,
            updated_at_millis: now,
        };
        runtime.users().upsert_user(&id, &user).await.expect("seed");
        user.id
    }

    async fn file(state: &AppState, id: &str, audience: Option<Vec<String>>) {
        file_kind(state, id, audience, "mention").await;
    }

    /// Same as [`file`], but lets the caller pick the notification `kind` —
    /// the runtime's other producers (`dispatch_failed`, `approval_expired`,
    /// `workflow_run_*`) all go through the same store.
    async fn file_kind(state: &AppState, id: &str, audience: Option<Vec<String>>, kind: &str) {
        state
            .registry()
            .get(&CompanyId::new("acme"))
            .expect("company")
            .notifications()
            .append(
                &CompanyId::new("acme"),
                &Notification {
                    id: id.to_string(),
                    kind: kind.to_string(),
                    subject: Subject {
                        kind: SubjectKind::Message,
                        id: "42".to_string(),
                    },
                    created_at: 1_000,
                    title: format!("{kind} {id}"),
                    audience,
                    context: Some("engineering".to_string()),
                },
            )
            .await
            .expect("append");
    }

    /// A signed-in `GET` — the `?kind=` query string is accepted and ignored
    /// (this module's own doc explains why the server no longer filters by
    /// kind); kept for parity with the production client, which still sends
    /// it on the nudge banner's own read.
    async fn call_kind(state: &AppState, kind: &str) -> (StatusCode, Value) {
        let request = Request::builder()
            .method("GET")
            .uri(format!("/api/v1/company/notifications?kind={kind}"))
            .header("cookie", crate::server::test_support::fixed_cookie("acme"))
            .body(Body::empty())
            .unwrap();
        let response = router(state.clone()).oneshot(request).await.unwrap();
        let status = response.status();
        let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        )
    }

    async fn call(
        state: &AppState,
        method: &str,
        body: Option<Value>,
        signed_in: bool,
    ) -> (StatusCode, Value) {
        let mut request = Request::builder()
            .method(method)
            .uri("/api/v1/company/notifications")
            .header("content-type", "application/json");
        if signed_in {
            request = request.header("cookie", crate::server::test_support::fixed_cookie("acme"));
        }
        let request = match body {
            Some(value) => request.body(Body::from(value.to_string())).unwrap(),
            None => request.body(Body::empty()).unwrap(),
        };
        let response = router(state.clone()).oneshot(request).await.unwrap();
        let status = response.status();
        let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        )
    }

    #[tokio::test]
    async fn a_targeted_mention_reaches_the_person_it_names() {
        let home = home();
        let state = state(home.path()).await;
        let mine = me(&state).await;
        let other = seed_other(&state).await;

        file(&state, "for-me", Some(vec![mine])).await;
        file(&state, "for-them", Some(vec![other])).await;

        let (status, feed) = call(&state, "GET", None, true).await;
        assert_eq!(status, StatusCode::OK);
        let rows = feed["notifications"].as_array().expect("rows");
        assert_eq!(rows.len(), 1, "a colleague's mention must not be listed");
        assert_eq!(rows[0]["id"], "for-me");
        assert_eq!(feed["unread"], 1);
        // The channel, so a badge can be placed with no transcript loaded.
        assert_eq!(rows[0]["context"], "engineering");
        assert_eq!(rows[0]["subjectKind"], "message");
        assert_eq!(rows[0]["subjectId"], "42");
    }

    /// The audience is who *else* was mentioned. Handing each recipient the
    /// user ids of everyone else is a disclosure the badge has no use for.
    #[tokio::test]
    async fn the_audience_is_never_projected() {
        let home = home();
        let state = state(home.path()).await;
        let mine = me(&state).await;
        let other = seed_other(&state).await;
        file(&state, "shared", Some(vec![mine, other.clone()])).await;

        let (_, feed) = call(&state, "GET", None, true).await;
        let row = &feed["notifications"][0];
        assert!(row.get("audience").is_none(), "{row}");
        assert!(!row.to_string().contains(&other), "{row}");
    }

    #[tokio::test]
    async fn a_company_wide_row_reaches_everyone() {
        let home = home();
        let state = state(home.path()).await;
        file(&state, "everyone", None).await;
        let (_, feed) = call(&state, "GET", None, true).await;
        assert_eq!(feed["notifications"].as_array().unwrap().len(), 1);
    }

    /// Clearing a badge needs no body at all.
    /// Clearing a badge must work the obvious way: a `PUT` with a JSON content
    /// type and no body. `Option<Json<_>>` would 400 on exactly that.
    #[tokio::test]
    async fn a_bodyless_put_marks_everything_read() {
        let home = home();
        let state = state(home.path()).await;
        let mine = me(&state).await;
        file(&state, "a", Some(vec![mine.clone()])).await;
        file(&state, "b", Some(vec![mine])).await;

        let (status, marked) = call(&state, "PUT", None, true).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(marked["unread"], 0);

        let (_, feed) = call(&state, "GET", None, true).await;
        assert_eq!(feed["unread"], 0);
        // The feed filters read rows out entirely (`list` keeps only unread
        // mentions), so the only assertion that can fail is that nothing
        // remains — `.all(readAt is set)` would be vacuously true over an
        // empty list and prove nothing.
        assert!(
            feed["notifications"].as_array().unwrap().is_empty(),
            "a mark-all-read leaves no unread mention in the feed"
        );
    }

    #[tokio::test]
    async fn a_malformed_body_is_refused_rather_than_read_as_mark_all() {
        let home = home();
        let state = state(home.path()).await;
        let mine = me(&state).await;
        file(&state, "a", Some(vec![mine])).await;

        let request = Request::builder()
            .method("PUT")
            .uri("/api/v1/company/notifications")
            .header("content-type", "application/json")
            .header("cookie", crate::server::test_support::fixed_cookie("acme"))
            .body(Body::from("{\"ids\": \"not-a-list\"}"))
            .unwrap();
        let response = router(state.clone()).oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);

        // And it changed nothing.
        let (_, feed) = call(&state, "GET", None, true).await;
        assert_eq!(feed["unread"], 1);
    }

    #[tokio::test]
    async fn naming_ids_marks_only_those() {
        let home = home();
        let state = state(home.path()).await;
        let mine = me(&state).await;
        file(&state, "a", Some(vec![mine.clone()])).await;
        file(&state, "b", Some(vec![mine])).await;

        let (_, marked) = call(&state, "PUT", Some(json!({"ids": ["a"]})), true).await;
        assert_eq!(marked["unread"], 1);
    }

    /// An explicitly empty array is a real answer — "I computed a set and it
    /// was empty" — and must not be read as "mark everything".
    #[tokio::test]
    async fn an_empty_id_list_marks_nothing() {
        let home = home();
        let state = state(home.path()).await;
        let mine = me(&state).await;
        file(&state, "a", Some(vec![mine])).await;

        let (_, marked) = call(&state, "PUT", Some(json!({"ids": []})), true).await;
        assert_eq!(marked["unread"], 1);
    }

    /// The runtime's failure/expiry producers (`dispatch_failed`,
    /// `approval_expired`, `workflow_run_failed`, ...) append through the same
    /// store as a mention. If this endpoint kept filtering to `kind ==
    /// "mention"`, those durable rows would never reach the client that is the
    /// store's only consumer — silently unbadged and unread forever.
    #[tokio::test]
    async fn a_dispatch_failure_reaches_the_feed_alongside_a_mention() {
        let home = home();
        let state = state(home.path()).await;
        let mine = me(&state).await;
        file_kind(
            &state,
            "bounced",
            Some(vec![mine.clone()]),
            "dispatch_failed",
        )
        .await;
        file_kind(
            &state,
            "expired",
            Some(vec![mine.clone()]),
            "approval_expired",
        )
        .await;
        file_kind(
            &state,
            "run",
            Some(vec![mine.clone()]),
            "workflow_run_failed",
        )
        .await;
        file(&state, "mentioned", Some(vec![mine])).await;

        let (status, feed) = call(&state, "GET", None, true).await;
        assert_eq!(status, StatusCode::OK);
        let rows = feed["notifications"].as_array().expect("rows");
        let kinds: Vec<&str> = rows.iter().map(|r| r["kind"].as_str().unwrap()).collect();
        assert_eq!(kinds.len(), 4, "{kinds:?}");
        assert!(kinds.contains(&"dispatch_failed"), "{kinds:?}");
        assert!(kinds.contains(&"approval_expired"), "{kinds:?}");
        assert!(kinds.contains(&"workflow_run_failed"), "{kinds:?}");
        assert!(kinds.contains(&"mention"), "{kinds:?}");
        assert_eq!(feed["unread"], 4);
    }

    #[tokio::test]
    async fn a_caller_with_no_person_behind_it_is_refused() {
        let home = home();
        let state = state(home.path()).await;
        for method in ["GET", "PUT"] {
            let (status, body) = call(&state, method, None, false).await;
            assert_eq!(status, StatusCode::UNAUTHORIZED, "{method}");
            assert_eq!(body["code"], "unauthorized", "{method}");
        }
    }

    // --- issue #1845: the week-1 nudge on the shared, kind-unscoped feed ---

    /// A nudge row rides the same unscoped feed as a mention (see the module
    /// doc's "no kind allowlist" reasoning, and
    /// `a_dispatch_failure_reaches_the_feed_alongside_a_mention` above for the
    /// general case) — both come back from one read, and `?kind=` on the
    /// request is decoration for the production client, not a server-side
    /// filter (`call_kind`'s own doc). `pickActiveNudge` on the client is what
    /// narrows a mixed feed down to the nudge banner's one row.
    #[tokio::test]
    async fn a_nudge_row_rides_the_same_feed_as_a_mention() {
        let home = home();
        let state = state(home.path()).await;
        let mine = me(&state).await;
        file(&state, "a-mention", Some(vec![mine.clone()])).await;
        file_kind(&state, "a-nudge", Some(vec![mine]), "workflow_nudge").await;

        let (status, feed) = call(&state, "GET", None, true).await;
        assert_eq!(status, StatusCode::OK);
        let rows = feed["notifications"].as_array().expect("rows");
        let ids: Vec<&str> = rows.iter().map(|r| r["id"].as_str().unwrap()).collect();
        assert_eq!(ids.len(), 2, "{ids:?}");
        assert!(ids.contains(&"a-mention"), "{ids:?}");
        assert!(ids.contains(&"a-nudge"), "{ids:?}");
    }

    /// Mark-read already marks by id or "everything visible", so a nudge row
    /// marked read must disappear from the feed exactly like a mention does.
    #[tokio::test]
    async fn marking_a_nudge_read_clears_it_from_the_feed() {
        let home = home();
        let state = state(home.path()).await;
        let mine = me(&state).await;
        file_kind(&state, "a-nudge", Some(vec![mine]), "workflow_nudge").await;

        let (_, marked) = call(&state, "PUT", Some(json!({"ids": ["a-nudge"]})), true).await;
        assert_eq!(marked["unread"], 0);

        let (_, feed) = call_kind(&state, "workflow_nudge").await;
        assert!(
            feed["notifications"].as_array().unwrap().is_empty(),
            "a read nudge must not still show as unread, {feed}"
        );
    }

    /// PR #1878 review finding (comment 3879491539): a workflow created
    /// through a path OTHER than `WorkflowsView`'s own create dialog — here,
    /// simulated the way accepting a Tasks proposal would attribute it, by
    /// journaling `WorkflowCreated { by: Some(Actor { kind: User, id }) }`
    /// directly rather than going through the dialog's `clearNudge` call —
    /// must still clear a pending nudge on the next read. Before the
    /// `reconcile_stale_nudges` fix this asserts, the row stayed unread
    /// forever: `PUT .../notifications` was the only thing that ever cleared
    /// it, and nothing on this path calls it.
    #[tokio::test]
    async fn a_workflow_created_through_another_surface_clears_the_nudge_on_next_read() {
        let home = home();
        let state = state(home.path()).await;
        let mine = me(&state).await;
        file_kind(
            &state,
            "a-nudge",
            Some(vec![mine.clone()]),
            "workflow_nudge",
        )
        .await;

        // Sanity: unread before the create, same as every other nudge test.
        let (_, before) = call_kind(&state, "workflow_nudge").await;
        assert_eq!(before["notifications"].as_array().unwrap().len(), 1);

        let runtime = state
            .registry()
            .get(&CompanyId::new("acme"))
            .expect("company");
        runtime
            .events()
            .append(
                &CompanyId::new("acme"),
                crate::ports::types::CompanyEvent::WorkflowCreated {
                    workflow_id: "wf-from-tasks".to_string(),
                    name: "From a Tasks proposal".to_string(),
                    by: Some(crate::ports::types::Actor {
                        kind: crate::ports::types::ActorKind::User,
                        id: mine,
                    }),
                },
            )
            .await
            .expect("journal the create");

        let (status, feed) = call_kind(&state, "workflow_nudge").await;
        assert_eq!(status, StatusCode::OK);
        assert!(
            feed["notifications"].as_array().unwrap().is_empty(),
            "a satisfied nudge must self-heal on the next read even though \
             nothing ever called PUT .../notifications for it, {feed}"
        );
        assert_eq!(feed["unread"], 0);

        // The reconciliation durably marked it read, not merely omitted it
        // from this one response — a later default-kind or explicit re-list
        // must not resurrect it.
        let (_, again) = call_kind(&state, "workflow_nudge").await;
        assert!(again["notifications"].as_array().unwrap().is_empty());
    }

    /// The reconciliation in `reconcile_stale_nudges` must not clear a nudge
    /// for a user who has NOT yet saved a workflow — otherwise every unread
    /// nudge would vanish on its very first read regardless of the
    /// underlying fact, defeating the feature entirely.
    #[tokio::test]
    async fn an_unsatisfied_nudge_stays_unread_across_reads() {
        let home = home();
        let state = state(home.path()).await;
        let mine = me(&state).await;
        file_kind(&state, "a-nudge", Some(vec![mine]), "workflow_nudge").await;

        let (_, first) = call_kind(&state, "workflow_nudge").await;
        assert_eq!(first["notifications"].as_array().unwrap().len(), 1);

        let (_, second) = call_kind(&state, "workflow_nudge").await;
        assert_eq!(
            second["notifications"].as_array().unwrap().len(),
            1,
            "reconciliation must not clear a nudge nobody has satisfied, {second}"
        );
        assert_eq!(second["unread"], 1);
    }
}
