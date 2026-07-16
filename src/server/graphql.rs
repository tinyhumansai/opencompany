//! GraphQL read surface: a query-only facade over the operator read routes.
//!
//! This mirrors the JSON reads served by [`operator`](crate::server::operator)
//! (`companies`, `company(id)`, `approvals`) as a single GraphQL endpoint,
//! resolving through the same [`AppState`] registry rather than reimplementing
//! any logic. Mutations and subscriptions are intentionally out of scope for
//! this slice.
//!
//! Auth reuses the operator bearer guard: in dev mode (`operator_token` unset)
//! every query is allowed; when a token is configured the request must carry
//! `Authorization: Bearer <token>`.

use async_graphql::{Context, EmptyMutation, EmptySubscription, Object, Schema, SimpleObject};
use async_graphql_axum::{GraphQLRequest, GraphQLResponse};
use axum::response::{Html, IntoResponse};
use axum::routing::{get, post};
use axum::{Router, extract::State};

use crate::AppState;
use crate::ports::types::CompanyId;
use crate::runtime::types::{ApprovalSummary, CompanyStatus};
use crate::server::operator::OperatorAuth;

/// Builds the GraphQL route fragment, merged into the main router.
///
/// `POST /graphql` serves queries; `GET /graphql` serves an embedded GraphiQL
/// explorer for interactive use during development.
pub fn router() -> Router<AppState> {
    Router::new()
        .route("/graphql", post(graphql_handler))
        .route("/graphql", get(graphiql))
}

/// A company status snapshot, projected for GraphQL.
///
/// A local mirror of [`CompanyStatus`] so the runtime types stay free of any
/// `async-graphql` derive coupling.
#[derive(SimpleObject)]
struct Company {
    /// The company id.
    id: String,
    /// The display name.
    name: String,
    /// Lifecycle state, e.g. `running`, `paused`, `archived`.
    lifecycle: String,
    /// The number of approvals currently awaiting the operator.
    pending_approvals: i32,
}

impl From<CompanyStatus> for Company {
    fn from(status: CompanyStatus) -> Self {
        Self {
            id: status.id.as_ref().to_string(),
            name: status.name,
            lifecycle: status.lifecycle,
            pending_approvals: status.pending_approvals as i32,
        }
    }
}

/// A parked approval, projected for GraphQL. Mirrors [`ApprovalSummary`].
#[derive(SimpleObject)]
struct Approval {
    /// The approval's id.
    id: String,
    /// The parked effect's dotted kind.
    kind: String,
    /// The USD amount involved, if any.
    amount_usd: Option<f64>,
    /// Epoch-millis the effect was parked.
    at_millis: f64,
}

impl From<ApprovalSummary> for Approval {
    fn from(summary: ApprovalSummary) -> Self {
        Self {
            id: summary.id.as_ref().to_string(),
            kind: summary.kind,
            amount_usd: summary.amount_usd,
            // GraphQL's `Int` is i32; epoch-millis overflow it, so widen to the
            // `Float` scalar which round-trips the full u64 range in practice.
            at_millis: summary.at_millis as f64,
        }
    }
}

/// The query root: every read the operator surface exposes.
struct QueryRoot;

#[Object]
impl QueryRoot {
    /// Status of every registered company.
    async fn companies(&self, ctx: &Context<'_>) -> async_graphql::Result<Vec<Company>> {
        let state = ctx.data::<AppState>()?;
        let mut out = Vec::new();
        for id in state.registry().list() {
            if let Some(runtime) = state.registry().get(&id) {
                out.push(runtime.status().await?.into());
            }
        }
        Ok(out)
    }

    /// One company's status, or `null` if no such company is registered.
    async fn company(
        &self,
        ctx: &Context<'_>,
        id: String,
    ) -> async_graphql::Result<Option<Company>> {
        let state = ctx.data::<AppState>()?;
        let Some(runtime) = state.registry().get(&CompanyId::new(&id)) else {
            return Ok(None);
        };
        Ok(Some(runtime.status().await?.into()))
    }

    /// The approvals currently awaiting the operator for one company.
    ///
    /// Returns an empty list for an unknown company, matching the intent of the
    /// operator approvals read (no company, no pending approvals).
    async fn approvals(
        &self,
        ctx: &Context<'_>,
        company_id: String,
    ) -> async_graphql::Result<Vec<Approval>> {
        let state = ctx.data::<AppState>()?;
        let Some(runtime) = state.registry().get(&CompanyId::new(&company_id)) else {
            return Ok(Vec::new());
        };
        Ok(runtime
            .pending_approvals()
            .into_iter()
            .map(Approval::from)
            .collect())
    }
}

/// `POST /graphql` — executes a query against the schema.
///
/// The schema is assembled per request with the current [`AppState`] injected
/// as context data; `AppState` is a cheap `Arc`-backed clone.
async fn graphql_handler(
    _auth: OperatorAuth,
    State(state): State<AppState>,
    req: GraphQLRequest,
) -> GraphQLResponse {
    let schema = Schema::build(QueryRoot, EmptyMutation, EmptySubscription)
        .data(state)
        .finish();
    schema.execute(req.into_inner()).await.into()
}

/// `GET /graphql` — a minimal embedded GraphiQL explorer.
async fn graphiql() -> impl IntoResponse {
    Html(async_graphql::http::graphiql_source("/graphql", None))
}

#[cfg(test)]
mod test {
    use axum::body::{Body, to_bytes};
    use axum::http::{Request, StatusCode};
    use std::sync::Arc;
    use tower::ServiceExt;

    use crate::company::CompanyManifest;
    use crate::ports::types::{CompanyId, CompanyRecord};
    use crate::runtime::RuntimeBuilder;
    use crate::server::router;
    use crate::store::FsCompanyStore;
    use crate::{AppConfig, AppState};

    fn home() -> std::path::PathBuf {
        std::env::temp_dir().join(format!("opencompany-gql-{}", crate::ports::generate_id()))
    }

    fn manifest() -> CompanyManifest {
        toml::from_str("[company]\nname = \"Acme\"\n[policy]\nmode = \"full\"\n").unwrap()
    }

    async fn state_with_company(home: &std::path::Path) -> AppState {
        let store = FsCompanyStore::new(home.to_path_buf());
        let id = CompanyId::new("acme");
        use crate::ports::CompanyStore;
        store
            .save(&CompanyRecord {
                id: id.clone(),
                manifest: manifest(),
                ledger: Vec::new(),
                lifecycle: "running".to_string(),
                overlay_agents: Vec::new(),
            })
            .await
            .unwrap();
        let runtime = RuntimeBuilder::new(home.to_path_buf(), manifest())
            .with_id(id.clone())
            .build()
            .await
            .unwrap();
        let state = AppState::new(AppConfig::default());
        state.registry().insert(id, Arc::new(runtime));
        state
    }

    async fn query(app: axum::Router, body: &str) -> serde_json::Value {
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/graphql")
                    .header("content-type", "application/json")
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    #[tokio::test]
    async fn companies_query_lists_the_company() {
        let home = home();
        let app = router(state_with_company(&home).await);
        let value = query(
            app,
            r#"{"query":"{ companies { id name lifecycle pendingApprovals } }"}"#,
        )
        .await;
        assert_eq!(value["data"]["companies"][0]["id"], "acme");
        assert_eq!(value["data"]["companies"][0]["name"], "Acme");
        assert_eq!(value["data"]["companies"][0]["lifecycle"], "running");
        tokio::fs::remove_dir_all(&home).await.ok();
    }

    #[tokio::test]
    async fn company_query_by_id_resolves() {
        let home = home();
        let app = router(state_with_company(&home).await);
        let value = query(
            app,
            r#"{"query":"{ company(id: \"acme\") { id pendingApprovals } }"}"#,
        )
        .await;
        assert_eq!(value["data"]["company"]["id"], "acme");
        tokio::fs::remove_dir_all(&home).await.ok();
    }

    #[tokio::test]
    async fn unknown_company_query_is_null() {
        let home = home();
        let app = router(state_with_company(&home).await);
        let value = query(app, r#"{"query":"{ company(id: \"ghost\") { id } }"}"#).await;
        assert!(value["data"]["company"].is_null());
        tokio::fs::remove_dir_all(&home).await.ok();
    }

    #[tokio::test]
    async fn approvals_query_is_empty_before_any_park() {
        let home = home();
        let app = router(state_with_company(&home).await);
        let value = query(
            app,
            r#"{"query":"{ approvals(companyId: \"acme\") { id kind } }"}"#,
        )
        .await;
        assert_eq!(value["data"]["approvals"].as_array().unwrap().len(), 0);
        tokio::fs::remove_dir_all(&home).await.ok();
    }
}
